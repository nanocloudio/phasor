//! The router between an isolate's calls and an adapter that serves them.
//!
//! It admits a call by its binding index alone, correlates the answer with the
//! call by request identifier and trace, and bounds how many calls may be
//! outstanding. It never drops or reorders a completion: a call that cannot be
//! forwarded is answered here, with the reason, rather than left unanswered.
//!
//! It holds no address and no credential. Which adapter serves the binding is
//! the graph's wiring, not this module's knowledge.

#![no_std]
#![allow(
    dead_code,
    unused_imports,
    unreachable_patterns,
    reason = "the Fluxor ABI source is mounted as one surface and this module consumes a subset"
)]

use core::ffi::c_void;

#[path = "../../../target/fluxor/fluxor-abi/sdk/abi.rs"]
mod abi;
use abi::SyscallTable;

include!("../../../target/fluxor/fluxor-abi/sdk/runtime.rs");

#[path = "../../common/binding.rs"]
mod binding;
#[path = "../../common/digest.rs"]
mod digest;
#[path = "../../common/numeric.rs"]
mod numeric;
#[path = "../../common/softfloat.rs"]
mod softfloat;
#[path = "../../common/value.rs"]
mod value;

use binding::{CallRecord, Cause, CompletionRecord, Disposition, CALL_FRAME, COMPLETION_FRAME};

const POLL_INPUT: u32 = 0x01;
const POLL_OUTPUT: u32 = 0x02;

/// Bindings this router admits. A call on any other index is refused here.
const ADMITTED: u32 = 1;
/// Calls that may be outstanding at once. One more is refused as busy, which
/// the caller may retry.
const IN_FLIGHT: usize = 4;
/// Frames that may be staged for each output at once.
const STAGE: usize = 8;

/// One call this router forwarded and is waiting on.
#[derive(Clone, Copy)]
struct Outstanding {
    request: u64,
    trace: u64,
    live: bool,
}

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    call_in: i32,
    reply_in: i32,
    completion_out: i32,
    request_out: i32,
    table: [Outstanding; IN_FLIGHT],
    call: [u8; CALL_FRAME],
    call_filled: usize,
    reply: [u8; COMPLETION_FRAME],
    reply_filled: usize,
    requests: [u8; CALL_FRAME * STAGE],
    requests_staged: usize,
    requests_written: usize,
    completions: [u8; COMPLETION_FRAME * STAGE],
    completions_staged: usize,
    completions_written: usize,
    /// Calls admitted, refused, and answered, which is what this module reports
    /// about itself.
    admitted: u64,
    refused: u64,
    answered: u64,
    dropped: u64,
    hung_up: bool,
    phase: u8,
}

fn in_flight(state: &State) -> usize {
    state.table.iter().filter(|slot| slot.live).count()
}

/// Stage a completion for the isolate. A staging buffer that is full is not a
/// reason to drop an answer: the router stops taking calls instead.
fn stage_completion(state: &mut State, record: &CompletionRecord) -> bool {
    let at = state.completions_staged;
    let frame = record.encode();
    let Some(slot) = state.completions.get_mut(at..at + COMPLETION_FRAME) else {
        return false;
    };
    slot.copy_from_slice(&frame);
    state.completions_staged = at + COMPLETION_FRAME;
    true
}

/// Stage a call for the adapter.
fn stage_request(state: &mut State, frame: &[u8; CALL_FRAME]) -> bool {
    let at = state.requests_staged;
    let Some(slot) = state.requests.get_mut(at..at + CALL_FRAME) else {
        return false;
    };
    slot.copy_from_slice(frame);
    state.requests_staged = at + CALL_FRAME;
    true
}

/// Admit, refuse, or forward one call.
fn route(state: &mut State, record: &CallRecord) {
    if record.binding >= ADMITTED {
        // A binding this deployment did not admit is refused here, and the
        // caller is told so rather than left waiting.
        state.refused = state.refused.saturating_add(1);
        let _ = stage_completion(
            state,
            &CompletionRecord {
                request: record.request,
                disposition: Disposition::Rejected,
                cause: Cause::Denied,
                trace: record.trace,
                value: None,
            },
        );
        return;
    }
    let Some(index) = state.table.iter().position(|slot| !slot.live) else {
        state.refused = state.refused.saturating_add(1);
        let _ = stage_completion(
            state,
            &CompletionRecord {
                request: record.request,
                disposition: Disposition::Rejected,
                cause: Cause::Busy,
                trace: record.trace,
                value: None,
            },
        );
        return;
    };
    if !stage_request(state, &record.encode()) {
        state.refused = state.refused.saturating_add(1);
        let _ = stage_completion(
            state,
            &CompletionRecord {
                request: record.request,
                disposition: Disposition::Rejected,
                cause: Cause::Busy,
                trace: record.trace,
                value: None,
            },
        );
        return;
    }
    if let Some(slot) = state.table.get_mut(index) {
        *slot = Outstanding {
            request: record.request,
            trace: record.trace,
            live: true,
        };
    }
    state.admitted = state.admitted.saturating_add(1);
}

/// Match an adapter's answer to the call it answers.
fn correlate(state: &mut State, record: &CompletionRecord) {
    let found = state
        .table
        .iter()
        .position(|slot| slot.live && slot.request == record.request);
    let Some(index) = found else {
        // An answer to something nobody asked is not routed anywhere.
        state.dropped = state.dropped.saturating_add(1);
        return;
    };
    let trace = state.table.get(index).map_or(0, |slot| slot.trace);
    if trace != record.trace {
        // The correlation must hold on both fields; a mismatch is a malformed
        // answer, and the caller hears that rather than the answer.
        state.dropped = state.dropped.saturating_add(1);
        if let Some(slot) = state.table.get_mut(index) {
            slot.live = false;
        }
        let _ = stage_completion(
            state,
            &CompletionRecord {
                request: record.request,
                disposition: Disposition::Rejected,
                cause: Cause::Malformed,
                trace,
                value: None,
            },
        );
        return;
    }
    if let Some(slot) = state.table.get_mut(index) {
        slot.live = false;
    }
    state.answered = state.answered.saturating_add(1);
    let _ = stage_completion(state, record);
}

/// Read whole frames off an input, one at a time.
fn take_frame(
    syscalls: &SyscallTable,
    channel: i32,
    buffer: *mut u8,
    filled: &mut usize,
    width: usize,
) -> bool {
    if channel < 0 {
        return false;
    }
    let poll = unsafe { (syscalls.channel_poll)(channel, POLL_INPUT) };
    if poll <= 0 || (poll as u32) & POLL_INPUT == 0 {
        return false;
    }
    let remaining = width.saturating_sub(*filled);
    if remaining == 0 {
        return true;
    }
    let read = unsafe { (syscalls.channel_read)(channel, buffer.add(*filled), remaining) };
    if read > 0 {
        *filled += usize::try_from(read).unwrap_or(0).min(remaining);
    }
    *filled == width
}

/// Push staged bytes out, and forget them once they are all gone.
fn push(
    syscalls: &SyscallTable,
    channel: i32,
    bytes: *const u8,
    staged: &mut usize,
    written: &mut usize,
) {
    if channel < 0 || *written >= *staged {
        return;
    }
    let poll = unsafe { (syscalls.channel_poll)(channel, POLL_OUTPUT) };
    if poll <= 0 || (poll as u32) & POLL_OUTPUT == 0 {
        return;
    }
    let remaining = staged.saturating_sub(*written);
    let count = unsafe { (syscalls.channel_write)(channel, bytes.add(*written), remaining) };
    if count > 0 {
        *written += usize::try_from(count).unwrap_or(0).min(remaining);
    }
    if *written >= *staged {
        *written = 0;
        *staged = 0;
    }
}

#[no_mangle]
#[link_section = ".text.module_state_size"]
pub extern "C" fn module_state_size() -> u32 {
    u32::try_from(core::mem::size_of::<State>()).unwrap_or(u32::MAX)
}

#[no_mangle]
#[link_section = ".text.module_init"]
pub extern "C" fn module_init(_syscalls: *const c_void) {}

#[no_mangle]
#[link_section = ".text.module_new"]
pub extern "C" fn module_new(
    in_chan: i32,
    out_chan: i32,
    _ctrl_chan: i32,
    _params: *const u8,
    _params_len: usize,
    state: *mut u8,
    state_size: usize,
    syscalls: *const c_void,
) -> i32 {
    if state.is_null() || syscalls.is_null() {
        return -1;
    }
    if state_size < core::mem::size_of::<State>() {
        return -2;
    }
    unsafe {
        let table = syscalls.cast::<SyscallTable>();
        let state = state.cast::<State>();
        core::ptr::write_bytes(state.cast::<u8>(), 0, core::mem::size_of::<State>());
        core::ptr::addr_of_mut!((*state).syscalls).write(table);
        core::ptr::addr_of_mut!((*state).call_in).write(in_chan);
        core::ptr::addr_of_mut!((*state).completion_out).write(out_chan);
        core::ptr::addr_of_mut!((*state).reply_in).write(dev_channel_port(&*table, 0, 1));
        core::ptr::addr_of_mut!((*state).request_out).write(dev_channel_port(&*table, 1, 1));
    }
    0
}

#[no_mangle]
#[link_section = ".text.module_step"]
pub extern "C" fn module_step(state: *mut u8) -> i32 {
    if state.is_null() {
        return -1;
    }
    let state = unsafe { &mut *state.cast::<State>() };
    if state.syscalls.is_null() || state.call_in < 0 || state.completion_out < 0 {
        return -2;
    }
    let syscalls = unsafe { &*state.syscalls };
    if state.phase == 1 {
        return 1;
    }
    // A call is taken only when there is room to answer it, which is what
    // stops an answer from ever being dropped for want of space.
    if state.completions_staged + COMPLETION_FRAME <= state.completions.len()
        && state.requests_staged + CALL_FRAME <= state.requests.len()
    {
        let buffer = state.call.as_mut_ptr();
        let mut filled = state.call_filled;
        let complete = take_frame(syscalls, state.call_in, buffer, &mut filled, CALL_FRAME);
        state.call_filled = filled;
        if complete {
            state.call_filled = 0;
            if let Some(record) = CallRecord::decode(&state.call) {
                route(state, &record);
            } else {
                state.dropped = state.dropped.saturating_add(1);
            }
        }
    }

    if state.completions_staged + COMPLETION_FRAME <= state.completions.len() {
        let buffer = state.reply.as_mut_ptr();
        let mut filled = state.reply_filled;
        let complete = take_frame(
            syscalls,
            state.reply_in,
            buffer,
            &mut filled,
            COMPLETION_FRAME,
        );
        state.reply_filled = filled;
        if complete {
            state.reply_filled = 0;
            if let Some(record) = CompletionRecord::decode(&state.reply) {
                correlate(state, &record);
            } else {
                state.dropped = state.dropped.saturating_add(1);
            }
        }
    }

    let requests = state.requests.as_ptr();
    let mut staged = state.requests_staged;
    let mut written = state.requests_written;
    push(
        syscalls,
        state.request_out,
        requests,
        &mut staged,
        &mut written,
    );
    state.requests_staged = staged;
    state.requests_written = written;

    let completions = state.completions.as_ptr();
    let mut staged = state.completions_staged;
    let mut written = state.completions_written;
    push(
        syscalls,
        state.completion_out,
        completions,
        &mut staged,
        &mut written,
    );
    state.completions_staged = staged;
    state.completions_written = written;

    // The router is finished when the isolate has hung up and everything
    // staged has left. A call still outstanding at that point has nobody left
    // to answer to; holding the graph open for it would help no one.
    if !state.hung_up {
        let poll = unsafe { (syscalls.channel_poll)(state.call_in, POLL_HUP) };
        if poll > 0 && (poll as u32) & POLL_HUP != 0 {
            state.hung_up = true;
        }
    }
    if state.hung_up
        && state.requests_staged == 0
        && state.completions_staged == 0
        && state.call_filled == 0
    {
        state.phase = 1;
        return 1;
    }
    0
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
