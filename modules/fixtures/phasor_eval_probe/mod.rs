//! On-graph conformance probe for the bounded expression evaluator.

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

#[macro_use]
#[path = "../../common/entry.rs"]
mod entry;

#[path = "../../common/eval_core.rs"]
mod eval_core;
#[path = "../../common/probe.rs"]
mod probe;
#[path = "../../common/text.rs"]
mod text;
#[path = "../../common/wire.rs"]
mod wire;
use eval_core::{evaluate, EvalError};

const CASE_COUNT: u16 = 8;

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    announced: bool,
    report_out: i32,
    exit_out: i32,
    progress: probe::Progress,
    byte: u16,
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

entry! {
    State;
    primary { report_out }
    inputs {}
    outputs { exit_out = 1 }
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
    if state.syscalls.is_null() {
        return -2;
    }
    // SAFETY: the table pointer was stored by `module_new` and checked
    // non-null above; the loader keeps it live for the module's lifetime.
    let syscalls = unsafe { &*state.syscalls };
    announce_ready!(state);
    // Every byte value is scanned once after the cases, so no input can
    // make the scanner misbehave.
    if state.progress.case >= CASE_COUNT && state.byte <= u16::from(u8::MAX) {
        let mut output = [0u8; 16];
        let input = [u8::try_from(state.byte).unwrap_or(u8::MAX)];
        let _ = evaluate(&input, 2, &mut output);
        state.byte = state.byte.saturating_add(1);
        return 0;
    }
    probe::step(
        &mut state.progress,
        syscalls,
        state.report_out,
        state.exit_out,
        b"phasor-eval-probe",
        CASE_COUNT,
        run_case,
    )
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
