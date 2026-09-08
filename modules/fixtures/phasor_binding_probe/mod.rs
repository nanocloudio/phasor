//! On-graph conformance probe for typed capability bindings.
//!
//! A program reaches the outside only through a binding the deployment
//! admitted. The probe grants one, calls it from source, checks the call record
//! that comes out, answers it, and checks that the promise settles.

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
#[path = "../../common/capability.rs"]
mod capability;
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
use binding::{
    Answer, Binding, Bindings, CallError, CallRecord, Cause, CompletionRecord, Disposition, Pending,
};
use bytecode::{Constant, ExceptionRegion, ExportRecord, Function, ImportRecord, Opcode, Unit};
use emit::{CodeBuilder, Patch, UnitWriter};
use heap::{Heap, Slot};
use job::{Job, Queue};
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

const CASE_COUNT: u16 = 26;
const FUEL: u32 = 400_000;
/// The trace every call the probe makes belongs to.
const TRACE: u64 = 0x5041_5348_4f52_0001;
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
const SLOT_COUNT: usize = 3072;
const ATOM_ENTRIES: usize = 2048;
const ATOM_HANDLES: usize = 1536;
const FRAME_COUNT: usize = 12;
const REGISTER_COUNT: usize = 96;
const WORKLIST: usize = 512;
const JOB_COUNT: usize = 16;
const BINDING_COUNT: usize = 4;
const PENDING_COUNT: usize = 4;
const OUTBOX_COUNT: usize = 8;

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
    jobs: [Job; JOB_COUNT],
    descriptors: [Binding; BINDING_COUNT],
    pending_calls: [Pending; PENDING_COUNT],
    outbox: [CallRecord; OUTBOX_COUNT],
}

/// A call the program made and the promise it is waiting on.
struct Called {
    record: CallRecord,
    promise: Value,
}

/// Compile `source`, grant one binding under `name`, and run it.
///
/// `check` is handed the machine afterwards, so a case can answer the call and
/// see what happens.
fn with_binding(
    storage: &mut Storage,
    source: &[u8],
    name: &[u8],
    in_flight_max: u32,
    check: impl Fn(&mut Vm<'_, '_, '_, '_>, Option<Called>, Value) -> bool,
) -> bool {
    let length = {
        let mut front = frontend_storage!(storage);
        let goal = frontend::Goal::Script;
        match frontend::compile(source, goal, Limits::CEILING, FUEL, &mut front) {
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

    let mut heap = Heap::with_worklist(
        &mut storage.arena,
        &mut storage.slots,
        &mut storage.worklist,
    );
    let mut atoms = Atoms::new(&mut storage.entries, &mut storage.handles);
    let Ok(realm) = realm::create(&mut heap, &mut atoms) else {
        return false;
    };
    let mut queue = Queue::new(&mut storage.jobs);
    let mut bindings = Bindings::new(&mut storage.descriptors, &mut storage.pending_calls);
    let admitted = bindings.admit(Binding {
        name: digest::digest(name),
        schema: digest::digest(b"schema"),
        in_flight_max,
        in_flight: 0,
        class: binding::Class::Async,
        snapshot: 0.0,
    });
    let Ok(admitted) = admitted else {
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
    machine.attach_jobs(&mut queue);
    machine.attach_bindings(&mut bindings, &mut storage.outbox);
    machine.set_trace(TRACE);
    if !name.is_empty() && machine.define_binding(name, admitted).is_err() {
        return false;
    }

    let value = match machine.run() {
        Completion::Value(value) => value,
        Completion::Throw(_) => Value::UNDEFINED,
        Completion::Terminated(_) => return false,
    };
    let called = machine.calls().first().map(|record| Called {
        record: *record,
        promise: value,
    });
    check(&mut machine, called, value)
}

/// Whether a string value reads as the given ASCII text.
fn reads_ascii(machine: &Vm<'_, '_, '_, '_>, value: Value, text: &[u8]) -> bool {
    if !value.is_string() {
        return false;
    }
    let mut units = [0u16; 32];
    let Ok(length) = string::copy_units(machine.heap(), value.as_handle(), &mut units) else {
        return false;
    };
    if length != text.len() {
        return false;
    }
    units
        .iter()
        .zip(text.iter())
        .take(length)
        .all(|(&unit, &byte)| unit == u16::from(byte))
}

/// Whether a value is a promise in the given state.
fn promise_is(machine: &Vm<'_, '_, '_, '_>, value: Value, state: u8) -> bool {
    value.is_object()
        && object::is_promise(machine.heap(), value.as_handle()) == Ok(true)
        && object::promise_state(machine.heap(), value.as_handle()) == Ok(state)
}

#[allow(
    clippy::match_same_arms,
    reason = "each case is an independent assertion and merging arms would hide which one failed"
)]
fn run_case(storage: &mut Storage, case: u16) -> bool {
    match case {
        // The table.
        0 => {
            let mut descriptors = [Binding::EMPTY; 2];
            let mut pending = [Pending::EMPTY; 2];
            let mut bindings = Bindings::new(&mut descriptors, &mut pending);
            let index = bindings.admit(Binding {
                name: digest::digest(b"clock"),
                schema: digest::digest(b"s"),
                in_flight_max: 1,
                in_flight: 0,
                class: binding::Class::Async,
                snapshot: 0.0,
            });
            index == Ok(0) && bindings.admitted() == 1
        }
        1 => {
            let mut descriptors = [Binding::EMPTY; 2];
            let mut pending = [Pending::EMPTY; 2];
            let mut bindings = Bindings::new(&mut descriptors, &mut pending);
            bindings.begin(0, Value::UNDEFINED, 0) == Err(CallError::NotAdmitted)
        }
        2 => {
            let mut descriptors = [Binding::EMPTY; 2];
            let mut pending = [Pending::EMPTY; 4];
            let mut bindings = Bindings::new(&mut descriptors, &mut pending);
            let Ok(index) = bindings.admit(Binding {
                name: digest::digest(b"clock"),
                schema: digest::digest(b"s"),
                in_flight_max: 2,
                in_flight: 0,
                class: binding::Class::Async,
                snapshot: 0.0,
            }) else {
                return false;
            };
            let first = bindings.begin(index, Value::UNDEFINED, 11);
            let second = bindings.begin(index, Value::UNDEFINED, 11);
            let third = bindings.begin(index, Value::UNDEFINED, 11);
            first.is_ok()
                && second.is_ok()
                && first != second
                && third == Err(CallError::TooManyInFlight)
                && bindings.in_flight() == 2
        }
        3 => {
            let mut descriptors = [Binding::EMPTY; 2];
            let mut pending = [Pending::EMPTY; 4];
            let mut bindings = Bindings::new(&mut descriptors, &mut pending);
            let Ok(index) = bindings.admit(Binding {
                name: digest::digest(b"clock"),
                schema: digest::digest(b"s"),
                in_flight_max: 2,
                in_flight: 0,
                class: binding::Class::Async,
                snapshot: 0.0,
            }) else {
                return false;
            };
            let Ok(request) = bindings.begin(index, Value::UNDEFINED, 11) else {
                return false;
            };
            let traced = bindings.trace_of(request) == Some(11);
            let first = traced && bindings.complete(request).is_ok();
            let second = bindings.complete(request);
            first && matches!(second, Err(CallError::UnknownRequest)) && bindings.in_flight() == 0
        }

        // A call from source.
        4 => with_binding(
            storage,
            b"readClock('a')",
            b"readClock",
            2,
            |machine, called, value| {
                called.is_some() && promise_is(machine, value, promise::PENDING)
            },
        ),
        5 => with_binding(
            storage,
            b"readClock('a')",
            b"readClock",
            2,
            |machine, called, _| {
                let Some(called) = called else {
                    return false;
                };
                called.record.binding == 0
                    && called.record.request != 0
                    && machine.calls().len() == 1
            },
        ),
        6 => with_binding(
            storage,
            b"readClock('a')",
            b"readClock",
            2,
            |machine, called, value| {
                let Some(called) = called else {
                    return false;
                };
                machine
                    .complete_call(
                        called.record.request,
                        Disposition::Fulfilled,
                        Value::number(42.0),
                    )
                    .is_ok()
                    && promise_is(machine, value, promise::FULFILLED)
                    && object::promise_value(machine.heap(), value.as_handle())
                        .map(|settled| settled.as_number())
                        == Ok(42.0)
            },
        ),
        7 => with_binding(
            storage,
            b"readClock('a')",
            b"readClock",
            2,
            |machine, called, value| {
                let Some(called) = called else {
                    return false;
                };
                machine
                    .complete_call(
                        called.record.request,
                        Disposition::Rejected,
                        Value::number(7.0),
                    )
                    .is_ok()
                    && promise_is(machine, value, promise::REJECTED)
            },
        ),
        8 => with_binding(
            storage,
            b"readClock('a')",
            b"readClock",
            2,
            |machine, called, _| {
                let Some(called) = called else {
                    return false;
                };
                let first = machine.complete_call(
                    called.record.request,
                    Disposition::Fulfilled,
                    Value::UNDEFINED,
                );
                let second = machine.complete_call(
                    called.record.request,
                    Disposition::Fulfilled,
                    Value::UNDEFINED,
                );
                first.is_ok() && second == Err(CallError::UnknownRequest)
            },
        ),
        9 => with_binding(
            storage,
            b"readClock('a')",
            b"readClock",
            2,
            |machine, _, _| {
                let before = machine.calls().len();
                machine.take_calls();
                before == 1 && machine.calls().is_empty()
            },
        ),

        // Two calls produce two records with different identifiers.
        10 => with_binding(
            storage,
            b"[readClock('a'), readClock('b')].length",
            b"readClock",
            2,
            |machine, _, _| {
                let calls = machine.calls();
                calls.len() == 2 && calls[0].request != calls[1].request
            },
        ),

        // The in-flight quota rejects rather than throwing.
        11 => with_binding(
            storage,
            b"[readClock('a'), readClock('b')][1]",
            b"readClock",
            1,
            |machine, _, value| promise_is(machine, value, promise::REJECTED),
        ),

        // The payload digest depends on the arguments.
        12 => with_binding(
            storage,
            b"readClock('a')",
            b"readClock",
            2,
            |_, called, _| {
                called.is_some_and(|called| called.record.payload != digest::digest(b""))
            },
        ),

        // A name that was not granted is not reachable at all.
        13 => with_binding(
            storage,
            b"missingCapability('a')",
            b"readClock",
            2,
            |machine, called, _| called.is_none() && machine.calls().is_empty(),
        ),

        // The wire records survive the trip out and back.
        14 => {
            let record = CallRecord {
                request: 7,
                binding: 3,
                payload_length: 0,
                trace: 0x0102_0304_0506_0708,
                payload: digest::digest(b"a"),
            };
            let frame = record.encode();
            CallRecord::decode(&frame) == Some(record) && CallRecord::decode(&frame[..8]).is_none()
        }
        15 => {
            let record = CompletionRecord {
                request: 7,
                disposition: Disposition::Fulfilled,
                cause: Cause::None,
                trace: 9,
                answer: Answer::Number(42.5),
            };
            let frame = record.encode();
            CompletionRecord::decode(&frame) == Some(record)
                && CompletionRecord::decode(&frame[..8]).is_none()
        }
        16 => {
            let record = CompletionRecord {
                request: 7,
                disposition: Disposition::Rejected,
                cause: Cause::Timeout,
                trace: 9,
                answer: Answer::None,
            };
            CompletionRecord::decode(&record.encode()) == Some(record)
                && Cause::Timeout.retryable()
                && !Cause::Denied.retryable()
        }

        // A completion carries the trace the call went out under, and one that
        // does not match is refused rather than settling the wrong promise.
        17 => with_binding(
            storage,
            b"readClock('a')",
            b"readClock",
            2,
            |machine, called, value| {
                let Some(called) = called else {
                    return false;
                };
                called.record.trace == TRACE
                    && machine
                        .apply_completion(&CompletionRecord {
                            request: called.record.request,
                            disposition: Disposition::Fulfilled,
                            cause: Cause::None,
                            trace: TRACE,
                            answer: Answer::Number(42.0),
                        })
                        .is_ok()
                    && promise_is(machine, value, promise::FULFILLED)
            },
        ),
        18 => with_binding(
            storage,
            b"readClock('a')",
            b"readClock",
            2,
            |machine, called, value| {
                let Some(called) = called else {
                    return false;
                };
                machine.apply_completion(&CompletionRecord {
                    request: called.record.request,
                    disposition: Disposition::Fulfilled,
                    cause: Cause::None,
                    trace: TRACE ^ 1,
                    answer: Answer::Number(42.0),
                }) == Err(CallError::UnknownRequest)
                    && promise_is(machine, value, promise::PENDING)
            },
        ),

        // A refusal reaches the program as an error saying why.
        19 => with_binding(
            storage,
            b"readClock('a')",
            b"readClock",
            2,
            |machine, called, value| {
                let Some(called) = called else {
                    return false;
                };
                if machine
                    .apply_completion(&CompletionRecord {
                        request: called.record.request,
                        disposition: Disposition::Rejected,
                        cause: Cause::Denied,
                        trace: TRACE,
                        answer: Answer::None,
                    })
                    .is_err()
                {
                    return false;
                }
                if !promise_is(machine, value, promise::REJECTED) {
                    return false;
                }
                let Ok(reason) = object::promise_value(machine.heap(), value.as_handle()) else {
                    return false;
                };
                let Ok(cause) = machine.property(reason, b"cause") else {
                    return false;
                };
                let Ok(retryable) = machine.property(reason, b"retryable") else {
                    return false;
                };
                reason.is_object()
                    && cause.is_string()
                    && reads_ascii(machine, cause, b"Denied")
                    && retryable.is_boolean()
                    && !retryable.as_boolean()
            },
        ),

        // What an image states it requires. A capability import is an
        // ordinary import with a scheme, so the requirement is recorded in
        // the image's own table and nothing outside it can claim one.
        20 => requires(
            storage,
            b"import { read } from \"phasor:store\"; export default 1;",
            |required, count| {
                count == 1
                    && required[0].interface() == b"store"
                    && required[0].member() == b"read"
                    && required[0].name() == digest::digest(b"store.read")
            },
        ),
        21 => requires(
            storage,
            b"import { now } from \"phasor:clock\";\nimport { read } from \"phasor:store\";\nexport default 1;",
            |required, count| {
                count == 2 && required[0].interface() == b"clock" && required[1].member() == b"read"
            },
        ),
        // An ordinary module import states no requirement.
        22 => requires(
            storage,
            b"import { x } from \"./other.js\"; export default 1;",
            |_, count| count == 0,
        ),
        23 => {
            capability::is_capability(&[0x70, 0x68, 0x61, 0x73, 0x6F, 0x72, 0x3A, 0x61])
                && !capability::is_capability(&[0x2E, 0x2F, 0x61])
                && !capability::is_capability(&[0x70, 0x68])
        }

        // A resource a provider opened comes back as a handle: an index and a
        // generation, meaningful only to the binding that issued it.
        24 => {
            let mut descriptors = [Binding::EMPTY; 2];
            let mut pending = [Pending::EMPTY; 2];
            let mut resources = [binding::Resource::EMPTY; 4];
            let mut bindings = Bindings::new(&mut descriptors, &mut pending);
            bindings.attach_resources(&mut resources);
            let admitted = bindings.admit(Binding {
                name: digest::digest(b"store.open"),
                schema: digest::digest(b"s"),
                in_flight_max: 2,
                in_flight: 0,
                class: binding::Class::Async,
                snapshot: 0.0,
            });
            let Ok(index) = admitted else {
                return false;
            };
            let Ok(handle) = bindings.open(index, 0x5A) else {
                return false;
            };
            bindings.resolve(index, handle) == Ok(0x5A)
                && bindings.resolve(index + 1, handle) == Err(CallError::StaleHandle)
                && binding::Handle::unpack(handle.pack()) == handle
        }
        25 => {
            let mut descriptors = [Binding::EMPTY; 1];
            let mut pending = [Pending::EMPTY; 1];
            let mut resources = [binding::Resource::EMPTY; 2];
            let mut bindings = Bindings::new(&mut descriptors, &mut pending);
            bindings.attach_resources(&mut resources);
            let admitted = bindings.admit(Binding {
                name: digest::digest(b"store.open"),
                schema: digest::digest(b"s"),
                in_flight_max: 1,
                in_flight: 0,
                class: binding::Class::Async,
                snapshot: 0.0,
            });
            let Ok(index) = admitted else {
                return false;
            };
            let Ok(handle) = bindings.open(index, 7) else {
                return false;
            };
            // A released handle names nothing ever again: the slot's
            // generation advanced, and the slot itself may be reused.
            bindings.release(index, handle) == Ok(7)
                && bindings.resolve(index, handle) == Err(CallError::StaleHandle)
                && bindings.open(index, 9).is_ok_and(|next| {
                    next.index == handle.index && next.generation != handle.generation
                })
        }
        _ => true,
    }
}

/// Compile a module and hand its stated requirements to `check`.
fn requires(
    storage: &mut Storage,
    source: &[u8],
    check: impl Fn(&[capability::Requirement; 8], usize) -> bool,
) -> bool {
    let length = {
        let mut front = frontend_storage!(storage);
        match frontend::compile(
            source,
            frontend::Goal::Module,
            Limits::CEILING,
            FUEL,
            &mut front,
        ) {
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
    let mut required = [capability::Requirement::EMPTY; 8];
    let count = capability::requirements(&unit, &mut required);
    check(&required, count)
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
        b"phasor-binding-probe",
        CASE_COUNT,
        |case| run_case(&mut state.storage, case),
    )
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
