//! The clock, as a capability rather than an ambient power.
//!
//! An isolate has no clock. A deployment that wants a program to read the time
//! wires this adapter behind the router, and the program reaches it only
//! through the binding it was granted. What it answers is a sample taken at the
//! moment the call is served, in the unit its parameter names.
//!
//! Nothing here converts the sample into an ECMAScript value: what crosses the
//! boundary is a number, and what the language makes of it is the engine's.

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
#[path = "../../common/softfloat.rs"]
mod softfloat;
#[path = "../../common/value.rs"]
mod value;
#[path = "../../common/wire.rs"]
mod wire;

use binding::{CallRecord, Cause, CompletionRecord, Disposition, CALL_FRAME, COMPLETION_FRAME};

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
    if wire::params_are_tlv(params, params_len, TLV_MAGIC, TLV_VERSION) {
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
    // SAFETY: each sample is one call through the loader's syscall table, live
    // for the module's lifetime.
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

entry! {
    State;
    primary { request_in, reply_out }
    inputs {}
    outputs {}
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
    if state.syscalls.is_null() || state.request_in < 0 || state.reply_out < 0 {
        return -2;
    }
    // SAFETY: the table pointer was stored by `module_new` and checked
    // non-null above; the loader keeps it live for the module's lifetime.
    let syscalls = unsafe { &*state.syscalls };
    if state.phase == 1 {
        return 1;
    }
    // One request is taken only when there is room for its answer.
    if state.staged + COMPLETION_FRAME <= state.replies.len()
        && wire::take_frame(
            syscalls,
            state.request_in,
            &mut state.request,
            &mut state.filled,
        )
    {
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

    wire::push_staged(
        syscalls,
        state.reply_out,
        &state.replies,
        &mut state.staged,
        &mut state.written,
    );

    if !state.hung_up && wire::hung_up(syscalls, state.request_in) {
        state.hung_up = true;
    }
    if state.hung_up && state.staged == 0 && state.filled == 0 {
        state.phase = 1;
        return 1;
    }
    0
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
