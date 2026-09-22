//! One ECMAScript Agent over module-owned storage, brought up the same way by
//! every host.
//!
//! The isolate, the execution oracle, and the probes all borrow the same
//! buffers, build a heap, an atom table, a job queue, and a binding table over
//! them, make or restore a realm, and attach every seam to one machine in one
//! order. This file is that bring-up, once. It owns nothing: every buffer is
//! the caller's, sized by the caller's constants, and the machine lives only
//! for the closure the caller hands in — which is what lets a bounded step
//! yield and a later step adopt the same storage.
//!
//! The limits are `policy::Policy`: the fuel a task may burn and the size of a
//! collection slice come from it, and a host that admits against a policy
//! runs under the same numbers it admitted.

#![allow(
    unexpected_cfgs,
    reason = "the omit flags belong to the variants of the modules that can leave a library area out; a module that declares no variant receives no matching --check-cfg, and for it every flag is absent, which is the whole language"
)]

use core::ffi::c_void;

use crate::binding::{Binding, Bindings, CallRecord, Pending};
use crate::bytecode::Unit;
use crate::heap::{Heap, Slot};
use crate::job::{Job, Queue};
use crate::object::ObjectError;
use crate::policy::Policy;

use crate::realm::{self, Realm};
use crate::regexp::Choice;
use crate::string::Atoms;
use crate::value::{Handle, Value};
use crate::vm::{CompileFn, Frame, ModuleInstance, Saves, Vm};

/// Why the machine could not be brought up.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentError {
    /// No unit to run: the caller admitted nothing.
    NoUnit,
    /// The realm could not be made in the heap given.
    Realm(ObjectError),
    /// A binding the host asked the table to admit was refused.
    Binding,
}

/// Every buffer a machine borrows, declared once by name.
pub struct Storage<'a> {
    pub arena: &'a mut [u8],
    pub slots: &'a mut [Slot],
    pub worklist: &'a mut [u32],
    pub entries: &'a mut [u32],
    pub handles: &'a mut [Handle],
    pub frames: &'a mut [Frame],
    pub registers: &'a mut [Value],
    pub roots: &'a mut [Handle],
    pub jobs: &'a mut [Job],
    pub choices: &'a mut [Choice],
    pub undo: &'a mut [(u8, u32)],
    pub subject: &'a mut [u16],
    pub descriptors: &'a mut [Binding],
    pub pending: &'a mut [Pending],
    pub outbox: &'a mut [CallRecord],
    /// Where a call's payload bytes are staged. A host whose bindings answer
    /// with numbers alone attaches none.
    pub payloads: Option<&'a mut [u8]>,
    /// Where the resources a provider opened are held, addressed by handle.
    /// A host whose bindings open none attaches none.
    pub resources: Option<&'a mut [crate::binding::Resource]>,
}

/// Build the borrowed `Storage` from any struct holding the machine's buffers
/// under their standard names.
#[allow(unused_macros, reason = "consumed by the fmods that include this file")]
macro_rules! agent_storage {
    ($s:expr) => {
        agent::Storage {
            arena: &mut $s.arena,
            slots: &mut $s.slots,
            worklist: &mut $s.worklist,
            entries: &mut $s.entries,
            handles: &mut $s.handles,
            frames: &mut $s.frames,
            registers: &mut $s.registers,
            roots: &mut $s.roots,
            jobs: &mut $s.jobs,
            #[cfg(not(feature = "omit_regexp"))]
            choices: &mut $s.choices,
            #[cfg(not(feature = "omit_regexp"))]
            undo: &mut $s.undo,
            #[cfg(not(feature = "omit_regexp"))]
            subject: &mut $s.subject,
            // No engine to match with, so no storage to match in: a host
            // built without regular expressions declares none of these.
            #[cfg(feature = "omit_regexp")]
            choices: &mut [],
            #[cfg(feature = "omit_regexp")]
            undo: &mut [],
            #[cfg(feature = "omit_regexp")]
            subject: &mut [],
            descriptors: &mut $s.descriptors,
            pending: &mut $s.pending,
            outbox: &mut $s.outbox,
            payloads: None,
            resources: None,
        }
    };
}

/// A linked closure the machine runs in place of one unit: the instance
/// records, one per module in evaluation order, and the table saying where
/// each module's resolved imports start.
pub struct Closure<'a> {
    pub instances: &'a mut [ModuleInstance],
    pub imports: &'a [(u32, u32)],
}

/// A compiler the machine asks in place rather than pausing for a host to
/// answer: the state pointer it is called back with, the entry that compiles,
/// and the units a compiled source becomes.
pub struct InPlaceCompiler<'a, 'u> {
    pub state: *mut c_void,
    pub compile: CompileFn<'u>,
    pub units: &'a mut [Unit<'u>],
}

/// What a host attaches beyond the storage: the bindings it admits, a
/// linked closure's instances and
/// resolved imports, the names a dynamic import resolves against, the cycle
/// pairs that share an error, and a compiler the machine asks in place.
pub struct Attachments<'a, 'u> {
    /// Bindings a fresh table admits before the machine runs, by digest.
    pub admitted: &'a [Binding],
    pub modules: Option<Closure<'a>>,
    pub module_names: &'a [([u8; 128], usize, u32)],
    pub module_cycles: &'a [(u32, u32)],
    pub compiler: Option<InPlaceCompiler<'a, 'u>>,
    /// Where a host-installed `print` writes, and how much of it is filled.
    pub print: Option<(&'a mut [u8], &'a mut usize)>,
}

impl<'a, 'u> Attachments<'a, 'u> {
    /// A script alone: nothing attached beyond its storage.
    pub const NONE: Self = Self {
        admitted: &[],
        modules: None,
        module_names: &[],
        module_cycles: &[],
        compiler: None,
        print: None,
    };
}

/// How the machine is metered and traced for one bring-up.
#[derive(Clone, Copy)]
pub struct Metering<'p> {
    /// The fuel and the collection slice come from here.
    pub policy: &'p Policy,
    /// Free arena below which the machine collects rather than waiting to
    /// fail: a collector tuning, not a limit, so it sits beside the policy.
    pub collection_headroom: u32,
    /// The trace context every call the machine makes carries.
    pub trace: u64,
}

/// Bring the machine up over fresh storage: a new heap, a new atom table, a
/// realm made here, and every seam attached; then run `step` and save.
///
/// The realm and the saves come back so a later `adopt` can continue.
pub fn fresh<'u, R>(
    units: &[Unit<'u>],
    mut storage: Storage<'_>,
    attach: Attachments<'_, 'u>,
    metering: Metering<'_>,
    step: impl FnOnce(&mut Vm<'_, 'u, '_, '_>) -> R,
) -> Result<(R, Realm, Saves), AgentError> {
    if units.is_empty() {
        return Err(AgentError::NoUnit);
    }
    let mut heap = Heap::with_worklist(storage.arena, storage.slots, storage.worklist);
    let mut atoms = Atoms::new(storage.entries, storage.handles);
    let realm = realm::create(&mut heap, &mut atoms).map_err(AgentError::Realm)?;
    let mut queue = Queue::new(storage.jobs);
    let mut bindings = Bindings::new(storage.descriptors, storage.pending);
    if let Some(resources) = storage.resources.take() {
        bindings.attach_resources(resources);
    }
    for binding in attach.admitted {
        if bindings.admit(*binding).is_err() {
            return Err(AgentError::Binding);
        }
    }
    let (result, saves) = run(
        units,
        &mut heap,
        &mut atoms,
        &mut queue,
        &mut bindings,
        storage.frames,
        storage.registers,
        storage.roots,
        storage.choices,
        storage.undo,
        storage.subject,
        storage.outbox,
        storage.payloads,
        realm,
        attach,
        metering,
        None,
        step,
    );
    Ok((result, realm, saves))
}

/// Bring the machine up over storage a previous step left: the heap, atom
/// table, job queue, and binding table restore from `saves`, the realm is the
/// one made before, and every seam attaches again; then run `step` and save.
pub fn adopt<'u, R>(
    units: &[Unit<'u>],
    mut storage: Storage<'_>,
    attach: Attachments<'_, 'u>,
    metering: Metering<'_>,
    realm: Realm,
    saves: &Saves,
    step: impl FnOnce(&mut Vm<'_, 'u, '_, '_>) -> R,
) -> Result<(R, Saves), AgentError> {
    if units.is_empty() {
        return Err(AgentError::NoUnit);
    }
    let mut heap = Heap::adopt(storage.arena, storage.slots, storage.worklist, &saves.heap);
    let mut atoms = Atoms::adopt(storage.entries, storage.handles, &saves.atoms);
    let mut queue = Queue::new(storage.jobs);
    let mut bindings = Bindings::adopt(storage.descriptors, storage.pending, &saves.bindings);
    if let Some(resources) = storage.resources.take() {
        bindings.adopt_resources(resources);
    }
    Ok(run(
        units,
        &mut heap,
        &mut atoms,
        &mut queue,
        &mut bindings,
        storage.frames,
        storage.registers,
        storage.roots,
        storage.choices,
        storage.undo,
        storage.subject,
        storage.outbox,
        storage.payloads,
        realm,
        attach,
        metering,
        Some(saves),
        step,
    ))
}

/// The one attachment sequence: regular expressions, jobs, bindings, the
/// collector, the trace, then the modules and the compiler, then — when
/// continuing — the saved machine state.
#[expect(
    clippy::too_many_arguments,
    reason = "the seams are the caller's storage, borrowed one by one for exactly one machine's lifetime"
)]
fn run<'a, 'u, R>(
    units: &'a [Unit<'u>],
    heap: &'a mut Heap<'_>,
    atoms: &'a mut Atoms<'_>,
    queue: &'a mut Queue<'a>,
    bindings: &'a mut Bindings<'a>,
    frames: &'a mut [Frame],
    registers: &'a mut [Value],
    roots: &'a mut [Handle],
    choices: &'a mut [Choice],
    undo: &'a mut [(u8, u32)],
    subject: &'a mut [u16],
    outbox: &'a mut [CallRecord],
    payloads: Option<&'a mut [u8]>,
    realm: Realm,
    attach: Attachments<'a, 'u>,
    metering: Metering<'_>,
    saves: Option<&Saves>,
    step: impl FnOnce(&mut Vm<'_, 'u, '_, '_>) -> R,
) -> (R, Saves) {
    // `fresh` and `adopt` refuse an empty slice before reaching here.
    let unit = &units[0];
    let mut machine = Vm::new(
        unit,
        heap,
        atoms,
        frames,
        registers,
        realm,
        metering.policy.fuel,
    );
    #[cfg(not(feature = "omit_regexp"))]
    machine.attach_regexp(choices, undo, subject);
    // Without the engine there is nothing to attach them to. The slices are
    // the empty ones the storage macro supplies on such a build.
    #[cfg(feature = "omit_regexp")]
    let _ = (choices, undo, subject);
    machine.attach_jobs(queue);
    machine.attach_bindings(bindings, outbox);
    if let Some(payloads) = payloads {
        machine.attach_payloads(payloads);
    }
    machine.attach_collector(
        roots,
        metering.policy.collection_slice,
        metering.collection_headroom,
    );
    machine.set_trace(metering.trace);
    if let Some(closure) = attach.modules {
        machine.attach_modules(units, closure.instances, closure.imports);
    }
    if !attach.module_names.is_empty() {
        machine.attach_module_names(attach.module_names);
    }
    if !attach.module_cycles.is_empty() {
        machine.attach_module_cycles(attach.module_cycles);
    }
    if let Some(compiler) = attach.compiler {
        machine.attach_compiler(compiler.state, compiler.compile, compiler.units);
    }
    if let Some((sink, length)) = attach.print {
        machine.attach_print(sink, length);
    }
    if let Some(saves) = saves {
        machine.restore_all(saves);
    }
    let result = step(&mut machine);
    let saves = machine.save();
    (result, saves)
}
