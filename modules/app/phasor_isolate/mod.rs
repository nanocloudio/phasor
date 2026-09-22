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
#[path = "../../common/capability.rs"]
mod capability;
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
#[path = "../../common/register.rs"]
mod register;
#[path = "../../common/softfloat.rs"]
mod softfloat;
#[path = "../../common/string.rs"]
mod string;
#[path = "../../common/text.rs"]
mod text;
#[path = "../../common/trace.rs"]
mod trace;
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
    Answer, Binding, Bindings, BindingsSave, CallRecord, Cause, CompletionRecord, Disposition,
    Pending, CALL_FRAME, COMPLETION_FRAME,
};
use bytecode::{Opcode, Unit};
use closure::Closure;
use diagnostic::{code, Diagnostic, Severity};
use heap::Slot;
use job::{Job, Queue, QueueSave};
use policy::Policy;
use realm::Realm;
#[cfg(not(feature = "omit_regexp"))]
use regexp::Choice;
use string::{Atoms, AtomsSave};
use value::{Handle, Value};
use vm::{Completion, Frame, ModuleInstance, Progress, Saves, Vm};

/// The storage profile: what this build sets aside for a program.
///
/// Selected by the silicon the module is compiled for, because the silicon is
/// what fixes the budget. An application-class core has a module-state arena
/// measured in megabytes and takes the sizes a deployed program needs. RP2350
/// has 240 KiB shared by every module in its graph and takes sizes that leave
/// room for them. The `embedded` variant selects the smaller profile on any
/// silicon, which is how a Linux lane runs the numbers a Cortex-M deployment
/// runs under.
///
/// The engine is the same in both. The language, the bytecode format and the
/// feature digest do not move, so one image is admitted by either. What moves
/// is how large a program may grow before `HeapExhausted`, how deep it may
/// call before `StackOverflow`, and how long an image may be before
/// `image-too-large`.
///
/// A silicon with no profile does not compile, because a size that merely
/// happened to fit would be a policy nobody chose. RP2040 is named so the
/// reason is at hand: its whole module-state arena is 64 KiB, and the realm
/// alone — the intrinsics a program starts with, before it allocates anything
/// — takes 69,240 bytes of heap.
mod profile {
    #[cfg(not(any(fluxor_silicon = "rp2350", feature = "profile_embedded")))]
    pub use application::*;
    #[cfg(any(fluxor_silicon = "rp2350", feature = "profile_embedded"))]
    pub use embedded::*;
    #[cfg(fluxor_silicon = "rp2040")]
    compile_error!(
        "phasor_isolate has no storage profile for rp2040: its 64 KiB module-state arena is smaller than the 69,240 bytes the realm alone takes"
    );

    /// Sized for the programs an application-class target is given: one that
    /// holds a few thousand properties is ordinary, and refusing it would be
    /// a policy no deployment asked for.
    ///
    /// The arena is NOT the bound on how large a response body a program can
    /// turn into a string: that stalls around 15 KiB against `phasor_http`'s
    /// 32 KiB body, and doubling the arena changes nothing. What costs is the
    /// number of allocations the decode makes, not the bytes it holds — see
    /// `bytesToText` in `modules/common/facade.rs`.
    pub mod application {
        pub const IMAGE_CAPACITY: usize = 64 * 1024;
        pub const ARENA_BYTES: usize = 256 * 1024;
        pub const SLOT_COUNT: usize = 8192;
        pub const WORKLIST: usize = 2048;
        pub const ATOM_ENTRIES: usize = 1024;
        pub const ATOM_HANDLES: usize = 768;
        /// How deep a program may call, and how many registers those calls
        /// may hold. A call is a frame here rather than a host stack frame,
        /// so this is what a program's recursion is bounded by.
        pub const FRAME_COUNT: usize = 256;
        pub const REGISTER_COUNT: usize = 4096;
        /// Bytes of payload one call or one completion may carry behind its
        /// frame. A call whose arguments do not fit is refused by the
        /// machine, and an answer longer than this is not taken.
        pub const PAYLOAD_BYTES: usize = 8 * 1024;
        /// Roots a collection stages before it starts: the registers in use,
        /// the frames, the interned handles, and the realm's own handles.
        pub const ROOT_COUNT: usize = 8192;
        /// One entry per byte of the longest function the verifier may be
        /// handed. An image cannot hold a function longer than itself, so
        /// matching the image capacity is exactly enough.
        pub const VERIFIER_CAPACITY: usize = 16 * 1024;
    }

    /// Sized to share a 240 KiB module-state arena: under 180 KiB of state
    /// in all, leaving the rest of the graph 60 KiB.
    ///
    /// The arena holds the realm — 69,240 bytes and 798 handles — and about
    /// 28 KiB of working heap over it, which is room for a hundred or so
    /// small objects: a script, not an application. Each table is matched to
    /// that. The handle table leaves 738 slots past the realm's. The atom
    /// table is a power of two, three-quarters full at its handle count.
    /// Thirty-two frames bound recursion at thirty-two calls. The verifier
    /// holds one entry per byte of the longest image it can be handed. Two
    /// kilobytes of image is a few hundred lines of source, which is what a
    /// program for a part this size is.
    pub mod embedded {
        pub const IMAGE_CAPACITY: usize = 2 * 1024;
        pub const ARENA_BYTES: usize = 96 * 1024;
        pub const SLOT_COUNT: usize = 1536;
        pub const WORKLIST: usize = 512;
        pub const ATOM_ENTRIES: usize = 512;
        pub const ATOM_HANDLES: usize = 384;
        pub const FRAME_COUNT: usize = 32;
        pub const REGISTER_COUNT: usize = 256;
        pub const PAYLOAD_BYTES: usize = 2048;
        pub const ROOT_COUNT: usize = 1024;
        pub const VERIFIER_CAPACITY: usize = 2048;
    }
}
use profile::*;

/// Opcodes this build cannot run. An image that carries one is refused when
/// it is admitted, as `image-not-admitted` naming the feature, rather than
/// running until it reaches the instruction: an image either runs whole on
/// this build or does not run on it.
#[cfg(feature = "omit_regexp")]
const REFUSED: &[verify::Refused] = &[(Opcode::CreateRegExp, diagnostic::image_feature::REGEXP)];
#[cfg(not(feature = "omit_regexp"))]
const REFUSED: &[verify::Refused] = &[];

const RESULT_CAPACITY: usize = 128;
const JOB_COUNT: usize = 32;
/// Bindings this isolate admits, and calls it lets be outstanding at once.
///
/// The general `host` call, and one for every capability a deployment may
/// grant. `MAX_BINDINGS` is the same number: the table an image's imports
/// resolve against and the table the machine admits are one table, so a
/// capability that was granted is one the program can reach.
const BINDING_COUNT: usize = MAX_BINDINGS;
const PENDING_COUNT: usize = 4;
const IN_FLIGHT_MAX: u32 = 4;
/// Resources a program may hold open at once. A provider that answers with a
/// resource answers with a handle, and a handle is an index and a generation
/// held here: an isolate with nowhere to hold one could be granted `open` and
/// would have to refuse every call to it.
const RESOURCE_COUNT: usize = 16;
/// Cells or bytes one collection slice works through.
const COLLECTION_SLICE: u32 = 512;
/// Free arena below which the isolate collects rather than waiting to fail.
///
/// A sixteenth of the arena. The fraction sets how often a program that
/// allocates steadily pays for a full collection, and every collection is
/// charged to the same budget as instructions, so it is a throughput number
/// and is measured as one: on `for(let i=0;i<N;i++)s+=i` at 4,000 iterations
/// on linux, a quarter costs 200.1 fuel per iteration and a sixteenth 134.0,
/// with the collector rather than the bytecode taking most of the difference.
/// Past a sixteenth the curve turns back -- a sixty-fourth is 139.9 and a
/// two-hundred-and-fifty-sixth 141.4 -- because each collection then starts
/// from a fuller heap and reclaims proportionally less. A sixteenth is where
/// it bottoms out, not a guess at "smaller is better".
///
/// Headroom is the smaller lever. The larger one is where a binding lives:
/// script-scope bindings sit in a context and allocate per iteration where
/// function locals sit in registers and do not, so the same loop inside a
/// function costs 17.0 fuel per iteration against 198.0 at script scope. A
/// direct `eval` may introduce a nearer binding at run time, which is what
/// `LdaShadowable` and `dynamic_names` in the lowerer are for.
const COLLECTION_HEADROOM: u32 = (ARENA_BYTES / 16) as u32;
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

/// Default for `empty_wait`: steps a hung-up empty image stream is given
/// before it is taken at its word.
///
/// Small on purpose, and the same value `phasor_compile` uses. Every real
/// producer writes within a few steps of the graph settling, so this is
/// imperceptible; it exists only to outlast a channel that reports hung-up
/// before its producer has written, which bcm2712 does.
const EMPTY_WAIT_STEPS: u32 = 64;
/// One control record: a kind, three bytes of padding, and a value.
/// Kind 1 asks the task to stop at its next safe point; kind 2 sets the
/// deadline, in the host's own time units; kind 3 reports the current time in
/// those units. A deadline passes only when a reported time reaches it, so a
/// graph that wires no clock has no deadline.
const CONTROL_FRAME: usize = 12;
/// Modules one closure may hold, and imports it may have in total.
const MAX_MODULES: usize = 16;
/// Bindings one image may state it requires.
const MAX_REQUIREMENTS: usize = 32;
/// The granted names a graph may name, as text. The register states the
/// bound, because the router holds the same text under it and the two must
/// agree byte for byte.
use register::GRANTS_BYTES;
/// Bindings this isolate may admit: the general `host` call, and one for each
/// capability a deployment granted.
const MAX_BINDINGS: usize = 17;
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

/// The bindings this isolate admits: the general `host` call, and one for
/// every capability the deployment granted.
///
/// What answers any of them, and where that is, the isolate never learns. The
/// order is the admission order, so a binding's index here is the index a
/// capability import resolves to.
fn admitted_bindings(state: &State) -> ([Binding; MAX_BINDINGS], usize) {
    let mut out = [Binding::EMPTY; MAX_BINDINGS];
    out[0] = Binding {
        name: digest::digest(b"host"),
        in_flight_max: IN_FLIGHT_MAX,
        in_flight: 0,
        class: binding::Class::Async,
        snapshot: 0.0,
        // The isolate's own host binding: its own scope, shared with
        // nothing.
        scope: 0,
    };
    let mut count = 1usize;
    // One scope per granted INTERFACE, not per member. A handle is opened by
    // one member and used by its siblings — `send` answers a response that
    // `status`, `headers`, `read` and `close` all read — so a scope per
    // member makes every one of those unresolvable, and a program can make
    // exactly one request before everything after the first call fails.
    // Scope 0 is the host binding above, so the first interface is 1.
    //
    // Looked up rather than counted, because a grant list is text a
    // deployment wrote and nothing makes an interface's members adjacent in
    // it. Two runs of `http` separated by `clock` are one capability.
    let mut seen = [crate::digest::Digest([0u8; 32]); MAX_BINDINGS];
    let mut seen_count = 0usize;
    each_grant(state, |interface, name| {
        let mark = crate::digest::digest(interface);
        let mut scope = 0u32;
        let mut at = 0usize;
        while at < seen_count {
            if seen.get(at).copied() == Some(mark) {
                scope = u32::try_from(at + 1).unwrap_or(u32::MAX);
                break;
            }
            at += 1;
        }
        if scope == 0 {
            if let Some(slot) = seen.get_mut(seen_count) {
                *slot = mark;
                seen_count += 1;
                scope = u32::try_from(seen_count).unwrap_or(u32::MAX);
            } else {
                // No room to record another interface. Giving it a scope
                // that already belongs to one would let handles cross, so
                // it gets one that matches nothing and its handles resolve
                // nowhere: a refusal, not a leak.
                scope = u32::MAX;
            }
        }
        if let Some(slot) = out.get_mut(count) {
            *slot = Binding {
                name,
                in_flight_max: IN_FLIGHT_MAX,
                in_flight: 0,
                class: binding::Class::Async,
                snapshot: 0.0,
                scope,
            };
            count += 1;
        }
    });
    (out, count)
}

/// Call `visit` with the name of every capability the graph granted.
///
/// The graph states them as text and this digests each in turn, so the name an
/// image requires and the name a deployment grants are computed by one
/// function over one form. A grant this cannot read is not a grant.
fn each_grant(state: &State, mut visit: impl FnMut(&[u8], crate::digest::Digest)) {
    let names = state.grants.get(..state.grants_length).unwrap_or(&[]);
    register::grants(names, |interface, member| {
        visit(interface, capability::name_of(interface, member));
    });
}

/// The binding that serves a capability, by the name both ends compute.
fn binding_of(state: &State, want: crate::digest::Digest) -> Option<u32> {
    let (admitted, count) = admitted_bindings(state);
    let mut index = 0usize;
    while index < count {
        if admitted
            .get(index)
            .is_some_and(|binding| binding.name == want)
        {
            return u32::try_from(index).ok();
        }
        index += 1;
    }
    None
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
/// Where a match backtracks, what it must put back, and the units it runs
/// over. A build without the regular expression engine sets none aside.
#[cfg(not(feature = "omit_regexp"))]
const CHOICE_COUNT: usize = 256;
#[cfg(not(feature = "omit_regexp"))]
const UNDO_COUNT: usize = 256;
#[cfg(not(feature = "omit_regexp"))]
const SUBJECT_UNITS: usize = 1024;
#[repr(C)]
struct Wire {
    /// Call frames staged to leave, each followed by its payload.
    calls: [u8; CALL_FRAME * PENDING_COUNT + PAYLOAD_BYTES],
    staged: usize,
    written: usize,
    completion: [u8; COMPLETION_FRAME],
    filled: usize,
    /// The bytes behind the completion frame, once the frame is whole.
    payload: [u8; PAYLOAD_BYTES],
    payload_filled: usize,
    payload_length: usize,
    frame_ready: bool,
    ready: bool,
    /// Steps this isolate has waited with a call outstanding and nothing to do.
    waited: u32,
}

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    /// Whether this module has told the scheduler its outputs are
    /// meaningful. Fluxor gates a module until every forward upstream has
    /// signalled `StepOutcome::Ready` (3 from a PIC module), and a module
    /// that never signals it holds its whole downstream dark for the life of
    /// the graph. Only bare metal enforces the gate -- linux and wasm step
    /// every module regardless -- so a module that forgets to announce runs
    /// everywhere but on the board.
    announced: bool,
    image_in: i32,
    completion_in: i32,
    result_out: i32,
    call_out: i32,
    diagnostic_out: i32,
    exit_out: i32,
    control_in: i32,
    steps: u32,
    call_wait: u32,
    /// What this deployment granted, as the graph named it: a list of
    /// `<interface>#<member>` separated by commas. An image may require
    /// nothing that is not here.
    ///
    /// It is the graph's to state, because granting is the deployment's act
    /// and not the image's. The isolate holds the names rather than their
    /// digests so that one function computes a capability's identity for
    /// both ends, which is what keeps them from drifting apart again.
    grants: [u8; GRANTS_BYTES],
    grants_length: usize,
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
    #[cfg(not(feature = "omit_regexp"))]
    choices: [Choice; CHOICE_COUNT],
    #[cfg(not(feature = "omit_regexp"))]
    undo: [(u8, u32); UNDO_COUNT],
    #[cfg(not(feature = "omit_regexp"))]
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
    /// Where the machine stages the arguments of the calls in its outbox.
    call_payloads: [u8; PAYLOAD_BYTES],
    /// The resources providers opened for this task, addressed by handle.
    resources: [binding::Resource; RESOURCE_COUNT],
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
    /// Steps a hung-up EMPTY image stream is read as "not yet" before it is
    /// read as an empty image. See the wait in `module_step`.
    empty_wait: u32,
    /// How many of those steps have passed.
    empty_waited: u32,
    /// Emit one `[iso]` line every `trace` steps; 0 is silent.
    trace: u32,
    /// Steps taken, which the step histogram cannot report.
    trace_steps: u32,
}

/// Admit the staged image and start the task, saving the machine's state.
fn start(state: &mut State) -> bool {
    if state.overflowed {
        // Refused before this is reached; kept so that a partial image can
        // never be admitted by any path.
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
                let Ok(unit) = verify::admit_with(image, &mut state.verifier_state, REFUSED) else {
                    return false;
                };
                units[index] = unit;
                index += 1;
            }
            count
        }
        None => {
            match verify::admit_with(bytes, &mut state.verifier_state, REFUSED) {
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
                // A capability names what the deployment granted, not a
                // module the stream carries. It resolves to the binding that
                // serves it, so a program may use what it required -- and,
                // because the gate and this table are the same table, only
                // what it was granted.
                if capability::is_capability(specifier.get(..specifier_length).unwrap_or(&[])) {
                    let mut required = [capability::Requirement::EMPTY; 1];
                    let Ok(1) = capability::requirements_of(
                        specifier.get(..specifier_length).unwrap_or(&[]),
                        name.get(..name_length).unwrap_or(&[]),
                        &mut required,
                    ) else {
                        return false;
                    };
                    let Some(binding) = binding_of(state, required[0].name()) else {
                        return false;
                    };
                    let Some(entry) = state.import_table.get_mut(state.import_count as usize)
                    else {
                        return false;
                    };
                    *entry = (crate::bytecode::CAPABILITY_IMPORT_SOURCE, binding);
                    state.import_count += 1;
                    import += 1;
                    continue;
                }
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
    let (admitted, admitted_count) = admitted_bindings(state);
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
        admitted.get(..admitted_count).unwrap_or(&[]),
    );
    let mut storage = agent_storage!(state);
    storage.payloads = Some(&mut state.call_payloads);
    storage.resources = Some(&mut state.resources);
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
    let mut storage = agent_storage!(state);
    storage.payloads = Some(&mut state.call_payloads);
    storage.resources = Some(&mut state.resources);
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
                let length = state.wire.payload_length;
                state.wire.payload_length = 0;
                state.wire.payload_filled = 0;
                if let Some(record) = CompletionRecord::decode(&state.wire.completion) {
                    let payload = state.wire.payload.get(..length).unwrap_or(&[]);
                    let _ = machine.apply_completion_with(&record, payload);
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
                        answer: Answer::None,
                    });
                }
                progressed = true;
            }

            // Whatever calls the program made leave as frames; the machine forgets them
            // once they are staged, so a record is carried exactly once.
            // Each frame is followed by the arguments it names, so a record and its
            // bytes are carried together or not at all: when the port side has no
            // room for all of them, they wait for the next step, whole.
            if outcome == Advance::Running && state.call_out >= 0 && !machine.calls().is_empty() {
                let staged = state.wire.staged;
                let payloads = machine.call_payloads();
                let mut needed = 0usize;
                for record in machine.calls() {
                    needed += CALL_FRAME + record.payload_length as usize;
                }
                if staged + needed <= state.wire.calls.len() {
                    let mut at = staged;
                    let mut payload_at = 0usize;
                    let mut count = 0u32;
                    for record in machine.calls() {
                        let length = record.payload_length as usize;
                        let frame = record.encode();
                        let (Some(slot), Some(payload)) = (
                            state.wire.calls.get_mut(at..at + CALL_FRAME),
                            payloads.get(payload_at..payload_at + length),
                        ) else {
                            break;
                        };
                        slot.copy_from_slice(&frame);
                        at += CALL_FRAME;
                        if let Some(slot) = state.wire.calls.get_mut(at..at + length) {
                            slot.copy_from_slice(payload);
                        }
                        at += length;
                        payload_at += length;
                        count += 1;
                    }
                    state.wire.staged = at;
                    machine.take_calls();
                    state.calls_made = state.calls_made.saturating_add(count);
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
                            answer: Answer::None,
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

    3, grants, str, 0
        => |s, d, len| {
            let taken = if len > GRANTS_BYTES { GRANTS_BYTES } else { len };
            s.grants_length = taken;
            if taken > 0 {
                // SAFETY: the params reader hands a pointer valid for `len`
                // bytes, and `taken` is no larger than it or the field.
                unsafe {
                    core::ptr::copy_nonoverlapping(d, s.grants.as_mut_ptr(), taken);
                }
            }
        };

    4, trace, u32, 0
        => |s, d, len| { s.trace = p_u32(d, len, 0, 0); };

    5, empty_wait, u32, EMPTY_WAIT_STEPS
        => |s, d, len| { s.empty_wait = p_u32(d, len, 0, EMPTY_WAIT_STEPS); };
}

/// Say where this module got to, once every `trace` steps.
///
/// The step counter is in the line because the scheduler's histogram cannot
/// supply it: the kernel records a step's time only in the `Continue` arm, so
/// a step that returned `Ready`, `Burst`, `Done` or an error never reaches
/// `MON_HIST`, and a module that stops appearing there cannot be told apart
/// from one that stopped returning `Continue`. This line is emitted from
/// every step regardless of what the step returns, so the two are
/// distinguishable — which on a board with no console and one log channel is
/// the difference between a diagnosis and a guess.
fn say(state: &mut State, syscalls: &SyscallTable) {
    if state.trace == 0 {
        return;
    }
    state.trace_steps = state.trace_steps.saturating_add(1);
    // Every step from the moment an image is staged, whatever the rate asks
    // for, and the rate only while there is nothing to run.
    //
    // This module completes a whole program in ONE advance call: a
    // 60,000-iteration loop retired 6.6M instructions in a single step on
    // bcm2712, and lowering the `steps` budget did not spread it across
    // more. So there is no mid-flight to sample -- the only honest window is
    // the one that brackets that single step, and a rate of one line per N
    // steps would pad it with up to N idle steps on each side and charge the
    // engine for time it spent waiting.
    //
    // `image_length > 0` while still in phase 0 is the staging step, which
    // is the last one before execution. Tracing from there gives a tight
    // bracket without putting a line on the wire for every step of a wait
    // that can be tens of thousands of them.
    let close_to_work = state.phase == 1 || state.image_length > 0;
    if !close_to_work && !state.trace_steps.is_multiple_of(state.trace) {
        return;
    }
    // The board's own microsecond clock, so a rate can be computed from
    // WITHIN one run: two consecutive lines give a fuel delta and a time
    // delta, and their quotient is throughput measured mid-flight. That
    // needs no second workload size and no subtraction of process start,
    // module instantiation or graph teardown -- the costs the Linux load
    // lane has to cancel by running two sizes and taking the difference.
    // SAFETY: the table is the loader's, live for the module's lifetime,
    // and the call takes no argument.
    let now = unsafe { dev_micros(syscalls) };
    let mut line = trace::Line::<128>::new();
    line.text(b"[iso] step=").number(state.trace_steps);
    line.text(b" phase=").number(u32::from(state.phase));
    line.text(b" img=")
        .number(u32::try_from(state.image_length).unwrap_or(u32::MAX));
    line.text(b" fuel=")
        .number(u32::try_from(state.fuel_spent).unwrap_or(u32::MAX));
    line.text(b" calls=").number(state.calls_made);
    line.text(b" us=")
        .number(u32::try_from(now).unwrap_or(u32::MAX));
    trace::write(syscalls, line.bytes());
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

/// Whether every binding the staged image states it requires is one this
/// isolate admits.
///
/// The requirements are read from the image's own import table, where nothing
/// outside the image can claim one for it, and checked before anything is
/// admitted or run.
/// Whether the deployment granted every capability the image requires.
///
/// `Some(false)` is a requirement that is not granted, or one that could not
/// be checked, which refuses the image the same way. `None` is an image that
/// does not parse at all, which is not the same thing: admission reads it
/// next and says what is wrong with it, rather than this saying something is
/// not granted when nothing was asked for.
fn requirements_granted(state: &State) -> Option<bool> {
    let bytes = state.image.get(..state.image_length)?;
    if bytes.is_empty() {
        // Nothing arrived. Whatever was to produce an image said why on its
        // own port, and an image that does not exist requires nothing.
        return Some(true);
    }
    let mut units = [Unit::EMPTY; MAX_MODULES];
    let count = match Closure::parse(bytes) {
        Ok(closure) => {
            let count = closure.count() as usize;
            // A closure with more modules than this reads is not a closure
            // whose first sixteen modules are the whole of it.
            if count > MAX_MODULES {
                return None;
            }
            let mut index = 0usize;
            while index < count {
                let Ok(unit) = closure
                    .image(u32::try_from(index).unwrap_or(u32::MAX))
                    .ok_or(())
                    .and_then(|image| Unit::parse(image).map_err(|_| ()))
                else {
                    return None;
                };
                units[index] = unit;
                index += 1;
            }
            count
        }
        Err(_) => match Unit::parse(bytes) {
            Ok(unit) => {
                units[0] = unit;
                1
            }
            Err(_) => return None,
        },
    };
    let mut required = [capability::Requirement::EMPTY; MAX_REQUIREMENTS];
    let mut index = 0usize;
    while index < count {
        let unit = units.get(index)?;
        // A requirement that could not be read is not a requirement that is
        // absent: refusing is the only answer that does not admit the image
        // nobody could check.
        let Ok(stated) = capability::requirements(unit, &mut required) else {
            return Some(false);
        };
        let mut position = 0usize;
        while position < stated {
            let requirement = required.get(position)?;
            if !granted(state, requirement.name()) {
                return Some(false);
            }
            position += 1;
        }
        index += 1;
    }
    Some(true)
}

/// Whether the deployment granted this capability. The gate and the table an
/// image's imports resolve against are the same table, so an image cannot be
/// admitted for a binding that is not there to use.
fn granted(state: &State, want: crate::digest::Digest) -> bool {
    binding_of(state, want).is_some()
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
    if !state.wire.frame_ready
        && wire::take_frame(
            syscalls,
            state.completion_in,
            &mut state.wire.completion,
            &mut state.wire.filled,
        )
    {
        state.wire.frame_ready = true;
        state.wire.payload_filled = 0;
        state.wire.payload_length = CompletionRecord::decode(&state.wire.completion)
            .map_or(0, |record| record.answer.payload_length() as usize)
            .min(PAYLOAD_BYTES);
    }
    // The bytes follow their frame: until every one of them is here, the
    // answer has not arrived.
    if state.wire.frame_ready
        && wire::take_payload(
            syscalls,
            state.completion_in,
            &mut state.wire.payload,
            &mut state.wire.payload_filled,
            state.wire.payload_length,
        )
    {
        state.wire.frame_ready = false;
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
    say(state, syscalls);
    announce_ready!(state);

    if state.phase == 3 {
        return 1;
    }
    drain_control(state, syscalls);

    if state.phase == 0 {
        if !stage_image(state, syscalls) {
            return 0;
        }
        // Longer than this build stages: what is staged is a prefix, not an
        // image, and nothing read from it means anything. Said with the
        // length that is staged, which is the number the author has to get
        // under.
        if state.overflowed {
            state.diagnostic = Diagnostic::at(code::IMAGE_TOO_LARGE, Severity::Error, 0)
                .with(u32::try_from(IMAGE_CAPACITY).unwrap_or(u32::MAX))
                .encode();
            state.has_diagnostic = true;
            state.failed = true;
            state.phase = 2;
            return 0;
        }
        // Every binding the image says it requires must be one the deployment
        // granted, or the image is refused whole. A program never starts and
        // then discovers that a capability it imported is `undefined`. An
        // image whose requirements cannot be read goes on to admission, which
        // says what is wrong with it rather than what is not granted.
        if requirements_granted(state) == Some(false) {
            state.diagnostic =
                Diagnostic::at(code::BINDING_NOT_GRANTED, Severity::Error, 0).encode();
            state.has_diagnostic = true;
            state.failed = true;
            state.phase = 2;
            return 0;
        }
        if state.image_length == 0 {
            // Zero bytes is not an image, and a hang-up on an empty stream is
            // not always an image that ended -- it can be one that has not
            // arrived.
            //
            // `stage_stream` answers `Complete` on `POLL_HUP`, and on bcm2712
            // a channel reports hung-up before its producer has ever written
            // to it. Retiring here would take a not-yet-written port for a
            // finished empty program -- and would hang up this module's own
            // ports, retiring whatever waits on its result.
            //
            // The wait is bounded for the same reason the compiler's is: a
            // graph that really does hand the isolate an empty image must
            // get an outcome rather than a hang.
            if state.empty_waited < state.empty_wait {
                state.empty_waited = state.empty_waited.saturating_add(1);
                return 0;
            }
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
