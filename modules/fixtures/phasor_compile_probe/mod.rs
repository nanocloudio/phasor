//! On-graph conformance probe for the whole front end.
//!
//! Each bounded step takes one source through scanning, parsing, lowering, and
//! verification, and checks the unit image that comes out. Nothing reaches the
//! check unless the verifier admitted it, because the lowering publishes
//! nothing else.

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

#[path = "../../common/arena.rs"]
mod arena;
#[path = "../../common/bytecode.rs"]
mod bytecode;
#[path = "../../common/diagnostic.rs"]
mod diagnostic;
#[path = "../../common/digest.rs"]
mod digest;
#[path = "../../common/emit.rs"]
mod emit;
#[path = "../../common/evalsite.rs"]
mod evalsite;
#[path = "../../common/feature.rs"]
mod feature;
#[path = "../../common/lex.rs"]
mod lex;
#[path = "../../common/lower.rs"]
#[macro_use]
mod lower;
#[path = "../../common/frontend.rs"]
#[macro_use]
mod frontend;
#[path = "../../common/numeric.rs"]
mod numeric;
#[path = "../../common/parse.rs"]
mod parse;
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
#[path = "../../common/verify.rs"]
mod verify;
#[path = "../../common/wire.rs"]
mod wire;

use arena::{Arena, Node, NodeKind};
use bytecode::{
    decode, Constant, ConstantKind, ExceptionRegion, ExportRecord, Function, ImportRecord, Opcode,
    Unit,
};
use diagnostic::code;
use emit::Patch;
use lex::Lexer;
use lower::{
    lower_expression, Binding as LexicalBinding, Pending as PendingFunction, Scope,
    Storage as LowerStorage,
};
use parse::Parser;
use source::{Limits, LineStart, LineTable};

const CASE_COUNT: u16 = 26;
const FUEL: u32 = 200_000;

const NODE_CAPACITY: usize = 128;
const LIST_CAPACITY: usize = 128;
const NUMBER_CAPACITY: usize = 32;
const SCRATCH_CAPACITY: usize = 64;
const LINE_CAPACITY: usize = 16;
const CODE_CAPACITY: usize = 512;
const IMAGE_CAPACITY: usize = 2048;
const CONSTANT_CAPACITY: usize = 32;
const DATA_CAPACITY: usize = 512;
const POINT_CAPACITY: usize = 32;
const PATCH_CAPACITY: usize = 32;
const LABEL_CAPACITY: usize = 32;
const STATE_CAPACITY: usize = 512;

/// Every buffer the front end needs, owned by the module.
const UNIT_CODE_CAPACITY: usize = 8192;
const UNIT_POINT_CAPACITY: usize = 256;
const FUNCTION_CAPACITY: usize = 64;
const EXCEPTION_CAPACITY: usize = 64;
const SCOPE_CAPACITY: usize = 128;
const LEXICAL_CAPACITY: usize = 256;
const PENDING_CAPACITY: usize = 64;
const IMPORT_CAPACITY: usize = 32;
const EXPORT_CAPACITY: usize = 32;
const EVAL_SITE_CAPACITY: usize = 2048;
struct Storage {
    nodes: [Node; NODE_CAPACITY],
    lists: [u32; LIST_CAPACITY],
    numbers: [f64; NUMBER_CAPACITY],
    scratch: [u32; SCRATCH_CAPACITY],
    starts: [LineStart; LINE_CAPACITY],
    code: [u8; CODE_CAPACITY],
    image: [u8; IMAGE_CAPACITY],
    constants: [Constant; CONSTANT_CAPACITY],
    constant_data: [u8; DATA_CAPACITY],
    safe_points: [u32; POINT_CAPACITY],
    patches: [Patch; PATCH_CAPACITY],
    labels: [u32; LABEL_CAPACITY],
    verifier_state: [i32; STATE_CAPACITY],
    unit_code: [u8; UNIT_CODE_CAPACITY],
    unit_safe_points: [u32; UNIT_POINT_CAPACITY],
    functions: [Function; FUNCTION_CAPACITY],
    exceptions: [ExceptionRegion; EXCEPTION_CAPACITY],
    scopes: [Scope; SCOPE_CAPACITY],
    lexical: [LexicalBinding; LEXICAL_CAPACITY],
    pending: [PendingFunction; PENDING_CAPACITY],
    imports: [ImportRecord; IMPORT_CAPACITY],
    exports: [ExportRecord; EXPORT_CAPACITY],
    eval_sites: [u8; EVAL_SITE_CAPACITY],
}

/// What one compilation produced.
#[derive(Clone, Copy)]
struct Outcome {
    length: usize,
    diagnostic: u16,
}

/// Compile one source all the way to a verified image.
fn compile(storage: &mut Storage, source: &[u8]) -> Outcome {
    let mut front = frontend_storage!(storage);
    match frontend::compile(
        source,
        frontend::Goal::Script,
        Limits::CEILING,
        FUEL,
        &mut front,
    ) {
        Ok(compiled) => Outcome {
            length: compiled.length,
            diagnostic: 0,
        },
        Err(diagnostic) => Outcome {
            length: 0,
            diagnostic: diagnostic.code(),
        },
    }
}

/// Compile and hand the verified image to `check`.
fn compiled(storage: &mut Storage, source: &[u8], check: impl Fn(&Unit<'_>) -> bool) -> bool {
    let outcome = compile(storage, source);
    if outcome.diagnostic != 0 {
        return false;
    }
    let Some(bytes) = storage.image.get(..outcome.length) else {
        return false;
    };
    match Unit::parse(bytes) {
        Ok(unit) => check(&unit),
        Err(_) => false,
    }
}

/// The opcodes of the entry function, up to `out.len()`.
fn opcodes(unit: &Unit<'_>, out: &mut [Opcode]) -> usize {
    let Some(function) = unit.function(0) else {
        return 0;
    };
    let Some(code) = unit.code(&function) else {
        return 0;
    };
    let mut offset = 0u32;
    let mut count = 0usize;
    while (offset as usize) < code.len() && count < out.len() {
        let Ok(instruction) = decode(code, offset) else {
            return count;
        };
        out[count] = instruction.opcode;
        count += 1;
        offset += instruction.length;
    }
    count
}

fn opcodes_are(unit: &Unit<'_>, expected: &[Opcode]) -> bool {
    let mut buffer = [Opcode::Return; 32];
    let count = opcodes(unit, &mut buffer);
    buffer.get(..count) == Some(expected)
}

#[allow(
    clippy::match_same_arms,
    reason = "each case is an independent assertion and merging arms would hide which one failed"
)]
fn run_case(storage: &mut Storage, case: u16) -> bool {
    match case {
        0 => compiled(storage, b"1 + 2", |unit| {
            opcodes_are(
                unit,
                &[
                    Opcode::LdaSmi,
                    Opcode::Star,
                    Opcode::LdaSmi,
                    Opcode::Add,
                    Opcode::Return,
                ],
            )
        }),
        1 => compiled(storage, b"-1", |unit| {
            opcodes_are(unit, &[Opcode::LdaSmi, Opcode::Negate, Opcode::Return])
        }),
        2 => compiled(storage, b"1 ? 2 : 3", |unit| {
            opcodes_are(
                unit,
                &[
                    Opcode::LdaSmi,
                    Opcode::JumpIfToBooleanFalse,
                    Opcode::LdaSmi,
                    Opcode::Jump,
                    Opcode::LdaSmi,
                    Opcode::Return,
                ],
            )
        }),
        3 => compiled(storage, b"a && b", |unit| {
            opcodes_are(
                unit,
                &[
                    Opcode::LdaGlobal,
                    Opcode::JumpIfToBooleanFalse,
                    Opcode::LdaGlobal,
                    Opcode::Return,
                ],
            )
        }),
        4 => compiled(storage, b"a ?? b", |unit| {
            opcodes_are(
                unit,
                &[
                    Opcode::LdaGlobal,
                    Opcode::JumpIfNotNullish,
                    Opcode::LdaGlobal,
                    Opcode::Return,
                ],
            )
        }),
        5 => compiled(storage, b"a.b", |unit| {
            opcodes_are(
                unit,
                &[
                    Opcode::LdaGlobal,
                    Opcode::Star,
                    Opcode::GetNamedProperty,
                    Opcode::Return,
                ],
            )
        }),
        6 => compiled(storage, b"a = 1", |unit| {
            opcodes_are(unit, &[Opcode::LdaSmi, Opcode::StaGlobal, Opcode::Return])
        }),

        // Constructs that must reach a verified image.
        7 => compiled(storage, b"a[b] = c", |unit| unit.header().code_length > 0),
        8 => compiled(storage, b"a += 1", |unit| unit.header().code_length > 0),
        9 => compiled(storage, b"a ||= b", |unit| unit.header().code_length > 0),
        10 => compiled(storage, b"x++", |unit| unit.header().code_length > 0),
        11 => compiled(storage, b"[1, 2, , 3]", |unit| {
            unit.header().code_length > 0
        }),
        12 => compiled(storage, b"({a: 1, [b]: 2, c})", |unit| {
            unit.header().code_length > 0
        }),
        13 => compiled(storage, b"`a${b}c`", |unit| unit.header().code_length > 0),
        14 => compiled(storage, b"f(1, 2)", |unit| unit.header().code_length > 0),
        15 => compiled(storage, b"new A(1).b", |unit| unit.header().code_length > 0),
        16 => compiled(storage, b"a?.b", |unit| unit.header().code_length > 0),
        17 => compiled(storage, b"delete a.b", |unit| unit.header().code_length > 0),

        // Constants: interned once, and carrying the right values.
        18 => compiled(storage, b"a + a + a", |unit| {
            unit.header().constant_count == 1
        }),
        19 => compiled(storage, b"1.5", |unit| match unit.constant(0) {
            Some(constant) => constant.value().to_bits() == 1.5f64.to_bits(),
            None => false,
        }),
        20 => compiled(storage, b"'ab'", |unit| {
            let Some(constant) = unit.constant(0) else {
                return false;
            };
            let mut units = [0u16; 8];
            unit.constant_units(&constant, &mut units) == Some(2)
                && units[0] == u16::from(b'a')
                && units[1] == u16::from(b'b')
        }),

        // The same source compiles to the same bytes, so identity is content.
        21 => {
            let first = compile(storage, b"f(1) + `x${y}`");
            let mut digest_first = digest::Digest([0; 32]);
            if first.diagnostic == 0 {
                if let Some(bytes) = storage.image.get(..first.length) {
                    digest_first = digest::digest(bytes);
                }
            }
            let second = compile(storage, b"f(1) + `x${y}`");
            let mut digest_second = digest::Digest([1; 32]);
            if second.diagnostic == 0 {
                if let Some(bytes) = storage.image.get(..second.length) {
                    digest_second = digest::digest(bytes);
                }
            }
            first.diagnostic == 0
                && second.diagnostic == 0
                && first.length == second.length
                && digest_first == digest_second
        }
        // A BigInt literal carries its digits and the radix they were written
        // in, and becomes an exact integer when the image runs.
        22 => compiled(storage, b"1n", |unit| {
            unit.constant(0)
                .is_some_and(|constant| matches!(constant.kind, ConstantKind::BigInt))
        }),
        // A spread walks whatever its operand iterates, so it compiles.
        23 => compiled(storage, b"[...a]", |unit| unit.header().code_length > 0),
        24 => compiled(storage, b"f(...a)", |unit| unit.header().code_length > 0),
        // Object spread is admitted: the runtime copies the properties.
        25 => compiled(storage, b"({...a})", |unit| unit.header().code_length > 0),

        _ => true,
    }
}

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    announced: bool,
    report_out: i32,
    exit_out: i32,
    storage: Storage,
    progress: probe::Progress,
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
    probe::step(
        &mut state.progress,
        syscalls,
        state.report_out,
        state.exit_out,
        b"phasor-compile-probe",
        CASE_COUNT,
        |case| run_case(&mut state.storage, case),
    )
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
