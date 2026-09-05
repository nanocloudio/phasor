//! The edge: what a person sees.
//!
//! Every phase before this one speaks in numbers — a code, a severity, a span,
//! the arguments that make the failure specific. Nothing before this point
//! renders text for a human, and nothing after it decides what happened. This
//! module is where the one becomes the other, and it is the only place in the
//! engine that knows what a diagnostic reads like.
//!
//! It writes what a phase produced to its output, renders any diagnostic to its
//! error output, and sets an exit status that says whether anything failed.

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

#[macro_use]
#[path = "../../common/entry.rs"]
mod entry;

#[path = "../../common/diagnostic.rs"]
mod diagnostic;
#[path = "../../common/text.rs"]
mod text;
#[path = "../../common/wire.rs"]
mod wire;

use diagnostic::{Diagnostic, Severity, FRAME};

const RESULT_CAPACITY: usize = 1024;
const REPORT_CAPACITY: usize = 512;

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    result_in: i32,
    diagnostic_in: i32,
    runtime_in: i32,
    stdout: i32,
    stderr: i32,
    exit_out: i32,
    result: [u8; RESULT_CAPACITY],
    result_length: usize,
    result_written: usize,
    frame: [u8; FRAME],
    frame_filled: usize,
    report: [u8; REPORT_CAPACITY],
    report_length: usize,
    report_written: usize,
    result_hung_up: bool,
    failed: bool,
    phase: u8,
}

/// Render a diagnostic as one line: what failed, and where.
fn render(state: &mut State, report: &Diagnostic) {
    let mut length = 0usize;
    length += text::put_ascii(&mut state.report[length..], b"phasor: ");
    if matches!(report.severity(), Severity::Fatal) {
        length += text::put_ascii(&mut state.report[length..], b"fatal ");
    }
    if let Some(name) = diagnostic::name_of(report.code()) {
        length += text::put_ascii(&mut state.report[length..], name);
    } else {
        length += text::put_ascii(&mut state.report[length..], b"diagnostic ");
        length += text::put_u32(&mut state.report[length..], u32::from(report.code()));
    }
    length += text::put_ascii(&mut state.report[length..], b" at ");
    length += text::put_u32(&mut state.report[length..], report.offset());
    if report.length() > 0 {
        length += text::put_ascii(&mut state.report[length..], b"..");
        length += text::put_u32(
            &mut state.report[length..],
            report.offset().saturating_add(report.length()),
        );
    }
    let arguments = report.arguments();
    if !arguments.is_empty() {
        length += text::put_ascii(&mut state.report[length..], b" (");
        for (index, argument) in arguments.iter().enumerate() {
            if index > 0 {
                length += text::put_ascii(&mut state.report[length..], b", ");
            }
            length += text::put_u32(&mut state.report[length..], *argument);
        }
        length += text::put_ascii(&mut state.report[length..], b")");
    }
    length += text::put_ascii(&mut state.report[length..], b"\n");
    state.report_length = length;
    state.failed = true;
}

/// Take one diagnostic frame from a port, and render it when it is whole.
fn take_diagnostic(state: &mut State, syscalls: &SyscallTable, port: i32) {
    if wire::take_frame(syscalls, port, &mut state.frame, &mut state.frame_filled) {
        state.frame_filled = 0;
        if let Some(report) = Diagnostic::decode(&state.frame) {
            render(state, &report);
        }
    }
}

entry! {
    State;
    primary { result_in, stdout }
    inputs { diagnostic_in = 1, runtime_in = 2 }
    outputs { stderr = 1, exit_out = 2 }
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
    if state.phase == 2 {
        return 1;
    }

    // What a phase produced is passed through unchanged: the edge renders
    // failures, not results.
    if state.result_in >= 0 && !state.result_hung_up {
        let mut overflowed = false;
        let staged = wire::stage_stream(
            syscalls,
            state.result_in,
            &mut state.result,
            &mut state.result_length,
            &mut overflowed,
        );
        if staged == wire::Staged::Complete {
            state.result_hung_up = true;
        }
    }

    // A diagnostic arrives as a frame of numbers and leaves as a line of text.
    // Either phase may have something to say, and the first that does is what
    // a reader is told: a run stops at its first failure.
    let mut source = state.diagnostic_in;
    if state.report_length == 0 {
        take_diagnostic(state, syscalls, source);
        source = state.runtime_in;
        if state.report_length == 0 {
            take_diagnostic(state, syscalls, source);
        }
    }

    // Write what there is to write, in whatever pieces the ports take.
    let result = state.result.get(..state.result_length).unwrap_or(&[]);
    let _ = wire::push_progress(syscalls, state.stdout, result, &mut state.result_written);
    let report = state.report.get(..state.report_length).unwrap_or(&[]);
    let _ = wire::push_progress(syscalls, state.stderr, report, &mut state.report_written);

    // The run is over when what produced a result has hung up and everything
    // there was to write has been written. A diagnostic port is not waited on:
    // its hang-up says nothing about whether the run finished. What is already
    // in one is read first, because a frame in a port is a frame that was
    // produced.
    let drained =
        state.result_written >= state.result_length && state.report_written >= state.report_length;
    let pending = wire::has_input(syscalls, state.diagnostic_in)
        || wire::has_input(syscalls, state.runtime_in);
    let finished = state.result_hung_up && drained && !pending && state.frame_filled == 0;
    if !finished {
        return 0;
    }

    if !wire::push_exit(syscalls, state.exit_out, state.failed) {
        return 0;
    }
    state.phase = 2;
    1
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
