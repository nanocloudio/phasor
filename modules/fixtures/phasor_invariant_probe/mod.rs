//! On-graph proof of the isolate's invariants over every conformance vector.
//!
//! For each program the probe checks that the run ends with an outcome from the
//! closed set, that the heap never exceeds the arena it was given, that a
//! smaller heap and shorter slices do not change the result, that a run
//! cancelled before it starts stops as cancelled, and that a tiny instruction
//! budget ends the run rather than overrunning it.

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
#[path = "../../common/replay.rs"]
mod replay;
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
#[path = "../../common/vectors.rs"]
mod vectors;
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
use policy::{Outcome, Policy};
use regexp::Choice;
use replay::{digest_outcome, Event, Profile, Recording};
use source::{Limits, LineStart, LineTable};
use string::Atoms;
use value::{Handle, Value};
use vm::{Completion, Frame, Progress, Vm};

const CASE_COUNT: u16 = 25;
/// Programs each case checks.
const PER_CASE: usize = 6;
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
const WORKLIST: usize = 512;
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
    worklist: [u32; WORKLIST],
    entries: [u32; ATOM_ENTRIES],
    handles: [Handle; ATOM_HANDLES],
    frames: [Frame; FRAME_COUNT],
    registers: [Value; REGISTER_COUNT],
}

/// Compile `source` and run it, returning the recording of the run.
///
/// `arena_bytes` and `slice` are deliberately variable: they change when
/// collection happens and how often the task yields, and neither may change
/// what the program produces.
fn record_run(
    storage: &mut Storage,
    source: &[u8],
    arena_bytes: usize,
    slice: u64,
) -> Option<Recording> {
    let length = {
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
            .map(|compiled| compiled.length)?
    };

    let bytes = storage.image.get(..length)?;
    let unit = Unit::parse(bytes).ok()?;
    let policy = Policy {
        heap_bytes: u32::try_from(arena_bytes).unwrap_or(0),
        ..Policy::MODEST
    };
    let mut recording = Recording::new(Profile::Deterministic, unit.logical_digest(), &policy);

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
    machine.start().ok()?;
    let completion = loop {
        match machine.resume(slice) {
            Progress::Running => {}
            Progress::Finished(completion) => break completion,
        }
    };

    let mut units = [0u16; 64];
    let mut written = 0usize;
    if let Completion::Value(value) | Completion::Throw(value) = completion {
        if let Ok(text) = machine.display(value) {
            let length = string::length(machine.heap(), text).unwrap_or(0) as usize;
            if length <= units.len() {
                written =
                    string::copy_units(machine.heap(), text, &mut units[..length]).unwrap_or(0);
            }
        }
    }
    recording.finish(digest_outcome(
        completion.outcome(),
        units.get(..written).unwrap_or(&[]),
    ));
    Some(recording)
}

/// Whether the same source produces the same outcome under two different heap
/// and slice configurations.
fn agrees(storage: &mut Storage, source: &[u8]) -> bool {
    let Some(wide) = record_run(storage, source, ARENA_BYTES, u64::MAX) else {
        return false;
    };
    let Some(narrow) = record_run(storage, source, ARENA_BYTES / 4, 3) else {
        return false;
    };
    let Some(again) = record_run(storage, source, ARENA_BYTES, u64::MAX) else {
        return false;
    };
    wide.outcome() == narrow.outcome() && wide.reproduces(&again)
}

/// What one run produced, and what it cost.
struct Run {
    outcome: Outcome,
    digest: digest::Digest,
    used: u32,
    capacity: u32,
}

/// Compile and run `source` under an explicit heap size, slice size, and
/// instruction budget, optionally cancelling it before it starts.
fn run(
    storage: &mut Storage,
    source: &[u8],
    arena_bytes: usize,
    slice: u64,
    fuel: u64,
    cancel: bool,
) -> Option<Run> {
    let length = {
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
            .map(|compiled| compiled.length)?
    };
    let bytes = storage.image.get(..length)?;
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
        fuel,
    );
    machine.attach_regexp(
        &mut storage.choices,
        &mut storage.undo,
        &mut storage.subject,
    );
    machine.start().ok()?;
    if cancel {
        machine.control().cancel();
    }
    let mut slices = 0u32;
    let completion = loop {
        match machine.resume(slice) {
            Progress::Running => {
                slices += 1;
                if slices > 100_000 {
                    return None;
                }
            }
            Progress::Finished(completion) => break completion,
        }
    };

    let outcome = completion.outcome();
    let mut units = [0u16; 64];
    let mut written = 0usize;
    if let Completion::Value(value) | Completion::Throw(value) = completion {
        if let Ok(text) = machine.display(value) {
            let length = string::length(machine.heap(), text).unwrap_or(0) as usize;
            if length <= units.len() {
                written =
                    string::copy_units(machine.heap(), text, &mut units[..length]).unwrap_or(0);
            }
        }
    }
    Some(Run {
        outcome,
        digest: digest_outcome(outcome, units.get(..written).unwrap_or(&[])),
        used: machine.heap().used(),
        capacity: u32::try_from(arena_bytes).unwrap_or(0),
    })
}

/// Check every invariant for one program.
fn holds_for(storage: &mut Storage, source: &[u8]) -> bool {
    let Some(base) = run(storage, source, ARENA_BYTES, u64::MAX, STEPS, false) else {
        return false;
    };
    // The outcome is one the policy names, and the heap stayed inside its
    // arena.
    if !matches!(
        base.outcome,
        Outcome::Returned | Outcome::Threw | Outcome::HeapExhausted
    ) || base.used > base.capacity
    {
        return false;
    }

    // A smaller heap and shorter slices do not change a result.
    if let Some(narrow) = run(storage, source, ARENA_BYTES / 4, 3, STEPS, false) {
        if narrow.outcome == Outcome::Returned
            && base.outcome == Outcome::Returned
            && narrow.digest != base.digest
        {
            return false;
        }
        if narrow.used > narrow.capacity {
            return false;
        }
    }

    // A run cancelled before it starts stops as cancelled.
    let Some(cancelled) = run(storage, source, ARENA_BYTES, 1, STEPS, true) else {
        return false;
    };
    if cancelled.outcome != Outcome::Cancelled {
        return false;
    }

    // A tiny instruction budget ends the run rather than overrunning it.
    let Some(starved) = run(storage, source, ARENA_BYTES, u64::MAX, 3, false) else {
        return false;
    };
    matches!(starved.outcome, Outcome::FuelExhausted | Outcome::Returned)
}

#[allow(
    clippy::match_same_arms,
    reason = "each case is an independent assertion and merging arms would hide which one failed"
)]
fn run_case(storage: &mut Storage, case: u16) -> bool {
    let first = case as usize * PER_CASE;
    let mut index = 0usize;
    while index < PER_CASE {
        let Some((source, _)) = vectors::vector(first + index) else {
            // The vectors run out on the last case, which is not a failure.
            return true;
        };
        if !holds_for(storage, source) {
            return false;
        }
        index += 1;
    }
    true
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
            b"phasor-invariant-probe: 25 passed\n"
        } else {
            let prefix = b"phasor-invariant-probe: failed at ";
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
