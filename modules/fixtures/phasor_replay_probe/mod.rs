//! On-graph conformance probe for the deterministic replay profile.
//!
//! The same program is run under different heap sizes and slice sizes, which
//! changes when collection happens and how often the task yields. None of that
//! may change what the program produces, and the recordings are how that is
//! checked.

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
#[macro_use]
mod lower;
#[path = "../../common/frontend.rs"]
#[macro_use]
mod frontend;
#[path = "../../common/numeric.rs"]
mod numeric;
#[path = "../../common/object.rs"]
mod object;
#[path = "../../common/parse.rs"]
mod parse;
#[path = "../../common/policy.rs"]
mod policy;
#[path = "../../common/probe.rs"]
mod probe;
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
#[path = "../../common/text.rs"]
mod text;
#[path = "../../common/unicode_id.rs"]
mod unicode_id;
#[path = "../../common/value.rs"]
mod value;
#[path = "../../common/verify.rs"]
mod verify;
#[path = "../../common/vm.rs"]
mod vm;
#[path = "../../common/wire.rs"]
mod wire;

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
use policy::Policy;
use regexp::Choice;
use replay::{digest_outcome, Event, Profile, Recording};
use source::{Limits, LineStart, LineTable};
use string::Atoms;
use value::{Handle, Value};
use vm::{Completion, Frame, Progress, Vm};

const CASE_COUNT: u16 = 15;
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
const ARENA_BYTES: usize = 192 * 1024;
/// A heap with room for the realm and the program, and not much more: a run in
/// it must reach the same answer as one with room to spare.
const NARROW_ARENA: usize = 128 * 1024;
const SLOT_COUNT: usize = 3072;
const WORKLIST: usize = 512;
const ATOM_ENTRIES: usize = 2048;
const ATOM_HANDLES: usize = 1536;
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
        let mut front = frontend_storage!(storage);
        let goal = frontend::Goal::Script;
        frontend::compile(source, goal, Limits::CEILING, FUEL, &mut front)
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
    let Some(narrow) = record_run(storage, source, NARROW_ARENA, 3) else {
        return false;
    };
    let Some(again) = record_run(storage, source, ARENA_BYTES, u64::MAX) else {
        return false;
    };
    wide.outcome() == narrow.outcome() && wide.reproduces(&again)
}

#[allow(
    clippy::match_same_arms,
    reason = "each case is an independent assertion and merging arms would hide which one failed"
)]
fn run_case(storage: &mut Storage, case: u16) -> bool {
    match case {
        // The same program produces the same result however the heap is sized
        // and however often the task yields.
        0 => agrees(storage, b"1 + 2 * 3"),
        1 => agrees(storage, b"'a' + [1, 2]"),
        2 => agrees(storage, b"`t${1 / 3}`"),
        3 => agrees(storage, b"[1, 2, 3].length + 'x'"),
        4 => agrees(storage, b"({a: 1, b: 'two'}).b"),
        5 => agrees(storage, b"0.1 + 0.2"),
        6 => agrees(storage, b"null.x"),

        // Different programs are different runs.
        7 => {
            let Some(first) = record_run(storage, b"1 + 1", ARENA_BYTES, u64::MAX) else {
                return false;
            };
            let Some(second) = record_run(storage, b"1 + 2", ARENA_BYTES, u64::MAX) else {
                return false;
            };
            !first.reproduces(&second) && first.digest() != second.digest()
        }

        // A recording's identity covers its events.
        8 => {
            let mut first =
                Recording::new(Profile::Observed, digest::digest(b"image"), &Policy::MODEST);
            let mut second = first;
            first.record(Event::Time(1));
            second.record(Event::Time(2));
            first.finish(digest::digest(b"out"));
            second.finish(digest::digest(b"out"));
            !first.reproduces(&second)
        }

        // A recording that lost events reproduces nothing.
        9 => {
            let mut recording =
                Recording::new(Profile::Observed, digest::digest(b"image"), &Policy::MODEST);
            let mut index = 0u64;
            while index < replay::MAX_EVENTS as u64 + 4 {
                recording.record(Event::Time(index));
                index += 1;
            }
            recording.finish(digest::digest(b"out"));
            let copy = recording;
            !recording.complete() && !recording.reproduces(&copy)
        }

        // A policy is part of a run's identity.
        10 => {
            let modest = replay::digest_policy(&Policy::MODEST);
            let ceiling = replay::digest_policy(&Policy::CEILING);
            modest != ceiling && modest == replay::digest_policy(&Policy::MODEST)
        }

        // An unfinished recording reproduces nothing.
        11 => {
            let first = Recording::new(
                Profile::Deterministic,
                digest::digest(b"image"),
                &Policy::MODEST,
            );
            let mut second = first;
            second.finish(digest::digest(b"out"));
            !first.reproduces(&second) && !second.reproduces(&first)
        }
        // A completion is an event like any other: two runs that were
        // answered differently are two different runs, and a recording that
        // did not cover the answer could not say so.
        12 => {
            let mut first =
                Recording::new(Profile::Observed, digest::digest(b"image"), &Policy::MODEST);
            let mut second = first;
            first.record(Event::Completion {
                request: 1,
                payload: digest::digest(b"yes"),
            });
            second.record(Event::Completion {
                request: 1,
                payload: digest::digest(b"no"),
            });
            first.finish(digest::digest(b"out"));
            second.finish(digest::digest(b"out"));
            !first.reproduces(&second)
        }

        // And the request it answered is part of it: the same bytes to a
        // different call is not the same run.
        13 => {
            let mut first =
                Recording::new(Profile::Observed, digest::digest(b"image"), &Policy::MODEST);
            let mut second = first;
            let answer = digest::digest(b"same");
            first.record(Event::Completion {
                request: 1,
                payload: answer,
            });
            second.record(Event::Completion {
                request: 2,
                payload: answer,
            });
            first.finish(digest::digest(b"out"));
            second.finish(digest::digest(b"out"));
            let mut third =
                Recording::new(Profile::Observed, digest::digest(b"image"), &Policy::MODEST);
            third.record(Event::Completion {
                request: 1,
                payload: answer,
            });
            third.finish(digest::digest(b"out"));
            !first.reproduces(&second) && first.reproduces(&third)
        }

        // A completion and a time are not the same event, even where the
        // numbers in them line up.
        14 => {
            let mut first =
                Recording::new(Profile::Observed, digest::digest(b"image"), &Policy::MODEST);
            let mut second = first;
            first.record(Event::Completion {
                request: 0,
                payload: digest::digest(b""),
            });
            second.record(Event::Time(0));
            first.finish(digest::digest(b"out"));
            second.finish(digest::digest(b"out"));
            !first.reproduces(&second)
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
    probe::step(
        &mut state.progress,
        syscalls,
        state.report_out,
        state.exit_out,
        b"phasor-replay-probe",
        CASE_COUNT,
        |case| run_case(&mut state.storage, case),
    )
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
