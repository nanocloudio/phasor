//! On-graph conformance probe for the Phasor tokenizer.
//!
//! Every assertion runs inside the module, one case per bounded step, so the
//! module ABI, the target compilation, and the step budget are part of the
//! evidence rather than a host harness.

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

#[path = "../../common/diagnostic.rs"]
mod diagnostic;
#[path = "../../common/lex.rs"]
mod lex;
#[path = "../../common/numeric.rs"]
mod numeric;
#[path = "../../common/probe.rs"]
mod probe;
#[path = "../../common/softfloat.rs"]
mod softfloat;
#[path = "../../common/source.rs"]
mod source;
#[path = "../../common/text.rs"]
mod text;
#[path = "../../common/unicode_id.rs"]
mod unicode_id;
#[path = "../../common/value.rs"]
mod value;
#[path = "../../common/wire.rs"]
mod wire;

use diagnostic::code;
use lex::{cook, Goal, Keyword, Lexer, Punctuator, Token, TokenKind};
use source::{Limits, LineStart, LineTable};

const CASE_COUNT: u16 = 48;
const MAX_TOKENS: usize = 24;
const FUEL: u32 = 100_000;

/// The tokens of one source, or the code of the diagnostic that stopped it.
struct Scan {
    kinds: [TokenKind; MAX_TOKENS],
    count: usize,
    error: u16,
}

impl Scan {
    fn kind(&self, index: usize) -> TokenKind {
        match self.kinds.get(index) {
            Some(&kind) => kind,
            None => TokenKind::EndOfSource,
        }
    }
}

/// Scan a whole source with the goal policy a parser would use for these
/// fixtures: an operand is expected after a punctuator or keyword, division
/// after a value, and a template continuation before a closing brace inside a
/// substitution.
fn scan(source: &[u8]) -> Scan {
    let mut starts = [LineStart {
        byte: 0,
        units_before: 0,
    }; 32];
    let mut result = Scan {
        kinds: [TokenKind::EndOfSource; MAX_TOKENS],
        count: 0,
        error: 0,
    };
    let table = LineTable::new(&mut starts);
    let mut lexer = match Lexer::new(source, Limits::CEILING, table, FUEL) {
        Ok(lexer) => lexer,
        Err(diagnostic) => {
            result.error = diagnostic.code();
            return result;
        }
    };

    let mut goal = Goal::HashbangOrDiv;
    let mut depth = 0u32;
    while result.count < MAX_TOKENS {
        let token = match lexer.next(goal) {
            Ok(token) => token,
            Err(diagnostic) => {
                result.error = diagnostic.code();
                return result;
            }
        };
        result.kinds[result.count] = token.kind;
        result.count += 1;
        if matches!(token.kind, TokenKind::EndOfSource) {
            return result;
        }

        match token.kind {
            TokenKind::TemplateHead => depth = depth.saturating_add(1),
            TokenKind::TemplateTail => depth = depth.saturating_sub(1),
            _ => {}
        }
        goal = match token.kind {
            TokenKind::Identifier
            | TokenKind::Number
            | TokenKind::BigInt
            | TokenKind::String
            | TokenKind::RegExp
            | TokenKind::NoSubstitutionTemplate
            | TokenKind::TemplateTail
            | TokenKind::Punctuator(Punctuator::CloseParen)
            | TokenKind::Punctuator(Punctuator::CloseBracket) => Goal::Div,
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
    result
}

/// The first token of a source, scanned with an operand expected.
fn first(source: &[u8]) -> Option<Token> {
    let mut starts = [LineStart {
        byte: 0,
        units_before: 0,
    }; 8];
    let table = LineTable::new(&mut starts);
    let mut lexer = Lexer::new(source, Limits::CEILING, table, FUEL).ok()?;
    lexer.next(Goal::RegExp).ok()
}

/// The diagnostic code a source produces, or zero when it scans cleanly.
fn error_of(source: &[u8]) -> u16 {
    scan(source).error
}

fn kinds_are(source: &[u8], expected: &[TokenKind]) -> bool {
    let scanned = scan(source);
    if scanned.error != 0 || scanned.count != expected.len() + 1 {
        return false;
    }
    let mut index = 0usize;
    while index < expected.len() {
        if scanned.kind(index) != expected[index] {
            return false;
        }
        index += 1;
    }
    matches!(scanned.kind(expected.len()), TokenKind::EndOfSource)
}

fn number_is(source: &[u8], expected: f64) -> bool {
    match first(source) {
        Some(token) => {
            matches!(token.kind, TokenKind::Number) && token.number.to_bits() == expected.to_bits()
        }
        None => false,
    }
}

/// Whether the first token carries the legacy-octal mark.
fn legacy_marked(source: &[u8]) -> bool {
    first(source).is_some_and(|token| token.flags & lex::token_flag::LEGACY_OCTAL != 0)
}

fn cooks_to(source: &[u8], expected: &[u16]) -> bool {
    let Some(token) = first(source) else {
        return false;
    };
    let mut buffer = [0u16; 64];
    match cook(source, &token, &mut buffer) {
        Some(written) => buffer.get(..written) == Some(expected),
        None => false,
    }
}

#[allow(
    clippy::match_same_arms,
    reason = "each case is an independent assertion and merging arms would hide which one failed"
)]
fn run_case(case: u16) -> bool {
    use Punctuator as P;
    use TokenKind::{
        BigInt, Identifier, NoSubstitutionTemplate, Number, PrivateName, RegExp, String as Str,
        TemplateHead, TemplateTail,
    };

    match case {
        // Token kinds.
        0 => kinds_are(
            b"let x = 1 + 2;",
            &[
                Identifier,
                Identifier,
                TokenKind::Punctuator(P::Assign),
                Number,
                TokenKind::Punctuator(P::Plus),
                Number,
                TokenKind::Punctuator(P::Semicolon),
            ],
        ),
        1 => kinds_are(b"return", &[TokenKind::Keyword(Keyword::Return)]),
        2 => kinds_are(b"#field", &[PrivateName]),
        3 => kinds_are(b"caf\xC3\xA9", &[Identifier]),
        4 => kinds_are(b"\xE4\xB8\xAD\xE6\x96\x87", &[Identifier]),
        5 => kinds_are(b"\\u0061bc", &[Identifier]),

        // Numeric values.
        6 => number_is(b"42", 42.0),
        7 => number_is(b"3.5", 3.5),
        8 => number_is(b"1e3", 1000.0),
        9 => number_is(b".5", 0.5),
        10 => number_is(b"0x1f", 31.0),
        11 => number_is(b"0b1011", 11.0),
        12 => number_is(b"0o17", 15.0),
        13 => number_is(b"1_000_000", 1_000_000.0),
        14 => number_is(b"1e-3", 0.001),
        15 => number_is(b"9007199254740993", 9_007_199_254_740_992.0),
        16 => number_is(b"1.7976931348623157e308", f64::MAX),
        17 => number_is(b"5e-324", f64::from_bits(1)),
        18 => number_is(b"1e-400", 0.0),
        19 => kinds_are(b"123n", &[BigInt]),

        // Numeric rejections.
        // A legacy octal integer lexes, marked for strict code to refuse.
        20 => number_is(b"0123", 83.0) && legacy_marked(b"0123"),
        21 => error_of(b"3in") == code::INVALID_NUMERIC_TERMINATOR,
        22 => error_of(b"0x1g") == code::INVALID_NUMERIC_TERMINATOR,
        23 => error_of(b"1__0") == code::INVALID_NUMERIC_SEPARATOR,
        24 => error_of(b"1_") == code::INVALID_NUMERIC_SEPARATOR,
        25 => error_of(b"0x") == code::MISSING_RADIX_DIGITS,
        26 => error_of(b"1.5n") == code::INVALID_BIGINT_LITERAL,

        // Literals and their cooked values.
        27 => cooks_to(b"\"a\\nb\"", &[0x61, 0x0A, 0x62]),
        28 => cooks_to(b"\"\\u0041\\u{1F600}\"", &[0x41, 0xD83D, 0xDE00]),
        29 => cooks_to(b"\"a\\\nb\"", &[0x61, 0x62]),
        30 => cooks_to(b"\"\\x41\"", &[0x41]),
        31 => cooks_to(b"`a\r\nb`", &[0x61, 0x0A, 0x62]),
        32 => cooks_to(b"\\u0061bc", &[0x61, 0x62, 0x63]),
        33 => error_of(b"\"abc") == code::UNTERMINATED_STRING,
        34 => error_of(b"\"a\nb\"") == code::UNTERMINATED_STRING,
        // A legacy octal escape cooks, marked for strict code to refuse.
        35 => cooks_to(b"\"\\01\"", &[0x01]) && legacy_marked(b"\"\\01\""),
        36 => error_of(b"\"\\u{110000}\"") == code::INVALID_CODE_POINT,
        // An escaped name is an identifier name, not a keyword: the parser
        // decides where that is illegal.
        37 => match first(b"\\u0069f") {
            Some(token) => {
                matches!(token.kind, TokenKind::Identifier)
                    && token.escaped
                    && token.spells_reserved
            }
            None => false,
        },

        // Templates.
        38 => kinds_are(b"`a${b}c`", &[TemplateHead, Identifier, TemplateTail]),
        39 => kinds_are(b"`plain`", &[NoSubstitutionTemplate]),
        40 => error_of(b"`open") == code::UNTERMINATED_TEMPLATE,
        41 => match first(b"`\\u{}`") {
            Some(token) => !token.cooked_valid,
            None => false,
        },

        // Context-sensitive scanning.
        42 => kinds_are(
            b"x = /ab+/gi",
            &[Identifier, TokenKind::Punctuator(P::Assign), RegExp],
        ),
        43 => kinds_are(
            b"a / b",
            &[Identifier, TokenKind::Punctuator(P::Slash), Identifier],
        ),
        44 => kinds_are(
            b"a?.5:b",
            &[
                Identifier,
                TokenKind::Punctuator(P::Question),
                Number,
                TokenKind::Punctuator(P::Colon),
                Identifier,
            ],
        ),
        45 => kinds_are(
            b"a>>>=1",
            &[
                Identifier,
                TokenKind::Punctuator(P::UnsignedShiftRightAssign),
                Number,
            ],
        ),

        // Trivia, positions, and bounds.
        46 => {
            let mut starts = [LineStart {
                byte: 0,
                units_before: 0,
            }; 8];
            let table = LineTable::new(&mut starts);
            let source = b"a\nb";
            let Ok(mut lexer) = Lexer::new(source, Limits::CEILING, table, FUEL) else {
                return false;
            };
            let Ok(_) = lexer.next(Goal::RegExp) else {
                return false;
            };
            let Ok(second) = lexer.next(Goal::Div) else {
                return false;
            };
            let position = lexer.lines().position(source, 2);
            second.line_break_before
                && position.is_some_and(|position| position.line == 2 && position.column == 1)
        }
        47 => {
            let mut starts = [LineStart {
                byte: 0,
                units_before: 0,
            }; 8];
            let table = LineTable::new(&mut starts);
            let Ok(mut lexer) = Lexer::new(b"aaaa bbbb", Limits::CEILING, table, 3) else {
                return false;
            };
            match lexer.next(Goal::RegExp) {
                Ok(_) => false,
                Err(diagnostic) => {
                    diagnostic.code() == code::COMPILE_BUDGET_EXHAUSTED && diagnostic.is_fatal()
                }
            }
        }
        _ => true,
    }
}

/// Scan one byte value as a whole source, proving malformed input neither
/// panics nor escapes its bounds.
fn scan_byte(byte: u8) {
    let source = [byte];
    let _ = scan(&source);
}

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    report_out: i32,
    exit_out: i32,
    progress: probe::Progress,
    byte: u16,
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
    // Every byte value is scanned once after the cases, so no input can
    // make the scanner misbehave.
    if state.progress.case >= CASE_COUNT && state.byte <= u16::from(u8::MAX) {
        scan_byte(u8::try_from(state.byte).unwrap_or(u8::MAX));
        state.byte = state.byte.saturating_add(1);
        return 0;
    }
    probe::step(
        &mut state.progress,
        syscalls,
        state.report_out,
        state.exit_out,
        b"phasor-lex-probe",
        CASE_COUNT,
        run_case,
    )
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
