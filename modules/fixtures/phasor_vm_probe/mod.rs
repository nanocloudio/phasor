//! On-graph conformance probe for the interpreter: expressions compiled from
//! source and run to a value, plus environments, closures, and calls.

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
#[path = "../../common/bigint.rs"]
mod bigint;
#[path = "../../common/binding.rs"]
mod binding;
#[path = "../../common/bytecode.rs"]
mod bytecode;
#[path = "../../common/diagnostic.rs"]
mod diagnostic;
#[path = "../../common/digest.rs"]
mod digest;
#[path = "../../common/dtoa.rs"]
mod dtoa;
#[path = "../../common/emit.rs"]
mod emit;
#[path = "../../common/env.rs"]
mod env;
#[path = "../../common/evalsite.rs"]
mod evalsite;
#[path = "../../common/feature.rs"]
mod feature;
#[path = "../../common/gc.rs"]
mod gc;
#[path = "../../common/heap.rs"]
mod heap;
#[path = "../../common/job.rs"]
mod job;
#[path = "../../common/lex.rs"]
mod lex;
#[path = "../../common/lower.rs"]
mod lower;
#[path = "../../common/numeric.rs"]
mod numeric;
#[path = "../../common/object.rs"]
mod object;
#[path = "../../common/parse.rs"]
mod parse;
#[path = "../../common/policy.rs"]
mod policy;
#[path = "../../common/promise.rs"]
mod promise;
#[path = "../../common/realm.rs"]
mod realm;
#[path = "../../common/regexp.rs"]
mod regexp;
#[path = "../../common/softfloat.rs"]
mod softfloat;
#[path = "../../common/source.rs"]
mod source;
#[path = "../../common/string.rs"]
mod string;
#[path = "../../common/unicode_id.rs"]
mod unicode_id;
#[path = "../../common/value.rs"]
mod value;
#[path = "../../common/verify.rs"]
mod verify;
#[path = "../../common/vm.rs"]
mod vm;

use arena::{Arena, Node, NodeKind};
use bytecode::{Constant, ExceptionRegion, ExportRecord, Function, ImportRecord, Opcode, Unit};
use emit::{CodeBuilder, Patch, UnitWriter};
use heap::{Heap, Slot};
use lex::Lexer;
use lower::{
    lower_expression, Binding as LexicalBinding, Pending as PendingFunction, Scope,
    Storage as LowerStorage,
};
use parse::Parser;
use regexp::Choice;
use source::{Limits, LineStart, LineTable};
use string::Atoms;
use value::{Handle, Value};
use vm::{Completion, Frame, Progress, Saves, Termination, Vm};

const CASE_COUNT: u16 = 98;
const FUEL: u32 = 400_000;
const STEPS: u64 = 400_000;

const NODE_CAPACITY: usize = 320;
const LIST_CAPACITY: usize = 320;
const NUMBER_CAPACITY: usize = 24;
const SCRATCH_CAPACITY: usize = 96;
const LINE_CAPACITY: usize = 8;
const CODE_CAPACITY: usize = 1024;
const IMAGE_CAPACITY: usize = 4096;
const CONSTANT_CAPACITY: usize = 48;
const DATA_CAPACITY: usize = 1024;
const POINT_CAPACITY: usize = 24;
const PATCH_CAPACITY: usize = 24;
const LABEL_CAPACITY: usize = 24;
const VERIFIER_CAPACITY: usize = 2048;
const ARENA_BYTES: usize = 64 * 1024;
/// An arena with room for the realm and a working program, but not for every
/// intermediate value one produces: a program of that shape must collect to
/// finish, and must run out of heap when it may not.
const COLLECTING_ARENA: usize = 36 * 1024;
const SLOT_COUNT: usize = 1536;
const ATOM_ENTRIES: usize = 512;
const ATOM_HANDLES: usize = 384;
const FRAME_COUNT: usize = 12;
const REGISTER_COUNT: usize = 96;
const WORKLIST: usize = 512;
const ROOT_COUNT: usize = 2048;
/// Room for the generated source that outgrows the heap.
const CHAIN_CAPACITY: usize = 1024;

/// Every buffer the front end and the interpreter need.
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
/// Where a match backtracks, what it must put back, and the units it runs over.
const CHOICE_COUNT: usize = 256;
const UNDO_COUNT: usize = 256;
const SUBJECT_UNITS: usize = 1024;
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
    verifier_state: [i32; VERIFIER_CAPACITY],
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
    arena: [u8; ARENA_BYTES],
    choices: [Choice; CHOICE_COUNT],
    undo: [(u8, u32); UNDO_COUNT],
    subject: [u16; SUBJECT_UNITS],
    slots: [Slot; SLOT_COUNT],
    entries: [u32; ATOM_ENTRIES],
    handles: [Handle; ATOM_HANDLES],
    frames: [Frame; FRAME_COUNT],
    registers: [Value; REGISTER_COUNT],
    worklist: [u32; WORKLIST],
    roots: [Handle; ROOT_COUNT],
    chain: [u8; CHAIN_CAPACITY],
}

/// Compile and run `source`, then check the result's string form.
fn evaluates(storage: &mut Storage, source: &[u8], expected: &[u8]) -> bool {
    let length = {
        let table = LineTable::new(&mut storage.starts);
        let Ok(lexer) = Lexer::new(source, Limits::CEILING, table, FUEL) else {
            return false;
        };
        let syntax = Arena::new(&mut storage.nodes, &mut storage.lists, &mut storage.numbers);
        let mut parser = Parser::new(lexer, syntax, &mut storage.scratch, Limits::CEILING);
        let Ok(root) = parser.parse_unit() else {
            return false;
        };
        let mut lowering = LowerStorage {
            code: &mut storage.code,
            image: &mut storage.image,
            constants: &mut storage.constants,
            constant_data: &mut storage.constant_data,
            safe_points: &mut storage.safe_points,
            patches: &mut storage.patches,
            labels: &mut storage.labels,
            verifier_state: &mut storage.verifier_state,
            unit_code: &mut storage.unit_code,
            unit_safe_points: &mut storage.unit_safe_points,
            functions: &mut storage.functions,
            exceptions: &mut storage.exceptions,
            scopes: &mut storage.scopes,
            bindings: &mut storage.lexical,
            pending: &mut storage.pending,
            imports: &mut storage.imports,
            exports: &mut storage.exports,
            eval_sites: &mut storage.eval_sites,
        };
        match lower_expression(source, parser.arena(), root, &mut lowering) {
            Ok(compiled) => compiled.length,
            Err(_) => return false,
        }
    };

    let Some(bytes) = storage.image.get(..length) else {
        return false;
    };
    let Ok(unit) = Unit::parse(bytes) else {
        return false;
    };

    let mut heap = Heap::new(&mut storage.arena, &mut storage.slots);
    let mut atoms = Atoms::new(&mut storage.entries, &mut storage.handles);
    let Ok(realm) = realm::create(&mut heap, &mut atoms) else {
        return false;
    };
    let mut machine = Vm::new(
        &unit,
        &mut heap,
        &mut atoms,
        &mut storage.frames,
        &mut storage.registers,
        realm,
        STEPS,
    );
    machine.attach_regexp(
        &mut storage.choices,
        &mut storage.undo,
        &mut storage.subject,
    );
    let value = match machine.run() {
        Completion::Value(value) => value,
        _ => return false,
    };
    let Ok(text) = machine.display(value) else {
        return false;
    };
    holds(machine.heap(), text, expected)
}

/// Write `links` string concatenations into the chain buffer, ending in a
/// length lookup so the result is small however long the strings get.
fn build_chain(storage: &mut Storage, links: usize) -> usize {
    let mut written = 0usize;
    let put = |bytes: &[u8], buffer: &mut [u8], written: &mut usize| {
        for &byte in bytes {
            if let Some(slot) = buffer.get_mut(*written) {
                *slot = byte;
                *written += 1;
            }
        }
    };
    put(b"(''", &mut storage.chain, &mut written);
    let mut index = 0usize;
    while index < links {
        put(b" + 'chunk", &mut storage.chain, &mut written);
        let digit = [b'0' + u8::try_from(index % 10).unwrap_or(0)];
        put(&digit, &mut storage.chain, &mut written);
        put(b"'", &mut storage.chain, &mut written);
        index += 1;
    }
    put(b").length", &mut storage.chain, &mut written);
    written
}

/// Compile and run the staged chain in `arena_bytes` of heap, collecting when
/// `collect` is set. Returns the number it produced and how many collections
/// ran.
/// Run a unit the way an isolate on a graph must: a bounded slice at a time,
/// with the machine rebuilt over the same storage between slices. A task that
/// survives that is a task that can wait for the outside world.
fn run_stepwise(storage: &mut Storage, unit: &Unit<'_>, arena_bytes: usize) -> Option<(f64, u32)> {
    let realm = {
        let arena = storage.arena.get_mut(..arena_bytes)?;
        let mut heap = Heap::with_worklist(arena, &mut storage.slots, &mut storage.worklist);
        let mut atoms = Atoms::new(&mut storage.entries, &mut storage.handles);
        realm::create(&mut heap, &mut atoms).ok()?
    };
    let mut saves = Saves::default();
    let mut started = false;
    let mut steps = 0u32;
    loop {
        let arena = storage.arena.get_mut(..arena_bytes)?;
        let mut heap = Heap::adopt(
            arena,
            &mut storage.slots,
            &mut storage.worklist,
            &saves.heap,
        );
        let mut atoms = Atoms::adopt(&mut storage.entries, &mut storage.handles, &saves.atoms);
        let mut machine = Vm::new(
            unit,
            &mut heap,
            &mut atoms,
            &mut storage.frames,
            &mut storage.registers,
            realm,
            STEPS,
        );
        machine.attach_regexp(
            &mut storage.choices,
            &mut storage.undo,
            &mut storage.subject,
        );
        machine.attach_collector(
            &mut storage.roots,
            256,
            u32::try_from(arena_bytes / 4).unwrap_or(0),
        );
        if started {
            machine.restore_all(&saves);
        } else {
            machine.start().ok()?;
            started = true;
        }
        match machine.resume(512) {
            Progress::Running => {
                saves = machine.save();
                steps += 1;
                if steps > 10_000 {
                    return None;
                }
            }
            Progress::Finished(Completion::Value(value)) => {
                return Some((value.as_number(), machine.collections()));
            }
            Progress::Finished(_) => return None,
        }
    }
}

fn run_chain(
    storage: &mut Storage,
    length: usize,
    arena_bytes: usize,
    collect: bool,
    stepwise: bool,
) -> Option<(f64, u32)> {
    let image_length = {
        let table = LineTable::new(&mut storage.starts);
        let source: &[u8] = storage.chain.get(..length)?;
        // The staged source is read only while the front end writes elsewhere.
        let source: &[u8] = unsafe { core::slice::from_raw_parts(source.as_ptr(), source.len()) };
        let lexer = Lexer::new(source, Limits::CEILING, table, FUEL).ok()?;
        let syntax = Arena::new(&mut storage.nodes, &mut storage.lists, &mut storage.numbers);
        let mut parser = Parser::new(lexer, syntax, &mut storage.scratch, Limits::CEILING);
        let root = parser.parse_unit().ok()?;
        let mut lowering = LowerStorage {
            code: &mut storage.code,
            image: &mut storage.image,
            constants: &mut storage.constants,
            constant_data: &mut storage.constant_data,
            safe_points: &mut storage.safe_points,
            patches: &mut storage.patches,
            labels: &mut storage.labels,
            verifier_state: &mut storage.verifier_state,
            unit_code: &mut storage.unit_code,
            unit_safe_points: &mut storage.unit_safe_points,
            functions: &mut storage.functions,
            exceptions: &mut storage.exceptions,
            scopes: &mut storage.scopes,
            bindings: &mut storage.lexical,
            pending: &mut storage.pending,
            imports: &mut storage.imports,
            exports: &mut storage.exports,
            eval_sites: &mut storage.eval_sites,
        };
        lower_expression(source, parser.arena(), root, &mut lowering)
            .ok()
            .map(|compiled| compiled.length)?
    };
    let bytes = storage.image.get(..image_length)?;
    if stepwise {
        // The image is staged and is not written again while it runs, so the
        // machine may keep reading it while the rest of the storage moves.
        let bytes: &[u8] = unsafe { core::slice::from_raw_parts(bytes.as_ptr(), bytes.len()) };
        let unit = Unit::parse(bytes).ok()?;
        return run_stepwise(storage, &unit, arena_bytes);
    }
    let unit = Unit::parse(bytes).ok()?;

    let arena = storage.arena.get_mut(..arena_bytes)?;
    let mut heap = Heap::with_worklist(arena, &mut storage.slots, &mut storage.worklist);
    let mut atoms = Atoms::new(&mut storage.entries, &mut storage.handles);
    let realm = realm::create(&mut heap, &mut atoms).ok()?;
    let mut machine = Vm::new(
        &unit,
        &mut heap,
        &mut atoms,
        &mut storage.frames,
        &mut storage.registers,
        realm,
        STEPS,
    );
    machine.attach_regexp(
        &mut storage.choices,
        &mut storage.undo,
        &mut storage.subject,
    );
    if collect {
        machine.attach_collector(
            &mut storage.roots,
            256,
            u32::try_from(arena_bytes / 4).unwrap_or(0),
        );
    }
    match machine.run() {
        Completion::Value(value) => Some((value.as_number(), machine.collections())),
        _ => None,
    }
}

/// Whether a string cell holds exactly this ASCII text.
fn holds(heap: &Heap<'_>, handle: Handle, text: &[u8]) -> bool {
    let Ok(length) = string::length(heap, handle) else {
        return false;
    };
    if length as usize != text.len() {
        return false;
    }
    let mut index = 0u32;
    while (index as usize) < text.len() {
        match string::unit_at(heap, handle, index) {
            Ok(Some(unit)) if unit == u16::from(text[index as usize]) => {}
            _ => return false,
        }
        index += 1;
    }
    true
}

/// Whether running `source` throws.
fn throws(storage: &mut Storage, source: &[u8]) -> bool {
    ends(storage, source, Ending::Throw)
}

/// A task that stops on a bound rather than throwing.
fn stops(storage: &mut Storage, source: &[u8]) -> bool {
    ends(storage, source, Ending::Quota)
}

/// How a program is expected to end.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Ending {
    Throw,
    Quota,
}

fn ends(storage: &mut Storage, source: &[u8], ending: Ending) -> bool {
    let length = {
        let table = LineTable::new(&mut storage.starts);
        let Ok(lexer) = Lexer::new(source, Limits::CEILING, table, FUEL) else {
            return false;
        };
        let syntax = Arena::new(&mut storage.nodes, &mut storage.lists, &mut storage.numbers);
        let mut parser = Parser::new(lexer, syntax, &mut storage.scratch, Limits::CEILING);
        let Ok(root) = parser.parse_unit() else {
            return false;
        };
        let mut lowering = LowerStorage {
            code: &mut storage.code,
            image: &mut storage.image,
            constants: &mut storage.constants,
            constant_data: &mut storage.constant_data,
            safe_points: &mut storage.safe_points,
            patches: &mut storage.patches,
            labels: &mut storage.labels,
            verifier_state: &mut storage.verifier_state,
            unit_code: &mut storage.unit_code,
            unit_safe_points: &mut storage.unit_safe_points,
            functions: &mut storage.functions,
            exceptions: &mut storage.exceptions,
            scopes: &mut storage.scopes,
            bindings: &mut storage.lexical,
            pending: &mut storage.pending,
            imports: &mut storage.imports,
            exports: &mut storage.exports,
            eval_sites: &mut storage.eval_sites,
        };
        match lower_expression(source, parser.arena(), root, &mut lowering) {
            Ok(compiled) => compiled.length,
            Err(_) => return false,
        }
    };
    let Some(bytes) = storage.image.get(..length) else {
        return false;
    };
    let Ok(unit) = Unit::parse(bytes) else {
        return false;
    };
    let mut heap = Heap::new(&mut storage.arena, &mut storage.slots);
    let mut atoms = Atoms::new(&mut storage.entries, &mut storage.handles);
    let Ok(realm) = realm::create(&mut heap, &mut atoms) else {
        return false;
    };
    let mut machine = Vm::new(
        &unit,
        &mut heap,
        &mut atoms,
        &mut storage.frames,
        &mut storage.registers,
        realm,
        STEPS,
    );
    machine.attach_regexp(
        &mut storage.choices,
        &mut storage.undo,
        &mut storage.subject,
    );
    match (machine.run(), ending) {
        (Completion::Throw(_), Ending::Throw) => true,
        (Completion::Terminated(reason), Ending::Quota) => reason == Termination::QuotaExceeded,
        _ => false,
    }
}

/// Build a two-function unit: an entry, and a callee that adds its first
/// argument to a value it closed over.
fn build_closure_unit(storage: &mut Storage) -> Option<usize> {
    let mut entry = [0u8; 16];
    let entry_length = {
        let mut builder = CodeBuilder::new(
            &mut entry,
            &mut storage.safe_points,
            &mut storage.patches,
            &mut storage.labels,
        );
        builder.emit(Opcode::LdaUndefined, &[]);
        builder.emit(Opcode::Return, &[]);
        builder.finish().ok()?
    };
    let mut callee = [0u8; 32];
    let callee_length = {
        let mut builder = CodeBuilder::new(
            &mut callee,
            &mut storage.safe_points,
            &mut storage.patches,
            &mut storage.labels,
        );
        builder.emit(Opcode::LdaContextSlot, &[0, 1]);
        builder.emit(Opcode::Star, &[1]);
        builder.emit(Opcode::Ldar, &[0]);
        builder.emit(Opcode::Add, &[1]);
        builder.emit(Opcode::Return, &[]);
        builder.finish().ok()?
    };

    let mut code = [0u8; 64];
    let total = entry_length as usize + callee_length as usize;
    code.get_mut(..entry_length as usize)?
        .copy_from_slice(entry.get(..entry_length as usize)?);
    code.get_mut(entry_length as usize..total)?
        .copy_from_slice(callee.get(..callee_length as usize)?);

    let functions = [
        Function {
            code_offset: 0,
            code_length: entry_length,
            register_count: 1,
            argument_count: 0,
            frame_extent: 1,
            exception_offset: 0,
            exception_count: 0,
            safe_point_offset: 0,
            safe_point_count: 0,
            context_depth: 4,
            context_slots: 0,
            flags: 0,
        },
        Function {
            code_offset: entry_length,
            code_length: callee_length,
            register_count: 2,
            argument_count: 1,
            frame_extent: 2,
            exception_offset: 0,
            exception_count: 0,
            safe_point_offset: 0,
            safe_point_count: 0,
            context_depth: 4,
            context_slots: 0,
            flags: 0,
        },
    ];
    UnitWriter::new(&mut storage.image)
        .write(&functions, &[], &[], code.get(..total)?, &[], &[], 0)
        .ok()
}

#[allow(
    clippy::match_same_arms,
    reason = "each case is an independent assertion and merging arms would hide which one failed"
)]
fn run_case(storage: &mut Storage, case: u16) -> bool {
    match case {
        // Arithmetic and precedence, evaluated end to end from source.
        0 => evaluates(storage, b"1 + 2", b"3"),
        1 => evaluates(storage, b"2 * 3 + 4", b"10"),
        2 => evaluates(storage, b"(2 + 3) * 4", b"20"),
        3 => evaluates(storage, b"7 % 3", b"1"),
        4 => evaluates(storage, b"2 ** 10", b"1024"),
        5 => evaluates(storage, b"0.1 + 0.2", b"0.30000000000000004"),
        6 => evaluates(storage, b"1 / 0", b"Infinity"),
        7 => evaluates(storage, b"-(3)", b"-3"),

        // Strings and coercion.
        8 => evaluates(storage, b"'a' + 'b'", b"ab"),
        9 => evaluates(storage, b"1 + '2'", b"12"),
        10 => evaluates(storage, b"'abc'.length", b"3"),
        11 => evaluates(storage, b"'abc'[1]", b"b"),
        12 => evaluates(storage, b"`x${1 + 1}y`", b"x2y"),
        13 => evaluates(storage, b"'' + [1, 2]", b"1,2"),
        14 => evaluates(storage, b"'' + {}", b"[object Object]"),

        // Comparison, equality, and the logical operators.
        15 => evaluates(storage, b"1 < 2", b"true"),
        16 => evaluates(storage, b"'a' < 'b'", b"true"),
        17 => evaluates(storage, b"1 == '1'", b"true"),
        18 => evaluates(storage, b"1 === '1'", b"false"),
        19 => evaluates(storage, b"null ?? 5", b"5"),
        20 => evaluates(storage, b"0 || 'x'", b"x"),
        21 => evaluates(storage, b"typeof undefined", b"undefined"),

        // Objects and arrays.
        22 => evaluates(storage, b"({a: 1, b: 2}).b", b"2"),
        23 => evaluates(storage, b"[1, 2, 3].length", b"3"),

        // A read through a nullish value throws.
        24 => throws(storage, b"null.x"),

        // A program whose intermediate values far outgrow the heap runs when
        // the machine may collect, and runs out of heap when it may not.
        26 => {
            let length = build_chain(storage, 90);
            let collected = run_chain(storage, length, COLLECTING_ARENA, true, false);
            match collected {
                Some((value, collections)) => value == 540.0 && collections > 0,
                None => false,
            }
        }
        // The same program, run a slice at a time with the machine rebuilt
        // between slices, gives the same answer: nothing about a task lives in
        // the machine that a module cannot carry itself.
        28 => {
            let length = build_chain(storage, 90);
            let straight = run_chain(storage, length, COLLECTING_ARENA, true, false);
            let stepwise = run_chain(storage, length, COLLECTING_ARENA, true, true);
            match (straight, stepwise) {
                (Some((left, _)), Some((right, collections))) => {
                    left == right && right == 540.0 && collections > 0
                }
                _ => false,
            }
        }
        27 => {
            // Without a collector the same program runs out of heap. The
            // arena here is the realm plus a little, so the chain cannot fit.
            let length = build_chain(storage, 90);
            run_chain(storage, length, COLLECTING_ARENA, false, false).is_none()
        }

        // Environments, closures, and calls, through a hand-built unit.
        25 => {
            let Some(length) = build_closure_unit(storage) else {
                return false;
            };
            let Some(bytes) = storage.image.get(..length) else {
                return false;
            };
            let Ok(unit) = verify::admit(bytes, &mut storage.verifier_state) else {
                return false;
            };

            let mut heap = Heap::new(&mut storage.arena, &mut storage.slots);
            let mut atoms = Atoms::new(&mut storage.entries, &mut storage.handles);
            let Ok(realm) = realm::create(&mut heap, &mut atoms) else {
                return false;
            };
            let Ok(name) = atoms.intern(&mut heap, &[u16::from(b'n')]) else {
                return false;
            };
            let Ok(closure) = env::create(
                &mut heap,
                env::EnvironmentKind::Declarative,
                Value::object(realm.environment),
                2,
            ) else {
                return false;
            };
            if env::declare_initialised(
                &mut heap,
                closure,
                name,
                env::binding::MUTABLE,
                Value::number(10.0),
            )
            .is_err()
            {
                return false;
            }
            let Ok(function) = object::create_function(
                &mut heap,
                Value::object(realm.function_prototype),
                1,
                Value::object(closure),
                0,
            ) else {
                return false;
            };

            let mut machine = Vm::new(
                &unit,
                &mut heap,
                &mut atoms,
                &mut storage.frames,
                &mut storage.registers,
                realm,
                STEPS,
            );
            machine.attach_regexp(
                &mut storage.choices,
                &mut storage.undo,
                &mut storage.subject,
            );
            let first = machine.call(
                Value::object(function),
                Value::UNDEFINED,
                &[Value::number(5.0)],
            );
            let second = machine.call(
                Value::object(function),
                Value::UNDEFINED,
                &[Value::number(1.5)],
            );
            match (first, second) {
                (Ok(first), Ok(second)) => first.as_number() == 15.0 && second.as_number() == 11.5,
                _ => false,
            }
        }
        // Statements, declarations, and functions, end to end from source.
        29 => evaluates(storage, b"var x = 1; x + 1", b"2"),
        30 => evaluates(storage, b"let a = 2; { let a = 3; } a", b"2"),
        31 => evaluates(storage, b"var s = 0; for (var i = 0; i < 4; i = i + 1) { s = s + i; } s", b"6"),
        32 => evaluates(storage, b"function add(a, b) { return a + b; } add(20, 22)", b"42"),
        33 => evaluates(storage, b"var f = function (n) { return n * 2; }; f(21)", b"42"),
        34 => evaluates(storage, b"var g = (n) => n + 1; g(41)", b"42"),
        35 => evaluates(
            storage,
            b"function outer() { var v = 10; return function () { return v + 1; }; } outer()()",
            b"11",
        ),
        36 => evaluates(
            storage,
            b"var r = ''; try { throw 'x'; } catch (e) { r = 'c' + e; } finally { r = r + '!'; } r",
            b"cx!",
        ),
        37 => evaluates(
            storage,
            b"var n = 0; while (true) { n = n + 1; if (n === 3) { break; } } n",
            b"3",
        ),
        38 => evaluates(
            storage,
            b"switch (2) { case 1: 'one'; break; case 2: 'two'; break; default: 'other'; }",
            b"two",
        ),
        39 => evaluates(
            storage,
            b"var cs = []; for (let i = 0; i < 3; i = i + 1) { cs.push(function () { return i; }); } cs[0]() + cs[2]()",
            b"2",
        ),

        // The library, which the engine implements itself.
        40 => evaluates(storage, b"[1, 2, 3].map(function (x) { return x * 2; }).join(',')", b"2,4,6"),
        41 => evaluates(storage, b"[3, 1, 2].sort().join('')", b"123"),
        42 => evaluates(storage, b"[1, 2, 3].reduce(function (a, b) { return a + b; }, 0)", b"6"),
        43 => evaluates(storage, b"'hello'.toUpperCase()", b"HELLO"),
        44 => evaluates(storage, b"'a-b-c'.split('-').length", b"3"),
        45 => evaluates(storage, b"(255).toString(16)", b"ff"),
        46 => evaluates(storage, b"(3.14159).toFixed(2)", b"3.14"),
        47 => evaluates(storage, b"Math.max(1, 5, 3) + Math.sqrt(16)", b"9"),
        48 => evaluates(storage, b"Object.keys({a: 1, b: 2}).join('')", b"ab"),
        49 => evaluates(storage, b"typeof Symbol()", b"symbol"),
        50 => evaluates(storage, b"Symbol('x').toString()", b"Symbol(x)"),
        51 => evaluates(storage, b"var s = Symbol('k'); var o = {}; o[s] = 5; o[s]", b"5"),
        52 => evaluates(storage, b"function f(a, b) { return this.v + a + b; } f.call({v: 1}, 2, 3)", b"6"),
        53 => evaluates(storage, b"function h(a, b) { return a + b; } h.bind(null, 10)(5)", b"15"),

        // Iteration: `for` over what a value iterates, and a spread.
        54 => evaluates(storage, b"var t = 0; for (var v of [1, 2, 3]) { t = t + v; } t", b"6"),
        55 => evaluates(storage, b"var t = ''; for (var c of 'abc') { t = t + c + '.'; } t", b"a.b.c."),
        56 => evaluates(storage, b"var t = ''; for (var k in {a: 1, b: 2}) { t = t + k; } t", b"ab"),
        57 => evaluates(storage, b"[0, ...[1, 2], 3].join('')", b"0123"),
        58 => evaluates(storage, b"function add3(a, b, c) { return a + b + c; } add3(...[1, 2, 3])", b"6"),
        59 => evaluates(storage, b"Array.from('abc').join('-')", b"a-b-c"),

        // BigInts, which are exact and never mix with Numbers.
        60 => evaluates(storage, b"2n + 3n", b"5"),
        61 => evaluates(storage, b"typeof 1n", b"bigint"),
        62 => evaluates(storage, b"(2n ** 64n)", b"18446744073709551616"),
        63 => evaluates(storage, b"(-7n) / 2n", b"-3"),
        64 => evaluates(storage, b"(-7n) % 2n", b"-1"),
        65 => evaluates(storage, b"0xffn + 1n", b"256"),
        66 => evaluates(storage, b"1n === 1n", b"true"),
        67 => evaluates(storage, b"1n == 1", b"true"),
        68 => evaluates(storage, b"5n > 3n", b"true"),
        69 => evaluates(storage, b"~5n", b"-6"),
        70 => evaluates(storage, b"(-5n) & 3n", b"3"),
        71 => evaluates(storage, b"(255n).toString(16)", b"ff"),
        72 => evaluates(storage, b"BigInt('123') * 2n", b"246"),
        73 => throws(storage, b"1n + 1"),
        74 => evaluates(storage, b"Number(9n) + 1", b"10"),
        75 => evaluates(
            storage,
            b"(123456789012345678901234567890n * 1000000n)",
            b"123456789012345678901234567890000000",
        ),

        // Regular expressions: the pattern grammar, the matcher, and the string
        // methods that take a pattern.
        76 => evaluates(storage, b"/abc/.test('xabcy')", b"true"),
        77 => evaluates(storage, b"/a+/.exec('baaa')[0]", b"aaa"),
        78 => evaluates(storage, b"'2026-08-25'.match(/(\\d+)-(\\d+)-(\\d+)/)[2]", b"08"),
        79 => evaluates(storage, b"'a1b2'.match(/\\d/g).join('')", b"12"),
        80 => evaluates(storage, b"'hello'.search(/l+/)", b"2"),
        81 => evaluates(storage, b"String(/ab+c/gi)", b"/ab+c/gi"),
        82 => evaluates(storage, b"'a1b2c3'.replace(/\\d/g, '#')", b"a#b#c#"),
        83 => evaluates(
            storage,
            b"'2026-08-25'.replace(/(\\d+)-(\\d+)-(\\d+)/, '$3/$2/$1')",
            b"25/08/2026",
        ),
        84 => evaluates(storage, b"'a,b;c'.split(/[,;]/).join('|')", b"a|b|c"),
        85 => evaluates(storage, b"/^\\d{4}$/.test('2026')", b"true"),
        86 => evaluates(storage, b"/(a+)\\1/.test('aaaa')", b"true"),
        87 => evaluates(storage, b"/a(?=b)/.test('ac')", b"false"),
        88 => evaluates(storage, b"'x'.replace(/x/, function (m) { return m + '!'; })", b"x!"),
        89 => evaluates(storage, b"/a.c/s.test('a\\nc')", b"true"),

        // An ordinary function has a `prototype`, an instance inherits from it,
        // and the two point at each other.
        90 => evaluates(
            storage,
            b"(function () { function A() {} A.prototype.v = 3; return new A().v; })()",
            b"3",
        ),
        91 => evaluates(
            storage,
            b"(function () { function A() {} return A.prototype.constructor === A; })()",
            b"true",
        ),
        92 => evaluates(
            storage,
            b"(function () { function A() {} var o = new A(); return Object.getPrototypeOf(o) === A.prototype; })()",
            b"true",
        ),
        // A defined property may be a pair of accessors rather than a value.
        93 => evaluates(
            storage,
            b"(function () { var o = {}; Object.defineProperty(o, 'x', {get: function () { return 4; }}); return o.x; })()",
            b"4",
        ),
        94 => evaluates(
            storage,
            b"(function () { var o = {v: 0}; Object.defineProperty(o, 'x', {set: function (n) { this.v = n; }}); o.x = 3; return o.v; })()",
            b"3",
        ),

        // A block that declares a binding and then returns leaves its context
        // behind rather than popping it where nothing can reach the pop.
        95 => evaluates(
            storage,
            b"(function () { if (true) { const v = 1; return v; } return 0; })()",
            b"1",
        ),
        // A loop that begins a function marks the position the function entry
        // already marked, and one position is one safe point.
        96 => evaluates(storage, b"(function () { while (true) { return 1; } })()", b"1"),

        // Listing more keys than the storage admits is a quota the task ends
        // on, never a list shorter than the object.
        97 => stops(
            storage,
            b"(function () { var o = {}; for (var i = 0; i < 200; i++) { o['k' + i] = i; } return Object.keys(o).length; })()",
        ),

        _ => true,
    }
}

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    report_out: i32,
    exit_out: i32,
    storage: Storage,
    case: u16,
    failures: u16,
    /// The first case that failed, which is what a report names.
    first_failure: u16,
    phase: u8,
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
        let state = state.cast::<State>();
        core::ptr::write_bytes(state.cast::<u8>(), 0, core::mem::size_of::<State>());
        core::ptr::addr_of_mut!((*state).syscalls).write(table);
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
    if state.case == 0 && state.failures == 0 && state.first_failure == 0 {
        state.first_failure = u16::MAX;
    }
    if state.syscalls.is_null() {
        return -2;
    }
    let syscalls = unsafe { &*state.syscalls };

    if state.phase == 2 {
        return 1;
    }
    if state.case < CASE_COUNT {
        let case = state.case;
        if !run_case(&mut state.storage, case) {
            state.failures = state.failures.saturating_add(1);
            if state.first_failure == u16::MAX {
                state.first_failure = case;
            }
        }
        state.case = state.case.saturating_add(1);
        return 0;
    }

    if state.phase == 0 {
        // A failure names the first case that failed, so a report is enough
        // to find it without instrumenting the module again.
        let mut buffer = [0u8; 64];
        let report: &[u8] = if state.failures == 0 {
            b"phasor-vm-probe: 98 passed\n"
        } else {
            let prefix = b"phasor-vm-probe: failed at ";
            let mut length = 0usize;
            while length < prefix.len() {
                buffer[length] = prefix[length];
                length += 1;
            }
            let mut digits = [0u8; 5];
            let mut count = 0usize;
            let mut value = state.first_failure;
            loop {
                digits[count] = b'0' + u8::try_from(value % 10).unwrap_or(0);
                count += 1;
                value /= 10;
                if value == 0 {
                    break;
                }
            }
            while count > 0 {
                count -= 1;
                buffer[length] = digits[count];
                length += 1;
            }
            buffer[length] = b'\n';
            length += 1;
            buffer.get(..length).unwrap_or(&[])
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
