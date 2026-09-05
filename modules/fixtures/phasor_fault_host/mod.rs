//! A provider that answers a router's calls the way a graph asks it to.
//!
//! It exists so a deployment can be tested against the answers a real provider
//! may give: a value, a refusal, a failure it is worth retrying, or nothing at
//! all. It reads nothing about the caller beyond the frame it was handed.

#![cfg_attr(not(feature = "host-test"), no_std)]
#![allow(
    dead_code,
    unused_imports,
    unreachable_patterns,
    reason = "the Fluxor ABI source is mounted as one surface and this fixture consumes a subset"
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

/// Answers this adapter can give.
const MODE_ANSWER: u8 = 0;
const MODE_DENY: u8 = 1;
const MODE_UNAVAILABLE: u8 = 2;
const MODE_SILENT: u8 = 3;

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
    mode: u8,
    value: u32,
    /// Requests to answer normally before the mode takes effect, so a graph can
    /// ask for a good answer and then a bad one.
    grace: u32,
    hung_up: bool,
    phase: u8,
}

define_params! {
    State;

    1, mode, u8, 0, enum { answer=0, deny=1, unavailable=2, silent=3 }
        => |s, d, len| { s.mode = p_u8(d, len, 0, 0); };
    2, value, u32, 41
        => |s, d, len| { s.value = p_u32(d, len, 0, 41); };
    3, grace, u32, 0
        => |s, d, len| { s.grace = p_u32(d, len, 0, 0); };
}

/// The answer this adapter gives to one call.
fn answer(state: &State, record: &CallRecord) -> Option<CompletionRecord> {
    let mode = if state.answered < u64::from(state.grace) {
        MODE_ANSWER
    } else {
        state.mode
    };
    let (disposition, cause, value) = match mode {
        MODE_DENY => (Disposition::Rejected, Cause::Denied, None),
        MODE_UNAVAILABLE => (Disposition::Rejected, Cause::Unavailable, None),
        MODE_SILENT => return None,
        _ => (
            Disposition::Fulfilled,
            Cause::None,
            Some(f64::from(state.value)),
        ),
    };
    Some(CompletionRecord {
        request: record.request,
        disposition,
        cause,
        trace: record.trace,
        value,
    })
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
            if let Some(reply) = answer(state, &record) {
                let at = state.staged;
                let frame = reply.encode();
                if let Some(slot) = state.replies.get_mut(at..at + COMPLETION_FRAME) {
                    slot.copy_from_slice(&frame);
                    state.staged = at + COMPLETION_FRAME;
                }
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
