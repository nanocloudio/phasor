//! On-graph conformance probe for the bounded seed evaluator.

#![no_std]
#![allow(
    dead_code,
    unused_imports,
    unreachable_patterns,
    reason = "the Fluxor ABI source is mounted as one surface and this fixture consumes a subset"
)]

use core::ffi::c_void;

#[path = "../../../target/fluxor/fluxor-abi/sdk/abi.rs"]
mod abi;
use abi::SyscallTable;

include!("../../../target/fluxor/fluxor-abi/sdk/runtime.rs");

#[path = "../../common/eval_core.rs"]
mod eval_core;
use eval_core::{evaluate, EvalError};

const CASE_COUNT: u16 = 8;

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    report_out: i32,
    exit_out: i32,
    case: u16,
    byte: u16,
    failures: u16,
    phase: u8,
}

fn evaluates_to(source: &[u8], fuel: u32, expected: &[u8]) -> bool {
    let mut output = [0u8; 32];
    match evaluate(source, fuel, &mut output) {
        Ok(length) => output.get(..length) == Some(expected),
        Err(_) => false,
    }
}

fn errors_with(source: &[u8], fuel: u32, expected: EvalError) -> bool {
    let mut output = [0u8; 32];
    evaluate(source, fuel, &mut output) == Err(expected)
}

fn run_case(case: u16) -> bool {
    match case {
        0 => evaluates_to(b" 40 + 2\t", 32, b"42"),
        1 => evaluates_to(b"-10 - - 4 + +2", 64, b"-4"),
        2 => errors_with(b"        1", 4, EvalError::BudgetExceeded),
        3 => errors_with(b"6 * 7", 32, EvalError::InvalidToken),
        4 => errors_with(b"2147483647 + 1", 64, EvalError::NumericOverflow),
        5 => {
            let mut output = [0u8; 1];
            evaluate(b"40 + 2", 32, &mut output) == Err(EvalError::OutputTooSmall)
        }
        6 => {
            EvalError::BudgetExceeded.token() == b"budget"
                && EvalError::NumericOverflow.token() == b"numeric-overflow"
                && EvalError::SourceTooLarge.token() == b"source-too-large"
        }
        _ => true,
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
    _in_chan: i32,
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
        core::ptr::write(
            state.cast::<State>(),
            State {
                syscalls: table,
                report_out: out_chan,
                exit_out: dev_channel_port(&*table, 1, 1),
                case: 0,
                byte: 0,
                failures: 0,
                phase: 0,
            },
        );
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
    if state.syscalls.is_null() {
        return -2;
    }
    let syscalls = unsafe { &*state.syscalls };

    if state.phase == 2 {
        return 1;
    }
    if state.case < CASE_COUNT - 1 {
        if !run_case(state.case) {
            state.failures = state.failures.saturating_add(1);
        }
        state.case = state.case.saturating_add(1);
        return 0;
    }
    if state.byte <= u16::from(u8::MAX) {
        let mut output = [0u8; 16];
        let input = [u8::try_from(state.byte).unwrap_or(u8::MAX)];
        let _ = evaluate(&input, 2, &mut output);
        state.byte = state.byte.saturating_add(1);
        return 0;
    }

    if state.phase == 0 {
        let report: &[u8] = if state.failures == 0 {
            b"phasor-eval-probe: 8 passed\n"
        } else {
            b"phasor-eval-probe: failed\n"
        };
        // A graph that gives the probe no report port still runs it; the
        // outcome then shows in the module's own completion status.
        if state.report_out >= 0 {
            let written = unsafe {
                (syscalls.channel_write)(state.report_out, report.as_ptr(), report.len())
            };
            if written != i32::try_from(report.len()).unwrap_or(i32::MAX) {
                return 0;
            }
        }
        state.phase = 1;
    }

    if state.exit_out >= 0 {
        let code = i32::from(state.failures != 0).to_le_bytes();
        let written =
            unsafe { (syscalls.channel_write)(state.exit_out, code.as_ptr(), code.len()) };
        if written != i32::try_from(code.len()).unwrap_or(i32::MAX) {
            return 0;
        }
    }
    state.phase = 2;
    // Completing is the pass signal for a graph with no port to report on; a
    // failure is a module error, which the kernel reports either way.
    if state.failures == 0 {
        1
    } else {
        -3
    }
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
