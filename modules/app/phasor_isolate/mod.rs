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

#![cfg_attr(not(feature = "host-test"), no_std)]
#![allow(
    dead_code,
    unused_imports,
    unreachable_patterns,
    reason = "the Fluxor ABI source is mounted as one surface and this module consumes a subset"
)]

#[path = "../../../target/fluxor/fluxor-abi/sdk/abi.rs"]
mod abi;
use abi::SyscallTable;

include!("../../../target/fluxor/fluxor-abi/sdk/runtime.rs");
include!("../../../target/fluxor/fluxor-abi/sdk/runtime/params.rs");

#[macro_use]
#[path = "../../common/entry.rs"]
mod entry;

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;
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
#[path = "../../common/numeric.rs"]
mod numeric;
#[path = "../../common/object.rs"]
mod object;
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
#[path = "../../common/wire.rs"]
mod wire;

use binding::{
    Binding, Bindings, BindingsSave, CallRecord, Cause, CompletionRecord, Disposition, Pending,
    CALL_FRAME, COMPLETION_FRAME,
};
use bytecode::Unit;
use closure::Closure;
use diagnostic::{Diagnostic, Severity};
use heap::Slot;
use job::{Job, Queue, QueueSave};
use policy::Policy;
use realm::Realm;
use regexp::Choice;
use string::{Atoms, AtomsSave};
use value::{Handle, Value};
use vm::{Completion, Frame, ModuleInstance, Progress, Saves, Vm};

const IMAGE_CAPACITY: usize = 64 * 1024;
const RESULT_CAPACITY: usize = 128;
// Sized for the application-class targets this fmod declares: a program that
// holds a few thousand properties is ordinary, and refusing it would be a
// policy choice no deployment asked for.
const ARENA_BYTES: usize = 256 * 1024;
const SLOT_COUNT: usize = 8192;
const WORKLIST: usize = 2048;
const ATOM_ENTRIES: usize = 1024;
const ATOM_HANDLES: usize = 768;
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
/// Roots a collection stages before it starts: the registers in use, the
/// frames, the interned handles, and the realm all fit with room over.
const ROOT_COUNT: usize = 8192;
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
/// Steps a call may wait for an answer before the isolate times it out, when
/// the graph names no other number. A provider that never answers must not
/// hold a task open forever, and a deployment that knows its providers says
/// how long waiting is reasonable.
const WAIT_LIMIT: u32 = 5_000;
/// One control record: a kind, three bytes of padding, and a value.
/// Kind 1 asks the task to stop at its next safe point; kind 2 sets the
/// deadline, in the host's own time units; kind 3 reports the current time in
/// those units. A deadline passes only when a reported time reaches it, so a
/// graph that wires no clock has no deadline.
const CONTROL_FRAME: usize = 12;
/// Modules one closure may hold, and imports it may have in total.
const MAX_MODULES: usize = 16;
const MAX_IMPORTS: usize = 128;
/// The trace every call this isolate makes belongs to.
const TRACE: u64 = 0x5041_5348_4f52_0001;

/// The limits this isolate runs a task under: its storage, as declared
/// above, and the fuel the graph named. Clamped to the ceiling, so a graph
/// can narrow the compiled-in maxima but never widen them.
fn policy(state: &State) -> Policy {
    Policy {
        heap_bytes: ARENA_BYTES as u32,
        heap_cells: SLOT_COUNT as u32,
        fuel: u64::from(state.steps),
        frames: FRAME_COUNT as u32,
        registers: REGISTER_COUNT as u32,
        jobs: JOB_COUNT as u32,
        pending_calls: PENDING_COUNT as u32,
        image_bytes: IMAGE_CAPACITY as u32,
        deadline_ms: 0,
        collection_slice: COLLECTION_SLICE,
    }
    .clamped()
}

/// The seams beyond the storage: the linked closure's instances and resolved
/// imports, when the image was one.
fn attachments<'a>(
    linked: bool,
    instances: &'a mut [ModuleInstance],
    import_table: &'a [(u32, u32)],
    module_count: u32,
    import_count: u32,
    admitted: &'a [Binding],
) -> agent::Attachments<'a, 'static> {
    agent::Attachments {
        admitted,
        modules: if linked {
            Some(agent::Closure {
                instances: instances
                    .get_mut(..module_count as usize)
                    .unwrap_or(&mut []),
                imports: import_table.get(..import_count as usize).unwrap_or(&[]),
            })
        } else {
            None
        },
        module_names: &[],
        module_cycles: &[],
        compiler: None,
        print: None,
    }
}

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
    control_in: i32,
    steps: u32,
    call_wait: u32,
    /// What the control port has asked for, applied to the machine each step.
    cancel_requested: bool,
    deadline: u64,
    now: u64,
    control_frame: [u8; CONTROL_FRAME],
    control_filled: usize,
    /// Counters for the telemetry ring, in `[observability].metrics` order.
    fuel_spent: u64,
    collections: u32,
    calls_made: u32,
    completions_applied: u32,
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
    // SAFETY: the image is staged and not written while the machine runs, so
    // it may keep reading it while the rest of the state moves.
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
                deferred_namespace: Value::UNDEFINED,
                completion: Value::UNDEFINED,
                body_pc: 0,
                evaluated: 0,
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
    // One binding is admitted here, by the digest of its name and schema. What
    // answers it, and where that is, the isolate never learns.
    let admitted = [Binding {
        name: digest::digest(b"host"),
        schema: digest::digest(b"number->number"),
        in_flight_max: IN_FLIGHT_MAX,
        in_flight: 0,
    }];
    let policy = policy(state);
    let metering = agent::Metering {
        policy: &policy,
        collection_headroom: COLLECTION_HEADROOM,
        trace: TRACE,
    };
    let linked = state.linked;
    let module_count = state.module_count;
    let import_count = state.import_count;
    let attach = attachments(
        linked,
        &mut state.instances,
        &state.import_table,
        module_count,
        import_count,
        &admitted,
    );
    let storage = agent_storage!(state);
    let started = agent::fresh(
        units.get(..count).unwrap_or(&[]),
        storage,
        attach,
        metering,
        |machine| {
            if machine.define_binding(b"host", 0).is_err() {
                return false;
            }
            if linked {
                // Every module's environment is made before any of them
                // runs, which is what lets one read another's exports once
                // it has.
                let mut index = 0u32;
                while index < module_count {
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
                true
            } else {
                machine.start().is_ok()
            }
        },
    );
    let Ok((ok, realm, saves)) = started else {
        return false;
    };
    if !ok {
        return false;
    }
    state.saves = saves;
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
    // SAFETY: the image is staged and not written while the machine runs, so
    // it may keep reading it while the rest of the state moves.
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
    let steps = state.steps;
    let policy = policy(state);
    let metering = agent::Metering {
        policy: &policy,
        collection_headroom: COLLECTION_HEADROOM,
        trace: TRACE,
    };
    let linked = state.linked;
    let module_count = state.module_count;
    let import_count = state.import_count;
    let attach = attachments(
        linked,
        &mut state.instances,
        &state.import_table,
        module_count,
        import_count,
        &[],
    );
    let storage = agent_storage!(state);
    let realm = state.realm;
    let saves = state.saves;
    let count = if linked { module_count as usize } else { 1 };
    let stepped = agent::adopt(
        units.get(..count).unwrap_or(&[]),
        storage,
        attach,
        metering,
        realm,
        &saves,
        |machine| {
            machine.retain(state.result_value);
            // What the control port asked for reaches the machine here, every step:
            // a cancel is sticky, and a deadline passes when a reported time reaches
            // it. The machine observes both only at safe points.
            if state.cancel_requested {
                machine.control().cancel();
            }
            if state.deadline != 0 {
                machine.control().set_deadline(state.deadline);
            }
            if state.now != 0 {
                machine.control().observe(state.now);
            }

            let mut outcome = Advance::Running;
            let mut progressed = false;

            // An answer that arrived settles its promise before anything else runs, so
            // the reaction it schedules is next in line rather than a step behind.
            if state.wire.ready {
                state.wire.ready = false;
                state.wire.filled = 0;
                if let Some(record) = CompletionRecord::decode(&state.wire.completion) {
                    let _ = machine.apply_completion(&record);
                    state.completions_applied = state.completions_applied.saturating_add(1);
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
                                if let Some(length) =
                                    render_throw(machine, reason, &mut state.result)
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
                    let staged_now = u32::try_from((at - staged) / CALL_FRAME).unwrap_or(0);
                    state.calls_made = state.calls_made.saturating_add(staged_now);
                    progressed = true;
                }
            }

            // A provider that never answers must not hold the task open. After the
            // wait, every outstanding call is timed out here, which the program sees as
            // an ordinary rejection saying why.
            if outcome == Advance::Running && !progressed && bindings_in_flight(machine) > 0 {
                state.wire.waited = state.wire.waited.saturating_add(1);
                if state.wire.waited > state.call_wait {
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
            if outcome == Advance::Running && state.body_done && queue_idle(machine) {
                match settled_value(machine, state.result_value) {
                    Settled::Waiting => {}
                    Settled::Value(value) => {
                        outcome = match render(machine, value, &mut state.result) {
                            Some(length) => {
                                state.result_length = length;
                                Advance::Done
                            }
                            None => Advance::Failed,
                        };
                    }
                    Settled::Rejected(reason) => {
                        state.diagnostic =
                            Diagnostic::at(diagnostic::termination::REJECTED, Severity::Error, 0)
                                .encode();
                        state.has_diagnostic = true;
                        // A refusal reaches the edge as text saying why, not as
                        // silence: the cause the completion carried is the answer.
                        outcome = match render_rejection(machine, reason, &mut state.result) {
                            Some(length) => {
                                state.result_length = length;
                                Advance::Rejected
                            }
                            None => Advance::Failed,
                        };
                    }
                }
            }

            state.fuel_spent = u64::from(steps).saturating_sub(machine.fuel());
            state.collections = machine.collections();
            outcome
        },
    );
    let Ok((outcome, saves)) = stepped else {
        return Advance::Failed;
    };
    state.saves = saves;
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

entry! {
    State;
    primary { image_in, result_out }
    inputs { completion_in = 1, control_in = 2 }
    outputs { call_out = 1, diagnostic_out = 2, exit_out = 3 }
    params apply_params
}

define_params! {
    State;

    1, steps, u32, 20_000_000
        => |s, d, len| { s.steps = p_u32(d, len, 0, 20_000_000); };

    2, call_wait, u32, 5_000
        => |s, d, len| { s.call_wait = p_u32(d, len, 0, WAIT_LIMIT); };
}

/// Take the graph's parameters, or the defaults where it gave none.
///
/// # Safety
/// `params` must be valid for reads of `params_len` bytes, or null.
unsafe fn apply_params(state: &mut State, params: *const u8, params_len: usize) {
    if wire::params_are_tlv(params, params_len, TLV_MAGIC, TLV_VERSION) {
        parse_tlv(state, params, params_len);
    } else {
        set_defaults(state);
    }
}

/// Emit the task's counters to the telemetry ring, once, when it ends.
///
/// Emission is gated on a subscribed consumer, so an unobserved graph pays
/// nothing. What leaves is numbers about the run — outcomes, fuel, collection
/// and call counts — never source, values, or payloads.
fn emit_telemetry(state: &mut State, syscalls: &SyscallTable) {
    // SAFETY: every call goes through the loader's syscall table, live for the
    // module's lifetime, with arguments that are plain numbers.
    unsafe {
        if !dev_telemetry_enabled(syscalls) {
            return;
        }
        let me = dev_self_index(syscalls);
        if me < 0 {
            return;
        }
        let midx = me as u16;
        let t = dev_micros(syscalls);
        let counter = abi::contracts::telemetry::METRIC_COUNTER;
        let outcome_finished = u64::from(!state.failed && !state.rejected);
        let outcome_stopped = u64::from(state.failed);
        let outcome_rejected = u64::from(state.rejected);
        for (id, value) in [
            (0u16, outcome_finished),
            (1, outcome_stopped),
            (2, outcome_rejected),
            (3, state.fuel_spent),
            (4, u64::from(state.collections)),
            (5, u64::from(state.calls_made)),
            (6, u64::from(state.completions_applied)),
        ] {
            dev_telemetry_metric(syscalls, -1, midx, t, counter, id, value);
        }
    }
}

/// Drain whole control records, staging a partial read until it completes.
fn drain_control(state: &mut State, syscalls: &SyscallTable) {
    if state.control_in < 0 {
        return;
    }
    loop {
        let before = state.control_filled;
        let whole = wire::take_frame(
            syscalls,
            state.control_in,
            &mut state.control_frame,
            &mut state.control_filled,
        );
        if !whole {
            if state.control_filled == before {
                return;
            }
            continue;
        }
        state.control_filled = 0;
        let kind = state.control_frame[0];
        let value = u64::from_le_bytes([
            state.control_frame[4],
            state.control_frame[5],
            state.control_frame[6],
            state.control_frame[7],
            state.control_frame[8],
            state.control_frame[9],
            state.control_frame[10],
            state.control_frame[11],
        ]);
        match kind {
            1 => state.cancel_requested = true,
            2 => state.deadline = value,
            3 => state.now = value,
            _ => {}
        }
    }
}

/// Stage the whole image before admitting it: a partial image is not an image,
/// and its digest would not be the one that was compiled.
fn stage_image(state: &mut State, syscalls: &SyscallTable) -> bool {
    wire::stage_stream(
        syscalls,
        state.image_in,
        &mut state.image,
        &mut state.image_length,
        &mut state.overflowed,
    ) == wire::Staged::Complete
}

/// Push staged call frames out, whole frames first: a partial record is not a
/// record, but the port is a byte stream and may take it in pieces.
fn push_calls(state: &mut State, syscalls: &SyscallTable) {
    wire::push_staged(
        syscalls,
        state.call_out,
        &state.wire.calls,
        &mut state.wire.staged,
        &mut state.wire.written,
    );
}

/// Take one completion frame off the port, in whatever pieces it arrives.
fn pull_completion(state: &mut State, syscalls: &SyscallTable) {
    if state.wire.ready {
        return;
    }
    if wire::take_frame(
        syscalls,
        state.completion_in,
        &mut state.wire.completion,
        &mut state.wire.filled,
    ) {
        state.wire.ready = true;
    }
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
    if state.syscalls.is_null() || state.image_in < 0 || state.result_out < 0 {
        return -2;
    }
    // SAFETY: the table pointer was stored by `module_new` and checked
    // non-null above; the loader keeps it live for the module's lifetime.
    let syscalls = unsafe { &*state.syscalls };

    if state.phase == 3 {
        return 1;
    }
    drain_control(state, syscalls);

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
        let result = state.result.get(..state.result_length).unwrap_or(&[]);
        if !wire::push_progress(
            syscalls,
            state.result_out,
            result,
            &mut state.result_written,
        ) {
            return 0;
        }
        if state.has_diagnostic
            && state.diagnostic_out >= 0
            && !wire::push_progress(
                syscalls,
                state.diagnostic_out,
                &state.diagnostic,
                &mut state.diagnostic_written,
            )
        {
            return 0;
        }
        if !wire::push_exit(syscalls, state.exit_out, state.failed || state.rejected) {
            return 0;
        }
        emit_telemetry(state, syscalls);
        state.phase = 3;
        // A program that stopped, threw, or was refused is an outcome: the exit
        // status says what happened and the diagnostic says why. A module error
        // would mean the isolate itself was at fault.
        return 1;
    }

    0
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
