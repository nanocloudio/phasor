//! On-graph Test262 execution oracle.
//!
//! The front-end oracle (`phasor_test262`) says what tokenizes and parses;
//! this one says what *behaves*. The driver stages the Test262 harness once,
//! then streams selected cases; each case is compiled behind the harness and
//! run, and one verdict byte leaves per case, in order, so the driver can name
//! the file behind every failure.
//!
//! A record is a six-byte header followed by the record's bytes:
//!
//! ```text
//! length: u32 little-endian | expectation: u8 | area: u8 | bytes
//! ```
//!
//! Expectations: `0` is a positive case, which must run to completion; `1` is
//! a negative runtime case, which must throw; `2` is the harness prelude,
//! which is stored rather than judged. The verdicts are:
//!
//! ```text
//! P passed          F ran but threw       X was to throw and did not
//! T stopped on a bound (fuel, heap, stack, quota)
//! R refused by the front end (out of the admitted grammar)
//! S skipped (larger than the staging buffers)
//! ```
//!
//! `R` and `S` are scope, not verdicts on behaviour: the front-end lane is
//! where refusals are measured. `F`, `X`, and `T` are the hunt list.

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

use arena::{Arena, Node};
use bytecode::{Constant, ExceptionRegion, ExportRecord, Function, ImportRecord, Unit};
use emit::Patch;
use heap::{Heap, Slot};
use job::{Job, Queue};
use lex::Lexer;
use lower::{
    lower_script, Binding as LexicalBinding, Pending as PendingFunction, Scope,
    Storage as LowerStorage,
};
use parse::Parser;
use regexp::Choice;
use source::{Limits, LineStart, LineTable};
use string::Atoms;
use value::{Handle, Value};
use vm::{Completion, Frame, ModuleInstance, Progress, Termination, Vm};

const POLL_INPUT: u32 = 0x01;
const POLL_OUTPUT: u32 = 0x02;

/// Largest case this module stages; the harness prelude has the same bound.
const CASE_CAPACITY: usize = 24 * 1024;
const PRELUDE_CAPACITY: usize = 8 * 1024;
/// A compiled source is the prelude, a separating newline, and the case.
const SOURCE_CAPACITY: usize = PRELUDE_CAPACITY + 1 + CASE_CAPACITY;
const HEADER: usize = 6;

/// Instructions one case may run, and lexer fuel for one compile.
const STEPS: u64 = 5_000_000;
const FUEL: u32 = 40_000_000;
/// Jobs drained per round after the body finishes, and the rounds admitted.
const JOB_SLICE: u32 = 64;
const JOB_ROUNDS: u32 = 4096;

/// Front-end storage, sized for a case behind the whole harness.
const LINE_CAPACITY: usize = 2048;
const NODE_CAPACITY: usize = 8192;
const LIST_CAPACITY: usize = 8192;
const NUMBER_CAPACITY: usize = 1024;
const SCRATCH_CAPACITY: usize = 2048;
const CODE_CAPACITY: usize = 16 * 1024;
const IMAGE_CAPACITY: usize = 96 * 1024;
const CONSTANT_CAPACITY: usize = 768;
const DATA_CAPACITY: usize = 24 * 1024;
const POINT_CAPACITY: usize = 2048;
const PATCH_CAPACITY: usize = 768;
const LABEL_CAPACITY: usize = 768;
const VERIFIER_CAPACITY: usize = 32 * 1024;
const UNIT_CODE_CAPACITY: usize = 32 * 1024;
const UNIT_POINT_CAPACITY: usize = 2048;
const FUNCTION_CAPACITY: usize = 384;
const EXCEPTION_CAPACITY: usize = 384;
const SCOPE_CAPACITY: usize = 768;
const LEXICAL_CAPACITY: usize = 2048;
const PENDING_CAPACITY: usize = 384;
const IMPORT_CAPACITY: usize = 8;
const EXPORT_CAPACITY: usize = 8;

/// Machine storage, larger than the isolate's: a conformance case is allowed
/// to be greedier than a deployed program.
const ARENA_BYTES: usize = 192 * 1024;
const SLOT_COUNT: usize = 6144;
const WORKLIST: usize = 2048;
const ATOM_ENTRIES: usize = 1024;
const ATOM_HANDLES: usize = 768;
const FRAME_COUNT: usize = 128;
const REGISTER_COUNT: usize = 3072;
const ROOT_COUNT: usize = 6144;
const JOB_COUNT: usize = 64;
const CHOICE_COUNT: usize = 256;
const UNDO_COUNT: usize = 256;
const SUBJECT_UNITS: usize = 4096;
const COLLECTION_SLICE: u32 = 512;
const COLLECTION_HEADROOM: u32 = (ARENA_BYTES / 4) as u32;

/// Units an `eval` may compile in one case, and the bytes one may occupy.
/// A repeated source reuses its unit, so a loop over one eval costs one slot.
const EVAL_SLOTS: usize = 24;
const EVAL_IMAGE: usize = 8 * 1024;
const EVAL_SOURCE: usize = 8 * 1024;

struct Storage {
    starts: [LineStart; LINE_CAPACITY],
    nodes: [Node; NODE_CAPACITY],
    lists: [u32; LIST_CAPACITY],
    numbers: [f64; NUMBER_CAPACITY],
    scratch: [u32; SCRATCH_CAPACITY],
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
    slots: [Slot; SLOT_COUNT],
    worklist: [u32; WORKLIST],
    entries: [u32; ATOM_ENTRIES],
    handles: [Handle; ATOM_HANDLES],
    frames: [Frame; FRAME_COUNT],
    registers: [Value; REGISTER_COUNT],
    roots: [Handle; ROOT_COUNT],
    jobs: [Job; JOB_COUNT],
    choices: [Choice; CHOICE_COUNT],
    undo: [(u8, u32); UNDO_COUNT],
    subject: [u16; SUBJECT_UNITS],
    eval_images: [[u8; EVAL_IMAGE]; EVAL_SLOTS],
    eval_lengths: [usize; EVAL_SLOTS],
    eval_digests: [u64; EVAL_SLOTS],
    eval_source: [u8; EVAL_SOURCE],
    instances: [ModuleInstance; EVAL_SLOTS + 1],
}

/// How a case ended, before the expectation is applied.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Ran {
    Finished,
    Threw,
    Stopped,
    Refused,
}

/// A cheap content digest, to reuse the unit an identical source compiled.
fn digest_of(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash ^ (bytes.len() as u64)
}

/// Compile one source into `image`, answering the image length.
fn compile_into(storage: &mut FrontEnd<'_>, source: &[u8], image: &mut [u8]) -> Option<usize> {
    let table = LineTable::new(storage.starts);
    let lexer = Lexer::new(source, Limits::CEILING, table, FUEL).ok()?;
    let syntax = Arena::new(storage.nodes, storage.lists, storage.numbers);
    let mut parser = Parser::new(lexer, syntax, storage.scratch, Limits::CEILING);
    let root = parser.parse_unit().ok()?;
    let mut lowering = LowerStorage {
        code: storage.code,
        image,
        constants: storage.constants,
        constant_data: storage.constant_data,
        safe_points: storage.safe_points,
        patches: storage.patches,
        labels: storage.labels,
        verifier_state: storage.verifier_state,
        unit_code: storage.unit_code,
        unit_safe_points: storage.unit_safe_points,
        functions: storage.functions,
        exceptions: storage.exceptions,
        scopes: storage.scopes,
        bindings: storage.lexical,
        pending: storage.pending,
        imports: storage.imports,
        exports: storage.exports,
    };
    let compiled = lower_script(source, parser.arena(), root, &mut lowering).ok()?;
    Some(compiled.length)
}

/// The front end's storage, borrowed apart from the machine's.
struct FrontEnd<'a> {
    starts: &'a mut [LineStart; LINE_CAPACITY],
    nodes: &'a mut [Node; NODE_CAPACITY],
    lists: &'a mut [u32; LIST_CAPACITY],
    numbers: &'a mut [f64; NUMBER_CAPACITY],
    scratch: &'a mut [u32; SCRATCH_CAPACITY],
    code: &'a mut [u8; CODE_CAPACITY],
    constants: &'a mut [Constant; CONSTANT_CAPACITY],
    constant_data: &'a mut [u8; DATA_CAPACITY],
    safe_points: &'a mut [u32; POINT_CAPACITY],
    patches: &'a mut [Patch; PATCH_CAPACITY],
    labels: &'a mut [u32; LABEL_CAPACITY],
    verifier_state: &'a mut [i32; VERIFIER_CAPACITY],
    unit_code: &'a mut [u8; UNIT_CODE_CAPACITY],
    unit_safe_points: &'a mut [u32; UNIT_POINT_CAPACITY],
    functions: &'a mut [Function; FUNCTION_CAPACITY],
    exceptions: &'a mut [ExceptionRegion; EXCEPTION_CAPACITY],
    scopes: &'a mut [Scope; SCOPE_CAPACITY],
    lexical: &'a mut [LexicalBinding; LEXICAL_CAPACITY],
    pending: &'a mut [PendingFunction; PENDING_CAPACITY],
    imports: &'a mut [ImportRecord; IMPORT_CAPACITY],
    exports: &'a mut [ExportRecord; EXPORT_CAPACITY],
}

/// Compile and run one assembled source.
fn execute(storage: &mut Storage, source: &[u8]) -> Ran {
    let length = {
        let table = LineTable::new(&mut storage.starts);
        let Ok(lexer) = Lexer::new(source, Limits::CEILING, table, FUEL) else {
            return Ran::Refused;
        };
        let syntax = Arena::new(&mut storage.nodes, &mut storage.lists, &mut storage.numbers);
        let mut parser = Parser::new(lexer, syntax, &mut storage.scratch, Limits::CEILING);
        let Ok(root) = parser.parse_unit() else {
            return Ran::Refused;
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
        match lower_script(source, parser.arena(), root, &mut lowering) {
            Ok(compiled) => compiled.length,
            Err(_) => return Ran::Refused,
        }
    };
    if storage.image.get(..length).is_none() {
        return Ran::Refused;
    }

    // The machine pauses when the program calls `eval`: the source is
    // compiled into a unit slot here, the machine is rebuilt over the same
    // storage with the unit attached, and the eval is entered as a frame. A
    // repeated source reuses its slot, so a loop over one eval is bounded.
    let mut eval_count = 0usize;
    let mut saves: Option<vm::Saves> = None;
    let mut kept_realm: Option<realm::Realm> = None;
    let mut enter: Option<u32> = None;
    let mut fail_pending = false;

    loop {
        // What to do next, decided while the machine exists and acted on
        // after its borrows end.
        let mut compile_request: Option<usize> = None;
        {
            let Ok(unit) = Unit::parse(storage.image.get(..length).unwrap_or(&[])) else {
                return Ran::Refused;
            };
            let mut units = [Unit::EMPTY; EVAL_SLOTS + 1];
            units[0] = unit;
            let mut slot = 0usize;
            while slot < eval_count {
                let held = storage.eval_lengths[slot];
                let image = storage.eval_images[slot].get(..held).unwrap_or(&[]);
                let Ok(parsed) = Unit::parse(image) else {
                    return Ran::Stopped;
                };
                units[slot + 1] = parsed;
                slot += 1;
            }
            let mut index = 0usize;
            while index <= eval_count {
                storage.instances[index] = ModuleInstance {
                    environment: Value::UNDEFINED,
                    import_base: 0,
                    namespace: Value::UNDEFINED,
                };
                index += 1;
            }

            let mut heap = match &saves {
                Some(kept) => Heap::adopt(
                    &mut storage.arena,
                    &mut storage.slots,
                    &mut storage.worklist,
                    &kept.heap,
                ),
                None => Heap::with_worklist(
                    &mut storage.arena,
                    &mut storage.slots,
                    &mut storage.worklist,
                ),
            };
            let mut atoms = match &saves {
                Some(kept) => Atoms::adopt(&mut storage.entries, &mut storage.handles, &kept.atoms),
                None => Atoms::new(&mut storage.entries, &mut storage.handles),
            };
            let realm = match kept_realm {
                Some(realm) => realm,
                None => match realm::create(&mut heap, &mut atoms) {
                    Ok(realm) => realm,
                    Err(_) => return Ran::Stopped,
                },
            };
            let mut queue = Queue::new(&mut storage.jobs);
            let mut machine = Vm::new(
                &units[0],
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
            machine.attach_jobs(&mut queue);
            machine.attach_collector(&mut storage.roots, COLLECTION_SLICE, COLLECTION_HEADROOM);
            machine.attach_modules(
                &units[..eval_count + 1],
                storage
                    .instances
                    .get_mut(..eval_count + 1)
                    .unwrap_or(&mut []),
                &[],
            );
            match &saves {
                Some(kept) => machine.restore_all(kept),
                None => {
                    if machine.start().is_err() {
                        return Ran::Stopped;
                    }
                }
            }
            if fail_pending {
                fail_pending = false;
                match machine.fail_eval() {
                    Some(Completion::Throw(_)) => return Ran::Threw,
                    Some(_) => return Ran::Stopped,
                    None => {}
                }
            }
            if let Some(unit) = enter.take() {
                if machine.enter_eval(unit).is_err() {
                    return Ran::Stopped;
                }
            }

            // One resume with the whole budget runs to the next pause or the
            // end; every arm decides the case except a pause, which falls
            // through to the compile step below with the machine's borrows
            // ended.
            match machine.resume(u64::MAX) {
                Progress::Running => {
                    let Some(handle) = machine.pending_eval() else {
                        return Ran::Stopped;
                    };
                    // Stage the source as UTF-8 for the compiler.
                    let count = string::length(machine.heap(), handle).unwrap_or(0) as usize;
                    let mut units16 = [0u16; 2048];
                    if count > units16.len() {
                        fail_pending = true;
                    } else {
                        let copied = string::copy_units(
                            machine.heap(),
                            handle,
                            units16.get_mut(..count).unwrap_or(&mut []),
                        )
                        .unwrap_or(0);
                        let mut at = 0usize;
                        let mut index = 0usize;
                        let mut fits = true;
                        while index < copied {
                            let unit = units16[index];
                            let needed = if unit < 0x80 {
                                1
                            } else if unit < 0x800 {
                                2
                            } else {
                                3
                            };
                            if at + needed > EVAL_SOURCE {
                                fits = false;
                                break;
                            }
                            if unit < 0x80 {
                                storage.eval_source[at] = unit as u8;
                            } else if unit < 0x800 {
                                storage.eval_source[at] = 0xC0 | (unit >> 6) as u8;
                                storage.eval_source[at + 1] = 0x80 | (unit & 0x3F) as u8;
                            } else {
                                storage.eval_source[at] = 0xE0 | (unit >> 12) as u8;
                                storage.eval_source[at + 1] = 0x80 | ((unit >> 6) & 0x3F) as u8;
                                storage.eval_source[at + 2] = 0x80 | (unit & 0x3F) as u8;
                            }
                            at += needed;
                            index += 1;
                        }
                        if fits {
                            compile_request = Some(at);
                        } else {
                            fail_pending = true;
                        }
                    }
                    saves = Some(machine.save());
                    kept_realm = Some(realm);
                }
                Progress::Finished(Completion::Value(_)) => {
                    // The body finished; whatever it queued still counts.
                    let mut round = 0u32;
                    while machine.pending_jobs() > 0 && round < JOB_ROUNDS {
                        match machine.run_jobs(JOB_SLICE) {
                            Ok(_) => {}
                            Err(Completion::Throw(_)) => return Ran::Threw,
                            Err(_) => return Ran::Stopped,
                        }
                        round += 1;
                    }
                    return Ran::Finished;
                }
                Progress::Finished(Completion::Throw(_)) => return Ran::Threw,
                Progress::Finished(Completion::Terminated(_)) => return Ran::Stopped,
            }
        }

        // The machine's borrows have ended; compile the staged source.
        if let Some(source_length) = compile_request {
            let source = storage.eval_source.get(..source_length).unwrap_or(&[]);
            let digest = digest_of(source);
            let mut found = None;
            let mut slot = 0usize;
            while slot < eval_count {
                if storage.eval_digests[slot] == digest {
                    found = Some(slot);
                    break;
                }
                slot += 1;
            }
            match found {
                Some(slot) => enter = Some(u32::try_from(slot + 1).unwrap_or(0)),
                None if eval_count >= EVAL_SLOTS => fail_pending = true,
                None => {
                    let mut front = FrontEnd {
                        starts: &mut storage.starts,
                        nodes: &mut storage.nodes,
                        lists: &mut storage.lists,
                        numbers: &mut storage.numbers,
                        scratch: &mut storage.scratch,
                        code: &mut storage.code,
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
                        lexical: &mut storage.lexical,
                        pending: &mut storage.pending,
                        imports: &mut storage.imports,
                        exports: &mut storage.exports,
                    };
                    let source = storage.eval_source.get(..source_length).unwrap_or(&[]);
                    match compile_into(&mut front, source, &mut storage.eval_images[eval_count]) {
                        Some(compiled) => {
                            storage.eval_lengths[eval_count] = compiled;
                            storage.eval_digests[eval_count] = digest;
                            eval_count += 1;
                            enter = Some(u32::try_from(eval_count).unwrap_or(0));
                        }
                        None => fail_pending = true,
                    }
                }
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
    prelude: [u8; PRELUDE_CAPACITY],
    source: [u8; SOURCE_CAPACITY],
    storage: Storage,
    filled: usize,
    prelude_length: usize,
    /// Bytes of an oversized record still to be discarded.
    discarding: usize,
    /// The discarded record's verdict, staged once the bytes are gone.
    discard_verdict: u8,
    passed: u32,
    failed: u32,
    refused: u32,
    stopped: u32,
    skipped: u32,
    cases: u32,
    verdicts: [u8; 256],
    verdict_length: usize,
    verdict_offset: usize,
    report: [u8; 160],
    report_length: usize,
    report_offset: usize,
    phase: u8,
}

/// Judge one staged case and answer its verdict byte.
fn judge(state: &mut State, length: usize, expectation: u8) -> u8 {
    let prelude = state.prelude_length;
    let total = prelude + 1 + length;
    if total > SOURCE_CAPACITY {
        return b'S';
    }
    state.source[..prelude].copy_from_slice(&state.prelude[..prelude]);
    state.source[prelude] = b'\n';
    state.source[prelude + 1..total].copy_from_slice(&state.buffer[HEADER..HEADER + length]);
    let source = state.source.get(..total).unwrap_or(&[]);
    let ran = execute(&mut state.storage, source);
    match (ran, expectation == 1) {
        (Ran::Refused, _) => b'R',
        (Ran::Stopped, _) => b'T',
        (Ran::Finished, false) | (Ran::Threw, true) => b'P',
        (Ran::Threw, false) => b'F',
        (Ran::Finished, true) => b'X',
    }
}

fn record_verdict(state: &mut State, verdict: u8) {
    match verdict {
        b'P' => state.passed += 1,
        b'F' | b'X' => state.failed += 1,
        b'R' => state.refused += 1,
        b'T' => state.stopped += 1,
        _ => state.skipped += 1,
    }
    state.cases += 1;
    if state.verdict_length < state.verdicts.len() {
        state.verdicts[state.verdict_length] = verdict;
        state.verdict_length += 1;
    }
}

/// Consume one complete record from the staging buffer, if one is there.
///
/// One record per step: a case executes under a real instruction budget, and
/// a step that ran a whole batch would be a step the scheduler cannot bound.
fn drain_one(state: &mut State) {
    if state.discarding > 0 {
        let drop = state.discarding.min(state.filled);
        if drop == 0 {
            return;
        }
        state.buffer.copy_within(drop..state.filled, 0);
        state.filled -= drop;
        state.discarding -= drop;
        if state.discarding == 0 {
            let verdict = state.discard_verdict;
            record_verdict(state, verdict);
        }
        return;
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

    if length > CASE_CAPACITY {
        state.buffer.copy_within(HEADER..state.filled, 0);
        state.filled -= HEADER;
        state.discarding = length;
        state.discard_verdict = b'S';
        return;
    }
    if state.filled < HEADER + length {
        return;
    }

    if expectation == 2 {
        let kept = length.min(PRELUDE_CAPACITY);
        state.prelude[..kept].copy_from_slice(&state.buffer[HEADER..HEADER + kept]);
        state.prelude_length = kept;
    } else {
        let verdict = judge(state, length, expectation);
        record_verdict(state, verdict);
    }
    state.buffer.copy_within(HEADER + length..state.filled, 0);
    state.filled -= HEADER + length;
}

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

fn compose(state: &mut State) -> usize {
    let mut out = [0u8; 160];
    let mut at = 0usize;
    out[at] = b'\n';
    at += 1;
    let prefix = b"phasor-run262: ";
    out[at..at + prefix.len()].copy_from_slice(prefix);
    at += prefix.len();
    for (value, label) in [
        (state.passed, &b" passed "[..]),
        (state.failed, &b" failed "[..]),
        (state.stopped, &b" stopped "[..]),
        (state.refused, &b" refused "[..]),
        (state.skipped, &b" skipped of "[..]),
    ] {
        at += write_u32(value, &mut out[at..]);
        out[at..at + label.len()].copy_from_slice(label);
        at += label.len();
    }
    at += write_u32(state.cases, &mut out[at..]);
    let tail = b" cases\n";
    out[at..at + tail.len()].copy_from_slice(tail);
    at += tail.len();
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

    // Verdicts leave before anything else happens, so the stream stays in
    // case order and the staging buffer stays small.
    if state.verdict_length > state.verdict_offset {
        let offset = state.verdict_offset;
        let remaining = state.verdict_length - offset;
        let written = unsafe {
            (syscalls.channel_write)(
                state.report_out,
                state.verdicts[offset..].as_ptr(),
                remaining,
            )
        };
        if written > 0 {
            state.verdict_offset += usize::try_from(written).unwrap_or(0).min(remaining);
        }
        if state.verdict_length > state.verdict_offset {
            return 0;
        }
        state.verdict_length = 0;
        state.verdict_offset = 0;
    }

    if state.phase == 1 {
        if state.report_length == 0 {
            state.report_length = compose(state);
            state.report_offset = 0;
        }
        let offset = state.report_offset;
        let remaining = state.report_length - offset;
        if remaining > 0 {
            let written = unsafe {
                (syscalls.channel_write)(
                    state.report_out,
                    state.report[offset..].as_ptr(),
                    remaining,
                )
            };
            if written <= 0 {
                return 0;
            }
            state.report_offset += usize::try_from(written).unwrap_or(0).min(remaining);
            if state.report_length > state.report_offset {
                return 0;
            }
        }
        state.phase = 2;
        return 0;
    }

    if state.phase == 2 {
        if state.exit_out >= 0 {
            let code = i32::from(state.failed != 0).to_le_bytes();
            let written =
                unsafe { (syscalls.channel_write)(state.exit_out, code.as_ptr(), code.len()) };
            if written != i32::try_from(code.len()).unwrap_or(i32::MAX) {
                return 0;
            }
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
        if capacity > 0 {
            let read = unsafe {
                (syscalls.channel_read)(
                    state.input,
                    state.buffer.as_mut_ptr().add(offset),
                    capacity,
                )
            };
            if read > 0 {
                state.filled += usize::try_from(read).unwrap_or(0).min(capacity);
            }
        }
        drain_one(state);
        return 0;
    }

    if (poll as u32) & POLL_HUP == 0 {
        return 0;
    }

    // The stream has ended; whole records may still be staged.
    if state.filled >= HEADER || state.discarding > 0 {
        drain_one(state);
        return 0;
    }
    state.phase = 1;
    0
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
