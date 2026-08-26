//! The isolate: an image runs here, and its calls leave on a port.
//!
//! One input stream ending in a hang-up is one unit image, which may have been
//! compiled anywhere. The module admits it, which verifies it before anything
//! runs, and executes it under a policy. A program that calls a granted binding
//! gets a promise and a call record leaves on the call port; the answer arrives
//! on the completion port and settles that promise, so a task that is waiting
//! on the outside survives between module steps.
//!
//! The machine itself does not: it borrows the storage that lives in this
//! module's state. Each step rebuilds it over that same storage and restores
//! its saved state, which is what lets a bounded step yield without losing a
//! task in the middle of one.

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
include!("../../../target/fluxor/fluxor-abi/sdk/runtime/params.rs");

#[path = "../../common/arena.rs"]
mod arena;
#[path = "../../common/bigint.rs"]
mod bigint;
#[path = "../../common/binding.rs"]
mod binding;
#[path = "../../common/bytecode.rs"]
mod bytecode;
#[path = "../../common/closure.rs"]
mod closure;
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

use binding::{
    Binding, Bindings, BindingsSave, CallRecord, Cause, CompletionRecord, Disposition, Pending,
    CALL_FRAME, COMPLETION_FRAME,
};
use bytecode::Unit;
use closure::Closure;
use diagnostic::{Diagnostic, Severity};
use heap::{Heap, HeapSave, Slot};
use job::{Job, Queue, QueueSave};
use realm::Realm;
use regexp::Choice;
use string::{Atoms, AtomsSave};
use value::{Handle, Value};
use vm::{Completion, Frame, ModuleInstance, Progress, Saves, Snapshot, Vm};

const POLL_INPUT: u32 = 0x01;
const POLL_OUTPUT: u32 = 0x02;

const IMAGE_CAPACITY: usize = 64 * 1024;
const RESULT_CAPACITY: usize = 128;
const ARENA_BYTES: usize = 64 * 1024;
const SLOT_COUNT: usize = 1536;
const WORKLIST: usize = 512;
const ATOM_ENTRIES: usize = 512;
const ATOM_HANDLES: usize = 384;
/// How deep a program may call, and how many registers those calls may hold.
/// A call is a frame here rather than a host stack frame, so this number is
/// what a program's recursion is bounded by: deep enough for ordinary nesting,
/// small enough that the whole machine still fits in a module's state.
const FRAME_COUNT: usize = 256;
const REGISTER_COUNT: usize = 4096;
const JOB_COUNT: usize = 32;
/// Bindings this isolate admits, and calls it lets be outstanding at once.
const BINDING_COUNT: usize = 1;
const PENDING_COUNT: usize = 4;
const IN_FLIGHT_MAX: u32 = 4;
/// Roots a collection stages before it starts.
const ROOT_COUNT: usize = 2048;
/// Cells or bytes one collection slice works through.
const COLLECTION_SLICE: u32 = 512;
/// Free arena below which the isolate collects rather than waiting to fail.
const COLLECTION_HEADROOM: u32 = (ARENA_BYTES / 4) as u32;
const VERIFIER_CAPACITY: usize = 16 * 1024;
/// Instructions one image may run here when the graph names no other number.
/// A budget is what makes a run answerable rather than open-ended, so it is a
/// parameter: a graph that runs bigger programs says so.
const STEPS: u32 = 20_000_000;
/// Instructions one module step may run, so a long program yields.
const SLICE: u64 = 4096;
/// Jobs one module step may run, so a flood of reactions still yields.
const JOB_SLICE: u32 = 16;
/// Steps a call may wait for an answer before the isolate times it out. A
/// provider that never answers must not hold a task open forever.
const WAIT_LIMIT: u32 = 5_000;
/// Modules one closure may hold, and imports it may have in total.
const MAX_MODULES: usize = 16;
const MAX_IMPORTS: usize = 128;
/// The trace every call this isolate makes belongs to.
const TRACE: u64 = 0x5041_5348_4f52_0001;

/// What one step of the machine did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Advance {
    /// The task is still running or still waiting on an answer.
    Running,
    /// The result is rendered in `state.result`.
    Done,
    /// The task's result rejected. The reason is rendered in `state.result`,
    /// and the run reports failure.
    Rejected,
    /// The image could not run, or the task terminated.
    Failed,
}

/// The port side: frames staged to leave, and the frame arriving.
/// Where a match backtracks, what it must put back, and the units it runs over.
const CHOICE_COUNT: usize = 256;
const UNDO_COUNT: usize = 256;
const SUBJECT_UNITS: usize = 1024;
#[repr(C)]
struct Wire {
    calls: [u8; CALL_FRAME * PENDING_COUNT],
    staged: usize,
    written: usize,
    completion: [u8; COMPLETION_FRAME],
    filled: usize,
    ready: bool,
    /// Steps this isolate has waited with a call outstanding and nothing to do.
    waited: u32,
}

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    image_in: i32,
    completion_in: i32,
    result_out: i32,
    call_out: i32,
    diagnostic_out: i32,
    exit_out: i32,
    steps: u32,
    image: [u8; IMAGE_CAPACITY],
    result: [u8; RESULT_CAPACITY],
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
    roots: [Handle; ROOT_COUNT],
    jobs: [Job; JOB_COUNT],
    descriptors: [Binding; BINDING_COUNT],
    pending: [Pending; PENDING_COUNT],
    outbox: [CallRecord; PENDING_COUNT],
    verifier_state: [i32; VERIFIER_CAPACITY],
    wire: Wire,
    realm: Realm,
    saves: Saves,
    /// The modules of a linked closure, when the image was one.
    instances: [ModuleInstance; MAX_MODULES],
    import_table: [(u32, u32); MAX_IMPORTS],
    module_count: u32,
    import_count: u32,
    /// Which module is running, and whether it has been started.
    current_module: u32,
    module_started: bool,
    /// Whether the staged image is a closure rather than one script.
    linked: bool,
    result_value: Value,
    image_length: usize,
    result_length: usize,
    result_written: usize,
    /// Whether the program body has run to its end. Its calls may not have been
    /// answered yet, which is the point of the wait that follows.
    body_done: bool,
    overflowed: bool,
    /// The image could not be admitted, or the task did not produce a value.
    /// That is this module failing, and it says so.
    failed: bool,
    /// The task produced a rejected result. The program ran; its answer is a
    /// refusal, which is an outcome rather than a fault.
    rejected: bool,
    /// Why a run produced no value, ready to hand to whatever renders it.
    diagnostic: [u8; diagnostic::FRAME],
    has_diagnostic: bool,
    diagnostic_written: usize,
    phase: u8,
}

/// Admit the staged image and start the task, saving the machine's state.
fn start(state: &mut State) -> bool {
    if state.overflowed {
        return false;
    }
    let image_length = state.image_length;
    let Some(bytes) = state.image.get(..image_length) else {
        return false;
    };
    // The image is staged once and never written again while it runs.
    let bytes: &[u8] = unsafe { core::slice::from_raw_parts(bytes.as_ptr(), bytes.len()) };
    // What arrived is either one script or a linked closure of modules.
    let closure = Closure::parse(bytes).ok();
    state.linked = closure.is_some();
    let mut units = [Unit::EMPTY; MAX_MODULES];
    let count = match &closure {
        Some(closure) => {
            let count = closure.count() as usize;
            if count > MAX_MODULES {
                return false;
            }
            let mut index = 0usize;
            while index < count {
                let Some(image) = closure.image(index as u32) else {
                    return false;
                };
                let Ok(unit) = verify::admit(image, &mut state.verifier_state) else {
                    return false;
                };
                units[index] = unit;
                index += 1;
            }
            count
        }
        None => {
            match verify::admit(bytes, &mut state.verifier_state) {
                Ok(unit) => units[0] = unit,
                Err(report) => {
                    state.diagnostic = report.encode();
                    state.has_diagnostic = true;
                    return false;
                }
            }
            1
        }
    };
    state.module_count = u32::try_from(count).unwrap_or(0);

    // Every import is resolved before anything runs: the specifier names a
    // module of the closure, and the name must be one that module exports.
    state.import_count = 0;
    if let Some(closure) = &closure {
        let mut index = 0usize;
        while index < count {
            state.instances[index] = ModuleInstance {
                environment: Value::UNDEFINED,
                import_base: state.import_count,
                namespace: Value::UNDEFINED,
            };
            let imports = units[index].header().import_count;
            let mut import = 0u32;
            while import < imports {
                let mut specifier = [0u16; 64];
                let mut name = [0u16; 64];
                let Some((specifier_length, name_length, _)) =
                    units[index].import_at(import, &mut specifier, &mut name)
                else {
                    return false;
                };
                // The specifier is compared as bytes, which is what the
                // container holds it as.
                let mut text = [0u8; 64];
                let mut at = 0usize;
                while at < specifier_length && at < text.len() {
                    text[at] = u8::try_from(specifier[at]).unwrap_or(b'?');
                    at += 1;
                }
                let Some(source) = closure.index_of(text.get(..at).unwrap_or(&[])) else {
                    return false;
                };
                let slot = if name_length == 0 {
                    u32::MAX
                } else {
                    match units[source as usize].export_slot(name.get(..name_length).unwrap_or(&[]))
                    {
                        Some(slot) => slot,
                        None => return false,
                    }
                };
                let Some(entry) = state.import_table.get_mut(state.import_count as usize) else {
                    return false;
                };
                *entry = (source, slot);
                state.import_count += 1;
                import += 1;
            }
            index += 1;
        }
    } else {
        state.instances[0] = ModuleInstance::EMPTY;
    }
    let unit = units[0];

    let steps = state.steps;
    let mut heap = Heap::with_worklist(&mut state.arena, &mut state.slots, &mut state.worklist);
    let mut atoms = Atoms::new(&mut state.entries, &mut state.handles);
    let Ok(realm) = realm::create(&mut heap, &mut atoms) else {
        return false;
    };
    let mut queue = Queue::new(&mut state.jobs);
    let mut bindings = Bindings::new(&mut state.descriptors, &mut state.pending);
    // One binding is admitted here, by the digest of its name and schema. What
    // answers it, and where that is, the isolate never learns.
    if bindings
        .admit(Binding {
            name: digest::digest(b"host"),
            schema: digest::digest(b"number->number"),
            in_flight_max: IN_FLIGHT_MAX,
            in_flight: 0,
        })
        .is_err()
    {
        return false;
    }
    let mut machine = Vm::new(
        &unit,
        &mut heap,
        &mut atoms,
        &mut state.frames,
        &mut state.registers,
        realm,
        u64::from(steps),
    );
    machine.attach_regexp(&mut state.choices, &mut state.undo, &mut state.subject);
    machine.attach_jobs(&mut queue);
    machine.attach_bindings(&mut bindings, &mut state.outbox);
    machine.attach_collector(&mut state.roots, COLLECTION_SLICE, COLLECTION_HEADROOM);
    machine.set_trace(TRACE);
    if machine.define_binding(b"host", 0).is_err() {
        return false;
    }
    if state.linked {
        let count = state.module_count;
        let imports = state
            .import_table
            .get(..state.import_count as usize)
            .unwrap_or(&[]);
        let instances = state.instances.get_mut(..count as usize).unwrap_or(&mut []);
        machine.attach_modules(
            units.get(..count as usize).unwrap_or(&[]),
            instances,
            imports,
        );
        // Every module's environment is made before any of them runs, which is
        // what lets one read another's exports once it has.
        let mut index = 0u32;
        while index < count {
            let Ok(environment) = machine.create_module_environment(index) else {
                return false;
            };
            machine.set_module_environment(index, environment);
            index += 1;
        }
        state.current_module = 0;
        if machine.start_module(0).is_err() {
            return false;
        }
        state.module_started = true;
    } else if machine.start().is_err() {
        return false;
    }
    state.saves = machine.save();
    state.realm = realm;
    true
}

/// Run one bounded slice of the task, apply any answer that arrived, and stage
/// any call the program made.
fn advance(state: &mut State) -> Advance {
    let image_length = state.image_length;
    let Some(bytes) = state.image.get(..image_length) else {
        return Advance::Failed;
    };
    let bytes: &[u8] = unsafe { core::slice::from_raw_parts(bytes.as_ptr(), bytes.len()) };
    // The image was verified when it was admitted; parsing it again only
    // rebuilds the view of bytes that have not changed since.
    let mut units = [Unit::EMPTY; MAX_MODULES];
    if state.linked {
        let Ok(closure) = Closure::parse(bytes) else {
            return Advance::Failed;
        };
        let mut index = 0u32;
        while index < state.module_count {
            let Some(image) = closure.image(index) else {
                return Advance::Failed;
            };
            let Ok(unit) = Unit::parse(image) else {
                return Advance::Failed;
            };
            units[index as usize] = unit;
            index += 1;
        }
    } else {
        let Ok(unit) = Unit::parse(bytes) else {
            return Advance::Failed;
        };
        units[0] = unit;
    }
    let unit = units[0];

    let steps = state.steps;
    let mut heap = Heap::adopt(
        &mut state.arena,
        &mut state.slots,
        &mut state.worklist,
        &state.saves.heap,
    );
    let mut atoms = Atoms::adopt(&mut state.entries, &mut state.handles, &state.saves.atoms);
    let mut queue = Queue::new(&mut state.jobs);
    let mut bindings = Bindings::adopt(
        &mut state.descriptors,
        &mut state.pending,
        &state.saves.bindings,
    );
    let mut machine = Vm::new(
        &unit,
        &mut heap,
        &mut atoms,
        &mut state.frames,
        &mut state.registers,
        state.realm,
        u64::from(steps),
    );
    machine.attach_regexp(&mut state.choices, &mut state.undo, &mut state.subject);
    machine.attach_jobs(&mut queue);
    machine.attach_bindings(&mut bindings, &mut state.outbox);
    machine.attach_collector(&mut state.roots, COLLECTION_SLICE, COLLECTION_HEADROOM);
    if state.linked {
        let count = state.module_count;
        let imports = state
            .import_table
            .get(..state.import_count as usize)
            .unwrap_or(&[]);
        let instances = state.instances.get_mut(..count as usize).unwrap_or(&mut []);
        machine.attach_modules(
            units.get(..count as usize).unwrap_or(&[]),
            instances,
            imports,
        );
    }
    machine.restore_all(&state.saves);
    machine.retain(state.result_value);

    let mut outcome = Advance::Running;
    let mut progressed = false;

    // An answer that arrived settles its promise before anything else runs, so
    // the reaction it schedules is next in line rather than a step behind.
    if state.wire.ready {
        state.wire.ready = false;
        state.wire.filled = 0;
        if let Some(record) = CompletionRecord::decode(&state.wire.completion) {
            let _ = machine.apply_completion(&record);
            progressed = true;
        }
    }

    if !state.body_done {
        match machine.resume(SLICE) {
            Progress::Running => {
                progressed = true;
                // This isolate carries no compiler, so an eval pause is
                // answered with the syntax error the call would throw: a
                // paused machine nothing will resume must not hold the graph.
                if machine.pending_eval().is_some() {
                    match machine.fail_eval() {
                        None => {}
                        Some(Completion::Throw(_)) => {
                            state.body_done = true;
                            state.diagnostic = Diagnostic::at(
                                diagnostic::termination::UNCAUGHT_THROW,
                                Severity::Error,
                                0,
                            )
                            .encode();
                            state.has_diagnostic = true;
                            outcome = Advance::Failed;
                        }
                        Some(Completion::Terminated(reason)) => {
                            state.body_done = true;
                            state.diagnostic =
                                Diagnostic::at(reason.code(), Severity::Error, 0).encode();
                            state.has_diagnostic = true;
                            outcome = Advance::Failed;
                        }
                        Some(Completion::Value(_)) => {}
                    }
                }
            }
            Progress::Finished(completion) => {
                progressed = true;
                match completion {
                    Completion::Value(value) => {
                        if state.linked {
                            // One module finished; the next one runs, and when
                            // the last has, the closure's value is what its
                            // entry module exports as `default`.
                            let next = state.current_module + 1;
                            if next < state.module_count {
                                state.current_module = next;
                                if machine.start_module(next).is_err() {
                                    outcome = Advance::Failed;
                                } else {
                                    state.module_started = true;
                                }
                            } else {
                                state.body_done = true;
                                let entry = state.module_count.saturating_sub(1);
                                let mut name = [0u16; 7];
                                for (index, byte) in b"default".iter().enumerate() {
                                    name[index] = u16::from(*byte);
                                }
                                let value = machine
                                    .module_export(entry, &name)
                                    .unwrap_or(Value::UNDEFINED);
                                state.result_value = value;
                                machine.retain(value);
                            }
                        } else {
                            state.body_done = true;
                            state.result_value = value;
                            machine.retain(value);
                        }
                    }
                    Completion::Terminated(reason) => {
                        // A task that stopped says why, in numbers, on a port
                        // of its own. Stopping is an outcome, not a fault.
                        state.body_done = true;
                        state.diagnostic =
                            Diagnostic::at(reason.code(), Severity::Error, 0).encode();
                        state.has_diagnostic = true;
                        outcome = Advance::Failed;
                    }
                    Completion::Throw(reason) => {
                        state.body_done = true;
                        state.diagnostic = Diagnostic::at(
                            diagnostic::termination::UNCAUGHT_THROW,
                            Severity::Error,
                            0,
                        )
                        .encode();
                        state.has_diagnostic = true;
                        // What was thrown reaches the edge in words, the way a
                        // rejection does: a person debugging a program needs
                        // the message, not just the fact.
                        if let Some(length) = render_throw(&mut machine, reason, &mut state.result)
                        {
                            state.result_length = length;
                        }
                        outcome = Advance::Failed;
                    }
                }
            }
        }
    }

    if outcome == Advance::Running {
        match machine.run_jobs(JOB_SLICE) {
            Ok(ran) => progressed |= ran > 0,
            Err(_) => outcome = Advance::Failed,
        }
    }

    // A graph that wired no call port granted no way out. The calls are
    // answered here as unavailable rather than staged for a port that does not
    // exist, so the program is told rather than left waiting.
    if outcome == Advance::Running && state.call_out < 0 && !machine.calls().is_empty() {
        let mut answers = [(0u64, 0u64); PENDING_COUNT];
        let mut count = 0usize;
        for record in machine.calls() {
            if let Some(slot) = answers.get_mut(count) {
                *slot = (record.request, record.trace);
                count += 1;
            }
        }
        machine.take_calls();
        for &(request, trace) in answers.get(..count).unwrap_or(&[]) {
            let _ = machine.apply_completion(&CompletionRecord {
                request,
                disposition: Disposition::Rejected,
                cause: Cause::Unavailable,
                trace,
                value: None,
            });
        }
        progressed = true;
    }

    // Whatever calls the program made leave as frames; the machine forgets them
    // once they are staged, so a record is carried exactly once.
    if outcome == Advance::Running && state.call_out >= 0 {
        let staged = state.wire.staged;
        let mut at = staged;
        for record in machine.calls() {
            let frame = record.encode();
            let Some(slot) = state.wire.calls.get_mut(at..at + CALL_FRAME) else {
                break;
            };
            slot.copy_from_slice(&frame);
            at += CALL_FRAME;
        }
        if at != staged {
            state.wire.staged = at;
            machine.take_calls();
            progressed = true;
        }
    }

    // A provider that never answers must not hold the task open. After the
    // wait, every outstanding call is timed out here, which the program sees as
    // an ordinary rejection saying why.
    if outcome == Advance::Running && !progressed && bindings_in_flight(&machine) > 0 {
        state.wire.waited = state.wire.waited.saturating_add(1);
        if state.wire.waited > WAIT_LIMIT {
            let mut requests = [0u64; PENDING_COUNT];
            let count = machine.outstanding(&mut requests);
            for &request in requests.get(..count).unwrap_or(&[]) {
                let _ = machine.apply_completion(&CompletionRecord {
                    request,
                    disposition: Disposition::Rejected,
                    cause: Cause::Timeout,
                    trace: TRACE,
                    value: None,
                });
            }
            state.wire.waited = 0;
        }
    } else if progressed {
        state.wire.waited = 0;
    }

    // The task is finished when its body has run, the value it produced is not
    // still waiting on anything, and no reaction is left to run.
    if outcome == Advance::Running && state.body_done && queue_idle(&machine) {
        match settled_value(&machine, state.result_value) {
            Settled::Waiting => {}
            Settled::Value(value) => {
                outcome = match render(&mut machine, value, &mut state.result) {
                    Some(length) => {
                        state.result_length = length;
                        Advance::Done
                    }
                    None => Advance::Failed,
                };
            }
            Settled::Rejected(reason) => {
                state.diagnostic =
                    Diagnostic::at(diagnostic::termination::REJECTED, Severity::Error, 0).encode();
                state.has_diagnostic = true;
                // A refusal reaches the edge as text saying why, not as
                // silence: the cause the completion carried is the answer.
                outcome = match render_rejection(&mut machine, reason, &mut state.result) {
                    Some(length) => {
                        state.result_length = length;
                        Advance::Rejected
                    }
                    None => Advance::Failed,
                };
            }
        }
    }

    state.saves = machine.save();
    outcome
}

/// What a program's result is, once whatever it was waiting on has answered.
enum Settled {
    Waiting,
    Value(Value),
    Rejected(Value),
}

fn settled_value(machine: &Vm<'_, '_, '_, '_>, value: Value) -> Settled {
    if !value.is_object() || object::is_promise(machine.heap(), value.as_handle()) != Ok(true) {
        return Settled::Value(value);
    }
    match object::promise_state(machine.heap(), value.as_handle()) {
        Ok(promise::FULFILLED) => match object::promise_value(machine.heap(), value.as_handle()) {
            Ok(settled) => Settled::Value(settled),
            Err(_) => Settled::Rejected(Value::UNDEFINED),
        },
        Ok(promise::REJECTED) => match object::promise_value(machine.heap(), value.as_handle()) {
            Ok(reason) => Settled::Rejected(reason),
            Err(_) => Settled::Rejected(Value::UNDEFINED),
        },
        _ => Settled::Waiting,
    }
}

fn queue_idle(machine: &Vm<'_, '_, '_, '_>) -> bool {
    machine.pending_jobs() == 0
}

fn bindings_in_flight(machine: &Vm<'_, '_, '_, '_>) -> u32 {
    machine.in_flight()
}

/// Render a value as the text that crosses the boundary: one byte per code unit
/// where it fits and a question mark where it does not.
fn render(
    machine: &mut Vm<'_, '_, '_, '_>,
    value: Value,
    out: &mut [u8; RESULT_CAPACITY],
) -> Option<usize> {
    let text = machine.display(value).ok()?;
    let length = string::length(machine.heap(), text).unwrap_or(0) as usize;
    let mut units = [0u16; RESULT_CAPACITY];
    if length >= units.len() {
        return None;
    }
    let written = string::copy_units(machine.heap(), text, units.get_mut(..length)?).unwrap_or(0);
    let mut index = 0usize;
    while index < written {
        let unit = units.get(index).copied().unwrap_or(0);
        let byte = if unit < 0x80 {
            u8::try_from(unit).unwrap_or(b'?')
        } else {
            b'?'
        };
        *out.get_mut(index)? = byte;
        index += 1;
    }
    *out.get_mut(index)? = b'\n';
    Some(index + 1)
}

/// Render a rejection: the typed cause where the reason carries one, and the
/// reason's own text where it does not.
/// Render an uncaught throw as `uncaught: <String(reason)>`.
fn render_throw(
    machine: &mut Vm<'_, '_, '_, '_>,
    reason: Value,
    out: &mut [u8; RESULT_CAPACITY],
) -> Option<usize> {
    let mut body = [0u8; RESULT_CAPACITY];
    let length = render(machine, reason, &mut body)?;
    let prefix = b"uncaught: ";
    let mut at = 0usize;
    while at < prefix.len() {
        *out.get_mut(at)? = *prefix.get(at)?;
        at += 1;
    }
    let mut index = 0usize;
    while index < length {
        *out.get_mut(at + index)? = *body.get(index)?;
        index += 1;
    }
    Some(at + length)
}

fn render_rejection(
    machine: &mut Vm<'_, '_, '_, '_>,
    reason: Value,
    out: &mut [u8; RESULT_CAPACITY],
) -> Option<usize> {
    let cause = machine.property(reason, b"cause").ok()?;
    let described = if cause.is_string() { cause } else { reason };
    let mut body = [0u8; RESULT_CAPACITY];
    let length = render(machine, described, &mut body)?;
    let prefix = b"rejected: ";
    let mut at = 0usize;
    while at < prefix.len() {
        *out.get_mut(at)? = *prefix.get(at)?;
        at += 1;
    }
    let mut index = 0usize;
    while index < length {
        *out.get_mut(at + index)? = *body.get(index)?;
        index += 1;
    }
    Some(at + length)
}

#[no_mangle]
#[link_section = ".text.module_state_size"]
pub extern "C" fn module_state_size() -> u32 {
    u32::try_from(core::mem::size_of::<State>()).unwrap_or(u32::MAX)
}

#[no_mangle]
#[link_section = ".text.module_init"]
pub extern "C" fn module_init(_syscalls: *const c_void) {}

#[allow(
    clippy::not_unsafe_ptr_arg_deref,
    reason = "the ABI fixes this signature: the loader passes the parameter block as a raw pointer and length, and the module reads it once under the contract that it is valid for that length"
)]
#[no_mangle]
#[link_section = ".text.module_new"]
pub extern "C" fn module_new(
    in_chan: i32,
    out_chan: i32,
    _ctrl_chan: i32,
    params: *const u8,
    params_len: usize,
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
        core::ptr::addr_of_mut!((*state).image_in).write(in_chan);
        core::ptr::addr_of_mut!((*state).result_out).write(out_chan);
        core::ptr::addr_of_mut!((*state).completion_in).write(dev_channel_port(&*table, 0, 1));
        core::ptr::addr_of_mut!((*state).call_out).write(dev_channel_port(&*table, 1, 1));
        core::ptr::addr_of_mut!((*state).diagnostic_out).write(dev_channel_port(&*table, 1, 2));
        core::ptr::addr_of_mut!((*state).exit_out).write(dev_channel_port(&*table, 1, 3));
        apply_params(&mut *state, params, params_len);
    }
    0
}

define_params! {
    State;

    1, steps, u32, 20_000_000
        => |s, d, len| { s.steps = p_u32(d, len, 0, 20_000_000); };
}

/// Take the graph's parameters, or the defaults where it gave none.
///
/// # Safety
/// `params` must be valid for reads of `params_len` bytes, or null.
unsafe fn apply_params(state: &mut State, params: *const u8, params_len: usize) {
    let tlv = !params.is_null()
        && params_len >= 4
        && *params == TLV_MAGIC
        && *params.add(1) == TLV_VERSION;
    if tlv {
        parse_tlv(state, params, params_len);
    } else {
        set_defaults(state);
    }
}

/// Stage the whole image before admitting it: a partial image is not an image,
/// and its digest would not be the one that was compiled.
fn stage_image(state: &mut State, syscalls: &SyscallTable) -> bool {
    let poll = unsafe { (syscalls.channel_poll)(state.image_in, POLL_INPUT | POLL_HUP) };
    if poll <= 0 {
        return false;
    }
    if (poll as u32) & POLL_INPUT != 0 {
        let offset = state.image_length;
        let remaining = IMAGE_CAPACITY.saturating_sub(offset);
        if remaining == 0 {
            state.overflowed = true;
            let mut discard = [0u8; 64];
            let _ = unsafe {
                (syscalls.channel_read)(state.image_in, discard.as_mut_ptr(), discard.len())
            };
            return false;
        }
        let read = unsafe {
            (syscalls.channel_read)(
                state.image_in,
                state.image.as_mut_ptr().add(offset),
                remaining,
            )
        };
        if read > 0 {
            state.image_length += usize::try_from(read).unwrap_or(0).min(remaining);
        }
        return false;
    }
    (poll as u32) & POLL_HUP != 0
}

/// Push staged call frames out, whole frames first: a partial record is not a
/// record, but the port is a byte stream and may take it in pieces.
fn push_calls(state: &mut State, syscalls: &SyscallTable) {
    if state.call_out < 0 || state.wire.written >= state.wire.staged {
        return;
    }
    let poll = unsafe { (syscalls.channel_poll)(state.call_out, POLL_OUTPUT) };
    if poll <= 0 || (poll as u32) & POLL_OUTPUT == 0 {
        return;
    }
    let offset = state.wire.written;
    let remaining = state.wire.staged.saturating_sub(offset);
    let written = unsafe {
        (syscalls.channel_write)(
            state.call_out,
            state.wire.calls.as_ptr().add(offset),
            remaining,
        )
    };
    if written > 0 {
        state.wire.written += usize::try_from(written).unwrap_or(0).min(remaining);
    }
    if state.wire.written == state.wire.staged {
        state.wire.written = 0;
        state.wire.staged = 0;
    }
}

/// Take one completion frame off the port, in whatever pieces it arrives.
fn pull_completion(state: &mut State, syscalls: &SyscallTable) {
    if state.completion_in < 0 || state.wire.ready {
        return;
    }
    let poll = unsafe { (syscalls.channel_poll)(state.completion_in, POLL_INPUT) };
    if poll <= 0 || (poll as u32) & POLL_INPUT == 0 {
        return;
    }
    let offset = state.wire.filled;
    let remaining = COMPLETION_FRAME.saturating_sub(offset);
    if remaining == 0 {
        state.wire.ready = true;
        return;
    }
    let read = unsafe {
        (syscalls.channel_read)(
            state.completion_in,
            state.wire.completion.as_mut_ptr().add(offset),
            remaining,
        )
    };
    if read > 0 {
        state.wire.filled += usize::try_from(read).unwrap_or(0).min(remaining);
    }
    if state.wire.filled == COMPLETION_FRAME {
        state.wire.ready = true;
    }
}

#[no_mangle]
#[link_section = ".text.module_step"]
pub extern "C" fn module_step(state: *mut u8) -> i32 {
    if state.is_null() {
        return -1;
    }
    let state = unsafe { &mut *state.cast::<State>() };
    if state.syscalls.is_null() || state.image_in < 0 || state.result_out < 0 {
        return -2;
    }
    let syscalls = unsafe { &*state.syscalls };

    if state.phase == 3 {
        return 1;
    }

    if state.phase == 0 {
        if !stage_image(state, syscalls) {
            return 0;
        }
        if state.image_length == 0 {
            // The stream ended with nothing in it: whatever was to produce an
            // image said why on its own port, and there is nothing to run.
            state.phase = 2;
            return 0;
        }
        if !start(state) {
            state.failed = true;
            state.phase = 2;
            return 0;
        }
        state.phase = 1;
        return 0;
    }

    if state.phase == 1 {
        push_calls(state, syscalls);
        pull_completion(state, syscalls);
        match advance(state) {
            Advance::Running => return 0,
            Advance::Done => state.phase = 2,
            Advance::Rejected => {
                state.rejected = true;
                state.phase = 2;
            }
            Advance::Failed => {
                state.failed = true;
                state.phase = 2;
            }
        }
        return 0;
    }

    if state.phase == 2 {
        if state.result_length > 0 {
            let poll = unsafe { (syscalls.channel_poll)(state.result_out, POLL_OUTPUT) };
            if poll <= 0 || (poll as u32) & POLL_OUTPUT == 0 {
                return 0;
            }
            let offset = state.result_written;
            let remaining = state.result_length.saturating_sub(offset);
            if remaining > 0 {
                let written = unsafe {
                    (syscalls.channel_write)(
                        state.result_out,
                        state.result.as_ptr().add(offset),
                        remaining,
                    )
                };
                if written > 0 {
                    state.result_written += usize::try_from(written).unwrap_or(0).min(remaining);
                }
                return 0;
            }
        }
        if state.has_diagnostic && state.diagnostic_out >= 0 {
            let poll = unsafe { (syscalls.channel_poll)(state.diagnostic_out, POLL_OUTPUT) };
            if poll > 0 && (poll as u32) & POLL_OUTPUT != 0 {
                let offset = state.diagnostic_written;
                let remaining = diagnostic::FRAME.saturating_sub(offset);
                if remaining > 0 {
                    let written = unsafe {
                        (syscalls.channel_write)(
                            state.diagnostic_out,
                            state.diagnostic.as_ptr().add(offset),
                            remaining,
                        )
                    };
                    if written > 0 {
                        state.diagnostic_written +=
                            usize::try_from(written).unwrap_or(0).min(remaining);
                    }
                }
            }
            if state.diagnostic_written < diagnostic::FRAME {
                return 0;
            }
        }
        if state.exit_out >= 0 {
            let code = i32::from(state.failed || state.rejected).to_le_bytes();
            let written =
                unsafe { (syscalls.channel_write)(state.exit_out, code.as_ptr(), code.len()) };
            if written != i32::try_from(code.len()).unwrap_or(i32::MAX) {
                return 0;
            }
        }
        state.phase = 3;
        // A program that stopped, threw, or was refused is an outcome: the exit
        // status says what happened and the diagnostic says why. A module error
        // would mean the isolate itself was at fault.
        return 1;
    }

    0
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
