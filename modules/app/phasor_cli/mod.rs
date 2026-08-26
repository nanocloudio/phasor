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

#[path = "../../common/diagnostic.rs"]
mod diagnostic;

use diagnostic::{code, termination, Diagnostic, Severity, FRAME};

const POLL_INPUT: u32 = 0x01;
const POLL_OUTPUT: u32 = 0x02;

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

/// The name a diagnostic code goes by.
///
/// The names are the ones the reference lists, so what a person reads and what
/// the documentation says are the same word. A code with no name is rendered as
/// its number, which is still something a reader can look up.
fn name_of(value: u16) -> (&'static [u8], bool) {
    let name: &[u8] = match value {
        code::TRANSFER_INCOMPLETE => b"transfer-incomplete",
        code::TRANSFER_OVERFLOW => b"transfer-overflow",
        code::DIGEST_MISMATCH => b"digest-mismatch",
        code::FEATURE_DIGEST_MISMATCH => b"feature-digest-mismatch",
        code::UNSUPPORTED_GOAL => b"unsupported-goal",
        code::COMPILE_BUDGET_EXHAUSTED => b"compile-budget-exhausted",
        code::TOO_MANY_DIAGNOSTICS => b"too-many-diagnostics",
        code::SOURCE_TOO_LARGE => b"source-too-large",
        code::TOO_MANY_LINES => b"too-many-lines",
        code::LINE_TOO_LONG => b"line-too-long",
        code::TOO_MANY_TOKENS => b"too-many-tokens",
        code::IDENTIFIER_TOO_LONG => b"identifier-too-long",
        code::LITERAL_TOO_LONG => b"literal-too-long",
        code::NUMERIC_LITERAL_TOO_LONG => b"numeric-literal-too-long",
        code::REGEXP_LITERAL_TOO_LONG => b"regexp-literal-too-long",
        code::TEMPLATE_NESTING_TOO_DEEP => b"template-nesting-too-deep",
        code::FEATURE_NOT_ADMITTED => b"feature-not-admitted",
        code::INVALID_UTF8 => b"invalid-utf8",
        code::INVALID_CHARACTER => b"invalid-character",
        code::UNTERMINATED_COMMENT => b"unterminated-comment",
        code::HASHBANG_NOT_AT_START => b"hashbang-not-at-start",
        code::INVALID_IDENTIFIER_ESCAPE => b"invalid-identifier-escape",
        code::ESCAPED_RESERVED_WORD => b"escaped-reserved-word",
        code::INVALID_NUMERIC_SEPARATOR => b"invalid-numeric-separator",
        code::LEGACY_OCTAL_LITERAL => b"legacy-octal-literal",
        code::INVALID_NUMERIC_TERMINATOR => b"invalid-numeric-terminator",
        code::MISSING_RADIX_DIGITS => b"missing-radix-digits",
        code::INVALID_BIGINT_LITERAL => b"invalid-bigint-literal",
        code::UNTERMINATED_STRING => b"unterminated-string",
        code::INVALID_ESCAPE => b"invalid-escape",
        code::LEGACY_OCTAL_ESCAPE => b"legacy-octal-escape",
        code::INVALID_CODE_POINT => b"invalid-code-point",
        code::UNTERMINATED_TEMPLATE => b"unterminated-template",
        code::INVALID_REGEXP_LITERAL => b"invalid-regexp-literal",
        code::INVALID_REGEXP_FLAG => b"invalid-regexp-flag",
        code::DUPLICATE_REGEXP_FLAG => b"duplicate-regexp-flag",
        code::REGEXP_PATTERN_UNSUPPORTED => b"regexp-pattern-unsupported",
        code::UNEXPECTED_TOKEN => b"unexpected-token",
        code::UNEXPECTED_END_OF_SOURCE => b"unexpected-end-of-source",
        code::EXPECTED_EXPRESSION => b"expected-expression",
        code::EXPECTED_CLOSE_PAREN => b"expected-close-paren",
        code::EXPECTED_CLOSE_BRACKET => b"expected-close-bracket",
        code::EXPECTED_CLOSE_BRACE => b"expected-close-brace",
        code::EXPECTED_COLON => b"expected-colon",
        code::EXPECTED_PROPERTY_NAME => b"expected-property-name",
        code::INVALID_ASSIGNMENT_TARGET => b"invalid-assignment-target",
        code::OPTIONAL_CHAIN_ASSIGNMENT => b"optional-chain-assignment",
        code::EXPONENT_OF_UNARY => b"exponent-of-unary",
        code::PRIVATE_NAME_OUT_OF_CONTEXT => b"private-name-out-of-context",
        code::EXPRESSION_TOO_DEEP => b"expression-too-deep",
        code::TOO_MANY_SYNTAX_NODES => b"too-many-syntax-nodes",
        code::SYNTAX_NOT_ADMITTED => b"syntax-not-admitted",
        code::MISSING_INITIALISER => b"missing-initialiser",
        code::INVALID_ARROW_PARAMETERS => b"invalid-arrow-parameters",
        code::DUPLICATE_BINDING => b"duplicate-binding",
        code::ASSIGNMENT_TO_CONSTANT => b"assignment-to-constant",
        code::UNDECLARED_LABEL => b"undeclared-label",
        code::ILLEGAL_BREAK_OR_CONTINUE => b"illegal-break-or-continue",
        code::RETURN_OUTSIDE_FUNCTION => b"return-outside-function",
        code::STRICT_ASSIGNMENT_TO_RESTRICTED_NAME => b"strict-assignment-to-restricted-name",
        code::UNKNOWN_OPCODE => b"unknown-opcode",
        code::TRUNCATED_OPERAND => b"truncated-operand",
        code::MISPLACED_PREFIX => b"misplaced-prefix",
        code::REGISTER_OUT_OF_RANGE => b"register-out-of-range",
        code::CONSTANT_OUT_OF_RANGE => b"constant-out-of-range",
        code::INVALID_JUMP_TARGET => b"invalid-jump-target",
        code::BACKWARD_JUMP_WITHOUT_SAFE_POINT => b"backward-jump-without-safe-point",
        code::INVALID_EXCEPTION_REGION => b"invalid-exception-region",
        code::OVERLAPPING_EXCEPTION_REGIONS => b"overlapping-exception-regions",
        code::CONTEXT_DEPTH_MISMATCH => b"context-depth-mismatch",
        code::CONTEXT_DEPTH_OUT_OF_RANGE => b"context-depth-out-of-range",
        code::FALLS_OFF_END => b"falls-off-end",
        code::INVALID_SAFE_POINT => b"invalid-safe-point",
        code::UNREACHABLE_CODE => b"unreachable-code",
        code::INCONSISTENT_DECLARED_BOUNDS => b"inconsistent-declared-bounds",
        code::MALFORMED_IMAGE => b"malformed-image",
        code::BYTECODE_FORMAT_MISMATCH => b"bytecode-format-mismatch",
        code::VERIFIER_STORAGE_TOO_SMALL => b"verifier-storage-too-small",
        code::CODE_TOO_LARGE => b"code-too-large",
        code::TOO_MANY_CONSTANTS => b"too-many-constants",
        code::TOO_MANY_REGISTERS => b"too-many-registers",
        code::JUMP_TOO_FAR => b"jump-too-far",
        code::LOWERING_NOT_ADMITTED => b"lowering-not-admitted",
        code::IMAGE_NOT_ADMITTED => b"image-not-admitted",
        code::FEATURE_LIST_MISMATCH => b"feature-list-mismatch",
        termination::FUEL_EXHAUSTED => b"fuel-exhausted",
        termination::QUOTA_EXCEEDED => b"quota-exceeded",
        termination::CANCELLED => b"cancelled",
        termination::DEADLINE_REACHED => b"deadline-reached",
        termination::STACK_OVERFLOW => b"stack-overflow",
        termination::REGISTERS_EXHAUSTED => b"registers-exhausted",
        termination::HEAP_EXHAUSTED => b"heap-exhausted",
        termination::NOT_IMPLEMENTED => b"not-implemented",
        termination::MALFORMED_IMAGE_AT_RUN_TIME => b"malformed-image-at-run-time",
        termination::REJECTED => b"rejected",
        termination::UNCAUGHT_THROW => b"uncaught-throw",
        _ => return (b"", false),
    };
    (name, true)
}

/// Render a diagnostic as one line: what failed, and where.
fn render(state: &mut State, report: &Diagnostic) {
    let mut length = 0usize;
    length += put(&mut state.report[length..], b"phasor: ");
    if matches!(report.severity(), Severity::Fatal) {
        length += put(&mut state.report[length..], b"fatal ");
    }
    let (name, known) = name_of(report.code());
    if known {
        length += put(&mut state.report[length..], name);
    } else {
        length += put(&mut state.report[length..], b"diagnostic ");
        length += put_number(&mut state.report[length..], u32::from(report.code()));
    }
    length += put(&mut state.report[length..], b" at ");
    length += put_number(&mut state.report[length..], report.offset());
    if report.length() > 0 {
        length += put(&mut state.report[length..], b"..");
        length += put_number(
            &mut state.report[length..],
            report.offset().saturating_add(report.length()),
        );
    }
    let arguments = report.arguments();
    if !arguments.is_empty() {
        length += put(&mut state.report[length..], b" (");
        for (index, argument) in arguments.iter().enumerate() {
            if index > 0 {
                length += put(&mut state.report[length..], b", ");
            }
            length += put_number(&mut state.report[length..], *argument);
        }
        length += put(&mut state.report[length..], b")");
    }
    length += put(&mut state.report[length..], b"\n");
    state.report_length = length;
    state.failed = true;
}

/// Take one diagnostic frame from a port, and render it when it is whole.
fn take_diagnostic(state: &mut State, syscalls: &SyscallTable, port: i32) {
    if port < 0 {
        return;
    }
    let poll = unsafe { (syscalls.channel_poll)(port, POLL_INPUT) };
    if poll <= 0 || (poll as u32) & POLL_INPUT == 0 {
        return;
    }
    let remaining = FRAME.saturating_sub(state.frame_filled);
    if remaining > 0 {
        let read = unsafe {
            (syscalls.channel_read)(
                port,
                state.frame.as_mut_ptr().add(state.frame_filled),
                remaining,
            )
        };
        if read > 0 {
            state.frame_filled += usize::try_from(read).unwrap_or(0).min(remaining);
        }
    }
    if state.frame_filled == FRAME {
        state.frame_filled = 0;
        if let Some(report) = Diagnostic::decode(&state.frame) {
            render(state, &report);
        }
    }
}

fn put(out: &mut [u8], text: &[u8]) -> usize {
    let mut written = 0usize;
    while written < text.len() {
        match out.get_mut(written) {
            Some(slot) => {
                *slot = text[written];
                written += 1;
            }
            None => break,
        }
    }
    written
}

fn put_number(out: &mut [u8], value: u32) -> usize {
    let mut digits = [0u8; 10];
    let mut count = 0usize;
    let mut rest = value;
    loop {
        digits[count] = b'0' + u8::try_from(rest % 10).unwrap_or(0);
        count += 1;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    let mut written = 0usize;
    while count > 0 {
        count -= 1;
        match out.get_mut(written) {
            Some(slot) => {
                *slot = digits[count];
                written += 1;
            }
            None => break,
        }
    }
    written
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
        core::ptr::addr_of_mut!((*state).result_in).write(in_chan);
        core::ptr::addr_of_mut!((*state).stdout).write(out_chan);
        core::ptr::addr_of_mut!((*state).diagnostic_in).write(dev_channel_port(&*table, 0, 1));
        core::ptr::addr_of_mut!((*state).runtime_in).write(dev_channel_port(&*table, 0, 2));
        core::ptr::addr_of_mut!((*state).stderr).write(dev_channel_port(&*table, 1, 1));
        core::ptr::addr_of_mut!((*state).exit_out).write(dev_channel_port(&*table, 1, 2));
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

    // What a phase produced is passed through unchanged: the edge renders
    // failures, not results.
    if state.result_in >= 0 && !state.result_hung_up {
        let poll = unsafe { (syscalls.channel_poll)(state.result_in, POLL_INPUT | POLL_HUP) };
        if poll > 0 && (poll as u32) & POLL_INPUT != 0 {
            let offset = state.result_length;
            let remaining = RESULT_CAPACITY.saturating_sub(offset);
            if remaining > 0 {
                let read = unsafe {
                    (syscalls.channel_read)(
                        state.result_in,
                        state.result.as_mut_ptr().add(offset),
                        remaining,
                    )
                };
                if read > 0 {
                    state.result_length += usize::try_from(read).unwrap_or(0).min(remaining);
                }
            }
        } else if poll > 0 && (poll as u32) & POLL_HUP != 0 {
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
    if state.result_written < state.result_length && state.stdout >= 0 {
        let poll = unsafe { (syscalls.channel_poll)(state.stdout, POLL_OUTPUT) };
        if poll > 0 && (poll as u32) & POLL_OUTPUT != 0 {
            let offset = state.result_written;
            let remaining = state.result_length.saturating_sub(offset);
            let written = unsafe {
                (syscalls.channel_write)(state.stdout, state.result.as_ptr().add(offset), remaining)
            };
            if written > 0 {
                state.result_written += usize::try_from(written).unwrap_or(0).min(remaining);
            }
        }
    }
    if state.report_written < state.report_length && state.stderr >= 0 {
        let poll = unsafe { (syscalls.channel_poll)(state.stderr, POLL_OUTPUT) };
        if poll > 0 && (poll as u32) & POLL_OUTPUT != 0 {
            let offset = state.report_written;
            let remaining = state.report_length.saturating_sub(offset);
            let written = unsafe {
                (syscalls.channel_write)(state.stderr, state.report.as_ptr().add(offset), remaining)
            };
            if written > 0 {
                state.report_written += usize::try_from(written).unwrap_or(0).min(remaining);
            }
        }
    }

    // The run is over when what produced a result has hung up and everything
    // there was to write has been written. A diagnostic port is not waited on:
    // its hang-up says nothing about whether the run finished. What is already
    // in one is read first, because a frame in a port is a frame that was
    // produced.
    let drained =
        state.result_written >= state.result_length && state.report_written >= state.report_length;
    let mut pending = false;
    for port in [state.diagnostic_in, state.runtime_in] {
        if port < 0 {
            continue;
        }
        let poll = unsafe { (syscalls.channel_poll)(port, POLL_INPUT) };
        if poll > 0 && (poll as u32) & POLL_INPUT != 0 {
            pending = true;
        }
    }
    let finished = state.result_hung_up && drained && !pending && state.frame_filled == 0;
    if !finished {
        return 0;
    }

    if state.exit_out >= 0 {
        let code = i32::from(state.failed).to_le_bytes();
        let written =
            unsafe { (syscalls.channel_write)(state.exit_out, code.as_ptr(), code.len()) };
        if written != i32::try_from(code.len()).unwrap_or(i32::MAX) {
            return 0;
        }
    }
    state.phase = 2;
    1
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
