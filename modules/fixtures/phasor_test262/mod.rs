//! On-graph Test262 conformance oracle for the front end.
//!
//! The module reads a batch of selected cases from one input stream and
//! tokenizes each of them, so the measurement runs through the module ABI and
//! the same code a deployment would run. The driver that assembles the batch
//! chooses the selection and supplies each case's expectation; this module
//! decides pass and fail and reports counts per feature area.
//!
//! A record is a six-byte header followed by the case source:
//!
//! ```text
//! length: u32 little-endian | expectation: u8 | area: u8 | source bytes
//! ```
//!
//! Bit zero of the expectation means the case is a negative test, which is
//! counted but makes no demand; bit one means the case is a module, which is
//! parsed with the module goal. An expectation of zero is a positive script,
//! which must tokenize with no diagnostic. A negative case makes no
//! demand: a lexical diagnostic and a clean scan are both admissible, because
//! most negative cases fail later than tokenizing.
//!
//! Each positive case is also offered to the parser, and the cases it accepts
//! are counted separately. The parser admits expressions only, so that count
//! measures how much of the corpus the implemented grammar reaches rather than
//! how correct it is.

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

#[path = "../../common/arena.rs"]
mod arena;
#[path = "../../common/diagnostic.rs"]
mod diagnostic;
#[path = "../../common/lex.rs"]
mod lex;
#[path = "../../common/numeric.rs"]
mod numeric;
#[path = "../../common/parse.rs"]
mod parse;
#[path = "../../common/softfloat.rs"]
mod softfloat;
#[path = "../../common/source.rs"]
mod source;
#[path = "../../common/unicode_id.rs"]
mod unicode_id;
#[path = "../../common/value.rs"]
mod value;

use arena::{Arena, Node, NodeKind};
use lex::{Goal, Keyword, Lexer, Punctuator, TokenKind};
use parse::Parser;
use source::{Limits, LineStart, LineTable};

const POLL_INPUT: u32 = 0x01;
const POLL_OUTPUT: u32 = 0x02;

/// Largest case this module tokenizes. Larger cases are counted as skipped
/// rather than silently dropped.
const CASE_CAPACITY: usize = 32 * 1024;
/// Header bytes before each case.
const HEADER: usize = 6;
/// Feature areas the driver may report, which is the number of directories
/// directly under the corpus's language tree.
const AREAS: usize = 32;
const FUEL: u32 = 40_000_000;
const LINE_CAPACITY: usize = 64;

/// Counts for one feature area.
#[derive(Clone, Copy)]
struct Area {
    positive: u32,
    passed: u32,
    parsed: u32,
    negative: u32,
    skipped: u32,
}

/// Storage the parser needs for one case.
const NODE_CAPACITY: usize = 2048;
const LIST_CAPACITY: usize = 2048;
const NUMBER_CAPACITY: usize = 256;
const SCRATCH_CAPACITY: usize = 512;

struct ParseStorage {
    nodes: [Node; NODE_CAPACITY],
    lists: [u32; LIST_CAPACITY],
    numbers: [f64; NUMBER_CAPACITY],
    scratch: [u32; SCRATCH_CAPACITY],
}

/// Whether the parser accepts the case as a whole expression.
fn parses(
    source: &[u8],
    starts: &mut [LineStart; LINE_CAPACITY],
    storage: &mut ParseStorage,
    module: bool,
) -> bool {
    let table = LineTable::new(starts);
    let Ok(lexer) = Lexer::new(source, Limits::CEILING, table, FUEL) else {
        return false;
    };
    let syntax = Arena::new(&mut storage.nodes, &mut storage.lists, &mut storage.numbers);
    let mut parser = Parser::new(lexer, syntax, &mut storage.scratch, Limits::CEILING);
    if module {
        return parser.parse_module().is_ok();
    }
    parser.parse_unit().is_ok()
}

/// Tokenize one case with the goal policy a driver can supply without a parser.
///
/// The policy is the ordinary one: an operand is expected after a punctuator or
/// a keyword, division after a value, and a template continuation before the
/// brace that closes a substitution. It is a heuristic, and the selection
/// excludes the cases where it is known to differ from a parser's choice.
fn tokenizes(source: &[u8], starts: &mut [LineStart; LINE_CAPACITY]) -> bool {
    let table = LineTable::new(starts);
    let Ok(mut lexer) = Lexer::new(source, Limits::CEILING, table, FUEL) else {
        return false;
    };
    let mut goal = Goal::HashbangOrDiv;
    let mut depth = 0u32;
    loop {
        let Ok(token) = lexer.next(goal) else {
            return false;
        };
        if matches!(token.kind, TokenKind::EndOfSource) {
            return true;
        }
        match token.kind {
            TokenKind::TemplateHead => depth = depth.saturating_add(1),
            TokenKind::TemplateTail => depth = depth.saturating_sub(1),
            _ => {}
        }
        goal = match token.kind {
            TokenKind::Identifier
            | TokenKind::PrivateName
            | TokenKind::Number
            | TokenKind::BigInt
            | TokenKind::String
            | TokenKind::RegExp
            | TokenKind::NoSubstitutionTemplate
            | TokenKind::TemplateTail
            | TokenKind::Keyword(
                Keyword::This | Keyword::Null | Keyword::True | Keyword::False | Keyword::Super,
            )
            | TokenKind::Punctuator(
                Punctuator::CloseParen
                | Punctuator::CloseBracket
                | Punctuator::CloseBrace
                | Punctuator::PlusPlus
                | Punctuator::MinusMinus,
            ) => Goal::Div,
            _ => Goal::RegExp,
        };
        if depth > 0 && matches!(goal, Goal::Div) {
            let mut at = lexer.cursor() as usize;
            while source.get(at).is_some_and(u8::is_ascii_whitespace) {
                at += 1;
            }
            if source.get(at) == Some(&b'}') {
                goal = Goal::TemplateTail;
            }
        }
    }
}

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    input: i32,
    report_out: i32,
    exit_out: i32,
    buffer: [u8; CASE_CAPACITY + HEADER],
    starts: [LineStart; LINE_CAPACITY],
    parse: ParseStorage,
    areas: [Area; AREAS],
    /// Bytes held in `buffer`.
    filled: usize,
    /// Bytes of the current oversized case still to be discarded.
    discarding: usize,
    unexpected: u32,
    cases: u32,
    report_offset: usize,
    report_length: usize,
    report: [u8; 64],
    line: u16,
    phase: u8,
}

/// Consume as many complete records as `buffer` holds.
fn drain(state: &mut State) {
    loop {
        if state.discarding > 0 {
            let available = state.filled;
            let drop = if state.discarding < available {
                state.discarding
            } else {
                available
            };
            if drop == 0 {
                return;
            }
            state.buffer.copy_within(drop..state.filled, 0);
            state.filled -= drop;
            state.discarding -= drop;
            continue;
        }
        if state.filled < HEADER {
            return;
        }
        let length = u32::from_le_bytes([
            state.buffer[0],
            state.buffer[1],
            state.buffer[2],
            state.buffer[3],
        ]) as usize;
        let expectation = state.buffer[4];
        let area = usize::from(state.buffer[5]).min(AREAS - 1);

        if length > CASE_CAPACITY {
            state.areas[area].skipped = state.areas[area].skipped.saturating_add(1);
            state.cases = state.cases.saturating_add(1);
            state.buffer.copy_within(HEADER..state.filled, 0);
            state.filled -= HEADER;
            state.discarding = length;
            continue;
        }
        if state.filled < HEADER + length {
            return;
        }

        let mut passed = true;
        let mut parsed = false;
        {
            // The case is scanned in place from the staging buffer.
            let (header_and_case, _) = state.buffer.split_at(HEADER + length);
            let (_, source) = header_and_case.split_at(HEADER);
            let mut starts = state.starts;
            let module = expectation & 2 != 0;
            if expectation & 1 == 0 {
                passed = tokenizes(source, &mut starts);
                let mut parse_starts = state.starts;
                parsed = parses(source, &mut parse_starts, &mut state.parse, module);
            } else {
                let _ = tokenizes(source, &mut starts);
            }
        }
        if expectation & 1 == 0 {
            state.areas[area].positive = state.areas[area].positive.saturating_add(1);
            if passed {
                state.areas[area].passed = state.areas[area].passed.saturating_add(1);
            } else {
                state.unexpected = state.unexpected.saturating_add(1);
            }
            if parsed {
                state.areas[area].parsed = state.areas[area].parsed.saturating_add(1);
            }
        } else {
            state.areas[area].negative = state.areas[area].negative.saturating_add(1);
        }
        state.cases = state.cases.saturating_add(1);
        state.buffer.copy_within(HEADER + length..state.filled, 0);
        state.filled -= HEADER + length;
    }
}

/// Write a decimal number into `out`, returning the bytes used.
fn write_u32(value: u32, out: &mut [u8]) -> usize {
    let mut digits = [0u8; 10];
    let mut count = 0usize;
    let mut remaining = value;
    loop {
        digits[count] = b'0' + u8::try_from(remaining % 10).unwrap_or(0);
        count += 1;
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    if out.len() < count {
        return 0;
    }
    let mut index = 0usize;
    while index < count {
        out[index] = digits[count - 1 - index];
        index += 1;
    }
    count
}

/// Render one report line into `state.report`.
fn compose(state: &mut State, line: u16) -> usize {
    let mut out = [0u8; 64];
    let mut at = 0usize;
    if usize::from(line) < AREAS {
        let area = state.areas[usize::from(line)];
        out[at] = b'a';
        out[at + 1] = b'r';
        out[at + 2] = b'e';
        out[at + 3] = b'a';
        out[at + 4] = b' ';
        at += 5;
        at += write_u32(u32::from(line), &mut out[at..]);
        for value in [
            area.positive,
            area.passed,
            area.parsed,
            area.negative,
            area.skipped,
        ] {
            out[at] = b' ';
            at += 1;
            at += write_u32(value, &mut out[at..]);
        }
    } else {
        let prefix = b"phasor-test262: ";
        out[at..at + prefix.len()].copy_from_slice(prefix);
        at += prefix.len();
        at += write_u32(state.unexpected, &mut out[at..]);
        let suffix = b" unexpected failures in ";
        out[at..at + suffix.len()].copy_from_slice(suffix);
        at += suffix.len();
        at += write_u32(state.cases, &mut out[at..]);
        let tail = b" cases";
        out[at..at + tail.len()].copy_from_slice(tail);
        at += tail.len();
    }
    out[at] = b'\n';
    at += 1;
    state.report[..at].copy_from_slice(&out[..at]);
    at
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
        core::ptr::addr_of_mut!((*state).input).write(in_chan);
        core::ptr::addr_of_mut!((*state).report_out).write(out_chan);
        core::ptr::addr_of_mut!((*state).exit_out).write(dev_channel_port(&*table, 1, 1));
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
    if state.syscalls.is_null() || state.input < 0 || state.report_out < 0 {
        return -2;
    }
    let syscalls = unsafe { &*state.syscalls };

    if state.phase == 3 {
        return 1;
    }

    // Finish any report line that is still being written.
    if state.report_length > state.report_offset {
        let offset = state.report_offset;
        let remaining = state.report_length - offset;
        let written = unsafe {
            (syscalls.channel_write)(state.report_out, state.report[offset..].as_ptr(), remaining)
        };
        if written <= 0 {
            return 0;
        }
        state.report_offset += usize::try_from(written).unwrap_or(0).min(remaining);
        if state.report_length > state.report_offset {
            return 0;
        }
        state.report_offset = 0;
        state.report_length = 0;
    }

    if state.phase == 1 {
        // Emit one report line per step.
        if usize::from(state.line) <= AREAS {
            let line = state.line;
            let length = compose(state, line);
            state.report_length = length;
            state.report_offset = 0;
            state.line = state.line.saturating_add(1);
            return 0;
        }
        state.phase = 2;
        return 0;
    }

    if state.phase == 2 {
        let code = i32::from(state.unexpected != 0).to_le_bytes();
        let written =
            unsafe { (syscalls.channel_write)(state.exit_out, code.as_ptr(), code.len()) };
        if written != i32::try_from(code.len()).unwrap_or(i32::MAX) {
            return 0;
        }
        state.phase = 3;
        return 1;
    }

    let poll = unsafe { (syscalls.channel_poll)(state.input, POLL_INPUT | POLL_HUP) };
    if poll <= 0 {
        return 0;
    }

    if (poll as u32) & POLL_INPUT != 0 {
        let offset = state.filled;
        let capacity = state.buffer.len().saturating_sub(offset);
        if capacity == 0 {
            drain(state);
            return 0;
        }
        let read = unsafe {
            (syscalls.channel_read)(state.input, state.buffer.as_mut_ptr().add(offset), capacity)
        };
        if read > 0 {
            state.filled += usize::try_from(read).unwrap_or(0).min(capacity);
            drain(state);
        }
        return 0;
    }

    if (poll as u32) & POLL_HUP == 0 {
        return 0;
    }

    drain(state);
    state.phase = 1;
    0
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
