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
    #[cfg(not(any(fluxor_abi = "thumbv8m", feature = "profile_embedded")))]
    pub use application::*;
    #[cfg(any(fluxor_abi = "thumbv8m", feature = "profile_embedded"))]
    pub use embedded::*;
    #[cfg(fluxor_abi = "thumbv6m")]
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
        /// Host stack the machine may use below its entry; 0 is unbounded.
        /// A module here has a stack of its own, sized by its MMU.
        pub const HOST_STACK_BYTES: u32 = 0;
        /// Modules one linked closure may hold.
        pub const MAX_MODULES: usize = 16;
        /// Collection slices one step may run; 0 finishes a collection where
        /// it starts.
        pub const COLLECTION_BUDGET: u32 = 0;
        /// Instructions one step runs before it hands the scheduler back.
        pub const SLICE: u64 = 4096;
        /// Jobs one step runs.
        pub const JOB_SLICE: u32 = 16;
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
    /// holds one entry per byte of the longest function it can be handed.
    /// Eight kilobytes of image is the console-and-timers surface a part this
    /// size carries ahead of its program (packaging/embedded/surface.js, some
    /// 3.3 KB compiled) and a program of a hundred lines or so.
    pub mod embedded {
        /// Host stack the machine may use below its entry. Every module on
        /// an RP part steps on the kernel's one stack, 40 KiB of which a
        /// module may reach; this is the part of it the engine's recursions
        /// may spend, and the rest is the entry path and the deepest frame
        /// below a check. The declaration beside `entry!` adds them up.
        ///
        /// A figure measured on Thumb frames, so it binds on that silicon
        /// only: the same profile on an application-class part, where frames
        /// are larger and a module has a stack of its own, is unbounded.
        #[cfg(fluxor_abi = "thumbv8m")]
        pub const HOST_STACK_BYTES: u32 = 16 * 1024;
        #[cfg(not(fluxor_abi = "thumbv8m"))]
        pub const HOST_STACK_BYTES: u32 = 0;
        /// Modules one linked closure may hold. Each is a parsed unit on the
        /// host stack while the task starts and while it advances, so four,
        /// which is a program and the few modules it splits into.
        pub const MAX_MODULES: usize = 4;
        /// Collection slices one step may run. A whole collection of this
        /// heap is some 150 slices and, on a 150 MHz core, 11 ms or more;
        /// twenty-four is under 2 ms, so a collection spans several steps
        /// and none of them outlasts a cooperative scheduler's deadline.
        pub const COLLECTION_BUDGET: u32 = 24;
        /// Instructions one step runs before it hands the scheduler back. An
        /// instruction is some 8 µs on a 150 MHz core with no floating-point
        /// unit, so 256 is about 2 ms; a long loop runs on, over more steps.
        pub const SLICE: u64 = 256;
        /// Jobs one step runs. A job runs to its end, so this is the bound a
        /// step's reactions have, and two keeps a turn's to one step.
        pub const JOB_SLICE: u32 = 2;
        /// The image staged from a parameter, a stream, or a program the
        /// kernel cannot map. A build that runs only programs read in place
        /// (`image_in_place`) stages none, and gives these bytes back.
        #[cfg(not(feature = "image_in_place"))]
        pub const IMAGE_CAPACITY: usize = 8 * 1024;
        #[cfg(feature = "image_in_place")]
        pub const IMAGE_CAPACITY: usize = 0;
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
/// Bytes of `print` output held between steps. A call that does not fit is
/// dropped whole, which is the host's quota on output.
const PRINT_BYTES: usize = 256;
/// Bytes of one `[js]` log line, the tag included; a longer line is cut.
const PRINT_LINE: usize = 120;
/// Grant entries the surface can place: one per admitted binding.
const SURFACE_ENTRIES: usize = MAX_BINDINGS;
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
    print: Option<(&'a mut [u8], &'a mut usize)>,
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
        print,
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

/// Request identifiers the seed's draws go out under, apart from any a task
/// makes.
const SEED_REQUEST: u64 = u64::MAX - 64;

/// Draw the seed a surfaced task's generator is keyed with, a word per call
/// on the granted entropy member, through the same port and router the task's
/// own calls take. Answers whether the drawing is over: it is at once when
/// the task is not surfaced or holds no entropy grant, and a refused draw
/// ends it without a seed, so the task runs with no `crypto` rather than one
/// keyed by less than it should be.
fn draw_seed(state: &mut State, syscalls: &SyscallTable) -> bool {
    let name = capability::name_of(b"wasi:random/random", b"random");
    let binding = if state.surface {
        binding_of(state, name)
    } else {
        None
    };
    let Some(binding) = binding else {
        state.seed_done = true;
        return true;
    };
    if state.call_out < 0 || state.completion_in < 0 {
        state.seed_done = true;
        return true;
    }
    if usize::from(state.seed_taken) == state.seed.len() {
        state.seed_done = true;
        return true;
    }
    let request = SEED_REQUEST + u64::from(state.seed_taken);
    if !state.seed_asked {
        let frame = CallRecord {
            request,
            binding,
            payload_length: 0,
            trace: TRACE,
            payload: crate::digest::digest(&[]),
        }
        .encode();
        let at = state.wire.staged;
        let staged = state
            .wire
            .calls
            .get_mut(at..at + CALL_FRAME)
            .is_some_and(|room| copy_into(room, &frame));
        if !staged {
            return false;
        }
        state.wire.staged = at + CALL_FRAME;
        state.seed_asked = true;
    }
    push_calls(state, syscalls);
    pull_completion(state, syscalls);
    if !state.wire.ready {
        return false;
    }
    state.wire.ready = false;
    state.wire.filled = 0;
    let Some(record) = CompletionRecord::decode(&state.wire.completion) else {
        state.seed_done = true;
        return true;
    };
    if record.request != request {
        // Not this draw's answer: nothing else is out before the task starts.
        return false;
    }
    state.seed_asked = false;
    match (record.disposition, record.answer) {
        (Disposition::Fulfilled, Answer::Number(value)) => {
            if let Some(word) = state.seed.get_mut(usize::from(state.seed_taken)) {
                *word = crate::value::to_uint32(value);
            }
            state.seed_taken += 1;
            false
        }
        _ => {
            // Refused: no seed, and so no `crypto`.
            state.seed_taken = 0;
            state.seed_done = true;
            true
        }
    }
}

/// One granted member as the surface places it: the global it goes under,
/// the name it has there, and the binding that serves it.
#[derive(Clone, Copy)]
struct SurfaceEntry {
    namespace: [u8; register::NAMESPACE_MAX],
    namespace_length: usize,
    member: [u8; register::MAX_MEMBER],
    member_length: usize,
    binding: u32,
}

impl SurfaceEntry {
    const EMPTY: Self = Self {
        namespace: [0; register::NAMESPACE_MAX],
        namespace_length: 0,
        member: [0; register::MAX_MEMBER],
        member_length: 0,
        binding: 0,
    };
}

/// Every granted member a program can call, in admission order, so that the
/// binding each is defined over is the one `admitted_bindings` admitted for
/// it. A snapshot member is read at a task boundary rather than called, and
/// this isolate supplies none, so it is not surfaced; nor is an interface the
/// register has no word for.
fn surface_entries(state: &State) -> ([SurfaceEntry; SURFACE_ENTRIES], usize) {
    let mut out = [SurfaceEntry::EMPTY; SURFACE_ENTRIES];
    let mut count = 0usize;
    // Binding 0 is `host`; each grant follows in order.
    let mut binding = 1u32;
    let names = state.grants.get(..state.grants_length).unwrap_or(&[]);
    register::grants(names, |interface, member| {
        let index = binding;
        binding += 1;
        let (namespace, namespace_length) = register::namespace(interface);
        let callable = register::member_of(interface, member).is_some_and(|m| !m.snapshot);
        if namespace_length == 0 || !callable || member.len() > register::MAX_MEMBER {
            return;
        }
        let Some(slot) = out.get_mut(count) else {
            return;
        };
        let mut entry = SurfaceEntry {
            namespace,
            namespace_length,
            binding: index,
            ..SurfaceEntry::EMPTY
        };
        if !copy_into(
            entry.member.get_mut(..member.len()).unwrap_or(&mut []),
            member,
        ) {
            return;
        }
        entry.member_length = member.len();
        *slot = entry;
        count += 1;
    });
    (out, count)
}

/// Put the grants on the global object, one namespace per interface, and
/// install `print`.
fn surface_grants(machine: &mut Vm<'_, '_, '_, '_>, entries: &[SurfaceEntry]) -> bool {
    let mut at = 0usize;
    while at < entries.len() {
        let Some(first) = entries.get(at) else {
            return false;
        };
        let word = first.namespace.get(..first.namespace_length).unwrap_or(&[]);
        // One namespace per interface, however its members were spread
        // through the grant list.
        let mut seen = false;
        let mut earlier = 0usize;
        while earlier < at {
            if entries
                .get(earlier)
                .is_some_and(|e| e.namespace.get(..e.namespace_length).unwrap_or(&[]) == word)
            {
                seen = true;
                break;
            }
            earlier += 1;
        }
        if !seen {
            let Ok(namespace) = machine.define_namespace(word) else {
                return false;
            };
            for entry in entries {
                if entry.namespace.get(..entry.namespace_length).unwrap_or(&[]) == word
                    && machine
                        .define_binding_in(
                            namespace,
                            entry.member.get(..entry.member_length).unwrap_or(&[]),
                            entry.binding,
                        )
                        .is_err()
                {
                    return false;
                }
            }
        }
        at += 1;
    }
    machine.install_print().is_ok()
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
    /// Surface the grants as the shell does: each granted interface a global
    /// namespace holding its granted members, and `print`, whose lines go to
    /// the kernel log. Off, a program reaches capabilities by import only.
    surface: bool,
    /// The image arrived whole as the `image` parameter, so nothing is
    /// staged from `image_in`.
    preloaded: bool,
    /// Where the image comes from when the graph carried no parameter: the
    /// bundle's program section, asked once (`PROGRAM_*`).
    program_phase: u8,
    /// The program's address where it can be read in place -- flash on a
    /// board, the mapped bundle on a host -- or 0, when it is copied into
    /// `image` instead.
    program_addr: usize,
    program_size: u32,
    /// What `print` wrote that has not yet gone to the log.
    printed: [u8; PRINT_BYTES],
    printed_length: usize,
    /// The longest one step of running took, in microseconds, which is what
    /// a part with a step deadline has to know. Reported with `trace`.
    slice_us_max: u32,
    /// How long admitting the image and starting the task took, after the
    /// realm was built.
    start_us: u32,
    /// The realm, built a stage per step before the task starts: on a part
    /// with a short step deadline the whole of it is too long for one.
    building: agent::Building,
    /// When the machine's own work began and ended in the last advance, so a
    /// slow one says whether the time went to it or to restoring and saving
    /// it. Reported with `trace`.
    body_from_us: u64,
    body_to_us: u64,
    /// The seed a surfaced task's generator is keyed with, drawn from the
    /// entropy grant before the program runs: words taken so far, whether a
    /// draw is out, and whether the drawing is over (with a seed or without).
    seed: [u32; 8],
    seed_taken: u8,
    seed_asked: bool,
    seed_done: bool,
}

/// Admit the staged image and start the task, saving the machine's state.
fn start(state: &mut State) -> bool {
    if state.overflowed {
        // Refused before this is reached; kept so that a partial image can
        // never be admitted by any path.
        return false;
    }
    let Some(bytes) = image_bytes(state) else {
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
    let (surface, surface_count) = surface_entries(state);
    let surfaced = state.surface;
    let seeded = surfaced && usize::from(state.seed_taken) == state.seed.len();
    let seed = state.seed;
    let policy = policy(state);
    let metering = agent::Metering {
        policy: &policy,
        collection_headroom: COLLECTION_HEADROOM,
        trace: TRACE,
        host_stack: HOST_STACK_BYTES,
        collection_budget: COLLECTION_BUDGET,
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
        if surfaced {
            Some((&mut state.printed[..], &mut state.printed_length))
        } else {
            None
        },
    );
    let mut storage = agent_storage!(state);
    storage.payloads = Some(&mut state.call_payloads);
    storage.resources = Some(&mut state.resources);
    let building = state.building;
    let started = agent::fresh_built(
        units.get(..count).unwrap_or(&[]),
        storage,
        attach,
        metering,
        &building,
        |machine| {
            if machine.define_binding(b"host", 0).is_err() {
                return false;
            }
            if surfaced && !surface_grants(machine, surface.get(..surface_count).unwrap_or(&[])) {
                return false;
            }
            if seeded {
                machine.seed_entropy(&seed);
                if machine.install_entropy().is_err() {
                    return false;
                }
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
    let Some(bytes) = image_bytes(state) else {
        return Advance::Failed;
    };
    // SAFETY: the image is staged and not written while the machine runs, so
    // it may keep reading it while the rest of the state moves.
    let bytes: &[u8] = unsafe { core::slice::from_raw_parts(bytes.as_ptr(), bytes.len()) };
    // The image was verified when it was admitted; parsing it again only
    // rebuilds the view of bytes that have not changed since — without the
    // two digests, each a hash over the build's whole opcode table or
    // feature list, which on a 150 MHz part were most of a step.
    let mut units = [Unit::EMPTY; MAX_MODULES];
    if state.linked {
        let Ok(closure) = Closure::parse_admitted(bytes) else {
            return Advance::Failed;
        };
        let mut index = 0u32;
        while index < state.module_count {
            let Some(image) = closure.image(index) else {
                return Advance::Failed;
            };
            let Ok(unit) = Unit::parse_admitted(image) else {
                return Advance::Failed;
            };
            units[index as usize] = unit;
            index += 1;
        }
    } else {
        let Ok(unit) = Unit::parse_admitted(bytes) else {
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
        host_stack: HOST_STACK_BYTES,
        collection_budget: COLLECTION_BUDGET,
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
        if state.surface {
            Some((&mut state.printed[..], &mut state.printed_length))
        } else {
            None
        },
    );
    let mut storage = agent_storage!(state);
    storage.payloads = Some(&mut state.call_payloads);
    storage.resources = Some(&mut state.resources);
    let realm = state.realm;
    let saves = state.saves;
    let count = if linked { module_count as usize } else { 1 };
    let syscalls = state.syscalls;
    let mut body_from = 0u64;
    let mut body_to = 0u64;
    let stepped = agent::adopt(
        units.get(..count).unwrap_or(&[]),
        storage,
        attach,
        metering,
        realm,
        &saves,
        |machine| {
            // SAFETY: the table pointer was checked non-null by the step.
            body_from = unsafe { dev_micros(&*syscalls) };
            if state.surface {
                // An event loop's budget is per turn, not per lifetime.
                machine.refuel(u64::from(steps));
            }
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

            // A collection the last step left open finishes before anything
            // else happens: no answer is applied and no code runs while the
            // collector holds the heap.
            if machine.collecting() {
                if let Some(stopped) = machine.collection_step() {
                    let reason = match stopped {
                        Completion::Terminated(reason) => reason.code(),
                        _ => diagnostic::termination::UNCAUGHT_THROW,
                    };
                    state.diagnostic = Diagnostic::at(reason, Severity::Error, 0).encode();
                    state.has_diagnostic = true;
                    outcome = Advance::Failed;
                }
                if outcome == Advance::Failed || machine.collecting() {
                    state.collections = machine.collections();
                    // SAFETY: as above.
                    body_to = unsafe { dev_micros(&*syscalls) };
                    return outcome;
                }
            }

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
                    Err(stopped) => {
                        // A reaction that stopped the machine says why, as a
                        // stopped body does: a failure with no reason is one
                        // nobody can act on.
                        let reason = match stopped {
                            Completion::Terminated(reason) => reason.code(),
                            _ => diagnostic::termination::UNCAUGHT_THROW,
                        };
                        state.diagnostic = Diagnostic::at(reason, Severity::Error, 0).encode();
                        state.has_diagnostic = true;
                        outcome = Advance::Failed;
                    }
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
            // still waiting on anything, and no reaction is left to run. A surfaced
            // task is a host's event loop as well: a timer it set is a call still in
            // flight, and the program is not over while one is.
            if outcome == Advance::Running
                && state.body_done
                && queue_idle(machine)
                && (!state.surface || bindings_in_flight(machine) == 0)
            {
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
            // SAFETY: as above.
            body_to = unsafe { dev_micros(&*syscalls) };
            outcome
        },
    );
    state.body_from_us = body_from;
    state.body_to_us = body_to;
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

// The stack this module may reach, for a part where every module steps on
// the kernel's one stack. Every recursion in the engine passes a check against
// `HOST_STACK_BYTES` (see `Vm::bound_host_stack`), so the depth is bounded by
// four measured terms, read off the call graph of the `embedded_lean` build
// on rp2350 with the checked functions cut:
//
//   14,784  W1, the deepest path that recurses nowhere: `module_step`,
//           `advance`, the dispatch loop, `step`;
//   10,216  E, the stack above where the limit is set — `agent::run` is
//           inlined into `advance` and `start`, so it is `module_step` and
//           the larger of those two frames;
//   16,384  the budget the recursions may spend below that point;
//    9,368  D, the deepest path below a check that passed, to the next
//           check or a leaf.
//
// max(W1, E + budget) + D = 35,968 bytes, declared as 38 KiB. (W1 + budget
// + D, which needs no E, is 40,536: over the declaration, and no longer the
// figure to use now the limit's point is resolved.) It holds only where every recursion is
// checked: the regular expression engine and JSON's nesting are not, so a
// build carrying either declares nothing and an RP graph refuses it, rather
// than admitting it on a figure its code can exceed.
declare_module_stack_bytes!(if cfg!(all(
    fluxor_abi = "thumbv8m",
    feature = "omit_regexp",
    feature = "omit_json"
)) {
    38 * 1024
} else {
    0
});

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

    6, surface, u8, 0
        => |s, d, len| { s.surface = p_u8(d, len, 0, 0) != 0; };

    // A compiled image carried by the graph's own config, for a part with no
    // compiler on it. It arrives as chunks, the first led by the whole
    // length, and is copied as it comes: the parameter buffer is the
    // loader's and is reused for the next module.
    7, image, blob, 0
        => |s, d, len| { preload_chunk(s, d, len); };
}

/// Copy `src` over `dst` when the lengths agree, answering whether it did.
/// `copy_from_slice` on lengths the compiler cannot prove equal would carry a
/// panic path, and a module image carries none.
fn copy_into(dst: &mut [u8], src: &[u8]) -> bool {
    if dst.len() != src.len() {
        return false;
    }
    for (to, from) in dst.iter_mut().zip(src) {
        *to = *from;
    }
    true
}

/// Append one chunk of the `image` parameter to the image buffer.
fn preload_chunk(state: &mut State, data: *const u8, length: usize) {
    if data.is_null() || length == 0 {
        return;
    }
    // SAFETY: the params reader hands a pointer valid for `length` bytes.
    let chunk = unsafe { core::slice::from_raw_parts(data, length) };
    let body = if state.preloaded {
        chunk
    } else {
        // The first chunk opens with the blob's whole length, which the
        // chunks themselves add up to; the bytes after it are image.
        state.preloaded = true;
        state.image_length = 0;
        chunk.get(2..).unwrap_or(&[])
    };
    let at = state.image_length;
    let copied = state
        .image
        .get_mut(at..at + body.len())
        .is_some_and(|room| copy_into(room, body));
    if copied {
        state.image_length = at + body.len();
    } else {
        state.overflowed = true;
    }
}

/// Say how the task ended, for a graph that wired nowhere to send it: on a
/// board, the log is the only place an outcome can be read.
fn log_outcome(state: &State, syscalls: &SyscallTable) {
    let mut line = trace::Line::<PRINT_LINE>::new();
    if state.failed {
        line.text(b"[iso] failed");
        if state.has_diagnostic {
            if let Some(diagnostic) = Diagnostic::decode(&state.diagnostic) {
                line.text(b" diagnostic=")
                    .number(u32::from(diagnostic.code()));
            }
        }
    } else {
        line.text(if state.rejected {
            b"[iso] rejected "
        } else {
            b"[iso] done "
        });
        let result = state.result.get(..state.result_length).unwrap_or(&[]);
        line.text(result.strip_suffix(b"\n").unwrap_or(result));
    }
    trace::write(syscalls, line.bytes());
}

/// Send what `print` wrote to the kernel log, one `[js]` line per line.
fn drain_printed(state: &mut State, syscalls: &SyscallTable) {
    let printed = state.printed.get(..state.printed_length).unwrap_or(&[]);
    for text in printed.split(|&byte| byte == b'\n') {
        if text.is_empty() {
            continue;
        }
        let mut line = trace::Line::<PRINT_LINE>::new();
        line.text(b"[js] ").text(text);
        trace::write(syscalls, line.bytes());
    }
    state.printed_length = 0;
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
    //
    // A surfaced task is the exception: it is an event loop, in the running
    // phase for as long as the board is up, so "every step while running"
    // is every step for good — a line a step floods the kernel's log ring
    // and evicts the boot lines before anything attaches to read them. It
    // has no single step to bracket either, so it takes the rate like an
    // idle wait does, and only its staging step is traced unconditionally.
    let one_shot_running = state.phase == 1 && !state.surface;
    let image_length = image_bytes(state).map_or(0, <[u8]>::len);
    let staging = state.phase == 0 && image_length > 0;
    let close_to_work = one_shot_running || staging;
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
        .number(u32::try_from(image_length).unwrap_or(u32::MAX));
    line.text(b" fuel=")
        .number(u32::try_from(state.fuel_spent).unwrap_or(u32::MAX));
    line.text(b" calls=").number(state.calls_made);
    line.text(b" us=")
        .number(u32::try_from(now).unwrap_or(u32::MAX));
    line.text(b" start=").number(state.start_us);
    line.text(b" slice_max=").number(state.slice_us_max);
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
    let bytes = image_bytes(state)?;
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

/// The admitted image: the program in place where the kernel mapped it, or
/// what was staged into `image`.
fn image_bytes(state: &State) -> Option<&[u8]> {
    if state.program_addr != 0 {
        // SAFETY: the kernel answered this span as the caller's own program,
        // readable for the module's lifetime; a slot that is running is never
        // written, so the bytes do not change under the machine.
        return Some(unsafe {
            core::slice::from_raw_parts(
                state.program_addr as *const u8,
                state.program_size as usize,
            )
        });
    }
    state.image.get(..state.image_length)
}

const PROGRAM_UNASKED: u8 = 0;
const PROGRAM_COPYING: u8 = 1;
const PROGRAM_LOADED: u8 = 2;
const PROGRAM_ABSENT: u8 = 3;
/// Bytes of a program copied a step: a flash read, well inside a deadline.
const PROGRAM_CHUNK: usize = 2048;
/// The `kind` a graph names a Phasor image by, hashed as the packer hashes
/// it (the SDK's FNV-1a).
const PROGRAM_KIND: u32 = fnv1a(b"phasor");

enum Program {
    /// Still being copied: ask again next step.
    Pending,
    /// The image is staged, or readable in place.
    Loaded,
    /// The bundle carries no program for this instance.
    Absent,
    /// It carries one this build will not run, said by name.
    Refused(Diagnostic),
}

/// The image from the bundle's program section, when the graph named one for
/// this instance: refused by its declared kind or format before a byte is
/// copied, then read in place where the kernel gives an address, or copied a
/// chunk a step where it does not. Admission still checks the image's own
/// digests and verifies it, as it does an image from anywhere.
fn load_program(state: &mut State, syscalls: &SyscallTable) -> Program {
    use abi::kernel_abi::{program as section, PROGRAM_INFO, PROGRAM_READ};
    match state.program_phase {
        PROGRAM_LOADED => return Program::Loaded,
        PROGRAM_ABSENT => return Program::Absent,
        PROGRAM_COPYING => {}
        _ => {
            let mut info = [0u8; section::INFO_LEN];
            // SAFETY: `info` is valid for `INFO_LEN` bytes; slot 0 is asked.
            let answered = unsafe {
                (syscalls.provider_call)(-1, PROGRAM_INFO, info.as_mut_ptr(), info.len())
            };
            if answered < 0 {
                state.program_phase = PROGRAM_ABSENT;
                return Program::Absent;
            }
            let word = |at: usize| {
                u32::from_le_bytes([info[at], info[at + 1], info[at + 2], info[at + 3]])
            };
            let address = u64::from(word(0)) | (u64::from(word(4)) << 32);
            let size = word(8);
            let kind = word(12);
            if kind != 0 && kind != PROGRAM_KIND {
                state.program_phase = PROGRAM_ABSENT;
                return Program::Refused(
                    Diagnostic::at(code::MALFORMED_IMAGE, Severity::Error, 0)
                        .with(diagnostic::image::MAGIC),
                );
            }
            let format = &info[32..64];
            if format.iter().any(|byte| *byte != 0) && format != &bytecode::format_digest().0[..] {
                state.program_phase = PROGRAM_ABSENT;
                return Program::Refused(
                    Diagnostic::at(code::BYTECODE_FORMAT_MISMATCH, Severity::Error, 0)
                        .with(diagnostic::image::FORMAT_DIGEST),
                );
            }
            state.program_size = size;
            state.image_length = 0;
            if let Ok(address) = usize::try_from(address) {
                if address != 0 {
                    state.program_addr = address;
                    state.program_phase = PROGRAM_LOADED;
                    return Program::Loaded;
                }
            }
            if size as usize > IMAGE_CAPACITY {
                state.overflowed = true;
                state.program_phase = PROGRAM_LOADED;
                return Program::Loaded;
            }
            state.program_phase = PROGRAM_COPYING;
        }
    }
    let size = state.program_size as usize;
    let at = state.image_length;
    if at < size {
        let want = core::cmp::min(PROGRAM_CHUNK, IMAGE_CAPACITY - at);
        // The request rides at the head of the buffer it is answered in.
        let mut small = [0u8; 8];
        let (buffer, length): (*mut u8, usize) = match state.image.get_mut(at..at + want) {
            Some(room) if room.len() >= 8 => (room.as_mut_ptr(), room.len()),
            _ => (small.as_mut_ptr(), small.len()),
        };
        let offset = (at as u32).to_le_bytes();
        // SAFETY: `buffer` is valid for `length` (at least 8) bytes.
        let got = unsafe {
            core::ptr::write_bytes(buffer, 0, 4);
            core::ptr::copy_nonoverlapping(offset.as_ptr(), buffer.add(4), 4);
            (syscalls.provider_call)(-1, PROGRAM_READ, buffer, length)
        };
        if got <= 0 {
            // Ended short of its size: what is staged is not the program.
            state.program_phase = PROGRAM_ABSENT;
            return Program::Refused(
                Diagnostic::at(code::MALFORMED_IMAGE, Severity::Error, 0)
                    .with(diagnostic::image::TRUNCATED),
            );
        }
        let got = got as usize;
        if buffer == small.as_mut_ptr() {
            let tail = state.image.get_mut(at..at + got).filter(|_| got <= 8);
            if !tail.is_some_and(|room| copy_into(room, &small[..got])) {
                state.overflowed = true;
            }
        }
        state.image_length = at + got;
        return Program::Pending;
    }
    state.program_phase = PROGRAM_LOADED;
    Program::Loaded
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

/// Asked to drain — by a reconfiguration, or by the runtime stopping —
/// the task ends as a cancel ends it: at the machine's next safe point,
/// with its outcome sent, and the step after that answers Done. A task is
/// not carried across a drain; RFC 0001 has a paused job not
/// hot-replaceable, and what comes back after one is a new task.
///
/// A task that has not started yet has nothing to wind down, and ends now.
#[cfg_attr(not(feature = "host-test"), unsafe(no_mangle))]
#[link_section = ".text.module_drain"]
pub extern "C" fn module_drain(state: *mut u8) -> i32 {
    if state.is_null() {
        return -1;
    }
    // SAFETY: as for `module_step`; the loader drains between steps.
    let state = unsafe { &mut *state.cast::<State>() };
    state.cancel_requested = true;
    if state.phase == 0 {
        state.diagnostic =
            Diagnostic::at(diagnostic::termination::CANCELLED, Severity::Error, 0).encode();
        state.has_diagnostic = true;
        state.failed = true;
        state.phase = 2;
    }
    0
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
    // The image comes on `image_in` unless the graph carried it as a
    // parameter; the result goes out on `result_out` when one is wired. A
    // program that runs for the life of the board has no result to wait on.
    if state.syscalls.is_null() {
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
        if !state.preloaded {
            match load_program(state, syscalls) {
                Program::Pending => return 0,
                Program::Loaded => {}
                Program::Refused(diagnostic) => {
                    state.diagnostic = diagnostic.encode();
                    state.has_diagnostic = true;
                    state.failed = true;
                    state.phase = 2;
                    return 0;
                }
                // No parameter and no program: the image comes on the
                // stream, or from nowhere.
                Program::Absent => {
                    if state.image_in < 0 {
                        return -2;
                    }
                    if !stage_image(state, syscalls) {
                        return 0;
                    }
                }
            }
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
        if image_bytes(state).map_or(0, <[u8]>::len) == 0 {
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
        // The realm first, a stage a step, so that no step builds more of it
        // than a cooperative scheduler can wait for.
        if !state.building.done() {
            if agent::build(agent_storage!(state), &mut state.building).is_err() {
                state.failed = true;
                state.phase = 2;
            }
            return 0;
        }
        // Then a seed, for a surfaced task with an entropy grant: the program
        // may ask for randomness in the call, so the generator it draws from
        // is keyed before it runs.
        if !state.seed_done && !draw_seed(state, syscalls) {
            return 0;
        }
        // SAFETY: the table is the loader's, live for the module's lifetime.
        let began = unsafe { dev_micros(syscalls) };
        let started = start(state);
        if state.trace != 0 {
            // SAFETY: as above.
            let took = unsafe { dev_micros(syscalls) }.saturating_sub(began);
            state.start_us = u32::try_from(took).unwrap_or(u32::MAX);
        }
        if !started {
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
        // SAFETY: the table is the loader's, live for the module's lifetime.
        let began = unsafe { dev_micros(syscalls) };
        let advanced = advance(state);
        // SAFETY: as above.
        let took = unsafe { dev_micros(syscalls) }.saturating_sub(began);
        state.slice_us_max = state
            .slice_us_max
            .max(u32::try_from(took).unwrap_or(u32::MAX));
        if state.trace != 0 && took > 2000 {
            let mut line = trace::Line::<200>::new();
            line.text(b"[iso] slow slice us=")
                .number(u32::try_from(took).unwrap_or(u32::MAX))
                .text(b" step=")
                .number(state.trace_steps)
                .text(b" fuel=")
                .number(u32::try_from(state.fuel_spent).unwrap_or(u32::MAX))
                .text(b" calls=")
                .number(state.calls_made)
                .text(b" setup=")
                .number(u32::try_from(state.body_from_us.saturating_sub(began)).unwrap_or(0))
                .text(b" body=")
                .number(
                    u32::try_from(state.body_to_us.saturating_sub(state.body_from_us)).unwrap_or(0),
                )
                .text(b" collections=")
                .number(state.collections);
            trace::write(syscalls, line.bytes());
        }
        if state.surface {
            drain_printed(state, syscalls);
        }
        match advanced {
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
        if state.result_out >= 0
            && !wire::push_progress(
                syscalls,
                state.result_out,
                result,
                &mut state.result_written,
            )
        {
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
        if state.result_out < 0 {
            log_outcome(state, syscalls);
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
