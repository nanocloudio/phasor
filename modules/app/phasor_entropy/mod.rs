//! Entropy, as a capability rather than an ambient power.
//!
//! An isolate has no randomness — there is no `Math.random`, because a program
//! that could help itself to unpredictability could also help itself to a
//! covert channel. A deployment that wants a program to have some wires this
//! adapter behind the router, and the program reaches it only through the
//! binding it was granted.
//!
//! What it answers is a seed: bytes the platform's own source produced, taken
//! at the moment the call is served, as an integer the engine can carry. What
//! a program makes of a seed is the program's business.

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

use binding::{
    Answer, CallRecord, Cause, CompletionRecord, Disposition, CALL_FRAME, COMPLETION_FRAME,
};

/// How wide a seed one call answers with.
const WIDTH_32: u8 = 0;
const WIDTH_53: u8 = 1;

const STAGE: usize = 8;

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    announced: bool,
    request_in: i32,
    reply_out: i32,
    request: [u8; CALL_FRAME],
    filled: usize,
    replies: [u8; COMPLETION_FRAME * STAGE],
    staged: usize,
    written: usize,
    answered: u64,
    width: u8,
    /// Calls this adapter will answer before it refuses. Zero admits every
    /// call; a deployment that wants a program to have a bounded amount of
    /// entropy says so here.
    quota: u32,
    hung_up: bool,
    phase: u8,
}

define_params! {
    State;

    1, width, u8, 0, enum { bits32=0, bits53=1 }
        => |s, d, len| { s.width = p_u8(d, len, 0, 0); };
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

/// The answer to one call: a seed, or a refusal when the quota is spent.
fn answer(state: &State, record: &CallRecord, syscalls: &SyscallTable) -> CompletionRecord {
    if state.quota != 0 && state.answered >= u64::from(state.quota) {
        return CompletionRecord {
            request: record.request,
            disposition: Disposition::Rejected,
            cause: Cause::Denied,
            trace: record.trace,
            answer: Answer::None,
        };
    }
    let mut bytes = [0u8; 8];
    let width = if state.width == WIDTH_53 { 7 } else { 4 };
    // SAFETY: `bytes` is writable for `width` bytes, and the provider call is
    // the kernel's own, through the loader's table.
    let filled = unsafe {
        (syscalls.provider_call)(-1, abi::kernel_abi::RANDOM_FILL, bytes.as_mut_ptr(), width)
    };
    // A platform answers either the count it filled or zero for success; a
    // negative answer is a source that could not produce anything.
    if filled < 0 {
        // A source that could not answer is unavailable, not empty: a seed of
        // zero is worse than no seed at all.
        return CompletionRecord {
            request: record.request,
            disposition: Disposition::Rejected,
            cause: Cause::Unavailable,
            trace: record.trace,
            answer: Answer::None,
        };
    }
    let seed = u64::from_le_bytes(bytes);
    CompletionRecord {
        request: record.request,
        disposition: Disposition::Fulfilled,
        cause: Cause::None,
        trace: record.trace,
        answer: Answer::Number(softfloat::from_u64(seed)),
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
    announce_ready!(state);
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
