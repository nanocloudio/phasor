//! The router between an isolate's calls and an adapter that serves them.
//!
//! It admits a call by its binding index alone, correlates the answer with the
//! call by request identifier and trace, and bounds how many calls may be
//! outstanding. It never drops or reorders a completion: a call that cannot be
//! forwarded is answered here, with the reason, rather than left unanswered.
//!
//! A call's arguments follow its frame as bytes, and an answer's bytes follow
//! its completion the same way. The router carries both behind the frame they
//! belong to and reads neither: what they mean is between the program and the
//! adapter, and a frame is not forwarded until every byte behind it is here.
//!
//! A binding is an index the isolate admitted: `host` first, then one per
//! grant in the order the deployment stated them. An adapter knows its
//! operations by method number instead, which is the register's. The router
//! is where the one becomes the other: it is told the same grants and which
//! interface the adapter behind it serves, forwards a call on a member of
//! that interface under the member's method, and refuses a call on any other
//! interface as denied, because nothing behind it could answer. `host` is
//! forwarded as method 0, the raw call a graph with no grants makes.
//!
//! It holds no address and no credential. Which adapter serves the binding is
//! the graph's wiring, not this module's knowledge.

#![cfg_attr(not(feature = "host-test"), no_std)]
#![allow(
    dead_code,
    unused_imports,
    unreachable_patterns,
    reason = "the Fluxor ABI source is mounted as one surface and this module consumes a subset"
)]

#[path = "../../../target/fluxor/fluxor-abi/sdk/abi.rs"]
mod abi;
use abi::SyscallTable;

include!("../../../target/fluxor/fluxor-abi/sdk/runtime.rs");
include!("../../../target/fluxor/fluxor-abi/sdk/runtime/params.rs");

#[macro_use]
#[path = "../../common/entry.rs"]
mod entry;

#[path = "../../common/binding.rs"]
mod binding;
#[path = "../../common/digest.rs"]
mod digest;
#[path = "../../common/numeric.rs"]
mod numeric;
#[path = "../../common/register.rs"]
mod register;
#[path = "../../common/softfloat.rs"]
mod softfloat;
#[path = "../../common/value.rs"]
mod value;
#[path = "../../common/wire.rs"]
mod wire;

use binding::{
    Answer, CallRecord, Cause, CompletionRecord, Disposition, CALL_FRAME, COMPLETION_FRAME,
};

/// The binding every isolate admits first, forwarded as the first method.
const HOST_BINDING: u32 = 0;
/// Bindings an isolate admits: `host` and one per grant. An isolate stops
/// admitting at its own table's end, so a call above this names no binding
/// there and is refused here rather than counted past it.
const MAX_BINDINGS: u32 = 17;
/// Bytes of grants and of an interface identifier a graph may state, the
/// same bounds the isolate holds them under.
use register::GRANTS_BYTES;
const INTERFACE_BYTES: usize = register::MAX_INTERFACE;
/// Calls that may be outstanding at once. One more is refused as busy, which
/// the caller may retry.
const IN_FLIGHT: usize = 4;
/// Bytes of payload one call or one completion may carry behind its frame.
const PAYLOAD_BYTES: usize = 8 * 1024;
/// Frames that may be staged for each output at once, each with its payload.
const STAGE: usize = 4;

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
    announced: bool,
    call_in: i32,
    reply_in: i32,
    completion_out: i32,
    request_out: i32,
    table: [Outstanding; IN_FLIGHT],
    /// The call frame being taken, and its payload behind it.
    call: [u8; CALL_FRAME],
    call_filled: usize,
    call_payload: [u8; PAYLOAD_BYTES],
    call_payload_filled: usize,
    call_payload_length: usize,
    call_ready: bool,
    /// The completion frame being taken, and its payload behind it.
    reply: [u8; COMPLETION_FRAME],
    reply_filled: usize,
    reply_payload: [u8; PAYLOAD_BYTES],
    reply_payload_filled: usize,
    reply_payload_length: usize,
    reply_ready: bool,
    requests: [u8; (CALL_FRAME + PAYLOAD_BYTES) * STAGE],
    requests_staged: usize,
    requests_written: usize,
    completions: [u8; (COMPLETION_FRAME + PAYLOAD_BYTES) * STAGE],
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
    /// The interface the adapter behind this router serves.
    interface: [u8; INTERFACE_BYTES],
    interface_length: usize,
    /// The grants the isolate was given, in the order that numbers them.
    grants: [u8; GRANTS_BYTES],
    grants_length: usize,
}

define_params! {
    State;

    1, interface, str, 0
        => |s, d, len| {
            let taken = if len > INTERFACE_BYTES { INTERFACE_BYTES } else { len };
            s.interface_length = taken;
            if taken > 0 {
                // SAFETY: the params reader hands a pointer valid for `len`
                // bytes, and `taken` is no larger than it or the field.
                unsafe {
                    core::ptr::copy_nonoverlapping(d, s.interface.as_mut_ptr(), taken);
                }
            }
        };
    2, grants, str, 0
        => |s, d, len| {
            let taken = if len > GRANTS_BYTES { GRANTS_BYTES } else { len };
            s.grants_length = taken;
            if taken > 0 {
                // SAFETY: as for `interface`.
                unsafe {
                    core::ptr::copy_nonoverlapping(d, s.grants.as_mut_ptr(), taken);
                }
            }
        };
}

/// Take the graph's parameters, or the defaults where it gave none.
///
/// # Safety
/// `params` must be valid for reads of `params_len` bytes, or null.
unsafe fn apply_params(state: &mut State, params: *const u8, params_len: usize) {
    if wire::params_are_tlv(params, params_len, TLV_MAGIC, TLV_VERSION) {
        parse_tlv(state, params, params_len);
    } else {
        set_defaults(state);
    }
}

/// The method the adapter behind this router answers `binding` under, or
/// nothing when the binding is not one it can serve.
///
/// `host` is method 0. Any other binding is the grant at that position,
/// counted from one; it is served when its interface is the one this router
/// was told it serves and the register knows the member.
fn method_of(state: &State, binding: u32) -> Option<u32> {
    if binding == HOST_BINDING {
        return Some(0);
    }
    if binding >= MAX_BINDINGS {
        return None;
    }
    let interface = state.interface.get(..state.interface_length).unwrap_or(&[]);
    if interface.is_empty() {
        return None;
    }
    let names = state.grants.get(..state.grants_length).unwrap_or(&[]);
    let mut index = HOST_BINDING;
    let mut found = None;
    register::grants(names, |granted, member| {
        index += 1;
        if index == binding && granted == interface {
            found = register::member_of(interface, member).map(|member| member.method);
        }
    });
    found
}

fn in_flight(state: &State) -> usize {
    state.table.iter().filter(|slot| slot.live).count()
}

/// Copy `src` into `dst` when they are the same length, answering whether
/// they were: a copy whose lengths the compiler cannot prove equal would
/// carry a panic path, and a module image carries none.
fn copy_into(dst: &mut [u8], src: &[u8]) -> bool {
    if dst.len() != src.len() {
        return false;
    }
    dst.copy_from_slice(src);
    true
}

/// Stage a completion for the isolate, with the bytes it answers with behind
/// it. A staging buffer that is full is not a reason to drop an answer: the
/// router stops taking calls instead.
fn stage_completion(state: &mut State, record: &CompletionRecord, payload: &[u8]) -> bool {
    let at = state.completions_staged;
    let frame = record.encode();
    let end = at + COMPLETION_FRAME + payload.len();
    if end > state.completions.len() {
        return false;
    }
    if !copy_into(
        state
            .completions
            .get_mut(at..at + COMPLETION_FRAME)
            .unwrap_or(&mut []),
        &frame,
    ) || !copy_into(
        state
            .completions
            .get_mut(at + COMPLETION_FRAME..end)
            .unwrap_or(&mut []),
        payload,
    ) {
        return false;
    }
    state.completions_staged = end;
    true
}

/// Stage a call for the adapter, with its arguments behind it.
fn stage_request(state: &mut State, frame: &[u8; CALL_FRAME], payload: &[u8]) -> bool {
    let at = state.requests_staged;
    let end = at + CALL_FRAME + payload.len();
    if end > state.requests.len() {
        return false;
    }
    if !copy_into(
        state
            .requests
            .get_mut(at..at + CALL_FRAME)
            .unwrap_or(&mut []),
        frame,
    ) || !copy_into(
        state
            .requests
            .get_mut(at + CALL_FRAME..end)
            .unwrap_or(&mut []),
        payload,
    ) {
        return false;
    }
    state.requests_staged = end;
    true
}

/// Admit, refuse, or forward one call, with the bytes behind it.
fn route(state: &mut State, record: &CallRecord, payload: &[u8]) {
    let Some(method) = method_of(state, record.binding) else {
        // A binding nothing behind this router serves is refused here, and
        // the caller is told so rather than left waiting.
        state.refused = state.refused.saturating_add(1);
        let _ = stage_completion(
            state,
            &CompletionRecord {
                request: record.request,
                disposition: Disposition::Rejected,
                cause: Cause::Denied,
                trace: record.trace,
                answer: Answer::None,
            },
            &[],
        );
        return;
    };
    let Some(index) = state.table.iter().position(|slot| !slot.live) else {
        state.refused = state.refused.saturating_add(1);
        let _ = stage_completion(
            state,
            &CompletionRecord {
                request: record.request,
                disposition: Disposition::Rejected,
                cause: Cause::Busy,
                trace: record.trace,
                answer: Answer::None,
            },
            &[],
        );
        return;
    };
    let mut forwarded = *record;
    forwarded.binding = method;
    if !stage_request(state, &forwarded.encode(), payload) {
        state.refused = state.refused.saturating_add(1);
        let _ = stage_completion(
            state,
            &CompletionRecord {
                request: record.request,
                disposition: Disposition::Rejected,
                cause: Cause::Busy,
                trace: record.trace,
                answer: Answer::None,
            },
            &[],
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

/// Match an adapter's answer to the call it answers, and carry its bytes on.
fn correlate(state: &mut State, record: &CompletionRecord, payload: &[u8]) {
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
                answer: Answer::None,
            },
            &[],
        );
        return;
    }
    if let Some(slot) = state.table.get_mut(index) {
        slot.live = false;
    }
    state.answered = state.answered.saturating_add(1);
    let _ = stage_completion(state, record, payload);
}

entry! {
    State;
    primary { call_in, completion_out }
    inputs { reply_in = 1 }
    outputs { request_out = 1 }
    params apply_params
}

#[cfg_attr(not(feature = "host-test"), unsafe(no_mangle))]
#[link_section = ".text.module_step"]
pub extern "C" fn module_step(state: *mut u8) -> i32 {
    if state.is_null() {
        return -1;
    }
    // SAFETY: `state` is the block `module_new` laid out, non-null as
    // checked above, and the loader hands it to one step at a time.
    let state = unsafe { &mut *state.cast::<State>() };
    if state.syscalls.is_null() || state.call_in < 0 || state.completion_out < 0 {
        return -2;
    }
    // SAFETY: the table pointer was stored by `module_new` and checked
    // non-null above; the loader keeps it live for the module's lifetime.
    let syscalls = unsafe { &*state.syscalls };
    announce_ready!(state);
    if state.phase == 1 {
        return 1;
    }
    // A call is taken only when there is room to answer it, which is what
    // stops an answer from ever being dropped for want of space.
    if state.completions_staged + COMPLETION_FRAME <= state.completions.len()
        && state.requests_staged + CALL_FRAME + PAYLOAD_BYTES <= state.requests.len()
    {
        if !state.call_ready
            && wire::take_frame(
                syscalls,
                state.call_in,
                &mut state.call,
                &mut state.call_filled,
            )
        {
            state.call_filled = 0;
            state.call_ready = true;
            state.call_payload_filled = 0;
            state.call_payload_length = CallRecord::decode(&state.call)
                .map_or(0, |record| record.payload_length as usize)
                .min(PAYLOAD_BYTES);
        }
        // The arguments follow their frame: until every byte is here, the
        // call has not arrived.
        if state.call_ready
            && wire::take_payload(
                syscalls,
                state.call_in,
                &mut state.call_payload,
                &mut state.call_payload_filled,
                state.call_payload_length,
            )
        {
            state.call_ready = false;
            let length = state.call_payload_length;
            let mut payload = [0u8; PAYLOAD_BYTES];
            copy_into(
                payload.get_mut(..length).unwrap_or(&mut []),
                state.call_payload.get(..length).unwrap_or(&[]),
            );
            if let Some(record) = CallRecord::decode(&state.call) {
                route(state, &record, payload.get(..length).unwrap_or(&[]));
            } else {
                state.dropped = state.dropped.saturating_add(1);
            }
            state.call_payload_filled = 0;
            state.call_payload_length = 0;
        }
    }

    if state.completions_staged + COMPLETION_FRAME + PAYLOAD_BYTES <= state.completions.len() {
        if !state.reply_ready
            && wire::take_frame(
                syscalls,
                state.reply_in,
                &mut state.reply,
                &mut state.reply_filled,
            )
        {
            state.reply_filled = 0;
            state.reply_ready = true;
            state.reply_payload_filled = 0;
            state.reply_payload_length = CompletionRecord::decode(&state.reply)
                .map_or(0, |record| record.answer.payload_length() as usize)
                .min(PAYLOAD_BYTES);
        }
        if state.reply_ready
            && wire::take_payload(
                syscalls,
                state.reply_in,
                &mut state.reply_payload,
                &mut state.reply_payload_filled,
                state.reply_payload_length,
            )
        {
            state.reply_ready = false;
            let length = state.reply_payload_length;
            let mut payload = [0u8; PAYLOAD_BYTES];
            copy_into(
                payload.get_mut(..length).unwrap_or(&mut []),
                state.reply_payload.get(..length).unwrap_or(&[]),
            );
            if let Some(record) = CompletionRecord::decode(&state.reply) {
                correlate(state, &record, payload.get(..length).unwrap_or(&[]));
            } else {
                state.dropped = state.dropped.saturating_add(1);
            }
            state.reply_payload_filled = 0;
            state.reply_payload_length = 0;
        }
    }

    wire::push_staged(
        syscalls,
        state.request_out,
        &state.requests,
        &mut state.requests_staged,
        &mut state.requests_written,
    );
    wire::push_staged(
        syscalls,
        state.completion_out,
        &state.completions,
        &mut state.completions_staged,
        &mut state.completions_written,
    );

    // The router is finished when the isolate has hung up and everything
    // staged has left. A call still outstanding at that point has nobody left
    // to answer to; holding the graph open for it would help no one.
    if !state.hung_up && wire::hung_up(syscalls, state.call_in) {
        state.hung_up = true;
    }
    if state.hung_up
        && state.requests_staged == 0
        && state.completions_staged == 0
        && state.call_filled == 0
        && !state.call_ready
    {
        state.phase = 1;
        return 1;
    }
    0
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
