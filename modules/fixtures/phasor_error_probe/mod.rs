//! On-graph conformance probe for exceptions: error objects, the error
//! constructors, unwinding to a handler, and a throw that crosses a frame.

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
use vm::{Completion, Frame, Vm};

const CASE_COUNT: u16 = 20;
const FUEL: u32 = 400_000;
const STEPS: u64 = 400_000;

const NODE_CAPACITY: usize = 96;
const LIST_CAPACITY: usize = 96;
const NUMBER_CAPACITY: usize = 24;
const SCRATCH_CAPACITY: usize = 48;
const LINE_CAPACITY: usize = 8;
const CODE_CAPACITY: usize = 512;
const IMAGE_CAPACITY: usize = 2048;
const CONSTANT_CAPACITY: usize = 24;
const DATA_CAPACITY: usize = 512;
const POINT_CAPACITY: usize = 24;
const PATCH_CAPACITY: usize = 24;
const LABEL_CAPACITY: usize = 24;
const VERIFIER_CAPACITY: usize = 512;
const ARENA_BYTES: usize = 64 * 1024;
const SLOT_COUNT: usize = 1536;
const ATOM_ENTRIES: usize = 512;
const ATOM_HANDLES: usize = 384;
const FRAME_COUNT: usize = 12;
const REGISTER_COUNT: usize = 96;

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
    arena: [u8; ARENA_BYTES],
    choices: [Choice; CHOICE_COUNT],
    undo: [(u8, u32); UNDO_COUNT],
    subject: [u16; SUBJECT_UNITS],
    slots: [Slot; SLOT_COUNT],
    entries: [u32; ATOM_ENTRIES],
    handles: [Handle; ATOM_HANDLES],
    frames: [Frame; FRAME_COUNT],
    registers: [Value; REGISTER_COUNT],
}

/// Compile `source` into the image buffer, returning its length.
fn compile(storage: &mut Storage, source: &[u8]) -> Option<usize> {
    let table = LineTable::new(&mut storage.starts);
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
    };
    lower_expression(source, parser.arena(), root, &mut lowering)
        .ok()
        .map(|compiled| compiled.length)
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

/// Compile and run `source`, expecting a throw whose string form is `expected`.
fn throws_with(storage: &mut Storage, source: &[u8], expected: &[u8]) -> bool {
    let length = match compile(storage, source) {
        Some(length) => length,
        None => return false,
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
    let thrown = match machine.run() {
        Completion::Throw(value) => value,
        _ => return false,
    };
    let Ok(text) = machine.display(thrown) else {
        return false;
    };
    holds(machine.heap(), text, expected)
}

/// A unit whose entry throws and catches in the same frame.
fn build_catching_unit(storage: &mut Storage) -> Option<(usize, ExceptionRegion)> {
    let mut code = [0u8; 32];
    let region;
    let length = {
        let mut builder = CodeBuilder::new(
            &mut code,
            &mut storage.safe_points,
            &mut storage.patches,
            &mut storage.labels,
        );
        let start = builder.length();
        builder.emit(Opcode::LdaSmi, &[7]);
        builder.emit(Opcode::Throw, &[]);
        let end = builder.length();
        let handler = builder.length();
        builder.emit(Opcode::Star, &[0]);
        builder.emit(Opcode::LdaSmi, &[1]);
        builder.emit(Opcode::Add, &[0]);
        builder.emit(Opcode::Return, &[]);
        region = ExceptionRegion {
            start,
            end,
            handler,
            register: 0,
            context_depth: 0,
        };
        builder.finish().ok()?
    };
    let function = Function {
        code_offset: 0,
        code_length: length,
        register_count: 2,
        argument_count: 0,
        frame_extent: 2,
        exception_offset: 0,
        exception_count: 1,
        safe_point_offset: 0,
        safe_point_count: 0,
        context_depth: 0,
        context_slots: 0,
        flags: 0,
    };
    let written = UnitWriter::new(&mut storage.image)
        .write(
            &[function],
            &[],
            &[],
            code.get(..length as usize)?,
            &[region],
            &[],
            0,
        )
        .ok()?;
    Some((written, region))
}

/// A unit whose entry calls a function that throws, and catches it.
fn build_crossing_unit(storage: &mut Storage) -> Option<usize> {
    let mut entry = [0u8; 48];
    let region;
    let entry_length = {
        let mut builder = CodeBuilder::new(
            &mut entry,
            &mut storage.safe_points,
            &mut storage.patches,
            &mut storage.labels,
        );
        let start = builder.length();
        builder.emit(Opcode::Ldar, &[0]);
        builder.emit(Opcode::Star, &[1]);
        builder.emit(Opcode::LdaUndefined, &[]);
        builder.emit(Opcode::Star, &[2]);
        builder.emit(Opcode::Call, &[1, 2, 1]);
        let end = builder.length();
        let handler = builder.length();
        builder.emit(Opcode::Star, &[3]);
        builder.emit(Opcode::Ldar, &[3]);
        builder.emit(Opcode::Return, &[]);
        region = ExceptionRegion {
            start,
            end,
            handler,
            register: 3,
            context_depth: 0,
        };
        builder.finish().ok()?
    };
    let mut callee = [0u8; 16];
    let callee_length = {
        let mut builder = CodeBuilder::new(
            &mut callee,
            &mut storage.safe_points,
            &mut storage.patches,
            &mut storage.labels,
        );
        builder.emit(Opcode::LdaSmi, &[42]);
        builder.emit(Opcode::Throw, &[]);
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
            register_count: 4,
            argument_count: 1,
            frame_extent: 4,
            exception_offset: 0,
            exception_count: 1,
            safe_point_offset: 0,
            safe_point_count: 0,
            context_depth: 0,
            context_slots: 0,
            flags: 0,
        },
        Function {
            code_offset: entry_length,
            code_length: callee_length,
            register_count: 1,
            argument_count: 0,
            frame_extent: 1,
            exception_offset: 0,
            exception_count: 0,
            safe_point_offset: 0,
            safe_point_count: 0,
            context_depth: 0,
            context_slots: 0,
            flags: 0,
        },
    ];
    UnitWriter::new(&mut storage.image)
        .write(&functions, &[], &[], code.get(..total)?, &[region], &[], 0)
        .ok()
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
        // The error constructors and their prototypes.
        0 => evaluates(storage, b"new Error('boom').message", b"boom"),
        1 => evaluates(storage, b"new TypeError('t').name", b"TypeError"),
        2 => evaluates(storage, b"new Error().name", b"Error"),
        3 => evaluates(storage, b"'' + new Error('x')", b"Error: x"),
        4 => evaluates(storage, b"'' + new RangeError()", b"RangeError"),
        5 => evaluates(
            storage,
            b"(new TypeError('t')) instanceof TypeError",
            b"true",
        ),
        6 => evaluates(storage, b"(new TypeError('t')) instanceof Error", b"true"),
        7 => evaluates(storage, b"(new Error('e')) instanceof TypeError", b"false"),
        8 => evaluates(storage, b"typeof Error", b"function"),
        9 => evaluates(storage, b"Error('called').message", b"called"),

        // The errors the engine itself throws.
        10 => throws_with(storage, b"null.x", b"TypeError"),
        11 => throws_with(storage, b"undefined.y", b"TypeError"),
        12 => throws_with(storage, b"(1)()", b"TypeError"),
        13 => throws_with(storage, b"missingBinding", b"ReferenceError"),
        14 => throws_with(storage, b"({}) instanceof 1", b"TypeError"),

        // `new` on an ordinary object and on a non-constructor.
        15 => throws_with(storage, b"new (1)", b"TypeError"),
        16 => evaluates(storage, b"typeof new Error('x')", b"object"),

        // Unwinding to a handler in the same frame.
        17 => {
            let Some((length, _)) = build_catching_unit(storage) else {
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
            matches!(machine.run(), Completion::Value(value) if value.as_number() == 8.0)
        }

        // A throw that crosses a frame is caught by the caller's region.
        18 => {
            let Some(length) = build_crossing_unit(storage) else {
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
            let Ok(thrower) = object::create_function(
                &mut heap,
                Value::object(realm.function_prototype),
                1,
                Value::UNDEFINED,
                0,
            ) else {
                return false;
            };
            let Ok(entry) = object::create_function(
                &mut heap,
                Value::object(realm.function_prototype),
                0,
                Value::UNDEFINED,
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
            matches!(
                machine.call(
                    Value::object(entry),
                    Value::UNDEFINED,
                    &[Value::object(thrower)],
                ),
                Ok(value) if value.as_number() == 42.0
            )
        }

        // An uncaught throw ends the run with the thrown value.
        19 => {
            let Some(length) = build_crossing_unit(storage) else {
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
            let Ok(thrower) = object::create_function(
                &mut heap,
                Value::object(realm.function_prototype),
                1,
                Value::UNDEFINED,
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
            matches!(
                machine.call(Value::object(thrower), Value::UNDEFINED, &[]),
                Err(Completion::Throw(value)) if value.as_number() == 42.0
            )
        }
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
            b"phasor-error-probe: 20 passed\n"
        } else {
            let prefix = b"phasor-error-probe: failed at ";
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
