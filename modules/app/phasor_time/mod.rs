//! The clock, as a capability rather than an ambient power.
//!
//! An isolate has no clock. A deployment that wants a program to read the time
//! wires this adapter behind the router, and the program reaches it only
//! through the binding it was granted. What it answers is a sample taken at the
//! moment the call is served, in the unit its parameter names.
//!
//! Nothing here converts the sample into an ECMAScript value: what crosses the
//! boundary is a number, and what the language makes of it is the engine's.

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
include!("../../../target/fluxor/fluxor-abi/sdk/runtime/params.rs");

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

/// Which observation a call is answered with.
const SOURCE_MONOTONIC_MS: u8 = 0;
const SOURCE_MONOTONIC_US: u8 = 1;
const SOURCE_UNIX_MS: u8 = 2;

const STAGE: usize = 8;

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    request_in: i32,
    reply_out: i32,
    request: [u8; CALL_FRAME],
    filled: usize,
    replies: [u8; COMPLETION_FRAME * STAGE],
    staged: usize,
    written: usize,
    answered: u64,
    source: u8,
    /// Calls this adapter will answer before it refuses. Zero admits every
    /// call; a deployment that wants a program to read the time a bounded
    /// number of times says so here.
    quota: u32,
    hung_up: bool,
    phase: u8,
}

define_params! {
    State;

    1, source, u8, 0, enum { monotonic_ms=0, monotonic_us=1, unix_ms=2 }
        => |s, d, len| { s.source = p_u8(d, len, 0, 0); };
    2, quota, u32, 0
        => |s, d, len| { s.quota = p_u32(d, len, 0, 0); };
}

/// Take the graph's parameters, or the defaults where it gave none.
///
/// # Safety
/// `params` must be valid for reads of `params_len` bytes, or null.
unsafe fn apply_params(state: &mut State, params: *const u8, params_len: usize) {
    let tlv = !params.is_null()
        && params_len >= 4
        && *params == TLV_MAGIC
        && *params.add(1) == TLV_VERSION;
    if tlv {
        parse_tlv(state, params, params_len);
    } else {
        set_defaults(state);
    }
}

/// The answer to one call: a sample, or a refusal when the quota is spent.
fn answer(state: &State, record: &CallRecord, syscalls: &SyscallTable) -> CompletionRecord {
    if state.quota != 0 && state.answered >= u64::from(state.quota) {
        return CompletionRecord {
            request: record.request,
            disposition: Disposition::Rejected,
            cause: Cause::Denied,
            trace: record.trace,
            value: None,
        };
    }
    let sample = unsafe {
        match state.source {
            SOURCE_MONOTONIC_US => dev_micros(syscalls),
            SOURCE_UNIX_MS => dev_unix_millis(syscalls),
            _ => dev_millis(syscalls),
        }
    };
    CompletionRecord {
        request: record.request,
        disposition: Disposition::Fulfilled,
        cause: Cause::None,
        trace: record.trace,
        value: Some(softfloat::from_u64(sample)),
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

#[allow(
    clippy::not_unsafe_ptr_arg_deref,
    reason = "the ABI fixes this signature: the loader passes the parameter block as a raw pointer and length, and the module reads it once under the contract that it is valid for that length"
)]
#[no_mangle]
#[link_section = ".text.module_new"]
pub extern "C" fn module_new(
    in_chan: i32,
    out_chan: i32,
    _ctrl_chan: i32,
    params: *const u8,
    params_len: usize,
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
        core::ptr::addr_of_mut!((*state).request_in).write(in_chan);
        core::ptr::addr_of_mut!((*state).reply_out).write(out_chan);
        apply_params(&mut *state, params, params_len);
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
    if state.syscalls.is_null() || state.request_in < 0 || state.reply_out < 0 {
        return -2;
    }
    let syscalls = unsafe { &*state.syscalls };
    if state.phase == 1 {
        return 1;
    }
    // One request is taken only when there is room for its answer.
    if state.staged + COMPLETION_FRAME <= state.replies.len() {
        let poll = unsafe { (syscalls.channel_poll)(state.request_in, POLL_INPUT) };
        if poll > 0 && (poll as u32) & POLL_INPUT != 0 {
            let remaining = CALL_FRAME.saturating_sub(state.filled);
            if remaining > 0 {
                let read = unsafe {
                    (syscalls.channel_read)(
                        state.request_in,
                        state.request.as_mut_ptr().add(state.filled),
                        remaining,
                    )
                };
                if read > 0 {
                    state.filled += usize::try_from(read).unwrap_or(0).min(remaining);
                }
            }
            if state.filled == CALL_FRAME {
                state.filled = 0;
                if let Some(record) = CallRecord::decode(&state.request) {
                    let reply = answer(state, &record, syscalls);
                    let at = state.staged;
                    let frame = reply.encode();
                    if let Some(slot) = state.replies.get_mut(at..at + COMPLETION_FRAME) {
                        slot.copy_from_slice(&frame);
                        state.staged = at + COMPLETION_FRAME;
                    }
                    state.answered = state.answered.saturating_add(1);
                }
            }
        }
    }

    if state.written < state.staged {
        let poll = unsafe { (syscalls.channel_poll)(state.reply_out, POLL_OUTPUT) };
        if poll > 0 && (poll as u32) & POLL_OUTPUT != 0 {
            let remaining = state.staged.saturating_sub(state.written);
            let count = unsafe {
                (syscalls.channel_write)(
                    state.reply_out,
                    state.replies.as_ptr().add(state.written),
                    remaining,
                )
            };
            if count > 0 {
                state.written += usize::try_from(count).unwrap_or(0).min(remaining);
            }
            if state.written >= state.staged {
                state.written = 0;
                state.staged = 0;
            }
        }
    }

    if !state.hung_up {
        let poll = unsafe { (syscalls.channel_poll)(state.request_in, POLL_HUP) };
        if poll > 0 && (poll as u32) & POLL_HUP != 0 {
            state.hung_up = true;
        }
    }
    if state.hung_up && state.staged == 0 && state.filled == 0 {
        state.phase = 1;
        return 1;
    }
    0
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
