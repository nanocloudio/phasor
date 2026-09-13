//! The shell: a person's way into the engine from a terminal.
//!
//! `fluxor exec phasor -- …`, or `phasor …` through the busybox link, runs
//! this applet over the `cli` stack. A script arrives on standard input and
//! runs once; `-e` runs the expression on the command line; `-i` reads a line
//! at a time and keeps one realm alive between them, which is a REPL. Every
//! input is one bounded task under the same policy an isolate runs, so a
//! runaway loop ends as `fuel-exhausted` here exactly as it would deployed.
//!
//! Nothing is ambient. The realm starts bare: no clock, no randomness, no
//! storage, no filesystem, no network. Each `--grant` admits one interface —
//! `clock`, `entropy`, `store`, `fs`, or `net` — as a namespace of the
//! members that interface declares, answered by the adapter the shell's graph
//! wires directly to a port of its own, and the shell says what it granted.
//! `print` is the one host function the shell installs, because standard
//! output is the shell's authority to give.

#![cfg_attr(not(feature = "host-test"), no_std)]
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

#[macro_use]
#[path = "../../common/entry.rs"]
mod entry;

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;
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

use arena::Node;
use binding::{
    Answer, Binding, CallRecord, Cause, CompletionRecord, Disposition, Pending, CALL_FRAME,
    COMPLETION_FRAME,
};
use bytecode::{Constant, ExceptionRegion, ExportRecord, Function, ImportRecord, Unit};
use diagnostic::{code, Diagnostic, Severity};
use emit::Patch;
use evalsite::EvalBinding;
use frontend::{EvalGoal, Goal};
use heap::{Heap, Slot};
use job::Job;
use lower::{Binding as LexicalBinding, Pending as PendingFunction, Scope};
use policy::Policy;
use realm::Realm;
use regexp::Choice;
use source::{Limits, LineStart};
use value::{Handle, Value};
use vm::{Compiled, Completion, EvalRequest, Frame, ModuleInstance, Progress, Saves, Vm};

// ---------------------------------------------------------------- the edge

/// Steps to wait for the argv record before running with none: `cli_in`
/// emits it early, and an empty argv never arrives at all.
const ARGV_WAIT: u32 = 2000;
const ARGV_CAPACITY: usize = 8192;
const MAX_ARGV: usize = 32;
/// Bytes of standard input staged per step.
const INPUT_CHUNK: usize = 4096;
/// One line being edited in the REPL.
const LINE_CAPACITY: usize = 8192;
/// Text waiting to leave on standard output.
const OUT_CAPACITY: usize = 64 * 1024;
/// What `print` may write between two steps: the shell's quota on output.
const PRINT_CAPACITY: usize = 64 * 1024;

// ------------------------------------------------------------- the front end

/// A script, or the REPL's lines gathered until they parse.
const SOURCE_CAPACITY: usize = 1024 * 1024;
/// The standard surface's source, as the surface module writes it.
const SURFACE_CAPACITY: usize = 128 * 1024;
const LINE_STARTS: usize = 16384;
const NODE_CAPACITY: usize = 131072;
const LIST_CAPACITY: usize = 131072;
const NUMBER_CAPACITY: usize = 8192;
const SCRATCH_CAPACITY: usize = 65536;
const CODE_CAPACITY: usize = 512 * 1024;
const IMAGE_CAPACITY: usize = 512 * 1024;
const CONSTANT_CAPACITY: usize = 16384;
const DATA_CAPACITY: usize = 512 * 1024;
const POINT_CAPACITY: usize = 32768;
const PATCH_CAPACITY: usize = 8192;
const LABEL_CAPACITY: usize = 8192;
const VERIFIER_CAPACITY: usize = 512 * 1024;
const UNIT_CODE_CAPACITY: usize = 768 * 1024;
const UNIT_POINT_CAPACITY: usize = 32768;
const FUNCTION_CAPACITY: usize = 2048;
const EXCEPTION_CAPACITY: usize = 2048;
const SCOPE_CAPACITY: usize = 4096;
const LEXICAL_CAPACITY: usize = 32768;
const PENDING_FUNCTIONS: usize = 2048;
const IMPORT_CAPACITY: usize = 64;
const EXPORT_CAPACITY: usize = 64;
const EVAL_SITE_CAPACITY: usize = 2048;
/// Lexer fuel for one compile.
const FUEL: u32 = 40_000_000;

/// Every unit a session makes — each input, each `eval` — lives here for
/// the session's life, because a closure made by an earlier input still
/// runs its code.
const IMAGE_REGION: usize = 4 * 1024 * 1024;
const MAX_UNITS: usize = 256;
/// The source of one `eval`, as UTF-8.
const EVAL_SOURCE: usize = 64 * 1024;

// ------------------------------------------------------------- the machine

const ARENA_BYTES: usize = 16 * 1024 * 1024;
const SLOT_COUNT: usize = 262_144;
const WORKLIST: usize = 4096;
const ATOM_ENTRIES: usize = 65536;
const ATOM_HANDLES: usize = 49152;
const FRAME_COUNT: usize = 1024;
const REGISTER_COUNT: usize = 16384;
const ROOT_COUNT: usize = 65536;
const JOB_COUNT: usize = 1024;
const CHOICE_COUNT: usize = 4096;
const UNDO_COUNT: usize = 4096;
const SUBJECT_UNITS: usize = 65536;
const COLLECTION_SLICE: u32 = 4096;
const COLLECTION_HEADROOM: u32 = (ARENA_BYTES / 4) as u32;

/// Instructions one input may run unless `--steps` says otherwise.
const STEPS_DEFAULT: u64 = 100_000_000;
/// Instructions per module step.
const SLICE: u64 = 1 << 14;
const JOB_SLICE: u32 = 64;
/// Idle steps with a call outstanding before the shell times it out.
const CALL_WAIT: u32 = 20_000;
const TRACE: u64 = 0x5041_5348_4f52_0002;

// ------------------------------------------------------------- the grants

/// The interfaces a person may grant. Each is a port pair on this module and
/// an adapter the graph wires to it, and each carries members: the bindings
/// the deployment admits, one per operation the interface offers.
const MAX_GRANTS: usize = 6;
/// Bindings across every granted interface.
const MAX_BINDINGS: usize = 16;
/// Members one interface may carry.
const MAX_MEMBERS: usize = 8;
const GRANT_CLOCK: u8 = 1;
const GRANT_ENTROPY: u8 = 2;
const GRANT_STORE: u8 = 3;
const GRANT_FS: u8 = 4;
const GRANT_NET: u8 = 5;
const GRANT_HTTP: u8 = 6;
const PENDING_COUNT: usize = 16;
const IN_FLIGHT_MAX: u32 = 8;
/// Resources a program may hold open at once.
const RESOURCE_COUNT: usize = 16;
/// Bytes of payload staged behind the call records of one step.
const CALL_PAYLOAD_BYTES: usize = 32 * 1024;
/// Bytes of payload one answer may carry.
const REPLY_PAYLOAD_BYTES: usize = 8 * 1024;

/// One member of an interface: the name a program calls it by, how it is
/// served, and the method number the adapter knows it as.
///
/// The name is held inline rather than borrowed: a module image takes no
/// relocations, so a table of static references would not survive loading.
#[derive(Clone, Copy)]
struct Member {
    name: [u8; 8],
    name_length: usize,
    class: binding::Class,
    method: u32,
}

impl Member {
    const EMPTY: Self = Self {
        name: [0; 8],
        name_length: 0,
        class: binding::Class::Async,
        method: 0,
    };

    const fn new(name: [u8; 8], name_length: usize, class: binding::Class, method: u32) -> Self {
        Self {
            name,
            name_length,
            class,
            method,
        }
    }

    fn name(&self) -> &[u8] {
        self.name.get(..self.name_length).unwrap_or(&[])
    }
}

/// The members of each interface, which are the bindings a grant admits.
///
/// A clock is a fact supplied at a task boundary, so its member is a snapshot
/// and a program reads it with no call. Everything else is a call.
fn members(kind: u8) -> ([Member; MAX_MEMBERS], usize) {
    let mut out = [Member::EMPTY; MAX_MEMBERS];
    let count = match kind {
        GRANT_CLOCK => {
            out[0] = Member::new(*b"now\0\0\0\0\0", 3, binding::Class::Snapshot, 0);
            // Waiting is a call: the adapter holds it until the delay has
            // passed, and the promise it answers is what a timer is built on.
            out[1] = Member::new(*b"sleep\0\0\0", 5, binding::Class::Async, 1);
            2
        }
        GRANT_ENTROPY => {
            out[0] = Member::new(*b"random\0\0", 6, binding::Class::Async, 0);
            1
        }
        GRANT_HTTP => {
            out[0] = Member::new(*b"send\0\0\0\0", 4, binding::Class::Async, 0);
            out[1] = Member::new(*b"status\0\0", 6, binding::Class::Async, 1);
            out[2] = Member::new(*b"headers\0", 7, binding::Class::Async, 2);
            out[3] = Member::new(*b"read\0\0\0\0", 4, binding::Class::Async, 3);
            out[4] = Member::new(*b"close\0\0\0", 5, binding::Class::Async, 4);
            out[5] = Member::new(*b"origin\0\0", 6, binding::Class::Async, 5);
            6
        }
        GRANT_NET => {
            out[0] = Member::new(*b"connect\0", 7, binding::Class::Async, 0);
            out[1] = Member::new(*b"send\0\0\0\0", 4, binding::Class::Async, 1);
            out[2] = Member::new(*b"receive\0", 7, binding::Class::Async, 2);
            out[3] = Member::new(*b"close\0\0\0", 5, binding::Class::Async, 3);
            out[4] = Member::new(*b"endpoint", 8, binding::Class::Async, 4);
            5
        }
        GRANT_FS => {
            out[0] = Member::new(*b"read\0\0\0\0", 4, binding::Class::Async, 0);
            out[1] = Member::new(*b"write\0\0\0", 5, binding::Class::Async, 1);
            out[2] = Member::new(*b"open\0\0\0\0", 4, binding::Class::Async, 4);
            out[3] = Member::new(*b"readAt\0\0", 6, binding::Class::Async, 5);
            out[4] = Member::new(*b"close\0\0\0", 5, binding::Class::Async, 6);
            out[5] = Member::new(*b"size\0\0\0\0", 4, binding::Class::Async, 7);
            6
        }
        GRANT_STORE => {
            out[0] = Member::new(*b"read\0\0\0\0", 4, binding::Class::Async, 0);
            out[1] = Member::new(*b"write\0\0\0", 5, binding::Class::Async, 1);
            out[2] = Member::new(*b"list\0\0\0\0", 4, binding::Class::Async, 2);
            out[3] = Member::new(*b"delete\0\0", 6, binding::Class::Async, 3);
            out[4] = Member::new(*b"open\0\0\0\0", 4, binding::Class::Async, 4);
            out[5] = Member::new(*b"readAt\0\0", 6, binding::Class::Async, 5);
            6
        }
        _ => 0,
    };
    (out, count)
}

/// How the shell was asked to run.
const MODE_SCRIPT: u8 = 0;
const MODE_EVAL: u8 = 1;
const MODE_REPL: u8 = 2;

/// Where the module is.
const PHASE_ARGS: u8 = 0;
const PHASE_RUN: u8 = 1;
const PHASE_FINISH: u8 = 2;
const PHASE_DONE: u8 = 3;

const HELP: &[u8] = b"phasor: a bounded JavaScript shell on Fluxor\n\
\n\
usage:\n\
  phasor                 run the script on standard input\n\
  phasor -e <source>     run the source given\n\
  phasor -i              read a line at a time, one realm throughout\n\
  phasor help            this text\n\
\n\
options:\n\
  --grant clock          admit clock.now/sleep: the time, and being told later\n\
  --grant entropy        admit entropy.random(): a promise of a random number\n\
  --grant store          admit store.read/write/list/delete/open/readAt\n\
  --grant fs             admit fs.read/write/open/readAt/close/size\n\
  --grant net            admit net.connect/send/receive/close/endpoint\n\
  --grant http           admit http.send/status/headers/read/close/origin\n\
  --steps <n>            instructions one input may run, up to the ceiling\n\
  --bare                 leave out the standard surface the graph offers\n\
\n\
The value of the last expression is printed; print(x) writes a line.\n\
Nothing is ambient: a program has only what was granted.\n";

/// What is waiting to leave on standard output.
#[repr(C)]
struct Out {
    bytes: [u8; OUT_CAPACITY],
    staged: usize,
    written: usize,
}

/// The call frames staged for one grant's port, and the reply taken from
/// the other.
#[repr(C)]
struct GrantWire {
    calls: [u8; (CALL_FRAME + REPLY_PAYLOAD_BYTES) * 2],
    staged: usize,
    written: usize,
    reply: [u8; COMPLETION_FRAME],
    filled: usize,
    /// The bytes the answer carries, and how many have arrived.
    reply_payload: [u8; REPLY_PAYLOAD_BYTES],
    reply_filled: usize,
    reply_length: usize,
    /// Whether the frame is whole and its payload is what remains.
    frame_ready: bool,
    ready: bool,
}

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    args_in: i32,
    stdout_out: i32,
    stdin_in: i32,
    clock_reply: i32,
    entropy_reply: i32,
    store_reply: i32,
    fs_reply: i32,
    net_reply: i32,
    http_reply: i32,
    surface_in: i32,
    exit_out: i32,
    clock_call: i32,
    entropy_call: i32,
    store_call: i32,
    fs_call: i32,
    net_call: i32,
    http_call: i32,

    mode: u8,
    phase: u8,
    waited: u32,
    steps: u64,
    /// The interfaces granted, as `GRANT_*` kinds.
    grants: [u8; MAX_GRANTS],
    grant_count: usize,
    /// For each admitted binding, the interface it belongs to and the method
    /// the adapter knows it as. This shell's own numbering is its own: what
    /// crosses to an adapter is the method its interface names.
    binding_kind: [u8; MAX_BINDINGS],
    binding_method: [u32; MAX_BINDINGS],
    binding_count: usize,
    wires: [GrantWire; MAX_GRANTS],
    /// Whether the façade has been compiled and run in this session's realm.
    /// It runs once, before anything a person typed.
    facade_done: bool,
    /// The surface's source, as the module that holds it wrote it, and how
    /// much has arrived. A graph that wires no surface has no façade, which
    /// is the same as asking for none.
    surface: [u8; SURFACE_CAPACITY],
    surface_length: usize,
    surface_overflowed: bool,
    surface_ready: bool,
    /// Whether the session runs without the façade at all.
    bare: bool,
    /// Whether the task in progress is the façade rather than a person's
    /// input, so its value is not echoed.
    running_facade: bool,
    /// Whether a fresh snapshot has been taken for the task about to run,
    /// whether one has been asked for, and what the last one said.
    snapshot_ready: bool,
    snapshot_asked: bool,
    snapshot_value: f64,

    arec: [u8; ARGV_CAPACITY],
    arec_length: usize,
    input: [u8; INPUT_CHUNK],
    /// How much of `input` holds bytes, and how far they have been read: a
    /// chunk may carry several lines, taken one at a time.
    input_length: usize,
    input_at: usize,
    line: [u8; LINE_CAPACITY],
    line_length: usize,
    /// The whole script, or the REPL's lines gathered until they parse.
    source: [u8; SOURCE_CAPACITY],
    source_length: usize,
    source_complete: bool,
    source_overflow: bool,
    input_closed: bool,
    prompt_due: bool,
    /// The lines gathered so far did not parse to their end: the next line
    /// continues them.
    continuing: bool,

    out: Out,
    printed: [u8; PRINT_CAPACITY],
    printed_length: usize,

    // The front end's storage.
    starts: [LineStart; LINE_STARTS],
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
    pending: [PendingFunction; PENDING_FUNCTIONS],
    imports: [ImportRecord; IMPORT_CAPACITY],
    exports: [ExportRecord; EXPORT_CAPACITY],
    eval_sites: [u8; EVAL_SITE_CAPACITY],

    // Every unit of the session, in the order it was made.
    images: [u8; IMAGE_REGION],
    offsets: [(usize, usize); MAX_UNITS],
    digests: [u64; MAX_UNITS],
    unit_count: usize,
    region_used: usize,
    eval_source: [u8; EVAL_SOURCE],

    // The machine's storage.
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
    descriptors: [Binding; MAX_BINDINGS],
    pending_calls: [Pending; PENDING_COUNT],
    outbox: [CallRecord; PENDING_COUNT],
    resources: [binding::Resource; RESOURCE_COUNT],
    call_payloads: [u8; CALL_PAYLOAD_BYTES],
    instances: [ModuleInstance; MAX_UNITS],

    realm: Realm,
    saves: Saves,
    /// The realm exists: a task has been brought up once.
    realm_made: bool,
    /// A task is in progress, and whether its body has run to its end.
    task_running: bool,
    task_starting: bool,
    body_done: bool,
    current_unit: u32,
    result_value: Value,
    /// Idle steps with a call outstanding.
    call_waited: u32,
    /// The last task failed: the exit status says so.
    failed: bool,
}

/// What one step of the task did.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Advance {
    Running,
    Done,
    Failed,
}

// ---------------------------------------------------------------- output

/// Copy `src` into `dst` when they are the same length, answering whether
/// they were: a copy whose lengths the compiler cannot prove equal would
/// carry a panic path, and a module image carries none.
fn copy_into(dst: &mut [u8], src: &[u8]) -> bool {
    if dst.len() != src.len() {
        return false;
    }
    dst.copy_from_slice(src);
    true
}

/// Append text to what leaves on standard output, answering whether it fit.
fn emit(out: &mut Out, bytes: &[u8]) -> bool {
    let at = out.staged;
    let Some(slot) = out.bytes.get_mut(at..at + bytes.len()) else {
        return false;
    };
    if !copy_into(slot, bytes) {
        return false;
    }
    out.staged = at + bytes.len();
    true
}

/// Room left on standard output before a write would not fit.
fn out_room(out: &Out) -> usize {
    out.bytes.len().saturating_sub(out.staged)
}

fn flush_out(out: &mut Out, port: i32, syscalls: &SyscallTable) -> bool {
    wire::push_staged(
        syscalls,
        port,
        &out.bytes,
        &mut out.staged,
        &mut out.written,
    );
    out.staged == 0
}

/// Render a diagnostic as the edge does: `phasor: <name> at <span>`.
fn emit_diagnostic(out: &mut Out, report: &Diagnostic) {
    let mut line = [0u8; 160];
    let mut length = 0usize;
    length += text::put_ascii(line.get_mut(length..).unwrap_or(&mut []), b"phasor: ");
    if matches!(report.severity(), Severity::Fatal) {
        length += text::put_ascii(line.get_mut(length..).unwrap_or(&mut []), b"fatal ");
    }
    if let Some(name) = diagnostic::name_of(report.code()) {
        length += text::put_ascii(line.get_mut(length..).unwrap_or(&mut []), name);
    } else {
        length += text::put_ascii(line.get_mut(length..).unwrap_or(&mut []), b"diagnostic ");
        length += text::put_u32(
            line.get_mut(length..).unwrap_or(&mut []),
            u32::from(report.code()),
        );
    }
    if report.code() < diagnostic::termination::FUEL_EXHAUSTED {
        length += text::put_ascii(line.get_mut(length..).unwrap_or(&mut []), b" at ");
        length += text::put_u32(line.get_mut(length..).unwrap_or(&mut []), report.offset());
        if report.length() > 0 {
            length += text::put_ascii(line.get_mut(length..).unwrap_or(&mut []), b"..");
            length += text::put_u32(
                line.get_mut(length..).unwrap_or(&mut []),
                report.offset().saturating_add(report.length()),
            );
        }
    }
    length += text::put_ascii(line.get_mut(length..).unwrap_or(&mut []), b"\n");
    emit(out, line.get(..length).unwrap_or(&[]));
}

/// Write a string on the heap to standard output as UTF-8, answering
/// whether it fit.
fn emit_string(out: &mut Out, machine: &Vm<'_, '_, '_, '_>, text: Handle) -> bool {
    let count = string::length(machine.heap(), text).unwrap_or(0);
    let mut index = 0u32;
    let start = out.staged;
    while index < count {
        let unit = string::unit_at(machine.heap(), text, index)
            .ok()
            .flatten()
            .unwrap_or(0);
        let low = if index + 1 < count {
            string::unit_at(machine.heap(), text, index + 1)
                .ok()
                .flatten()
                .unwrap_or(0)
        } else {
            0
        };
        let paired = (0xD800..=0xDBFF).contains(&unit) && (0xDC00..=0xDFFF).contains(&low);
        let code_point = if paired {
            0x1_0000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(low) - 0xDC00)
        } else {
            u32::from(unit)
        };
        let mut bytes = [0u8; 4];
        let needed = if code_point < 0x80 {
            bytes[0] = code_point as u8;
            1
        } else if code_point < 0x800 {
            bytes[0] = 0xC0 | (code_point >> 6) as u8;
            bytes[1] = 0x80 | (code_point & 0x3F) as u8;
            2
        } else if code_point < 0x1_0000 {
            bytes[0] = 0xE0 | (code_point >> 12) as u8;
            bytes[1] = 0x80 | ((code_point >> 6) & 0x3F) as u8;
            bytes[2] = 0x80 | (code_point & 0x3F) as u8;
            3
        } else {
            bytes[0] = 0xF0 | (code_point >> 18) as u8;
            bytes[1] = 0x80 | ((code_point >> 12) & 0x3F) as u8;
            bytes[2] = 0x80 | ((code_point >> 6) & 0x3F) as u8;
            bytes[3] = 0x80 | (code_point & 0x3F) as u8;
            4
        };
        if !emit(out, &bytes[..needed]) {
            out.staged = start;
            return false;
        }
        index += if paired { 2 } else { 1 };
    }
    true
}

/// Write a value as `String(value)` on a line of its own.
fn emit_value(out: &mut Out, machine: &mut Vm<'_, '_, '_, '_>, value: Value) -> bool {
    let Ok(text) = machine.display(value) else {
        return emit(out, b"[value]\n");
    };
    emit_string(out, machine, text) && emit(out, b"\n")
}

/// Write an uncaught throw: `Uncaught <String(reason)>`.
fn emit_throw(out: &mut Out, machine: &mut Vm<'_, '_, '_, '_>, reason: Value) {
    let start = out.staged;
    if !(emit(out, b"Uncaught ") && emit_value(out, machine, reason)) {
        out.staged = start;
        emit(out, b"Uncaught [value]\n");
    }
}

// ---------------------------------------------------------------- argv

/// Split the NUL-separated argv record into spans.
fn split_argv(record: &[u8], spans: &mut [(usize, usize); MAX_ARGV]) -> usize {
    let mut count = 0usize;
    let mut start = 0usize;
    let mut at = 0usize;
    while at <= record.len() && count < MAX_ARGV {
        if at == record.len() || record.get(at).copied() == Some(0) {
            if at > start {
                spans[count] = (start, at);
                count += 1;
            }
            start = at + 1;
        }
        at += 1;
    }
    count
}

/// A decimal argument, or nothing.
fn parse_decimal(text: &[u8]) -> Option<u64> {
    if text.is_empty() {
        return None;
    }
    let mut value = 0u64;
    for &byte in text {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
    }
    Some(value)
}

/// What reading the command line decided.
enum Parsed {
    Run,
    Help,
    /// A refusal, already written out.
    Refused,
}

fn read_argv(state: &mut State) -> Parsed {
    let mut spans = [(0usize, 0usize); MAX_ARGV];
    let mut record = [0u8; ARGV_CAPACITY];
    let length = state.arec_length.min(ARGV_CAPACITY);
    copy_into(
        record.get_mut(..length).unwrap_or(&mut []),
        state.arec.get(..length).unwrap_or(&[]),
    );
    let count = split_argv(&record[..length], &mut spans);
    let mut index = 0usize;
    while index < count {
        let (start, end) = spans.get(index).copied().unwrap_or((0, 0));
        let word = record.get(start..end).unwrap_or(&[]);
        let next = if index + 1 < count {
            let (s, e) = spans.get(index + 1).copied().unwrap_or((0, 0));
            Some(record.get(s..e).unwrap_or(&[]))
        } else {
            None
        };
        match word {
            b"help" | b"--help" | b"-h" => return Parsed::Help,
            b"-i" | b"repl" => state.mode = MODE_REPL,
            b"--bare" => state.bare = true,
            b"-e" | b"eval" => {
                let Some(source) = next else {
                    emit(&mut state.out, b"phasor: -e needs a source\n");
                    return Parsed::Refused;
                };
                if source.len() > SOURCE_CAPACITY {
                    emit(&mut state.out, b"phasor: source-too-large\n");
                    return Parsed::Refused;
                }
                let length = source.len();
                if !copy_into(state.source.get_mut(..length).unwrap_or(&mut []), source) {
                    emit(&mut state.out, b"phasor: source-too-large\n");
                    return Parsed::Refused;
                }
                state.source_length = length;
                state.source_complete = true;
                state.mode = MODE_EVAL;
                index += 1;
            }
            b"--grant" => {
                let Some(kind) = next.and_then(grant_of) else {
                    emit(
                        &mut state.out,
                        b"phasor: --grant takes clock, entropy, store, fs, net, or http\n",
                    );
                    return Parsed::Refused;
                };
                let held = state.grants.get(..state.grant_count).unwrap_or(&[]);
                if !held.contains(&kind) {
                    let Some(slot) = state.grants.get_mut(state.grant_count) else {
                        emit(&mut state.out, b"phasor: too many grants\n");
                        return Parsed::Refused;
                    };
                    *slot = kind;
                    state.grant_count += 1;
                }
                index += 1;
            }
            b"--steps" => {
                let Some(steps) = next.and_then(parse_decimal) else {
                    emit(&mut state.out, b"phasor: --steps takes a number\n");
                    return Parsed::Refused;
                };
                state.steps = steps.max(1);
                index += 1;
            }
            _ => {
                emit(&mut state.out, b"phasor: unknown argument: ");
                emit(&mut state.out, word);
                emit(&mut state.out, b"\n");
                return Parsed::Refused;
            }
        }
        index += 1;
    }
    Parsed::Run
}

// ------------------------------------------------------------ the policy

/// The limits every input runs under: the storage as declared, and the
/// fuel `--steps` named, clamped to the ceiling.
fn policy(state: &State) -> Policy {
    Policy {
        heap_bytes: ARENA_BYTES as u32,
        heap_cells: SLOT_COUNT as u32,
        fuel: state.steps,
        frames: FRAME_COUNT as u32,
        registers: REGISTER_COUNT as u32,
        jobs: JOB_COUNT as u32,
        pending_calls: PENDING_COUNT as u32,
        image_bytes: IMAGE_REGION as u32,
        deadline_ms: 0,
        collection_slice: COLLECTION_SLICE,
    }
    .clamped()
}

/// The identifier a deployment grants an interface by, which is what the
/// binding digests are taken over.
///
/// It follows the WebAssembly System Interface where an interface for the
/// thing exists, so an adapter written to it means something outside this
/// project; where none does, the name is this project's own and says so.
/// `docs/reference/capability-register.md` is the register.
fn interface_id(kind: u8) -> ([u8; capability::MAX_INTERFACE], usize) {
    match kind {
        GRANT_CLOCK => (*b"wasi:clocks/wall-clock\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0", 22),
        GRANT_ENTROPY => (*b"wasi:random/random\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0", 18),
        GRANT_FS => (*b"wasi:filesystem/types\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0", 21),
        GRANT_HTTP => (*b"wasi:http/outgoing-handler\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0", 26),
        GRANT_NET => (*b"wasi:sockets/tcp\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0", 16),
        GRANT_STORE => (*b"phasor:store/keyvalue\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0", 21),
        _ => ([0; capability::MAX_INTERFACE], 0),
    }
}

/// The name a program calls an interface by, which is the namespace it finds
/// in the realm. Held inline: a match returning borrowed literals becomes a
/// switch table of pointers, and a module image takes no relocations for one.
fn grant_name(kind: u8) -> ([u8; 8], usize) {
    match kind {
        GRANT_CLOCK => (*b"clock\0\0\0", 5),
        GRANT_ENTROPY => (*b"entropy\0", 7),
        GRANT_STORE => (*b"store\0\0\0", 5),
        GRANT_FS => (*b"fs\0\0\0\0\0\0", 2),
        GRANT_NET => (*b"net\0\0\0\0\0", 3),
        GRANT_HTTP => (*b"http\0\0\0\0", 4),
        _ => ([0; 8], 0),
    }
}

/// The interface a grant names on the command line.
fn grant_of(word: &[u8]) -> Option<u8> {
    match word {
        b"clock" => Some(GRANT_CLOCK),
        b"entropy" => Some(GRANT_ENTROPY),
        b"store" => Some(GRANT_STORE),
        b"fs" => Some(GRANT_FS),
        b"net" => Some(GRANT_NET),
        b"http" => Some(GRANT_HTTP),
        _ => None,
    }
}

// ------------------------------------------------------- the front end

/// Compile the gathered source as a script into a new unit of the session,
/// answering its index, or the diagnostic that refused it.
fn compile_input(state: &mut State) -> Result<u32, Diagnostic> {
    let length = state.source_length;
    // SAFETY: the source is read once and never written while the front end
    // runs, and the front end's storage is every other field it borrows.
    let source: &[u8] = unsafe { core::slice::from_raw_parts(state.source.as_ptr(), length) };
    compile_bytes(state, source)
}

/// Compile a source into a new unit of the session, answering its index.
fn compile_bytes(state: &mut State, source: &[u8]) -> Result<u32, Diagnostic> {
    let mut storage = frontend_storage!(state);
    let compiled = frontend::compile(source, Goal::Script, Limits::CEILING, FUEL, &mut storage)?;
    place_unit(state, compiled.length).ok_or(Diagnostic::at(
        code::COMPILE_BUDGET_EXHAUSTED,
        Severity::Fatal,
        0,
    ))
}

/// Move the image the front end left in `image` into the session's region,
/// answering the unit's index.
fn place_unit(state: &mut State, length: usize) -> Option<u32> {
    if state.unit_count >= MAX_UNITS {
        return None;
    }
    let at = state.region_used;
    let slot = state.images.get_mut(at..at + length)?;
    if !copy_into(slot, state.image.get(..length)?) {
        return None;
    }
    let index = state.unit_count;
    *state.offsets.get_mut(index)? = (at, at + length);
    *state.digests.get_mut(index)? = 0;
    state.unit_count = index + 1;
    state.region_used = at + length;
    u32::try_from(index).ok()
}

/// The machine's in-place compiler: the front end over its storage, the
/// free tail of the session's region, and the digests of what it made so a
/// repeated `eval` is one unit.
struct InPlace<'r, 'u, 's> {
    front: frontend::Storage<'s>,
    region: &'r mut &'u mut [u8],
    offsets: &'r mut [(usize, usize); MAX_UNITS],
    digests: &'r mut [u64; MAX_UNITS],
    count: &'r mut usize,
    used: &'r mut usize,
    source: &'r mut [u8; EVAL_SOURCE],
    /// Units attached to the machine before this compiler's own.
    base: usize,
}

fn in_place_compile<'u>(
    state: *mut c_void,
    heap: &Heap<'_>,
    request: &EvalRequest<'u>,
    units: &mut [Unit<'u>],
) -> Compiled {
    // SAFETY: the machine holds the pointer only while the compiler it was
    // attached with lives, in the same block, and calls it from one thread.
    let compiler = unsafe { &mut *state.cast::<InPlace<'_, 'u, '_>>() };
    compiler.compile(heap, request, units)
}

fn digest_of(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash ^ (bytes.len() as u64)
}

/// Stage a string on the heap as UTF-8, answering its length.
fn stage_source(heap: &Heap<'_>, handle: Handle, out: &mut [u8; EVAL_SOURCE]) -> Option<usize> {
    let count = string::length(heap, handle).ok()?;
    let mut at = 0usize;
    let mut index = 0u32;
    while index < count {
        let unit = string::unit_at(heap, handle, index).ok()??;
        let low = if index + 1 < count {
            string::unit_at(heap, handle, index + 1)
                .ok()
                .flatten()
                .unwrap_or(0)
        } else {
            0
        };
        let paired = (0xD800..=0xDBFF).contains(&unit) && (0xDC00..=0xDFFF).contains(&low);
        let code_point = if paired {
            0x1_0000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(low) - 0xDC00)
        } else {
            u32::from(unit)
        };
        let needed = if code_point < 0x80 {
            1
        } else if code_point < 0x800 {
            2
        } else if code_point < 0x1_0000 {
            3
        } else {
            4
        };
        let slot = out.get_mut(at..at + needed)?;
        match needed {
            1 => slot[0] = code_point as u8,
            2 => {
                slot[0] = 0xC0 | (code_point >> 6) as u8;
                slot[1] = 0x80 | (code_point & 0x3F) as u8;
            }
            3 => {
                slot[0] = 0xE0 | (code_point >> 12) as u8;
                slot[1] = 0x80 | ((code_point >> 6) & 0x3F) as u8;
                slot[2] = 0x80 | (code_point & 0x3F) as u8;
            }
            _ => {
                slot[0] = 0xF0 | (code_point >> 18) as u8;
                slot[1] = 0x80 | ((code_point >> 12) & 0x3F) as u8;
                slot[2] = 0x80 | ((code_point >> 6) & 0x3F) as u8;
                slot[3] = 0x80 | (code_point & 0x3F) as u8;
            }
        }
        at += needed;
        index += if paired { 2 } else { 1 };
    }
    Some(at)
}

impl<'u> InPlace<'_, 'u, '_> {
    fn compile(
        &mut self,
        heap: &Heap<'_>,
        request: &EvalRequest<'u>,
        units: &mut [Unit<'u>],
    ) -> Compiled {
        let Some(length) = stage_source(heap, request.source, self.source) else {
            return Compiled::Exhausted;
        };
        // The slot's identity is the source and its site: the same text at
        // another site, as a script, or in another realm is another unit.
        let mut digest = digest_of(self.source.get(..length).unwrap_or(&[]));
        if request.script {
            digest = digest.rotate_left(7) ^ 0x5C71;
        }
        digest ^= u64::from(request.realm).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        if let Some((module, function, pc)) = request.site {
            digest ^= digest_of(&module.to_le_bytes())
                .rotate_left(1)
                .wrapping_add(u64::from(function) << 32 | u64::from(pc));
        }
        let mut index = self.base;
        while index < *self.count {
            if self.digests.get(index).copied() == Some(digest) {
                return Compiled::Unit(index - self.base);
            }
            index += 1;
        }
        if *self.count >= MAX_UNITS || *self.count - self.base >= units.len() {
            return Compiled::Exhausted;
        }
        let mut scope = [EvalBinding {
            name: &[],
            slot: 0,
            depth: 0,
            kind: 0,
        }; 64];
        let mut goal = EvalGoal::INDIRECT;
        let mut scope_count = 0usize;
        if let (Some((_, function, pc)), Some(unit)) = (request.site, request.site_unit) {
            if let Some(site) = evalsite::find(unit.eval_sites(), function, pc) {
                goal.strict = site.flags & evalsite::FLAG_STRICT != 0;
                goal.function_site = site.flags & evalsite::FLAG_FUNCTION != 0;
                goal.super_property = site.flags & evalsite::FLAG_SUPER_PROPERTY != 0;
                goal.super_call = site.flags & evalsite::FLAG_SUPER_CALL != 0;
                goal.new_target = site.flags & evalsite::FLAG_NEW_TARGET != 0;
                goal.deny_arguments = site.flags & evalsite::FLAG_NO_ARGUMENTS != 0;
                goal.privates = site.flags & evalsite::FLAG_PRIVATES != 0;
                goal.parameter_site = site.flags & evalsite::FLAG_PARAMETERS != 0;
                goal.var_env_depth = site.var_depth;
                if site.flags & evalsite::FLAG_TRUNCATED == 0 {
                    scope_count = site.bindings(&mut scope);
                }
            }
        }
        goal.scope = scope.get(..scope_count).unwrap_or(&[]);
        let source = self.source.get(..length).unwrap_or(&[]);
        let goal = if request.script {
            Goal::Script
        } else {
            Goal::Eval(goal)
        };
        let Ok(compiled) = frontend::compile(source, goal, Limits::CEILING, FUEL, &mut self.front)
        else {
            return Compiled::Refused;
        };
        let image_length = compiled.length;
        let free = core::mem::take(self.region);
        if free.len() < image_length {
            *self.region = free;
            return Compiled::Exhausted;
        }
        let (head, rest) = free.split_at_mut(image_length);
        *self.region = rest;
        if !copy_into(
            head,
            self.front.lower.image.get(..image_length).unwrap_or(&[]),
        ) {
            return Compiled::Exhausted;
        }
        let Ok(unit) = Unit::parse(head) else {
            return Compiled::Refused;
        };
        let index = *self.count;
        let slot = index - self.base;
        let (Some(place), Some(offset), Some(kept)) = (
            units.get_mut(slot),
            self.offsets.get_mut(index),
            self.digests.get_mut(index),
        ) else {
            return Compiled::Exhausted;
        };
        *place = unit;
        *offset = (*self.used, *self.used + image_length);
        *kept = digest;
        *self.count = index + 1;
        *self.used += image_length;
        Compiled::Unit(slot)
    }
}

// ------------------------------------------------------------ the task

/// Bring the machine up over the session's storage and run one slice of the
/// current task.
fn advance(state: &mut State) -> Advance {
    let policy = policy(state);
    let grants = state.grants;
    let grant_count = state.grant_count;
    let snapshot_value = state.snapshot_value;
    let running_facade = state.running_facade;
    let starting = state.task_starting;
    let current = state.current_unit;
    let mode = state.mode;
    let first = !state.realm_made;
    let attached = state.unit_count.min(MAX_UNITS);
    let used = state.region_used.min(IMAGE_REGION);
    let (done, mut free) = state.images.split_at_mut(used);
    let done: &[u8] = done;
    let mut table = [Unit::EMPTY; MAX_UNITS];
    let mut index = 0usize;
    while index < attached {
        let (start, end) = state.offsets.get(index).copied().unwrap_or((0, 0));
        let (Some(image), Some(place)) = (done.get(start..end), table.get_mut(index)) else {
            return Advance::Failed;
        };
        let Ok(unit) = Unit::parse(image) else {
            return Advance::Failed;
        };
        *place = unit;
        index += 1;
    }
    let (units, extra) = table.split_at_mut(attached.min(MAX_UNITS));
    let units: &[Unit<'_>] = units;

    let mut compiler = InPlace {
        front: frontend_storage!(state),
        region: &mut free,
        offsets: &mut state.offsets,
        digests: &mut state.digests,
        count: &mut state.unit_count,
        used: &mut state.region_used,
        source: &mut state.eval_source,
        base: attached,
    };

    // One binding per member of each granted interface, named by the digest
    // of `<interface>#<member>`, which is what an image states it requires.
    let mut admitted = [Binding::EMPTY; MAX_BINDINGS];
    let mut count = 0usize;
    let mut grant = 0usize;
    while grant < grant_count {
        let kind = grants.get(grant).copied().unwrap_or(0);
        let (interface_members, member_count) = members(kind);
        let mut position = 0usize;
        while position < member_count {
            let member = interface_members[position];
            position += 1;
            let Some(slot) = admitted.get_mut(count) else {
                break;
            };
            // The binding's identity is the interface's identifier and the
            // member's name, computed by the same function an image's stated
            // requirement is: two implementations of one name are two names,
            // and a grant would never match the requirement it answers.
            let (id, id_length) = interface_id(kind);
            *slot = Binding {
                name: capability::name_of(id.get(..id_length).unwrap_or(&[]), member.name()),
                in_flight_max: IN_FLIGHT_MAX,
                in_flight: 0,
                class: member.class,
                snapshot: snapshot_value,
            };
            if let (Some(k), Some(m)) = (
                state.binding_kind.get_mut(count),
                state.binding_method.get_mut(count),
            ) {
                *k = kind;
                *m = member.method;
            }
            count += 1;
        }
        grant += 1;
    }
    state.binding_count = count;
    let binding_count = count;
    let attach = agent::Attachments {
        admitted: admitted.get(..binding_count).unwrap_or(&[]),
        modules: Some(agent::Closure {
            instances: state.instances.get_mut(..attached).unwrap_or(&mut []),
            imports: &[],
        }),
        module_names: &[],
        module_cycles: &[],
        compiler: Some(agent::InPlaceCompiler {
            state: (&mut compiler as *mut InPlace<'_, '_, '_>).cast::<c_void>(),
            compile: in_place_compile,
            units: extra,
        }),
        print: Some((&mut state.printed, &mut state.printed_length)),
    };
    let metering = agent::Metering {
        policy: &policy,
        collection_headroom: COLLECTION_HEADROOM,
        trace: TRACE,
    };
    let storage = agent::Storage {
        arena: &mut state.arena,
        slots: &mut state.slots,
        worklist: &mut state.worklist,
        entries: &mut state.entries,
        handles: &mut state.handles,
        frames: &mut state.frames,
        registers: &mut state.registers,
        roots: &mut state.roots,
        jobs: &mut state.jobs,
        choices: &mut state.choices,
        undo: &mut state.undo,
        subject: &mut state.subject,
        descriptors: &mut state.descriptors,
        pending: &mut state.pending_calls,
        outbox: &mut state.outbox,
        payloads: Some(&mut state.call_payloads),
        resources: Some(&mut state.resources),
    };
    let step = |machine: &mut Vm<'_, '_, '_, '_>| -> Advance {
        if first {
            // The realm's own additions: the grants, by name, and `print`.
            // Each granted interface is one namespace object holding the
            // members that were admitted. Nothing about it is privileged: it
            // is reachable because it was granted, and holds exactly what was.
            let mut index = 0u32;
            let mut grant = 0usize;
            while grant < grant_count {
                let kind = grants.get(grant).copied().unwrap_or(0);
                let (interface, interface_length) = grant_name(kind);
                let Ok(namespace) =
                    machine.define_namespace(interface.get(..interface_length).unwrap_or(&[]))
                else {
                    emit(&mut state.out, b"phasor: heap-exhausted\n");
                    return Advance::Failed;
                };
                let (interface_members, member_count) = members(kind);
                let mut position = 0usize;
                while position < member_count {
                    let member = interface_members[position];
                    position += 1;
                    if machine
                        .define_binding_in(namespace, member.name(), index)
                        .is_err()
                    {
                        emit(&mut state.out, b"phasor: heap-exhausted\n");
                        return Advance::Failed;
                    }
                    index += 1;
                }
                grant += 1;
            }
            if machine.install_print().is_err() {
                emit(&mut state.out, b"phasor: heap-exhausted\n");
                return Advance::Failed;
            }
        }
        if starting {
            state.task_starting = false;
            if machine.start_script(current).is_err() {
                emit(&mut state.out, b"phasor: stack-overflow\n");
                return Advance::Failed;
            }
        }
        machine.retain(state.result_value);

        let mut progressed = false;
        // Answers that arrived settle their promises before anything runs.
        let mut grant = 0usize;
        while grant < grant_count {
            let Some(wire) = state.wires.get_mut(grant) else {
                break;
            };
            if wire.ready {
                wire.ready = false;
                wire.filled = 0;
                let length = wire.reply_length;
                let mut payload = [0u8; REPLY_PAYLOAD_BYTES];
                copy_into(
                    payload.get_mut(..length).unwrap_or(&mut []),
                    wire.reply_payload.get(..length).unwrap_or(&[]),
                );
                wire.reply_length = 0;
                wire.reply_filled = 0;
                let reply = wire.reply;
                if let Some(record) = CompletionRecord::decode(&reply) {
                    let _ = machine
                        .apply_completion_with(&record, payload.get(..length).unwrap_or(&[]));
                    progressed = true;
                }
            }
            grant += 1;
        }

        let mut outcome = Advance::Running;
        if !state.body_done {
            match machine.resume(SLICE) {
                Progress::Running => {
                    progressed = true;
                    // The compiler is attached, so a pause means it had no
                    // room: the call throws the syntax error it would have.
                    if machine.pending_eval().is_some() {
                        match machine.fail_eval() {
                            None | Some(Completion::Value(_)) => {}
                            Some(Completion::Throw(reason)) => {
                                state.body_done = true;
                                emit_throw(&mut state.out, machine, reason);
                                outcome = Advance::Failed;
                            }
                            Some(Completion::Terminated(reason)) => {
                                state.body_done = true;
                                emit_diagnostic(
                                    &mut state.out,
                                    &Diagnostic::at(reason.code(), Severity::Error, 0),
                                );
                                outcome = Advance::Failed;
                            }
                        }
                    }
                }
                Progress::Finished(Completion::Value(value)) => {
                    progressed = true;
                    state.body_done = true;
                    state.result_value = value;
                    machine.retain(value);
                }
                Progress::Finished(Completion::Throw(reason)) => {
                    state.body_done = true;
                    emit_throw(&mut state.out, machine, reason);
                    outcome = Advance::Failed;
                }
                Progress::Finished(Completion::Terminated(reason)) => {
                    state.body_done = true;
                    emit_diagnostic(
                        &mut state.out,
                        &Diagnostic::at(reason.code(), Severity::Error, 0),
                    );
                    outcome = Advance::Failed;
                }
            }
        }

        if outcome == Advance::Running {
            match machine.run_jobs(JOB_SLICE) {
                Ok(ran) => progressed |= ran > 0,
                Err(Completion::Throw(reason)) => {
                    emit_throw(&mut state.out, machine, reason);
                    outcome = Advance::Failed;
                }
                Err(Completion::Terminated(reason)) => {
                    emit_diagnostic(
                        &mut state.out,
                        &Diagnostic::at(reason.code(), Severity::Error, 0),
                    );
                    outcome = Advance::Failed;
                }
                Err(Completion::Value(_)) => {}
            }
        }

        // What `print` wrote leaves with everything else.
        let printed = machine.printed();
        if !printed.is_empty() && out_room(&state.out) >= printed.len() {
            emit(&mut state.out, printed);
            machine.take_printed();
        }

        // Calls leave on the port of the interface they were made on, each
        // followed by its payload. What crosses is the method the interface
        // names, not this shell's own binding numbering.
        if outcome == Advance::Running && !machine.calls().is_empty() {
            let mut copied = [0u8; CALL_PAYLOAD_BYTES];
            let payloads = machine.call_payloads();
            let taken = payloads.len().min(copied.len());
            copy_into(
                copied.get_mut(..taken).unwrap_or(&mut []),
                payloads.get(..taken).unwrap_or(&[]),
            );
            let mut payload_at = 0usize;
            let mut staged_any = false;
            for record in machine.calls() {
                let index = record.binding as usize;
                let kind = state.binding_kind.get(index).copied().unwrap_or(0);
                let method = state.binding_method.get(index).copied().unwrap_or(0);
                let length = record.payload_length as usize;
                let payload = copied.get(payload_at..payload_at + length).unwrap_or(&[]);
                payload_at += length;
                let mut forwarded = *record;
                forwarded.binding = method;
                let frame = forwarded.encode();
                let Some(wire) = wire_of(&mut state.wires, grants, grant_count, kind) else {
                    continue;
                };
                let at = wire.staged;
                if !copy_into(
                    wire.calls.get_mut(at..at + CALL_FRAME).unwrap_or(&mut []),
                    &frame,
                ) || !copy_into(
                    wire.calls
                        .get_mut(at + CALL_FRAME..at + CALL_FRAME + length)
                        .unwrap_or(&mut []),
                    payload,
                ) {
                    continue;
                }
                wire.staged = at + CALL_FRAME + length;
                staged_any = true;
            }
            if staged_any {
                machine.take_calls();
                progressed = true;
            }
        }

        // A provider that never answers must not hold the shell open.
        if outcome == Advance::Running && !progressed && machine.in_flight() > 0 {
            state.call_waited = state.call_waited.saturating_add(1);
            if state.call_waited > CALL_WAIT {
                let mut requests = [0u64; PENDING_COUNT];
                let count = machine.outstanding(&mut requests);
                for &request in requests.get(..count).unwrap_or(&[]) {
                    let _ = machine.apply_completion(&CompletionRecord {
                        request,
                        disposition: Disposition::Rejected,
                        cause: Cause::Timeout,
                        trace: TRACE,
                        answer: Answer::None,
                    });
                }
                state.call_waited = 0;
            }
        } else if progressed {
            state.call_waited = 0;
        }

        // The input is done when its body has run, its value is settled, and
        // no reaction is left to run.
        if outcome == Advance::Running && state.body_done && machine.pending_jobs() == 0 {
            let value = state.result_value;
            let settled = if value.is_object()
                && object::is_promise(machine.heap(), value.as_handle()) == Ok(true)
            {
                match object::promise_state(machine.heap(), value.as_handle()) {
                    Ok(promise::FULFILLED) => {
                        Some(Ok(object::promise_value(machine.heap(), value.as_handle())
                            .unwrap_or(Value::UNDEFINED)))
                    }
                    Ok(promise::REJECTED) => Some(Err(object::promise_value(
                        machine.heap(),
                        value.as_handle(),
                    )
                    .unwrap_or(Value::UNDEFINED))),
                    _ => None,
                }
            } else {
                Some(Ok(value))
            };
            match settled {
                None => {}
                Some(Ok(value)) => {
                    // A script's completion is printed when it says something;
                    // the REPL echoes every value, as a REPL does.
                    if !running_facade && (mode == MODE_REPL || !value.is_undefined()) {
                        emit_value(&mut state.out, machine, value);
                    }
                    outcome = Advance::Done;
                }
                Some(Err(reason)) => {
                    emit_throw(&mut state.out, machine, reason);
                    outcome = Advance::Failed;
                }
            }
        }
        outcome
    };

    let outcome = if first {
        match agent::fresh(units, storage, attach, metering, step) {
            Ok((outcome, realm, saves)) => {
                state.realm = realm;
                state.saves = saves;
                state.realm_made = true;
                outcome
            }
            Err(_) => {
                emit(&mut state.out, b"phasor: heap-exhausted\n");
                Advance::Failed
            }
        }
    } else {
        let realm = state.realm;
        let saves = state.saves;
        match agent::adopt(units, storage, attach, metering, realm, &saves, step) {
            Ok((outcome, saves)) => {
                state.saves = saves;
                outcome
            }
            Err(_) => {
                emit(&mut state.out, b"phasor: heap-exhausted\n");
                Advance::Failed
            }
        }
    };
    if outcome != Advance::Running {
        state.failed = outcome == Advance::Failed;
        state.task_running = false;
        state.body_done = true;
        state.result_value = Value::UNDEFINED;
    }
    outcome
}

/// Compile the gathered source and start it as the next task. Answers
/// whether a task started; a source that does not compile says why.
fn begin_task(state: &mut State, unit: u32) {
    state.current_unit = unit;
    state.task_starting = true;
    state.task_running = true;
    state.body_done = false;
    state.call_waited = 0;
    state.result_value = Value::UNDEFINED;
}

fn start_input(state: &mut State) -> bool {
    match compile_input(state) {
        Ok(unit) => {
            begin_task(state, unit);
            true
        }
        Err(report) => {
            emit_diagnostic(&mut state.out, &report);
            state.failed = true;
            false
        }
    }
}

// ---------------------------------------------------------- the ports

/// The wire an interface's calls leave on, by the grant's position.
fn wire_of(
    wires: &mut [GrantWire; MAX_GRANTS],
    grants: [u8; MAX_GRANTS],
    grant_count: usize,
    kind: u8,
) -> Option<&mut GrantWire> {
    let mut grant = 0usize;
    while grant < grant_count {
        if grants.get(grant).copied() == Some(kind) {
            return wires.get_mut(grant);
        }
        grant += 1;
    }
    None
}

/// The ports an interface's calls and answers use.
fn ports_of(state: &State, kind: u8) -> (i32, i32) {
    match kind {
        GRANT_CLOCK => (state.clock_call, state.clock_reply),
        GRANT_ENTROPY => (state.entropy_call, state.entropy_reply),
        GRANT_STORE => (state.store_call, state.store_reply),
        GRANT_FS => (state.fs_call, state.fs_reply),
        GRANT_NET => (state.net_call, state.net_reply),
        GRANT_HTTP => (state.http_call, state.http_reply),
        _ => (-1, -1),
    }
}

fn push_calls(state: &mut State, syscalls: &SyscallTable) {
    let mut grant = 0usize;
    while grant < state.grant_count {
        let kind = state.grants.get(grant).copied().unwrap_or(0);
        let (port, _) = ports_of(state, kind);
        let Some(wire) = state.wires.get_mut(grant) else {
            break;
        };
        if port >= 0 {
            wire::push_staged(
                syscalls,
                port,
                &wire.calls,
                &mut wire.staged,
                &mut wire.written,
            );
        } else {
            wire.staged = 0;
            wire.written = 0;
        }
        grant += 1;
    }
}

fn pull_replies(state: &mut State, syscalls: &SyscallTable) {
    let mut grant = 0usize;
    while grant < state.grant_count {
        let kind = state.grants.get(grant).copied().unwrap_or(0);
        let (_, port) = ports_of(state, kind);
        let Some(wire) = state.wires.get_mut(grant) else {
            break;
        };
        if port < 0 || wire.ready {
            grant += 1;
            continue;
        }
        if !wire.frame_ready && wire::take_frame(syscalls, port, &mut wire.reply, &mut wire.filled)
        {
            wire.frame_ready = true;
            wire.reply_filled = 0;
            wire.reply_length = CompletionRecord::decode(&wire.reply)
                .map_or(0, |record| record.answer.payload_length() as usize)
                .min(REPLY_PAYLOAD_BYTES);
        }
        // The payload follows its frame: until every byte is here, the answer
        // has not arrived.
        if wire.frame_ready
            && wire::take_payload(
                syscalls,
                port,
                &mut wire.reply_payload,
                &mut wire.reply_filled,
                wire.reply_length,
            )
        {
            wire.frame_ready = false;
            wire.ready = true;
        }
        grant += 1;
    }
}

/// Ask the clock adapter for the fact the task about to run will read.
///
/// A snapshot binding is never called by the program: the value is supplied
/// at the task boundary, which is what lets a clock be a capability and still
/// answer synchronously. The shell asks for it here, on the port a call would
/// use, under a request identifier of its own. Answers whether it is ready.
fn take_snapshot(state: &mut State, syscalls: &SyscallTable) -> bool {
    let held =
        (0..state.grant_count).find(|&grant| state.grants.get(grant).copied() == Some(GRANT_CLOCK));
    let Some(grant) = held else {
        state.snapshot_ready = true;
        return true;
    };
    let (call_port, reply_port) = ports_of(state, GRANT_CLOCK);
    if call_port < 0 || reply_port < 0 {
        state.snapshot_ready = true;
        return true;
    }
    if !state.snapshot_asked {
        let record = CallRecord {
            request: u64::MAX,
            binding: 0,
            payload_length: 0,
            trace: TRACE,
            payload: digest::digest(b""),
        };
        let frame = record.encode();
        let Some(wire) = state.wires.get_mut(grant) else {
            state.snapshot_ready = true;
            return true;
        };
        let at = wire.staged;
        if !copy_into(
            wire.calls.get_mut(at..at + CALL_FRAME).unwrap_or(&mut []),
            &frame,
        ) {
            state.snapshot_ready = true;
            return true;
        }
        wire.staged = at + CALL_FRAME;
        state.snapshot_asked = true;
    }
    push_calls(state, syscalls);
    pull_replies(state, syscalls);
    let Some(wire) = state.wires.get_mut(grant) else {
        return false;
    };
    if !wire.ready {
        return false;
    }
    wire.ready = false;
    wire.filled = 0;
    wire.reply_length = 0;
    wire.reply_filled = 0;
    let reply = wire.reply;
    if let Some(record) = CompletionRecord::decode(&reply) {
        if let Answer::Number(value) = record.answer {
            state.snapshot_value = value;
        }
    }
    state.snapshot_asked = false;
    state.snapshot_ready = true;
    true
}

// ------------------------------------------------------------ the REPL

const PROMPT: &[u8] = b"> ";
const CONTINUE: &[u8] = b"... ";

/// Take what standard input has, editing the line in progress: the terminal
/// is in raw mode, so the shell echoes, erases, and ends lines itself.
/// Answers whether a line was completed.
fn read_line(state: &mut State, syscalls: &SyscallTable) -> bool {
    if state.input_closed {
        return false;
    }
    if state.input_at >= state.input_length {
        let count = wire::read_available(syscalls, state.stdin_in, &mut state.input);
        if count == 0 {
            if wire::hung_up(syscalls, state.stdin_in) {
                state.input_closed = true;
            }
            return false;
        }
        state.input_length = count;
        state.input_at = 0;
    }
    while state.input_at < state.input_length {
        let byte = state.input.get(state.input_at).copied().unwrap_or(0);
        state.input_at += 1;
        match byte {
            b'\r' | b'\n' => {
                emit(&mut state.out, b"\n");
                return true;
            }
            0x7F | 0x08 => {
                if state.line_length > 0 {
                    state.line_length -= 1;
                    emit(&mut state.out, b"\x08 \x08");
                }
            }
            // Control-D on an empty line ends the session; Control-C
            // abandons the line.
            0x04 => {
                if state.line_length == 0 && state.source_length == 0 {
                    state.input_closed = true;
                    emit(&mut state.out, b"\n");
                    return false;
                }
            }
            0x03 => {
                state.line_length = 0;
                state.source_length = 0;
                state.continuing = false;
                emit(&mut state.out, b"^C\n");
                state.prompt_due = true;
            }
            _ => {
                if byte >= 0x20 || byte == b'\t' {
                    if let Some(slot) = state.line.get_mut(state.line_length) {
                        *slot = byte;
                        state.line_length += 1;
                        emit(&mut state.out, &[byte]);
                    }
                }
            }
        }
    }
    false
}

/// A refusal that means the line is not over yet: more of it may follow.
fn wants_more(report: &Diagnostic) -> bool {
    matches!(
        report.code(),
        code::UNEXPECTED_END_OF_SOURCE | code::UNTERMINATED_TEMPLATE
    )
}

/// One step of the REPL while no task runs: prompt, gather a line, and
/// start it when it parses.
fn repl_idle(state: &mut State, syscalls: &SyscallTable) {
    if state.prompt_due {
        if !emit(
            &mut state.out,
            if state.continuing { CONTINUE } else { PROMPT },
        ) {
            return;
        }
        state.prompt_due = false;
    }
    if !read_line(state, syscalls) {
        if state.input_closed {
            state.phase = PHASE_FINISH;
        }
        return;
    }
    let line_length = state.line_length;
    state.line_length = 0;
    let at = state.source_length;
    if at + line_length + 1 > SOURCE_CAPACITY {
        emit(&mut state.out, b"phasor: source-too-large\n");
        state.source_length = 0;
        state.continuing = false;
        state.prompt_due = true;
        return;
    }
    let mut copy = [0u8; LINE_CAPACITY];
    let line = state.line.get(..line_length).unwrap_or(&[]);
    let taken = copy.get_mut(..line_length).unwrap_or(&mut []);
    if !copy_into(taken, line) {
        return;
    }
    let placed = state
        .source
        .get_mut(at..at + line_length)
        .unwrap_or(&mut []);
    if !copy_into(placed, taken) {
        return;
    }
    if let Some(end) = state.source.get_mut(at + line_length) {
        *end = b'\n';
    }
    state.source_length = at + line_length + 1;
    // A blank line alone is nothing to run; inside a construct it continues it.
    let blank = state
        .source
        .get(..state.source_length)
        .unwrap_or(&[])
        .iter()
        .all(|byte| byte.is_ascii_whitespace());
    if blank {
        state.source_length = 0;
        state.prompt_due = true;
        return;
    }
    match compile_input(state) {
        Ok(unit) => {
            state.source_length = 0;
            state.continuing = false;
            state.current_unit = unit;
            state.task_starting = true;
            state.task_running = true;
            state.body_done = false;
            state.call_waited = 0;
            state.result_value = Value::UNDEFINED;
        }
        Err(report) => {
            if wants_more(&report) {
                state.continuing = true;
            } else {
                emit_diagnostic(&mut state.out, &report);
                state.source_length = 0;
                state.continuing = false;
            }
            state.prompt_due = true;
        }
    }
}

// ------------------------------------------------------------ the step

entry! {
    State;
    primary { args_in, stdout_out }
    inputs {
        stdin_in = 1,
        clock_reply = 2,
        entropy_reply = 3,
        store_reply = 4,
        fs_reply = 5,
        surface_in = 6,
        net_reply = 7,
        http_reply = 8,
    }
    outputs {
        exit_out = 1,
        clock_call = 2,
        entropy_call = 3,
        store_call = 4,
        fs_call = 5,
        net_call = 6,
        http_call = 7,
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
    if state.syscalls.is_null() || state.stdout_out < 0 {
        return -2;
    }
    // SAFETY: the table pointer was stored by `module_new` and checked
    // non-null above; the loader keeps it live for the module's lifetime.
    let syscalls = unsafe { &*state.syscalls };

    if state.phase == PHASE_DONE {
        return 1;
    }
    let drained = flush_out(&mut state.out, state.stdout_out, syscalls);

    if state.phase == PHASE_ARGS {
        state.steps = STEPS_DEFAULT;
        let count = if state.args_in >= 0 {
            wire::read_available(syscalls, state.args_in, &mut state.arec)
        } else {
            0
        };
        if count == 0 {
            state.waited += 1;
            if state.waited < ARGV_WAIT {
                return 0;
            }
        }
        state.arec_length = count;
        match read_argv(state) {
            Parsed::Run => {}
            Parsed::Help => {
                emit(&mut state.out, HELP);
                state.phase = PHASE_FINISH;
                return 0;
            }
            Parsed::Refused => {
                state.failed = true;
                state.phase = PHASE_FINISH;
                return 0;
            }
        }
        if state.grant_count > 0 {
            emit(&mut state.out, b"granted:");
            let mut grant = 0usize;
            while grant < state.grant_count {
                emit(&mut state.out, b" ");
                let (interface, interface_length) =
                    grant_name(state.grants.get(grant).copied().unwrap_or(0));
                emit(
                    &mut state.out,
                    interface.get(..interface_length).unwrap_or(&[]),
                );
                grant += 1;
            }
            emit(&mut state.out, b"\n");
        }
        if state.mode == MODE_REPL {
            state.prompt_due = true;
        }
        state.phase = PHASE_RUN;
        return 0;
    }

    if state.phase == PHASE_RUN {
        // Output that has not left yet is backpressure on the task: nothing
        // more is produced until the port has taken it.
        if !drained && out_room(&state.out) < OUT_CAPACITY / 4 {
            return 0;
        }
        // The standard surface goes into the realm before anything a person
        // typed, as a program like any other: compiled by the same front end,
        // verified, and run under the same bounds.
        if !state.facade_done && !state.bare && !state.task_running {
            // The surface arrives from the module that holds it, whole, ended
            // by a hang-up. A graph that wired none has no façade at all,
            // which is what asking for none looks like.
            if state.surface_in < 0 {
                state.facade_done = true;
                return 0;
            }
            if !state.surface_ready {
                let staged = wire::stage_stream(
                    syscalls,
                    state.surface_in,
                    &mut state.surface,
                    &mut state.surface_length,
                    &mut state.surface_overflowed,
                );
                if staged != wire::Staged::Complete {
                    return 0;
                }
                state.surface_ready = true;
                if state.surface_overflowed {
                    emit(&mut state.out, b"phasor: surface too large\n");
                    state.failed = true;
                    state.phase = PHASE_FINISH;
                    return 0;
                }
            }
            state.facade_done = true;
            if state.surface_length == 0 {
                return 0;
            }
            // SAFETY: the surface is staged and never written while the front
            // end runs, and the front end's storage is every other field.
            let source: &[u8] = unsafe {
                core::slice::from_raw_parts(state.surface.as_ptr(), state.surface_length)
            };
            match compile_bytes(state, source) {
                Ok(unit) => {
                    state.running_facade = true;
                    begin_task(state, unit);
                }
                Err(report) => {
                    emit_diagnostic(&mut state.out, &report);
                    state.failed = true;
                    state.phase = PHASE_FINISH;
                }
            }
            return 0;
        }
        push_calls(state, syscalls);
        pull_replies(state, syscalls);
        if state.task_running {
            // The task about to run reads facts rather than calling for them:
            // the snapshot is taken here, at the boundary, before any of the
            // program runs.
            if !state.snapshot_ready && !take_snapshot(state, syscalls) {
                return 0;
            }
            if advance(state) != Advance::Running {
                state.snapshot_ready = false;
                if state.running_facade {
                    // The surface is in place; what a person asked for runs
                    // next, in the same realm.
                    state.running_facade = false;
                    if state.mode == MODE_REPL {
                        state.prompt_due = true;
                    }
                } else if state.mode == MODE_REPL {
                    state.prompt_due = true;
                } else {
                    state.phase = PHASE_FINISH;
                }
            }
            return 0;
        }
        match state.mode {
            MODE_REPL => repl_idle(state, syscalls),
            _ => {
                if !state.source_complete {
                    let staged = wire::stage_stream(
                        syscalls,
                        state.stdin_in,
                        &mut state.source,
                        &mut state.source_length,
                        &mut state.source_overflow,
                    );
                    if staged != wire::Staged::Complete {
                        return 0;
                    }
                    state.source_complete = true;
                    if state.source_overflow {
                        emit(&mut state.out, b"phasor: source-too-large\n");
                        state.failed = true;
                        state.phase = PHASE_FINISH;
                        return 0;
                    }
                }
                if !start_input(state) {
                    state.phase = PHASE_FINISH;
                }
            }
        }
        return 0;
    }

    // PHASE_FINISH: everything written leaves, then the status.
    if !drained {
        return 0;
    }
    if !wire::push_exit(syscalls, state.exit_out, state.failed) {
        return 0;
    }
    state.phase = PHASE_DONE;
    1
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
