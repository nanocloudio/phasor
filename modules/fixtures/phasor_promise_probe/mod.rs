//! On-graph conformance probe for the job queue and promises.

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
#[macro_use]
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
use job::{Job, JobKind, Queue, QueueError};
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

const CASE_COUNT: u16 = 21;
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
const SLOT_COUNT: usize = 3072;
const ATOM_ENTRIES: usize = 2048;
const ATOM_HANDLES: usize = 1536;
const FRAME_COUNT: usize = 12;
const REGISTER_COUNT: usize = 96;
const WORKLIST: usize = 512;
const JOB_COUNT: usize = 16;

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
}

/// What running a source produced.
struct Outcome {
    /// The value the program returned, if it returned one.
    value: Option<Value>,
    /// The promise state of that value, when it is a promise.
    state: Option<u8>,
    /// The value that promise settled with, once the jobs ran.
    settled: Option<f64>,
    /// The text of the value, when it is not a promise.
    text: [u16; 32],
    text_length: usize,
    threw: bool,
}

/// Compile `source`, run it, then run every job it queued.
fn evaluate(storage: &mut Storage, source: &[u8]) -> Option<Outcome> {
    let length = {
        let table = LineTable::new(&mut storage.starts);
        let lexer = Lexer::new(source, Limits::CEILING, table, FUEL).ok()?;
        let syntax = Arena::new(&mut storage.nodes, &mut storage.lists, &mut storage.numbers);
        let mut parser = Parser::new(lexer, syntax, &mut storage.scratch, Limits::CEILING);
        let root = parser.parse_unit().ok()?;
        let mut lowering = lower_storage!(storage);
        lower_expression(source, parser.arena(), root, &mut lowering)
            .ok()
            .map(|compiled| compiled.length)?
    };
    let bytes = storage.image.get(..length)?;
    let unit = Unit::parse(bytes).ok()?;

    let mut heap = Heap::with_worklist(
        &mut storage.arena,
        &mut storage.slots,
        &mut storage.worklist,
    );
    let mut atoms = Atoms::new(&mut storage.entries, &mut storage.handles);
    let realm = realm::create(&mut heap, &mut atoms).ok()?;
    let mut queue = Queue::new(&mut storage.jobs);
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

    let mut outcome = Outcome {
        value: None,
        state: None,
        settled: None,
        text: [0; 32],
        text_length: 0,
        threw: false,
    };
    let completion = machine.run();
    let value = match completion {
        Completion::Value(value) => value,
        Completion::Throw(_) => {
            outcome.threw = true;
            return Some(outcome);
        }
        Completion::Terminated(_) => return None,
    };
    outcome.value = Some(value);

    let promise =
        value.is_object() && object::is_promise(machine.heap(), value.as_handle()) == Ok(true);
    if promise {
        outcome.state = object::promise_state(machine.heap(), value.as_handle()).ok();
        let mut rounds = 0u32;
        while machine.pending_jobs() > 0 && rounds < 32 {
            if machine.run_jobs(16).is_err() {
                return None;
            }
            rounds += 1;
        }
        outcome.state = object::promise_state(machine.heap(), value.as_handle()).ok();
        outcome.settled = object::promise_value(machine.heap(), value.as_handle())
            .ok()
            .map(|settled| settled.as_number());
    } else if let Ok(text) = machine.display(value) {
        let length = string::length(machine.heap(), text).unwrap_or(0) as usize;
        if length <= outcome.text.len() {
            outcome.text_length =
                string::copy_units(machine.heap(), text, &mut outcome.text[..length]).unwrap_or(0);
        }
    }
    Some(outcome)
}

/// Whether the text of a non-promise result is this ASCII text.
fn text_is(outcome: &Outcome, expected: &[u8]) -> bool {
    if outcome.text_length != expected.len() {
        return false;
    }
    let mut index = 0usize;
    while index < expected.len() {
        if outcome.text[index] != u16::from(expected[index]) {
            return false;
        }
        index += 1;
    }
    true
}

#[allow(
    clippy::match_same_arms,
    reason = "each case is an independent assertion and merging arms would hide which one failed"
)]
fn run_case(storage: &mut Storage, case: u16) -> bool {
    match case {
        // The queue itself.
        0 => {
            let mut slots = [Job::EMPTY; 3];
            let queue = Queue::new(&mut slots);
            queue.is_empty() && queue.capacity() == 3 && queue.enqueued() == 0
        }
        1 => {
            let mut slots = [Job::EMPTY; 2];
            let mut queue = Queue::new(&mut slots);
            queue.push(Job::EMPTY).is_ok()
                && queue.push(Job::EMPTY).is_ok()
                && queue.push(Job::EMPTY) == Err(QueueError::Full)
        }
        2 => {
            // First in, first out, including across the ring's wrap.
            let mut slots = [Job::EMPTY; 3];
            let mut queue = Queue::new(&mut slots);
            let mut index = 0u32;
            while index < 3 {
                let job = Job {
                    argument: Value::number(f64::from(index)),
                    ..Job::EMPTY
                };
                if queue.push(job).is_err() {
                    return false;
                }
                index += 1;
            }
            let first = queue.pop();
            let pushed = queue.push(Job {
                argument: Value::number(9.0),
                ..Job::EMPTY
            });
            let second = queue.pop();
            first.is_some_and(|job| job.argument.as_number() == 0.0)
                && pushed.is_ok()
                && second.is_some_and(|job| job.argument.as_number() == 1.0)
        }
        3 => {
            let mut slots = [Job::EMPTY; 4];
            let mut queue = Queue::new(&mut slots);
            let _ = queue.push(Job::EMPTY);
            let _ = queue.pop();
            let _ = queue.push(Job::EMPTY);
            queue.enqueued() == 2 && queue.len() == 1
        }
        4 => matches!(Job::EMPTY.kind, JobKind::Reaction),

        // Promises through the language.
        5 => evaluate(storage, b"typeof Promise")
            .is_some_and(|outcome| text_is(&outcome, b"function")),
        6 => evaluate(storage, b"typeof Promise.resolve(1)")
            .is_some_and(|outcome| text_is(&outcome, b"object")),
        7 => evaluate(storage, b"(Promise.resolve(1)) instanceof Promise")
            .is_some_and(|outcome| text_is(&outcome, b"true")),

        // A reaction runs as a job, never during the call that attached it.
        8 => evaluate(storage, b"Promise.resolve(41).then()").is_some_and(|outcome| {
            outcome.state == Some(promise::FULFILLED) && outcome.settled == Some(41.0)
        }),
        9 => evaluate(storage, b"Promise.reject(7).then()").is_some_and(|outcome| {
            outcome.state == Some(promise::REJECTED) && outcome.settled == Some(7.0)
        }),
        10 => evaluate(storage, b"Promise.resolve(1).then().then()").is_some_and(|outcome| {
            outcome.state == Some(promise::FULFILLED) && outcome.settled == Some(1.0)
        }),
        11 => evaluate(storage, b"Promise.resolve(2).then().then().then()")
            .is_some_and(|outcome| outcome.settled == Some(2.0)),

        // A promise whose executor settles nothing stays pending.
        12 => evaluate(storage, b"new Promise(Promise.resolve)")
            .is_some_and(|outcome| outcome.state == Some(promise::PENDING)),

        // Misuse throws rather than doing something surprising.
        13 => evaluate(storage, b"new Promise(1)").is_some_and(|outcome| outcome.threw),
        14 => evaluate(storage, b"Promise.prototype.then.call")
            .is_some_and(|outcome| outcome.value.is_some()),
        15 => {
            evaluate(storage, b"({}).then").is_some_and(|outcome| text_is(&outcome, b"undefined"))
        }
        // A promise resolved with a promise waits for it rather than holding
        // it, which is what makes a chain a chain.
        16 => evaluate(storage, b"Promise.resolve(Promise.resolve(7))")
            .is_some_and(|outcome| outcome.settled == Some(7.0)),
        17 => evaluate(
            storage,
            b"Promise.resolve(1).then(function (v) { return Promise.resolve(v + 1); })",
        )
        .is_some_and(|outcome| outcome.settled == Some(2.0)),
        // An ordinary object with a callable `then` is followed the same way.
        18 => evaluate(
            storage,
            b"Promise.resolve({ then: function (resolve) { resolve(9); } })",
        )
        .is_some_and(|outcome| outcome.settled == Some(9.0)),
        // A thenable that throws rejects the promise that adopted it.
        19 => evaluate(
            storage,
            b"Promise.resolve({ then: function () { throw 3; } })",
        )
        .is_some_and(|outcome| outcome.state == Some(promise::REJECTED)),
        // A promise resolved with itself can never settle, so it rejects.
        20 => evaluate(
            storage,
            b"var settle; var p = new Promise(function (r) { settle = r; }); settle(p); p",
        )
        .is_some_and(|outcome| outcome.state == Some(promise::REJECTED)),

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
            b"phasor-promise-probe: 21 passed\n"
        } else {
            let prefix = b"phasor-promise-probe: failed at ";
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
