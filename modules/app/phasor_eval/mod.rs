//! Bounded Phasor seed evaluator as a Fluxor transformer module.

#![cfg_attr(not(feature = "host-test"), no_std)]
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

#[path = "../../common/eval_core.rs"]
mod eval_core;
use eval_core::{evaluate, EvalError};

const SOURCE_CAPACITY: usize = 256;
const RESULT_CAPACITY: usize = 64;
const FUEL_LIMIT: u32 = 64;
const POLL_INPUT: u32 = 0x01;
const POLL_OUTPUT: u32 = 0x02;

#[repr(C)]
struct ModuleState {
    syscalls: *const SyscallTable,
    source_in: i32,
    result_out: i32,
    exit_out: i32,
    phase: u8,
    pending_len: u16,
    pending_offset: u16,
    source_len: u16,
    source_overflow: bool,
    result: [u8; RESULT_CAPACITY],
    source: [u8; SOURCE_CAPACITY],
    bytes_in: u32,
    bytes_out: u32,
    evaluations: u32,
    errors: u32,
    budget_exhausted: u32,
    bp_steps: u32,
}

impl ModuleState {
    const fn new(syscalls: *const SyscallTable, source_in: i32, result_out: i32) -> Self {
        Self {
            syscalls,
            source_in,
            result_out,
            exit_out: -1,
            phase: 0,
            pending_len: 0,
            pending_offset: 0,
            source_len: 0,
            source_overflow: false,
            result: [0; RESULT_CAPACITY],
            source: [0; SOURCE_CAPACITY],
            bytes_in: 0,
            bytes_out: 0,
            evaluations: 0,
            errors: 0,
            budget_exhausted: 0,
            bp_steps: 0,
        }
    }
}

fn encode_error(error: EvalError, output: &mut [u8]) -> usize {
    let token = error.token();
    let prefix = b"error:";
    let required = prefix.len() + token.len() + 1;
    if required > output.len() {
        return 0;
    }
    output[..prefix.len()].copy_from_slice(prefix);
    output[prefix.len()..prefix.len() + token.len()].copy_from_slice(token);
    output[required - 1] = b'\n';
    required
}

unsafe fn flush_pending(state: &mut ModuleState, syscalls: &SyscallTable) -> bool {
    if state.pending_len == 0 {
        return true;
    }
    let offset = usize::from(state.pending_offset);
    let remaining = usize::from(state.pending_len);
    let written =
        (syscalls.channel_write)(state.result_out, state.result[offset..].as_ptr(), remaining);
    if written <= 0 {
        state.bp_steps = state.bp_steps.saturating_add(1);
        return false;
    }
    let count = usize::try_from(written).unwrap_or(0).min(remaining);
    state.bytes_out = state
        .bytes_out
        .saturating_add(u32::try_from(count).unwrap_or(u32::MAX));
    state.pending_offset = state
        .pending_offset
        .saturating_add(u16::try_from(count).unwrap_or(u16::MAX));
    state.pending_len = state
        .pending_len
        .saturating_sub(u16::try_from(count).unwrap_or(u16::MAX));
    if state.pending_len == 0 {
        state.pending_offset = 0;
        true
    } else {
        state.bp_steps = state.bp_steps.saturating_add(1);
        false
    }
}

#[cfg_attr(not(feature = "host-test"), unsafe(no_mangle))]
#[link_section = ".text.module_state_size"]
pub extern "C" fn module_state_size() -> u32 {
    u32::try_from(core::mem::size_of::<ModuleState>()).unwrap_or(u32::MAX)
}

#[cfg_attr(not(feature = "host-test"), unsafe(no_mangle))]
#[link_section = ".text.module_init"]
pub extern "C" fn module_init(_syscalls: *const c_void) {}

#[cfg_attr(not(feature = "host-test"), unsafe(no_mangle))]
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
    if state_size < core::mem::size_of::<ModuleState>() {
        return -2;
    }

    // SAFETY: the Fluxor ABI supplies an aligned, writable state arena of at
    // least `state_size` bytes and owns it for this module's lifetime.
    unsafe {
        core::ptr::write(
            state.cast::<ModuleState>(),
            ModuleState::new(syscalls.cast::<SyscallTable>(), in_chan, out_chan),
        );
        let initialized = &mut *state.cast::<ModuleState>();
        initialized.exit_out = dev_channel_port(&*initialized.syscalls, 1, 1);
    }
    0
}

#[cfg_attr(not(feature = "host-test"), unsafe(no_mangle))]
#[link_section = ".text.module_step"]
pub extern "C" fn module_step(state: *mut u8) -> i32 {
    if state.is_null() {
        return -1;
    }

    // SAFETY: `module_new` initialized the arena as `ModuleState`; Fluxor owns
    // the allocation and does not invoke this module concurrently.
    let state = unsafe { &mut *state.cast::<ModuleState>() };
    if state.syscalls.is_null() || state.source_in < 0 || state.result_out < 0 {
        return -2;
    }
    // SAFETY: the syscall table is supplied by Fluxor and remains live for the
    // module instance's lifetime.
    let syscalls = unsafe { &*state.syscalls };

    if state.phase == 2 {
        return 1;
    }

    // SAFETY: pending offsets and lengths are maintained within `result` by
    // this module, and the syscall table follows the Fluxor channel ABI.
    if !unsafe { flush_pending(state, syscalls) } {
        return 0;
    }
    if state.phase == 1 {
        if state.exit_out >= 0 {
            let code = 0i32.to_le_bytes();
            // SAFETY: `code` is a live four-byte buffer and `exit_out` is the
            // output handle Fluxor resolved for port index 1.
            let written =
                unsafe { (syscalls.channel_write)(state.exit_out, code.as_ptr(), code.len()) };
            if written != i32::try_from(code.len()).unwrap_or(i32::MAX) {
                return 0;
            }
        }
        state.phase = 2;
        return 1;
    }

    // SAFETY: both channel handles were supplied by Fluxor for this instance.
    let output_poll = unsafe { (syscalls.channel_poll)(state.result_out, POLL_OUTPUT) };
    if output_poll <= 0 || (output_poll as u32) & POLL_OUTPUT == 0 {
        state.bp_steps = state.bp_steps.saturating_add(1);
        return 0;
    }
    // SAFETY: both channel handles were supplied by Fluxor for this instance.
    let input_poll = unsafe { (syscalls.channel_poll)(state.source_in, POLL_INPUT | POLL_HUP) };
    if input_poll <= 0 {
        return 0;
    }

    if (input_poll as u32) & POLL_INPUT != 0 {
        let offset = usize::from(state.source_len);
        let remaining = state.source.len().saturating_sub(offset);
        let mut overflow = [0u8; 64];
        let (buffer, capacity) = if remaining == 0 {
            state.source_overflow = true;
            (overflow.as_mut_ptr(), overflow.len())
        } else {
            // SAFETY: offset is derived from source_len and capped by remaining.
            (unsafe { state.source.as_mut_ptr().add(offset) }, remaining)
        };
        // SAFETY: buffer is writable for capacity bytes and the input handle
        // belongs to this module instance.
        let read = unsafe { (syscalls.channel_read)(state.source_in, buffer, capacity) };
        if read > 0 {
            let count = usize::try_from(read).unwrap_or(0).min(capacity);
            state.bytes_in = state
                .bytes_in
                .saturating_add(u32::try_from(count).unwrap_or(u32::MAX));
            if remaining != 0 {
                state.source_len = state
                    .source_len
                    .saturating_add(u16::try_from(count).unwrap_or(u16::MAX));
            }
        }
        return 0;
    }

    if (input_poll as u32) & POLL_HUP == 0 {
        return 0;
    }

    let source_len = usize::from(state.source_len).min(state.source.len());

    let evaluated = if state.source_overflow {
        Err(EvalError::SourceTooLarge)
    } else {
        evaluate(
            &state.source[..source_len],
            FUEL_LIMIT,
            &mut state.result[..RESULT_CAPACITY - 1],
        )
    };
    let result_len = match evaluated {
        Ok(length) => {
            state.result[length] = b'\n';
            state.evaluations = state.evaluations.saturating_add(1);
            length + 1
        }
        Err(error) => {
            state.errors = state.errors.saturating_add(1);
            if error == EvalError::BudgetExceeded {
                state.budget_exhausted = state.budget_exhausted.saturating_add(1);
            }
            encode_error(error, &mut state.result)
        }
    };

    state.pending_len = u16::try_from(result_len).unwrap_or(0);
    state.pending_offset = 0;
    state.phase = 1;
    // SAFETY: the just-encoded pending range is within `result`.
    let _ = unsafe { flush_pending(state, syscalls) };
    0
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
