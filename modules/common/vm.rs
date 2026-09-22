//! The bytecode interpreter.
//!
//! One virtual machine is one running Agent: a frame stack, a register file, an
//! accumulator, and a fuel budget, all over caller-provided storage. Execution
//! is a loop over verified instructions, so no instruction needs to re-check
//! what the verifier already proved.
//!
//! Every operation that the language defines as calling something, such as an
//! accessor or a `valueOf`, is performed by pushing a frame and continuing the
//! same loop, so a call needs no host recursion and stays inside the budget.

#![allow(
    unexpected_cfgs,
    reason = "the omit flags belong to the variants of the modules that can leave a library area out; a module that declares no variant receives no matching --check-cfg, and for it every flag is absent, which is the whole language"
)]

use core::ffi::c_void;

use crate::binding::{Bindings, CallError, CallRecord, Cause, CompletionRecord, Disposition};
use crate::bytecode::function_flag as record_flag;
use crate::bytecode::{decode, ConstantKind, Instruction, Opcode, Unit};
use crate::env::{self, EnvironmentKind};
use crate::heap::Heap;
use crate::job::{Job, JobKind, Queue};
use crate::object::{self, attribute, Assignment, Descriptor, Lookup};
use crate::promise;
use crate::realm::{native, ErrorKind, Realm};
use crate::string::{self, Atoms, Key};
use crate::value::Handle;
use crate::value::{self, Tag, Value};

// The machine is one type across these files: each child adds `impl Vm`
// blocks and sees the parent through `use super::*`; the parent sees what a
// child marks `pub(super)` through these globs.
#[path = "vm/bigints.rs"]
mod bigints;
#[path = "vm/buffers.rs"]
mod buffers;
#[path = "vm/calls.rs"]
mod calls;
#[path = "vm/coercion.rs"]
mod coercion;
#[path = "vm/coroutines.rs"]
mod coroutines;
#[path = "vm/date.rs"]
mod date;
#[path = "vm/eval.rs"]
mod eval;
#[path = "vm/host.rs"]
mod host;
#[path = "vm/iteration.rs"]
mod iteration;
#[path = "vm/json.rs"]
#[cfg(not(feature = "omit_json"))]
mod json;
#[path = "vm/modules.rs"]
mod modules;
#[path = "vm/names.rs"]
mod names;
#[path = "vm/natives/mod.rs"]
mod natives;
#[path = "vm/privates.rs"]
mod privates;
#[path = "vm/properties.rs"]
mod properties;
#[path = "vm/proxy.rs"]
mod proxy;
#[path = "vm/realms.rs"]
mod realms;
#[path = "vm/regexps.rs"]
#[cfg(not(feature = "omit_regexp"))]
mod regexps;
use bigints::*;
use buffers::*;
use calls::*;
use coercion::*;
use coroutines::*;
use date::*;
use eval::*;
use host::*;
use iteration::*;
#[cfg(not(feature = "omit_json"))]
use json::*;
use modules::*;
use names::*;
use natives::*;
use privates::*;
use properties::*;
use proxy::*;
use realms::*;
#[cfg(not(feature = "omit_regexp"))]
use regexps::*;

/// How a suspended frame is being resumed.
pub mod resume {
    /// An ordinary `next` or fulfilled await: the value lands in the
    /// accumulator.
    pub const NEXT: u8 = 0;
    /// A `throw` or rejected await.
    pub const THROW: u8 = 1;
    /// A `return` requested of a suspended generator.
    pub const RETURN: u8 = 2;
}

/// Why execution stopped without a value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Termination {
    /// The instruction budget ran out.
    FuelExhausted,
    /// A bounded queue or table was full.
    QuotaExceeded,
    /// The host asked for the task to stop.
    Cancelled,
    /// The wall-clock deadline passed.
    DeadlineReached,
    /// The call stack reached its admitted depth.
    StackOverflow,
    /// The register file has no room for a frame.
    RegistersExhausted,
    /// The heap could not satisfy an allocation.
    HeapExhausted,
    /// A construct the interpreter does not implement was reached.
    NotImplemented,
    /// The image and the interpreter disagree, which the verifier should have
    /// prevented.
    Malformed,
}

impl Termination {
    /// The diagnostic code this termination reports as.
    pub const fn code(self) -> u16 {
        use crate::diagnostic::termination as reason;
        match self {
            Self::FuelExhausted => reason::FUEL_EXHAUSTED,
            Self::QuotaExceeded => reason::QUOTA_EXCEEDED,
            Self::Cancelled => reason::CANCELLED,
            Self::DeadlineReached => reason::DEADLINE_REACHED,
            Self::StackOverflow => reason::STACK_OVERFLOW,
            Self::RegistersExhausted => reason::REGISTERS_EXHAUSTED,
            Self::HeapExhausted => reason::HEAP_EXHAUSTED,
            Self::NotImplemented => reason::NOT_IMPLEMENTED,
            Self::Malformed => reason::MALFORMED_IMAGE_AT_RUN_TIME,
        }
    }
}

/// How a run ended.
#[derive(Clone, Copy, Debug)]
pub enum Completion {
    /// The program produced a value.
    Value(Value),
    /// The program threw. The value is what it threw.
    Throw(Value),
    /// Execution stopped for a reason the program cannot catch.
    Terminated(Termination),
}

/// One call frame.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    code: u32,
    pc: u32,
    base: u32,
    registers: u32,
    environment: Value,
    this: Value,
    /// The function object this frame is running, which is what a function's
    /// own name refers to inside it.
    callee: Value,
    /// Contexts this frame has pushed and not popped, so unwinding can leave
    /// exactly the ones the protected code entered.
    contexts: u32,
    /// The module this frame's code belongs to, which is what says which unit
    /// its constants and functions come from.
    module: u32,
    /// Whether the frame is building an instance, in which case a return of
    /// anything but an object answers the instance instead.
    construct: bool,
    /// How many arguments the call actually supplied, which is what the
    /// `arguments` object reports rather than the declared parameter count.
    argument_count: u32,
    /// The promise an async call answers, settled by how the frame ends.
    /// `undefined` on every other frame.
    promise: Value,
    /// A derived constructor's `this` waits in its dead zone until
    /// `super()` binds it.
    this_pending: bool,
    /// How the frame was last resumed — 0 `next`, 1 `throw`, 2 `return` —
    /// which is what a delegating `yield*` reads to forward the request.
    resume_kind: u8,
    /// A sync generator frame resumed on this loop, in place of the `next`
    /// call that woke it: its yield or return answers that call with an
    /// iteration result, and an eval inside it can pause the machine.
    direct_resume: bool,
}

impl Frame {
    /// An unused frame slot, for building frame storage.
    pub const EMPTY: Self = Self {
        code: 0,
        pc: 0,
        base: 0,
        registers: 0,
        environment: Value::UNDEFINED,
        this: Value::UNDEFINED,
        callee: Value::UNDEFINED,
        contexts: 0,
        module: 0,
        construct: false,
        argument_count: 0,
        promise: Value::UNDEFINED,
        this_pending: false,
        resume_kind: 0,
        direct_resume: false,
    };
}

impl Completion {
    /// The heap, or its handle table, has no room: an uncatchable stop.
    pub const HEAP_EXHAUSTED: Self = Self::Terminated(Termination::HeapExhausted);
    /// The image asked for something no verified image can: a cell that is
    /// not there, a constant out of range. An uncatchable stop.
    pub const MALFORMED: Self = Self::Terminated(Termination::Malformed);
    /// A declared bound was reached: an uncatchable stop.
    pub const QUOTA_EXCEEDED: Self = Self::Terminated(Termination::QuotaExceeded);

    /// The policy outcome this completion is, which is the vocabulary a host
    /// and a recording use.
    pub const fn outcome(&self) -> crate::policy::Outcome {
        use crate::policy::Outcome;
        match self {
            Self::Value(_) => Outcome::Returned,
            Self::Throw(_) => Outcome::Threw,
            Self::Terminated(termination) => match termination {
                Termination::FuelExhausted => Outcome::FuelExhausted,
                Termination::Cancelled => Outcome::Cancelled,
                Termination::DeadlineReached => Outcome::DeadlineReached,
                Termination::HeapExhausted => Outcome::HeapExhausted,
                Termination::StackOverflow => Outcome::StackOverflow,
                Termination::RegistersExhausted | Termination::QuotaExceeded => {
                    Outcome::QuotaExceeded
                }
                Termination::NotImplemented | Termination::Malformed => Outcome::ImageRejected,
            },
        }
    }
}

/// What a host asks of a running task between slices.
///
/// The engine never reads a clock: the host supplies the current time, and the
/// engine compares it with the deadline it was given. That keeps time a
/// capability rather than an ambient power, and keeps the comparison
/// deterministic for a replay.
#[derive(Clone, Copy, Debug, Default)]
pub struct Control {
    cancelled: bool,
    /// The time the task must not run past, in the host's own units, or zero
    /// for no deadline.
    deadline: u64,
    /// The current time, as the host last reported it.
    now: u64,
}

impl Control {
    /// Ask the running task to stop at its next safe point.
    pub fn cancel(&mut self) {
        self.cancelled = true;
    }

    /// Whether a stop has been asked for.
    pub const fn cancelled(&self) -> bool {
        self.cancelled
    }

    /// Set the deadline, in the host's time units.
    pub fn set_deadline(&mut self, deadline: u64) {
        self.deadline = deadline;
    }

    /// Report the current time. A host that has no clock never calls this, and
    /// no deadline can then pass.
    pub fn observe(&mut self, now: u64) {
        self.now = now;
    }

    /// Whether the deadline has passed.
    pub const fn expired(&self) -> bool {
        self.deadline != 0 && self.now >= self.deadline
    }
}

/// Whether a task finished within the slice it was given.
#[derive(Clone, Copy, Debug)]
pub enum Progress {
    /// The task is at a safe point and should be resumed.
    Running,
    /// The task ended.
    Finished(Completion),
}

/// A compiler the machine can ask for an eval's unit in place. The machine
/// pauses for an eval only on its outermost loop; a promise job or a
/// native's callback runs nested on the host's stack, where the only way to
/// an eval is to compile it there and then. The function is handed the
/// state pointer it was attached with, the heap the source string is on,
/// the request, and the table to put the unit in; it answers the slot,
/// refuses the source as a syntax error, or reports that nothing fits — in
/// which case the machine pauses, or fails the call where it cannot. (A
/// plain function and a pointer rather than a trait object: a loaded module
/// carries no vtable.)
pub type CompileFn<'u> = fn(*mut c_void, &Heap<'_>, &EvalRequest<'u>, &mut [Unit<'u>]) -> Compiled;

/// What the machine tells a compiler about a pending eval.
pub struct EvalRequest<'u> {
    /// The source text, a string on the machine's heap.
    pub source: Handle,
    /// The recorded direct-eval site — module, function, pc — if any.
    pub site: Option<(u32, u32, u32)>,
    /// The unit the site is in, for its scope record.
    pub site_unit: Option<Unit<'u>>,
    /// Whether the source is a script (`$262.evalScript`) rather than eval
    /// code.
    pub script: bool,
    /// The realm the eval runs in.
    pub realm: u8,
}

/// A compiler's answer.
pub enum Compiled {
    /// The unit is in the given slot of the compiler's table.
    Unit(usize),
    /// The source does not compile.
    Refused,
    /// No room for another unit.
    Exhausted,
}

/// The interpreter over one unit image.
/// A machine's own state, apart from the storage it was given.
///
/// An isolate that must survive between module steps cannot hold a `Vm`: it
/// borrows the storage that lives in the module's state. It holds this instead,
/// rebuilds the machine over the same storage on the next step, and restores
/// it, so a task that made a call is exactly where it was when the answer
/// arrives.
#[derive(Clone, Copy, Debug)]
pub struct Snapshot {
    depth: u32,
    top: u32,
    accumulator: Value,
    fuel: u64,
    control: Control,
    slice: u64,
    started: bool,
    current_native: Option<Handle>,
    collections: u32,
    pressed_collections: u32,
    trace: u64,
    retained: Value,
    /// The source of an eval the machine is paused on, waiting for the host
    /// to compile it into a unit.
    pending_eval: Value,
    pending_eval_module: u32,
    pending_eval_function: u32,
    pending_eval_pc: u32,
    pending_eval_environment: Value,
    pending_eval_this: Value,
    pending_eval_callee: Value,
    /// What a function built from source for a subclass `super()` or `new`
    /// takes once made: the subclass's prototype, and the class whose
    /// instance fields then run on it.
    pending_eval_prototype: Value,
    pending_eval_fields: Value,
    /// The depth of the eval frame whose return delivers that function, or
    /// `u32::MAX` while none does.
    eval_result_depth: u32,
    /// Whether the pending source is a whole script — `$262.evalScript` —
    /// rather than eval code.
    pending_eval_script: bool,
    /// How many evals have been entered: a reused unit's template sites are
    /// keyed by it, so each parse gets template objects of its own.
    eval_generation: u32,
    /// The `new.target` the next constructor entry binds instead of its
    /// callee: what `super()` forwards, or what `Reflect.construct` names.
    pending_new_target: Value,
    outbox_length: usize,
    print_status: u8,
    /// The state `Math.random` draws from: a fixed seed, so the sequence
    /// replays exactly unless the host seeds it from an entropy capability.
    random_state: u64,
    realms: [Option<Realm>; MAX_REALMS],
    unit_realm: [u8; MAX_UNIT_REALMS],
    pending_eval_realm: u8,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            depth: 0,
            top: 0,
            accumulator: Value::UNDEFINED,
            fuel: 0,
            control: Control::default(),
            slice: 0,
            started: false,
            current_native: None,
            collections: 0,
            pressed_collections: 0,
            trace: 0,
            retained: Value::UNDEFINED,
            pending_eval: Value::UNDEFINED,
            pending_eval_module: u32::MAX,
            pending_eval_function: u32::MAX,
            pending_eval_pc: u32::MAX,
            pending_eval_environment: Value::UNDEFINED,
            pending_eval_this: Value::UNDEFINED,
            pending_eval_callee: Value::UNDEFINED,
            pending_eval_prototype: Value::UNDEFINED,
            pending_eval_fields: Value::UNDEFINED,
            eval_result_depth: u32::MAX,
            pending_eval_script: false,
            eval_generation: 0,
            pending_new_target: Value::UNDEFINED,
            outbox_length: 0,
            print_status: 0,
            random_state: RANDOM_SEED,
            realms: [None; MAX_REALMS],
            unit_realm: [0; MAX_UNIT_REALMS],
            pending_eval_realm: 0,
        }
    }
}

/// One module of a linked closure, as the machine sees it.
#[derive(Clone, Copy, Debug)]
pub struct ModuleInstance {
    /// The environment holding the module's top-level bindings.
    pub environment: Value,
    /// Where this module's resolved imports start in the import table.
    pub import_base: u32,
    /// The object `import * as name` names, made when one is first asked for.
    pub namespace: Value,
    /// The object `import defer * as name` names: shaped as the namespace
    /// is, but a meaningful use evaluates the module first.
    pub deferred_namespace: Value,
    /// What evaluating the module answered: a promise, when it awaited.
    pub completion: Value,
    /// Where the body begins, past the instantiation prologue — evaluation
    /// resumes here so the closures instantiation made stay the bindings.
    pub body_pc: u32,
    /// How far the module has run: 0 untouched, 1 running, 2 done,
    /// 3 threw — with what it threw kept as the completion.
    pub evaluated: u8,
}

impl ModuleInstance {
    pub const EMPTY: Self = Self {
        environment: Value::UNDEFINED,
        import_base: 0,
        namespace: Value::UNDEFINED,
        deferred_namespace: Value::UNDEFINED,
        completion: Value::UNDEFINED,
        body_pc: 0,
        evaluated: 0,
    };
}

/// Everything a rebuilt machine needs to carry on: its own state and the state
/// of the heap, atoms, job queue, and binding table it was given.
#[derive(Clone, Copy, Debug, Default)]
pub struct Saves {
    pub machine: Snapshot,
    pub heap: crate::heap::HeapSave,
    pub atoms: crate::string::AtomsSave,
    pub queue: crate::job::QueueSave,
    pub bindings: crate::binding::BindingsSave,
}

pub struct Vm<'a, 'u, 'h, 'atoms> {
    /// The units this machine may run. A script has one; a linked module
    /// closure has one per module, and a module's index is its unit's.
    units: &'a [Unit<'u>],
    /// Where the machine starts, and what a frame belongs to when nothing said
    /// otherwise.
    entry_module: u32,
    /// The modules of a linked closure: their environments, and where their
    /// resolved imports start.
    modules: Option<&'a mut [ModuleInstance]>,
    /// Every module's imports, resolved to a module and a slot in it.
    imports: Option<&'a [(u32, u32)]>,
    /// A compiler the machine asks in place for an eval, and the table the
    /// units it makes go into, numbered after `units`.
    compiler: Option<(*mut c_void, CompileFn<'u>)>,
    extra_units: Option<&'a mut [Unit<'u>]>,
    heap: &'a mut Heap<'h>,
    atoms: &'a mut Atoms<'atoms>,
    frames: &'a mut [Frame],
    registers: &'a mut [Value],
    depth: u32,
    top: u32,
    accumulator: Value,
    fuel: u64,
    realm: Realm,
    /// Every realm this machine holds — the one it was made with first — and
    /// which of them each unit's code belongs to, so a frame runs in the
    /// realm its function was made in.
    realms: [Option<Realm>; MAX_REALMS],
    unit_realm: [u8; MAX_UNIT_REALMS],
    /// The realm the pending eval's source compiles into.
    pending_eval_realm: u8,
    control: Control,
    /// The job queue, when the host gave the machine one. Without it, promises
    /// have nowhere to schedule and say so rather than running a handler at the
    /// wrong time.
    queue: Option<&'a mut Queue<'a>>,
    /// Instructions left in the current slice.
    slice: u64,
    /// Whether a task has been started and not yet finished.
    started: bool,
    /// Whether any deferred namespace exists: only then do property walks
    /// pay to look for one on a prototype chain.
    deferred_live: bool,
    /// Whether the machine is running an instantiation pass, which ends at
    /// the module prologue's marker instead of running the body.
    instantiating: bool,
    /// The names a dynamic import resolves against: each staged module's
    /// key beside its unit.
    module_names: &'a [([u8; 128], usize, u32)],
    /// Modules in an evaluation cycle, as unit pairs sharing their errors.
    module_cycles: &'a [(u32, u32)],
    /// The native function object currently being called, for a native that
    /// needs what it was bound to.
    current_native: Option<Handle>,
    /// Storage for the roots a collection starts from, and what it costs.
    ///
    /// Without it the machine cannot collect and simply runs out of heap, which
    /// is what a host that never attached a collector asked for.
    roots_storage: Option<&'a mut [Handle]>,
    collection_slice: u32,
    collection_headroom: u32,
    collections: u32,
    /// Collections in a row that ended with the heap still under pressure.
    /// See [`PRESSED_COLLECTIONS`].
    pressed_collections: u32,
    /// The trace context every call this machine makes carries.
    trace: u64,
    /// A value the host asked the machine to keep alive: a result it is holding
    /// while the calls behind it are still outstanding.
    retained: Value,
    /// The source of an eval the machine is paused on, waiting for the host
    /// to compile it into a unit.
    pending_eval: Value,
    /// Which call paused for the eval: the module, function, and pc of the
    /// `Call` instruction, or `u32::MAX` when the call was not a recorded
    /// direct site.
    pending_eval_module: u32,
    pending_eval_function: u32,
    pending_eval_pc: u32,
    /// Where a direct eval's code runs: the caller's environment and its
    /// `this`, or undefined for global eval.
    pending_eval_environment: Value,
    pending_eval_this: Value,
    /// The function whose frame paused for the eval, whose home object the
    /// eval's `super` reads through.
    pending_eval_callee: Value,
    /// What a function built from source for a subclass `super()` or `new`
    /// takes once made: the subclass's prototype, and the class whose
    /// instance fields then run on it.
    pending_eval_prototype: Value,
    pending_eval_fields: Value,
    /// The depth of the eval frame whose return delivers that function, or
    /// `u32::MAX` while none does.
    eval_result_depth: u32,
    /// Whether the pending source is a whole script — `$262.evalScript` —
    /// rather than eval code.
    pending_eval_script: bool,
    /// How many evals have been entered: a reused unit's template sites are
    /// keyed by it, so each parse gets template objects of its own.
    eval_generation: u32,
    pending_new_target: Value,
    /// What a host-installed `print` reported: 0 none, 1 the async-test
    /// completion line, 2 anything else.
    print_status: u8,
    /// Where a host-installed `print` writes its text, when the host attached
    /// somewhere: the buffer and how much of it is filled.
    print_sink: Option<(&'a mut [u8], &'a mut usize)>,
    /// The state `Math.random` draws from: a fixed seed, so the sequence
    /// replays exactly unless the host seeds it from an entropy capability.
    random_state: u64,
    /// How many native operations on the host stack have entered JavaScript
    /// beneath them right now. Each is a real host frame, so the bound is
    /// explicit rather than whatever stack the platform happened to give.
    nested: u32,
    /// Where a match backtracks, and what it must put back when it does.
    #[cfg(not(feature = "omit_regexp"))]
    regexp_choices: Option<&'a mut [crate::regexp::Choice]>,
    #[cfg(not(feature = "omit_regexp"))]
    regexp_undo: Option<&'a mut [(u8, u32)]>,
    /// The units a match runs over, copied out of the heap.
    #[cfg(not(feature = "omit_regexp"))]
    regexp_subject: Option<&'a mut [u16]>,
    /// The bindings this isolate was granted, when a host attached any.
    bindings: Option<&'a mut Bindings<'a>>,
    /// Call records the program produced and the host has not taken.
    outbox: Option<&'a mut [CallRecord]>,
    outbox_length: usize,
    /// Where a call's payload bytes are staged, and how much is staged: the
    /// host takes them behind the records they belong to.
    payload_out: Option<&'a mut [u8]>,
    payload_length: usize,
    /// The binding whose completion is being applied, so a resource it
    /// answered with is recorded against it.
    completing_binding: u32,
}

/// The error type used inside the interpreter, where a throw and a termination
/// both stop the current operation.
type Step = Result<(), Completion>;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// A machine over `unit`, with the global object `global`.
    pub fn new(
        unit: &'a Unit<'u>,
        heap: &'a mut Heap<'h>,
        atoms: &'a mut Atoms<'atoms>,
        frames: &'a mut [Frame],
        registers: &'a mut [Value],
        realm: Realm,
        fuel: u64,
    ) -> Self {
        Self {
            units: core::slice::from_ref(unit),
            entry_module: 0,
            modules: None,
            imports: None,
            compiler: None,
            extra_units: None,
            heap,
            atoms,
            frames,
            registers,
            depth: 0,
            top: 0,
            accumulator: Value::UNDEFINED,
            fuel,
            realm,
            control: Control::default(),
            queue: None,
            slice: 0,
            started: false,
            deferred_live: false,
            instantiating: false,
            module_names: &[],
            module_cycles: &[],
            current_native: None,
            roots_storage: None,
            collection_slice: 0,
            collection_headroom: 0,
            collections: 0,
            pressed_collections: 0,
            trace: 0,
            retained: Value::UNDEFINED,
            pending_eval: Value::UNDEFINED,
            pending_eval_module: u32::MAX,
            pending_eval_function: u32::MAX,
            pending_eval_pc: u32::MAX,
            pending_eval_environment: Value::UNDEFINED,
            pending_eval_this: Value::UNDEFINED,
            pending_eval_callee: Value::UNDEFINED,
            pending_eval_prototype: Value::UNDEFINED,
            pending_eval_fields: Value::UNDEFINED,
            eval_result_depth: u32::MAX,
            pending_eval_script: false,
            eval_generation: 0,
            pending_new_target: Value::UNDEFINED,
            nested: 0,
            #[cfg(not(feature = "omit_regexp"))]
            regexp_choices: None,
            #[cfg(not(feature = "omit_regexp"))]
            regexp_undo: None,
            #[cfg(not(feature = "omit_regexp"))]
            regexp_subject: None,
            bindings: None,
            outbox: None,
            outbox_length: 0,
            payload_out: None,
            payload_length: 0,
            completing_binding: u32::MAX,
            print_status: 0,
            print_sink: None,
            random_state: RANDOM_SEED,
            realms: [Some(realm), None, None, None],
            unit_realm: [0; MAX_UNIT_REALMS],
            pending_eval_realm: 0,
        }
    }

    /// The whole machine's state, including the storage it was given, so a
    /// host that rebuilds it on the next step carries on exactly here.
    pub fn save(&self) -> Saves {
        Saves {
            machine: self.snapshot(),
            heap: self.heap.save(),
            atoms: self.atoms.save(),
            queue: match &self.queue {
                Some(queue) => queue.save(),
                None => crate::job::QueueSave::default(),
            },
            bindings: match &self.bindings {
                Some(bindings) => bindings.save(),
                None => crate::binding::BindingsSave::default(),
            },
        }
    }

    /// Carry on from a saved state over the same storage.
    pub fn restore_all(&mut self, saves: &Saves) {
        self.restore(&saves.machine);
        self.heap.restore(&saves.heap);
        self.atoms.restore(&saves.atoms);
        if let Some(queue) = self.queue.as_deref_mut() {
            queue.restore(&saves.queue);
        }
        if let Some(bindings) = self.bindings.as_deref_mut() {
            bindings.restore(&saves.bindings);
        }
    }

    /// The machine's state, to be restored onto the same storage later.
    pub const fn snapshot(&self) -> Snapshot {
        Snapshot {
            depth: self.depth,
            top: self.top,
            accumulator: self.accumulator,
            fuel: self.fuel,
            control: self.control,
            slice: self.slice,
            started: self.started,
            current_native: self.current_native,
            collections: self.collections,
            pressed_collections: self.pressed_collections,
            trace: self.trace,
            retained: self.retained,
            pending_eval: self.pending_eval,
            pending_eval_module: self.pending_eval_module,
            pending_eval_function: self.pending_eval_function,
            pending_eval_pc: self.pending_eval_pc,
            pending_eval_environment: self.pending_eval_environment,
            pending_eval_this: self.pending_eval_this,
            pending_eval_callee: self.pending_eval_callee,
            pending_eval_prototype: self.pending_eval_prototype,
            pending_eval_fields: self.pending_eval_fields,
            eval_result_depth: self.eval_result_depth,
            pending_eval_script: self.pending_eval_script,
            eval_generation: self.eval_generation,
            pending_new_target: self.pending_new_target,
            outbox_length: self.outbox_length,
            print_status: self.print_status,
            random_state: self.random_state,
            realms: self.realms,
            unit_realm: self.unit_realm,
            pending_eval_realm: self.pending_eval_realm,
        }
    }

    /// Carry on from a saved state over the same frames, registers, and heap.
    pub fn restore(&mut self, snapshot: &Snapshot) {
        self.depth = snapshot.depth;
        self.top = snapshot.top;
        self.accumulator = snapshot.accumulator;
        self.fuel = snapshot.fuel;
        self.control = snapshot.control;
        self.slice = snapshot.slice;
        self.started = snapshot.started;
        self.current_native = snapshot.current_native;
        self.collections = snapshot.collections;
        self.pressed_collections = snapshot.pressed_collections;
        self.trace = snapshot.trace;
        self.retained = snapshot.retained;
        self.pending_eval = snapshot.pending_eval;
        self.pending_eval_module = snapshot.pending_eval_module;
        self.pending_eval_function = snapshot.pending_eval_function;
        self.pending_eval_pc = snapshot.pending_eval_pc;
        self.pending_eval_environment = snapshot.pending_eval_environment;
        self.pending_eval_this = snapshot.pending_eval_this;
        self.pending_eval_callee = snapshot.pending_eval_callee;
        self.pending_eval_prototype = snapshot.pending_eval_prototype;
        self.pending_eval_fields = snapshot.pending_eval_fields;
        self.eval_result_depth = snapshot.eval_result_depth;
        self.pending_eval_script = snapshot.pending_eval_script;
        self.eval_generation = snapshot.eval_generation;
        self.pending_new_target = snapshot.pending_new_target;
        self.outbox_length = snapshot.outbox_length;
        self.print_status = snapshot.print_status;
        self.random_state = snapshot.random_state;
        if snapshot.realms[0].is_some() {
            self.realms = snapshot.realms;
        }
        self.unit_realm = snapshot.unit_realm;
        self.pending_eval_realm = snapshot.pending_eval_realm;
        self.sync_realm();
    }

    /// Collections this machine has run.
    pub const fn collections(&self) -> u32 {
        self.collections
    }

    /// Remaining fuel.
    pub const fn fuel(&self) -> u64 {
        self.fuel
    }

    /// The global object, for a host that reads what a task left behind.
    pub const fn global(&self) -> Value {
        Value::object(self.realm.global)
    }

    /// Run the unit's entry function to completion, in one go.
    pub fn run(&mut self) -> Completion {
        if let Err(completion) = self.start() {
            return completion;
        }
        loop {
            match self.resume(u64::MAX) {
                Progress::Finished(completion) => return completion,
                Progress::Running => {
                    // A host that runs to completion has no compiler to hand
                    // an eval pause to; the call fails rather than waiting
                    // for an answer that cannot come.
                    if self.pending_eval().is_some() {
                        if let Some(completion) = self.fail_eval() {
                            return completion;
                        }
                    }
                }
            }
        }
    }

    /// Run what `start_module` or `start` began, to its end.
    pub fn run_started(&mut self) -> Completion {
        loop {
            match self.resume(u64::MAX) {
                Progress::Finished(completion) => return completion,
                Progress::Running => {}
            }
        }
    }

    /// Begin the unit's entry function without running it.
    /// Start a unit as a script: the harness prelude runs this way ahead
    /// of a module closure, so what it declares lands on the global object
    /// where every module of the closure sees it.
    pub fn start_script(&mut self, unit: u32) -> Result<(), Completion> {
        self.entry_module = unit;
        self.start()
    }

    pub fn start(&mut self) -> Result<(), Completion> {
        let entry = self.unit_of(self.entry_module).header().entry_function;
        let environment = Value::object(self.realm.lexical);
        // A script's top-level `this` is the global object.
        self.push_frame(
            entry,
            environment,
            Value::object(self.realm.global),
            Value::UNDEFINED,
            self.entry_module,
        )?;
        self.started = true;
        Ok(())
    }

    /// Run at most `slice` instructions of the started task.
    ///
    /// Returning `Running` means the task stopped at a safe point with its
    /// state in the machine, which is what lets one task span several module
    /// steps.
    pub fn resume(&mut self, slice: u64) -> Progress {
        if !self.started {
            return Progress::Finished(Completion::MALFORMED);
        }
        self.slice = slice;
        match self.drive(1, true) {
            Some(completion) => {
                self.started = false;
                Progress::Finished(completion)
            }
            None => Progress::Running,
        }
    }

    /// Whether the pending source is to compile as a whole script rather
    /// than as eval code: `$262.evalScript` asked for it.
    pub const fn pending_eval_is_script(&self) -> bool {
        self.pending_eval_script
    }

    /// The realm the pending source compiles into, which makes the same
    /// text another unit in another realm.
    pub const fn pending_eval_realm(&self) -> u8 {
        self.pending_eval_realm
    }

    /// Whether a native is a constructor that builds a function from source:
    /// `Function`, `GeneratorFunction`, or `AsyncGeneratorFunction`.
    const fn builds_from_source(native: u32) -> bool {
        matches!(
            native,
            crate::realm::native::FUNCTION
                | crate::realm::native::GENERATOR_FUNCTION
                | crate::realm::native::ASYNC_GENERATOR_FUNCTION
                | crate::realm::native::ASYNC_FUNCTION
        )
    }

    /// Run until the frame that was current on entry returns.
    fn execute(&mut self) -> Completion {
        let floor = self.depth;
        loop {
            match self.drive(floor, false) {
                Some(completion) => return completion,
                None => {
                    // A nested evaluation is never sliced, so the only pause
                    // is an eval's — and on the host's own stack there is no
                    // way to hand it to the compiler, so the call fails with
                    // the syntax error it would produce, catchably.
                    if self.pending_eval().is_some() {
                        if let Some(completion) = self.fail_eval_to(floor) {
                            return completion;
                        }
                        continue;
                    }
                    return Completion::MALFORMED;
                }
            }
        }
    }

    /// The instruction loop.
    ///
    /// `sliced` marks the outermost invocation, which is the only one that may
    /// stop at a safe point and hand control back to the host.
    fn drive(&mut self, floor: u32, sliced: bool) -> Option<Completion> {
        loop {
            if self.depth < floor {
                return Some(Completion::Value(self.accumulator));
            }
            if self.pending_eval.is_string() {
                // A program called `eval`: the attached compiler answers in
                // place, else the machine waits for the host to compile the
                // source, and nothing runs until it is entered or refused.
                match self.serve_eval(floor) {
                    Served::Entered | Served::Unwound(None) => continue,
                    Served::Unwound(Some(completion)) => return Some(completion),
                    Served::Pause => return None,
                }
            }
            if self.fuel == 0 {
                return Some(Completion::Terminated(Termination::FuelExhausted));
            }
            self.fuel -= 1;
            if sliced {
                if self.slice == 0 {
                    return None;
                }
                self.slice -= 1;
                // Only the outermost loop collects: a nested evaluation holds
                // values the roots do not name.
                if let Some(stop) = self.maybe_collect() {
                    return Some(stop);
                }
            }

            let frame = self.frames[self.depth as usize - 1];
            let Some(function) = self.unit_of(frame.module).function(frame.code) else {
                return Some(Completion::MALFORMED);
            };
            let Some(code) = self.unit_of(frame.module).code(&function) else {
                return Some(Completion::MALFORMED);
            };
            let Ok(instruction) = decode(code, frame.pc) else {
                return Some(Completion::MALFORMED);
            };
            let next = frame.pc + instruction.length;
            self.frames[self.depth as usize - 1].pc = next;

            match self.step(&instruction, &frame) {
                Ok(Flow::Continue) | Ok(Flow::Enter) => {}
                Ok(Flow::Jump(target)) => {
                    // A backward edge is a safe point: it is where a loop can
                    // be interrupted, and the verifier proved one is declared
                    // there.
                    if target <= frame.pc {
                        if let Some(stop) = self.check_control() {
                            return Some(stop);
                        }
                    }
                    self.frames[self.depth as usize - 1].pc = target;
                }
                Ok(Flow::Return(value)) => {
                    // A return is a safe point.
                    if let Some(stop) = self.check_control() {
                        return Some(stop);
                    }
                    let returning = self.frames[self.depth as usize - 1];
                    let derived = returning.construct
                        && self
                            .unit_of(returning.module)
                            .function(returning.code)
                            .is_some_and(|record| {
                                record.flags & record_flag::DERIVED_CONSTRUCTOR != 0
                            });
                    // A derived constructor may return an object or
                    // undefined, and nothing else; and it may not finish
                    // without calling `super()` unless it returns an object.
                    // Either fault is the caller's to catch: the frame has
                    // finished, finalisers and all, before it is raised.
                    let fault = if derived && !value.is_object() && !value.is_undefined() {
                        Some(self.throw_type_error())
                    } else if returning.construct && returning.this_pending && !value.is_object() {
                        Some(self.throw_reference_error())
                    } else {
                        None
                    };
                    if let Some(completion) = fault {
                        let thrown = match completion {
                            Completion::Throw(reason) => reason,
                            other => return Some(other),
                        };
                        self.depth -= 1;
                        self.top = returning.base;
                        self.sync_realm();
                        if self.depth < floor {
                            return Some(Completion::Throw(thrown));
                        }
                        if let Some(completion) = self.unwind(thrown, floor) {
                            return Some(completion);
                        }
                        continue;
                    }
                    if self.eval_result_depth == self.depth {
                        // A function built from source for a subclass: it
                        // answers to the subclass, and the subclass's
                        // instance fields run on it.
                        self.eval_result_depth = u32::MAX;
                        let prototype = self.pending_eval_prototype;
                        let fields = self.pending_eval_fields;
                        self.pending_eval_prototype = Value::UNDEFINED;
                        self.pending_eval_fields = Value::UNDEFINED;
                        if value.is_object() {
                            if prototype.is_object() {
                                let _ =
                                    object::set_prototype(self.heap, value.as_handle(), prototype);
                            }
                            if fields.is_object() {
                                if let Err(completion) = self.run_class_fields(fields, value) {
                                    return Some(completion);
                                }
                            }
                        }
                    }
                    self.depth -= 1;
                    self.top = returning.base;
                    self.sync_realm();
                    // An async frame's return settles its promise, and the
                    // promise is what the caller receives.
                    if returning.promise.is_object()
                        && object::is_generator(self.heap, returning.promise.as_handle())
                            .unwrap_or(false)
                    {
                        // A generator frame's return finishes the generator;
                        // the resumer receives the return value.
                        let async_bit = self.generator_async_bit(returning.promise);
                        if object::set_generator(
                            self.heap,
                            returning.promise.as_handle(),
                            object::generator_state::DONE | async_bit,
                            Value::UNDEFINED,
                        )
                        .is_err()
                        {
                            return Some(Completion::MALFORMED);
                        }
                        if async_bit != 0 {
                            if let Err(completion) =
                                self.settle_pending_next(returning.promise, Ok(value), true)
                            {
                                return Some(completion);
                            }
                        }
                        if returning.direct_resume {
                            // The frame stood in for a `next` call: the call
                            // answers the finished generator's result.
                            match self.iteration_result(value, true) {
                                Ok(result) => self.accumulator = result,
                                Err(completion) => return Some(completion),
                            }
                        } else {
                            self.accumulator = value;
                        }
                    } else if returning.promise.is_object() {
                        if let Err(completion) = self.resolve(returning.promise.as_handle(), value)
                        {
                            return Some(completion);
                        }
                        self.accumulator = returning.promise;
                    } else {
                        // A constructor that returns anything but an object
                        // answers the instance it was building.
                        self.accumulator = if returning.construct && !value.is_object() {
                            returning.this
                        } else {
                            value
                        };
                    }
                }
                Ok(Flow::Await(value)) => {
                    // An await is a safe point.
                    if let Some(stop) = self.check_control() {
                        return Some(stop);
                    }
                    match self.suspend_await(value) {
                        Ok(()) => {
                            if self.depth < floor {
                                return Some(Completion::Value(self.accumulator));
                            }
                        }
                        Err(completion) => return Some(completion),
                    }
                }
                Ok(Flow::Begin) => match self.suspend_start() {
                    Ok(generator) => {
                        self.accumulator = generator;
                        if self.depth < floor {
                            return Some(Completion::Value(generator));
                        }
                    }
                    Err(completion) => return Some(completion),
                },
                Ok(Flow::YieldDelegate(value)) => {
                    if let Some(stop) = self.check_control() {
                        return Some(stop);
                    }
                    match self.suspend_yield_delegate() {
                        Ok(()) => {
                            self.accumulator = value;
                            if self.depth < floor {
                                return Some(Completion::Value(value));
                            }
                        }
                        Err(completion) => return Some(completion),
                    }
                }
                Ok(Flow::Yield(value, star)) => {
                    // A yield is a safe point.
                    if let Some(stop) = self.check_control() {
                        return Some(stop);
                    }
                    let yielding = self.frames[self.depth as usize - 1];
                    let keeper = yielding.promise;
                    if self.generator_async_bit(keeper) != 0 {
                        // The value was awaited before the yield, so the
                        // request is answered at once — and a delegated value
                        // passes through unawaited. A request already waiting
                        // resumes the generator in place, without suspending.
                        if let Err(completion) =
                            self.settle_pending_next_with(keeper, Ok(value), false, false)
                        {
                            return Some(completion);
                        }
                        match self.head_request(keeper) {
                            Ok(Some((argument, operation))) => {
                                let kind = if operation == native::GENERATOR_THROW {
                                    resume::THROW
                                } else if operation == native::GENERATOR_RETURN {
                                    resume::RETURN
                                } else {
                                    resume::NEXT
                                };
                                if let Some(running) = self.frames.get_mut(self.depth as usize - 1)
                                {
                                    running.resume_kind = kind;
                                }
                                self.accumulator = argument;
                                continue;
                            }
                            Ok(None) => {}
                            Err(completion) => return Some(completion),
                        }
                    }
                    match self.suspend_yield(star) {
                        Ok(()) => {
                            if yielding.direct_resume {
                                // The frame stood in for a `next` call: the
                                // call answers an iteration result.
                                match self.iteration_result(value, false) {
                                    Ok(result) => self.accumulator = result,
                                    Err(completion) => return Some(completion),
                                }
                            } else {
                                self.accumulator = value;
                            }
                            if self.depth < floor {
                                return Some(Completion::Value(self.accumulator));
                            }
                        }
                        Err(completion) => return Some(completion),
                    }
                }
                Err(Completion::Throw(thrown)) => {
                    if let Some(completion) = self.unwind(thrown, floor) {
                        return Some(completion);
                    }
                }
                Err(completion) => return Some(completion),
            }
        }
    }

    /// Observe cancellation and the deadline. Called only at safe points, so a
    /// task never stops in the middle of an operation.
    fn check_control(&self) -> Option<Completion> {
        if self.control.cancelled() {
            return Some(Completion::Terminated(Termination::Cancelled));
        }
        if self.control.expired() {
            return Some(Completion::Terminated(Termination::DeadlineReached));
        }
        None
    }

    /// Give a thrown value to the innermost handler that covers the throwing
    /// instruction, unwinding frames that have none.
    ///
    /// Returns `None` when a handler took the value and execution continues,
    /// and the completion to report when the throw leaves this run.
    fn unwind(&mut self, thrown: Value, floor: u32) -> Option<Completion> {
        while self.depth > 0 {
            let frame = self.frames[self.depth as usize - 1];
            if let Some(region) = self.handler_for(&frame) {
                // Leave every context the protected code entered, so the
                // handler runs in the environment it was compiled against.
                let mut environment = frame.environment;
                let mut open = frame.contexts;
                while open > region.context_depth {
                    if !environment.is_object() {
                        break;
                    }
                    match env::parent(self.heap, environment.as_handle()) {
                        Ok(parent) => environment = parent,
                        Err(_) => break,
                    }
                    open -= 1;
                }
                let index = self.depth as usize - 1;
                self.frames[index].pc = region.handler;
                self.frames[index].environment = environment;
                self.frames[index].contexts = region.context_depth;
                self.set_register(&frame, region.register, thrown);
                self.accumulator = thrown;
                return None;
            }
            self.depth -= 1;
            self.top = frame.base;
            self.sync_realm();
            // A throw leaving a generator frame finishes the generator and
            // keeps travelling.
            if frame.promise.is_object()
                && object::is_generator(self.heap, frame.promise.as_handle()).unwrap_or(false)
            {
                let async_bit = self.generator_async_bit(frame.promise);
                let _ = object::set_generator(
                    self.heap,
                    frame.promise.as_handle(),
                    object::generator_state::DONE | async_bit,
                    Value::UNDEFINED,
                );
                if async_bit != 0 {
                    // The rejection reaches the pending `next`'s promise;
                    // the throw itself stops at the generator's edge.
                    if let Err(completion) =
                        self.settle_pending_next(frame.promise, Err(thrown), true)
                    {
                        return Some(completion);
                    }
                    self.accumulator = Value::UNDEFINED;
                    if self.depth < floor {
                        return Some(Completion::Value(Value::UNDEFINED));
                    }
                    return None;
                }
                if self.depth < floor {
                    return Some(Completion::Throw(thrown));
                }
                continue;
            }
            // A throw leaving an async frame is that call's rejection: the
            // promise takes the reason and the caller receives the promise.
            if frame.promise.is_object() {
                if let Err(completion) =
                    self.settle(frame.promise.as_handle(), promise::REJECTED, thrown)
                {
                    return Some(completion);
                }
                self.accumulator = frame.promise;
                if self.depth < floor {
                    return Some(Completion::Value(frame.promise));
                }
                return None;
            }
            if self.depth < floor {
                return Some(Completion::Throw(thrown));
            }
        }
        Some(Completion::Throw(thrown))
    }

    /// The innermost exception region of `frame` that covers the instruction
    /// that threw, which is the one before the program counter.
    fn handler_for(&self, frame: &Frame) -> Option<crate::bytecode::ExceptionRegion> {
        let function = self.unit_of(frame.module).function(frame.code)?;
        // The program counter has already advanced past the throwing
        // instruction, so a region ending exactly at it still covers it.
        let position = frame.pc.saturating_sub(1);
        let mut found: Option<crate::bytecode::ExceptionRegion> = None;
        let mut index = 0u32;
        while index < function.exception_count {
            let region = self
                .unit()
                .exception_region(function.exception_offset + index)?;
            if region.start <= position && position < region.end {
                let better = match found {
                    Some(current) => region.start >= current.start && region.end <= current.end,
                    None => true,
                };
                if better {
                    found = Some(region);
                }
            }
            index += 1;
        }
        found
    }

    /// Whether the running frame's code is strict.
    fn frame_is_strict(&self, frame: &Frame) -> bool {
        self.unit_of(frame.module)
            .function(frame.code)
            .is_some_and(|record| record.flags & record_flag::STRICT != 0)
    }

    fn register(&self, frame: &Frame, register: u32) -> Value {
        let at = frame.base + register;
        match self.registers.get(at as usize) {
            Some(&value) => value,
            None => Value::UNDEFINED,
        }
    }

    fn set_register(&mut self, frame: &Frame, register: u32, value: Value) {
        let at = frame.base + register;
        if let Some(slot) = self.registers.get_mut(at as usize) {
            *slot = value;
        }
    }

    /// Execute one instruction.
    fn step(&mut self, instruction: &Instruction, frame: &Frame) -> Result<Flow, Completion> {
        use Opcode as Op;
        let operands = instruction.operands;
        let signed = instruction.signed;

        match instruction.opcode {
            Op::LdaUndefined => self.accumulator = Value::UNDEFINED,
            Op::LdaNull => self.accumulator = Value::NULL,
            Op::LdaTrue => self.accumulator = Value::TRUE,
            Op::LdaFalse => self.accumulator = Value::FALSE,
            Op::LdaZero => self.accumulator = Value::number(0.0),
            Op::LdaSmi => {
                self.accumulator = Value::number(crate::softfloat::from_i64(i64::from(signed[0])));
            }
            Op::LdaConstant => {
                self.accumulator = self.load_constant(operands[0])?;
            }
            Op::Ldar => self.accumulator = self.register(frame, operands[0]),
            Op::Star => {
                let value = self.accumulator;
                self.set_register(frame, operands[0], value);
            }
            Op::Mov => {
                let value = self.register(frame, operands[0]);
                self.set_register(frame, operands[1], value);
            }

            Op::Add => {
                let left = self.register(frame, operands[0]);
                let right = self.accumulator;
                self.accumulator = self.add_values(left, right)?;
            }
            Op::Sub | Op::Mul | Op::Div | Op::Mod | Op::Exp => {
                return self.op_arithmetic(frame, instruction.opcode, &operands)
            }
            Op::BitAnd
            | Op::BitOr
            | Op::BitXor
            | Op::ShiftLeft
            | Op::ShiftRight
            | Op::ShiftRightLogical => {
                return self.op_bitwise(frame, instruction.opcode, &operands)
            }

            Op::Inc | Op::Dec | Op::Negate | Op::BitNot | Op::ToNumeric
                if matches!(self.accumulator.tag(), Tag::BigInt)
                    || (self.accumulator.is_object() && {
                        let primitive = self.coerce_to_primitive(self.accumulator, Hint::Number)?;
                        self.accumulator = primitive;
                        matches!(primitive.tag(), Tag::BigInt)
                    }) =>
            {
                // The unary operations on a BigInt stay exact, and `~` is what
                // the two's-complement form makes it: `-(v + 1)`.
                let value = self.big_int_operand(self.accumulator)?;
                let mut one = crate::bigint::Number::ZERO;
                one.limbs[0] = 1;
                one.length = 1;
                let outcome = match instruction.opcode {
                    Op::Inc => crate::bigint::add(&value, &one),
                    Op::Dec => crate::bigint::subtract(&value, &one),
                    Op::Negate => Ok(crate::bigint::negate(&value)),
                    Op::BitNot => {
                        crate::bigint::add(&value, &one).map(|v| crate::bigint::negate(&v))
                    }
                    _ => Ok(value),
                };
                let number = outcome.map_err(|_| Completion::MALFORMED)?;
                self.accumulator = self.big_int_value(&number)?;
            }
            Op::ToNumber => {
                if matches!(self.accumulator.tag(), Tag::BigInt) {
                    return Err(self.throw_type_error());
                }
                let number = self.coerce_to_number(self.accumulator)?;
                self.accumulator = Value::number(number);
            }
            Op::LdaWithReceiver => self.op_lda_with_receiver(frame, &operands)?,
            Op::CreateDisposeStack => {
                self.accumulator = self.new_array()?;
            }
            Op::AddDisposable => return self.op_add_disposable(frame, &operands),
            Op::AddDisposableAsync => return self.op_add_disposable_async(frame, &operands),
            Op::DisposeStack => {
                let stack = self.register(frame, operands[0]);
                self.dispose_stack(stack, None)?;
            }
            Op::DisposeStackThrow => {
                let stack = self.register(frame, operands[0]);
                let thrown = self.register(frame, operands[1]);
                self.dispose_stack(stack, Some(thrown))?;
            }
            Op::DisposeStackNext => {
                let stack = self.register(frame, operands[0]);
                let pending = self.register(frame, operands[1]);
                let (awaited, pending) = self.dispose_stack_next(stack, pending)?;
                self.set_register(frame, operands[1], pending);
                self.accumulator = awaited;
            }
            Op::SuppressError => {
                let stack = self.register(frame, operands[0]);
                let pending = self.register(frame, operands[1]);
                let thrown = self.register(frame, operands[2]);
                let folded = self.fold_pending(stack, pending, thrown)?;
                self.set_register(frame, operands[1], folded);
            }
            Op::ForInHas => {
                let subject = self.register(frame, operands[0]);
                let present = if subject.is_object() {
                    let key = self.coerce_to_key(self.accumulator)?;
                    self.has_property_of(subject, key)?
                } else {
                    true
                };
                self.accumulator = Value::boolean(present);
            }
            Op::Inc | Op::Dec | Op::Negate | Op::BitNot | Op::ToNumeric => {
                self.op_unary_numeric(instruction.opcode)?
            }
            Op::LogicalNot => {
                let truth = self.coerce_to_boolean(self.accumulator)?;
                self.accumulator = Value::boolean(!truth);
            }
            Op::TypeOf => self.op_typeof()?,
            Op::ToString => {
                let value = self.accumulator;
                self.accumulator = self.coerce_to_string(value)?;
            }
            Op::ToPropertyKey => self.op_to_property_key()?,

            Op::TestEqual | Op::TestNotEqual => {
                let left = self.register(frame, operands[0]);
                let right = self.accumulator;
                let equal = self.loose_equals(left, right)?;
                self.accumulator =
                    Value::boolean(equal == matches!(instruction.opcode, Op::TestEqual));
            }
            Op::TestStrictEqual | Op::TestStrictNotEqual => {
                let left = self.register(frame, operands[0]);
                let right = self.accumulator;
                let equal = self.strict_equals(left, right)?;
                self.accumulator =
                    Value::boolean(equal == matches!(instruction.opcode, Op::TestStrictEqual));
            }
            Op::TestLess | Op::TestGreater | Op::TestLessEqual | Op::TestGreaterEqual => {
                let left = self.register(frame, operands[0]);
                let right = self.accumulator;
                self.accumulator = self.compare(instruction.opcode, left, right)?;
            }
            Op::TestPrivateIn => {
                let key = self.constant_key(operands[0])?;
                let target = self.accumulator;
                if !target.is_object() {
                    return Err(self.throw_type_error());
                }
                self.accumulator = Value::boolean(self.private_find(frame, target, key)?);
            }
            Op::TestInstanceOf => {
                let left = self.register(frame, operands[0]);
                let right = self.accumulator;
                self.accumulator = Value::boolean(self.instance_of(left, right)?);
            }
            Op::TestIn => self.op_test_in(frame, &operands)?,

            Op::GetNamedProperty => {
                let target = self.register(frame, operands[0]);
                let key = self.constant_key(operands[1])?;
                if self.is_private_key(key) {
                    self.accumulator = self.private_get(frame, target, key)?;
                } else {
                    self.accumulator = self.get_property(target, key)?;
                }
            }
            Op::GetKeyedProperty => {
                let target = self.register(frame, operands[0]);
                // The base must be coercible before the key is: coercing the
                // key can run a program's `toString`, and a read through
                // nothing is a type error first.
                if target.is_nullish() {
                    return Err(self.throw_type_error());
                }
                let key_value = self.accumulator;
                let key = self.coerce_to_key(key_value)?;
                self.accumulator = self.get_property(target, key)?;
            }
            Op::SetNamedProperty => {
                let target = self.register(frame, operands[0]);
                let key = self.constant_key(operands[1])?;
                let value = self.accumulator;
                if self.is_private_key(key) {
                    self.private_set(frame, target, key, value)?;
                } else {
                    let strict = self.frame_is_strict(frame);
                    self.set_property_of(target, key, value, strict)?;
                }
                // The assignment's value survives a setter's own return.
                self.accumulator = value;
            }
            Op::SetKeyedProperty => {
                let target = self.register(frame, operands[0]);
                if target.is_nullish() {
                    return Err(self.throw_type_error());
                }
                let key_value = self.register(frame, operands[1]);
                let key = self.coerce_to_key(key_value)?;
                let value = self.accumulator;
                // SetKeyedStrictMarker
                let strict = self.frame_is_strict(frame);
                self.set_property_of(target, key, value, strict)?;
                self.accumulator = value;
            }
            Op::DeleteNamedProperty => {
                let target = self.accumulator;
                let key = self.constant_key(operands[0])?;
                let gone = self.delete_property(target, key)?;
                if !gone && self.frame_is_strict(frame) {
                    // Strict code refuses to keep going where the property
                    // stays: a non-configurable delete is a TypeError.
                    return Err(self.throw_type_error());
                }
                self.accumulator = Value::boolean(gone);
            }
            Op::DeleteKeyedProperty => {
                let target = self.register(frame, operands[0]);
                let key_value = self.accumulator;
                let key = self.coerce_to_key(key_value)?;
                let gone = self.delete_property(target, key)?;
                if !gone && self.frame_is_strict(frame) {
                    return Err(self.throw_type_error());
                }
                self.accumulator = Value::boolean(gone);
            }

            Op::LdaGlobal => {
                let key = self.constant_key(operands[0])?;
                if let Some(value) = self.global_lexical_read(key)? {
                    self.accumulator = value;
                    return Ok(Flow::Continue);
                }
                let present = object::has_property(self.heap, self.realm.global, key)
                    .map_err(|_| Completion::MALFORMED)?;
                if !present {
                    return Err(self.throw_reference_error());
                }
                self.accumulator = self.get_property(Value::object(self.realm.global), key)?;
            }
            Op::LdaGlobalOrUndefined => return self.op_lda_global_or_undefined(&operands),
            Op::StaGlobal => {
                let key = self.constant_key(operands[0])?;
                let value = self.accumulator;
                if self.global_lexical_write(key, value)? {
                    return Ok(Flow::Continue);
                }
                self.set_property(Value::object(self.realm.global), key, value)?;
                self.accumulator = value;
            }
            Op::StaGlobalStrict => return self.op_sta_global_strict(&operands),
            Op::CheckGlobalLexical => self.op_check_global_lexical(&operands)?,
            Op::CheckGlobalVar => {
                let key = self.constant_key(operands[0])?;
                if let Key::Name(name) = key {
                    if self.global_lexical_find(name)?.is_some() {
                        return Err(self.throw_error_of(ErrorKind::Syntax));
                    }
                }
            }
            Op::DeclareGlobalLexical => {
                let key = self.constant_key(operands[0])?;
                if let Key::Name(name) = key {
                    let flags = if operands[1] != 0 {
                        env::binding::STRICT | env::binding::PERMANENT
                    } else {
                        env::binding::MUTABLE | env::binding::PERMANENT
                    };
                    self.global_lexical_declare(name, flags)?;
                }
            }
            Op::HasGlobal => {
                let key = self.constant_key(operands[0])?;
                let lexical = match key {
                    Key::Name(name) => self.global_lexical_find(name)?.is_some(),
                    _ => false,
                };
                let present = lexical
                    || object::has_property(self.heap, self.realm.global, key)
                        .map_err(|_| Completion::MALFORMED)?;
                self.accumulator = Value::boolean(present);
            }
            Op::StaGlobalResolved => return self.op_sta_global_resolved(frame, &operands),
            Op::InitGlobalLexical => self.op_init_global_lexical(&operands)?,
            Op::DeclareGlobal => self.op_declare_global(&operands)?,
            Op::DeclareGlobalFunction => self.op_declare_global_function(&operands)?,
            Op::LdaDynamic => {
                let key = self.constant_key(operands[0])?;
                let environment = frame.environment;
                let strict = self.frame_is_strict(frame);
                match self.dynamic_read(environment, key, strict)? {
                    Some(value) => self.accumulator = value,
                    None => return Err(self.throw_reference_error()),
                }
            }
            Op::LdaDynamicCallee => {
                let key = self.constant_key(operands[0])?;
                let environment = frame.environment;
                let strict = self.frame_is_strict(frame);
                match self.dynamic_read_base(environment, key, strict)? {
                    Some((value, base)) => {
                        self.set_register(frame, operands[1], base);
                        self.accumulator = value;
                    }
                    None => return Err(self.throw_reference_error()),
                }
            }
            Op::TypeofDynamic => {
                let key = self.constant_key(operands[0])?;
                let environment = frame.environment;
                self.accumulator = self
                    .dynamic_read(environment, key, false)?
                    .unwrap_or(Value::UNDEFINED);
            }
            Op::StaDynamic => {
                let key = self.constant_key(operands[0])?;
                let value = self.accumulator;
                let environment = frame.environment;
                let strict = self.frame_is_strict(frame);
                self.dynamic_write(environment, key, value, strict)?;
                self.accumulator = value;
            }
            Op::DeclareEvalVar => {
                let key = self.constant_key(operands[0])?;
                let environment = frame.environment;
                self.declare_eval_var(environment, key)?;
            }
            Op::DeleteDynamic => {
                let key = self.constant_key(operands[0])?;
                let environment = frame.environment;
                let gone = self.dynamic_delete(environment, key)?;
                self.accumulator = Value::boolean(gone);
            }
            Op::LdaImport => {
                let index = operands[0];
                self.accumulator = self.import_value(frame.module, index)?;
            }
            Op::LdaCallee => {
                self.accumulator = frame.callee;
            }
            Op::LdaContextSlot => {
                let value = self.context_slot(frame, operands[0], operands[1])?;
                self.accumulator = value;
            }
            Op::StaContextSlot => {
                let value = self.accumulator;
                self.set_context_slot(frame, operands[0], operands[1], value)?;
            }
            Op::LdaShadowable => {
                let key = self.constant_key(operands[0])?;
                if let Some(value) = self.shadowing_binding(frame, key, operands[2], false)? {
                    self.accumulator = value;
                } else {
                    let value = self.context_slot(frame, operands[1], operands[2])?;
                    self.accumulator = value;
                }
            }
            Op::StaShadowable => {
                let key = self.constant_key(operands[0])?;
                let value = self.accumulator;
                let shadowed = if let Key::Name(name) = key {
                    self.shadowing_store(frame, name, operands[2], value)?
                } else {
                    false
                };
                if !shadowed {
                    self.set_context_slot(frame, operands[1], operands[2], value)?;
                }
                self.accumulator = value;
            }
            Op::PrepareShadowable => {
                let key = self.constant_key(operands[0])?;
                let environment = frame.environment;
                let strict = self.frame_is_strict(frame);
                self.accumulator =
                    self.prepare_shadowable(environment, key, operands[2], strict)?;
            }
            Op::LdaPrepared => {
                let environment = self.register(frame, operands[0]);
                let key = self.constant_key(operands[1])?;
                self.accumulator = self.read_prepared(environment, key, operands[2])?;
            }
            Op::StaPrepared => {
                let environment = self.register(frame, operands[0]);
                let key = self.constant_key(operands[1])?;
                let value = self.accumulator;
                let strict = self
                    .unit_of(frame.module)
                    .function(frame.code)
                    .is_some_and(|record| record.flags & record_flag::STRICT != 0);
                self.write_prepared(environment, key, operands[2], value, strict)?;
                self.accumulator = value;
            }
            Op::CreateDefaultConstructor => self.op_create_default_constructor(&operands)?,
            Op::MakeClassConstructor => self.op_make_class_constructor(frame, &operands)?,
            Op::DefineMethod | Op::DefineMethodKeyed => {
                self.op_define_method(frame, instruction.opcode, &operands)?
            }
            Op::DefineClassAccessor | Op::DefineClassAccessorKeyed => {
                self.op_define_class_accessor(frame, instruction.opcode, &operands)?
            }
            Op::LdaSuperProperty => self.op_lda_super_property(frame, &operands)?,
            Op::PrivateKey => {
                let class = self.register(frame, operands[0]);
                let marker = operands[1] != 0;
                let name = self.accumulator;
                if !class.is_object() || !name.is_string() {
                    return Err(Completion::MALFORMED);
                }
                self.accumulator =
                    self.private_storage_string(name.as_handle(), class.as_handle(), marker)?;
            }
            Op::CacheTemplate => self.op_cache_template(frame, &operands)?,
            Op::GetSuperBase => self.op_get_super_base(frame)?,
            Op::ThrowReference => {
                return Err(self.throw_reference_error());
            }
            Op::ThrowSelfAssignment => {
                return Err(self.throw_type_error());
            }
            Op::LdaSuperKeyed => {
                let base = self.register(frame, operands[0]);
                let key_value = self.accumulator;
                let key = self.coerce_to_key(key_value)?;
                let receiver = self.this_value(frame)?;
                if !base.is_object() {
                    return Err(self.throw_type_error());
                }
                self.accumulator = self.super_get(base, key, receiver)?;
            }
            Op::StaSuperNamed | Op::StaSuperKeyed => {
                self.op_sta_super(frame, instruction.opcode, &operands)?
            }
            Op::CallSuper => return self.op_call_super(frame, &operands),
            Op::GetHeritagePrototype => self.op_get_heritage_prototype(frame, &operands)?,
            Op::DynamicImport => {
                let deferred = operands[0] != 0;
                let specifier = self.accumulator;
                let options = self.register(frame, operands[1]);
                self.accumulator = self.dynamic_import(specifier, options, deferred)?;
            }
            Op::InstantiationEnd => self.op_instantiation_end(frame)?,
            Op::ImportReject => self.op_import_reject()?,
            Op::BindThis => return self.op_bind_this(frame),
            Op::LdaNewTarget => {
                self.accumulator = self.new_target_of(frame.environment)?;
            }
            Op::InitFields => {
                let (callee, _) = self.super_constructor_of(frame)?;
                let this = self.this_value(frame)?;
                self.run_class_fields(callee, this)?;
            }
            Op::SetHome => {
                let home = self.register(frame, operands[0]);
                let function = self.accumulator;
                if function.is_object() && home.is_object() {
                    object::set_home_object(self.heap, function.as_handle(), home.as_handle())
                        .map_err(|_| Completion::MALFORMED)?;
                }
            }
            Op::Brand => self.op_brand(frame)?,
            Op::DefineField => {
                let name = self.register(frame, operands[0]);
                let value = self.accumulator;
                let this = self.this_value(frame)?;
                self.define_field(this, name, value)?;
            }
            Op::PushObjectContext => {
                let value = self.accumulator;
                let object = self.coerce_to_object(value)?;
                let parent = frame.environment;
                let record = env::create_object_environment(self.heap, parent, object.as_handle())
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                self.set_frame_environment(Value::object(record));
                self.adjust_contexts(1);
            }
            Op::PushContext => {
                let slots = operands[0];
                let parent = frame.environment;
                let record = env::create(self.heap, EnvironmentKind::Declarative, parent, slots)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                self.declare_slots(record, slots)?;
                self.set_frame_environment(Value::object(record));
                self.adjust_contexts(1);
            }
            Op::PopContext => {
                let current = frame.environment;
                if !current.is_object() {
                    return Err(Completion::MALFORMED);
                }
                let parent = env::parent(self.heap, current.as_handle())
                    .map_err(|_| Completion::MALFORMED)?;
                self.set_frame_environment(parent);
                self.adjust_contexts(-1);
            }
            Op::InitContextSlot => {
                let value = self.accumulator;
                self.init_context_slot(frame, operands[0], operands[1], value)?;
            }
            #[cfg(not(feature = "omit_regexp"))]
            Op::CreateRegExp => self.op_create_regexp(&operands)?,
            // Unreachable on a build that admits images through `verify_with`
            // and refuses this opcode there; kept so the machine is total
            // over its own instruction set whatever admitted the image.
            #[cfg(feature = "omit_regexp")]
            Op::CreateRegExp => {
                return Err(Completion::Terminated(Termination::NotImplemented));
            }
            Op::CreateClosure => self.op_create_closure(frame, &operands)?,
            Op::ToPropertyKeyChecked => {
                let base = self.register(frame, operands[0]);
                if base.is_nullish() {
                    return Err(self.throw_type_error());
                }
                let key = self.coerce_to_key(self.accumulator)?;
                self.accumulator = self.key_to_value(key)?;
            }
            Op::SetPrototype => {
                let target = self.register(frame, operands[0]);
                let value = self.accumulator;
                if target.is_object() && (value.is_object() || value.is_null()) {
                    object::set_prototype(self.heap, target.as_handle(), value)
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                }
            }
            Op::NameClosure => self.op_name_closure(&operands)?,
            Op::DefineAutoAccessor => {
                let target = self.register(frame, operands[0]);
                let key_value = self.register(frame, operands[1]);
                let key = self.coerce_to_key(key_value)?;
                self.define_auto_accessor(target, key)?;
            }
            Op::NameClosureKeyed => {
                let closure = self.accumulator;
                let key_value = self.register(frame, operands[0]);
                let key = self.coerce_to_key(key_value)?;
                self.name_closure_for(closure, key)?;
            }
            Op::DefineNamedGetter | Op::DefineNamedSetter => {
                let target = self.register(frame, operands[0]);
                let key = self.constant_key(operands[1])?;
                let getter = matches!(instruction.opcode, Op::DefineNamedGetter);
                self.define_accessor(
                    target,
                    key,
                    self.accumulator,
                    getter,
                    attribute::ENUMERABLE | attribute::CONFIGURABLE,
                )?;
            }
            Op::DefineKeyedGetter | Op::DefineKeyedSetter => {
                let target = self.register(frame, operands[0]);
                let key_value = self.register(frame, operands[1]);
                let key = self.coerce_to_key(key_value)?;
                let getter = matches!(instruction.opcode, Op::DefineKeyedGetter);
                self.define_accessor(
                    target,
                    key,
                    self.accumulator,
                    getter,
                    attribute::ENUMERABLE | attribute::CONFIGURABLE,
                )?;
            }
            Op::CreateArguments => self.op_create_arguments(frame, &operands)?,
            Op::GetEnumerable => {
                let value = self.accumulator;
                self.accumulator = self.enumerable_keys(value)?;
            }
            Op::CallWithArray => return self.op_call_with_array(frame, &operands),
            Op::ConstructWithArray => return self.op_construct_with_array(frame, &operands),
            Op::CallSuperWithArray => return self.op_call_super_with_array(frame, &operands),
            Op::GetIterator => {
                let value = self.accumulator;
                match self.iterator_of(value)? {
                    Some(iterator) => self.accumulator = iterator,
                    None => return Err(self.throw_type_error()),
                }
            }
            Op::GetAsyncIterator => {
                let value = self.accumulator;
                match self.async_iterator_of(value)? {
                    Some(iterator) => self.accumulator = iterator,
                    None => return Err(self.throw_type_error()),
                }
            }
            Op::IteratorNext => {
                let iterator = self.register(frame, operands[0]);
                match self.iterator_step(iterator)? {
                    Some(value) => {
                        self.accumulator = value;
                        self.set_register(frame, operands[1], Value::boolean(false));
                    }
                    None => {
                        self.accumulator = Value::UNDEFINED;
                        self.set_register(frame, operands[1], Value::boolean(true));
                    }
                }
            }
            Op::IteratorClose => {
                let done = self.register(frame, operands[1]);
                if !(matches!(done.tag(), Tag::Boolean) && done.as_boolean()) {
                    let iterator = self.register(frame, operands[0]);
                    self.close_iterator(iterator)?;
                }
            }
            Op::RequireObject => {
                if !self.accumulator.is_object() {
                    return Err(self.throw_type_error());
                }
            }
            Op::IteratorCloseQuiet => {
                let done = self.register(frame, operands[1]);
                if !(matches!(done.tag(), Tag::Boolean) && done.as_boolean()) {
                    let iterator = self.register(frame, operands[0]);
                    match self.close_iterator(iterator) {
                        Ok(()) | Err(Completion::Throw(_)) => {}
                        Err(other) => return Err(other),
                    }
                }
            }
            Op::LdaThis => {
                self.accumulator = self.this_value(frame)?;
            }

            Op::CreateEmptyArray => {
                let array = self.create_array()?;
                self.accumulator = array;
            }
            Op::CreateEmptyObject => {
                let object = object::create(self.heap, Value::object(self.realm.object_prototype))
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                self.accumulator = Value::object(object);
            }
            Op::AppendArrayElement | Op::AppendArrayHole => {
                let array = self.register(frame, operands[0]);
                let value = if matches!(instruction.opcode, Op::AppendArrayHole) {
                    None
                } else {
                    Some(self.accumulator)
                };
                self.append_element(array, value)?;
            }
            Op::DefineNamedProperty => {
                let target = self.register(frame, operands[0]);
                let key = self.constant_key(operands[1])?;
                let value = self.accumulator;
                self.define_property(target, key, value)?;
            }
            Op::DefineKeyedProperty => {
                let target = self.register(frame, operands[0]);
                let key_value = self.register(frame, operands[1]);
                let key = self.coerce_to_key(key_value)?;
                let value = self.accumulator;
                self.define_property(target, key, value)?;
            }
            Op::CopyDataProperties => {
                let target = self.register(frame, operands[0]);
                let source = self.accumulator;
                self.copy_data_properties(target, source)?;
            }
            Op::CopyDataPropertiesExcluding => {
                let target = self.register(frame, operands[0]);
                let excluded = self.register(frame, operands[1]);
                let source = self.accumulator;
                self.copy_data_properties_excluding(target, source, excluded)?;
            }

            Op::Call | Op::CallProperty | Op::TailCall => {
                return self.op_call(frame, instruction.opcode, &operands)
            }
            Op::Construct => return self.op_construct(frame, &operands),

            Op::Jump => return Ok(Flow::Jump(self.jump_target(frame, signed[0]))),
            Op::JumpIfTrue | Op::JumpIfFalse => {
                let truth =
                    matches!(self.accumulator.tag(), Tag::Boolean) && self.accumulator.as_boolean();
                if truth == matches!(instruction.opcode, Op::JumpIfTrue) {
                    return Ok(Flow::Jump(self.jump_target(frame, signed[0])));
                }
            }
            Op::JumpIfToBooleanTrue | Op::JumpIfToBooleanFalse => {
                let truth = self.coerce_to_boolean(self.accumulator)?;
                if truth == matches!(instruction.opcode, Op::JumpIfToBooleanTrue) {
                    return Ok(Flow::Jump(self.jump_target(frame, signed[0])));
                }
            }
            Op::JumpIfNotUndefined => {
                if !self.accumulator.is_undefined() {
                    return Ok(Flow::Jump(self.jump_target(frame, signed[0])));
                }
            }
            Op::CreateRestArguments => {
                let first = operands[0];
                let count = frame.argument_count.saturating_sub(first);
                let array = self.new_array()?;
                let mut index = 0u32;
                while index < count {
                    let value = self.register(frame, first + index);
                    self.set_element(array, index, value)?;
                    index += 1;
                }
                self.set_length(array, count)?;
                self.accumulator = array;
            }
            Op::JumpIfNullish | Op::JumpIfNotNullish => {
                let nullish = self.accumulator.is_nullish();
                if nullish == matches!(instruction.opcode, Op::JumpIfNullish) {
                    return Ok(Flow::Jump(self.jump_target(frame, signed[0])));
                }
            }
            Op::Return => return Ok(Flow::Return(self.accumulator)),
            Op::Await => {
                let value = self.accumulator;
                if !frame.promise.is_object() {
                    // Awaiting outside an async frame is bytecode no compiler
                    // of this format emits.
                    return Err(Completion::MALFORMED);
                }
                return Ok(Flow::Await(value));
            }
            Op::Yield => {
                let value = self.accumulator;
                if !frame.promise.is_object() {
                    return Err(Completion::MALFORMED);
                }
                return Ok(Flow::Yield(value, false));
            }
            Op::YieldStar => {
                let value = self.accumulator;
                if !frame.promise.is_object() {
                    return Err(Completion::MALFORMED);
                }
                return Ok(Flow::Yield(value, true));
            }
            Op::YieldDelegate => {
                let value = self.accumulator;
                if !frame.promise.is_object() {
                    return Err(Completion::MALFORMED);
                }
                return Ok(Flow::YieldDelegate(value));
            }
            Op::ResumeKind => {
                let kind = self
                    .frames
                    .get(self.depth as usize - 1)
                    .map_or(0, |running| running.resume_kind);
                self.accumulator = Value::number(f64::from(kind));
            }
            Op::InitialYield => return Ok(Flow::Begin),
            Op::Throw => return Err(Completion::Throw(self.accumulator)),
        }
        Ok(Flow::Continue)
    }

    /// A jump displacement is measured from the start of the jumping
    /// instruction, which is where this frame's program counter stood before it
    /// advanced.
    fn jump_target(&self, frame: &Frame, displacement: i32) -> u32 {
        let target = i64::from(frame.pc) + i64::from(displacement);
        u32::try_from(target.max(0)).unwrap_or(0)
    }

    /// What a failed object operation means: exhausted storage is a heap
    /// outcome, an overfull bound is a quota, and only a reference that names
    /// nothing live is a malformed image.
    const fn object_failure(error: object::ObjectError) -> Completion {
        match error {
            object::ObjectError::Heap(
                crate::heap::HeapError::ArenaFull | crate::heap::HeapError::SlotsFull,
            ) => Completion::HEAP_EXHAUSTED,
            object::ObjectError::TooManyKeys | object::ObjectError::PrototypeChainTooDeep => {
                Completion::QUOTA_EXCEEDED
            }
            _ => Completion::MALFORMED,
        }
    }

    /// What a failed key listing means: storage that was too small is a quota,
    /// not an exhausted heap, and either way it is an outcome rather than a
    /// shorter list than the object actually has.
    const fn key_failure(error: object::ObjectError) -> Completion {
        match error {
            object::ObjectError::TooManyKeys => Completion::QUOTA_EXCEEDED,
            _ => Completion::HEAP_EXHAUSTED,
        }
    }

    /// Build an error object of `kind`, with the realm's prototype for it.
    fn create_error(&mut self, kind: ErrorKind, message: Value) -> Result<Value, Completion> {
        let prototype = self.realm.prototype_of(kind);
        let object = object::create(self.heap, Value::object(prototype))
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        if !message.is_undefined() {
            let text = self.coerce_to_string(message)?;
            let key = self.ascii_key(b"message")?;
            object::define_own_property(
                self.heap,
                object,
                key,
                Descriptor::data(text, attribute::WRITABLE | attribute::CONFIGURABLE),
            )
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        Ok(Value::object(object))
    }

    /// An error a program constructs: the message is the first argument —
    /// or, for `SuppressedError(error, suppressed, message)`, the third,
    /// with the two errors as own properties.
    fn error_from_arguments(
        &mut self,
        kind: ErrorKind,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        if matches!(kind, ErrorKind::Aggregate) {
            // `AggregateError(errors, message)`: the errors, iterated into an
            // array of their own, ride as an own property.
            let errors = arguments.first().copied().unwrap_or(Value::UNDEFINED);
            let message = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
            let made = self.create_error(kind, message)?;
            let list = self.create_array()?;
            let Some(iterator) = self.iterator_of(errors)? else {
                return Err(self.throw_type_error());
            };
            while let Some(value) = self.iterator_step(iterator)? {
                self.append_element(list, Some(value))?;
            }
            let key = self.ascii_key(b"errors")?;
            object::define_own_property(
                self.heap,
                made.as_handle(),
                key,
                Descriptor::data(list, attribute::WRITABLE | attribute::CONFIGURABLE),
            )
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            return Ok(made);
        }
        if !matches!(kind, ErrorKind::Suppressed) {
            let message = arguments.first().copied().unwrap_or(Value::UNDEFINED);
            return self.create_error(kind, message);
        }
        let error = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        let suppressed = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
        let message = arguments.get(2).copied().unwrap_or(Value::UNDEFINED);
        self.suppressed_error(error, suppressed, message)
    }

    // Errors the engine itself throws.

    /// Throw an error of a given kind, for a native that must refuse.
    fn throw_error_of(&mut self, kind: ErrorKind) -> Completion {
        self.throw_error(kind)
    }

    fn throw_type_error(&mut self) -> Completion {
        self.throw_error(ErrorKind::Type)
    }

    fn throw_reference_error(&mut self) -> Completion {
        self.throw_error(ErrorKind::Reference)
    }

    fn throw_error(&mut self, kind: ErrorKind) -> Completion {
        match self.create_error(kind, Value::UNDEFINED) {
            Ok(value) => Completion::Throw(value),
            Err(completion) => completion,
        }
    }
}

enum Flow {
    Continue,
    Jump(u32),
    Return(Value),
    /// A frame was pushed: the loop runs it, and what it returns lands in the
    /// accumulator without the host stack growing.
    Enter,
    /// The running async frame awaits the value: it suspends, and its promise
    /// is what the caller receives.
    Await(Value),
    /// The running generator yields the value: it suspends into its
    /// generator object, and the value is what the resumer receives.
    /// The flag says the yield is a delegating `yield*` step, which a
    /// `return` or `throw` resumes rather than settling itself.
    Yield(Value, bool),
    /// A sync `yield*` step whose value is the inner iterator's own result
    /// object, handed to the resumer untouched.
    YieldDelegate(Value),
    /// The generator's parameters are bound: it suspends at its start, and
    /// its generator object answers the call.
    Begin,
}

/// Realms one machine may hold, and units whose realm it tracks.
const MAX_REALMS: usize = 4;
const MAX_UNIT_REALMS: usize = 320;
