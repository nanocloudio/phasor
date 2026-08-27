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

/// How a pending eval was served.
enum Served {
    Entered,
    Unwound(Option<Completion>),
    Pause,
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
    /// The state `Math.random` draws from: a fixed seed, so the sequence
    /// replays exactly unless the host seeds it from an entropy capability.
    random_state: u64,
    /// How many native operations on the host stack have entered JavaScript
    /// beneath them right now. Each is a real host frame, so the bound is
    /// explicit rather than whatever stack the platform happened to give.
    nested: u32,
    /// Where a match backtracks, and what it must put back when it does.
    regexp_choices: Option<&'a mut [crate::regexp::Choice]>,
    regexp_undo: Option<&'a mut [(u8, u32)]>,
    /// The units a match runs over, copied out of the heap.
    regexp_subject: Option<&'a mut [u16]>,
    /// The bindings this isolate was granted, when a host attached any.
    bindings: Option<&'a mut Bindings<'a>>,
    /// Call records the program produced and the host has not taken.
    outbox: Option<&'a mut [CallRecord]>,
    outbox_length: usize,
}

/// The error type used inside the interpreter, where a throw and a termination
/// both stop the current operation.
type Step = Result<(), Completion>;

/// The elements one dispose-stack entry takes: resource, disposer, hint.
const DISPOSABLE_STRIDE: u32 = 3;

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
            regexp_choices: None,
            regexp_undo: None,
            regexp_subject: None,
            bindings: None,
            outbox: None,
            outbox_length: 0,
            print_status: 0,
            random_state: RANDOM_SEED,
            realms: [Some(realm), None, None, None],
            unit_realm: [0; MAX_UNIT_REALMS],
            pending_eval_realm: 0,
        }
    }

    /// Run a linked closure rather than one unit: the units are the closure's
    /// modules, in evaluation order, and the table says where each module's
    /// resolved imports start.
    pub fn attach_modules(
        &mut self,
        units: &'a [Unit<'u>],
        modules: &'a mut [ModuleInstance],
        imports: &'a [(u32, u32)],
    ) {
        self.units = units;
        self.deferred_live = modules
            .iter()
            .any(|instance| instance.deferred_namespace.is_object());
        self.modules = Some(modules);
        self.imports = Some(imports);
    }

    /// Attach the specifier names dynamic imports resolve against.
    pub fn attach_module_names(&mut self, names: &'a [([u8; 128], usize, u32)]) {
        self.module_names = names;
    }

    /// Attach the cycle pairs whose members share evaluation errors.
    pub fn attach_module_cycles(&mut self, cycles: &'a [(u32, u32)]) {
        self.module_cycles = cycles;
    }

    /// Mark a module errored, its cycle with it: the specification records
    /// one [[EvaluationError]] for a whole strongly-connected component.
    fn poison_cycle(&mut self, module: u32, error: Value) {
        self.set_module_status(module, 3);
        if let Some(modules) = self.modules.as_deref_mut() {
            if let Some(instance) = modules.get_mut(module as usize) {
                instance.completion = error;
            }
        }
        let mut index = 0usize;
        while index < self.module_cycles.len() {
            let (one, two) = self.module_cycles[index];
            let partner = if one == module {
                Some(two)
            } else if two == module {
                Some(one)
            } else {
                None
            };
            if let Some(partner) = partner {
                if self.module_status(partner) != 3 {
                    self.set_module_status(partner, 3);
                    if let Some(modules) = self.modules.as_deref_mut() {
                        if let Some(instance) = modules.get_mut(partner as usize) {
                            instance.completion = error;
                        }
                    }
                }
            }
            index += 1;
        }
    }

    /// Attach a compiler the machine asks in place when a program calls
    /// `eval` where the machine cannot pause — inside a promise job or a
    /// native's callback — and, when it can pause, before pausing. The units
    /// the compiler makes live in `units`, numbered after the attached ones.
    pub fn attach_compiler(
        &mut self,
        state: *mut c_void,
        compile: CompileFn<'u>,
        units: &'a mut [Unit<'u>],
    ) {
        self.compiler = Some((state, compile));
        self.extra_units = Some(units);
    }

    /// The unit a module runs.
    fn unit_of(&self, module: u32) -> &Unit<'u> {
        let index = module as usize;
        if let Some(unit) = self.units.get(index) {
            return unit;
        }
        if let Some(extra) = &self.extra_units {
            if let Some(unit) = extra.get(index.wrapping_sub(self.units.len())) {
                return unit;
            }
        }
        &self.units[0]
    }

    /// Ask the attached compiler for the pending eval: enter what it makes,
    /// throw the syntax error for what it refuses, or pause — for the host
    /// protocol, or the nested call's failure — where there is no compiler
    /// or no room.
    fn serve_eval(&mut self, floor: u32) -> Served {
        let Some(source) = self.pending_eval() else {
            return Served::Pause;
        };
        let site = self.pending_eval_site();
        let site_unit = site.map(|(module, _, _)| *self.unit_of(module));
        let request = EvalRequest {
            source,
            site,
            site_unit,
            script: self.pending_eval_script,
            realm: self.pending_eval_realm,
        };
        let outcome = match (self.compiler, self.extra_units.as_deref_mut()) {
            (Some((state, compile)), Some(extra)) => compile(state, &*self.heap, &request, extra),
            _ => return Served::Pause,
        };
        match outcome {
            Compiled::Unit(slot) => {
                let index = u32::try_from(self.units.len() + slot).unwrap_or(u32::MAX);
                match self.enter_eval(index) {
                    Ok(()) => Served::Entered,
                    Err(completion) => Served::Unwound(Some(completion)),
                }
            }
            Compiled::Refused => Served::Unwound(self.fail_eval_to(floor)),
            Compiled::Exhausted => Served::Pause,
        }
    }

    /// The module the running frame belongs to.
    fn current_module(&self) -> u32 {
        match self.frames.get(self.depth.saturating_sub(1) as usize) {
            Some(frame) if self.depth > 0 => frame.module,
            _ => self.entry_module,
        }
    }

    /// The unit the running frame belongs to.
    fn unit(&self) -> &Unit<'u> {
        self.unit_of(self.current_module())
    }

    /// The value an import names: the slot it resolved to, in the environment
    /// of the module that exports it.
    ///
    /// The read goes through the exporting module every time, so an import sees
    /// what that module holds now rather than what it held when the importing
    /// module ran.
    fn import_value(&mut self, module: u32, index: u32) -> Result<Value, Completion> {
        let base = match &self.modules {
            Some(modules) => modules
                .get(module as usize)
                .map_or(u32::MAX, |instance| instance.import_base),
            None => u32::MAX,
        };
        if base == u32::MAX {
            return Err(Completion::Terminated(Termination::Malformed));
        }
        let Some((source, slot)) = self
            .imports
            .as_ref()
            .and_then(|imports| imports.get((base + index) as usize).copied())
        else {
            return Err(Completion::Terminated(Termination::Malformed));
        };
        if slot == u32::MAX {
            // `import * as name` names the module itself.
            return self.namespace_of(source);
        }
        if slot == crate::bytecode::DEFER_IMPORT_NAME {
            // `import defer * as name` names it too, evaluation withheld.
            return self.deferred_namespace_of(source);
        }
        if slot == crate::bytecode::POISON_IMPORT {
            // The linker could not resolve this name; reading it is the
            // SyntaxError the linking would have raised.
            return Err(self.throw_error_of(ErrorKind::Syntax));
        }
        if slot == crate::bytecode::HOST_POISON_IMPORT {
            // A phase the host does not serve: its refusal, a TypeError.
            return Err(self.throw_type_error());
        }
        let environment = self.module_environment(source);
        if !environment.is_object() {
            return Err(self.throw_reference_error());
        }
        match env::slot_value(self.heap, environment.as_handle(), slot) {
            Ok(value) => Ok(value),
            // A slot not yet written is a binding still in its dead zone:
            // the module that owns it has not reached its declaration.
            Err(env::EnvironmentError::Uninitialised | env::EnvironmentError::Unresolvable) => {
                Err(self.throw_reference_error())
            }
            Err(_) => Err(Completion::Terminated(Termination::Malformed)),
        }
    }

    /// The object that names a module's exports.
    ///
    /// Each of its properties reads the module's slot when it is read, so a
    /// namespace shows what the module holds now rather than what it held when
    /// the namespace was made.
    fn namespace_of(&mut self, module: u32) -> Result<Value, Completion> {
        self.namespace_object(module, false)
    }

    /// The deferred twin of a module's namespace: one distinct object per
    /// module, shaped exactly as the namespace is, whose meaningful use
    /// evaluates the module first.
    fn deferred_namespace_of(&mut self, module: u32) -> Result<Value, Completion> {
        self.namespace_object(module, true)
    }

    fn namespace_object(&mut self, module: u32, deferred: bool) -> Result<Value, Completion> {
        if let Some(modules) = &self.modules {
            if let Some(instance) = modules.get(module as usize) {
                let held = if deferred {
                    instance.deferred_namespace
                } else {
                    instance.namespace
                };
                if held.is_object() {
                    return Ok(held);
                }
            }
        }
        // A namespace has no prototype, takes nothing new, and lists its
        // names in code unit order, the way the specification sorts them.
        let object = object::create(self.heap, Value::NULL).map_err(|_| self.heap_failure())?;
        let namespace = Value::object(object);
        // The module's own exports come first; whatever its `export * from`
        // records merge in follows, without `default`, and a name the list
        // already holds keeps its first source.
        let mut names = [[0u16; 64]; 64];
        let mut lengths = [0usize; 64];
        let mut slots = [0u32; 64];
        let mut sources = [0u32; 64];
        let mut owns = [false; 64];
        let mut dead = [false; 64];
        let mut final_units = [0u32; 64];
        let mut final_slots = [0u32; 64];
        let mut held = 0usize;
        let mut queue = [0u32; 16];
        queue[0] = module;
        let mut queued = 1usize;
        let mut front = 0usize;
        while front < queued {
            let source = queue[front];
            let own = front == 0;
            let count = self.unit_of(source).header().export_count;
            let mut export = 0u32;
            while export < count {
                let Some(record) = self.unit_of(source).export(export) else {
                    break;
                };
                if record.name == u32::MAX {
                    // A star: whatever module the record's import reaches
                    // joins the queue, once.
                    let index = record.slot & !crate::bytecode::EXPORT_IMPORT_MARK;
                    let base = match &self.modules {
                        Some(modules) => modules
                            .get(source as usize)
                            .map_or(u32::MAX, |instance| instance.import_base),
                        None => u32::MAX,
                    };
                    let target = self
                        .imports
                        .as_ref()
                        .and_then(|imports| imports.get((base.wrapping_add(index)) as usize))
                        .map(|&(unit, _)| unit);
                    if let Some(target) = target {
                        let mut seen = false;
                        let mut at = 0usize;
                        while at < queued {
                            if queue[at] == target {
                                seen = true;
                                break;
                            }
                            at += 1;
                        }
                        if !seen && queued < queue.len() {
                            queue[queued] = target;
                            queued += 1;
                        }
                    }
                    export += 1;
                    continue;
                }
                let mut units = [0u16; 64];
                let length = {
                    let unit = self.unit_of(source);
                    let Some(constant) = unit.constant(record.name) else {
                        break;
                    };
                    unit.constant_units(&constant, &mut units).unwrap_or(0)
                };
                let name = units.get(..length).unwrap_or(&[]);
                let is_default = name
                    == [
                        u16::from(b'd'),
                        u16::from(b'e'),
                        u16::from(b'f'),
                        u16::from(b'a'),
                        u16::from(b'u'),
                        u16::from(b'l'),
                        u16::from(b't'),
                    ];
                if (!own && is_default) || held >= names.len() {
                    export += 1;
                    continue;
                }
                // Where the name finally lands: an indirection's row in the
                // import table, or this module's own slot. Two stars giving
                // one name different bindings make it ambiguous, and an
                // ambiguous name is left off the namespace — unless the
                // module's own export claims it.
                let landing = if record.slot & crate::bytecode::EXPORT_IMPORT_MARK != 0 {
                    let index = record.slot & !crate::bytecode::EXPORT_IMPORT_MARK;
                    let base = match &self.modules {
                        Some(modules) => modules
                            .get(source as usize)
                            .map_or(u32::MAX, |instance| instance.import_base),
                        None => u32::MAX,
                    };
                    self.imports
                        .as_ref()
                        .and_then(|imports| imports.get(base.wrapping_add(index) as usize))
                        .copied()
                } else {
                    Some((source, record.slot))
                };
                let Some((final_unit, final_slot)) = landing else {
                    export += 1;
                    continue;
                };
                let mut duplicate = false;
                let mut at = 0usize;
                while at < held {
                    if names[at].get(..lengths[at]) == Some(name) {
                        if !owns[at]
                            && !dead[at]
                            && (final_units[at], final_slots[at]) != (final_unit, final_slot)
                        {
                            dead[at] = true;
                        }
                        duplicate = true;
                        break;
                    }
                    at += 1;
                }
                if duplicate {
                    export += 1;
                    continue;
                }
                let mut place = held;
                while place > 0 {
                    let previous = names[place - 1].get(..lengths[place - 1]).unwrap_or(&[]);
                    if previous <= units.get(..length).unwrap_or(&[]) {
                        break;
                    }
                    names[place] = names[place - 1];
                    lengths[place] = lengths[place - 1];
                    slots[place] = slots[place - 1];
                    sources[place] = sources[place - 1];
                    owns[place] = owns[place - 1];
                    dead[place] = dead[place - 1];
                    final_units[place] = final_units[place - 1];
                    final_slots[place] = final_slots[place - 1];
                    place -= 1;
                }
                names[place] = units;
                lengths[place] = length;
                slots[place] = record.slot;
                sources[place] = source;
                owns[place] = own;
                dead[place] = false;
                final_units[place] = final_unit;
                final_slots[place] = final_slot;
                held += 1;
                export += 1;
            }
            front += 1;
        }
        let mut index = 0usize;
        while index < held {
            if dead.get(index).copied().unwrap_or(false) {
                index += 1;
                continue;
            }
            let record = crate::bytecode::ExportRecord {
                name: 0,
                slot: slots.get(index).copied().unwrap_or(0),
            };
            let units = names[index];
            let length = lengths[index];
            let source = sources.get(index).copied().unwrap_or(module);
            let name = self.make_string(units.get(..length).unwrap_or(&[]))?;
            let key = self.coerce_to_key(name)?;
            let getter = object::create_native(
                self.heap,
                Value::object(self.realm.function_prototype),
                native::NAMESPACE_GET,
                0,
            )
            .map_err(|_| self.heap_failure())?;
            let binding = self.new_array()?;
            self.set_element(
                binding,
                0,
                Value::number(crate::softfloat::from_u64(u64::from(source))),
            )?;
            self.set_element(
                binding,
                1,
                Value::number(crate::softfloat::from_u64(u64::from(record.slot))),
            )?;
            let _ = &record;
            if deferred {
                // The getter of a deferred namespace knows it, and knows
                // `then`, which reads as undefined while the module waits.
                let is_then = units.get(..length)
                    == Some(&[
                        u16::from(b't'),
                        u16::from(b'h'),
                        u16::from(b'e'),
                        u16::from(b'n'),
                    ]);
                let flags = 1u64 | if is_then { 2 } else { 0 };
                self.set_element(binding, 2, Value::number(crate::softfloat::from_u64(flags)))?;
                self.set_length(binding, 3)?;
            } else {
                self.set_length(binding, 2)?;
            }
            object::set_bound_value(self.heap, getter, binding).map_err(|_| self.heap_failure())?;
            object::define_own_property(
                self.heap,
                object,
                key,
                Descriptor::accessor(
                    Value::object(getter),
                    Value::UNDEFINED,
                    attribute::ENUMERABLE,
                ),
            )
            .map_err(|_| self.heap_failure())?;
            index += 1;
        }
        let tag = if deferred {
            self.ascii_string(b"Deferred Module")?
        } else {
            self.ascii_string(b"Module")?
        };
        object::define_own_property(
            self.heap,
            object,
            Key::Symbol(self.realm.to_string_tag_symbol),
            Descriptor::data(tag, 0),
        )
        .map_err(|_| self.heap_failure())?;
        let kind = if deferred {
            object::exotic::DEFERRED
        } else {
            object::exotic::NAMESPACE
        };
        object::set_exotic_kind(self.heap, object, kind)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        let _ = object::prevent_extensions(self.heap, object);
        if let Some(modules) = self.modules.as_deref_mut() {
            if let Some(instance) = modules.get_mut(module as usize) {
                if deferred {
                    instance.deferred_namespace = namespace;
                } else {
                    instance.namespace = namespace;
                }
            }
        }
        if deferred {
            self.deferred_live = true;
        }
        Ok(namespace)
    }

    /// Walk a prototype chain up to where a key would be found, running the
    /// deferred trigger on any deferred namespace passed on the way — a
    /// read or an `in` consults its [[Get]] or [[HasProperty]] even from a
    /// chain, and that consultation is a meaningful use.
    fn deferred_chain_trigger(&mut self, target: Value, key: Key) -> Result<(), Completion> {
        let mut holder = target;
        let mut depth = 0u32;
        while holder.is_object() && depth <= object::MAX_PROTOTYPE_DEPTH {
            if object::exotic_kind(self.heap, holder.as_handle()).unwrap_or(0)
                == object::exotic::DEFERRED
            {
                self.deferred_trigger(holder, Some(key))?;
            }
            let found = object::get_own_property(self.heap, holder.as_handle(), key)
                .unwrap_or(None)
                .is_some();
            if found {
                break;
            }
            holder = object::prototype(self.heap, holder.as_handle())
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            depth += 1;
        }
        Ok(())
    }

    /// Which module a deferred namespace names, found by the object itself.
    fn module_of_deferred(&self, target: Value) -> Option<u32> {
        let modules = self.modules.as_ref()?;
        let mut index = 0usize;
        while index < modules.len() {
            let held = modules.get(index)?.deferred_namespace;
            if held.is_object() && held.as_handle() == target.as_handle() {
                return u32::try_from(index).ok();
            }
            index += 1;
        }
        None
    }

    /// How far a module has run: untouched, running, or done. A module
    /// whose completion promise has fulfilled is done the moment it does,
    /// however the field lags — an access inside the settling job sees it.
    pub fn module_status(&self, module: u32) -> u8 {
        let held = match &self.modules {
            Some(modules) => modules.get(module as usize).copied(),
            None => None,
        };
        let Some(instance) = held else {
            return 2;
        };
        if instance.evaluated == 1
            && instance.completion.is_object()
            && object::promise_state(self.heap, instance.completion.as_handle())
                == Ok(promise::FULFILLED)
        {
            return 2;
        }
        instance.evaluated
    }

    /// How far a module has run, told from outside: the loader marks the
    /// whole eager set evaluating before the first body, done as each
    /// settles — which is what a deferred access checks against.
    pub fn set_module_status(&mut self, module: u32, status: u8) {
        if let Some(modules) = self.modules.as_deref_mut() {
            if let Some(instance) = modules.get_mut(module as usize) {
                instance.evaluated = status;
            }
        }
    }

    /// What a meaningful use of a deferred namespace does: run the module.
    /// A symbol key never counts, nor does `then`, which promise resolution
    /// probes without meaning to use the module; no key at all — a key
    /// listing — counts. Using a namespace whose own module is still mid
    /// evaluation is the error the specification names.
    fn deferred_trigger(&mut self, target: Value, key: Option<Key>) -> Result<(), Completion> {
        if !target.is_object()
            || object::exotic_kind(self.heap, target.as_handle()).unwrap_or(0)
                != object::exotic::DEFERRED
        {
            return Ok(());
        }
        match key {
            Some(Key::Symbol(_)) => return Ok(()),
            Some(key) => {
                let then = self.ascii_key(b"then")?;
                if key == then {
                    return Ok(());
                }
            }
            None => {}
        }
        let Some(module) = self.module_of_deferred(target) else {
            return Ok(());
        };
        match self.module_status(module) {
            2 => Ok(()),
            6 => Err(self.throw_error_of(ErrorKind::Syntax)),
            1 | 4 => Err(self.throw_type_error()),
            3 => {
                // An errored module answers every later use with the very
                // error its evaluation threw.
                let held = self.module_completion(module);
                Err(Completion::Throw(held))
            }
            _ => {
                // Nothing runs unless the whole subgraph is ready: a
                // dependency someone else is mid-evaluating refuses the
                // trigger before any body runs.
                self.deferred_ready(module, true)?;
                self.evaluate_module_now(module, true)
            }
        }
    }

    /// Whether a deferred module's whole subgraph can run now, checked
    /// without running anything.
    fn deferred_ready(&mut self, module: u32, strict: bool) -> Result<(), Completion> {
        let mut seen = [u32::MAX; MAX_UNIT_REALMS];
        let mut count = 0usize;
        // Loading comes before linking: a module the host refused to load
        // anywhere in the graph rejects with the host's error, before any
        // name resolution gets to raise its SyntaxError.
        let hosts = self.scan_hosts(module, &mut seen, &mut count);
        let mut index = 0usize;
        while index < count {
            let held = seen[index];
            if held != u32::MAX && self.module_status(held) == 5 {
                self.set_module_status(held, 0);
            }
            index += 1;
        }
        hosts?;
        let mut seen = [u32::MAX; MAX_UNIT_REALMS];
        let mut count = 0usize;
        let outcome = self.scan_ready(module, &mut seen, &mut count, false, strict);
        let mut index = 0usize;
        while index < count {
            let held = seen[index];
            if held != u32::MAX && self.module_status(held) == 5 {
                self.set_module_status(held, 0);
            }
            index += 1;
        }
        outcome
    }

    /// Whether any module the graph loads was refused by the host, every
    /// edge followed, deferred ones included.
    fn scan_hosts(
        &mut self,
        module: u32,
        seen: &mut [u32; MAX_UNIT_REALMS],
        count: &mut usize,
    ) -> Result<(), Completion> {
        if *count >= seen.len() {
            return Err(Completion::Terminated(Termination::Malformed));
        }
        seen[*count] = module;
        *count += 1;
        self.set_module_status(module, 5);
        let base = match &self.modules {
            Some(modules) => modules
                .get(module as usize)
                .map_or(u32::MAX, |instance| instance.import_base),
            None => u32::MAX,
        };
        if base == u32::MAX {
            return Ok(());
        }
        let held = self.unit_of(module).header().import_count;
        let mut import = 0u32;
        while import < held {
            let row = self
                .imports
                .as_ref()
                .and_then(|imports| imports.get((base + import) as usize))
                .copied();
            if let Some((source, slot)) = row {
                if slot == crate::bytecode::HOST_POISON_IMPORT {
                    return Err(self.throw_type_error());
                }
                if source != module && self.module_status(source) == 0 {
                    self.scan_hosts(source, seen, count)?;
                }
            }
            import += 1;
        }
        Ok(())
    }

    fn scan_ready(
        &mut self,
        module: u32,
        seen: &mut [u32; MAX_UNIT_REALMS],
        count: &mut usize,
        poison_only: bool,
        strict: bool,
    ) -> Result<(), Completion> {
        if *count >= seen.len() {
            return Err(Completion::Terminated(Termination::Malformed));
        }
        seen[*count] = module;
        *count += 1;
        self.set_module_status(module, 5);
        let base = match &self.modules {
            Some(modules) => modules
                .get(module as usize)
                .map_or(u32::MAX, |instance| instance.import_base),
            None => u32::MAX,
        };
        if base == u32::MAX {
            return Ok(());
        }
        let held = self.unit_of(module).header().import_count;
        let mut import = 0u32;
        while import < held {
            let row = self
                .imports
                .as_ref()
                .and_then(|imports| imports.get((base + import) as usize))
                .copied();
            if let Some((source, slot)) = row {
                if slot == crate::bytecode::POISON_IMPORT {
                    // A row linking refused: the SyntaxError it earned.
                    return Err(self.throw_error_of(ErrorKind::Syntax));
                }
                if slot == crate::bytecode::HOST_POISON_IMPORT {
                    return Err(self.throw_type_error());
                }
                if source != module {
                    if slot == crate::bytecode::DEFER_IMPORT_NAME {
                        // What stays deferred stays out of the run — but
                        // linking was eager: a poisoned name anywhere in the
                        // deferred graph is the SyntaxError it earned, an
                        // async module pre-evaluated on this edge's behalf
                        // must have settled, and its error is the answer.
                        // Only an async target was pre-evaluated on this
                        // edge's behalf; a sync deferred module's fate is
                        // its trigger's business, not this import's.
                        let entry_function = self.unit_of(source).header().entry_function;
                        let flags = self
                            .unit_of(source)
                            .function(entry_function)
                            .map_or(0, |held| held.flags);
                        if !poison_only && flags & crate::bytecode::function_flag::ASYNC != 0 {
                            match self.module_status(source) {
                                1 => return Err(self.throw_type_error()),
                                3 => {
                                    let error = self.module_completion(source);
                                    return Err(Completion::Throw(error));
                                }
                                _ => {}
                            }
                        }
                        match self.module_status(source) {
                            6 => return Err(self.throw_error_of(ErrorKind::Syntax)),
                            0 => self.scan_ready(source, seen, count, true, strict)?,
                            _ => {}
                        }
                    } else if poison_only {
                        match self.module_status(source) {
                            6 => return Err(self.throw_error_of(ErrorKind::Syntax)),
                            0 => self.scan_ready(source, seen, count, true, strict)?,
                            _ => {}
                        }
                    } else {
                        match self.module_status(source) {
                            0 => self.scan_ready(source, seen, count, false, strict)?,
                            // A dependency mid-evaluation refuses a trigger;
                            // an import simply waits it out.
                            1 if strict => return Err(self.throw_type_error()),
                            6 => return Err(self.throw_error_of(ErrorKind::Syntax)),
                            3 => {
                                let error = self.module_completion(source);
                                return Err(Completion::Throw(error));
                            }
                            _ => {}
                        }
                    }
                }
            }
            import += 1;
        }
        Ok(())
    }

    /// Whether a deferred namespace's module has already run to its end.
    fn deferred_done(&self, target: Value) -> bool {
        self.module_of_deferred(target)
            .map_or(true, |module| self.module_status(module) >= 2)
    }

    /// Run just a module's instantiation: its bindings exist afterwards,
    /// its function declarations hold closures, and nothing else has run —
    /// which is what lets a cycle call across itself before bodies start.
    pub fn instantiate_module(&mut self, module: u32) -> Result<(), Completion> {
        if self.nested >= MAX_NESTED_ENTRIES {
            return Err(Completion::Terminated(Termination::StackOverflow));
        }
        let entry = self.unit_of(module).header().entry_function;
        let environment = self.module_environment(module);
        if !environment.is_object() {
            return Err(Completion::Terminated(Termination::Malformed));
        }
        self.push_frame(
            entry,
            environment,
            Value::UNDEFINED,
            Value::UNDEFINED,
            module,
        )?;
        let held = self.instantiating;
        self.instantiating = true;
        self.nested += 1;
        let completion = self.execute();
        self.nested -= 1;
        self.instantiating = held;
        match completion {
            Completion::Value(_) => Ok(()),
            other => Err(other),
        }
    }

    /// The completion an unsettled async module behind one of this
    /// module's deferred edges will answer — the gate its evaluation waits
    /// behind — or undefined when nothing gates it.
    fn pending_defer_gate(&mut self, module: u32) -> Value {
        let mut seen = [u32::MAX; MAX_UNIT_REALMS];
        let mut count = 0usize;
        self.defer_gate_scan(module, &mut seen, &mut count, false)
    }

    fn defer_gate_scan(
        &mut self,
        module: u32,
        seen: &mut [u32; MAX_UNIT_REALMS],
        count: &mut usize,
        behind_defer: bool,
    ) -> Value {
        let mut at = 0usize;
        while at < *count {
            if seen[at] == module {
                return Value::UNDEFINED;
            }
            at += 1;
        }
        if *count >= seen.len() {
            return Value::UNDEFINED;
        }
        seen[*count] = module;
        *count += 1;
        if behind_defer {
            let entry = self.unit_of(module).header().entry_function;
            let flags = self
                .unit_of(module)
                .function(entry)
                .map_or(0, |held| held.flags);
            if flags & crate::bytecode::function_flag::ASYNC != 0 {
                let completion = self.module_completion(module);
                let unsettled = self.module_status(module) == 1
                    && (!completion.is_object()
                        || object::promise_state(self.heap, completion.as_handle())
                            == Ok(promise::PENDING));
                // Mid-body before its first await there is nothing
                // concrete to gate on; a suspended one hands over its
                // completion.
                if unsettled && completion.is_object() {
                    return completion;
                }
            }
        }
        let base = match &self.modules {
            Some(modules) => modules
                .get(module as usize)
                .map_or(u32::MAX, |instance| instance.import_base),
            None => u32::MAX,
        };
        if base == u32::MAX {
            return Value::UNDEFINED;
        }
        let held = self.unit_of(module).header().import_count;
        let mut import = 0u32;
        while import < held {
            let row = self
                .imports
                .as_ref()
                .and_then(|imports| imports.get((base + import) as usize))
                .copied();
            if let Some((source, slot)) = row {
                if source != module {
                    let crossing = behind_defer || slot == crate::bytecode::DEFER_IMPORT_NAME;
                    let found = self.defer_gate_scan(source, seen, count, crossing);
                    if found.is_object() {
                        return found;
                    }
                }
            }
            import += 1;
        }
        Value::UNDEFINED
    }

    /// Run a deferred module on this loop, its unevaluated dependencies
    /// first, depth first in import order. A dependency found mid
    /// evaluation is a cycle, left to finish on its own.
    fn evaluate_module_now(&mut self, module: u32, strict: bool) -> Result<(), Completion> {
        let mut blocked = Value::UNDEFINED;
        self.evaluate_module_gated(module, strict, &mut blocked)
    }

    /// Like `evaluate_module_now`, but a lenient evaluation skips any
    /// dependency gated behind an unsettled async-deferred subgraph — and
    /// its own body with it — handing the gate back for the caller to
    /// wait on and try again.
    fn evaluate_module_gated(
        &mut self,
        module: u32,
        strict: bool,
        blocked: &mut Value,
    ) -> Result<(), Completion> {
        self.set_module_status(module, 4);
        let base = match &self.modules {
            Some(modules) => modules
                .get(module as usize)
                .map_or(u32::MAX, |instance| instance.import_base),
            None => u32::MAX,
        };
        if base != u32::MAX {
            let count = self.unit_of(module).header().import_count;
            let mut import = 0u32;
            while import < count {
                let row = self
                    .imports
                    .as_ref()
                    .and_then(|imports| imports.get((base + import) as usize))
                    .copied();
                if let Some((source, slot)) = row {
                    if slot == crate::bytecode::POISON_IMPORT {
                        return Err(self.throw_error_of(ErrorKind::Syntax));
                    }
                    if slot == crate::bytecode::HOST_POISON_IMPORT {
                        return Err(self.throw_type_error());
                    }
                    // What this module itself defers stays deferred; a
                    // dependency someone else is mid-evaluating is not
                    // usable yet, and one that threw answers its error.
                    if source != module && slot != crate::bytecode::DEFER_IMPORT_NAME {
                        match self.module_status(source) {
                            0 => {
                                // A dependency gated behind an unsettled
                                // async-deferred subgraph waits its turn;
                                // its siblings run meanwhile.
                                if !strict {
                                    let gate = self.pending_defer_gate(source);
                                    if gate.is_object() {
                                        *blocked = gate;
                                        import += 1;
                                        continue;
                                    }
                                }
                                // A sibling's gate is not this dependency's:
                                // it runs against a fresh gate, and only a
                                // gate of its own holds this body back too.
                                let mut inner = Value::UNDEFINED;
                                self.evaluate_module_gated(source, strict, &mut inner)?;
                                if inner.is_object() {
                                    *blocked = inner;
                                }
                            }
                            1 if strict => return Err(self.throw_type_error()),
                            6 => return Err(self.throw_error_of(ErrorKind::Syntax)),
                            3 => {
                                let held = self.module_completion(source);
                                return Err(Completion::Throw(held));
                            }
                            _ => {}
                        }
                    }
                }
                import += 1;
            }
        }
        if blocked.is_object() {
            // A dependency waits behind a gate: so does this body, its
            // status handed back for the retry to find untouched.
            self.set_module_status(module, 0);
            return Ok(());
        }
        if self.nested >= MAX_NESTED_ENTRIES {
            return Err(Completion::Terminated(Termination::StackOverflow));
        }
        let entry = self.unit_of(module).header().entry_function;
        let environment = self.module_environment(module);
        if !environment.is_object() {
            return Err(Completion::Terminated(Termination::Malformed));
        }
        self.push_frame(
            entry,
            environment,
            Value::UNDEFINED,
            Value::UNDEFINED,
            module,
        )?;
        let resume = match &self.modules {
            Some(modules) => modules
                .get(module as usize)
                .map_or(0, |instance| instance.body_pc),
            None => 0,
        };
        if resume != 0 {
            self.frames[self.depth as usize - 1].pc = resume;
        }
        let before = self.fuel;
        self.nested += 1;
        let completion = self.execute();
        self.nested -= 1;
        let spent = before.saturating_sub(self.fuel);
        self.slice = self.slice.saturating_sub(spent);
        match completion {
            Completion::Value(value) => {
                // A body that answered a pending promise is still running;
                // its completion is kept for whoever waits on it.
                if value.is_object()
                    && object::is_promise(self.heap, value.as_handle()).unwrap_or(false)
                    && object::promise_state(self.heap, value.as_handle()) != Ok(promise::FULFILLED)
                {
                    if let Some(modules) = self.modules.as_deref_mut() {
                        if let Some(instance) = modules.get_mut(module as usize) {
                            instance.completion = value;
                            instance.evaluated = 1;
                        }
                    }
                    return Ok(());
                }
                self.set_module_status(module, 2);
                Ok(())
            }
            Completion::Throw(error) => {
                self.poison_cycle(module, error);
                Err(Completion::Throw(error))
            }
            other => {
                self.set_module_status(module, 2);
                Err(other)
            }
        }
    }

    /// Surface what a namespace binding holds: reflection over a namespace
    /// reads the binding, so a name still in its dead zone throws here as
    /// a direct read would.
    fn namespace_touch(&mut self, object: Value, key: Key) -> Result<(), Completion> {
        self.deferred_trigger(object, Some(key))?;
        if object.is_object()
            && matches!(
                object::exotic_kind(self.heap, object.as_handle()).unwrap_or(0),
                object::exotic::NAMESPACE | object::exotic::DEFERRED
            )
            && !matches!(key, Key::Symbol(_))
            && object::get_own_property(self.heap, object.as_handle(), key)
                .unwrap_or(None)
                .is_some()
        {
            self.get_property(object, key)?;
        }
        Ok(())
    }

    /// A module's environment, which holds its top-level bindings.
    fn module_environment(&self, module: u32) -> Value {
        match &self.modules {
            Some(modules) => modules
                .get(module as usize)
                .map_or(Value::UNDEFINED, |instance| instance.environment),
            None => Value::UNDEFINED,
        }
    }

    /// Grant the machine its bindings and somewhere to put the call records a
    /// program produces.
    pub fn attach_bindings(
        &mut self,
        bindings: &'a mut Bindings<'a>,
        outbox: &'a mut [CallRecord],
    ) {
        self.bindings = Some(bindings);
        self.outbox = Some(outbox);
        self.outbox_length = 0;
    }

    /// Make an admitted binding reachable from the program under `name`.
    ///
    /// The program gets a function; calling it makes a call record and returns
    /// a promise. Nothing about the provider is visible to it.
    pub fn define_binding(&mut self, name: &[u8], binding: u32) -> Result<(), Completion> {
        let function = object::create_native(
            self.heap,
            Value::object(self.realm.function_prototype),
            native::BINDING_BASE + binding,
            0,
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        let key = self.ascii_key(name)?;
        object::define_own_property(
            self.heap,
            self.realm.global,
            key,
            Descriptor::data(
                Value::object(function),
                attribute::WRITABLE | attribute::CONFIGURABLE,
            ),
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(())
    }

    /// The call records the program has produced and the host has not taken.
    pub fn calls(&self) -> &[CallRecord] {
        match &self.outbox {
            Some(outbox) => outbox.get(..self.outbox_length).unwrap_or(&[]),
            None => &[],
        }
    }

    /// Forget the call records the host has taken.
    pub fn take_calls(&mut self) {
        self.outbox_length = 0;
    }

    /// Answer a call the program made.
    ///
    /// The promise it returned settles, and the reaction runs as a job, so a
    /// completion never runs program code at the moment it arrives.
    pub fn complete_call(
        &mut self,
        request: u64,
        disposition: Disposition,
        value: Value,
    ) -> Result<(), CallError> {
        let pending = {
            let Some(bindings) = self.bindings.as_deref_mut() else {
                return Err(CallError::UnknownRequest);
            };
            bindings.complete(request)?
        };
        if !pending.promise.is_object() {
            return Ok(());
        }
        let state = match disposition {
            Disposition::Fulfilled => promise::FULFILLED,
            Disposition::Rejected => promise::REJECTED,
        };
        let Some(queue) = self.queue.as_deref_mut() else {
            return Err(CallError::UnknownRequest);
        };
        promise::settle(self.heap, queue, pending.promise.as_handle(), state, value)
            .map(|_| ())
            .map_err(|_| CallError::PendingFull)
    }

    /// Apply a completion record that arrived from outside.
    ///
    /// A rejection settles with an error carrying the typed cause, so the
    /// program sees why rather than only that. The trace the call carried must
    /// match the one that comes back: a completion for the right request under
    /// the wrong trace is refused.
    pub fn apply_completion(&mut self, record: &CompletionRecord) -> Result<(), CallError> {
        let trace = {
            let Some(bindings) = self.bindings.as_deref() else {
                return Err(CallError::UnknownRequest);
            };
            bindings
                .trace_of(record.request)
                .ok_or(CallError::UnknownRequest)?
        };
        if trace != record.trace {
            return Err(CallError::UnknownRequest);
        }

        let value = match record.disposition {
            Disposition::Fulfilled => match record.value {
                Some(number) => Value::number(number),
                None => Value::UNDEFINED,
            },
            Disposition::Rejected => match self.create_cause_error(record.cause) {
                Ok(value) => value,
                Err(_) => Value::UNDEFINED,
            },
        };
        self.complete_call(record.request, record.disposition, value)
    }

    /// The error a rejected completion settles with: an ordinary error object
    /// carrying the cause and whether the same call could succeed again.
    fn create_cause_error(&mut self, cause: Cause) -> Result<Value, Completion> {
        let error = self.create_error(ErrorKind::Error, Value::UNDEFINED)?;
        if !error.is_object() {
            return Ok(error);
        }
        let (name, length) = cause.name();
        let text = self.ascii_string(name.get(..length).unwrap_or(&[]))?;
        let cause_key = self.ascii_key(b"cause")?;
        let retry_key = self.ascii_key(b"retryable")?;
        object::define_own_property(
            self.heap,
            error.as_handle(),
            cause_key,
            Descriptor::data(text, attribute::DEFAULT),
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        object::define_own_property(
            self.heap,
            error.as_handle(),
            retry_key,
            Descriptor::data(Value::boolean(cause.retryable()), attribute::DEFAULT),
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(error)
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

    /// What a host-installed `print` reported: 0 nothing, 1 the async test
    /// protocol's completion line, 2 anything else.
    pub fn print_status(&self) -> u8 {
        self.print_status
    }

    /// How many calls this machine is waiting on.
    pub fn in_flight(&self) -> u32 {
        match &self.bindings {
            Some(bindings) => bindings.in_flight(),
            None => 0,
        }
    }

    /// The identifiers of the calls this machine is waiting on.
    pub fn outstanding(&self, out: &mut [u64]) -> usize {
        match &self.bindings {
            Some(bindings) => bindings.outstanding(out),
            None => 0,
        }
    }

    /// Keep a value alive across collections while the host holds it.
    ///
    /// A host that keeps a result while the calls behind it are outstanding
    /// holds the only reference to it; without this a collection would reclaim
    /// what the host is waiting on.
    pub fn retain(&mut self, value: Value) {
        self.retained = value;
    }

    /// Give the machine the trace every call it makes belongs to.
    pub fn set_trace(&mut self, trace: u64) {
        self.trace = trace;
    }

    /// Read a named property from an object, for a host inspecting a result.
    ///
    /// An accessor answers as absent: running one would run program code, and a
    /// host reading a field is not a place where program code may run.
    pub fn property(&mut self, object: Value, name: &[u8]) -> Result<Value, Completion> {
        if !object.is_object() {
            return Ok(Value::UNDEFINED);
        }
        let key = self.ascii_key(name)?;
        match object::get(self.heap, object.as_handle(), key) {
            Ok(Lookup::Value(value)) => Ok(value),
            _ => Ok(Value::UNDEFINED),
        }
    }

    /// Let the machine collect on its own.
    ///
    /// `roots` is where a collection stages the handles it starts from,
    /// `slice` is how much work one slice does, and `headroom` is the free
    /// arena below which the machine collects rather than waiting to fail.
    pub fn attach_collector(&mut self, roots: &'a mut [Handle], slice: u32, headroom: u32) {
        self.roots_storage = Some(roots);
        self.collection_slice = slice.max(1);
        self.collection_headroom = headroom;
    }

    /// Collections this machine has run.
    pub const fn collections(&self) -> u32 {
        self.collections
    }

    /// Collect if the heap is running low, at an instruction boundary where
    /// every live value is in the accumulator, a register, a frame, the realm,
    /// the interned names, or an outstanding call.
    ///
    /// The work is charged to the same budget as instructions, so a program
    /// that makes a collection necessary pays for it.
    fn maybe_collect(&mut self) -> Option<Completion> {
        self.roots_storage.as_ref()?;
        // A heap runs out of two things: the arena and the handle table. A
        // program that makes many small cells — a call's environment, say —
        // exhausts the table long before the bytes, so both are watched.
        let slots = self.heap.slot_capacity();
        let slot_headroom = (slots / 8).max(16);
        let pressed =
            self.heap.free() < self.collection_headroom || self.heap.free_slots() < slot_headroom;
        if !pressed {
            return None;
        }
        self.collect_now()
    }

    /// Collect immediately, whatever the pressure heuristic says.
    ///
    /// An allocation that failed with garbage still reclaimable — a table
    /// that doubled away from its old copies faster than the headroom check
    /// watched — collects here and retries, so a failure means the live data
    /// truly does not fit.
    fn collect_now(&mut self) -> Option<Completion> {
        self.roots_storage.as_ref()?;
        // Stage the roots into the caller's storage, then collect in slices.
        // The storage is taken out and put back so the machine can read its own
        // state while writing into it.
        let storage = self.roots_storage.take()?;
        let count = self.roots(storage);
        let room = storage.len();
        self.roots_storage = Some(storage);
        if count > room {
            // The host gave too little root storage. Running out of heap is an
            // ordinary outcome; collecting from a partial root set would not
            // be, so the collection does not happen.
            return None;
        }

        let slice = self.collection_slice;
        let started = {
            let storage = self.roots_storage.as_deref()?;
            let roots = storage.get(..count)?;
            self.heap.begin_collection(roots).is_ok()
        };
        if !started {
            return None;
        }

        while crate::gc::collect_slice(self.heap, slice) != crate::heap::Phase::Idle {
            // Each slice is charged to the same budget as instructions, so a
            // program that makes a collection necessary pays for it. A
            // collection always finishes: stopping half way would leave a heap
            // that is neither marked nor compacted.
            self.fuel = self.fuel.saturating_sub(u64::from(slice));
        }
        self.fuel = self.fuel.saturating_sub(u64::from(slice));
        self.collections = self.collections.saturating_add(1);
        None
    }

    /// Give the machine a job queue, which promises need.
    pub fn attach_jobs(&mut self, queue: &'a mut Queue<'a>) {
        self.queue = Some(queue);
    }

    /// Jobs waiting to run.
    pub fn pending_jobs(&self) -> usize {
        match &self.queue {
            Some(queue) => queue.len(),
            None => 0,
        }
    }

    /// Run queued jobs, one at a time and in order, up to `budget` of them.
    ///
    /// Returns how many ran. A job that throws settles the promise derived from
    /// it; nothing else observes the throw, which is what keeps one job's
    /// failure from ending the task.
    pub fn run_jobs(&mut self, budget: u32) -> Result<u32, Completion> {
        let mut ran = 0u32;
        while ran < budget {
            let Some(job) = self.next_job() else {
                return Ok(ran);
            };
            self.run_job(job)?;
            ran += 1;
        }
        Ok(ran)
    }

    fn next_job(&mut self) -> Option<Job> {
        match &mut self.queue {
            Some(queue) => queue.pop(),
            None => None,
        }
    }

    fn run_job(&mut self, job: Job) -> Result<(), Completion> {
        match job.kind {
            JobKind::Settle => {
                if !job.target.is_object() {
                    return Ok(());
                }
                let state = job.derived.as_number();
                let state = if state == 1.0 {
                    promise::FULFILLED
                } else {
                    promise::REJECTED
                };
                self.settle(job.target.as_handle(), state, job.argument)
            }
            JobKind::Adopt => {
                // The promise that adopted the thenable is the derived one; the
                // functions it is given settle it when the thenable does.
                if !job.derived.is_object() {
                    return Ok(());
                }
                let promise = job.derived.as_handle();
                // The resolution already read `then` once; the job calls what
                // that single read produced rather than reading it again.
                let then = job.argument;
                if !self.is_callable_value(then) {
                    return self.settle(promise, promise::FULFILLED, job.target);
                }
                let resolve = self.settle_function(native::PROMISE_SETTLE_FULFILLED, promise)?;
                let reject = self.settle_function(native::PROMISE_SETTLE_REJECTED, promise)?;
                match self.call_value(then, job.target, &[resolve, reject]) {
                    Ok(_) => Ok(()),
                    Err(Completion::Throw(reason)) => {
                        self.settle(promise, promise::REJECTED, reason)
                    }
                    Err(other) => Err(other),
                }
            }
            JobKind::Reaction => {
                let outcome = self.call_value(job.target, Value::UNDEFINED, &[job.argument]);
                if !job.derived.is_object() {
                    // Nothing is waiting on the result, so a throw here is the
                    // task's, exactly as an unhandled rejection would be.
                    return outcome.map(|_| ());
                }
                let derived = job.derived.as_handle();
                match outcome {
                    // A handler that returns a promise makes the derived one
                    // wait for it, which is what makes a chain a chain.
                    Ok(value) => self.resolve(derived, value),
                    Err(Completion::Throw(reason)) => {
                        self.settle(derived, promise::REJECTED, reason)
                    }
                    Err(other) => Err(other),
                }
            }
        }
    }

    /// Settle a promise and schedule whatever was waiting on it.
    /// Resolve a promise with a value, which is not the same as settling it: a
    /// value that is itself thenable is followed rather than held.
    fn resolve(&mut self, promise: Handle, value: Value) -> Result<(), Completion> {
        if value.is_object() {
            if value.as_handle() == promise {
                // A promise resolved with itself can never settle, which the
                // specification makes a rejection rather than a hang.
                let reason = self.create_error(ErrorKind::Type, Value::UNDEFINED)?;
                return self.settle(promise, promise::REJECTED, reason);
            }
            let then_key = self.ascii_key(b"then")?;
            // A `then` getter that throws rejects the promise with what it
            // threw rather than throwing out of the resolution.
            let then = match self.get_property(value, then_key) {
                Ok(method) => method,
                Err(Completion::Throw(reason)) => {
                    return self.settle(promise, promise::REJECTED, reason);
                }
                Err(other) => return Err(other),
            };
            if self.is_callable_value(then) {
                let job = Job {
                    kind: JobKind::Adopt,
                    target: value,
                    argument: then,
                    derived: Value::object(promise),
                };
                let Some(queue) = self.queue.as_deref_mut() else {
                    return Err(Completion::Terminated(Termination::NotImplemented));
                };
                return queue
                    .push(job)
                    .map_err(|_| Completion::Terminated(Termination::QuotaExceeded));
            }
        }
        self.settle(promise, promise::FULFILLED, value)
    }

    fn settle(&mut self, promise: Handle, state: u8, value: Value) -> Result<(), Completion> {
        let Some(queue) = self.queue.as_deref_mut() else {
            return Err(Completion::Terminated(Termination::NotImplemented));
        };
        promise::settle(self.heap, queue, promise, state, value)
            .map(|_| ())
            .map_err(|_| Completion::Terminated(Termination::QuotaExceeded))
    }

    /// The control block, which a host sets between slices.
    pub fn control(&mut self) -> &mut Control {
        &mut self.control
    }

    /// Remaining fuel.
    pub const fn fuel(&self) -> u64 {
        self.fuel
    }

    /// The global object, for a host that reads what a task left behind.
    pub const fn global(&self) -> Value {
        Value::object(self.realm.global)
    }

    /// The heap, for a caller that needs to read a produced value.
    pub fn heap(&self) -> &Heap<'h> {
        self.heap
    }

    /// Write every handle the machine can still reach into `out`, which is
    /// what a collection needs before it starts.
    ///
    /// The roots are the accumulator, every register of every live frame, each
    /// frame's environment and receiver, the realm's objects, and the interned
    /// names.
    pub fn roots(&self, out: &mut [Handle]) -> usize {
        let mut written = 0usize;
        // Every root is counted, whether or not it fits: a caller that gave too
        // little storage must be told, not handed a shorter list that would
        // make a collection reclaim something live.
        let push = |value: Value, out: &mut [Handle], written: &mut usize| {
            if matches!(
                value.tag(),
                Tag::String | Tag::Symbol | Tag::BigInt | Tag::Object
            ) {
                if let Some(slot) = out.get_mut(*written) {
                    *slot = value.as_handle();
                }
                *written += 1;
            }
        };

        push(self.accumulator, out, &mut written);
        push(self.retained, out, &mut written);
        push(self.pending_eval, out, &mut written);
        push(self.pending_eval_environment, out, &mut written);
        push(self.pending_eval_this, out, &mut written);
        push(self.pending_eval_callee, out, &mut written);
        push(self.pending_eval_prototype, out, &mut written);
        push(self.pending_eval_fields, out, &mut written);
        let mut index = 0usize;
        while index < self.top as usize {
            if let Some(&value) = self.registers.get(index) {
                push(value, out, &mut written);
            }
            index += 1;
        }
        let mut depth = 0usize;
        while depth < self.depth as usize {
            if let Some(frame) = self.frames.get(depth) {
                push(frame.environment, out, &mut written);
                push(frame.this, out, &mut written);
                push(frame.callee, out, &mut written);
                push(frame.promise, out, &mut written);
            }
            depth += 1;
        }
        for realm in self.realms.iter().flatten() {
            for handle in [
                realm.global,
                realm.environment,
                realm.lexical,
                realm.var_names,
                realm.object_prototype,
                realm.array_prototype,
                realm.function_prototype,
                realm.error_prototype,
                realm.promise_prototype,
                realm.string_prototype,
                realm.number_prototype,
                realm.boolean_prototype,
                realm.symbol_prototype,
                realm.iterator_prototype,
                realm.big_int_prototype,
                realm.iterator_symbol,
                realm.async_iterator_symbol,
                realm.dispose_symbol,
                realm.async_dispose_symbol,
                realm.generator_function_prototype,
                realm.generator_object_prototype,
                realm.async_generator_function_prototype,
                realm.async_generator_object_prototype,
                realm.async_function_prototype,
                realm.map_prototype,
                realm.weak_ref_prototype,
                realm.date_prototype,
                realm.array_buffer_prototype,
                realm.data_view_prototype,
                realm.typed_array_prototype,
                realm.shared_array_buffer_prototype,
                realm.set_prototype,
                realm.weak_map_prototype,
                realm.weak_set_prototype,
                realm.to_primitive_symbol,
                realm.to_string_tag_symbol,
                realm.species_symbol,
                realm.unscopables_symbol,
                realm.hint_default,
                realm.hint_number,
                realm.hint_string,
                realm.has_instance_symbol,
            ] {
                if let Some(slot) = out.get_mut(written) {
                    *slot = handle;
                }
                written += 1;
            }
            for handle in realm.typed_array_prototypes {
                if let Some(slot) = out.get_mut(written) {
                    *slot = handle;
                }
                written += 1;
            }
            for &handle in &realm.error_prototypes {
                if let Some(slot) = out.get_mut(written) {
                    *slot = handle;
                }
                written += 1;
            }
        }
        // A linked closure's environments and namespaces live only here
        // between runs of its modules.
        if let Some(modules) = &self.modules {
            for instance in modules.iter() {
                push(instance.environment, out, &mut written);
                push(instance.namespace, out, &mut written);
                push(instance.deferred_namespace, out, &mut written);
                push(instance.completion, out, &mut written);
            }
        }
        if let Some(queue) = &self.queue {
            written += queue.roots(out.get_mut(written..).unwrap_or(&mut []));
        }
        if let Some(bindings) = &self.bindings {
            written += bindings.roots(out.get_mut(written..).unwrap_or(&mut []));
        }
        for &handle in self.atoms.handles() {
            if let Some(slot) = out.get_mut(written) {
                *slot = handle;
            }
            written += 1;
        }
        written
    }

    /// Run a whole collection from the machine's own roots.
    ///
    /// Collection happens between instructions, never inside one, so nothing
    /// the interpreter is holding on its own stack can be missed.
    pub fn collect(&mut self, roots: &mut [Handle], slice: u32) -> bool {
        let written = self.roots(roots);
        // A root list that did not fit is not a root list.
        let Some(roots) = roots.get(..written) else {
            return false;
        };
        crate::gc::collect(self.heap, roots, slice).is_ok()
    }

    /// The string a value displays as, which is what a caller reporting a
    /// result needs.
    pub fn display(&mut self, value: Value) -> Result<Handle, Completion> {
        let text = self.coerce_to_string(value)?;
        Ok(text.as_handle())
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

    /// Make the environment a module's top-level bindings live in.
    ///
    /// A module's environment is made before it runs and given to it, which is
    /// what lets another module read its exports once it has.
    pub fn create_module_environment(&mut self, module: u32) -> Result<Value, Completion> {
        let unit = self.unit_of(module);
        let entry = unit.header().entry_function;
        let slots = unit
            .function(entry)
            .map_or(0, |function| function.context_slots);
        let record = env::create(
            self.heap,
            EnvironmentKind::Declarative,
            Value::object(self.realm.lexical),
            slots,
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        self.declare_slots(record, slots)?;
        Ok(Value::object(record))
    }

    /// What a module exports under a name, for a host reading a result out of
    /// a closure it evaluated.
    pub fn module_export(&mut self, module: u32, name: &[u16]) -> Option<Value> {
        let slot = self.unit_of(module).export_slot(name)?;
        let environment = self.module_environment(module);
        if !environment.is_object() {
            return None;
        }
        env::slot_value(self.heap, environment.as_handle(), slot).ok()
    }

    /// Keep what evaluating a module answered, for a dependant to wait on.
    pub fn set_module_completion(&mut self, module: u32, completion: Value) {
        if let Some(modules) = self.modules.as_deref_mut() {
            if let Some(instance) = modules.get_mut(module as usize) {
                instance.completion = completion;
            }
        }
    }

    /// What evaluating a module answered, or undefined before it ran.
    pub fn module_completion(&self, module: u32) -> Value {
        match &self.modules {
            Some(modules) => modules
                .get(module as usize)
                .map_or(Value::UNDEFINED, |instance| instance.completion),
            None => Value::UNDEFINED,
        }
    }

    /// Give a module the environment its bindings live in.
    pub fn set_module_environment(&mut self, module: u32, environment: Value) {
        if let Some(modules) = self.modules.as_deref_mut() {
            if let Some(instance) = modules.get_mut(module as usize) {
                instance.environment = environment;
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

    /// Begin one module of a linked closure, in the environment it was given.
    pub fn start_module(&mut self, module: u32) -> Result<(), Completion> {
        self.entry_module = module;
        if let Some(modules) = self.modules.as_deref_mut() {
            if let Some(instance) = modules.get_mut(module as usize) {
                instance.evaluated = 1;
            }
        }
        let entry = self.unit_of(module).header().entry_function;
        let environment = self.module_environment(module);
        if !environment.is_object() {
            return Err(Completion::Terminated(Termination::Malformed));
        }
        self.depth = 0;
        self.top = 0;
        self.push_frame(
            entry,
            environment,
            Value::UNDEFINED,
            Value::UNDEFINED,
            module,
        )?;
        // An instantiated module's body starts past its prologue, keeping
        // the closures instantiation already bound.
        let resume = match &self.modules {
            Some(modules) => modules
                .get(module as usize)
                .map_or(0, |instance| instance.body_pc),
            None => 0,
        };
        if resume != 0 {
            self.frames[self.depth as usize - 1].pc = resume;
        }
        self.started = true;
        Ok(())
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
            return Progress::Finished(Completion::Terminated(Termination::Malformed));
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

    /// The source the machine is paused on, when an `eval` call is waiting
    /// for the host to compile it.
    pub fn pending_eval(&self) -> Option<Handle> {
        if self.pending_eval.is_string() {
            Some(self.pending_eval.as_handle())
        } else {
            None
        }
    }

    /// The global lexical binding of a name, if a script declared one: the
    /// record holding it and its index, walking the chain of records beneath
    /// the head down to the global object's environment.
    fn global_lexical_find(&mut self, name: Handle) -> Result<Option<(Handle, u32)>, Completion> {
        let mut current = self.realm.lexical;
        let mut depth = 0u32;
        loop {
            if env::kind(self.heap, current)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?
                != EnvironmentKind::Declarative
            {
                return Ok(None);
            }
            if let Some(index) = env::index_of(self.heap, current, name)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?
            {
                return Ok(Some((current, index)));
            }
            let parent = env::parent(self.heap, current)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            if !parent.is_object() || depth > env::MAX_SCOPE_DEPTH {
                return Ok(None);
            }
            current = parent.as_handle();
            depth += 1;
        }
    }

    /// Declare a global lexical binding, threading a fresh record beneath
    /// the head when it has no room left.
    fn global_lexical_declare(&mut self, name: Handle, flags: u8) -> Result<(), Completion> {
        let head = self.realm.lexical;
        if self.declare_in_chain(head, name, flags)? {
            return Ok(());
        }
        let parent = env::parent(self.heap, head)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        let fresh = env::create(
            self.heap,
            EnvironmentKind::Declarative,
            parent,
            crate::realm::GLOBAL_LEXICAL_CAPACITY * 4,
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        env::set_parent(self.heap, head, Value::object(fresh))
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        env::declare(self.heap, fresh, name, flags)
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(())
    }

    /// Declare a name in the first declarative record of a chain with room,
    /// answering whether one had any.
    fn declare_in_chain(
        &mut self,
        head: Handle,
        name: Handle,
        flags: u8,
    ) -> Result<bool, Completion> {
        let mut current = head;
        let mut depth = 0u32;
        loop {
            if env::kind(self.heap, current)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?
                != EnvironmentKind::Declarative
            {
                return Ok(false);
            }
            if env::declare(self.heap, current, name, flags).is_ok() {
                return Ok(true);
            }
            let parent = env::parent(self.heap, current)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            if !parent.is_object() || depth > env::MAX_SCOPE_DEPTH {
                return Ok(false);
            }
            current = parent.as_handle();
            depth += 1;
        }
    }

    /// Read a global lexical binding, if the name has one: its dead zone is
    /// the ReferenceError the specification makes it.
    fn global_lexical_read(&mut self, key: Key) -> Result<Option<Value>, Completion> {
        let Key::Name(name) = key else {
            return Ok(None);
        };
        let Some((environment, index)) = self.global_lexical_find(name)? else {
            return Ok(None);
        };
        match env::slot_value(self.heap, environment, index) {
            Ok(value) => Ok(Some(value)),
            Err(env::EnvironmentError::Uninitialised) => Err(self.throw_reference_error()),
            Err(_) => Err(Completion::Terminated(Termination::Malformed)),
        }
    }

    /// Write a global lexical binding, if the name has one, answering whether
    /// it did: a const refuses with a TypeError, a binding still in its dead
    /// zone with a ReferenceError.
    fn global_lexical_write(&mut self, key: Key, value: Value) -> Result<bool, Completion> {
        let Key::Name(name) = key else {
            return Ok(false);
        };
        let Some((environment, index)) = self.global_lexical_find(name)? else {
            return Ok(false);
        };
        let flags = env::binding_flags(self.heap, environment, index)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        if flags & env::binding::INITIALISED == 0 {
            return Err(self.throw_reference_error());
        }
        if flags & env::binding::MUTABLE == 0 {
            return Err(self.throw_type_error());
        }
        env::set_slot(self.heap, environment, index, value)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        self.accumulator = value;
        Ok(true)
    }

    /// Whether a script has declared the name with `var` or as a function.
    fn global_var_name_declared(&mut self, name: Handle) -> Result<bool, Completion> {
        let mut current = self.realm.var_names;
        let mut depth = 0u32;
        loop {
            if env::index_of(self.heap, current, name)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?
                .is_some()
            {
                return Ok(true);
            }
            let parent = env::parent(self.heap, current)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            if !parent.is_object() || depth > env::MAX_SCOPE_DEPTH {
                return Ok(false);
            }
            current = parent.as_handle();
            depth += 1;
        }
    }

    /// Record a name a script declared with `var` or as a function.
    fn global_var_name_declare(&mut self, name: Handle) -> Result<(), Completion> {
        if self.global_var_name_declared(name)? {
            return Ok(());
        }
        let head = self.realm.var_names;
        if self.declare_in_chain(head, name, env::binding::MUTABLE)? {
            return Ok(());
        }
        let parent = env::parent(self.heap, head)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        let fresh = env::create(
            self.heap,
            EnvironmentKind::Declarative,
            parent,
            crate::realm::GLOBAL_LEXICAL_CAPACITY * 4,
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        env::set_parent(self.heap, head, Value::object(fresh))
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        env::declare(self.heap, fresh, name, env::binding::MUTABLE)
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(())
    }

    /// Whether the image recorded the instruction a frame is on as a direct
    /// eval site.
    fn eval_site_recorded(&self, frame: &Frame) -> bool {
        crate::evalsite::find(
            self.unit_of(frame.module).eval_sites(),
            frame.code,
            frame.pc,
        )
        .is_some()
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

    /// The call site the machine paused on, when the image recorded it as a
    /// direct eval: the module, the function, and the pc of the `Call`.
    pub fn pending_eval_site(&self) -> Option<(u32, u32, u32)> {
        if self.pending_eval_module == u32::MAX {
            None
        } else {
            Some((
                self.pending_eval_module,
                self.pending_eval_function,
                self.pending_eval_pc,
            ))
        }
    }

    /// Enter the unit the host compiled for the pending eval.
    ///
    /// A unit compiled against a recorded direct-eval site runs over the
    /// caller's environment with the caller's `this`; anything else runs as
    /// global code. Either way its completion value answers the `eval` call.
    pub fn enter_eval(&mut self, unit: u32) -> Result<(), Completion> {
        self.pending_eval = Value::UNDEFINED;
        self.pending_eval_script = false;
        self.eval_generation = self.eval_generation.wrapping_add(1);
        let entry = self.unit_of(unit).header().entry_function;
        let index = self.pending_eval_realm;
        if let Some(slot) = self.unit_realm.get_mut(unit as usize) {
            *slot = index;
        }
        let target = self
            .realms
            .get(usize::from(index))
            .copied()
            .flatten()
            .unwrap_or(self.realm);
        let (environment, this) = if self.pending_eval_environment.is_object() {
            (self.pending_eval_environment, self.pending_eval_this)
        } else {
            (Value::object(target.lexical), Value::object(target.global))
        };
        self.pending_eval_module = u32::MAX;
        self.pending_eval_environment = Value::UNDEFINED;
        let kept_this = this;
        let kept_callee = self.pending_eval_callee;
        self.pending_eval_this = Value::UNDEFINED;
        self.pending_eval_callee = Value::UNDEFINED;
        self.push_frame(entry, environment, kept_this, kept_callee, unit)?;
        if self.pending_eval_prototype.is_object() || self.pending_eval_fields.is_object() {
            self.eval_result_depth = self.depth;
        }
        Ok(())
    }

    /// Refuse the pending eval: the source did not compile, and the `eval`
    /// call throws a syntax error the program can catch.
    pub fn fail_eval(&mut self) -> Option<Completion> {
        self.fail_eval_to(1)
    }

    fn fail_eval_to(&mut self, floor: u32) -> Option<Completion> {
        self.pending_eval = Value::UNDEFINED;
        self.pending_eval_module = u32::MAX;
        self.pending_eval_environment = Value::UNDEFINED;
        self.pending_eval_this = Value::UNDEFINED;
        self.pending_eval_callee = Value::UNDEFINED;
        self.pending_eval_prototype = Value::UNDEFINED;
        self.pending_eval_fields = Value::UNDEFINED;
        self.eval_result_depth = u32::MAX;
        self.pending_eval_script = false;
        let completion = self.throw_error_of(ErrorKind::Syntax);
        let Completion::Throw(thrown) = completion else {
            return Some(completion);
        };
        self.unwind(thrown, floor)
    }

    /// Call a function value with a receiver and arguments.
    ///
    /// The arguments arrive in the callee's first registers, which is where its
    /// code expects its parameters.
    pub fn call(
        &mut self,
        callee: Value,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        self.call_value(callee, this, arguments)
    }

    fn call_value(
        &mut self,
        callee: Value,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        if !callee.is_object() {
            return Err(self.throw_type_error());
        }
        let function = callee.as_handle();
        match object::is_callable(self.heap, function) {
            Ok(true) => {}
            _ => return Err(self.throw_type_error()),
        }
        if object::is_native(self.heap, function).unwrap_or(false) {
            let native = object::function_code(self.heap, function)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            // A native may need the function object it was called through, for
            // whatever that function was bound to.
            let previous = self.current_native;
            self.current_native = Some(function);
            // A native runs in the realm it belongs to: what it creates
            // takes that realm's prototypes.
            let saved = self.realm;
            let index = self.realm_index_of_function(function);
            if let Some(realm) = self.realms.get(usize::from(index)).copied().flatten() {
                self.realm = realm;
            }
            let outcome = self.call_native(native, this, arguments);
            self.realm = saved;
            self.current_native = previous;
            return outcome;
        }
        if object::function_flags(self.heap, function).unwrap_or(0) & object::function_flag::CLASS
            != 0
        {
            // A class constructor answers only to `new`.
            return Err(self.throw_type_error());
        }
        let code = object::function_code(self.heap, function)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        let closure = object::function_environment(self.heap, function)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;

        if let Some(stop) = self.check_control() {
            return Err(stop);
        }
        // A function runs the unit of the module it was made in, wherever it
        // is called from.
        let module = object::function_module(self.heap, function).unwrap_or(0);
        // A native that enters JavaScript nests an interpreter loop on the
        // host stack, and the nesting is bounded by its own declared depth:
        // the frame table bounds JavaScript recursion, this bounds the host's.
        if self.nested >= MAX_NESTED_ENTRIES {
            return Err(Completion::Terminated(Termination::StackOverflow));
        }
        let environment = self.prepare_call_environment(closure, this, code, module)?;
        self.push_frame(code, environment, this, callee, module)?;
        // The arguments occupy the callee's first registers.
        let frame = self.frames[self.depth as usize - 1];
        let mut index = 0usize;
        while index < arguments.len() {
            let register = u32::try_from(index).unwrap_or(0);
            if register >= frame.registers {
                break;
            }
            self.set_register(&frame, register, arguments[index]);
            index += 1;
        }
        self.frames[self.depth as usize - 1].argument_count = u32::try_from(index).unwrap_or(0);
        // The nested run is bounded by fuel like any other, and what it burns
        // is charged against the outer slice when one is open, so a module
        // step that did heavy nested work hands control back promptly rather
        // than pretending the work took one instruction.
        let before = self.fuel;
        self.nested += 1;
        let completion = self.execute();
        self.nested -= 1;
        let spent = before.saturating_sub(self.fuel);
        self.slice = self.slice.saturating_sub(spent);
        match completion {
            Completion::Value(value) => Ok(value),
            other => Err(other),
        }
    }

    /// Enter a call without recursing in the host.
    ///
    /// A call to a function made of bytecode pushes a frame and lets the
    /// instruction loop run it: what the callee returns lands in the
    /// accumulator, which is where the caller expects its result. Only a native
    /// is called on the host's stack, because a native is host code.
    ///
    /// Answers whether a frame was pushed.
    fn enter_call(
        &mut self,
        callee: Value,
        this: Value,
        arguments: &[Value],
        construct: bool,
    ) -> Result<bool, Completion> {
        if !callee.is_object() {
            return Ok(false);
        }
        let function = callee.as_handle();
        match object::is_callable(self.heap, function) {
            Ok(true) => {}
            _ => return Ok(false),
        }
        if object::is_native(self.heap, function).unwrap_or(false) {
            // `eval` with a string pauses the machine: the compiler lives in
            // the host, which compiles the source into a unit and enters it.
            // Anything else `eval` answers unchanged, as the specification
            // says it does.
            let native = object::function_code(self.heap, function).unwrap_or(u32::MAX);
            if matches!(
                native,
                crate::realm::native::GENERATOR_NEXT
                    | crate::realm::native::GENERATOR_RETURN
                    | crate::realm::native::GENERATOR_THROW
            ) && !construct
            {
                // A suspended sync generator resumes on this loop, its frame
                // standing in for the call: an eval inside it can pause the
                // machine, and its yield answers the call.
                let generator = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                if generator.is_object() {
                    if let Some((raw_state, coroutine)) =
                        object::generator(self.heap, generator.as_handle())
                            .map_err(|_| Completion::Terminated(Termination::Malformed))?
                    {
                        let async_bit = raw_state & object::generator_state::ASYNC;
                        let state = raw_state & !object::generator_state::ASYNC;
                        if async_bit == 0
                            && state == object::generator_state::SUSPENDED
                            && coroutine.is_object()
                        {
                            let star = self.coroutine_is_star(coroutine)?;
                            let kind = if native == crate::realm::native::GENERATOR_THROW {
                                resume::THROW
                            } else if native == crate::realm::native::GENERATOR_RETURN {
                                resume::RETURN
                            } else {
                                resume::NEXT
                            };
                            // A yield reads a throw or return itself; a
                            // generator not yet started answers those
                            // without running, which the native does.
                            if kind == resume::NEXT || star {
                                let _ = object::set_generator(
                                    self.heap,
                                    generator.as_handle(),
                                    object::generator_state::RUNNING,
                                    Value::UNDEFINED,
                                );
                                let argument =
                                    arguments.first().copied().unwrap_or(Value::UNDEFINED);
                                self.restore_coroutine(coroutine, argument, kind, true)?;
                                return Ok(true);
                            }
                        }
                    }
                }
            }
            if native == crate::realm::native::FUNCTION_PROTOTYPE_CALL && !construct {
                // `f.call(this, ...)` enters `f` directly: a first-class
                // frame, so an eval inside `f` can still pause the machine.
                let bound_this = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                let rest = arguments.get(1..).unwrap_or(&[]);
                return self.enter_call(this, bound_this, rest, false);
            }
            if native == crate::realm::native::FUNCTION_PROTOTYPE_APPLY && !construct {
                let bound_this = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                let list = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
                if list.is_nullish() {
                    return self.enter_call(this, bound_this, &[], false);
                }
                if list.is_object() {
                    let length = self.length_of(list)?;
                    let mut values = [Value::UNDEFINED; MAX_ARGUMENTS];
                    let count = (length as usize).min(values.len());
                    let mut index = 0usize;
                    while index < count {
                        values[index] = self.element(list, u32::try_from(index).unwrap_or(0))?;
                        index += 1;
                    }
                    return self.enter_call(this, bound_this, &values[..count], false);
                }
                return Err(self.throw_type_error());
            }
            if native == crate::realm::native::EVAL_SCRIPT && !construct {
                // `$262.evalScript`: the source compiles as a script of its
                // own and runs as global code, declaration instantiation
                // and all.
                let source = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                let source = self.coerce_to_string(source)?;
                self.pending_eval = source;
                self.pending_eval_realm = self.realm_index_of_function(function);
                self.pending_eval_script = true;
                self.pending_eval_module = u32::MAX;
                self.pending_eval_function = u32::MAX;
                self.pending_eval_pc = u32::MAX;
                self.pending_eval_environment = Value::UNDEFINED;
                self.pending_eval_this = Value::UNDEFINED;
                return Ok(true);
            }
            if native == crate::realm::native::EVAL && !construct {
                let source = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                if source.is_string() {
                    self.pending_eval = source;
                    self.pending_eval_realm = self.realm_index_of_function(function);
                    // The Call arm overwrites these when the site is a
                    // recorded direct eval; anything else runs as global.
                    self.pending_eval_module = u32::MAX;
                    self.pending_eval_function = u32::MAX;
                    self.pending_eval_pc = u32::MAX;
                    self.pending_eval_environment = Value::UNDEFINED;
                    self.pending_eval_this = Value::UNDEFINED;
                } else {
                    self.accumulator = source;
                }
                return Ok(true);
            }
            // `Function(...)` is an eval in a wrapper: the parameters and the
            // body are assembled into a function expression, and the value
            // that expression evaluates to answers the call.
            if Self::builds_from_source(native) {
                let source = self.function_source(native, arguments)?;
                self.pending_eval = source;
                self.pending_eval_realm = self.realm_index_of_function(function);
                return Ok(true);
            }
            return Ok(false);
        }
        if !construct
            && object::function_flags(self.heap, function).unwrap_or(0)
                & object::function_flag::CLASS
                != 0
        {
            // A class constructor answers only to `new`.
            return Err(self.throw_type_error());
        }
        let code = object::function_code(self.heap, function)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        let closure = object::function_environment(self.heap, function)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        if let Some(stop) = self.check_control() {
            return Err(stop);
        }
        let module = object::function_module(self.heap, function).unwrap_or(0);
        let environment = self.prepare_call_environment(closure, this, code, module)?;
        self.bind_new_target(environment, construct, callee)?;
        self.push_frame(code, environment, this, callee, module)?;
        let index = self.depth as usize - 1;
        self.frames[index].construct = construct;
        let frame = self.frames[index];
        let mut position = 0usize;
        while position < arguments.len() {
            let register = u32::try_from(position).unwrap_or(0);
            if register >= frame.registers {
                break;
            }
            self.set_register(&frame, register, arguments[position]);
            position += 1;
        }
        self.frames[index].argument_count = u32::try_from(position).unwrap_or(0);
        Ok(true)
    }

    /// `super()` into Function or a generator constructor builds from
    /// source, pausing for the compiler: what it makes answers to
    /// `new.target` and binds as `this` when the eval returns. Answers
    /// whether the parent was such a constructor.
    fn super_builds_from_source(
        &mut self,
        frame: &Frame,
        parent: Value,
        callee: Value,
        arguments: &[Value],
    ) -> Result<bool, Completion> {
        if !parent.is_object() || !object::is_native(self.heap, parent.as_handle()).unwrap_or(false)
        {
            return Ok(false);
        }
        let native = object::function_code(self.heap, parent.as_handle()).unwrap_or(0);
        if !Self::builds_from_source(native) {
            return Ok(false);
        }
        let new_target = self.new_target_of(frame.environment)?;
        let subclass = if new_target.is_object() {
            new_target
        } else {
            callee
        };
        let key = self.ascii_key(b"prototype")?;
        let prototype = self.get_property(subclass, key)?;
        let source = self.function_source(native, arguments)?;
        self.pending_eval = source;
        self.pending_eval_realm = self.realm_index_of_function(parent.as_handle());
        self.pending_eval_prototype = if prototype.is_object() {
            prototype
        } else {
            Value::UNDEFINED
        };
        self.pending_eval_fields = Value::UNDEFINED;
        Ok(true)
    }

    /// Whether a tail call may give up the running frame: the callee is
    /// bytecode the machine will enter as a frame — never a native, which
    /// answers inline and needs the frame to return through — and the frame
    /// is a plain call, with no promise, instance, or eval result waiting
    /// on how it ends.
    fn tail_call_admitted(&self, frame: &Frame, callee: Value) -> bool {
        if self.depth == 0
            || frame.construct
            || frame.this_pending
            || frame.direct_resume
            || frame.promise.is_object()
            || self.eval_result_depth == self.depth
        {
            return false;
        }
        if !callee.is_object() {
            return false;
        }
        let function = callee.as_handle();
        object::is_callable(self.heap, function) == Ok(true)
            && !object::is_native(self.heap, function).unwrap_or(true)
            && object::function_flags(self.heap, function).unwrap_or(0)
                & object::function_flag::CLASS
                == 0
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

    /// The source text `Function(parameters..., body)` denotes — or, for the
    /// generator constructors, the generator function expression it makes.
    fn function_source(&mut self, native: u32, arguments: &[Value]) -> Result<Value, Completion> {
        let head: &[u8] = match native {
            crate::realm::native::GENERATOR_FUNCTION => b"(function* anonymous(",
            crate::realm::native::ASYNC_GENERATOR_FUNCTION => b"(async function* anonymous(",
            crate::realm::native::ASYNC_FUNCTION => b"(async function anonymous(",
            _ => b"(function anonymous(",
        };
        let mut source = self.ascii_string(head)?;
        if arguments.len() > 1 {
            let comma = self.ascii_string(b",")?;
            let mut index = 0usize;
            while index + 1 < arguments.len() {
                if index > 0 {
                    source = self.concat_values(source, comma)?;
                }
                let parameter = self.coerce_to_string(arguments[index])?;
                source = self.concat_values(source, parameter)?;
                index += 1;
            }
        }
        let open = self.ascii_string(b"\n) {\n")?;
        source = self.concat_values(source, open)?;
        if let Some(&last) = arguments.last() {
            let body = self.coerce_to_string(last)?;
            source = self.concat_values(source, body)?;
        }
        let close = self.ascii_string(b"\n})")?;
        self.concat_values(source, close)
    }

    /// The instance a `new` builds, before its constructor runs.
    fn new_instance(&mut self, callee: Value) -> Result<Value, Completion> {
        if !callee.is_object() {
            return Err(self.throw_type_error());
        }
        if !object::is_constructor(self.heap, callee.as_handle()).unwrap_or(false) {
            return Err(self.throw_type_error());
        }
        // The new object's prototype is the constructor's `prototype`
        // property, or the ordinary one when that is not an object.
        let prototype_key = self.ascii_key(b"prototype")?;
        let prototype = self.get_property(callee, prototype_key)?;
        let prototype = if prototype.is_object() {
            prototype
        } else {
            // GetPrototypeFromConstructor: the constructor's own realm's.
            Value::object(self.realm_of_function(callee.as_handle()).object_prototype)
        };
        let instance = object::create(self.heap, prototype)
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(Value::object(instance))
    }

    /// Build the environment a called function starts with: a function record
    /// whose parent is the closure and whose bindings are the arguments.
    fn prepare_call_environment(
        &mut self,
        closure: Value,
        this: Value,
        code: u32,
        module: u32,
    ) -> Result<Value, Completion> {
        let (slots, arrow, strict, dynamic, derived) = match self.unit_of(module).function(code) {
            Some(function) => (
                function.context_slots,
                function.flags & record_flag::ARROW != 0,
                function.flags & record_flag::STRICT != 0,
                function.flags & record_flag::DYNAMIC != 0,
                function.flags & record_flag::DERIVED_CONSTRUCTOR != 0,
            ),
            None => (0, false, false, false, false),
        };
        // An arrow has no `this` of its own, so `this` resolves to the
        // enclosing function's — but its environment is still a variable
        // environment for the `var`s a direct eval may declare into it.
        let kind = if arrow {
            EnvironmentKind::Arrow
        } else {
            EnvironmentKind::Function
        };
        // A function whose code may direct-eval keeps spare capacity for the
        // bindings sloppy eval code creates at run time.
        let capacity = if dynamic {
            slots.saturating_add(EVAL_VAR_SPARE)
        } else {
            slots
        };
        let record = env::create(self.heap, kind, closure, capacity)
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        if !arrow {
            // Strict code takes `this` exactly as passed; sloppy code binds
            // the global for a missing receiver and wraps a primitive one.
            let bound = if strict {
                this
            } else if this.is_nullish() {
                Value::object(self.realm.global)
            } else if !this.is_object() {
                self.coerce_to_object(this)?
            } else {
                this
            };
            env::set_this(self.heap, record, bound)
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
            // A derived constructor's `this` is dead until `super()` binds
            // it, for an arrow reading through as much as for the body.
            if derived {
                env::mark_this_uninitialised(self.heap, record)
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
            }
        }
        self.declare_slots(record, slots)?;
        Ok(Value::object(record))
    }

    /// Give a fresh function environment its `new.target`: the callee under
    /// `new`, unless an entry was named ahead of time, and undefined for a
    /// plain call.
    fn bind_new_target(
        &mut self,
        environment: Value,
        construct: bool,
        callee: Value,
    ) -> Result<(), Completion> {
        let named = self.pending_new_target;
        self.pending_new_target = Value::UNDEFINED;
        let target = if !construct {
            Value::UNDEFINED
        } else if named.is_undefined() {
            callee
        } else {
            named
        };
        if environment.is_object() {
            env::set_new_target(self.heap, environment.as_handle(), target)
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
            env::set_function(self.heap, environment.as_handle(), callee)
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        }
        Ok(())
    }

    /// Whether a frame runs an arrow, whose `super()`, `this` and fields all
    /// belong to the enclosing function.
    fn frame_is_arrow(&self, frame: &Frame) -> bool {
        self.unit_of(frame.module)
            .function(frame.code)
            .is_some_and(|record| record.flags & record_flag::ARROW != 0)
    }

    /// The constructor a frame's `super()` constructs through, with the
    /// environment holding its `this`: the frame's own function, or for an
    /// arrow the nearest enclosing function's.
    fn super_constructor_of(&mut self, frame: &Frame) -> Result<(Value, Value), Completion> {
        if !self.frame_is_arrow(frame) {
            return Ok((frame.callee, frame.environment));
        }
        let mut environment = frame.environment;
        let mut depth = 0u32;
        while environment.is_object() && depth <= env::MAX_SCOPE_DEPTH {
            let handle = environment.as_handle();
            if env::kind(self.heap, handle) == Ok(EnvironmentKind::Function) {
                let function = env::function(self.heap, handle)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                return Ok((function, environment));
            }
            environment = env::parent(self.heap, handle)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            depth += 1;
        }
        Ok((Value::UNDEFINED, Value::UNDEFINED))
    }

    /// Bind `this` for a constructor whose `super()` ran in an arrow: the
    /// environment, and the constructor's own frame where it is still on
    /// the stack.
    fn bind_constructor_this(
        &mut self,
        constructor: Value,
        environment: Value,
        this: Value,
    ) -> Result<(), Completion> {
        if environment.is_object() {
            env::set_this(self.heap, environment.as_handle(), this)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        }
        let mut index = 0usize;
        while index < self.depth as usize {
            let running = self.frames[index];
            if running.construct && value::same_value(running.callee, constructor) {
                self.frames[index].this = this;
                self.frames[index].this_pending = false;
            }
            index += 1;
        }
        Ok(())
    }

    /// The `new.target` visible from an environment: the nearest function
    /// environment's, which is what an arrow or a direct eval reads through.
    fn new_target_of(&mut self, environment: Value) -> Result<Value, Completion> {
        let mut environment = environment;
        let mut depth = 0u32;
        while environment.is_object() && depth <= env::MAX_SCOPE_DEPTH {
            let handle = environment.as_handle();
            if env::kind(self.heap, handle) == Ok(EnvironmentKind::Function) {
                return env::new_target(self.heap, handle)
                    .map_err(|_| Completion::Terminated(Termination::Malformed));
            }
            environment = env::parent(self.heap, handle)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            depth += 1;
        }
        Ok(Value::UNDEFINED)
    }

    /// Give a context its slots, unnamed and uninitialised.
    ///
    /// The lowering resolved every name to an index already, so a slot needs no
    /// name at run time. It starts uninitialised, which is what puts a `let`
    /// before its declaration in the temporal dead zone.
    fn declare_slots(&mut self, record: Handle, slots: u32) -> Result<(), Completion> {
        let mut index = 0u32;
        while index < slots {
            env::declare(self.heap, record, Handle::new(0, 0), env::binding::MUTABLE)
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
            index += 1;
        }
        Ok(())
    }

    fn push_frame(
        &mut self,
        code: u32,
        environment: Value,
        this: Value,
        callee: Value,
        module: u32,
    ) -> Step {
        let Some(function) = self.unit_of(module).function(code) else {
            return Err(Completion::Terminated(Termination::Malformed));
        };
        if self.depth as usize >= self.frames.len() {
            return Err(Completion::Terminated(Termination::StackOverflow));
        }
        let base = self.top;
        let end = base
            .checked_add(function.register_count)
            .ok_or(Completion::Terminated(Termination::RegistersExhausted))?;
        if end as usize > self.registers.len() {
            return Err(Completion::Terminated(Termination::RegistersExhausted));
        }
        let mut index = base;
        while index < end {
            if let Some(slot) = self.registers.get_mut(index as usize) {
                *slot = Value::UNDEFINED;
            }
            index += 1;
        }

        // An async call answers a promise whatever the body does, so the
        // promise exists from the first instruction. An async generator's
        // keeper is the generator object InitialYield makes instead, and
        // until then a parameter error throws to the caller.
        let promise = if function.flags & record_flag::ASYNC != 0
            && function.flags & record_flag::GENERATOR == 0
        {
            Value::object(self.new_promise()?)
        } else {
            Value::UNDEFINED
        };
        self.frames[self.depth as usize] = Frame {
            code,
            pc: 0,
            base,
            registers: function.register_count,
            environment,
            this,
            callee,
            contexts: 0,
            module,
            construct: false,
            argument_count: 0,
            promise,
            this_pending: function.flags & record_flag::DERIVED_CONSTRUCTOR != 0,
            resume_kind: 0,
            direct_resume: false,
        };
        self.depth += 1;
        self.top = end;
        self.sync_realm();
        Ok(())
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
                    return Completion::Terminated(Termination::Malformed);
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
                return Some(Completion::Terminated(Termination::Malformed));
            };
            let Some(code) = self.unit_of(frame.module).code(&function) else {
                return Some(Completion::Terminated(Termination::Malformed));
            };
            let Ok(instruction) = decode(code, frame.pc) else {
                return Some(Completion::Terminated(Termination::Malformed));
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
                            return Some(Completion::Terminated(Termination::Malformed));
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

    /// Resolve a name through the environment chain the way run-time lookup
    /// must: an object environment whose object's `Symbol.unscopables`
    /// blocks the name is stepped past rather than matched.
    fn resolve_name(
        &mut self,
        environment: Handle,
        name: Handle,
    ) -> Result<Option<env::Resolution>, Completion> {
        let mut base = environment;
        let mut skipped = 0u32;
        loop {
            let found = self.resolve_chain(base, name)?;
            let Some(resolution) = found else {
                return Ok(None);
            };
            if resolution.index != u32::MAX {
                return Ok(Some(env::Resolution {
                    depth: resolution.depth + skipped,
                    ..resolution
                }));
            }
            let object = env::binding_object(self.heap, resolution.environment)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            if object.is_object() && resolution.environment != self.realm.environment {
                let blocked = {
                    let unscopables =
                        self.get_property(object, Key::Symbol(self.realm.unscopables_symbol))?;
                    if unscopables.is_object() {
                        let entry = self.get_property(unscopables, Key::Name(name))?;
                        self.coerce_to_boolean(entry)?
                    } else {
                        false
                    }
                };
                if blocked {
                    // Step past this object environment and keep walking.
                    let parent = env::parent(self.heap, resolution.environment)
                        .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                    if !parent.is_object() {
                        return Ok(None);
                    }
                    skipped += resolution.depth + 1;
                    base = parent.as_handle();
                    continue;
                }
            }
            return Ok(Some(env::Resolution {
                depth: resolution.depth + skipped,
                ..resolution
            }));
        }
    }

    /// Whether a key belongs to the engine's hidden namespaces — a private
    /// member's `#` spelling or an internal `\0` record — which reflection
    /// never reports.
    fn hidden_key(&self, key: Key) -> bool {
        let Key::Name(handle) = key else {
            return false;
        };
        matches!(crate::string::unit_at(self.heap, handle, 0), Ok(Some(0)))
    }

    /// A private member read or write demands the member exists: touching
    /// `#name` on an object without it is a TypeError, never `undefined`.
    /// Whether the key is a private name.
    fn is_private_key(&self, key: Key) -> bool {
        if let Key::Name(handle) = key {
            crate::string::unit_at(self.heap, handle, 0) == Ok(Some(u16::from(b'#')))
        } else {
            false
        }
    }

    /// The storage key one class evaluation's private `name` lives under: a
    /// NUL prefix keeps it off every reflective surface, and the identity of
    /// the class's prototype keeps same-named privates of other classes —
    /// and other evaluations of this class — apart.
    fn private_storage_units(
        &mut self,
        name: Handle,
        class: Handle,
        marker: bool,
        units: &mut [u16; 512],
    ) -> Result<usize, Completion> {
        let length = crate::string::length(self.heap, name)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?
            as usize;
        if length + 25 > units.len() {
            return Err(Completion::Terminated(Termination::HeapExhausted));
        }
        units[0] = 0;
        let mut start = 1;
        if marker {
            // A declaration marker: the class declares the name, whether or
            // not any object holds a member under it yet.
            units[1] = u16::from(b'!');
            start = 2;
        }
        crate::string::copy_units(self.heap, name, &mut units[start..start + length])
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        let mut at = start + length;
        units[at] = u16::from(b'@');
        at += 1;
        for part in [class.index, class.generation] {
            let mut value = part;
            let start = at;
            loop {
                units[at] = u16::from(b'0') + (value % 10) as u16;
                at += 1;
                value /= 10;
                if value == 0 {
                    break;
                }
            }
            units[start..at].reverse();
            units[at] = u16::from(b'.');
            at += 1;
        }
        Ok(at)
    }

    fn private_storage_key(
        &mut self,
        name: Handle,
        class: Handle,
        marker: bool,
    ) -> Result<Key, Completion> {
        let mut units = [0u16; 512];
        let at = self.private_storage_units(name, class, marker, &mut units)?;
        let interned = self
            .atoms
            .intern(self.heap, &units[..at])
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(Key::Name(interned))
    }

    fn private_storage_string(
        &mut self,
        name: Handle,
        class: Handle,
        marker: bool,
    ) -> Result<Value, Completion> {
        let mut units = [0u16; 512];
        let at = self.private_storage_units(name, class, marker, &mut units)?;
        let handle = crate::string::create(self.heap, &units[..at])
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(Value::string(handle))
    }

    /// The class evaluation an access site belongs to: the running method's
    /// home names its prototype and constructor.
    fn private_site(&mut self, frame: &Frame) -> Result<(Value, Value), Completion> {
        let mut site_prototype = Value::UNDEFINED;
        let mut site_constructor = Value::UNDEFINED;
        if frame.callee.is_object() {
            if let Ok(Some(home)) = object::home_object(self.heap, frame.callee.as_handle()) {
                let home_value = Value::object(home);
                if object::is_callable(self.heap, home).unwrap_or(false) {
                    site_constructor = home_value;
                    let key = self.ascii_key(b"prototype")?;
                    site_prototype = self.get_property(home_value, key)?;
                } else {
                    site_prototype = home_value;
                    let key = self.ascii_key(b"constructor")?;
                    site_constructor = self.get_property(home_value, key)?;
                }
            }
        }
        Ok((site_prototype, site_constructor))
    }

    /// Whether the receiver carries the site's private member: the brand
    /// check `#x in o` performs.
    fn private_find(&mut self, frame: &Frame, target: Value, key: Key) -> Result<bool, Completion> {
        Ok(self.private_resolve(frame, target, key).is_ok())
    }

    /// Whether `target` was stamped with `expected`'s brand.
    fn carries_brand(&mut self, target: Value, expected: Handle) -> Result<bool, Completion> {
        let brand_key = self.ascii_key(b"\0brand")?;
        let held = object::get_own_property(self.heap, target.as_handle(), brand_key)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        if let Some(descriptor) = held {
            let list = descriptor.value;
            if list.is_object() {
                let count = self.length_of(list)?;
                let mut index = 0u32;
                while index < count {
                    let stamped = self.element(list, index)?;
                    if stamped.is_object() && stamped.as_handle() == expected {
                        return Ok(true);
                    }
                    index += 1;
                }
            }
        }
        Ok(false)
    }

    /// The class prototype lexically enclosing `proto`'s class, when one was
    /// recorded at its definition.
    fn outer_private_scope(&mut self, proto: Value) -> Result<Option<Value>, Completion> {
        if !proto.is_object() {
            return Ok(None);
        }
        let outer_key = self.ascii_key(b"\0outer")?;
        let held = object::get_own_property(self.heap, proto.as_handle(), outer_key)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        Ok(held.map(|descriptor| descriptor.value))
    }

    /// Resolve a private member against the site's class scopes, innermost
    /// first: the descriptor and where it was found, or the field-bearing
    /// receiver, or nothing the site can see.
    fn private_resolve(
        &mut self,
        frame: &Frame,
        target: Value,
        key: Key,
    ) -> Result<PrivateResolution, Completion> {
        let Key::Name(name) = key else {
            return Err(self.throw_type_error());
        };
        let (site_prototype, site_constructor) = self.private_site(frame)?;
        let mut proto = site_prototype;
        let mut ctor = site_constructor;
        let mut depth = 0u32;
        while depth <= env::MAX_SCOPE_DEPTH {
            if !proto.is_object() && !ctor.is_object() {
                break;
            }
            let class = if proto.is_object() {
                proto.as_handle()
            } else {
                ctor.as_handle()
            };
            let mangled = self.private_storage_key(name, class, false)?;
            for (site, is_static) in [(proto, false), (ctor, true)] {
                if !site.is_object() {
                    continue;
                }
                let found = object::get_own_property(self.heap, site.as_handle(), mangled)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                let Some(descriptor) = found else {
                    continue;
                };
                if site.as_handle() != target.as_handle() {
                    // A static private lives on the constructor and answers
                    // to it alone; an instance member asks for the brand.
                    if is_static {
                        return Err(self.throw_type_error());
                    }
                    let branded =
                        proto.is_object() && self.carries_brand(target, proto.as_handle())?;
                    if !branded {
                        return Err(self.throw_type_error());
                    }
                }
                return Ok(PrivateResolution::Member(descriptor, mangled));
            }
            let own = object::get_own_property(self.heap, target.as_handle(), mangled)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            if let Some(descriptor) = own {
                return Ok(PrivateResolution::Field(descriptor, mangled));
            }
            // A class that declares the name — a field no receiver of this
            // walk holds — shadows every outer scope: the resolution stops
            // here rather than reading an outer class's same-named member.
            if proto.is_object() {
                let marker = self.private_storage_key(name, class, true)?;
                let declared = object::get_own_property(self.heap, proto.as_handle(), marker)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                if declared.is_some() {
                    return Err(self.throw_type_error());
                }
            }
            let Some(next) = self.outer_private_scope(proto)? else {
                break;
            };
            proto = next;
            ctor = if next.is_object() {
                let constructor_key = self.ascii_key(b"constructor")?;
                self.get_property(next, constructor_key)?
            } else {
                Value::UNDEFINED
            };
            depth += 1;
        }
        Err(self.throw_type_error())
    }

    /// Read a private member the site can see.
    fn private_get(&mut self, frame: &Frame, target: Value, key: Key) -> Result<Value, Completion> {
        if !target.is_object() {
            return Err(self.throw_type_error());
        }
        match self.private_resolve(frame, target, key)? {
            PrivateResolution::Field(descriptor, _) => Ok(descriptor.value),
            PrivateResolution::Member(descriptor, _) => match descriptor.kind {
                object::DescriptorKind::Data => Ok(descriptor.value),
                object::DescriptorKind::Accessor => {
                    if self.is_callable_value(descriptor.getter) {
                        self.call_value(descriptor.getter, target, &[])
                    } else {
                        Err(self.throw_type_error())
                    }
                }
            },
        }
    }

    /// Write a private member the site can see: a field takes the value, an
    /// accessor's setter runs, a method refuses.
    fn private_set(
        &mut self,
        frame: &Frame,
        target: Value,
        key: Key,
        value: Value,
    ) -> Result<(), Completion> {
        if !target.is_object() {
            return Err(self.throw_type_error());
        }
        match self.private_resolve(frame, target, key)? {
            PrivateResolution::Field(descriptor, mangled) => {
                object::define_own_property(
                    self.heap,
                    target.as_handle(),
                    mangled,
                    Descriptor::data(value, descriptor.attributes),
                )
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                Ok(())
            }
            PrivateResolution::Member(descriptor, mangled) => match descriptor.kind {
                // A static private field is writable data on the class
                // object; a private method is not writable and refuses.
                object::DescriptorKind::Data => {
                    if descriptor.attributes & attribute::WRITABLE != 0 {
                        object::define_own_property(
                            self.heap,
                            target.as_handle(),
                            mangled,
                            Descriptor::data(value, descriptor.attributes),
                        )
                        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                        Ok(())
                    } else {
                        Err(self.throw_type_error())
                    }
                }
                object::DescriptorKind::Accessor => {
                    if self.is_callable_value(descriptor.setter) {
                        self.call_value(descriptor.setter, target, &[value])?;
                        Ok(())
                    } else {
                        Err(self.throw_type_error())
                    }
                }
            },
        }
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
                let left_value = self.register(frame, operands[0]);
                let right_value = self.accumulator;
                // ToNumeric completes for the left operand — wrapper
                // unwrapped, symbol refused — before the right's begins.
                let left_value = self.numeric_value_of(left_value)?;
                let right_value = self.numeric_value_of(right_value)?;
                if self.either_is_big_int(left_value, right_value) {
                    self.accumulator =
                        self.big_int_arithmetic(instruction.opcode, left_value, right_value)?;
                    return Ok(Flow::Continue);
                }
                let left = left_value.as_number();
                let right = right_value.as_number();
                let result = match instruction.opcode {
                    Op::Sub => value::subtract(left, right),
                    Op::Mul => value::multiply(left, right),
                    Op::Div => value::divide(left, right),
                    Op::Mod => value::remainder(left, right),
                    _ => crate::numeric::power(left, right),
                };
                self.accumulator = Value::number(result);
            }
            Op::BitAnd
            | Op::BitOr
            | Op::BitXor
            | Op::ShiftLeft
            | Op::ShiftRight
            | Op::ShiftRightLogical => {
                let left_value = self.register(frame, operands[0]);
                let right_value = self.accumulator;
                let left_value = self.numeric_value_of(left_value)?;
                let right_value = self.numeric_value_of(right_value)?;
                if self.either_is_big_int(left_value, right_value) {
                    self.accumulator =
                        self.big_int_bitwise(instruction.opcode, left_value, right_value)?;
                    return Ok(Flow::Continue);
                }
                let left = self.coerce_to_number(left_value)?;
                let right = self.coerce_to_number(right_value)?;
                let result = match instruction.opcode {
                    Op::BitAnd => value::bitwise_and(left, right),
                    Op::BitOr => value::bitwise_or(left, right),
                    Op::BitXor => value::bitwise_xor(left, right),
                    Op::ShiftLeft => value::shift_left(left, right),
                    Op::ShiftRight => value::shift_right(left, right),
                    _ => value::unsigned_shift_right(left, right),
                };
                self.accumulator = Value::number(result);
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
                let number = outcome.map_err(|_| Completion::Terminated(Termination::Malformed))?;
                self.accumulator = self.big_int_value(&number)?;
            }
            Op::ToNumber => {
                if matches!(self.accumulator.tag(), Tag::BigInt) {
                    return Err(self.throw_type_error());
                }
                let number = self.coerce_to_number(self.accumulator)?;
                self.accumulator = Value::number(number);
            }
            Op::LdaWithReceiver => {
                let key = self.constant_key(operands[0])?;
                let mut receiver = Value::UNDEFINED;
                if let (Key::Name(name), true) = (key, frame.environment.is_object()) {
                    if let Ok(Some(found)) = self.resolve_name(frame.environment.as_handle(), name)
                    {
                        if found.index == u32::MAX {
                            receiver = env::binding_object(self.heap, found.environment)
                                .unwrap_or(Value::UNDEFINED);
                        }
                    }
                }
                self.accumulator = receiver;
            }
            Op::CreateDisposeStack => {
                self.accumulator = self.new_array()?;
            }
            Op::AddDisposable => {
                let stack = self.register(frame, operands[0]);
                let resource = self.accumulator;
                if resource.is_nullish() {
                    return Ok(Flow::Continue);
                }
                if !resource.is_object() {
                    return Err(self.throw_type_error());
                }
                let key = Key::Symbol(self.realm.dispose_symbol);
                let method = self.get_property(resource, key)?;
                if !self.is_callable_value(method) {
                    return Err(self.throw_type_error());
                }
                self.push_disposable(stack, resource, method, Value::UNDEFINED)?;
            }
            Op::AddDisposableAsync => {
                let stack = self.register(frame, operands[0]);
                let resource = self.accumulator;
                if resource.is_nullish() {
                    // Nothing to dispose, but the block still awaits once.
                    self.push_disposable(stack, Value::UNDEFINED, Value::UNDEFINED, Value::TRUE)?;
                    return Ok(Flow::Continue);
                }
                if !resource.is_object() {
                    return Err(self.throw_type_error());
                }
                let key = Key::Symbol(self.realm.async_dispose_symbol);
                let mut method = self.get_property(resource, key)?;
                let mut hint = Value::TRUE;
                if method.is_nullish() {
                    // A resource with only `@@dispose` is disposed by it, the
                    // await that follows being of undefined, not its result.
                    let key = Key::Symbol(self.realm.dispose_symbol);
                    method = self.get_property(resource, key)?;
                    hint = Value::FALSE;
                }
                if !self.is_callable_value(method) {
                    return Err(self.throw_type_error());
                }
                self.push_disposable(stack, resource, method, hint)?;
            }
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
                let number = self.coerce_to_number(self.accumulator)?;
                let result = match instruction.opcode {
                    Op::Inc => value::add(number, 1.0),
                    Op::Dec => value::subtract(number, 1.0),
                    Op::Negate => value::unary_minus(number),
                    Op::BitNot => value::bitwise_not(number),
                    _ => number,
                };
                self.accumulator = Value::number(result);
            }
            Op::LogicalNot => {
                let truth = self.coerce_to_boolean(self.accumulator)?;
                self.accumulator = Value::boolean(!truth);
            }
            Op::TypeOf => {
                let value = self.accumulator;
                let callable = value.is_object()
                    && object::is_callable(self.heap, value.as_handle()).unwrap_or(false);
                let name: &[u8] = match value::type_of(&value, callable) {
                    value::TypeOf::Undefined => b"undefined",
                    value::TypeOf::Object => b"object",
                    value::TypeOf::Boolean => b"boolean",
                    value::TypeOf::Number => b"number",
                    value::TypeOf::String => b"string",
                    value::TypeOf::Symbol => b"symbol",
                    value::TypeOf::BigInt => b"bigint",
                    value::TypeOf::Function => b"function",
                };
                self.accumulator = self.ascii_string(name)?;
            }
            Op::ToString => {
                let value = self.accumulator;
                self.accumulator = self.coerce_to_string(value)?;
            }
            Op::ToPropertyKey => {
                let value = self.accumulator;
                // A symbol is already a key; an object becomes its
                // primitive first, which may itself be a symbol; anything
                // else becomes one by becoming a string.
                let primitive = if value.is_object() {
                    self.coerce_to_primitive(value, Hint::String)?
                } else {
                    value
                };
                if !matches!(primitive.tag(), Tag::Symbol) {
                    self.accumulator = self.coerce_to_string(primitive)?;
                } else {
                    self.accumulator = primitive;
                }
            }

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
            Op::TestIn => {
                let key_value = self.register(frame, operands[0]);
                let target = self.accumulator;
                if !target.is_object() {
                    return Err(self.throw_type_error());
                }
                let key = self.coerce_to_key(key_value)?;
                self.materialise_function_facts(target, key)?;
                // A private member is not a property, and the engine's own
                // hidden records are nobody's: neither answers to `in`.
                let present = if self.hidden_key(key) {
                    false
                } else {
                    self.has_property_of(target, key)?
                };
                self.accumulator = Value::boolean(present);
            }

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
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                if !present {
                    return Err(self.throw_reference_error());
                }
                self.accumulator = self.get_property(Value::object(self.realm.global), key)?;
            }
            Op::LdaGlobalOrUndefined => {
                let key = self.constant_key(operands[0])?;
                if let Some(value) = self.global_lexical_read(key)? {
                    self.accumulator = value;
                    return Ok(Flow::Continue);
                }
                let present = object::has_property(self.heap, self.realm.global, key)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                self.accumulator = if present {
                    self.get_property(Value::object(self.realm.global), key)?
                } else {
                    Value::UNDEFINED
                };
            }
            Op::StaGlobal => {
                let key = self.constant_key(operands[0])?;
                let value = self.accumulator;
                if self.global_lexical_write(key, value)? {
                    return Ok(Flow::Continue);
                }
                self.set_property(Value::object(self.realm.global), key, value)?;
                self.accumulator = value;
            }
            Op::StaGlobalStrict => {
                let key = self.constant_key(operands[0])?;
                let value = self.accumulator;
                if self.global_lexical_write(key, value)? {
                    return Ok(Flow::Continue);
                }
                let present = object::has_property(self.heap, self.realm.global, key)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                if !present {
                    // Strict assignment never creates a binding: a name the
                    // global object lost — or never had — is a reference
                    // error, not a new property.
                    return Err(self.throw_error_of(ErrorKind::Reference));
                }
                let value = self.accumulator;
                self.set_property_of(Value::object(self.realm.global), key, value, true)?;
                self.accumulator = value;
            }
            Op::CheckGlobalLexical => {
                let key = self.constant_key(operands[0])?;
                if let Key::Name(name) = key {
                    let taken = self.global_lexical_find(name)?.is_some()
                        || self.global_var_name_declared(name)?;
                    if taken {
                        return Err(self.throw_error_of(ErrorKind::Syntax));
                    }
                }
                // HasRestrictedGlobalProperty: a non-configurable global
                // property may not be shadowed by a lexical.
                let existing = object::get_own_property(self.heap, self.realm.global, key)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                if let Some(held) = existing {
                    if held.attributes & attribute::CONFIGURABLE == 0 {
                        return Err(self.throw_error_of(ErrorKind::Syntax));
                    }
                }
            }
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
                        .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                self.accumulator = Value::boolean(present);
            }
            Op::StaGlobalResolved => {
                let key = self.constant_key(operands[0])?;
                let resolved = self.register(frame, operands[1]);
                if !(matches!(resolved.tag(), Tag::Boolean) && resolved.as_boolean()) {
                    // The reference was unresolvable when it formed: strict
                    // code throws however the global changed since.
                    return Err(self.throw_error_of(ErrorKind::Reference));
                }
                let value = self.accumulator;
                if self.global_lexical_write(key, value)? {
                    return Ok(Flow::Continue);
                }
                let present = object::has_property(self.heap, self.realm.global, key)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                if !present {
                    return Err(self.throw_error_of(ErrorKind::Reference));
                }
                self.set_property_of(Value::object(self.realm.global), key, value, true)?;
                self.accumulator = value;
            }
            Op::InitGlobalLexical => {
                let key = self.constant_key(operands[0])?;
                let value = self.accumulator;
                let mut done = false;
                if let Key::Name(name) = key {
                    if let Some((environment, index)) = self.global_lexical_find(name)? {
                        env::initialise(self.heap, environment, index, value)
                            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                        done = true;
                    }
                }
                if !done {
                    self.set_property(Value::object(self.realm.global), key, value)?;
                }
                self.accumulator = value;
            }
            Op::DeclareGlobal => {
                let key = self.constant_key(operands[0])?;
                if let Key::Name(name) = key {
                    // A global lexical with this name refuses the `var`: the
                    // SyntaxError declaration instantiation throws.
                    if self.global_lexical_find(name)?.is_some() {
                        return Err(self.throw_error_of(ErrorKind::Syntax));
                    }
                    self.global_var_name_declare(name)?;
                }
                let present = object::has_property(self.heap, self.realm.global, key)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                if !present {
                    // A `var` that is declared and never assigned still reads
                    // as `undefined` rather than as an unresolvable name.
                    let admitted = object::define_own_property(
                        self.heap,
                        self.realm.global,
                        key,
                        Descriptor::data(
                            Value::UNDEFINED,
                            attribute::WRITABLE | attribute::ENUMERABLE,
                        ),
                    )
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                    if !admitted {
                        return Err(self.throw_type_error());
                    }
                }
            }
            Op::DeclareGlobalFunction => {
                let key = self.constant_key(operands[0])?;
                let check_only = operands[1] == 0;
                let from_eval = operands[1] == 2;
                let existing = object::get_own_property(self.heap, self.realm.global, key)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                match existing {
                    None if check_only => {
                        let extensible = object::is_extensible(self.heap, self.realm.global)
                            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                        if !extensible {
                            return Err(self.throw_type_error());
                        }
                    }
                    None => {
                        // An eval's function is configurable, a script's
                        // not: CreateGlobalFunctionBinding's D.
                        let mut attributes = attribute::WRITABLE | attribute::ENUMERABLE;
                        if from_eval {
                            attributes |= attribute::CONFIGURABLE;
                        }
                        let admitted = object::define_own_property(
                            self.heap,
                            self.realm.global,
                            key,
                            Descriptor::data(Value::UNDEFINED, attributes),
                        )
                        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                        if !admitted {
                            return Err(self.throw_type_error());
                        }
                        if !from_eval {
                            if let Key::Name(name) = key {
                                self.global_var_name_declare(name)?;
                            }
                        }
                    }
                    Some(held) => {
                        // CanDeclareGlobalFunction: a configurable property
                        // accepts any redefinition; otherwise only a writable
                        // enumerable data property may take the function.
                        let definable = held.attributes & attribute::CONFIGURABLE != 0
                            || (matches!(held.kind, object::DescriptorKind::Data)
                                && held.attributes & attribute::WRITABLE != 0
                                && held.attributes & attribute::ENUMERABLE != 0);
                        if !definable {
                            return Err(self.throw_type_error());
                        }
                        if !check_only && held.attributes & attribute::CONFIGURABLE != 0 {
                            // A configurable property is redefined outright,
                            // configurable for an eval and not for a script.
                            let mut attributes = attribute::WRITABLE | attribute::ENUMERABLE;
                            if from_eval {
                                attributes |= attribute::CONFIGURABLE;
                            }
                            let _ = object::define_own_property(
                                self.heap,
                                self.realm.global,
                                key,
                                Descriptor::data(Value::UNDEFINED, attributes),
                            );
                        }
                        if !check_only && !from_eval {
                            if let Key::Name(name) = key {
                                self.global_var_name_declare(name)?;
                            }
                        }
                    }
                }
            }
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
            Op::CreateDefaultConstructor => {
                let mut flags = object::function_flag::CONSTRUCTOR
                    | object::function_flag::CLASS
                    | object::function_flag::STRICT;
                if operands[0] != 0 {
                    flags |= object::function_flag::DERIVED;
                }
                let function = object::create_native(
                    self.heap,
                    Value::object(self.realm.function_prototype),
                    native::DEFAULT_CONSTRUCTOR,
                    flags,
                )
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                self.accumulator = Value::object(function);
            }
            Op::MakeClassConstructor => {
                let constructor = self.accumulator;
                let proto = self.register(frame, operands[0]);
                if !constructor.is_object() || !proto.is_object() {
                    return Err(Completion::Terminated(Termination::Malformed));
                }
                let function = constructor.as_handle();
                let mut flags = object::function_flag::CONSTRUCTOR
                    | object::function_flag::CLASS
                    | object::function_flag::STRICT;
                if operands[1] != 0 {
                    flags |= object::function_flag::DERIVED;
                }
                object::add_function_flags(self.heap, function, flags)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                object::set_home_object(self.heap, function, proto.as_handle())
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                // The class was written inside some class's code — or none:
                // the defining site's prototype links the private scopes, so
                // a nested class still sees the outer class's members.
                let (outer_prototype, _) = self.private_site(frame)?;
                if outer_prototype.is_object() {
                    let outer_key = self.ascii_key(b"\0outer")?;
                    object::define_own_property(
                        self.heap,
                        proto.as_handle(),
                        outer_key,
                        Descriptor::data(outer_prototype, attribute::WRITABLE),
                    )
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                }
                let prototype_key = self.ascii_key(b"prototype")?;
                object::define_own_property(
                    self.heap,
                    function,
                    prototype_key,
                    Descriptor::data(proto, 0),
                )
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                let constructor_key = self.ascii_key(b"constructor")?;
                object::define_own_property(
                    self.heap,
                    proto.as_handle(),
                    constructor_key,
                    Descriptor::data(constructor, attribute::WRITABLE | attribute::CONFIGURABLE),
                )
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
            }
            Op::DefineMethod | Op::DefineMethodKeyed => {
                let target = self.register(frame, operands[0]);
                let key = if matches!(instruction.opcode, Op::DefineMethod) {
                    self.constant_key(operands[1])?
                } else {
                    let value = self.register(frame, operands[1]);
                    self.coerce_to_key(value)?
                };
                let method = self.accumulator;
                if !target.is_object() {
                    return Err(Completion::Terminated(Termination::Malformed));
                }
                if method.is_object() {
                    let _ =
                        object::set_home_object(self.heap, method.as_handle(), target.as_handle());
                }
                // A method is named for its key; a private one keeps the
                // name its definition gave it.
                if !self.hidden_key(key) {
                    self.name_closure_for(method, key)?;
                }
                // A private method is not writable — a private write finding
                // one refuses — while a public method stays an ordinary
                // writable property.
                let attributes = if self.hidden_key(key) {
                    attribute::CONFIGURABLE
                } else {
                    attribute::WRITABLE | attribute::CONFIGURABLE
                };
                let admitted = object::define_own_property(
                    self.heap,
                    target.as_handle(),
                    key,
                    Descriptor::data(method, attributes),
                )
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                if !admitted {
                    // `static ['prototype']` and its kin: the definition the
                    // property refuses is a TypeError.
                    return Err(self.throw_type_error());
                }
            }
            Op::DefineClassAccessor | Op::DefineClassAccessorKeyed => {
                let target = self.register(frame, operands[0]);
                let key = if matches!(instruction.opcode, Op::DefineClassAccessor) {
                    self.constant_key(operands[1])?
                } else {
                    let value = self.register(frame, operands[1]);
                    self.coerce_to_key(value)?
                };
                let accessor = self.accumulator;
                if !target.is_object() {
                    return Err(Completion::Terminated(Termination::Malformed));
                }
                if accessor.is_object() {
                    let _ = object::set_home_object(
                        self.heap,
                        accessor.as_handle(),
                        target.as_handle(),
                    );
                }
                self.define_accessor(
                    target,
                    key,
                    accessor,
                    operands[2] == 0,
                    attribute::CONFIGURABLE,
                )?;
            }
            Op::LdaSuperProperty => {
                if frame.this_pending {
                    return Err(self.throw_reference_error());
                }
                let key = self.constant_key(operands[0])?;
                let callee = frame.callee;
                if !callee.is_object() {
                    return Err(self.throw_type_error());
                }
                let home = object::home_object(self.heap, callee.as_handle())
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                let Some(home) = home else {
                    return Err(self.throw_type_error());
                };
                let parent = object::prototype(self.heap, home)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                let receiver = self.this_value(frame)?;
                if !parent.is_object() {
                    // A base that is not an object cannot be read through.
                    return Err(self.throw_type_error());
                }
                self.accumulator = self.super_get(parent, key, receiver)?;
            }
            Op::PrivateKey => {
                let class = self.register(frame, operands[0]);
                let marker = operands[1] != 0;
                let name = self.accumulator;
                if !class.is_object() || !name.is_string() {
                    return Err(Completion::Terminated(Termination::Malformed));
                }
                self.accumulator =
                    self.private_storage_string(name.as_handle(), class.as_handle(), marker)?;
            }
            Op::CacheTemplate => {
                let site = operands[0];
                let built = self.accumulator;
                let registry_key = self.ascii_key(b"\0tpl")?;
                let held = object::get_own_property(self.heap, self.realm.global, registry_key)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                let registry = match held {
                    Some(descriptor) if descriptor.value.is_object() => descriptor.value,
                    _ => {
                        let made = object::create(self.heap, Value::NULL)
                            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                        object::define_own_property(
                            self.heap,
                            self.realm.global,
                            registry_key,
                            Descriptor::data(Value::object(made), attribute::WRITABLE),
                        )
                        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                        Value::object(made)
                    }
                };
                // An eval's templates belong to the parse that made them: a
                // reused unit keys its sites by the entry that ran it.
                let generation = if frame.module == self.entry_module {
                    0
                } else {
                    self.eval_generation
                };
                let mut text = [0u8; 40];
                let written = {
                    let mut at = 0usize;
                    for (value, stop) in [(frame.module, b'_'), (site, b'.'), (generation, b'g')] {
                        let mut digits = value;
                        let start = at;
                        loop {
                            text[at] = b'0' + (digits % 10) as u8;
                            at += 1;
                            digits /= 10;
                            if digits == 0 {
                                break;
                            }
                        }
                        text[start..at].reverse();
                        text[at] = stop;
                        at += 1;
                    }
                    at
                };
                let site_key = self.ascii_key(text.get(..written).unwrap_or(b"?"))?;
                let cached = object::get_own_property(self.heap, registry.as_handle(), site_key)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                match cached {
                    Some(descriptor) if descriptor.value.is_object() => {
                        self.accumulator = descriptor.value;
                    }
                    _ => {
                        // The template object and its raw twin are frozen:
                        // the array a site answers can never be reshaped.
                        let raw_key = self.ascii_key(b"raw")?;
                        let raw = self.get_property(built, raw_key)?;
                        self.freeze_template(built)?;
                        if raw.is_object() && built.is_object() {
                            object::define_own_property(
                                self.heap,
                                built.as_handle(),
                                raw_key,
                                Descriptor::data(raw, 0),
                            )
                            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                            self.freeze_template(raw)?;
                        }
                        object::define_own_property(
                            self.heap,
                            registry.as_handle(),
                            site_key,
                            Descriptor::data(built, attribute::WRITABLE),
                        )
                        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                        self.accumulator = built;
                    }
                }
            }
            Op::GetSuperBase => {
                if frame.this_pending {
                    return Err(self.throw_reference_error());
                }
                let callee = frame.callee;
                if !callee.is_object() {
                    return Err(self.throw_type_error());
                }
                let home = object::home_object(self.heap, callee.as_handle())
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                let Some(home) = home else {
                    return Err(self.throw_type_error());
                };
                self.accumulator = object::prototype(self.heap, home)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            }
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
                let base = self.register(frame, operands[0]);
                let key = if matches!(instruction.opcode, Op::StaSuperNamed) {
                    self.constant_key(operands[1])?
                } else {
                    let key_value = self.register(frame, operands[1]);
                    self.coerce_to_key(key_value)?
                };
                let receiver = self.this_value(frame)?;
                let value = self.accumulator;
                if !base.is_object() {
                    return Err(self.throw_type_error());
                }
                let strict = self.frame_is_strict(frame);
                self.super_set(base, key, value, receiver, strict)?;
                self.accumulator = value;
            }
            Op::CallSuper => {
                let arrow = self.frame_is_arrow(frame);
                let (callee, home) = self.super_constructor_of(frame)?;
                if !callee.is_object() {
                    return Err(self.throw_type_error());
                }
                let parent = object::prototype(self.heap, callee.as_handle())
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                let first = operands[0];
                let count = operands[1];
                let mut arguments = [Value::UNDEFINED; MAX_ARGUMENTS];
                let passed = (count as usize).min(MAX_ARGUMENTS);
                let mut index = 0usize;
                while index < passed {
                    arguments[index] =
                        self.register(frame, first + u32::try_from(index).unwrap_or(0));
                    index += 1;
                }
                let this = if arrow {
                    env::this_value(self.heap, home.as_handle())
                        .map_err(|_| Completion::Terminated(Termination::Malformed))?
                } else {
                    frame.this
                };
                if parent.is_object()
                    && object::is_callable(self.heap, parent.as_handle()) == Ok(true)
                    && !object::is_native(self.heap, parent.as_handle()).unwrap_or(false)
                {
                    // The parent constructs for the same `new.target`.
                    self.pending_new_target = self.new_target_of(frame.environment)?;
                    if self.enter_call(parent, this, &arguments[..passed], true)? {
                        return Ok(Flow::Enter);
                    }
                }
                if self.super_builds_from_source(frame, parent, callee, &arguments[..passed])? {
                    return Ok(Flow::Continue);
                }
                // A native parent — the default constructor, or a library
                // base — runs on the host stack; the instance stays `this`.
                let made = self.super_call_native(parent, this, &arguments[..passed], callee)?;
                if !arrow {
                    if let Some(running) = self.frames.get_mut(self.depth as usize - 1) {
                        running.this = made;
                    }
                    let environment = frame.environment;
                    self.rebind_environment_this(environment, made)?;
                }
                self.accumulator = made;
            }
            Op::GetHeritagePrototype => {
                let parent = self.register(frame, operands[0]);
                if !parent.is_object()
                    || !object::is_constructor(self.heap, parent.as_handle()).unwrap_or(false)
                {
                    return Err(self.throw_type_error());
                }
                let key = self.ascii_key(b"prototype")?;
                let proto = self.get_property(parent, key)?;
                if !proto.is_object() && !proto.is_null() {
                    return Err(self.throw_type_error());
                }
                self.accumulator = proto;
            }
            Op::DynamicImport => {
                let deferred = operands[0] != 0;
                let specifier = self.accumulator;
                let options = self.register(frame, operands[1]);
                self.accumulator = self.dynamic_import(specifier, options, deferred)?;
            }
            Op::InstantiationEnd => {
                // An instantiation pass ends here, its frame discarded and
                // the body's start remembered; an evaluation entered fresh
                // walks straight through.
                if self.instantiating {
                    let resume = self.frames[self.depth as usize - 1].pc;
                    if let Some(modules) = self.modules.as_deref_mut() {
                        if let Some(instance) = modules.get_mut(frame.module as usize) {
                            instance.body_pc = resume;
                        }
                    }
                    self.depth -= 1;
                    self.top = frame.base;
                    self.sync_realm();
                    self.accumulator = Value::UNDEFINED;
                }
            }
            Op::ImportReject => {
                // `import()` never throws: the specifier's coercion happens
                // on the promise's behalf, and its failure is the rejection.
                let specifier = self.accumulator;
                let promise = self.new_promise()?;
                let reason = match self.coerce_to_string(specifier) {
                    // A source-phase import has no host record to answer
                    // it: the specification's linking error is a syntax one.
                    Ok(_) => self.create_error(ErrorKind::Syntax, Value::UNDEFINED)?,
                    Err(Completion::Throw(thrown)) => thrown,
                    Err(other) => return Err(other),
                };
                self.settle(promise, promise::REJECTED, reason)?;
                self.accumulator = Value::object(promise);
            }
            Op::BindThis => {
                let index = self.depth as usize - 1;
                if self.frame_is_arrow(frame) {
                    // An arrow's `super()` binds the enclosing constructor's
                    // `this`: once, in its environment and its frame.
                    let (constructor, home) = self.super_constructor_of(frame)?;
                    if !home.is_object()
                        || !env::this_uninitialised(self.heap, home.as_handle()).unwrap_or(false)
                    {
                        return Err(self.throw_reference_error());
                    }
                    let answered = self.accumulator;
                    let bound = if answered.is_object() {
                        answered
                    } else {
                        env::this_value(self.heap, home.as_handle())
                            .map_err(|_| Completion::Terminated(Termination::Malformed))?
                    };
                    self.bind_constructor_this(constructor, home, bound)?;
                    return Ok(Flow::Continue);
                }
                if !self.frames[index].this_pending {
                    // `super()` binds `this` exactly once — after the parent
                    // constructor has run, which is why the check sits here.
                    return Err(self.throw_reference_error());
                }
                self.frames[index].this_pending = false;
                // `super()` may answer another object — a parent constructor
                // overriding its return — and that object is `this` now,
                // in the frame and in the environment reads resolve through.
                let answered = self.accumulator;
                if answered.is_object() {
                    self.frames[index].this = answered;
                    self.rebind_environment_this(self.frames[index].environment, answered)?;
                }
            }
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
                        .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                }
            }
            Op::Brand => {
                let this = self.this_value(frame)?;
                let (callee, _) = self.super_constructor_of(frame)?;
                if this.is_object() && callee.is_object() {
                    // A non-extensible instance takes no private methods.
                    if !object::is_extensible(self.heap, this.as_handle()).unwrap_or(true) {
                        return Err(self.throw_type_error());
                    }
                    let key = self.ascii_key(b"prototype")?;
                    let proto = self.get_property(callee, key)?;
                    if proto.is_object() {
                        // Initialising the same object under the same class
                        // twice is the TypeError the specification makes it.
                        if self.carries_brand(this, proto.as_handle())? {
                            return Err(self.throw_type_error());
                        }
                        let brand_key = self.ascii_key(b"\0brand")?;
                        let held = object::get_own_property(self.heap, this.as_handle(), brand_key)
                            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                        let list = match held {
                            Some(descriptor) if descriptor.value.is_object() => descriptor.value,
                            _ => {
                                let made = self.new_array()?;
                                object::define_own_property(
                                    self.heap,
                                    this.as_handle(),
                                    brand_key,
                                    Descriptor::data(made, attribute::WRITABLE),
                                )
                                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                                made
                            }
                        };
                        self.append_element(list, Some(proto))?;
                    }
                }
            }
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
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                self.set_frame_environment(Value::object(record));
                self.adjust_contexts(1);
            }
            Op::PushContext => {
                let slots = operands[0];
                let parent = frame.environment;
                let record = env::create(self.heap, EnvironmentKind::Declarative, parent, slots)
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                self.declare_slots(record, slots)?;
                self.set_frame_environment(Value::object(record));
                self.adjust_contexts(1);
            }
            Op::PopContext => {
                let current = frame.environment;
                if !current.is_object() {
                    return Err(Completion::Terminated(Termination::Malformed));
                }
                let parent = env::parent(self.heap, current.as_handle())
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                self.set_frame_environment(parent);
                self.adjust_contexts(-1);
            }
            Op::InitContextSlot => {
                let value = self.accumulator;
                self.init_context_slot(frame, operands[0], operands[1], value)?;
            }
            Op::CreateRegExp => {
                let constant = self
                    .unit()
                    .constant(operands[0])
                    .ok_or(Completion::Terminated(Termination::Malformed))?;
                let mut units = [0u16; 513];
                let length = self
                    .unit()
                    .constant_units(&constant, &mut units)
                    .ok_or(Completion::Terminated(Termination::Malformed))?;
                let flags = crate::regexp::flags_of_token(units.first().copied().unwrap_or(0))
                    .map_err(|_| self.throw_error_of(ErrorKind::Syntax))?;
                let pattern = units.get(1..length).unwrap_or(&[]);
                // A literal makes a new expression every time it is evaluated,
                // because its `lastIndex` is state a program can write.
                let value = self.create_regexp(pattern, flags)?;
                self.accumulator = value;
            }
            Op::CreateClosure => {
                let index = operands[0];
                // An arrow is not a constructor: it has no `this` to bind and
                // no prototype to build an instance from.
                let record_flags = self.unit().function(index).map_or(0, |record| record.flags);
                let plain = record_flags
                    & (record_flag::ARROW
                        | record_flag::ASYNC
                        | record_flag::GENERATOR
                        | record_flag::METHOD)
                    == 0;
                let flags = if plain {
                    object::function_flag::CONSTRUCTOR
                } else {
                    0
                };
                // A generator function is an instance of its own kind's
                // prototype, whose `prototype` names what it instantiates.
                let prototype = if record_flags & record_flag::GENERATOR == 0 {
                    if record_flags & record_flag::ASYNC == 0 {
                        self.realm.function_prototype
                    } else {
                        self.realm.async_function_prototype
                    }
                } else if record_flags & record_flag::ASYNC == 0 {
                    self.realm.generator_function_prototype
                } else {
                    self.realm.async_generator_function_prototype
                };
                // The closure belongs to the module whose code made it, so it
                // runs that module's unit wherever it is called from.
                let function = object::create_function_in(
                    self.heap,
                    Value::object(prototype),
                    index,
                    frame.environment,
                    flags,
                    frame.module,
                )
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                // `length` is the function's own first property, as the
                // specification orders own keys: length, name, prototype.
                let arity = self
                    .unit()
                    .function(index)
                    .map_or(0, |record| record.argument_count);
                let length_key = self.ascii_key(b"length")?;
                object::define_own_property(
                    self.heap,
                    function,
                    length_key,
                    Descriptor::data(Value::number(f64::from(arity)), attribute::CONFIGURABLE),
                )
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                // A generator function is an instance of the generator
                // function prototype, and carries a fresh `prototype` its
                // instances default to.
                let record_flags = self.unit().function(index).map_or(0, |record| record.flags);
                let generator_record = record_flags & record_flag::GENERATOR != 0;
                if generator_record {
                    let asynchronous = record_flags & record_flag::ASYNC != 0;
                    let (function_home, instance_home) = if asynchronous {
                        (
                            self.realm.async_generator_function_prototype,
                            self.realm.async_generator_object_prototype,
                        )
                    } else {
                        (
                            self.realm.generator_function_prototype,
                            self.realm.generator_object_prototype,
                        )
                    };
                    let _ =
                        object::set_prototype(self.heap, function, Value::object(function_home));
                    let instances = object::create(self.heap, Value::object(instance_home))
                        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                    let prototype_key = self.ascii_key(b"prototype")?;
                    object::define_own_property(
                        self.heap,
                        function,
                        prototype_key,
                        Descriptor::data(Value::object(instances), attribute::WRITABLE),
                    )
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                }
                // Every closure keeps the home its surroundings had: an
                // arrow reads `super` through it, and any inner function
                // still names the class whose private scope encloses it.
                // Whether `super` itself is admitted was already decided at
                // compile time, so carrying the home is never a widening.
                if frame.callee.is_object() {
                    if let Ok(Some(home)) = object::home_object(self.heap, frame.callee.as_handle())
                    {
                        let _ = object::set_home_object(self.heap, function, home);
                    }
                }
                self.accumulator = Value::object(function);
            }
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
                        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                }
            }
            Op::NameClosure => {
                let closure = self.accumulator;
                if closure.is_object() {
                    let handle = closure.as_handle();
                    let name_key = self.ascii_key(b"name")?;
                    let absent = object::get_own_property(self.heap, handle, name_key)
                        .unwrap_or(None)
                        .is_none();
                    if absent {
                        let constant = self.constant_key(operands[0])?;
                        let name = self.name_for_key(constant, None)?;
                        object::define_own_property(
                            self.heap,
                            handle,
                            name_key,
                            object::Descriptor::data(name, attribute::CONFIGURABLE),
                        )
                        .map_err(|_| self.heap_failure())?;
                    }
                }
            }
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
            Op::CreateArguments => {
                // An ordinary object, not an array: its `length` does not
                // follow its indices, and `Array.isArray` says no. It borrows
                // the array values iterator so `for (x of arguments)` walks
                // it, which is the one array behaviour it has.
                let count = frame.argument_count;
                let object = object::create(self.heap, Value::object(self.realm.object_prototype))
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                let target = Value::object(object);
                let mut index = 0u32;
                while index < count {
                    let value = self.register(frame, index);
                    object::define_own_property(
                        self.heap,
                        object,
                        Key::Index(index),
                        object::Descriptor::data(value, attribute::DEFAULT),
                    )
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                    index += 1;
                }
                let length_key = self.ascii_key(b"length")?;
                let length = Value::number(crate::softfloat::from_u64(u64::from(count)));
                object::define_own_property(
                    self.heap,
                    object,
                    length_key,
                    object::Descriptor::data(length, attribute::WRITABLE | attribute::CONFIGURABLE),
                )
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                // A simple sloppy parameter list maps the leading indices
                // onto the parameter slots, in both directions.
                let mapped = operands[0].min(count);
                if mapped > 0 {
                    object::map_arguments(self.heap, object, frame.environment, mapped)
                        .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                }
                // `callee` is the function being run, which is what lets an
                // anonymous function call itself through its own arguments —
                // except in strict code, where the accessor refuses, as
                // `Function.prototype.caller` does.
                let callee_key = self.ascii_key(b"callee")?;
                let strict = self
                    .unit_of(frame.module)
                    .function(frame.code)
                    .is_some_and(|record| record.flags & record_flag::STRICT != 0);
                let callee = if strict {
                    let caller_key = self.ascii_key(b"caller")?;
                    let thrower = object::get_own_property(
                        self.heap,
                        self.realm.function_prototype,
                        caller_key,
                    )
                    .unwrap_or(None)
                    .map_or(Value::UNDEFINED, |held| held.getter);
                    object::Descriptor::accessor(thrower, thrower, 0)
                } else {
                    object::Descriptor::data(
                        frame.callee,
                        attribute::WRITABLE | attribute::CONFIGURABLE,
                    )
                };
                object::define_own_property(self.heap, object, callee_key, callee)
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                let values_key = self.ascii_key(b"values")?;
                let values =
                    self.prototype_property(self.realm.array_prototype, target, values_key)?;
                if values.is_object() {
                    object::define_own_property(
                        self.heap,
                        object,
                        Key::Symbol(self.realm.iterator_symbol),
                        object::Descriptor::data(
                            values,
                            attribute::WRITABLE | attribute::CONFIGURABLE,
                        ),
                    )
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                }
                self.accumulator = target;
            }
            Op::GetEnumerable => {
                let value = self.accumulator;
                self.accumulator = self.enumerable_keys(value)?;
            }
            Op::CallWithArray => {
                let callee = self.register(frame, operands[0]);
                let receiver = self.register(frame, operands[1]);
                let list = self.register(frame, operands[2]);
                let length = self.length_of(list)?;
                let mut values = [Value::UNDEFINED; MAX_ARGUMENTS];
                let count = (length as usize).min(values.len());
                let mut index = 0usize;
                while index < count {
                    values[index] = self.element(list, u32::try_from(index).unwrap_or(0))?;
                    index += 1;
                }
                let arguments = values.get(..count).unwrap_or(&[]);
                if self.enter_call(callee, receiver, arguments, false)? {
                    // A spread call of `eval` is as direct as a plain one.
                    if self.pending_eval.is_string() && self.eval_site_recorded(frame) {
                        self.pending_eval_module = frame.module;
                        self.pending_eval_function = frame.code;
                        self.pending_eval_pc = frame.pc;
                        self.pending_eval_environment = frame.environment;
                        self.pending_eval_this = self.this_value(frame)?;
                        self.pending_eval_callee = frame.callee;
                    }
                    return Ok(Flow::Enter);
                }
                let result = self.call_value(callee, receiver, arguments)?;
                self.accumulator = result;
            }
            Op::ConstructWithArray => {
                let callee = self.register(frame, operands[0]);
                let list = self.register(frame, operands[1]);
                let length = self.length_of(list)?;
                let mut values = [Value::UNDEFINED; MAX_ARGUMENTS];
                let count = (length as usize).min(values.len());
                let mut index = 0usize;
                while index < count {
                    values[index] = self.element(list, u32::try_from(index).unwrap_or(0))?;
                    index += 1;
                }
                let gathered = values.get(..count).unwrap_or(&[]);
                if callee.is_object()
                    && object::is_callable(self.heap, callee.as_handle()) == Ok(true)
                    && !object::is_native(self.heap, callee.as_handle()).unwrap_or(false)
                {
                    let instance = self.new_instance(callee)?;
                    if self.enter_call(callee, instance, gathered, true)? {
                        return Ok(Flow::Enter);
                    }
                }
                self.accumulator = self.construct(callee, gathered)?;
            }
            Op::CallSuperWithArray => {
                let arrow = self.frame_is_arrow(frame);
                let (callee, home) = self.super_constructor_of(frame)?;
                if !callee.is_object() {
                    return Err(self.throw_type_error());
                }
                let parent = object::prototype(self.heap, callee.as_handle())
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                let list = self.register(frame, operands[0]);
                let length = self.length_of(list)?;
                let mut values = [Value::UNDEFINED; MAX_ARGUMENTS];
                let count = (length as usize).min(values.len());
                let mut index = 0usize;
                while index < count {
                    values[index] = self.element(list, u32::try_from(index).unwrap_or(0))?;
                    index += 1;
                }
                let this = if arrow {
                    env::this_value(self.heap, home.as_handle())
                        .map_err(|_| Completion::Terminated(Termination::Malformed))?
                } else {
                    frame.this
                };
                if parent.is_object()
                    && object::is_callable(self.heap, parent.as_handle()) == Ok(true)
                    && !object::is_native(self.heap, parent.as_handle()).unwrap_or(false)
                {
                    self.pending_new_target = self.new_target_of(frame.environment)?;
                    if self.enter_call(parent, this, values.get(..count).unwrap_or(&[]), true)? {
                        return Ok(Flow::Enter);
                    }
                }
                if self.super_builds_from_source(
                    frame,
                    parent,
                    callee,
                    values.get(..count).unwrap_or(&[]),
                )? {
                    return Ok(Flow::Continue);
                }
                let made = self.super_call_native(
                    parent,
                    this,
                    values.get(..count).unwrap_or(&[]),
                    callee,
                )?;
                if !arrow {
                    if let Some(running) = self.frames.get_mut(self.depth as usize - 1) {
                        running.this = made;
                    }
                    let environment = frame.environment;
                    self.rebind_environment_this(environment, made)?;
                }
                self.accumulator = made;
            }
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
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
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
                let callee = self.register(frame, operands[0]);
                let receiver = self.register(frame, operands[1]);
                let count = operands[2];
                let mut arguments = [Value::UNDEFINED; MAX_ARGUMENTS];
                let passed = (count.saturating_sub(1) as usize).min(MAX_ARGUMENTS);
                let mut index = 0usize;
                while index < passed {
                    arguments[index] =
                        self.register(frame, operands[1] + 1 + u32::try_from(index).unwrap_or(0));
                    index += 1;
                }
                if matches!(instruction.opcode, Op::TailCall)
                    && self.tail_call_admitted(frame, callee)
                {
                    // A proper tail call: this frame has nothing left to do,
                    // so it goes before the callee's frame is made, and the
                    // callee answers this frame's caller directly.
                    if let Some(stop) = self.check_control() {
                        return Err(stop);
                    }
                    self.depth -= 1;
                    self.top = frame.base;
                    self.sync_realm();
                    self.enter_call(callee, receiver, &arguments[..passed], false)?;
                    return Ok(Flow::Enter);
                }
                if self.enter_call(callee, receiver, &arguments[..passed], false)? {
                    // A pause here may be a direct eval: the site is the
                    // instruction itself, and the eval's code — if the image
                    // recorded the site — runs over this frame's environment
                    // with this frame's `this`. An unrecorded site is an
                    // indirect eval, which runs as global code.
                    if self.pending_eval.is_string() && self.eval_site_recorded(frame) {
                        self.pending_eval_module = frame.module;
                        self.pending_eval_function = frame.code;
                        self.pending_eval_pc = frame.pc;
                        self.pending_eval_environment = frame.environment;
                        self.pending_eval_this = self.this_value(frame)?;
                        self.pending_eval_callee = frame.callee;
                    }
                    return Ok(Flow::Enter);
                }
                let result = self.call_value(callee, receiver, &arguments[..passed])?;
                self.accumulator = result;
            }
            Op::Construct => {
                let callee = self.register(frame, operands[0]);
                let first = operands[1];
                let count = operands[2];
                let mut arguments = [Value::UNDEFINED; MAX_ARGUMENTS];
                let passed = (count as usize).min(MAX_ARGUMENTS);
                let mut index = 0usize;
                while index < passed {
                    arguments[index] =
                        self.register(frame, first + u32::try_from(index).unwrap_or(0));
                    index += 1;
                }
                // A constructor made of bytecode runs in a frame of its own,
                // with the instance as its receiver.
                if callee.is_object()
                    && object::is_callable(self.heap, callee.as_handle()) == Ok(true)
                    && !object::is_native(self.heap, callee.as_handle()).unwrap_or(false)
                {
                    let instance = self.new_instance(callee)?;
                    if self.enter_call(callee, instance, &arguments[..passed], true)? {
                        return Ok(Flow::Enter);
                    }
                }
                // `new Function(...)` builds from source exactly as the call
                // does, pausing for the compiler.
                if callee.is_object()
                    && object::is_native(self.heap, callee.as_handle()).unwrap_or(false)
                {
                    let native = object::function_code(self.heap, callee.as_handle()).unwrap_or(0);
                    if Self::builds_from_source(native) {
                        let source = self.function_source(native, &arguments[..passed])?;
                        self.pending_eval = source;
                        self.pending_eval_realm = self.realm_index_of_function(callee.as_handle());
                        return Ok(Flow::Continue);
                    }
                    // A class with no written constructor deriving from one
                    // of those builds from source for the class.
                    if native == crate::realm::native::DEFAULT_CONSTRUCTOR
                        && object::function_flags(self.heap, callee.as_handle()).unwrap_or(0)
                            & object::function_flag::DERIVED
                            != 0
                    {
                        let parent = object::prototype(self.heap, callee.as_handle())
                            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                        if parent.is_object()
                            && object::is_native(self.heap, parent.as_handle()).unwrap_or(false)
                        {
                            let parent_native =
                                object::function_code(self.heap, parent.as_handle()).unwrap_or(0);
                            if Self::builds_from_source(parent_native) {
                                let key = self.ascii_key(b"prototype")?;
                                let prototype = self.get_property(callee, key)?;
                                let source =
                                    self.function_source(parent_native, &arguments[..passed])?;
                                self.pending_eval = source;
                                self.pending_eval_prototype = if prototype.is_object() {
                                    prototype
                                } else {
                                    Value::UNDEFINED
                                };
                                self.pending_eval_fields = callee;
                                return Ok(Flow::Continue);
                            }
                        }
                    }
                }
                self.accumulator = self.construct(callee, &arguments[..passed])?;
            }

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
                    return Err(Completion::Terminated(Termination::Malformed));
                }
                return Ok(Flow::Await(value));
            }
            Op::Yield => {
                let value = self.accumulator;
                if !frame.promise.is_object() {
                    return Err(Completion::Terminated(Termination::Malformed));
                }
                return Ok(Flow::Yield(value, false));
            }
            Op::YieldStar => {
                let value = self.accumulator;
                if !frame.promise.is_object() {
                    return Err(Completion::Terminated(Termination::Malformed));
                }
                return Ok(Flow::Yield(value, true));
            }
            Op::YieldDelegate => {
                let value = self.accumulator;
                if !frame.promise.is_object() {
                    return Err(Completion::Terminated(Termination::Malformed));
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

    // Coercions.

    fn coerce_to_boolean(&mut self, value: Value) -> Result<bool, Completion> {
        if let Some(truth) = value::to_boolean_primitive(&value) {
            return Ok(truth);
        }
        match value.tag() {
            Tag::String => {
                let length = string::length(self.heap, value.as_handle())
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                Ok(length != 0)
            }
            Tag::BigInt => {
                let number = self.big_int_operand(value)?;
                Ok(!number.is_zero())
            }
            _ => Ok(true),
        }
    }

    fn coerce_to_number(&mut self, value: Value) -> Result<f64, Completion> {
        if let Some(number) = value::to_number_primitive(&value) {
            return Ok(number);
        }
        match value.tag() {
            Tag::String => {
                let mut units = [0u16; MAX_STRING_UNITS];
                let length = string::length(self.heap, value.as_handle())
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?
                    as usize;
                if length > units.len() {
                    return Ok(f64::NAN);
                }
                string::copy_units(self.heap, value.as_handle(), &mut units[..length])
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                Ok(value::string_to_number(&units[..length]))
            }
            Tag::Object => {
                let primitive = self.coerce_to_primitive(value, Hint::Number)?;
                if primitive.is_object() {
                    return Err(self.throw_type_error());
                }
                self.coerce_to_number(primitive)
            }
            _ => Err(self.throw_type_error()),
        }
    }

    /// `ToPrimitive` for an object: try `valueOf`, then `toString`, calling
    /// whichever is callable, in the order the hint requires.
    fn coerce_to_primitive(&mut self, value: Value, hint: Hint) -> Result<Value, Completion> {
        if !value.is_object() {
            return Ok(value);
        }
        // A `Symbol.toPrimitive` method decides for itself, hint and all.
        let hint_value = Value::string(match hint {
            Hint::String => self.realm.hint_string,
            Hint::Number => self.realm.hint_number,
            _ => self.realm.hint_default,
        });
        let exotic = self.get_property(value, Key::Symbol(self.realm.to_primitive_symbol))?;
        if self.is_callable_value(exotic) {
            let result = self.call_value(exotic, value, &[hint_value])?;
            if !result.is_object() {
                return Ok(result);
            }
            return Err(self.throw_type_error());
        }
        // GetMethod: a present Symbol.toPrimitive that is not callable is a
        // TypeError, not a fall-through to valueOf and toString.
        if !exotic.is_nullish() {
            return Err(self.throw_type_error());
        }
        let order: [&[u8]; 2] = match hint {
            Hint::String => [b"toString", b"valueOf"],
            _ => [b"valueOf", b"toString"],
        };
        for name in order {
            let key = self.ascii_key(name)?;
            let method = self.get_property(value, key)?;
            if method.is_object()
                && object::is_callable(self.heap, method.as_handle()).unwrap_or(false)
            {
                let result = self.call_value(method, value, &[])?;
                if !result.is_object() {
                    return Ok(result);
                }
            }
        }
        Err(self.throw_type_error())
    }

    fn coerce_to_string(&mut self, value: Value) -> Result<Value, Completion> {
        match value.tag() {
            Tag::String => Ok(value),
            Tag::BigInt => self.big_int_text(value, 10),
            Tag::Number => {
                let mut units = [0u16; 40];
                let written = string::number_to_string(value.as_number(), &mut units);
                self.make_string(&units[..written])
            }
            Tag::Undefined => self.ascii_string(b"undefined"),
            Tag::Null => self.ascii_string(b"null"),
            Tag::Boolean => {
                if value.as_boolean() {
                    self.ascii_string(b"true")
                } else {
                    self.ascii_string(b"false")
                }
            }
            Tag::Object => {
                let primitive = self.coerce_to_primitive(value, Hint::String)?;
                if primitive.is_object() {
                    return Err(self.throw_type_error());
                }
                self.coerce_to_string(primitive)
            }
            _ => Err(self.throw_type_error()),
        }
    }

    fn make_string(&mut self, units: &[u16]) -> Result<Value, Completion> {
        let handle = string::create(self.heap, units)
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(Value::string(handle))
    }

    fn ascii_string(&mut self, text: &[u8]) -> Result<Value, Completion> {
        let handle = string::create_ascii(self.heap, text)
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(Value::string(handle))
    }

    fn ascii_key(&mut self, text: &[u8]) -> Result<Key, Completion> {
        let mut units = [0u16; 32];
        let mut length = 0usize;
        for &byte in text {
            if length < units.len() {
                units[length] = u16::from(byte);
                length += 1;
            }
        }
        let handle = self
            .atoms
            .intern(self.heap, &units[..length])
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(Key::Name(handle))
    }

    /// The key a value denotes, interning a string so that key comparison stays
    /// a handle comparison.
    fn coerce_to_key(&mut self, value: Value) -> Result<Key, Completion> {
        let value = if value.is_object() {
            self.coerce_to_primitive(value, Hint::String)?
        } else {
            value
        };
        if matches!(value.tag(), Tag::Symbol) {
            return Ok(Key::Symbol(value.as_handle()));
        }
        let text = self.coerce_to_string(value)?;
        let handle = text.as_handle();
        let length = string::length(self.heap, handle)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?
            as usize;
        let mut units = [0u16; MAX_STRING_UNITS];
        if length > units.len() {
            // A long name is never an array index; it is interned by the
            // string it already is.
            let interned = self
                .atoms
                .intern_string(self.heap, handle)
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
            return Ok(Key::Name(interned));
        }
        string::copy_units(self.heap, handle, &mut units[..length])
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        if let Some(index) = string::array_index(&units[..length]) {
            return Ok(Key::Index(index));
        }
        let interned = self
            .atoms
            .intern(self.heap, &units[..length])
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(Key::Name(interned))
    }

    /// Find a name through the environment chain the way a direct eval's
    /// world requires: a binding an eval created is found by its text, an
    /// object environment by its property, and anything else falls to the
    /// global object. `None` is an unresolvable name.
    fn dynamic_read(
        &mut self,
        environment: Value,
        key: Key,
        strict: bool,
    ) -> Result<Option<Value>, Completion> {
        Ok(self
            .dynamic_read_base(environment, key, strict)?
            .map(|(value, _)| value))
    }

    /// `dynamic_read`, also answering the reference's base: the `with`
    /// object that bound the name, or undefined where none did.
    fn dynamic_read_base(
        &mut self,
        environment: Value,
        key: Key,
        strict: bool,
    ) -> Result<Option<(Value, Value)>, Completion> {
        if let Key::Name(name) = key {
            if environment.is_object() {
                match self.resolve_name(environment.as_handle(), name) {
                    Ok(Some(found)) => {
                        if found.index == u32::MAX {
                            let object = env::binding_object(self.heap, found.environment)
                                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                            // GetBindingValue re-asks HasProperty: resolving
                            // the name may have run an unscopables getter
                            // that deleted the binding it found.
                            let still = self.has_property_of(object, key)?;
                            if !still {
                                if strict {
                                    return Err(self.throw_reference_error());
                                }
                                return Ok(Some((Value::UNDEFINED, object)));
                            }
                            let value = self.get_property(object, key)?;
                            return Ok(Some((value, object)));
                        }
                        let value = self.slot_read(found.environment, found.index)?;
                        return Ok(Some((value, Value::UNDEFINED)));
                    }
                    Ok(None) => {}
                    Err(completion) => return Err(completion),
                }
            }
        }
        let present = object::has_property(self.heap, self.realm.global, key)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        if present {
            let value = self.get_property(Value::object(self.realm.global), key)?;
            Ok(Some((value, Value::UNDEFINED)))
        } else {
            Ok(None)
        }
    }

    /// A named binding — one a direct eval created — strictly nearer than the
    /// static slot at `depth`, when one shadows it. Object environments do
    /// not shadow a slot: they sit at the chain's root, beyond it.
    fn shadowing_binding(
        &mut self,
        frame: &Frame,
        key: Key,
        depth: u32,
        _write: bool,
    ) -> Result<Option<Value>, Completion> {
        let Key::Name(name) = key else {
            return Ok(None);
        };
        if !frame.environment.is_object() {
            return Ok(None);
        }
        match self.resolve_name(frame.environment.as_handle(), name) {
            Ok(Some(found)) if found.depth <= depth => {
                if found.index == u32::MAX {
                    // An object environment — a `with` object — interposed.
                    let object = env::binding_object(self.heap, found.environment)
                        .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                    return self.get_property(object, key).map(Some);
                }
                self.slot_read(found.environment, found.index).map(Some)
            }
            Ok(_) => Ok(None),
            Err(_) => Err(Completion::Terminated(Termination::Malformed)),
        }
    }

    /// Write through a shadowing named binding, answering whether one took
    /// the value.
    fn shadowing_store(
        &mut self,
        frame: &Frame,
        name: Handle,
        depth: u32,
        value: Value,
    ) -> Result<bool, Completion> {
        if !frame.environment.is_object() {
            return Ok(false);
        }
        match self.resolve_name(frame.environment.as_handle(), name) {
            Ok(Some(found)) if found.depth <= depth => {
                if found.index == u32::MAX {
                    let object = env::binding_object(self.heap, found.environment)
                        .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                    self.set_property(object, Key::Name(name), value)?;
                    return Ok(true);
                }
                self.slot_write(found.environment, found.index, value)
                    .map(|()| true)
            }
            Ok(_) => Ok(false),
            Err(completion) => Err(completion),
        }
    }

    /// The environment an assignment to a shadowable slot writes into,
    /// chosen when the reference forms: a named binding's environment when
    /// one sits nearer than `depth`, the environment at `depth` otherwise.
    fn prepare_shadowable(
        &mut self,
        environment: Value,
        key: Key,
        depth: u32,
        strict: bool,
    ) -> Result<Value, Completion> {
        if let Key::Name(name) = key {
            if environment.is_object() {
                match self.resolve_name(environment.as_handle(), name) {
                    Ok(Some(found)) if found.depth <= depth => {
                        return Ok(Value::object(found.environment));
                    }
                    Ok(_) => {}
                    Err(completion) => return Err(completion),
                }
            }
        }
        if depth == u32::MAX {
            // A free name falls to the global object's environment — except
            // that strict code decides unresolvable now, as the reference
            // forms: the write throws however the global changes after.
            if strict {
                let lexical = match key {
                    Key::Name(name) => self.global_lexical_find(name)?.is_some(),
                    _ => false,
                };
                let present = lexical
                    || object::has_property(self.heap, self.realm.global, key)
                        .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                if !present {
                    return Ok(Value::NULL);
                }
            }
            return Ok(Value::object(self.realm.environment));
        }
        let mut current = environment;
        let mut remaining = depth;
        while remaining > 0 {
            if !current.is_object() {
                return Err(self.throw_reference_error());
            }
            current = env::parent(self.heap, current.as_handle())
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            remaining -= 1;
        }
        Ok(current)
    }

    fn read_prepared(
        &mut self,
        environment: Value,
        key: Key,
        slot: u32,
    ) -> Result<Value, Completion> {
        if !environment.is_object() {
            return Err(self.throw_reference_error());
        }
        let handle = environment.as_handle();
        if env::kind(self.heap, handle) == Ok(EnvironmentKind::Object) {
            let object = env::binding_object(self.heap, handle)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            // A name the object no longer carries is an unresolvable
            // reference, which a read makes a ReferenceError.
            if object.is_object() {
                let present = self.has_property_of(object, key)?;
                if !present {
                    return Err(self.throw_reference_error());
                }
            }
            return self.get_property(object, key);
        }
        let index = if let Key::Name(name) = key {
            env::index_of(self.heap, handle, name)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?
                .unwrap_or(slot)
        } else {
            slot
        };
        self.slot_read(handle, index)
    }

    fn write_prepared(
        &mut self,
        environment: Value,
        key: Key,
        slot: u32,
        value: Value,
        strict: bool,
    ) -> Result<(), Completion> {
        if !environment.is_object() {
            return Err(self.throw_reference_error());
        }
        let handle = environment.as_handle();
        if env::kind(self.heap, handle) == Ok(EnvironmentKind::Object) {
            let object = env::binding_object(self.heap, handle)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            // Strict code refuses to recreate a binding the object lost
            // between the reference and the write.
            if object.is_object() {
                let present = self.has_property_of(object, key)?;
                if !present && strict {
                    return Err(self.throw_reference_error());
                }
            }
            return self.set_property(object, key, value);
        }
        let index = if let Key::Name(name) = key {
            env::index_of(self.heap, handle, name)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?
                .unwrap_or(slot)
        } else {
            slot
        };
        self.slot_write(handle, index, value)
    }

    /// Read a binding by index: its dead zone is a ReferenceError.
    fn slot_read(&mut self, environment: Handle, index: u32) -> Result<Value, Completion> {
        match env::slot_value(self.heap, environment, index) {
            Ok(value) => Ok(value),
            Err(env::EnvironmentError::Uninitialised) => Err(self.throw_reference_error()),
            Err(_) => Err(Completion::Terminated(Termination::Malformed)),
        }
    }

    /// Write a binding by index: its dead zone is a ReferenceError, and an
    /// immutable binding — a `const` — refuses with a TypeError.
    fn slot_write(
        &mut self,
        environment: Handle,
        index: u32,
        value: Value,
    ) -> Result<(), Completion> {
        match env::set_slot(self.heap, environment, index, value) {
            Ok(()) => Ok(()),
            Err(env::EnvironmentError::Uninitialised) => Err(self.throw_reference_error()),
            Err(env::EnvironmentError::Immutable) => Err(self.throw_type_error()),
            Err(_) => Err(Completion::Terminated(Termination::Malformed)),
        }
    }

    /// Assign a name the way sloppy code does: into the binding that holds
    /// it, or as a new property of the global object.
    fn dynamic_write(
        &mut self,
        environment: Value,
        key: Key,
        value: Value,
        strict: bool,
    ) -> Result<(), Completion> {
        if let Key::Name(name) = key {
            if environment.is_object() {
                match self.resolve_name(environment.as_handle(), name) {
                    Ok(Some(found)) => {
                        if found.index == u32::MAX {
                            let object = env::binding_object(self.heap, found.environment)
                                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                            // SetMutableBinding re-asks HasProperty for the
                            // same reason as the read: strict code refuses a
                            // binding an unscopables getter deleted.
                            let still = self.has_property_of(object, key)?;
                            if !still && strict {
                                return Err(self.throw_reference_error());
                            }
                            return self.set_property(object, key, value);
                        }
                        return self.slot_write(found.environment, found.index, value);
                    }
                    Ok(None) => {}
                    Err(completion) => return Err(completion),
                }
            }
        }
        if strict {
            let present = object::has_property(self.heap, self.realm.global, key)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            if !present {
                return Err(self.throw_reference_error());
            }
        }
        self.set_property(Value::object(self.realm.global), key, value)
    }

    /// Declare a `var` from eval code in the nearest variable environment: a
    /// function or arrow environment when the chain holds one, the global
    /// object otherwise. A binding that exists is left exactly as it is.
    fn declare_eval_var(&mut self, environment: Value, key: Key) -> Result<(), Completion> {
        if let Key::Name(name) = key {
            let mut current = environment;
            let mut depth = 0u32;
            while current.is_object() && depth <= env::MAX_SCOPE_DEPTH {
                let handle = current.as_handle();
                let Ok(kind) = env::kind(self.heap, handle) else {
                    break;
                };
                match kind {
                    EnvironmentKind::Function | EnvironmentKind::Arrow => {
                        let held = env::index_of(self.heap, handle, name)
                            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                        if held.is_none()
                            && env::declare_initialised(
                                self.heap,
                                handle,
                                name,
                                env::binding::MUTABLE,
                                Value::UNDEFINED,
                            )
                            .is_err()
                        {
                            // The spare capacity ran out: the name falls to
                            // the global object rather than the run failing.
                            break;
                        }
                        return Ok(());
                    }
                    EnvironmentKind::Object => break,
                    EnvironmentKind::Declarative => {
                        // A lexical binding between the eval and its variable
                        // environment refuses the `var`: the SyntaxError the
                        // declaration instantiation throws.
                        let held = env::index_of(self.heap, handle, name)
                            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                        if held.is_some() {
                            let completion = self.throw_error_of(ErrorKind::Syntax);
                            let Completion::Throw(reason) = completion else {
                                return Err(completion);
                            };
                            return Err(Completion::Throw(reason));
                        }
                    }
                }
                let Ok(parent) = env::parent(self.heap, handle) else {
                    break;
                };
                current = parent;
                depth += 1;
            }
        }
        if let Key::Name(name) = key {
            // A global lexical with this name refuses the eval's `var`.
            if self.global_lexical_find(name)?.is_some() {
                return Err(self.throw_error_of(ErrorKind::Syntax));
            }
        }
        let present = object::has_property(self.heap, self.realm.global, key)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        if !present {
            let admitted = object::define_own_property(
                self.heap,
                self.realm.global,
                key,
                Descriptor::data(
                    Value::UNDEFINED,
                    attribute::WRITABLE | attribute::ENUMERABLE | attribute::CONFIGURABLE,
                ),
            )
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
            if !admitted {
                // CanDeclareGlobalVar: a global that cannot take the
                // binding is the TypeError the specification makes it.
                return Err(self.throw_type_error());
            }
        }
        Ok(())
    }

    /// Delete a name for eval-touched code: a binding an eval created is
    /// removed, a global property is deleted, and a missing name is already
    /// gone.
    fn dynamic_delete(&mut self, environment: Value, key: Key) -> Result<bool, Completion> {
        if let Key::Name(name) = key {
            if environment.is_object() {
                match self.resolve_name(environment.as_handle(), name) {
                    Ok(Some(found)) => {
                        if found.index != u32::MAX {
                            let flags =
                                env::binding_flags(self.heap, found.environment, found.index)
                                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                            if flags & env::binding::PERMANENT != 0 {
                                return Ok(false);
                            }
                            return env::remove(self.heap, found.environment, found.index)
                                .map(|()| true)
                                .map_err(|_| Completion::Terminated(Termination::Malformed));
                        }
                        let object = env::binding_object(self.heap, found.environment)
                            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                        if object.is_object() && object.as_handle() != self.realm.global {
                            return self.delete_property(object, key);
                        }
                    }
                    Ok(None) => {}
                    Err(completion) => return Err(completion),
                }
            }
        }
        self.delete_property(Value::object(self.realm.global), key)
    }

    fn constant_key(&mut self, index: u32) -> Result<Key, Completion> {
        let value = self.load_constant(index)?;
        self.coerce_to_key(value)
    }

    fn load_constant(&mut self, index: u32) -> Result<Value, Completion> {
        let Some(constant) = self.unit().constant(index) else {
            return Err(Completion::Terminated(Termination::Malformed));
        };
        match constant.kind {
            ConstantKind::Number => Ok(Value::number(constant.value())),
            ConstantKind::String | ConstantKind::Key => {
                let mut units = [0u16; MAX_STRING_UNITS];
                let length = constant.second as usize;
                if length > units.len() {
                    // A long constant goes to the heap straight from the
                    // image, with no buffer of its own between.
                    let bytes = self
                        .unit()
                        .constant_unit_bytes(&constant)
                        .ok_or(Completion::Terminated(Termination::Malformed))?;
                    let handle = string::create_from_le_bytes(self.heap, bytes)
                        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                    return Ok(Value::string(handle));
                }
                if self
                    .unit()
                    .constant_units(&constant, &mut units[..length])
                    .is_none()
                {
                    return Err(Completion::Terminated(Termination::Malformed));
                }
                self.make_string(&units[..length])
            }
            ConstantKind::RegExp => Err(Completion::Terminated(Termination::Malformed)),
            ConstantKind::BigInt => {
                // Read out of the image where it lies: a literal of any length
                // is the number it was written as, or an image the machine
                // refuses, never a prefix of itself.
                let bytes = self
                    .unit()
                    .constant_bytes(&constant)
                    .ok_or(Completion::Terminated(Termination::Malformed))?;
                // The constant carries its radix in front of its digits.
                let radix = u32::from(bytes.first().copied().unwrap_or(10));
                let number =
                    crate::bigint::from_digits(bytes.get(1..).unwrap_or(&[]), radix, false)
                        .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                let handle = crate::bigint::write(self.heap, &number)
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                Ok(Value::big_int(handle))
            }
        }
    }

    // Operators.

    fn add_values(&mut self, left: Value, right: Value) -> Result<Value, Completion> {
        let left = self.coerce_to_primitive(left, Hint::Default)?;
        let right = self.coerce_to_primitive(right, Hint::Default)?;
        if self.either_is_big_int(left, right)
            && !matches!(left.tag(), Tag::String)
            && !matches!(right.tag(), Tag::String)
        {
            return self.big_int_arithmetic(Opcode::Add, left, right);
        }
        if matches!(left.tag(), Tag::String) || matches!(right.tag(), Tag::String) {
            let left_text = self.coerce_to_string(left)?;
            let right_text = self.coerce_to_string(right)?;
            let joined = string::concat(self.heap, left_text.as_handle(), right_text.as_handle())
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
            return Ok(Value::string(joined));
        }
        let left_number = self.coerce_to_number(left)?;
        let right_number = self.coerce_to_number(right)?;
        Ok(Value::number(value::add(left_number, right_number)))
    }

    fn strict_equals(&mut self, left: Value, right: Value) -> Result<bool, Completion> {
        if let Some(equal) = value::strict_equals(&left, &right) {
            return Ok(equal);
        }
        // Two distinct string cells with the same contents are the same value.
        if matches!(left.tag(), Tag::String) && matches!(right.tag(), Tag::String) {
            return string::equals(self.heap, left.as_handle(), right.as_handle())
                .map_err(|_| Completion::Terminated(Termination::Malformed));
        }
        // Two BigInt cells are the same value when they hold the same integer.
        if matches!(left.tag(), Tag::BigInt) && matches!(right.tag(), Tag::BigInt) {
            let left = self.big_int_operand(left)?;
            let right = self.big_int_operand(right)?;
            return Ok(crate::bigint::compare(&left, &right) == core::cmp::Ordering::Equal);
        }
        Ok(false)
    }

    fn loose_equals(&mut self, left: Value, right: Value) -> Result<bool, Completion> {
        if left.tag() == right.tag() {
            return self.strict_equals(left, right);
        }
        match (left.tag(), right.tag()) {
            (Tag::Null, Tag::Undefined) | (Tag::Undefined, Tag::Null) => Ok(true),
            (Tag::BigInt, Tag::Number)
            | (Tag::Number, Tag::BigInt)
            | (Tag::BigInt, Tag::String)
            | (Tag::String, Tag::BigInt) => {
                // A BigInt equals a Number when they are the same mathematical
                // value, whatever their types.
                let ordering = self.big_int_ordering(left, right)?;
                Ok(ordering == Some(core::cmp::Ordering::Equal))
            }
            (Tag::Number, Tag::String) | (Tag::String, Tag::Number) => {
                let left_number = self.coerce_to_number(left)?;
                let right_number = self.coerce_to_number(right)?;
                Ok(value::number_equals(left_number, right_number))
            }
            (Tag::Boolean, _) => {
                let number = self.coerce_to_number(left)?;
                self.loose_equals(Value::number(number), right)
            }
            (_, Tag::Boolean) => {
                let number = self.coerce_to_number(right)?;
                self.loose_equals(left, Value::number(number))
            }
            (Tag::Object, Tag::Number | Tag::String | Tag::BigInt | Tag::Symbol) => {
                let primitive = self.coerce_to_primitive(left, Hint::Default)?;
                self.loose_equals(primitive, right)
            }
            (Tag::Number | Tag::String | Tag::BigInt | Tag::Symbol, Tag::Object) => {
                let primitive = self.coerce_to_primitive(right, Hint::Default)?;
                self.loose_equals(left, primitive)
            }
            _ => Ok(false),
        }
    }

    fn compare(&mut self, opcode: Opcode, left: Value, right: Value) -> Result<Value, Completion> {
        let left_primitive = self.coerce_to_primitive(left, Hint::Number)?;
        let right_primitive = self.coerce_to_primitive(right, Hint::Number)?;
        if matches!(left_primitive.tag(), Tag::String)
            && matches!(right_primitive.tag(), Tag::String)
        {
            let ordering = string::compare(
                self.heap,
                left_primitive.as_handle(),
                right_primitive.as_handle(),
            )
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            let result = match opcode {
                Opcode::TestLess => ordering == core::cmp::Ordering::Less,
                Opcode::TestGreater => ordering == core::cmp::Ordering::Greater,
                Opcode::TestLessEqual => ordering != core::cmp::Ordering::Greater,
                _ => ordering != core::cmp::Ordering::Less,
            };
            return Ok(Value::boolean(result));
        }
        if self.either_is_big_int(left_primitive, right_primitive) {
            let Some(ordering) = self.big_int_ordering(left_primitive, right_primitive)? else {
                return Ok(Value::FALSE);
            };
            let result = match opcode {
                Opcode::TestLess => ordering == core::cmp::Ordering::Less,
                Opcode::TestGreater => ordering == core::cmp::Ordering::Greater,
                Opcode::TestLessEqual => ordering != core::cmp::Ordering::Greater,
                _ => ordering != core::cmp::Ordering::Less,
            };
            return Ok(Value::boolean(result));
        }
        let left_number = self.coerce_to_number(left_primitive)?;
        let right_number = self.coerce_to_number(right_primitive)?;
        let comparison = value::compare_numbers(left_number, right_number);
        if matches!(comparison, value::Comparison::Undefined) {
            return Ok(Value::FALSE);
        }
        let result = match opcode {
            Opcode::TestLess => matches!(comparison, value::Comparison::Less),
            Opcode::TestGreater => matches!(comparison, value::Comparison::Greater),
            Opcode::TestLessEqual => !matches!(comparison, value::Comparison::Greater),
            _ => !matches!(comparison, value::Comparison::Less),
        };
        Ok(Value::boolean(result))
    }

    fn instance_of(&mut self, left: Value, right: Value) -> Result<bool, Completion> {
        // `Symbol.hasInstance` decides first when the right side carries one:
        // whatever it answers, coerced to a boolean, is the result.
        if right.is_object() {
            let method = self.get_property(right, Key::Symbol(self.realm.has_instance_symbol))?;
            if !method.is_nullish() {
                if !self.is_callable_value(method) {
                    return Err(self.throw_type_error());
                }
                let answer = self.call_value(method, right, &[left])?;
                return self.coerce_to_boolean(answer);
            }
        }
        if !right.is_object() || !object::is_callable(self.heap, right.as_handle()).unwrap_or(false)
        {
            return Err(self.throw_type_error());
        }
        if !left.is_object() {
            return Ok(false);
        }
        let key = self.ascii_key(b"prototype")?;
        let prototype = self.get_property(right, key)?;
        if !prototype.is_object() {
            return Err(self.throw_type_error());
        }
        let mut current = object::prototype(self.heap, left.as_handle())
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        let mut depth = 0u32;
        while current.is_object() {
            if current.as_handle() == prototype.as_handle() {
                return Ok(true);
            }
            depth += 1;
            if depth > object::MAX_PROTOTYPE_DEPTH {
                return Err(Completion::Terminated(Termination::Malformed));
            }
            current = object::prototype(self.heap, current.as_handle())
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        }
        Ok(false)
    }

    // Properties.

    fn get_property(&mut self, target: Value, key: Key) -> Result<Value, Completion> {
        if target.is_nullish() {
            return Err(self.throw_type_error());
        }
        // A mapped arguments index reads its parameter's slot.
        if let (Key::Index(index), true) = (key, target.is_object()) {
            if let Ok(Some((environment, mapped))) =
                object::arguments_map(self.heap, target.as_handle())
            {
                if index < 32 && mapped & (1 << index) != 0 && environment.is_object() {
                    if let Ok(value) = env::slot_value(self.heap, environment.as_handle(), index) {
                        return Ok(value);
                    }
                }
            }
        }
        self.materialise_prototype(target, key)?;
        self.materialise_function_facts(target, key)?;
        if matches!(target.tag(), Tag::String) {
            if let Some(value) = self.string_property(target, key)? {
                return Ok(value);
            }
            // A string's other properties are its prototype's, and a method
            // found there is called with the string itself as its receiver.
            return self.prototype_property(self.realm.string_prototype, target, key);
        }
        if !target.is_object() {
            let prototype = match target.tag() {
                Tag::Number => self.realm.number_prototype,
                Tag::Boolean => self.realm.boolean_prototype,
                Tag::Symbol => self.realm.symbol_prototype,
                Tag::BigInt => self.realm.big_int_prototype,
                _ => return Ok(Value::UNDEFINED),
            };
            return self.prototype_property(prototype, target, key);
        }
        match object::exotic_kind(self.heap, target.as_handle()).unwrap_or(0) {
            object::exotic::PROXY if !self.hidden_key(key) => {
                return self.proxy_get(target, key);
            }
            object::exotic::TYPED_ARRAY => {
                if let Key::Index(index) = key {
                    return self.typed_array_read(target, index);
                }
            }
            object::exotic::DEFERRED if !self.hidden_key(key) => {
                self.deferred_trigger(target, Some(key))?;
                // `then` on a module still deferred answers undefined, so
                // resolving a promise with the namespace never runs it.
                if !self.deferred_done(target) {
                    let then = self.ascii_key(b"then")?;
                    if key == then {
                        return Ok(Value::UNDEFINED);
                    }
                }
            }
            _ => {}
        }
        if self.deferred_live && !self.hidden_key(key) {
            self.deferred_chain_trigger(target, key)?;
        }
        let lookup = object::get(self.heap, target.as_handle(), key)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        match lookup {
            Lookup::Absent => Ok(Value::UNDEFINED),
            Lookup::Value(value) => Ok(value),
            Lookup::Accessor(getter) => {
                if getter.is_undefined() {
                    return Ok(Value::UNDEFINED);
                }
                self.call_value(getter, target, &[])
            }
        }
    }

    /// Give a function the `prototype` object it is supposed to have, the first
    /// time anything asks for it.
    ///
    /// Every ordinary function has one, and an instance made from the function
    /// inherits from it — that is what makes `A.prototype.method = ...` reach
    /// every `new A()`. Building it when it is asked for rather than when the
    /// closure is made costs a program that never uses it nothing, and a heap
    /// this size notices the difference: a callback in a loop is a closure too.
    fn materialise_prototype(&mut self, target: Value, key: Key) -> Result<(), Completion> {
        if !target.is_object() {
            return Ok(());
        }
        let handle = target.as_handle();
        let flags = object::function_flags(self.heap, handle).unwrap_or(0);
        if flags & object::function_flag::CONSTRUCTOR == 0 {
            return Ok(());
        }
        // A bound function constructs through its target but owns no
        // `prototype` of its own; nor does `Proxy`, or a proxy over a
        // constructor.
        if object::is_native(self.heap, handle).unwrap_or(false)
            && matches!(
                object::function_code(self.heap, handle),
                Ok(native::BOUND_FUNCTION | native::PROXY | native::PROXY_CALL)
            )
        {
            return Ok(());
        }
        let name = self.ascii_key(b"prototype")?;
        if key != name {
            return Ok(());
        }
        if object::get_own_property(self.heap, handle, name)
            .unwrap_or(None)
            .is_some()
        {
            return Ok(());
        }
        let prototype = object::create(self.heap, Value::object(self.realm.object_prototype))
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        // `constructor` points back, and neither property is enumerable: they
        // are the object's machinery, not its contents.
        let constructor = self.ascii_key(b"constructor")?;
        object::define_own_property(
            self.heap,
            prototype,
            constructor,
            object::Descriptor::data(
                target,
                object::attribute::WRITABLE | object::attribute::CONFIGURABLE,
            ),
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        object::define_own_property(
            self.heap,
            handle,
            name,
            object::Descriptor::data(Value::object(prototype), object::attribute::WRITABLE),
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(())
    }

    /// Give a function its `length` and `name` the first time either is read.
    ///
    /// The length is the declared parameter count, straight from the function
    /// record; the name of a function made of bytecode is the empty string
    /// until the image carries one.
    fn materialise_function_facts(&mut self, target: Value, key: Key) -> Result<(), Completion> {
        if !target.is_object() {
            return Ok(());
        }
        let handle = target.as_handle();
        if object::is_callable(self.heap, handle) != Ok(true) {
            return Ok(());
        }
        let native = object::is_native(self.heap, handle).unwrap_or(true);
        let length_key = self.ascii_key(b"length")?;
        let name_key = self.ascii_key(b"name")?;
        if key != length_key && key != name_key {
            return Ok(());
        }
        // Once made — or deleted, which asks for it first — a fact is never
        // made again: a deleted `name` stays deleted.
        let mark = if key == length_key {
            object::function_flag::MEASURED
        } else {
            object::function_flag::NAMED
        };
        let flags = object::function_flags(self.heap, handle).unwrap_or(0);
        if flags & mark != 0 {
            return Ok(());
        }
        let _ = object::add_function_flags(self.heap, handle, mark);
        if object::get_own_property(self.heap, handle, key)
            .unwrap_or(None)
            .is_some()
        {
            return Ok(());
        }
        let value = if key == length_key {
            let count = if native {
                let id = object::function_code(self.heap, handle).unwrap_or(0);
                crate::realm::native::arity(id)
            } else {
                let code = object::function_code(self.heap, handle).unwrap_or(0);
                let module = object::function_module(self.heap, handle).unwrap_or(0);
                self.unit_of(module)
                    .function(code)
                    .map_or(0, |record| record.argument_count)
            };
            Value::number(crate::softfloat::from_u64(u64::from(count)))
        } else {
            self.ascii_string(b"")?
        };
        object::define_own_property(
            self.heap,
            handle,
            key,
            object::Descriptor::data(value, object::attribute::CONFIGURABLE),
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(())
    }

    /// Give a bound function the `length` and `name` its target implies.
    ///
    /// The length is what is left of the target's parameters once the bound
    /// arguments are counted off, never below zero and never from a target
    /// whose `length` is not a number. The name is the target's, behind
    /// `bound `.
    fn name_bound_function(
        &mut self,
        bound: Handle,
        target: Value,
        bound_count: u32,
    ) -> Result<(), Completion> {
        let length_key = self.ascii_key(b"length")?;
        let declared = self.get_property(target, length_key)?;
        let remaining = if declared.is_number() {
            // `+Infinity` stays infinite however much is bound; anything else
            // is the declared count less the bound arguments, floored at zero.
            let count = crate::value::to_integer_or_infinity(declared.as_number());
            if count == f64::INFINITY {
                count
            } else {
                let bound_arguments = crate::softfloat::from_u64(u64::from(bound_count));
                let left = crate::softfloat::sub(count, bound_arguments);
                if crate::softfloat::compare(left, 0.0) > 0 {
                    left
                } else {
                    0.0
                }
            }
        } else {
            0.0
        };
        object::define_own_property(
            self.heap,
            bound,
            length_key,
            object::Descriptor::data(Value::number(remaining), object::attribute::CONFIGURABLE),
        )
        .map_err(|_| self.heap_failure())?;

        let name_key = self.ascii_key(b"name")?;
        let declared = self.get_property(target, name_key)?;
        let prefix = self.ascii_string(b"bound ")?;
        let name = if declared.is_string() {
            let joined = string::concat(self.heap, prefix.as_handle(), declared.as_handle())
                .map_err(|_| self.heap_failure())?;
            Value::string(joined)
        } else {
            prefix
        };
        object::define_own_property(
            self.heap,
            bound,
            name_key,
            object::Descriptor::data(name, object::attribute::CONFIGURABLE),
        )
        .map_err(|_| self.heap_failure())?;
        Ok(())
    }

    /// Read a property from a primitive's prototype, with the primitive as the
    /// receiver, so a method sees the value it was called on rather than a
    /// wrapper made for the occasion.
    fn prototype_property(
        &mut self,
        prototype: Handle,
        receiver: Value,
        key: Key,
    ) -> Result<Value, Completion> {
        let lookup = object::get(self.heap, prototype, key)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        match lookup {
            Lookup::Absent => Ok(Value::UNDEFINED),
            Lookup::Value(value) => Ok(value),
            Lookup::Accessor(getter) => {
                if getter.is_undefined() {
                    return Ok(Value::UNDEFINED);
                }
                self.call_value(getter, receiver, &[])
            }
        }
    }

    /// A string's own properties: its length and its indexed code units.
    fn string_property(&mut self, target: Value, key: Key) -> Result<Option<Value>, Completion> {
        let handle = target.as_handle();
        let length = string::length(self.heap, handle)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        match key {
            Key::Index(index) => {
                let unit = string::unit_at(self.heap, handle, index)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                match unit {
                    Some(unit) => Ok(Some(self.make_string(&[unit])?)),
                    None => Ok(None),
                }
            }
            Key::Name(name) => {
                let expected = self.ascii_key(b"length")?;
                if Key::Name(name) == expected {
                    return Ok(Some(Value::number(crate::softfloat::from_u64(u64::from(
                        length,
                    )))));
                }
                Ok(None)
            }
            Key::Symbol(_) => Ok(None),
        }
    }

    /// Store through `object::set`, collecting and retrying when the heap is
    /// full: a table that outgrew its copies leaves them as garbage, and a
    /// failure counts only when the live data truly does not fit.
    fn set_with_room(
        &mut self,
        target: Value,
        key: Key,
        value: Value,
    ) -> Result<Assignment, Completion> {
        match object::set(self.heap, target.as_handle(), key, value) {
            Ok(outcome) => Ok(outcome),
            Err(object::ObjectError::Heap(
                crate::heap::HeapError::ArenaFull | crate::heap::HeapError::SlotsFull,
            )) => {
                if let Some(stop) = self.collect_now() {
                    return Err(stop);
                }
                object::set(self.heap, target.as_handle(), key, value).map_err(Self::object_failure)
            }
            Err(error) => Err(Self::object_failure(error)),
        }
    }

    fn set_property(&mut self, target: Value, key: Key, value: Value) -> Result<(), Completion> {
        self.set_property_of(target, key, value, false)
    }

    /// Assign a property, with strict code's refusals: a write a receiver
    /// refuses — non-writable, setter-less, non-extensible, or a primitive —
    /// is a TypeError under strict code and silence outside it.
    fn set_property_of(
        &mut self,
        target: Value,
        key: Key,
        value: Value,
        strict: bool,
    ) -> Result<(), Completion> {
        if target.is_nullish() {
            return Err(self.throw_type_error());
        }
        // A mapped arguments index writes its parameter's slot.
        if let (Key::Index(index), true) = (key, target.is_object()) {
            if let Ok(Some((environment, mapped))) =
                object::arguments_map(self.heap, target.as_handle())
            {
                if index < 32 && mapped & (1 << index) != 0 && environment.is_object() {
                    let _ = env::set_slot(self.heap, environment.as_handle(), index, value);
                    return Ok(());
                }
            }
        }
        if !target.is_object() {
            // A write to a primitive walks its prototype chain first: a
            // setter or a proxy there sees the write, with the primitive as
            // receiver; anything else discards it outside strict mode.
            if self.primitive_write(target, key, value)? {
                return Ok(());
            }
            if strict {
                return Err(self.throw_type_error());
            }
            return Ok(());
        }
        match object::exotic_kind(self.heap, target.as_handle()).unwrap_or(0) {
            object::exotic::PROXY if !self.hidden_key(key) => {
                let done = self.proxy_set(target, key, value)?;
                if !done && strict {
                    return Err(self.throw_type_error());
                }
                return Ok(());
            }
            object::exotic::NAMESPACE | object::exotic::DEFERRED => {
                // A namespace takes no write; looking first surfaces the
                // ReferenceError a binding still in its dead zone throws.
                // A write is no meaningful use of a deferred namespace, so
                // an unevaluated module stays that way.
                if self.deferred_done(target) {
                    self.namespace_touch(target, key)?;
                }
                if strict {
                    return Err(self.throw_type_error());
                }
                return Ok(());
            }
            object::exotic::TYPED_ARRAY => {
                if let Key::Index(index) = key {
                    return self.typed_array_write(target, index, value);
                }
            }
            _ => {}
        }
        // A typed array on the chain swallows a write under a numeric name
        // that is not one of its indices: TypedArray [[Set]] answers true
        // and stores nothing, so nothing lands on the receiver either.
        if self.is_canonical_numeric_key(key)? {
            let mut holder = object::prototype(self.heap, target.as_handle())
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            let mut depth = 0u32;
            while holder.is_object() && depth < object::MAX_PROTOTYPE_DEPTH {
                if object::exotic_kind(self.heap, holder.as_handle()).unwrap_or(0)
                    == object::exotic::TYPED_ARRAY
                {
                    return Ok(());
                }
                holder = object::prototype(self.heap, holder.as_handle())
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                depth += 1;
            }
        }
        // A function's `length` and `name` exist before a write can miss
        // them, so the write meets the read-only property they are.
        self.materialise_function_facts(target, key)?;
        // Assigning an array's `length` drops what is now past the end.
        if let Key::Name(_) = key {
            if self.is_array(target)? {
                let length_key = self.ascii_key(b"length")?;
                if key == length_key {
                    let old = self.length_of(target)?;
                    let wanted = self.coerce_to_number(value)?;
                    let new = value::to_uint32(wanted);
                    let outcome = self.set_with_room(target, key, value)?;
                    if matches!(outcome, Assignment::Done) {
                        let mut index = new;
                        while index < old {
                            self.delete_element(target, index)?;
                            index += 1;
                        }
                    }
                    return Ok(());
                }
            }
        }
        let outcome = self.set_with_room(target, key, value)?;
        match outcome {
            Assignment::Done | Assignment::Refused => {
                if strict && matches!(outcome, Assignment::Refused) {
                    return Err(self.throw_type_error());
                }
                // An array's `length` follows its highest index: a store past
                // the end grows it.
                if let Key::Index(index) = key {
                    if matches!(outcome, Assignment::Done) && self.is_array(target)? {
                        let length = self.length_of(target)?;
                        if index >= length && index < u32::MAX {
                            self.set_length(target, index + 1)?;
                        }
                    }
                }
                Ok(())
            }
            Assignment::Setter(setter) => {
                let _ = self.call_value(Value::object(setter), target, &[value])?;
                Ok(())
            }
        }
    }

    /// Define one half of an accessor, keeping the other half an existing
    /// accessor already carries.
    fn define_accessor(
        &mut self,
        target: Value,
        key: Key,
        closure: Value,
        getter: bool,
        attributes: u8,
    ) -> Result<(), Completion> {
        if !target.is_object() {
            return Err(self.throw_type_error());
        }
        let handle = target.as_handle();
        let existing =
            object::get_own_property(self.heap, handle, key).map_err(|_| self.heap_failure())?;
        let (mut get, mut set) = match existing {
            Some(found) if matches!(found.kind, object::DescriptorKind::Accessor) => {
                (found.getter, found.setter)
            }
            _ => (Value::UNDEFINED, Value::UNDEFINED),
        };
        if getter {
            get = closure;
        } else {
            set = closure;
        }
        // The accessor's function is named for the key, behind `get ` or
        // `set `: what named evaluation gives an accessor.
        if closure.is_object() && !object::is_native(self.heap, closure.as_handle()).unwrap_or(true)
        {
            let prefix: &[u8] = if getter { b"get " } else { b"set " };
            let name = self.name_for_key(key, Some(prefix))?;
            let name_key = self.ascii_key(b"name")?;
            object::define_own_property(
                self.heap,
                closure.as_handle(),
                name_key,
                object::Descriptor::data(name, attribute::CONFIGURABLE),
            )
            .map_err(|_| self.heap_failure())?;
        }
        let admitted = object::define_own_property(
            self.heap,
            handle,
            key,
            object::Descriptor::accessor(get, set, attributes),
        )
        .map_err(|_| self.heap_failure())?;
        if !admitted {
            // An accessor over a property that refuses redefinition —
            // `static ['prototype']`, most of all — is a TypeError.
            return Err(self.throw_type_error());
        }
        Ok(())
    }

    /// Define an `accessor` field's face: a getter and a setter over the
    /// hidden name the field stores behind, `NUL acc ` and the key's text,
    /// which reflection never reports.
    fn define_auto_accessor(&mut self, target: Value, key: Key) -> Result<(), Completion> {
        if !target.is_object() {
            return Err(self.throw_type_error());
        }
        let name = self.key_to_value(key)?;
        if !name.is_string() {
            return Err(self.throw_type_error());
        }
        let prefix_units = [
            0u16,
            u16::from(b'a'),
            u16::from(b'c'),
            u16::from(b'c'),
            u16::from(b' '),
        ];
        let prefix = self
            .atoms
            .intern(self.heap, &prefix_units)
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        let backing =
            string::concat(self.heap, prefix, name.as_handle()).map_err(|_| self.heap_failure())?;
        let backing = Value::string(backing);
        let mut pair = [Value::UNDEFINED; 2];
        for (slot, id) in pair
            .iter_mut()
            .zip([native::ACCESSOR_GET, native::ACCESSOR_SET])
        {
            let function = object::create_native(
                self.heap,
                Value::object(self.realm.function_prototype),
                id,
                0,
            )
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
            object::set_bound_value(self.heap, function, backing)
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
            *slot = Value::object(function);
        }
        object::define_own_property(
            self.heap,
            target.as_handle(),
            key,
            object::Descriptor::accessor(pair[0], pair[1], attribute::CONFIGURABLE),
        )
        .map_err(|_| self.heap_failure())?;
        Ok(())
    }

    /// Name a closure for the key it is defined under, when it carries no
    /// name yet: named evaluation through a computed key or a method.
    fn name_closure_for(&mut self, closure: Value, key: Key) -> Result<(), Completion> {
        if !closure.is_object() {
            return Ok(());
        }
        let handle = closure.as_handle();
        if object::is_native(self.heap, handle).unwrap_or(true) {
            return Ok(());
        }
        let name_key = self.ascii_key(b"name")?;
        let flags = object::function_flags(self.heap, handle).unwrap_or(0);
        if flags & object::function_flag::NAMED != 0
            || object::get_own_property(self.heap, handle, name_key)
                .unwrap_or(None)
                .is_some()
        {
            return Ok(());
        }
        let name = self.name_for_key(key, None)?;
        object::define_own_property(
            self.heap,
            handle,
            name_key,
            object::Descriptor::data(name, attribute::CONFIGURABLE),
        )
        .map_err(|_| self.heap_failure())?;
        let _ = object::add_function_flags(self.heap, handle, object::function_flag::NAMED);
        Ok(())
    }

    /// The `name` a function takes from a property key: the key's text, a
    /// symbol as `[description]` — or nothing for one without — behind an
    /// optional prefix such as `get `.
    fn name_for_key(&mut self, key: Key, prefix: Option<&[u8]>) -> Result<Value, Completion> {
        let body = match key {
            Key::Symbol(handle) => {
                let length = string::length(self.heap, handle).unwrap_or(0);
                if length == 0 {
                    self.ascii_string(b"")?
                } else {
                    let open = self.ascii_string(b"[")?;
                    let close = self.ascii_string(b"]")?;
                    let inner = string::concat(self.heap, open.as_handle(), handle)
                        .map_err(|_| self.heap_failure())?;
                    let whole = string::concat(self.heap, inner, close.as_handle())
                        .map_err(|_| self.heap_failure())?;
                    Value::string(whole)
                }
            }
            _ => self.key_to_value(key)?,
        };
        let Some(prefix) = prefix else {
            return Ok(body);
        };
        let head = self.ascii_string(prefix)?;
        let whole = string::concat(self.heap, head.as_handle(), body.as_handle())
            .map_err(|_| self.heap_failure())?;
        Ok(Value::string(whole))
    }

    /// The value a mapped arguments index reports: its parameter's slot,
    /// where the object maps that index.
    fn mapped_argument(&mut self, target: Value, key: Key) -> Option<Value> {
        let Key::Index(index) = key else {
            return None;
        };
        if !target.is_object() {
            return None;
        }
        let (environment, mapped) = object::arguments_map(self.heap, target.as_handle()).ok()??;
        if index >= 32 || mapped & (1 << index) == 0 || !environment.is_object() {
            return None;
        }
        env::slot_value(self.heap, environment.as_handle(), index).ok()
    }

    fn define_property(&mut self, target: Value, key: Key, value: Value) -> Result<(), Completion> {
        if !target.is_object() {
            return Err(self.throw_type_error());
        }
        if object::exotic_kind(self.heap, target.as_handle()).unwrap_or(0) == object::exotic::PROXY
            && !self.hidden_key(key)
        {
            if !self.proxy_define(target, key, value, attribute::DEFAULT)? {
                return Err(self.throw_type_error());
            }
            return Ok(());
        }
        let descriptor = Descriptor::data(value, attribute::DEFAULT);
        match object::define_own_property(self.heap, target.as_handle(), key, descriptor) {
            Ok(true) => Ok(()),
            // CreateDataPropertyOrThrow: a property that cannot take the
            // definition — `prototype` on a class constructor, most of all —
            // is a TypeError.
            Ok(false) => Err(self.throw_type_error()),
            Err(object::ObjectError::Heap(
                crate::heap::HeapError::ArenaFull | crate::heap::HeapError::SlotsFull,
            )) => {
                // The arena may hold reclaimable garbage the pressure check
                // did not see; a failure counts only after a collection.
                if let Some(stop) = self.collect_now() {
                    return Err(stop);
                }
                object::define_own_property(self.heap, target.as_handle(), key, descriptor)
                    .map(|_| ())
                    .map_err(Self::object_failure)
            }
            Err(error) => Err(Self::object_failure(error)),
        }
    }

    fn delete_property(&mut self, target: Value, key: Key) -> Result<bool, Completion> {
        // A reference through nothing is a type error, exactly as a read
        // through it is; a primitive base holds nothing deletable and
        // answers true.
        if target.is_nullish() {
            return Err(self.throw_type_error());
        }
        if !target.is_object() {
            return Ok(true);
        }
        self.materialise_function_facts(target, key)?;
        if object::exotic_kind(self.heap, target.as_handle()).unwrap_or(0) == object::exotic::PROXY
            && !self.hidden_key(key)
        {
            return self.proxy_delete(target, key);
        }
        if !self.hidden_key(key) {
            self.deferred_trigger(target, Some(key))?;
        }
        let removed =
            object::delete(self.heap, target.as_handle(), key).map_err(Self::object_failure)?;
        // Deleting a mapped arguments index breaks the aliasing for good —
        // but only a delete that succeeded: a non-configurable index keeps
        // its mapping.
        if removed {
            if let Key::Index(index) = key {
                let _ = object::unmap_argument(self.heap, target.as_handle(), index);
            }
        }
        Ok(removed)
    }

    fn copy_data_properties(&mut self, target: Value, source: Value) -> Result<(), Completion> {
        self.copy_data_properties_excluding(target, source, Value::UNDEFINED)
    }

    /// CopyDataProperties: the source's own enumerable properties, in own-key
    /// order, defined on the target — except the keys `excluded` holds as
    /// own properties, which are never asked about on the source. A proxy
    /// source answers through its `ownKeys`, `getOwnPropertyDescriptor`,
    /// and `get` traps, in that order for each key.
    fn copy_data_properties_excluding(
        &mut self,
        target: Value,
        source: Value,
        excluded: Value,
    ) -> Result<(), Completion> {
        if !target.is_object() {
            return Ok(());
        }
        // A string's own enumerable properties are its indexed characters:
        // what ToObject would expose, without making the wrapper.
        if source.is_string() {
            let length = crate::string::length(self.heap, source.as_handle()).unwrap_or(0);
            let mut index = 0u32;
            while index < length {
                let key = Key::Index(index);
                index += 1;
                if self.is_excluded(excluded, key)? {
                    continue;
                }
                let value = self.get_property(source, key)?;
                self.define_property(target, key, value)?;
            }
            return Ok(());
        }
        if !source.is_object() {
            return Ok(());
        }
        let proxy = object::exotic_kind(self.heap, source.as_handle()).unwrap_or(0)
            == object::exotic::PROXY;
        let mut keys = [Key::Index(0); MAX_COPIED_KEYS];
        let written = if proxy {
            self.proxy_own_keys(source, &mut keys)?
        } else {
            object::own_keys(self.heap, source.as_handle(), &mut keys).map_err(Self::key_failure)?
        };
        let mut index = 0usize;
        while index < written {
            let key = keys[index];
            index += 1;
            if self.is_excluded(excluded, key)? {
                continue;
            }
            // Only the enumerable own properties cross: a spread copies what
            // enumeration would see.
            let enumerable = if proxy {
                self.proxy_own_enumerable(source, key)?
            } else {
                self.is_enumerable(source, key)?
            };
            if !enumerable {
                continue;
            }
            let value = self.get_property(source, key)?;
            self.define_property(target, key, value)?;
        }
        Ok(())
    }

    /// OrdinarySet with a receiver of its own: the property is found on the
    /// target's chain, and a data property lands on the receiver — through
    /// its own record, or its traps where it is a proxy. Answers whether
    /// the write was taken.
    fn set_with_receiver(
        &mut self,
        target: Value,
        key: Key,
        value: Value,
        receiver: Value,
    ) -> Result<bool, Completion> {
        let mut holder = target;
        let found = loop {
            if !holder.is_object() {
                break None;
            }
            if object::exotic_kind(self.heap, holder.as_handle()).unwrap_or(0)
                == object::exotic::PROXY
            {
                let (proxy_target, handler) = self.proxy_parts(holder)?;
                let trap = self.proxy_trap(handler, b"set")?;
                if trap.is_undefined() {
                    holder = proxy_target;
                    continue;
                }
                let name = self.key_to_value(key)?;
                let answer =
                    self.call_value(trap, handler, &[proxy_target, name, value, receiver])?;
                return self.coerce_to_boolean(answer);
            }
            let own = object::get_own_property(self.heap, holder.as_handle(), key)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            if own.is_some() {
                break own;
            }
            holder = object::prototype(self.heap, holder.as_handle())
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        };
        if let Some(descriptor) = found {
            if matches!(descriptor.kind, object::DescriptorKind::Accessor) {
                if !self.is_callable_value(descriptor.setter) {
                    return Ok(false);
                }
                self.call_value(descriptor.setter, receiver, &[value])?;
                return Ok(true);
            }
            if !descriptor.has(attribute::WRITABLE) {
                return Ok(false);
            }
        }
        if !receiver.is_object() {
            return Ok(false);
        }
        if object::exotic_kind(self.heap, receiver.as_handle()).unwrap_or(0)
            == object::exotic::PROXY
        {
            let existing = self.proxy_own_descriptor(receiver, key)?;
            let attributes = if existing.is_undefined() {
                attribute::DEFAULT
            } else {
                let mut attributes = 0u8;
                for (name, bit) in [
                    (&b"writable"[..], attribute::WRITABLE),
                    (&b"enumerable"[..], attribute::ENUMERABLE),
                    (&b"configurable"[..], attribute::CONFIGURABLE),
                ] {
                    let field = self.ascii_key(name)?;
                    let flag = self.get_property(existing, field)?;
                    if self.coerce_to_boolean(flag)? {
                        attributes |= bit;
                    }
                }
                let has_accessor = {
                    let get_key = self.ascii_key(b"get")?;
                    let set_key = self.ascii_key(b"set")?;
                    self.has_property_of(existing, get_key)?
                        || self.has_property_of(existing, set_key)?
                };
                if has_accessor || attributes & attribute::WRITABLE == 0 {
                    return Ok(false);
                }
                attributes
            };
            return self.proxy_define(receiver, key, value, attributes);
        }
        // Defining on a deferred namespace is a meaningful use of it — the
        // definition itself is refused, but the module runs.
        if object::exotic_kind(self.heap, receiver.as_handle()).unwrap_or(0)
            == object::exotic::DEFERRED
            && !self.hidden_key(key)
        {
            self.deferred_trigger(receiver, Some(key))?;
        }
        let existing = object::get_own_property(self.heap, receiver.as_handle(), key)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        let attributes = match existing {
            Some(descriptor) => {
                if matches!(descriptor.kind, object::DescriptorKind::Accessor)
                    || !descriptor.has(attribute::WRITABLE)
                {
                    return Ok(false);
                }
                descriptor.attributes
            }
            None => {
                // A new property needs an extensible receiver.
                let extensible = object::is_extensible(self.heap, receiver.as_handle())
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                if !extensible {
                    return Ok(false);
                }
                attribute::DEFAULT
            }
        };
        object::define_own_property(
            self.heap,
            receiver.as_handle(),
            key,
            Descriptor::data(value, attributes),
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(true)
    }

    /// A proxy's own property descriptor for a key as the trap reports it:
    /// the descriptor object, or undefined.
    fn proxy_own_descriptor(&mut self, proxy: Value, key: Key) -> Result<Value, Completion> {
        let (target, handler) = self.proxy_parts(proxy)?;
        let trap = self.proxy_trap(handler, b"getOwnPropertyDescriptor")?;
        if trap.is_undefined() {
            let own = object::get_own_property(self.heap, target.as_handle(), key)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            return match own {
                Some(descriptor) => self.descriptor_object(descriptor),
                None => Ok(Value::UNDEFINED),
            };
        }
        let name = self.key_to_value(key)?;
        let descriptor = self.call_value(trap, handler, &[target, name])?;
        if !descriptor.is_undefined() && !descriptor.is_object() {
            return Err(self.throw_type_error());
        }
        Ok(descriptor)
    }

    /// A property descriptor as the object reflection hands out.
    fn descriptor_object(&mut self, found: Descriptor) -> Result<Value, Completion> {
        let result = object::create(self.heap, Value::object(self.realm.object_prototype))
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        let result = Value::object(result);
        let fields: [(&[u8], Value); 4] = if matches!(found.kind, object::DescriptorKind::Data) {
            [
                (b"value", found.value),
                (b"writable", Value::boolean(found.has(attribute::WRITABLE))),
                (
                    b"enumerable",
                    Value::boolean(found.has(attribute::ENUMERABLE)),
                ),
                (
                    b"configurable",
                    Value::boolean(found.has(attribute::CONFIGURABLE)),
                ),
            ]
        } else {
            [
                (b"get", found.getter),
                (b"set", found.setter),
                (
                    b"enumerable",
                    Value::boolean(found.has(attribute::ENUMERABLE)),
                ),
                (
                    b"configurable",
                    Value::boolean(found.has(attribute::CONFIGURABLE)),
                ),
            ]
        };
        for (name, value) in fields {
            let key = self.ascii_key(name)?;
            self.define_property(result, key, value)?;
        }
        Ok(result)
    }

    /// Whether a key is one the exclusion object names.
    fn is_excluded(&mut self, excluded: Value, key: Key) -> Result<bool, Completion> {
        if !excluded.is_object() {
            return Ok(false);
        }
        let held = object::get_own_property(self.heap, excluded.as_handle(), key)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        Ok(held.is_some())
    }

    // Arrays.

    fn create_array(&mut self) -> Result<Value, Completion> {
        let array = object::create(self.heap, Value::object(self.realm.array_prototype))
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        let length_key = self.ascii_key(b"length")?;
        object::define_own_property(
            self.heap,
            array,
            length_key,
            Descriptor::data(Value::number(0.0), attribute::WRITABLE),
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(Value::object(array))
    }

    fn append_element(&mut self, array: Value, value: Option<Value>) -> Result<(), Completion> {
        if !array.is_object() {
            return Err(self.throw_type_error());
        }
        let length_key = self.ascii_key(b"length")?;
        let length = self.get_property(array, length_key)?;
        let index = value::to_uint32(length.as_number());
        if let Some(value) = value {
            self.define_property(array, Key::Index(index), value)?;
        }
        let next = Value::number(crate::softfloat::from_u64(u64::from(index) + 1));
        object::define_own_property(
            self.heap,
            array.as_handle(),
            length_key,
            Descriptor::data(next, attribute::WRITABLE),
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(())
    }

    // Environments.

    fn context_slot(&mut self, frame: &Frame, index: u32, depth: u32) -> Result<Value, Completion> {
        let mut environment = frame.environment;
        let mut remaining = depth;
        while remaining > 0 {
            if !environment.is_object() {
                return Err(self.throw_reference_error());
            }
            environment = env::parent(self.heap, environment.as_handle())
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            remaining -= 1;
        }
        if !environment.is_object() {
            return Err(self.throw_reference_error());
        }
        match env::slot_value(self.heap, environment.as_handle(), index) {
            Ok(value) => Ok(value),
            Err(env::EnvironmentError::Uninitialised) => Err(self.throw_reference_error()),
            Err(_) => Err(Completion::Terminated(Termination::Malformed)),
        }
    }

    /// Count a context the running frame entered or left.
    fn adjust_contexts(&mut self, change: i32) {
        if let Some(frame) = self.frames.get_mut(self.depth.saturating_sub(1) as usize) {
            frame.contexts = if change >= 0 {
                frame.contexts.saturating_add(1)
            } else {
                frame.contexts.saturating_sub(1)
            };
        }
    }

    /// Replace the running frame's environment, which is what entering and
    /// leaving a block scope does.
    fn set_frame_environment(&mut self, environment: Value) {
        if let Some(frame) = self.frames.get_mut(self.depth.saturating_sub(1) as usize) {
            frame.environment = environment;
        }
    }

    /// The `this` of the nearest enclosing function environment.
    /// Make a template array immutable: every element loses write and
    /// reshape, and the array takes no more.
    fn freeze_template(&mut self, array: Value) -> Result<(), Completion> {
        if !array.is_object() {
            return Ok(());
        }
        let count = self.length_of(array)?;
        let mut index = 0u32;
        while index < count {
            let value = self.element(array, index)?;
            object::define_own_property(
                self.heap,
                array.as_handle(),
                Key::Index(index),
                Descriptor::data(value, attribute::ENUMERABLE),
            )
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
            index += 1;
        }
        let length_key = self.ascii_key(b"length")?;
        let length = Value::number(crate::softfloat::from_u64(u64::from(count)));
        object::define_own_property(
            self.heap,
            array.as_handle(),
            length_key,
            Descriptor::data(length, 0),
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        let _ = object::prevent_extensions(self.heap, array.as_handle());
        Ok(())
    }

    /// Read `key` starting at the super base, running any getter on the
    /// frame's own `this` rather than on the base.
    fn super_get(&mut self, base: Value, key: Key, receiver: Value) -> Result<Value, Completion> {
        let mut holder = base;
        let mut depth = 0u32;
        while holder.is_object() && depth <= object::MAX_PROTOTYPE_DEPTH {
            if self.deferred_live
                && !self.hidden_key(key)
                && object::exotic_kind(self.heap, holder.as_handle()).unwrap_or(0)
                    == object::exotic::DEFERRED
            {
                self.deferred_trigger(holder, Some(key))?;
            }
            let found = object::get_own_property(self.heap, holder.as_handle(), key)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            if let Some(descriptor) = found {
                return match descriptor.kind {
                    object::DescriptorKind::Data => Ok(descriptor.value),
                    object::DescriptorKind::Accessor => {
                        if self.is_callable_value(descriptor.getter) {
                            self.call_value(descriptor.getter, receiver, &[])
                        } else {
                            Ok(Value::UNDEFINED)
                        }
                    }
                };
            }
            holder = object::prototype(self.heap, holder.as_handle())
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            depth += 1;
        }
        Ok(Value::UNDEFINED)
    }

    /// Write through a super reference: the base chain decides — a setter
    /// runs with the receiver, a read-only property refuses — and otherwise
    /// the value lands as the receiver's own data property.
    fn super_set(
        &mut self,
        base: Value,
        key: Key,
        value: Value,
        receiver: Value,
        strict: bool,
    ) -> Result<(), Completion> {
        let mut holder = base;
        let mut depth = 0u32;
        let mut refused = false;
        while holder.is_object() && depth <= object::MAX_PROTOTYPE_DEPTH {
            let found = object::get_own_property(self.heap, holder.as_handle(), key)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            if let Some(descriptor) = found {
                match descriptor.kind {
                    object::DescriptorKind::Accessor => {
                        if self.is_callable_value(descriptor.setter) {
                            self.call_value(descriptor.setter, receiver, &[value])?;
                            return Ok(());
                        }
                        refused = true;
                    }
                    object::DescriptorKind::Data => {
                        refused = !descriptor.has(attribute::WRITABLE);
                    }
                }
                break;
            }
            holder = object::prototype(self.heap, holder.as_handle())
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            depth += 1;
        }
        if !refused {
            // A namespace receiver reads the binding first, so a name still
            // in its dead zone throws before the write is refused.
            self.namespace_touch(receiver, key)?;
            if receiver.is_object() {
                let own = object::get_own_property(self.heap, receiver.as_handle(), key)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                let admitted = match own {
                    Some(existing) => {
                        if matches!(existing.kind, object::DescriptorKind::Accessor)
                            || !existing.has(attribute::WRITABLE)
                        {
                            false
                        } else {
                            object::define_own_property(
                                self.heap,
                                receiver.as_handle(),
                                key,
                                Descriptor::data(value, existing.attributes),
                            )
                            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?
                        }
                    }
                    None => object::define_own_property(
                        self.heap,
                        receiver.as_handle(),
                        key,
                        Descriptor::data(value, attribute::DEFAULT),
                    )
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?,
                };
                if admitted {
                    return Ok(());
                }
            }
            refused = true;
        }
        if refused && strict {
            return Err(self.throw_type_error());
        }
        Ok(())
    }

    /// Point the frame's function environment at a replacement `this` — a
    /// `super()` whose parent overrode its return.
    fn rebind_environment_this(
        &mut self,
        environment: Value,
        this: Value,
    ) -> Result<(), Completion> {
        let mut current = environment;
        let mut depth = 0u32;
        while current.is_object() && depth <= env::MAX_SCOPE_DEPTH {
            let handle = current.as_handle();
            if env::kind(self.heap, handle) == Ok(EnvironmentKind::Function) {
                env::set_this(self.heap, handle, this)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                return Ok(());
            }
            let Ok(parent) = env::parent(self.heap, handle) else {
                break;
            };
            current = parent;
            depth += 1;
        }
        Ok(())
    }

    fn this_value(&mut self, frame: &Frame) -> Result<Value, Completion> {
        if frame.this_pending {
            // A derived constructor's `this` waits for `super()`.
            return Err(self.throw_reference_error());
        }
        let mut environment = frame.environment;
        let mut depth = 0u32;
        while environment.is_object() && depth <= env::MAX_SCOPE_DEPTH {
            let handle = environment.as_handle();
            if env::kind(self.heap, handle) == Ok(EnvironmentKind::Function) {
                if env::this_uninitialised(self.heap, handle).unwrap_or(false) {
                    return Err(self.throw_reference_error());
                }
                return env::this_value(self.heap, handle)
                    .map_err(|_| Completion::Terminated(Termination::Malformed));
            }
            environment = env::parent(self.heap, handle)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            depth += 1;
        }
        // Outside any function, `this` is what the host started the task
        // with: the entry frame's receiver, not the current frame's, which a
        // top-level arrow called through `call` would otherwise read.
        Ok(self.frames.first().map_or(frame.this, |entry| entry.this))
    }

    fn init_context_slot(
        &mut self,
        frame: &Frame,
        index: u32,
        depth: u32,
        value: Value,
    ) -> Result<(), Completion> {
        let mut environment = frame.environment;
        let mut remaining = depth;
        while remaining > 0 {
            if !environment.is_object() {
                return Err(self.throw_reference_error());
            }
            environment = env::parent(self.heap, environment.as_handle())
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            remaining -= 1;
        }
        if !environment.is_object() {
            return Err(self.throw_reference_error());
        }
        env::initialise(self.heap, environment.as_handle(), index, value)
            .map_err(|_| Completion::Terminated(Termination::Malformed))
    }

    fn set_context_slot(
        &mut self,
        frame: &Frame,
        index: u32,
        depth: u32,
        value: Value,
    ) -> Result<(), Completion> {
        let mut environment = frame.environment;
        let mut remaining = depth;
        while remaining > 0 {
            if !environment.is_object() {
                return Err(self.throw_reference_error());
            }
            environment = env::parent(self.heap, environment.as_handle())
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            remaining -= 1;
        }
        if !environment.is_object() {
            return Err(self.throw_reference_error());
        }
        match env::set_slot(self.heap, environment.as_handle(), index, value) {
            Ok(()) => Ok(()),
            Err(env::EnvironmentError::Uninitialised) => Err(self.throw_reference_error()),
            Err(env::EnvironmentError::Immutable) => Err(self.throw_type_error()),
            Err(_) => Err(Completion::Terminated(Termination::Malformed)),
        }
    }

    // The library the engine implements itself. Everything here is a pure
    // function of its arguments and the heap: nothing reaches outside the
    // isolate, which is why there is no `Math.random` and no clock.

    /// The length an array-like says it has.
    fn length_of(&mut self, target: Value) -> Result<u32, Completion> {
        let key = self.ascii_key(b"length")?;
        let value = self.get_property(target, key)?;
        let number = self.coerce_to_number(value)?;
        Ok(value::to_uint32(number))
    }

    fn set_length(&mut self, target: Value, length: u32) -> Result<(), Completion> {
        let key = self.ascii_key(b"length")?;
        let value = Value::number(crate::softfloat::from_u64(u64::from(length)));
        self.set_property(target, key, value)
    }

    fn element(&mut self, target: Value, index: u32) -> Result<Value, Completion> {
        self.get_property(target, Key::Index(index))
    }

    fn set_element(&mut self, target: Value, index: u32, value: Value) -> Result<(), Completion> {
        self.define_property(target, Key::Index(index), value)
    }

    /// A new array holding nothing.
    fn new_array(&mut self) -> Result<Value, Completion> {
        self.create_array()
    }

    /// The integer an argument denotes, clamped into `[0, length]`, with a
    /// negative value counted back from the end. This is what every method that
    /// takes a range does with its arguments.
    fn relative_index(
        &mut self,
        value: Value,
        length: u32,
        default: u32,
    ) -> Result<u32, Completion> {
        if value.is_undefined() {
            return Ok(default);
        }
        let number = self.coerce_to_number(value)?;
        if number.is_nan() {
            return Ok(0);
        }
        let length = f64::from(length);
        let index = value::truncate(number);
        let index = if index < 0.0 { length + index } else { index };
        let index = if index < 0.0 {
            0.0
        } else if index > length {
            length
        } else {
            index
        };
        Ok(value::to_uint32(index))
    }

    /// The string a value is, as a handle, for the methods that work on one.
    fn string_handle(&mut self, value: Value) -> Result<Handle, Completion> {
        let text = self.coerce_to_string(value)?;
        Ok(text.as_handle())
    }

    /// The receiver a method on a primitive was called with, unwrapped where it
    /// is an object built around one.
    /// The primitive `this` a wrapper method demands, of exactly `tag`:
    /// anything else is the TypeError the specification's thisValue steps
    /// throw — which is also what keeps `toString` on a plain object from
    /// coercing itself forever.
    fn this_primitive_of(&mut self, this: Value, tag: Tag) -> Result<Value, Completion> {
        let value = self.primitive_this(this)?;
        if value.tag() as u8 == tag as u8 {
            Ok(value)
        } else {
            Err(self.throw_type_error())
        }
    }

    fn primitive_this(&mut self, this: Value) -> Result<Value, Completion> {
        if !this.is_object() {
            return Ok(this);
        }
        match object::wrapper_value(self.heap, this.as_handle()) {
            Ok(Some(value)) => Ok(value),
            _ => Ok(this),
        }
    }

    fn heap_failure(&self) -> Completion {
        Completion::Terminated(Termination::HeapExhausted)
    }

    /// What a failed object operation means: exhausted storage is a heap
    /// outcome, an overfull bound is a quota, and only a reference that names
    /// nothing live is a malformed image.
    const fn object_failure(error: object::ObjectError) -> Completion {
        match error {
            object::ObjectError::Heap(
                crate::heap::HeapError::ArenaFull | crate::heap::HeapError::SlotsFull,
            ) => Completion::Terminated(Termination::HeapExhausted),
            object::ObjectError::TooManyKeys | object::ObjectError::PrototypeChainTooDeep => {
                Completion::Terminated(Termination::QuotaExceeded)
            }
            _ => Completion::Terminated(Termination::Malformed),
        }
    }

    /// What a failed key listing means: storage that was too small is a quota,
    /// not an exhausted heap, and either way it is an outcome rather than a
    /// shorter list than the object actually has.
    const fn key_failure(error: object::ObjectError) -> Completion {
        match error {
            object::ObjectError::TooManyKeys => Completion::Terminated(Termination::QuotaExceeded),
            _ => Completion::Terminated(Termination::HeapExhausted),
        }
    }

    /// Call `function` with `this` and up to four arguments.
    fn call_with(
        &mut self,
        function: Value,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        self.call_value(function, this, arguments)
    }

    /// Whether an own property is enumerable, which is what the key lists ask.
    fn is_enumerable(&mut self, object: Value, key: Key) -> Result<bool, Completion> {
        if !object.is_object() {
            return Ok(false);
        }
        let descriptor = object::get_own_property(self.heap, object.as_handle(), key)
            .map_err(|_| self.heap_failure())?;
        Ok(descriptor.is_some_and(|descriptor| descriptor.has(attribute::ENUMERABLE)))
    }

    /// The own enumerable keys, values, or key/value pairs of an object.
    fn own_entries(&mut self, target: Value, kind: u32) -> Result<Value, Completion> {
        let object = self.coerce_to_object(target)?;
        // A function's `length` and `name` are own properties whether or
        // not anything has read them yet.
        let length_key = self.ascii_key(b"length")?;
        let name_key = self.ascii_key(b"name")?;
        self.materialise_function_facts(object, length_key)?;
        self.materialise_function_facts(object, name_key)?;
        let result = self.new_array()?;
        // A typed array's own keys begin with its indices.
        if object::exotic_kind(self.heap, object.as_handle()).unwrap_or(0)
            == object::exotic::TYPED_ARRAY
            && kind != native::OBJECT_GET_OWN_PROPERTY_SYMBOLS
        {
            let count = self.typed_array_length(object)?.unwrap_or(0);
            let mut index = 0u32;
            while index < count {
                let name = self.key_to_value(Key::Index(index))?;
                let entry = match kind {
                    native::OBJECT_VALUES => self.typed_array_read(object, index)?,
                    native::OBJECT_ENTRIES => {
                        let pair = self.new_array()?;
                        let value = self.typed_array_read(object, index)?;
                        self.set_element(pair, 0, name)?;
                        self.set_element(pair, 1, value)?;
                        self.set_length(pair, 2)?;
                        pair
                    }
                    _ => name,
                };
                self.append_element(result, Some(entry))?;
                index += 1;
            }
        }
        // Listing a deferred namespace's keys is a meaningful use.
        self.deferred_trigger(object, None)?;
        let mut keys = [Key::Index(0); MAX_OWN_KEYS];
        let count = object::own_keys(self.heap, object.as_handle(), &mut keys)
            .map_err(Self::key_failure)?;
        let mut written = self.length_of(result)?;
        let symbols = kind == native::OBJECT_GET_OWN_PROPERTY_SYMBOLS;
        // Reflect.ownKeys answers every own key, names before symbols.
        let everything = kind == native::REFLECT_OWN_KEYS;
        let phases: u32 = if everything { 2 } else { 1 };
        let mut phase = 0u32;
        while phase < phases {
            for &key in keys.get(..count).unwrap_or(&[]) {
                let symbol = matches!(key, Key::Symbol(_));
                let wanted = if everything { phase == 1 } else { symbols };
                if symbol != wanted || self.hidden_key(key) {
                    continue;
                }
                // Listing keys reads no binding: a name still in its dead
                // zone lists fine, and only reading its value throws.
                if kind != native::OBJECT_GET_OWN_PROPERTY_NAMES
                    && kind != native::OBJECT_GET_OWN_PROPERTY_SYMBOLS
                    && kind != native::REFLECT_OWN_KEYS
                {
                    self.namespace_touch(object, key)?;
                }
                if !symbols
                    && !everything
                    && kind != native::OBJECT_GET_OWN_PROPERTY_NAMES
                    && !self.is_enumerable(object, key)?
                {
                    continue;
                }
                let name = self.key_to_value(key)?;
                let entry = match kind {
                    native::OBJECT_KEYS
                    | native::OBJECT_GET_OWN_PROPERTY_NAMES
                    | native::OBJECT_GET_OWN_PROPERTY_SYMBOLS
                    | native::REFLECT_OWN_KEYS => name,
                    native::OBJECT_VALUES => self.get_property(object, key)?,
                    _ => {
                        let pair = self.new_array()?;
                        let value = self.get_property(object, key)?;
                        self.set_element(pair, 0, name)?;
                        self.set_element(pair, 1, value)?;
                        self.set_length(pair, 2)?;
                        pair
                    }
                };
                self.set_element(result, written, entry)?;
                written += 1;
            }
            phase += 1;
        }
        self.set_length(result, written)?;
        Ok(result)
    }

    /// A property key as the value a program sees.
    fn key_to_value(&mut self, key: Key) -> Result<Value, Completion> {
        match key {
            Key::Index(index) => {
                let number = Value::number(crate::softfloat::from_u64(u64::from(index)));
                self.coerce_to_string(number)
            }
            Key::Name(handle) => Ok(Value::string(handle)),
            Key::Symbol(handle) => Ok(Value::symbol(handle)),
        }
    }

    /// An object for a value: itself where it is one, and a wrapper around it
    /// where it is a primitive.
    fn coerce_to_object(&mut self, value: Value) -> Result<Value, Completion> {
        if value.is_object() {
            return Ok(value);
        }
        let prototype = match value.tag() {
            Tag::String => self.realm.string_prototype,
            Tag::Number => self.realm.number_prototype,
            Tag::Boolean => self.realm.boolean_prototype,
            Tag::Symbol => self.realm.symbol_prototype,
            Tag::BigInt => self.realm.big_int_prototype,
            _ => return Err(self.throw_type_error()),
        };
        let handle = object::create_wrapper(self.heap, Value::object(prototype), value)
            .map_err(|_| self.heap_failure())?;
        if matches!(value.tag(), Tag::String) {
            // A string wrapper carries the length its primitive has.
            let length =
                string::length(self.heap, value.as_handle()).map_err(|_| self.heap_failure())?;
            let key = self.ascii_key(b"length")?;
            object::define_own_property(
                self.heap,
                handle,
                key,
                Descriptor::data(
                    Value::number(crate::softfloat::from_u64(u64::from(length))),
                    0,
                ),
            )
            .map_err(|_| self.heap_failure())?;
        }
        Ok(Value::object(handle))
    }

    fn is_array(&mut self, value: Value) -> Result<bool, Completion> {
        if !value.is_object() {
            return Ok(false);
        }
        // Array.prototype anywhere up the chain: the array itself, or an
        // instance of a class extending Array.
        let mut current = value;
        let mut depth = 0u32;
        while current.is_object() && depth < 8 {
            let prototype = object::prototype(self.heap, current.as_handle())
                .map_err(|_| self.heap_failure())?;
            if prototype.is_object() && prototype.as_handle() == self.realm.array_prototype {
                return Ok(true);
            }
            current = prototype;
            depth += 1;
        }
        Ok(false)
    }

    fn delete_element(&mut self, target: Value, index: u32) -> Result<(), Completion> {
        if target.is_object() {
            object::delete(self.heap, target.as_handle(), Key::Index(index))
                .map_err(|_| self.heap_failure())?;
        }
        Ok(())
    }

    /// The array methods that call a function once per element.
    fn array_walk(
        &mut self,
        id: u32,
        this: Value,
        callback: Value,
        receiver: Value,
    ) -> Result<Value, Completion> {
        let length = self.length_of(this)?;
        let result = match id {
            native::ARRAY_MAP | native::ARRAY_FILTER => self.new_array()?,
            _ => Value::UNDEFINED,
        };
        let mut written = 0u32;
        let mut index = 0u32;
        while index < length {
            let value = self.element(this, index)?;
            let position = Value::number(crate::softfloat::from_u64(u64::from(index)));
            let outcome = self.call_with(callback, receiver, &[value, position, this])?;
            let truth = self.coerce_to_boolean(outcome)?;
            match id {
                native::ARRAY_MAP => {
                    self.set_element(result, index, outcome)?;
                    written = index + 1;
                }
                native::ARRAY_FILTER => {
                    if truth {
                        self.set_element(result, written, value)?;
                        written += 1;
                    }
                }
                native::ARRAY_SOME => {
                    if truth {
                        return Ok(Value::boolean(true));
                    }
                }
                native::ARRAY_EVERY => {
                    if !truth {
                        return Ok(Value::boolean(false));
                    }
                }
                native::ARRAY_FIND => {
                    if truth {
                        return Ok(value);
                    }
                }
                native::ARRAY_FIND_INDEX => {
                    if truth {
                        return Ok(position);
                    }
                }
                _ => {}
            }
            index += 1;
        }
        match id {
            native::ARRAY_MAP | native::ARRAY_FILTER => {
                self.set_length(result, written)?;
                Ok(result)
            }
            native::ARRAY_SOME => Ok(Value::boolean(false)),
            native::ARRAY_EVERY => Ok(Value::boolean(true)),
            native::ARRAY_FIND => Ok(Value::UNDEFINED),
            native::ARRAY_FIND_INDEX => Ok(Value::number(-1.0)),
            _ => Ok(Value::UNDEFINED),
        }
    }

    /// Sort an array in place, by a comparison function or by string order.
    ///
    /// The sort is an insertion sort: it is stable, it allocates nothing, and
    /// the arrays an isolate this size holds are small.
    fn sort_array(&mut self, this: Value, comparator: Value) -> Result<Value, Completion> {
        let length = self.length_of(this)?;
        let mut index = 1u32;
        while index < length {
            let value = self.element(this, index)?;
            let mut at = index;
            while at > 0 {
                let left = self.element(this, at - 1)?;
                let ordered = if comparator.is_undefined() {
                    let left_text = self.coerce_to_string(left)?;
                    let right_text = self.coerce_to_string(value)?;
                    string::compare(self.heap, left_text.as_handle(), right_text.as_handle())
                        .map_err(|_| self.heap_failure())?
                        != core::cmp::Ordering::Greater
                } else {
                    let outcome = self.call_with(comparator, Value::UNDEFINED, &[left, value])?;
                    let number = self.coerce_to_number(outcome)?;
                    // A comparison that answers NaN leaves the order alone,
                    // which is what treating it as "not greater" does.
                    matches!(
                        number.partial_cmp(&0.0),
                        Some(core::cmp::Ordering::Less | core::cmp::Ordering::Equal) | None
                    )
                };
                if ordered {
                    break;
                }
                self.set_element(this, at, left)?;
                at -= 1;
            }
            self.set_element(this, at, value)?;
            index += 1;
        }
        Ok(this)
    }

    /// An argument used as a position: an integer at or above zero.
    fn index_argument(&mut self, value: Value) -> Result<u32, Completion> {
        if value.is_undefined() {
            return Ok(0);
        }
        let number = self.coerce_to_number(value)?;
        if number.is_nan() || number < 0.0 {
            return Ok(0);
        }
        Ok(value::to_uint32(value::truncate(number)))
    }

    /// An argument used as a position inside a string, clamped to its length.
    fn clamped_index(
        &mut self,
        value: Value,
        length: u32,
        default: u32,
    ) -> Result<u32, Completion> {
        if value.is_undefined() {
            return Ok(default);
        }
        Ok(self.index_argument(value)?.min(length))
    }

    fn is_callable_value(&self, value: Value) -> bool {
        value.is_object() && object::is_callable(self.heap, value.as_handle()) == Ok(true)
    }

    /// `Symbol(description)` as text, which is the only way to see one.
    fn symbol_text(&mut self, value: Value) -> Result<Value, Completion> {
        if !matches!(value.tag(), Tag::Symbol) {
            return Err(self.throw_type_error());
        }
        let open = self.ascii_string(b"Symbol(")?;
        let close = self.ascii_string(b")")?;
        let body = string::concat(self.heap, open.as_handle(), value.as_handle())
            .map_err(|_| self.heap_failure())?;
        let text =
            string::concat(self.heap, body, close.as_handle()).map_err(|_| self.heap_failure())?;
        Ok(Value::string(text))
    }

    /// The enumerable string keys of a value and its prototypes, in order,
    /// with a name seen once however many times it appears in the chain.
    fn enumerable_keys(&mut self, value: Value) -> Result<Value, Completion> {
        let array = self.new_array()?;
        if value.is_nullish() {
            return Ok(array);
        }
        let object = self.coerce_to_object(value)?;
        let mut written = 0u32;
        // A typed array enumerates its indices first.
        if object::exotic_kind(self.heap, object.as_handle()).unwrap_or(0)
            == object::exotic::TYPED_ARRAY
        {
            let count = self.typed_array_length(object)?.unwrap_or(0);
            while written < count {
                let name = self.key_to_value(Key::Index(written))?;
                self.set_element(array, written, name)?;
                written += 1;
            }
        }
        let mut current = object;
        let mut depth = 0u32;
        while current.is_object() && depth < object::MAX_PROTOTYPE_DEPTH {
            let mut keys = [Key::Index(0); MAX_OWN_KEYS];
            let count = object::own_keys(self.heap, current.as_handle(), &mut keys)
                .map_err(Self::key_failure)?;
            for &key in keys.get(..count).unwrap_or(&[]) {
                if matches!(key, Key::Symbol(_)) {
                    continue;
                }
                self.namespace_touch(current, key)?;
                // A property an earlier object of the chain owns shadows this
                // one whatever its attributes: a non-enumerable own property
                // hides an enumerable inherited one rather than revealing it.
                let mut shadowed = false;
                let mut ancestor = object;
                while ancestor.is_object() {
                    if ancestor.as_handle() == current.as_handle() {
                        break;
                    }
                    if object::get_own_property(self.heap, ancestor.as_handle(), key)
                        .map_err(|_| self.heap_failure())?
                        .is_some()
                    {
                        shadowed = true;
                        break;
                    }
                    ancestor = object::prototype(self.heap, ancestor.as_handle())
                        .map_err(|_| self.heap_failure())?;
                }
                if shadowed || !self.is_enumerable(current, key)? {
                    continue;
                }
                let name = self.key_to_value(key)?;
                let mut seen = false;
                let mut index = 0u32;
                while index < written {
                    let existing = self.element(array, index)?;
                    if self.strict_equals(existing, name)? {
                        seen = true;
                        break;
                    }
                    index += 1;
                }
                if !seen {
                    self.set_element(array, written, name)?;
                    written += 1;
                }
            }
            current = object::prototype(self.heap, current.as_handle())
                .map_err(|_| self.heap_failure())?;
            depth += 1;
        }
        self.set_length(array, written)?;
        Ok(array)
    }

    /// The iterator a value offers, or nothing where it offers none.
    /// The iterator a `for await` walks: the async protocol's when the
    /// value carries one, the sync protocol's otherwise — whose results the
    /// loop awaits either way.
    fn async_iterator_of(&mut self, value: Value) -> Result<Option<Value>, Completion> {
        if value.is_nullish() {
            return Ok(None);
        }
        let key = Key::Symbol(self.realm.async_iterator_symbol);
        let method = self.get_property(value, key)?;
        if self.is_callable_value(method) {
            let iterator = self.call_value(method, value, &[])?;
            if !iterator.is_object() {
                return Err(self.throw_type_error());
            }
            return Ok(Some(iterator));
        }
        // GetMethod: only an absent method falls back to the sync iterator;
        // a present value that is not callable is a TypeError.
        if !method.is_nullish() {
            return Err(self.throw_type_error());
        }
        let Some(sync) = self.iterator_of(value)? else {
            return Ok(None);
        };
        self.async_from_sync(sync).map(Some)
    }

    /// Wrap a sync iterator as an async one: `next`, `return`, and `throw`
    /// call through and answer a promise settled once the result's value
    /// has been awaited — the specification's %AsyncFromSyncIteratorPrototype%.
    fn async_from_sync(&mut self, sync: Value) -> Result<Value, Completion> {
        let wrapper = object::create(self.heap, Value::object(self.realm.object_prototype))
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        // The iterator record fetches `next` once, at creation: every step
        // calls what that fetch produced.
        let next_key = self.ascii_key(b"next")?;
        let next_method = self.get_property(sync, next_key)?;
        for (name, held) in [(&b"\0sync"[..], sync), (&b"\0next"[..], next_method)] {
            let key = self.ascii_key(name)?;
            object::define_own_property(
                self.heap,
                wrapper,
                key,
                Descriptor::data(held, attribute::WRITABLE),
            )
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        }
        for (name, id) in [
            (&b"next"[..], native::ASYNC_FROM_SYNC_NEXT),
            (&b"return"[..], native::ASYNC_FROM_SYNC_RETURN),
            (&b"throw"[..], native::ASYNC_FROM_SYNC_THROW),
        ] {
            let method = self.settle_function(id, wrapper)?;
            let key = self.ascii_key(name)?;
            object::define_own_property(
                self.heap,
                wrapper,
                key,
                Descriptor::data(method, attribute::WRITABLE | attribute::CONFIGURABLE),
            )
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        }
        Ok(Value::object(wrapper))
    }

    /// One step of an async-from-sync wrapper: call the sync iterator's
    /// method and answer a promise that settles once the result's value has
    /// been awaited — a rejection closing the sync iterator where the step
    /// was not already done, unless the step was a `return`.
    fn async_from_sync_step(
        &mut self,
        id: u32,
        wrapper: Value,
        argument: Option<Value>,
    ) -> Result<Value, Completion> {
        let promise = self.new_promise()?;
        let answer = Value::object(promise);
        let sync_key = self.ascii_key(b"\0sync")?;
        let sync = if wrapper.is_object() {
            object::get_own_property(self.heap, wrapper.as_handle(), sync_key)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?
                .map_or(Value::UNDEFINED, |descriptor| descriptor.value)
        } else {
            Value::UNDEFINED
        };
        let name: &[u8] = match id {
            native::ASYNC_FROM_SYNC_RETURN => b"return",
            native::ASYNC_FROM_SYNC_THROW => b"throw",
            _ => b"\0next",
        };
        let key = self.ascii_key(name)?;
        let fetched = if id == native::ASYNC_FROM_SYNC_NEXT {
            if wrapper.is_object() {
                object::get_own_property(self.heap, wrapper.as_handle(), key)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))
                    .map(|held| held.map_or(Value::UNDEFINED, |descriptor| descriptor.value))
            } else {
                Ok(Value::UNDEFINED)
            }
        } else {
            self.get_property(sync, key)
        };
        let method = match fetched {
            Ok(method) => method,
            Err(Completion::Throw(reason)) => {
                self.settle(promise, promise::REJECTED, reason)?;
                return Ok(answer);
            }
            Err(other) => return Err(other),
        };
        if id != native::ASYNC_FROM_SYNC_NEXT && method.is_nullish() {
            if id == native::ASYNC_FROM_SYNC_RETURN {
                // No `return`: the iterator counts as closed, the value
                // handed back done.
                let result = self.iteration_result(argument.unwrap_or(Value::UNDEFINED), true)?;
                self.resolve(promise, result)?;
                return Ok(answer);
            }
            // No `throw`: the iterator is closed, and the caller told so.
            match self.close_iterator(sync) {
                Ok(()) => {}
                Err(Completion::Throw(reason)) => {
                    self.settle(promise, promise::REJECTED, reason)?;
                    return Ok(answer);
                }
                Err(other) => return Err(other),
            }
            let reason = self.create_error(ErrorKind::Type, Value::UNDEFINED)?;
            self.settle(promise, promise::REJECTED, reason)?;
            return Ok(answer);
        }
        let called = match argument {
            Some(argument) => self.call_value(method, sync, &[argument]),
            None => self.call_value(method, sync, &[]),
        };
        let result = match called {
            Ok(result) => result,
            Err(Completion::Throw(reason)) => {
                self.settle(promise, promise::REJECTED, reason)?;
                return Ok(answer);
            }
            Err(other) => return Err(other),
        };
        if !result.is_object() {
            let reason = self.create_error(ErrorKind::Type, Value::UNDEFINED)?;
            self.settle(promise, promise::REJECTED, reason)?;
            return Ok(answer);
        }
        let done_key = self.ascii_key(b"done")?;
        let value_key = self.ascii_key(b"value")?;
        let fields = match self.get_property(result, done_key) {
            Ok(done) => match self.coerce_to_boolean(done) {
                Ok(done) => match self.get_property(result, value_key) {
                    Ok(value) => Ok((done, value)),
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        let (done, value) = match fields {
            Ok(fields) => fields,
            Err(Completion::Throw(reason)) => {
                self.settle(promise, promise::REJECTED, reason)?;
                return Ok(answer);
            }
            Err(other) => return Err(other),
        };
        let close_on_rejection = id != native::ASYNC_FROM_SYNC_RETURN && !done;
        let wrapped = match self.promise_for(value) {
            Ok(wrapped) => wrapped,
            Err(Completion::Throw(reason)) => {
                if close_on_rejection {
                    match self.close_iterator(sync) {
                        Ok(()) | Err(Completion::Throw(_)) => {}
                        Err(other) => return Err(other),
                    }
                }
                self.settle(promise, promise::REJECTED, reason)?;
                return Ok(answer);
            }
            Err(other) => return Err(other),
        };
        let on_ok = self.settle_function(
            if done {
                native::ASYNC_FROM_SYNC_DONE
            } else {
                native::ASYNC_FROM_SYNC_MORE
            },
            promise,
        )?;
        let on_err = if close_on_rejection && sync.is_object() {
            self.settle_function(native::ASYNC_FROM_SYNC_CLOSE, sync.as_handle())?
        } else {
            self.settle_function(native::ASYNC_FROM_SYNC_PASS, promise)?
        };
        let Some(queue) = self.queue.as_deref_mut() else {
            return Err(Completion::Terminated(Termination::NotImplemented));
        };
        promise::react(self.heap, queue, wrapped, on_ok, on_err, answer)
            .map_err(|_| Completion::Terminated(Termination::QuotaExceeded))?;
        Ok(answer)
    }

    fn iterator_of(&mut self, value: Value) -> Result<Option<Value>, Completion> {
        if value.is_nullish() {
            return Ok(None);
        }
        let key = Key::Symbol(self.realm.iterator_symbol);
        let method = self.get_property(value, key)?;
        if !self.is_callable_value(method) {
            return Ok(None);
        }
        let iterator = self.call_value(method, value, &[])?;
        if !iterator.is_object() {
            return Err(self.throw_type_error());
        }
        Ok(Some(iterator))
    }

    /// Close an iterator the language finished with early: its `return`
    /// method runs, and a result that is not an object is the TypeError the
    /// specification makes it.
    fn close_iterator(&mut self, iterator: Value) -> Result<(), Completion> {
        if !iterator.is_object() {
            return Ok(());
        }
        let return_key = self.ascii_key(b"return")?;
        let method = self.get_property(iterator, return_key)?;
        if method.is_nullish() {
            return Ok(());
        }
        if !self.is_callable_value(method) {
            return Err(self.throw_type_error());
        }
        let result = self.call_value(method, iterator, &[])?;
        if !result.is_object() {
            return Err(self.throw_type_error());
        }
        Ok(())
    }

    /// The next value an iterator produces, or nothing when it is done.
    fn iterator_step(&mut self, iterator: Value) -> Result<Option<Value>, Completion> {
        let next_key = self.ascii_key(b"next")?;
        let next = self.get_property(iterator, next_key)?;
        if !self.is_callable_value(next) {
            return Err(self.throw_type_error());
        }
        let result = self.call_value(next, iterator, &[])?;
        if !result.is_object() {
            return Err(self.throw_type_error());
        }
        let done_key = self.ascii_key(b"done")?;
        let done = self.get_property(result, done_key)?;
        if self.coerce_to_boolean(done)? {
            return Ok(None);
        }
        let value_key = self.ascii_key(b"value")?;
        Ok(Some(self.get_property(result, value_key)?))
    }

    /// The Number a string denotes when a program asks for it by name.
    fn parse_number(&mut self, text: Handle, radix: u32, float: bool) -> Result<Value, Completion> {
        let length = string::length(self.heap, text).map_err(|_| self.heap_failure())? as usize;
        let mut units = [0u16; 128];
        let room = length.min(units.len());
        string::copy_units(self.heap, text, units.get_mut(..room).unwrap_or(&mut []))
            .map_err(|_| self.heap_failure())?;
        let units = units.get(..room).unwrap_or(&[]);
        if float {
            return Ok(Value::number(crate::numeric::parse_float_prefix(units)));
        }
        Ok(Value::number(crate::numeric::parse_int_prefix(
            units, radix,
        )))
    }

    // BigInts. A BigInt is exact, so it never mixes with a Number in
    // arithmetic: the specification makes that a type error rather than a
    // conversion, because either direction would lose something.

    /// ToNumeric: the primitive first, kept when it is a BigInt, a Number
    /// otherwise — with a symbol refused as the TypeError it is.
    fn numeric_value_of(&mut self, value: Value) -> Result<Value, Completion> {
        let primitive = if value.is_object() {
            self.coerce_to_primitive(value, Hint::Number)?
        } else {
            value
        };
        if matches!(primitive.tag(), Tag::BigInt) {
            return Ok(primitive);
        }
        Ok(Value::number(self.coerce_to_number(primitive)?))
    }

    fn either_is_big_int(&self, left: Value, right: Value) -> bool {
        matches!(left.tag(), Tag::BigInt) || matches!(right.tag(), Tag::BigInt)
    }

    /// The limbs of a value that must be a BigInt.
    fn big_int_operand(&mut self, value: Value) -> Result<crate::bigint::Number, Completion> {
        if !matches!(value.tag(), Tag::BigInt) {
            return Err(self.throw_type_error());
        }
        crate::bigint::read(self.heap, value.as_handle())
            .map_err(|_| Completion::Terminated(Termination::Malformed))
    }

    /// One code unit of a string, or zero past its end.
    fn string_unit(&self, handle: Handle, at: usize) -> Result<u16, Completion> {
        Ok(
            string::unit_at(self.heap, handle, u32::try_from(at).unwrap_or(u32::MAX))
                .map_err(|_| self.heap_failure())?
                .unwrap_or(0),
        )
    }

    /// What a failed BigInt operation means to a program: a numeral too big
    /// for the engine's limbs is a range error, because the text named a
    /// number the engine cannot hold; anything else is text that is not a
    /// numeral at all.
    fn big_int_failure(&mut self, error: crate::bigint::BigIntError) -> Completion {
        match error {
            crate::bigint::BigIntError::TooLarge => self.throw_error_of(ErrorKind::Range),
            _ => self.throw_error_of(ErrorKind::Syntax),
        }
    }

    fn big_int_value(&mut self, number: &crate::bigint::Number) -> Result<Value, Completion> {
        let handle = crate::bigint::write(self.heap, number)
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(Value::big_int(handle))
    }

    fn big_int_arithmetic(
        &mut self,
        opcode: Opcode,
        left: Value,
        right: Value,
    ) -> Result<Value, Completion> {
        let left = self.big_int_operand(left)?;
        let right = self.big_int_operand(right)?;
        let outcome = match opcode {
            Opcode::Add => crate::bigint::add(&left, &right),
            Opcode::Sub => crate::bigint::subtract(&left, &right),
            Opcode::Mul => crate::bigint::multiply(&left, &right),
            Opcode::Div => crate::bigint::divide(&left, &right).map(|(quotient, _)| quotient),
            Opcode::Mod => crate::bigint::divide(&left, &right).map(|(_, remainder)| remainder),
            Opcode::Exp => crate::bigint::power(&left, &right),
            _ => return Err(self.throw_type_error()),
        };
        match outcome {
            Ok(number) => self.big_int_value(&number),
            Err(crate::bigint::BigIntError::DivisionByZero) => {
                Err(self.throw_error_of(ErrorKind::Range))
            }
            Err(crate::bigint::BigIntError::NegativeExponent) => {
                Err(self.throw_error_of(ErrorKind::Range))
            }
            Err(crate::bigint::BigIntError::TooLarge) => Err(self.throw_error_of(ErrorKind::Range)),
            Err(_) => Err(Completion::Terminated(Termination::Malformed)),
        }
    }

    fn big_int_bitwise(
        &mut self,
        opcode: Opcode,
        left: Value,
        right: Value,
    ) -> Result<Value, Completion> {
        let left = self.big_int_operand(left)?;
        let right = self.big_int_operand(right)?;
        let outcome = match opcode {
            Opcode::BitAnd => crate::bigint::bitwise(crate::bigint::Bitwise::And, &left, &right),
            Opcode::BitOr => crate::bigint::bitwise(crate::bigint::Bitwise::Or, &left, &right),
            Opcode::BitXor => crate::bigint::bitwise(crate::bigint::Bitwise::Xor, &left, &right),
            Opcode::ShiftLeft | Opcode::ShiftRight => {
                let Some(count) = right.to_i64() else {
                    return Err(self.throw_error_of(ErrorKind::Range));
                };
                let left_shift = matches!(opcode, Opcode::ShiftLeft) == (count >= 0);
                let magnitude = count.unsigned_abs();
                if left_shift {
                    crate::bigint::shift_left(&left, magnitude)
                } else {
                    crate::bigint::shift_right(&left, magnitude)
                }
            }
            // An unsigned shift has no meaning for a value with no width.
            _ => return Err(self.throw_type_error()),
        };
        match outcome {
            Ok(number) => self.big_int_value(&number),
            Err(crate::bigint::BigIntError::TooLarge) => Err(self.throw_error_of(ErrorKind::Range)),
            Err(_) => Err(Completion::Terminated(Termination::Malformed)),
        }
    }

    /// How a BigInt orders against another value, exactly.
    ///
    /// A comparison with a Number is a comparison of mathematical values, so it
    /// is made on the integer part and then on what the fraction adds, rather
    /// than by rounding the BigInt to a double.
    fn big_int_ordering(
        &mut self,
        left: Value,
        right: Value,
    ) -> Result<Option<core::cmp::Ordering>, Completion> {
        if matches!(left.tag(), Tag::BigInt) && matches!(right.tag(), Tag::BigInt) {
            let left = self.big_int_operand(left)?;
            let right = self.big_int_operand(right)?;
            return Ok(Some(crate::bigint::compare(&left, &right)));
        }
        let (big, other, flipped) = if matches!(left.tag(), Tag::BigInt) {
            (left, right, false)
        } else {
            (right, left, true)
        };
        let big = self.big_int_operand(big)?;
        let ordering = match other.tag() {
            Tag::String => {
                // A string compares as the BigInt it denotes, and as nothing at
                // all when it denotes none.
                let Ok(parsed) = self.big_int_of(other) else {
                    return Ok(None);
                };
                let parsed = self.big_int_operand(parsed)?;
                Some(crate::bigint::compare(&big, &parsed))
            }
            _ => {
                let number = self.coerce_to_number(other)?;
                if number.is_nan() {
                    None
                } else if number.is_infinite() {
                    Some(if number > 0.0 {
                        core::cmp::Ordering::Less
                    } else {
                        core::cmp::Ordering::Greater
                    })
                } else {
                    let integer = value::truncate(number);
                    let fraction = number - integer;
                    let other = crate::bigint::from_f64(integer)
                        .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                    let ordering = crate::bigint::compare(&big, &other);
                    Some(if ordering != core::cmp::Ordering::Equal {
                        ordering
                    } else if fraction > 0.0 {
                        core::cmp::Ordering::Less
                    } else if fraction < 0.0 {
                        core::cmp::Ordering::Greater
                    } else {
                        core::cmp::Ordering::Equal
                    })
                }
            }
        };
        Ok(match (ordering, flipped) {
            (Some(ordering), true) => Some(ordering.reverse()),
            (ordering, _) => ordering,
        })
    }

    /// The text of a BigInt, which is its digits with no suffix.
    fn big_int_text(&mut self, value: Value, radix: u32) -> Result<Value, Completion> {
        let number = self.big_int_operand(value)?;
        // A value of the admitted width is at most this many digits in binary,
        // which is the longest text any radix produces.
        let mut units = [0u16; crate::bigint::MAX_LIMBS * 32 + 2];
        let written = crate::bigint::text(&number, radix, &mut units);
        self.make_string(units.get(..written).unwrap_or(&[]))
    }

    /// `BigInt(value)`, which admits an integral Number, a string of digits, a
    /// boolean, or a BigInt.
    fn big_int_of(&mut self, value: Value) -> Result<Value, Completion> {
        match value.tag() {
            Tag::BigInt => Ok(value),
            Tag::Boolean => {
                let mut number = crate::bigint::Number::ZERO;
                if value.as_boolean() {
                    number.limbs[0] = 1;
                    number.length = 1;
                }
                self.big_int_value(&number)
            }
            Tag::Number => {
                let number = crate::bigint::from_f64(value.as_number())
                    .map_err(|_| self.throw_error_of(ErrorKind::Range))?;
                self.big_int_value(&number)
            }
            Tag::String => {
                let handle = value.as_handle();
                let length =
                    string::length(self.heap, handle).map_err(|_| self.heap_failure())? as usize;
                // The numeral is what lies between the whitespace at either
                // end, so the end is found before the digits are read.
                let mut at = 0usize;
                let mut end = length;
                while at < end && is_string_white_space(self.string_unit(handle, at)?) {
                    at += 1;
                }
                while end > at && is_string_white_space(self.string_unit(handle, end - 1)?) {
                    end -= 1;
                }
                // An empty string, and one that is nothing but whitespace, is
                // zero. Everything else must be a numeral.
                if at == end {
                    return self.big_int_value(&crate::bigint::Number::ZERO);
                }
                let first = self.string_unit(handle, at)?;
                let signed = first == u16::from(b'-') || first == u16::from(b'+');
                let negative = first == u16::from(b'-');
                if signed {
                    at += 1;
                }
                // A radix prefix, on an unsigned numeral only.
                let mut radix = 10u32;
                if !signed && end - at >= 2 && self.string_unit(handle, at)? == u16::from(b'0') {
                    radix = match self.string_unit(handle, at + 1)? {
                        0x78 | 0x58 => 16,
                        0x6F | 0x4F => 8,
                        0x62 | 0x42 => 2,
                        _ => 10,
                    };
                    if radix != 10 {
                        at += 2;
                    }
                }
                let mut accumulator = crate::bigint::Accumulator::new(radix);
                while at < end {
                    let unit = self.string_unit(handle, at)?;
                    // The string grammar has no separators: only a literal
                    // written in source may carry them.
                    if unit > 0x7F || unit == u16::from(b'_') {
                        return Err(self.throw_error_of(ErrorKind::Syntax));
                    }
                    accumulator
                        .push(unit as u8)
                        .map_err(|error| self.big_int_failure(error))?;
                    at += 1;
                }
                // A sign or a radix prefix with no digits behind it is not a
                // numeral.
                if accumulator.digits() == 0 {
                    return Err(self.throw_error_of(ErrorKind::Syntax));
                }
                let number = accumulator.finish_signed(negative);
                self.big_int_value(&number)
            }
            _ => Err(self.throw_type_error()),
        }
    }

    // Regular expressions. A pattern is compiled once into a program of bytes,
    // and the program is held in the object the pattern made.

    /// Give the machine somewhere to match in. Without it, a program that uses
    /// a regular expression is told so rather than matching in storage it was
    /// never given.
    pub fn attach_regexp(
        &mut self,
        choices: &'a mut [crate::regexp::Choice],
        undo: &'a mut [(u8, u32)],
        subject: &'a mut [u16],
    ) {
        self.regexp_choices = Some(choices);
        self.regexp_undo = Some(undo);
        self.regexp_subject = Some(subject);
    }

    /// Build a regular expression from its pattern and flags.
    fn create_regexp(&mut self, pattern: &[u16], flags: u8) -> Result<Value, Completion> {
        let mut code = [0u8; crate::regexp::MAX_PROGRAM];
        let (length, groups) = crate::regexp::compile(pattern, flags, &mut code)
            .map_err(|_| self.throw_error_of(ErrorKind::Syntax))?;
        // The program is held the way a string's units are held: it is bytes,
        // it holds no reference, and the collector already knows how to move
        // one of those.
        let mut units = [0u16; crate::regexp::MAX_PROGRAM + 2];
        units[0] = u16::from(groups);
        let mut index = 0usize;
        while index < length {
            units[index + 1] = u16::from(code[index]);
            index += 1;
        }
        let program = self.make_string(units.get(..length + 1).unwrap_or(&[]))?;
        let source = self.make_string(pattern)?;
        let handle = object::create_regexp(
            self.heap,
            Value::object(self.realm.regexp_prototype),
            program,
            flags,
        )
        .map_err(|_| self.heap_failure())?;
        let value = Value::object(handle);
        let source_key = self.ascii_key(b"source")?;
        object::define_own_property(self.heap, handle, source_key, Descriptor::data(source, 0))
            .map_err(|_| self.heap_failure())?;
        let mut flag_units = [0u16; 8];
        let written = crate::regexp::flag_text(flags, &mut flag_units);
        let flag_text = self.make_string(flag_units.get(..written).unwrap_or(&[]))?;
        let flags_key = self.ascii_key(b"flags")?;
        object::define_own_property(self.heap, handle, flags_key, Descriptor::data(flag_text, 0))
            .map_err(|_| self.heap_failure())?;
        for (name, bit) in [
            (&b"global"[..], crate::regexp::flag::GLOBAL),
            (&b"ignoreCase"[..], crate::regexp::flag::IGNORE_CASE),
            (&b"multiline"[..], crate::regexp::flag::MULTILINE),
            (&b"dotAll"[..], crate::regexp::flag::DOT_ALL),
            (&b"sticky"[..], crate::regexp::flag::STICKY),
        ] {
            let key = self.ascii_key(name)?;
            object::define_own_property(
                self.heap,
                handle,
                key,
                Descriptor::data(Value::boolean(flags & bit != 0), 0),
            )
            .map_err(|_| self.heap_failure())?;
        }
        let last_index_key = self.ascii_key(b"lastIndex")?;
        object::define_own_property(
            self.heap,
            handle,
            last_index_key,
            Descriptor::data(Value::number(0.0), attribute::WRITABLE),
        )
        .map_err(|_| self.heap_failure())?;
        Ok(value)
    }

    /// Match `subject` with a regular expression from `start`, answering the
    /// slots it filled.
    fn match_regexp(
        &mut self,
        regexp: Value,
        subject: Handle,
        start: u32,
        sticky: bool,
    ) -> Result<Option<crate::regexp::Slots>, Completion> {
        if !regexp.is_object() {
            return Err(self.throw_type_error());
        }
        let Some((program, flags)) = object::regexp_program(self.heap, regexp.as_handle())
            .map_err(|_| self.heap_failure())?
        else {
            return Err(self.throw_type_error());
        };
        if !program.is_string() {
            return Err(self.throw_type_error());
        }

        // The program and the subject are copied out of the heap, because the
        // matcher works on units and the heap may move under it.
        let program_length = string::length(self.heap, program.as_handle())
            .map_err(|_| self.heap_failure())? as usize;
        let mut code = [0u8; crate::regexp::MAX_PROGRAM];
        let mut units = [0u16; crate::regexp::MAX_PROGRAM + 2];
        if program_length > units.len() {
            return Err(self.throw_error_of(ErrorKind::Range));
        }
        string::copy_units(
            self.heap,
            program.as_handle(),
            units.get_mut(..program_length).unwrap_or(&mut []),
        )
        .map_err(|_| self.heap_failure())?;
        let groups = u8::try_from(units.first().copied().unwrap_or(0)).unwrap_or(0);
        let mut index = 1usize;
        while index < program_length && index - 1 < code.len() {
            code[index - 1] = u8::try_from(units[index]).unwrap_or(0);
            index += 1;
        }

        let subject_length =
            string::length(self.heap, subject).map_err(|_| self.heap_failure())? as usize;
        let Some(buffer) = self.regexp_subject.as_deref_mut() else {
            return Err(Completion::Terminated(Termination::NotImplemented));
        };
        if subject_length > buffer.len() {
            return Err(Completion::Terminated(Termination::QuotaExceeded));
        }
        string::copy_units(self.heap, subject, &mut buffer[..subject_length])
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;

        let program = crate::regexp::Program {
            code: code.get(..program_length.saturating_sub(1)).unwrap_or(&[]),
            groups,
            flags,
        };
        let (Some(choices), Some(undo), Some(subject_units)) = (
            self.regexp_choices.as_deref_mut(),
            self.regexp_undo.as_deref_mut(),
            self.regexp_subject.as_deref(),
        ) else {
            return Err(Completion::Terminated(Termination::NotImplemented));
        };
        let input = subject_units.get(..subject_length).unwrap_or(&[]);
        let mut matcher = crate::regexp::Matcher { choices, undo };
        // The match spends the task's own fuel, so what it may spend is what
        // the task has left, up to the ceiling one match is allowed.
        let budget = u32::try_from(self.fuel.min(u64::from(REGEXP_FUEL))).unwrap_or(REGEXP_FUEL);
        let mut fuel = budget;
        let mut at = start as usize;
        loop {
            if at > input.len() {
                return Ok(None);
            }
            if let Some(slots) = crate::regexp::run(&program, input, at, &mut matcher, &mut fuel) {
                self.fuel = self.fuel.saturating_sub(u64::from(budget - fuel));
                return Ok(Some(slots));
            }
            if sticky {
                self.fuel = self.fuel.saturating_sub(u64::from(budget - fuel));
                return Ok(None);
            }
            // Unicode mode advances by code point: a start inside a
            // surrogate pair is not a start at all.
            if flags & crate::regexp::flag::UNICODE != 0
                && input
                    .get(at)
                    .is_some_and(|&unit| (0xD800..0xDC00).contains(&unit))
                && input
                    .get(at + 1)
                    .is_some_and(|&low| (0xDC00..0xE000).contains(&low))
            {
                at += 2;
            } else {
                at += 1;
            }
            if fuel == 0 {
                // Matching is charged to the task's fuel, so a pathological
                // pattern ends the task rather than the machine.
                self.fuel = 0;
                return Err(Completion::Terminated(Termination::FuelExhausted));
            }
            if at > input.len() {
                self.fuel = self.fuel.saturating_sub(u64::from(budget - fuel));
                return Ok(None);
            }
        }
    }

    /// `replace` with a pattern: every match where it is global, and the first
    /// otherwise. A `$` in the replacement text names part of the match.
    fn replace_with_pattern(
        &mut self,
        subject: Handle,
        pattern: Value,
        replacement: Value,
    ) -> Result<Value, Completion> {
        let (global, sticky) = self.regexp_kind(pattern)?;
        let groups = self.regexp_groups(pattern);
        let length = string::length(self.heap, subject).map_err(|_| self.heap_failure())?;
        let callable = self.is_callable_value(replacement);
        let mut result = self.ascii_string(b"")?.as_handle();
        let mut position = 0u32;
        let mut start = 0u32;
        loop {
            let Some(slots) = self.match_regexp(pattern, subject, start, sticky)? else {
                break;
            };
            let head = string::slice(self.heap, subject, position, slots[0])
                .map_err(|_| self.heap_failure())?;
            result = string::concat(self.heap, result, head).map_err(|_| self.heap_failure())?;

            let text = if callable {
                let mut arguments = [Value::UNDEFINED; MAX_ARGUMENTS];
                let matched = string::slice(self.heap, subject, slots[0], slots[1])
                    .map_err(|_| self.heap_failure())?;
                arguments[0] = Value::string(matched);
                let mut count = 1usize;
                let mut index = 1usize;
                while index <= usize::from(groups) && count + 2 < arguments.len() {
                    let (from, to) = (slots[index * 2], slots[index * 2 + 1]);
                    arguments[count] = if from == u32::MAX || to == u32::MAX {
                        Value::UNDEFINED
                    } else {
                        let piece = string::slice(self.heap, subject, from, to)
                            .map_err(|_| self.heap_failure())?;
                        Value::string(piece)
                    };
                    count += 1;
                    index += 1;
                }
                arguments[count] = Value::number(crate::softfloat::from_u64(u64::from(slots[0])));
                arguments[count + 1] = Value::string(subject);
                count += 2;
                let named = self.named_captures(pattern, subject, &slots)?;
                if named.is_object() && count < arguments.len() {
                    arguments[count] = named;
                    count += 1;
                }
                let outcome = self.call_with(
                    replacement,
                    Value::UNDEFINED,
                    arguments.get(..count).unwrap_or(&[]),
                )?;
                self.string_handle(outcome)?
            } else {
                let text = self.string_handle(replacement)?;
                let named = self.named_captures(pattern, subject, &slots)?;
                self.expand_replacement(subject, text, &slots, groups, named)?
            };
            result = string::concat(self.heap, result, text).map_err(|_| self.heap_failure())?;

            position = slots[1];
            start = if slots[1] > slots[0] {
                slots[1]
            } else {
                slots[1] + 1
            };
            if !global {
                break;
            }
            if start > length {
                break;
            }
        }
        let tail =
            string::slice(self.heap, subject, position, length).map_err(|_| self.heap_failure())?;
        result = string::concat(self.heap, result, tail).map_err(|_| self.heap_failure())?;
        Ok(Value::string(result))
    }

    /// Expand `$&`, `` $` ``, `$'`, `$$`, and `$1`-`$99` in a replacement.
    fn expand_replacement(
        &mut self,
        subject: Handle,
        replacement: Handle,
        slots: &crate::regexp::Slots,
        groups: u8,
        named: Value,
    ) -> Result<Handle, Completion> {
        let length = string::length(self.heap, replacement).map_err(|_| self.heap_failure())?;
        let subject_length = string::length(self.heap, subject).map_err(|_| self.heap_failure())?;
        let mut result = self.ascii_string(b"")?.as_handle();
        let mut index = 0u32;
        let mut plain_start = 0u32;
        while index < length {
            let unit = string::unit_at(self.heap, replacement, index)
                .map_err(|_| self.heap_failure())?
                .unwrap_or(0);
            if unit != 0x24 || index + 1 >= length {
                index += 1;
                continue;
            }
            let next = string::unit_at(self.heap, replacement, index + 1)
                .map_err(|_| self.heap_failure())?
                .unwrap_or(0);
            let (piece, width) = match next {
                0x24 => {
                    let text = string::slice(self.heap, replacement, index, index + 1)
                        .map_err(|_| self.heap_failure())?;
                    (Some(text), 2)
                }
                0x26 => {
                    let text = string::slice(self.heap, subject, slots[0], slots[1])
                        .map_err(|_| self.heap_failure())?;
                    (Some(text), 2)
                }
                0x60 => {
                    let text = string::slice(self.heap, subject, 0, slots[0])
                        .map_err(|_| self.heap_failure())?;
                    (Some(text), 2)
                }
                0x27 => {
                    let text = string::slice(self.heap, subject, slots[1], subject_length)
                        .map_err(|_| self.heap_failure())?;
                    (Some(text), 2)
                }
                0x30..=0x39 => {
                    // One or two digits, whichever names a group there is.
                    let mut number = u32::from(next - 0x30);
                    let mut width = 2u32;
                    if index + 2 < length {
                        let third = string::unit_at(self.heap, replacement, index + 2)
                            .map_err(|_| self.heap_failure())?
                            .unwrap_or(0);
                        if (0x30..=0x39).contains(&third) {
                            let wider = number * 10 + u32::from(third - 0x30);
                            if wider <= u32::from(groups) && wider != 0 {
                                number = wider;
                                width = 3;
                            }
                        }
                    }
                    if number == 0 || number > u32::from(groups) {
                        (None, 0)
                    } else {
                        let (from, to) =
                            (slots[number as usize * 2], slots[number as usize * 2 + 1]);
                        let text = if from == u32::MAX || to == u32::MAX {
                            self.ascii_string(b"")?.as_handle()
                        } else {
                            string::slice(self.heap, subject, from, to)
                                .map_err(|_| self.heap_failure())?
                        };
                        (Some(text), width)
                    }
                }
                // `$<name>`: the named capture, or nothing for a name the
                // pattern lacks; literal where the pattern names no group.
                0x3C if named.is_object() => {
                    let mut close = index + 2;
                    while close < length {
                        let unit = string::unit_at(self.heap, replacement, close)
                            .map_err(|_| self.heap_failure())?
                            .unwrap_or(0);
                        if unit == 0x3E {
                            break;
                        }
                        close += 1;
                    }
                    if close >= length {
                        (None, 0)
                    } else {
                        let name = string::slice(self.heap, replacement, index + 2, close)
                            .map_err(|_| self.heap_failure())?;
                        let key = self.coerce_to_key(Value::string(name))?;
                        let capture = self.get_property(named, key)?;
                        let text = if capture.is_undefined() {
                            self.ascii_string(b"")?.as_handle()
                        } else {
                            self.string_handle(capture)?
                        };
                        (Some(text), close - index + 1)
                    }
                }
                _ => (None, 0),
            };
            let Some(piece) = piece else {
                index += 1;
                continue;
            };
            let plain = string::slice(self.heap, replacement, plain_start, index)
                .map_err(|_| self.heap_failure())?;
            result = string::concat(self.heap, result, plain).map_err(|_| self.heap_failure())?;
            result = string::concat(self.heap, result, piece).map_err(|_| self.heap_failure())?;
            index += width;
            plain_start = index;
        }
        let plain = string::slice(self.heap, replacement, plain_start, length)
            .map_err(|_| self.heap_failure())?;
        result = string::concat(self.heap, result, plain).map_err(|_| self.heap_failure())?;
        Ok(result)
    }

    /// `split` with a pattern: the pieces between matches, with whatever the
    /// pattern captured between them.
    fn split_by_pattern(&mut self, subject: Handle, pattern: Value) -> Result<Value, Completion> {
        let groups = self.regexp_groups(pattern);
        let length = string::length(self.heap, subject).map_err(|_| self.heap_failure())?;
        let array = self.new_array()?;
        let mut written = 0u32;
        let mut position = 0u32;
        let mut start = 0u32;
        loop {
            if start > length {
                break;
            }
            let Some(slots) = self.match_regexp(pattern, subject, start, false)? else {
                break;
            };
            if slots[1] == slots[0] && slots[0] >= length {
                break;
            }
            let piece = string::slice(self.heap, subject, position, slots[0])
                .map_err(|_| self.heap_failure())?;
            self.set_element(array, written, Value::string(piece))?;
            written += 1;
            let mut index = 1usize;
            while index <= usize::from(groups) {
                let (from, to) = (slots[index * 2], slots[index * 2 + 1]);
                let value = if from == u32::MAX || to == u32::MAX {
                    Value::UNDEFINED
                } else {
                    let text = string::slice(self.heap, subject, from, to)
                        .map_err(|_| self.heap_failure())?;
                    Value::string(text)
                };
                self.set_element(array, written, value)?;
                written += 1;
                index += 1;
            }
            position = slots[1];
            start = if slots[1] > slots[0] {
                slots[1]
            } else {
                slots[1] + 1
            };
        }
        let tail =
            string::slice(self.heap, subject, position, length).map_err(|_| self.heap_failure())?;
        self.set_element(array, written, Value::string(tail))?;
        written += 1;
        self.set_length(array, written)?;
        Ok(array)
    }

    /// A value used as a pattern: itself where it is one, and a pattern of its
    /// text where it is not.
    fn as_regexp(&mut self, value: Value) -> Result<Value, Completion> {
        if value.is_object()
            && object::regexp_program(self.heap, value.as_handle())
                .map_err(|_| self.heap_failure())?
                .is_some()
        {
            return Ok(value);
        }
        let text = self.string_handle(value)?;
        let length = string::length(self.heap, text).map_err(|_| self.heap_failure())? as usize;
        let mut units = [0u16; 512];
        let room = length.min(units.len());
        string::copy_units(self.heap, text, units.get_mut(..room).unwrap_or(&mut []))
            .map_err(|_| self.heap_failure())?;
        // A string used as a pattern is taken literally, which is what
        // `String.prototype.replace` and `split` do with one.
        let mut quoted = [0u16; 1024];
        let mut written = 0usize;
        for &unit in units.get(..room).unwrap_or(&[]) {
            if matches!(
                unit,
                0x5C | 0x5E
                    | 0x24
                    | 0x2E
                    | 0x2A
                    | 0x2B
                    | 0x3F
                    | 0x28
                    | 0x29
                    | 0x5B
                    | 0x5D
                    | 0x7B
                    | 0x7D
                    | 0x7C
                    | 0x2F
            ) {
                if let Some(slot) = quoted.get_mut(written) {
                    *slot = 0x5C;
                    written += 1;
                }
            }
            if let Some(slot) = quoted.get_mut(written) {
                *slot = unit;
                written += 1;
            }
        }
        self.create_regexp(quoted.get(..written).unwrap_or(&[]), 0)
    }

    /// Whether a pattern is global, and whether it is sticky.
    fn regexp_kind(&mut self, regexp: Value) -> Result<(bool, bool), Completion> {
        let Some((_, flags)) = (if regexp.is_object() {
            object::regexp_program(self.heap, regexp.as_handle())
                .map_err(|_| self.heap_failure())?
        } else {
            None
        }) else {
            return Err(self.throw_type_error());
        };
        Ok((
            flags & crate::regexp::flag::GLOBAL != 0,
            flags & crate::regexp::flag::STICKY != 0,
        ))
    }

    /// The array a match answers: the whole match, then each group, with the
    /// index it was found at and the input it was found in.
    fn match_result(
        &mut self,
        subject: Handle,
        slots: &crate::regexp::Slots,
        regexp: Value,
    ) -> Result<Value, Completion> {
        let groups = if regexp.is_object() {
            object::regexp_program(self.heap, regexp.as_handle())
                .map_err(|_| self.heap_failure())?
                .map_or(0, |_| self.regexp_groups(regexp))
        } else {
            0
        };
        let array = self.new_array()?;
        let text = string::slice(self.heap, subject, slots[0], slots[1])
            .map_err(|_| self.heap_failure())?;
        self.set_element(array, 0, Value::string(text))?;
        let mut index = 1u32;
        while index <= u32::from(groups) {
            let start = slots[index as usize * 2];
            let end = slots[index as usize * 2 + 1];
            let value = if start == u32::MAX || end == u32::MAX {
                Value::UNDEFINED
            } else {
                let text = string::slice(self.heap, subject, start, end)
                    .map_err(|_| self.heap_failure())?;
                Value::string(text)
            };
            self.set_element(array, index, value)?;
            index += 1;
        }
        self.set_length(array, index)?;
        let index_key = self.ascii_key(b"index")?;
        let value = Value::number(crate::softfloat::from_u64(u64::from(slots[0])));
        self.define_property(array, index_key, value)?;
        let input_key = self.ascii_key(b"input")?;
        self.define_property(array, input_key, Value::string(subject))?;
        // `groups`: an object of the named captures, or undefined where the
        // pattern names none.
        let groups_value = self.named_captures(regexp, subject, slots)?;
        let groups_key = self.ascii_key(b"groups")?;
        self.define_property(array, groups_key, groups_value)?;
        Ok(array)
    }

    /// The program bytes a regexp holds, copied out for reading its tables.
    fn regexp_code(
        &mut self,
        regexp: Value,
        code: &mut [u8; crate::regexp::MAX_PROGRAM],
    ) -> Result<usize, Completion> {
        let Ok(Some((program, _))) = object::regexp_program(self.heap, regexp.as_handle()) else {
            return Ok(0);
        };
        if !program.is_string() {
            return Ok(0);
        }
        let length = string::length(self.heap, program.as_handle())
            .map_err(|_| self.heap_failure())? as usize;
        let mut units = [0u16; crate::regexp::MAX_PROGRAM + 2];
        if length > units.len() {
            return Ok(0);
        }
        string::copy_units(
            self.heap,
            program.as_handle(),
            units.get_mut(..length).unwrap_or(&mut []),
        )
        .map_err(|_| self.heap_failure())?;
        let mut index = 1usize;
        while index < length && index - 1 < code.len() {
            code[index - 1] = u8::try_from(units[index]).unwrap_or(0);
            index += 1;
        }
        Ok(length.saturating_sub(1))
    }

    /// The `groups` object of a match: each named group's capture under its
    /// name, on an object with no prototype — or undefined without names.
    fn named_captures(
        &mut self,
        regexp: Value,
        subject: Handle,
        slots: &crate::regexp::Slots,
    ) -> Result<Value, Completion> {
        if !regexp.is_object() {
            return Ok(Value::UNDEFINED);
        }
        let mut code = [0u8; crate::regexp::MAX_PROGRAM];
        let length = self.regexp_code(regexp, &mut code)?;
        let mut names = crate::regexp::Names::of(code.get(..length).unwrap_or(&[]));
        if names.count() == 0 {
            return Ok(Value::UNDEFINED);
        }
        let groups = object::create(self.heap, Value::NULL)
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        while let Some((index, bytes)) = names.next_entry() {
            let mut units = [0u16; crate::regexp::MAX_NAME_UNITS];
            let mut count = 0usize;
            while count * 2 + 1 < bytes.len() && count < units.len() {
                units[count] = u16::from(bytes[count * 2]) | (u16::from(bytes[count * 2 + 1]) << 8);
                count += 1;
            }
            let name = self.make_string(units.get(..count).unwrap_or(&[]))?;
            let key = self.coerce_to_key(name)?;
            let (from, to) = (
                slots
                    .get(usize::from(index) * 2)
                    .copied()
                    .unwrap_or(u32::MAX),
                slots
                    .get(usize::from(index) * 2 + 1)
                    .copied()
                    .unwrap_or(u32::MAX),
            );
            let value = if from == u32::MAX || to == u32::MAX {
                Value::UNDEFINED
            } else {
                let text =
                    string::slice(self.heap, subject, from, to).map_err(|_| self.heap_failure())?;
                Value::string(text)
            };
            object::define_own_property(
                self.heap,
                groups,
                key,
                Descriptor::data(
                    value,
                    attribute::WRITABLE | attribute::ENUMERABLE | attribute::CONFIGURABLE,
                ),
            )
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        }
        Ok(Value::object(groups))
    }

    /// How many groups a pattern captures, read from its program.
    fn regexp_groups(&mut self, regexp: Value) -> u8 {
        let Ok(Some((program, _))) = object::regexp_program(self.heap, regexp.as_handle()) else {
            return 0;
        };
        if !program.is_string() {
            return 0;
        }
        string::unit_at(self.heap, program.as_handle(), 0)
            .ok()
            .flatten()
            .and_then(|unit| u8::try_from(unit).ok())
            .unwrap_or(0)
    }

    /// Make a call on an admitted binding.
    ///
    /// The engine records the call and hands back a promise; it does not wait,
    /// and it does not touch a provider.
    fn host_call(&mut self, binding: u32, arguments: &[Value]) -> Result<Value, Completion> {
        let promise = self.new_promise()?;
        let payload = self.payload_digest(arguments)?;

        let outcome = {
            let Some(bindings) = self.bindings.as_deref_mut() else {
                return Err(Completion::Terminated(Termination::NotImplemented));
            };
            bindings.begin(binding, Value::object(promise), self.trace)
        };
        let request = match outcome {
            Ok(request) => request,
            Err(CallError::NotAdmitted) => return Err(self.throw_type_error()),
            Err(_) => {
                // A quota is not a program error: the promise is rejected with
                // one, and the program decides what to do.
                let reason = self.create_error(ErrorKind::Range, Value::UNDEFINED)?;
                self.settle(promise, promise::REJECTED, reason)?;
                return Ok(Value::object(promise));
            }
        };

        let record = CallRecord {
            request,
            binding,
            trace: self.trace,
            payload,
        };
        let Some(outbox) = self.outbox.as_deref_mut() else {
            return Err(Completion::Terminated(Termination::NotImplemented));
        };
        match outbox.get_mut(self.outbox_length) {
            Some(slot) => {
                *slot = record;
                self.outbox_length += 1;
            }
            None => return Err(Completion::Terminated(Termination::QuotaExceeded)),
        }
        Ok(Value::object(promise))
    }

    /// The digest of a call's arguments, which is what crosses the boundary in
    /// place of the values themselves.
    fn payload_digest(&mut self, arguments: &[Value]) -> Result<crate::digest::Digest, Completion> {
        let mut hasher = crate::digest::Hasher::new();
        for &argument in arguments {
            let text = self.coerce_to_string(argument)?;
            let handle = text.as_handle();
            let length = string::length(self.heap, handle)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            hasher.update(&length.to_le_bytes());
            let mut index = 0u32;
            while index < length {
                let unit = string::unit_at(self.heap, handle, index)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?
                    .unwrap_or(0);
                hasher.update(&unit.to_le_bytes());
                index += 1;
            }
        }
        Ok(hasher.finish())
    }

    /// Suspend the top frame on an awaited value: the frame's state moves to
    /// the heap, reactions on the value's promise resume it, and the async
    /// call's own promise is left in the accumulator for the caller.
    fn suspend_await(&mut self, value: Value) -> Result<(), Completion> {
        let frame = self.frames[self.depth as usize - 1];
        let coroutine = self.capture_frame(&frame, frame.promise)?;
        // The awaited value becomes a promise, and settling it resumes the
        // frame with the value or the reason.
        let inner = self.promise_for(value)?;
        let on_fulfilled = self.settle_function(native::ASYNC_RESUME_FULFILLED, coroutine)?;
        let on_rejected = self.settle_function(native::ASYNC_RESUME_REJECTED, coroutine)?;
        let Some(queue) = self.queue.as_deref_mut() else {
            return Err(Completion::Terminated(Termination::NotImplemented));
        };
        promise::react(
            self.heap,
            queue,
            inner,
            on_fulfilled,
            on_rejected,
            Value::UNDEFINED,
        )
        .map_err(|_| Completion::Terminated(Termination::QuotaExceeded))?;
        self.depth -= 1;
        self.top = frame.base;
        self.sync_realm();
        self.accumulator = frame.promise;
        Ok(())
    }

    /// The promise an awaited value stands for — the specification's
    /// PromiseResolve. A promise whose `constructor` is `Promise` is awaited
    /// directly, one tick and no detour through a patched `then`; anything
    /// else resolves a fresh promise, a thenable being followed.
    fn promise_for(&mut self, value: Value) -> Result<Handle, Completion> {
        if value.is_object() && object::is_promise(self.heap, value.as_handle()).unwrap_or(false) {
            let key = self.ascii_key(b"constructor")?;
            let constructor = self.get_property(value, key)?;
            if constructor.is_object() && constructor.as_handle() == self.realm.promise_constructor
            {
                return Ok(value.as_handle());
            }
        }
        let inner = self.new_promise()?;
        self.resolve(inner, value)?;
        Ok(inner)
    }

    /// Move a frame's state to the heap: its registers, position, and
    /// environment, with `keeper` in its promise slot — the async call's
    /// promise, or the generator the frame belongs to.
    fn capture_frame(&mut self, frame: &Frame, keeper: Value) -> Result<Handle, Completion> {
        let coroutine = object::create(self.heap, Value::NULL)
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        let registers = self.create_array()?;
        let mut index = 0u32;
        while index < frame.registers {
            let held = self.register(frame, index);
            self.set_element(registers, index, held)?;
            index += 1;
        }
        self.set_length(registers, frame.registers)?;
        for (name, held) in [
            (&b"code"[..], Value::number(f64::from(frame.code))),
            (&b"pc"[..], Value::number(f64::from(frame.pc))),
            (&b"module"[..], Value::number(f64::from(frame.module))),
            (&b"contexts"[..], Value::number(f64::from(frame.contexts))),
            (&b"argc"[..], Value::number(f64::from(frame.argument_count))),
            (&b"env"[..], frame.environment),
            (&b"this"[..], frame.this),
            (&b"callee"[..], frame.callee),
            (&b"promise"[..], keeper),
            (&b"regs"[..], registers),
        ] {
            let key = self.ascii_key(name)?;
            object::define_own_property(
                self.heap,
                coroutine,
                key,
                Descriptor::data(held, attribute::WRITABLE),
            )
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        }
        Ok(coroutine)
    }

    /// Suspend the top frame at a `yield`: the generator keeps the frame,
    /// and the yielded value is what the resumer receives.
    /// Whether a captured coroutine is suspended at a delegating `yield*`,
    /// which is what lets `return` and `throw` resume it for forwarding.
    fn coroutine_is_star(&mut self, coroutine: Value) -> Result<bool, Completion> {
        if !coroutine.is_object() {
            return Ok(false);
        }
        let star_key = self.ascii_key(b"star")?;
        let star = self.get_property(coroutine, star_key)?;
        Ok(matches!(star.tag(), Tag::Boolean) && star.as_boolean())
    }

    /// Suspend a delegating sync yield: star-marked for kind dispatch, and
    /// delegate-marked so the resumer hands the value through untouched.
    /// Whether a captured coroutine is a delegated sync yield.
    fn coroutine_is_delegate(&mut self, coroutine: Value) -> Result<bool, Completion> {
        if !coroutine.is_object() {
            return Ok(false);
        }
        let delegate_key = self.ascii_key(b"delegate")?;
        let held = self.get_property(coroutine, delegate_key)?;
        Ok(matches!(held.tag(), Tag::Boolean) && held.as_boolean())
    }

    fn suspend_yield_delegate(&mut self) -> Result<(), Completion> {
        self.suspend_yield_inner(true, true)
    }

    fn suspend_yield(&mut self, star: bool) -> Result<(), Completion> {
        self.suspend_yield_inner(star, false)
    }

    fn suspend_yield_inner(&mut self, star: bool, delegate: bool) -> Result<(), Completion> {
        let frame = self.frames[self.depth as usize - 1];
        let generator = frame.promise;
        if !generator.is_object() {
            return Err(Completion::Terminated(Termination::Malformed));
        }
        let async_bit = self.generator_async_bit(generator);
        let coroutine = self.capture_frame(&frame, generator)?;
        if star {
            let star_key = self.ascii_key(b"star")?;
            object::define_own_property(
                self.heap,
                coroutine,
                star_key,
                Descriptor::data(Value::boolean(true), attribute::WRITABLE),
            )
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        }
        if delegate {
            let delegate_key = self.ascii_key(b"delegate")?;
            object::define_own_property(
                self.heap,
                coroutine,
                delegate_key,
                Descriptor::data(Value::boolean(true), attribute::WRITABLE),
            )
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        }
        object::set_generator(
            self.heap,
            generator.as_handle(),
            object::generator_state::SUSPENDED | async_bit,
            Value::object(coroutine),
        )
        .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        self.depth -= 1;
        self.top = frame.base;
        self.sync_realm();
        Ok(())
    }

    /// The async bit a generator carries, preserved across state moves.
    fn generator_async_bit(&self, generator: Value) -> u8 {
        if !generator.is_object() {
            return 0;
        }
        match object::generator(self.heap, generator.as_handle()) {
            Ok(Some((state, _))) => state & object::generator_state::ASYNC,
            _ => 0,
        }
    }

    /// An async generator's yielded value settles its pending `next` only
    /// once the value itself settles: a fulfilled value is the result, a
    /// rejection finishes the generator with it.
    /// Answer an async generator's `return` request once its value has been
    /// awaited — what the specification calls AsyncGeneratorAwaitReturn. The
    /// generator counts as running meanwhile, so later requests queue.
    fn await_return(&mut self, generator: Value, value: Value) -> Result<(), Completion> {
        let Some(handle) = generator.is_object().then(|| generator.as_handle()) else {
            return Ok(());
        };
        let async_bit = self.generator_async_bit(generator);
        let _ = object::set_generator(
            self.heap,
            handle,
            object::generator_state::RUNNING | async_bit,
            Value::UNDEFINED,
        );
        let inner = match self.promise_for(value) {
            Ok(inner) => inner,
            Err(Completion::Throw(reason)) => {
                // A `constructor` that throws rejects the request.
                let _ = object::set_generator(
                    self.heap,
                    handle,
                    object::generator_state::DONE | async_bit,
                    Value::UNDEFINED,
                );
                return self.settle_pending_next(generator, Err(reason), true);
            }
            Err(other) => return Err(other),
        };
        let on_ok = self.settle_function(native::ASYNC_GEN_RETURN_FULFILLED, handle)?;
        let on_err = self.settle_function(native::ASYNC_GEN_RETURN_REJECTED, handle)?;
        let Some(queue) = self.queue.as_deref_mut() else {
            return Err(Completion::Terminated(Termination::NotImplemented));
        };
        promise::react(self.heap, queue, inner, on_ok, on_err, Value::UNDEFINED)
            .map_err(|_| Completion::Terminated(Termination::QuotaExceeded))
    }

    /// Serve the request at the head of an async generator's queue — next,
    /// throw, or return — as the generator's state allows: resuming it at a
    /// yield, completing one that has not started, or answering one that has
    /// finished, a `return` awaiting its value first.
    fn serve_async_request(
        &mut self,
        generator: Value,
        raw_state: u8,
        coroutine: Value,
        operation: u32,
        argument: Value,
    ) -> Result<(), Completion> {
        let handle = generator.as_handle();
        let async_bit = raw_state & object::generator_state::ASYNC;
        let state = raw_state & !object::generator_state::ASYNC;
        if state == object::generator_state::RUNNING {
            // Mid-run: the request waits its turn in the queue.
            return Ok(());
        }
        let at_yield = state == object::generator_state::SUSPENDED
            && coroutine.is_object()
            && self.coroutine_is_star(coroutine)?;
        if operation == native::GENERATOR_RETURN && !at_yield {
            return self.await_return(generator, argument);
        }
        if !at_yield && operation == native::GENERATOR_THROW {
            // Not started, or finished: the throw is the answer.
            let _ = object::set_generator(
                self.heap,
                handle,
                object::generator_state::DONE | async_bit,
                Value::UNDEFINED,
            );
            return self.settle_pending_next(generator, Err(argument), true);
        }
        if state != object::generator_state::SUSPENDED || !coroutine.is_object() {
            // A finished generator answers done forever.
            return self.settle_pending_next(generator, Ok(Value::UNDEFINED), true);
        }
        let _ = object::set_generator(
            self.heap,
            handle,
            object::generator_state::RUNNING | async_bit,
            Value::UNDEFINED,
        );
        let kind = if operation == native::GENERATOR_THROW {
            resume::THROW
        } else if operation == native::GENERATOR_RETURN {
            resume::RETURN
        } else {
            resume::NEXT
        };
        match self.resume_coroutine(coroutine, argument, kind) {
            Ok(_) => Ok(()),
            Err(Completion::Throw(reason)) => {
                self.settle_pending_next(generator, Err(reason), true)
            }
            Err(other) => Err(other),
        }
    }

    /// Settle the promise an async generator's pending `next` is waiting on,
    /// if one waits.
    fn settle_pending_next(
        &mut self,
        generator: Value,
        outcome: Result<Value, Value>,
        done: bool,
    ) -> Result<(), Completion> {
        self.settle_pending_next_with(generator, outcome, done, true)
    }

    /// The request at the head of an async generator's queue, if one waits.
    fn head_request(&mut self, generator: Value) -> Result<Option<(Value, u32)>, Completion> {
        if !generator.is_object() {
            return Ok(None);
        }
        let key = self.ascii_key(b"\0next")?;
        let queue_value = object::get_own_property(self.heap, generator.as_handle(), key)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?
            .map_or(Value::UNDEFINED, |descriptor| descriptor.value);
        if !queue_value.is_object() {
            return Ok(None);
        }
        let length_key = self.ascii_key(b"length")?;
        if self.get_property(queue_value, length_key)?.as_number() < 1.0 {
            return Ok(None);
        }
        let head = self.get_property(queue_value, Key::Index(0))?;
        self.pending_request(head).map(Some)
    }

    /// As `settle_pending_next`; `chain` says whether a request left waiting
    /// gets a job to resume the generator, which a caller that goes on
    /// serving the queue itself declines.
    fn settle_pending_next_with(
        &mut self,
        generator: Value,
        outcome: Result<Value, Value>,
        done: bool,
        chain: bool,
    ) -> Result<(), Completion> {
        if !generator.is_object() {
            return Ok(());
        }
        let key = self.ascii_key(b"\0next")?;
        let held = object::get_own_property(self.heap, generator.as_handle(), key)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        let Some(descriptor) = held else {
            return Ok(());
        };
        let queue = descriptor.value;
        if !queue.is_object() {
            return Ok(());
        }
        // The oldest request answers first: requests queue while the
        // generator works, and each settles in turn.
        let length_key = self.ascii_key(b"length")?;
        let count = self.get_property(queue, length_key)?.as_number() as u32;
        if count == 0 {
            return Ok(());
        }
        let pending = self.get_property(queue, Key::Index(0))?;
        let mut index = 1u32;
        while index < count {
            let shifted = self.get_property(queue, Key::Index(index))?;
            self.set_element(queue, index - 1, shifted)?;
            index += 1;
        }
        self.delete_element(queue, count - 1)?;
        self.set_length(queue, count - 1)?;
        if !pending.is_object() {
            return Ok(());
        }
        match outcome {
            Ok(value) => {
                let result = self.iteration_result(value, done)?;
                self.resolve(pending.as_handle(), result)?;
            }
            Err(reason) => {
                self.settle(pending.as_handle(), promise::REJECTED, reason)?;
            }
        }
        if count > 1 && chain {
            // Another request waits: a job resumes the generator for it once
            // the current turn's work is out of the way.
            let drain = self.settle_function(native::ASYNC_GEN_DRAIN, generator.as_handle())?;
            let job = crate::job::Job {
                kind: crate::job::JobKind::Reaction,
                target: drain,
                argument: Value::UNDEFINED,
                derived: Value::UNDEFINED,
            };
            let Some(queue) = self.queue.as_deref_mut() else {
                return Err(Completion::Terminated(Termination::NotImplemented));
            };
            queue
                .push(job)
                .map_err(|_| Completion::Terminated(Termination::QuotaExceeded))?;
        }
        Ok(())
    }

    /// How many `next` requests wait on an async generator.
    fn pending_next_count(&mut self, generator: Handle) -> Result<u32, Completion> {
        let key = self.ascii_key(b"\0next")?;
        let held = object::get_own_property(self.heap, generator, key)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        let Some(descriptor) = held else {
            return Ok(0);
        };
        if !descriptor.value.is_object() {
            return Ok(0);
        }
        let length_key = self.ascii_key(b"length")?;
        Ok(self.get_property(descriptor.value, length_key)?.as_number() as u32)
    }

    /// Resume an async generator for the oldest queued request, called from
    /// the job the previous turn's settlement queued.
    fn drain_async_generator(&mut self, generator: Value) -> Result<(), Completion> {
        if !generator.is_object() {
            return Ok(());
        }
        let handle = generator.as_handle();
        if self.pending_next_count(handle)? == 0 {
            return Ok(());
        }
        let Some((raw_state, coroutine)) = object::generator(self.heap, handle)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?
        else {
            return Ok(());
        };
        let Some((argument, operation)) = self.head_request(generator)? else {
            return Ok(());
        };
        self.serve_async_request(generator, raw_state, coroutine, operation, argument)
    }

    /// What a queued request asked for: its argument and its operation.
    fn pending_request(&mut self, pending: Value) -> Result<(Value, u32), Completion> {
        if !pending.is_object() {
            return Ok((Value::UNDEFINED, native::GENERATOR_NEXT));
        }
        let arg_key = self.ascii_key(b"\0arg")?;
        let kind_key = self.ascii_key(b"\0kind")?;
        let argument = object::get_own_property(self.heap, pending.as_handle(), arg_key)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?
            .map_or(Value::UNDEFINED, |descriptor| descriptor.value);
        let kind = object::get_own_property(self.heap, pending.as_handle(), kind_key)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?
            .map_or(Value::UNDEFINED, |descriptor| descriptor.value);
        let operation = if matches!(kind.tag(), Tag::Number) {
            kind.as_number() as u32
        } else {
            native::GENERATOR_NEXT
        };
        Ok((argument, operation))
    }

    /// Append one pending `next` promise to an async generator's request
    /// queue, making the queue if this is the first.
    fn push_pending_next(&mut self, generator: Handle, pending: Value) -> Result<(), Completion> {
        let key = self.ascii_key(b"\0next")?;
        let held = object::get_own_property(self.heap, generator, key)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        let queue = match held {
            Some(descriptor) if descriptor.value.is_object() => descriptor.value,
            _ => {
                let queue = self.create_array()?;
                object::define_own_property(
                    self.heap,
                    generator,
                    key,
                    Descriptor::data(queue, attribute::WRITABLE),
                )
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                queue
            }
        };
        self.append_element(queue, Some(pending))?;
        Ok(())
    }

    /// Make a generator object over the top frame, which was just pushed and
    /// given its arguments and has not run: the call's answer.
    fn suspend_start(&mut self) -> Result<Value, Completion> {
        let frame = self.frames[self.depth as usize - 1];
        // The instance's prototype is the function's own `prototype` when
        // that is an object, and the shared generator prototype otherwise.
        let record_flags = self
            .unit_of(frame.module)
            .function(frame.code)
            .map_or(0, |record| record.flags);
        let mut instance_prototype = if record_flags & record_flag::ASYNC != 0 {
            Value::object(self.realm.async_generator_object_prototype)
        } else {
            Value::object(self.realm.generator_object_prototype)
        };
        if frame.callee.is_object() {
            let prototype_key = self.ascii_key(b"prototype")?;
            let named = self.get_property(frame.callee, prototype_key)?;
            if named.is_object() {
                instance_prototype = named;
            }
        }
        let generator = object::create_generator(self.heap, instance_prototype, Value::UNDEFINED)
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        let asynchronous = self
            .unit_of(frame.module)
            .function(frame.code)
            .is_some_and(|record| record.flags & record_flag::ASYNC != 0);
        let async_bit = if asynchronous {
            object::generator_state::ASYNC
        } else {
            0
        };
        let coroutine = self.capture_frame(&frame, Value::object(generator))?;
        object::set_generator(
            self.heap,
            generator,
            object::generator_state::SUSPENDED | async_bit,
            Value::object(coroutine),
        )
        .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        self.depth -= 1;
        self.top = frame.base;
        self.sync_realm();
        // The generator answers the iteration protocol through its own
        // methods, so no prototype work is needed anywhere else.
        for (name, id) in [
            (&b"next"[..], native::GENERATOR_NEXT),
            (&b"return"[..], native::GENERATOR_RETURN),
            (&b"throw"[..], native::GENERATOR_THROW),
        ] {
            let method = self.settle_function(id, generator)?;
            let key = self.ascii_key(name)?;
            object::define_own_property(
                self.heap,
                generator,
                key,
                Descriptor::data(method, attribute::WRITABLE | attribute::CONFIGURABLE),
            )
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        }
        let this_iterator = object::create_native(
            self.heap,
            Value::object(self.realm.function_prototype),
            native::ITERATOR_SELF,
            0,
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        for key in [
            Key::Symbol(self.realm.iterator_symbol),
            Key::Symbol(self.realm.async_iterator_symbol),
        ] {
            object::define_own_property(
                self.heap,
                generator,
                key,
                Descriptor::data(
                    Value::object(this_iterator),
                    attribute::WRITABLE | attribute::CONFIGURABLE,
                ),
            )
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        }
        Ok(Value::object(generator))
    }

    /// Resume, finish, or throw into a generator, answering the iteration
    /// result its resumer receives.
    fn generator_resume(
        &mut self,
        generator: Value,
        operation: u32,
        argument: Value,
    ) -> Result<Value, Completion> {
        if !generator.is_object() {
            return Err(self.throw_type_error());
        }
        let handle = generator.as_handle();
        let Some((raw_state, coroutine)) = object::generator(self.heap, handle)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?
        else {
            return Err(self.throw_type_error());
        };
        let async_bit = raw_state & object::generator_state::ASYNC;
        let state = raw_state & !object::generator_state::ASYNC;
        if async_bit != 0 {
            // An async generator answers a promise; the run settles it, at
            // once or after an await.
            let pending = self.new_promise()?;
            for (name, held) in [
                (&b"\0arg"[..], argument),
                (&b"\0kind"[..], Value::number(f64::from(operation))),
            ] {
                let key = self.ascii_key(name)?;
                object::define_own_property(
                    self.heap,
                    pending,
                    key,
                    Descriptor::data(held, attribute::WRITABLE),
                )
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
            }
            self.push_pending_next(handle, Value::object(pending))?;
            if self.pending_next_count(handle)? > 1 {
                // An older request is still working; this one waits.
                return Ok(Value::object(pending));
            }
            self.serve_async_request(generator, raw_state, coroutine, operation, argument)?;
            return Ok(Value::object(pending));
        }
        if operation == native::GENERATOR_RETURN
            && !(state == object::generator_state::SUSPENDED
                && self.coroutine_is_star(coroutine)?)
        {
            let _ = object::set_generator(
                self.heap,
                handle,
                object::generator_state::DONE,
                Value::UNDEFINED,
            );
            return self.iteration_result(argument, true);
        }
        if state != object::generator_state::SUSPENDED || !coroutine.is_object() {
            if operation == native::GENERATOR_THROW {
                return Err(Completion::Throw(argument));
            }
            // A finished generator answers done forever.
            return self.iteration_result(Value::UNDEFINED, true);
        }
        let _ = object::set_generator(
            self.heap,
            handle,
            object::generator_state::RUNNING,
            Value::UNDEFINED,
        );
        let value = self.resume_coroutine(
            coroutine,
            argument,
            if operation == native::GENERATOR_THROW {
                resume::THROW
            } else if operation == native::GENERATOR_RETURN {
                resume::RETURN
            } else {
                resume::NEXT
            },
        )?;
        let Some((after, after_coroutine)) = object::generator(self.heap, handle)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?
        else {
            return Err(Completion::Terminated(Termination::Malformed));
        };
        if after == object::generator_state::SUSPENDED
            && self.coroutine_is_delegate(after_coroutine)?
        {
            // The delegation hands the inner iterator's own result object
            // through, identity and unread value alike.
            return Ok(value);
        }
        self.iteration_result(value, after != object::generator_state::SUSPENDED)
    }

    /// Run the instance fields a class constructor carries: each key takes
    /// its initialiser's value — with `this` bound — as an own property of
    /// the instance, in order.
    fn run_class_fields(&mut self, constructor: Value, this: Value) -> Result<(), Completion> {
        if !constructor.is_object() {
            return Ok(());
        }
        let key = self.ascii_key(b"\0fields")?;
        let held = object::get_own_property(self.heap, constructor.as_handle(), key)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        let Some(descriptor) = held else {
            return Ok(());
        };
        let fields = descriptor.value;
        if !fields.is_object() {
            return Ok(());
        }
        let length_key = self.ascii_key(b"length")?;
        let count = self.get_property(fields, length_key)?.as_number() as u32;
        let mut index = 0u32;
        while index < count {
            let name = self.get_property(fields, Key::Index(index))?;
            let init = self.get_property(fields, Key::Index(index + 1))?;
            let value = if self.is_callable_value(init) {
                self.call_value(init, this, &[])?
            } else {
                Value::UNDEFINED
            };
            self.define_field(this, name, value)?;
            index += 2;
        }
        Ok(())
    }

    /// Define one evaluated instance field on `this`, hiding a private name
    /// from enumeration and refusing an instance that cannot take it.
    fn define_field(&mut self, this: Value, name: Value, value: Value) -> Result<(), Completion> {
        let property = self.coerce_to_key(name)?;
        let private = if let Key::Name(handle) = property {
            crate::string::unit_at(self.heap, handle, 0) == Ok(Some(0))
        } else {
            false
        };
        let attributes = if private {
            attribute::WRITABLE | attribute::CONFIGURABLE
        } else {
            attribute::DEFAULT
        };
        if this.is_object() {
            if !private
                && object::exotic_kind(self.heap, this.as_handle()).unwrap_or(0)
                    == object::exotic::PROXY
            {
                // A field on a proxy goes through its defineProperty trap.
                if !self.proxy_define(this, property, value, attributes)? {
                    return Err(self.throw_type_error());
                }
                return Ok(());
            }
            // A public field lands through [[DefineOwnProperty]], which is
            // a meaningful use of a deferred namespace.
            if !private
                && object::exotic_kind(self.heap, this.as_handle()).unwrap_or(0)
                    == object::exotic::DEFERRED
            {
                self.deferred_trigger(this, Some(property))?;
            }
            let admitted = object::define_own_property(
                self.heap,
                this.as_handle(),
                property,
                Descriptor::data(value, attributes),
            )
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
            if !admitted {
                // CreateDataPropertyOrThrow: a frozen or non-extensible
                // instance refuses the field.
                return Err(self.throw_type_error());
            }
        }
        Ok(())
    }

    /// One `{value, done}` iteration result.
    fn iteration_result(&mut self, value: Value, done: bool) -> Result<Value, Completion> {
        let result = object::create(self.heap, Value::object(self.realm.object_prototype))
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        let value_key = self.ascii_key(b"value")?;
        let done_key = self.ascii_key(b"done")?;
        object::define_own_property(
            self.heap,
            result,
            value_key,
            Descriptor::data(value, attribute::DEFAULT),
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        object::define_own_property(
            self.heap,
            result,
            done_key,
            Descriptor::data(Value::boolean(done), attribute::DEFAULT),
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(Value::object(result))
    }

    /// Resume a suspended async frame with a settled value, running it until
    /// it finishes or suspends again.
    fn resume_coroutine(
        &mut self,
        coroutine: Value,
        value: Value,
        kind: u8,
    ) -> Result<Value, Completion> {
        if let Some(completion) = self.restore_coroutine(coroutine, value, kind, false)? {
            return match completion {
                Completion::Value(answer) => Ok(answer),
                other => Err(other),
            };
        }
        let before = self.fuel;
        self.nested += 1;
        let completion = self.execute();
        self.nested -= 1;
        let spent = before.saturating_sub(self.fuel);
        self.slice = self.slice.saturating_sub(spent);
        match completion {
            Completion::Value(answer) => Ok(answer),
            other => Err(other),
        }
    }

    /// Put a captured frame back on the stack, resumed with `value` in the
    /// given way. Answers a completion only when a `throw` resumption found
    /// no handler in the frame, which then unwound; `direct` marks a frame
    /// this loop runs in place of the call that woke it.
    fn restore_coroutine(
        &mut self,
        coroutine: Value,
        value: Value,
        kind: u8,
        direct: bool,
    ) -> Result<Option<Completion>, Completion> {
        if !coroutine.is_object() {
            return Err(Completion::Terminated(Termination::Malformed));
        }
        let handle = coroutine.as_handle();
        let mut scalar = [0u32; 5];
        for (index, name) in [
            (&b"code"[..]),
            (&b"pc"[..]),
            (&b"module"[..]),
            (&b"contexts"[..]),
            (&b"argc"[..]),
        ]
        .into_iter()
        .enumerate()
        {
            let key = self.ascii_key(name)?;
            let held = self.get_property(coroutine, key)?;
            scalar[index] = held.as_number() as u32;
        }
        let env_key = self.ascii_key(b"env")?;
        let this_key = self.ascii_key(b"this")?;
        let callee_key = self.ascii_key(b"callee")?;
        let promise_key = self.ascii_key(b"promise")?;
        let regs_key = self.ascii_key(b"regs")?;
        let environment = self.get_property(coroutine, env_key)?;
        let this = self.get_property(coroutine, this_key)?;
        let callee = self.get_property(coroutine, callee_key)?;
        let promise = self.get_property(coroutine, promise_key)?;
        let registers = self.get_property(coroutine, regs_key)?;
        let _ = handle;
        if self.depth as usize >= self.frames.len() {
            return Err(Completion::Terminated(Termination::StackOverflow));
        }
        if self.nested >= MAX_NESTED_ENTRIES {
            return Err(Completion::Terminated(Termination::StackOverflow));
        }
        let length_key = self.ascii_key(b"length")?;
        let count = self.get_property(registers, length_key)?.as_number() as u32;
        let base = self.top;
        let end = base
            .checked_add(count)
            .ok_or(Completion::Terminated(Termination::RegistersExhausted))?;
        if end as usize > self.registers.len() {
            return Err(Completion::Terminated(Termination::RegistersExhausted));
        }
        let mut index = 0u32;
        while index < count {
            let key = Key::Index(index);
            let held = self.get_property(registers, key)?;
            if let Some(slot) = self.registers.get_mut((base + index) as usize) {
                *slot = held;
            }
            index += 1;
        }
        self.frames[self.depth as usize] = Frame {
            code: scalar[0],
            pc: scalar[1],
            base,
            registers: count,
            environment,
            this,
            callee,
            contexts: scalar[3],
            module: scalar[2],
            construct: false,
            argument_count: scalar[4],
            promise,
            this_pending: false,
            resume_kind: 0,
            direct_resume: direct,
        };
        self.depth += 1;
        self.top = end;
        self.sync_realm();
        let star_key = self.ascii_key(b"star")?;
        let star = self.get_property(coroutine, star_key)?;
        let star = matches!(star.tag(), Tag::Boolean) && star.as_boolean();
        if star {
            // A delegating yield reads the kind itself and forwards it to
            // the inner iterator, so nothing is thrown or returned here.
            if let Some(running) = self.frames.get_mut(self.depth as usize - 1) {
                running.resume_kind = kind;
            }
            self.accumulator = value;
        } else if kind == resume::THROW {
            // The rejection is thrown at the await, into whatever handler the
            // function wrote around it — or out through its promise.
            let floor = self.depth;
            if let Some(completion) = self.unwind(value, floor) {
                return Ok(Some(completion));
            }
        } else {
            self.accumulator = value;
        }
        Ok(None)
    }

    /// Call a native parent constructor for `super()`, with `this` already
    /// made: what a derived class over a library base does.
    /// Answers the instance the constructor continues with: a library base
    /// builds it — internal slots and all — so the parent is constructed
    /// and the eagerly made `this` is put aside.
    fn super_call_native(
        &mut self,
        parent: Value,
        this: Value,
        arguments: &[Value],
        subclass: Value,
    ) -> Result<Value, Completion> {
        if !parent.is_object() {
            return Err(self.throw_type_error());
        }
        let handle = parent.as_handle();
        if object::is_callable(self.heap, handle) != Ok(true) {
            return Err(self.throw_type_error());
        }
        let native = object::function_code(self.heap, handle).unwrap_or(u32::MAX);
        if native == native::DEFAULT_CONSTRUCTOR {
            // The default derived constructor forwards to ITS parent, and
            // either way the parent class's instance fields run on `this`.
            let flags = object::function_flags(self.heap, handle).unwrap_or(0);
            if flags & object::function_flag::DERIVED != 0 {
                let grandparent = object::prototype(self.heap, handle)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                self.super_call_native_or_enterless(grandparent, this, arguments)?;
            }
            self.run_class_fields(parent, this)?;
            return Ok(this);
        }
        let made = self.construct(parent, arguments)?;
        if made.is_object() && subclass.is_object() {
            // The instance answers to the subclass: its prototype is the
            // subclass's, exactly as construction under new.target makes it.
            let key = self.ascii_key(b"prototype")?;
            let proto = self.get_property(subclass, key)?;
            if proto.is_object() {
                let _ = object::set_prototype(self.heap, made.as_handle(), proto);
            }
        }
        Ok(made)
    }

    /// `super()` into a parent that may be bytecode, from a place that
    /// cannot enter a frame: run it as a nested call.
    fn super_call_native_or_enterless(
        &mut self,
        parent: Value,
        this: Value,
        arguments: &[Value],
    ) -> Result<(), Completion> {
        if parent.is_object()
            && object::is_callable(self.heap, parent.as_handle()) == Ok(true)
            && !object::is_native(self.heap, parent.as_handle()).unwrap_or(false)
        {
            return self
                .call_constructor_value(parent, this, arguments)
                .map(|_| ());
        }
        self.super_call_native(parent, this, arguments, Value::UNDEFINED)
            .map(|_| ())
    }

    /// Call a bytecode constructor with `this` already made, as a nested
    /// execution: `call_value` without the only-via-new refusal.
    fn call_constructor_value(
        &mut self,
        callee: Value,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let function = callee.as_handle();
        let code = object::function_code(self.heap, function)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        let closure = object::function_environment(self.heap, function)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        if let Some(stop) = self.check_control() {
            return Err(stop);
        }
        let module = object::function_module(self.heap, function).unwrap_or(0);
        if self.nested >= MAX_NESTED_ENTRIES {
            return Err(Completion::Terminated(Termination::StackOverflow));
        }
        let environment = self.prepare_call_environment(closure, this, code, module)?;
        self.bind_new_target(environment, true, callee)?;
        self.push_frame(code, environment, this, callee, module)?;
        let frame = self.frames[self.depth as usize - 1];
        let mut index = 0usize;
        while index < arguments.len() {
            let register = u32::try_from(index).unwrap_or(0);
            if register >= frame.registers {
                break;
            }
            self.set_register(&frame, register, arguments[index]);
            index += 1;
        }
        self.frames[self.depth as usize - 1].argument_count = u32::try_from(index).unwrap_or(0);
        self.frames[self.depth as usize - 1].construct = true;
        let before = self.fuel;
        self.nested += 1;
        let completion = self.execute();
        self.nested -= 1;
        let spent = before.saturating_sub(self.fuel);
        self.slice = self.slice.saturating_sub(spent);
        match completion {
            Completion::Value(value) => Ok(value),
            other => Err(other),
        }
    }

    /// Create a pending promise with the realm's prototype.
    /// `import(specifier)`: a promise of the module's namespace, resolved
    /// against the closure the loader staged. `import.defer` answers the
    /// deferred namespace without running anything. Whatever goes wrong —
    /// a coercion that throws, a name the closure does not hold, a body
    /// that throws — lands on the promise, never on the caller.
    fn dynamic_import(
        &mut self,
        specifier: Value,
        options: Value,
        deferred: bool,
    ) -> Result<Value, Completion> {
        let promise = self.new_promise()?;
        match self.dynamic_import_inner(specifier, options, deferred, promise) {
            Ok(()) => {}
            Err(Completion::Throw(reason)) => {
                self.settle(promise, promise::REJECTED, reason)?;
            }
            Err(other) => return Err(other),
        }
        Ok(Value::object(promise))
    }

    fn dynamic_import_inner(
        &mut self,
        specifier: Value,
        options: Value,
        deferred: bool,
        promise: Handle,
    ) -> Result<(), Completion> {
        let text = self.coerce_to_string(specifier)?;
        // The second argument is inspected on the promise's behalf: not an
        // object, an attribute that is no string, an unknown attribute, or
        // a type no loader here reads — each rejects with a TypeError. A
        // known type picks the staged variant of the module.
        let mut marker = 0u8;
        if !options.is_undefined() {
            if !options.is_object() {
                return Err(self.throw_type_error());
            }
            let with_key = self.ascii_key(b"with")?;
            let with = self.get_property(options, with_key)?;
            if !with.is_undefined() {
                if !with.is_object() {
                    return Err(self.throw_type_error());
                }
                let type_key = self.ascii_key(b"type")?;
                let proxied = object::exotic_kind(self.heap, with.as_handle()).unwrap_or(0)
                    == object::exotic::PROXY;
                let mut keys = [Key::Index(0); MAX_OWN_KEYS];
                let count = if proxied {
                    self.proxy_own_keys(with, &mut keys)?
                } else {
                    object::own_keys(self.heap, with.as_handle(), &mut keys)
                        .map_err(Self::key_failure)?
                };
                for &key in keys.get(..count).unwrap_or(&[]) {
                    if matches!(key, Key::Symbol(_)) || self.hidden_key(key) {
                        continue;
                    }
                    if proxied {
                        // The proxy answers for its own keys: the trap's
                        // descriptor decides enumerability, and its getter
                        // failures are the import's rejection.
                        let descriptor = self.proxy_own_descriptor(with, key)?;
                        if descriptor.is_undefined() {
                            continue;
                        }
                        let enumerable_key = self.ascii_key(b"enumerable")?;
                        let enumerable = self.get_property(descriptor, enumerable_key)?;
                        if !self.coerce_to_boolean(enumerable)? {
                            continue;
                        }
                    } else if !self.is_enumerable(with, key)? {
                        continue;
                    }
                    let value = self.get_property(with, key)?;
                    if !value.is_string() {
                        return Err(self.throw_type_error());
                    }
                    if key != type_key {
                        return Err(self.throw_type_error());
                    }
                    let mut units = [0u16; 8];
                    let length =
                        crate::string::copy_units(self.heap, value.as_handle(), &mut units)
                            .unwrap_or(usize::MAX);
                    marker = match units.get(..length.min(8)) {
                        Some(held) if held == b"json".map(u16::from) => b'j',
                        Some(held) if held == b"text".map(u16::from) => b't',
                        Some(held) if held == b"bytes".map(u16::from) => b'b',
                        _ => return Err(self.throw_type_error()),
                    };
                }
            }
        }
        let mut units16 = [0u16; 128];
        let length = crate::string::copy_units(self.heap, text.as_handle(), &mut units16)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        let module = self.resolve_module_name(units16.get(..length).unwrap_or(&[]), marker);
        let Some(module) = module else {
            let reason = self.create_error(ErrorKind::Type, Value::UNDEFINED)?;
            self.settle(promise, promise::REJECTED, reason)?;
            return Ok(());
        };
        if deferred {
            // `import.defer` runs nothing sync — but what awaits in the
            // module's graph is pre-evaluated, and the promise waits for it.
            let namespace = self.deferred_namespace_of(module)?;
            let mut pending = Value::UNDEFINED;
            let mut owner = module;
            let mut seen = [u32::MAX; MAX_UNIT_REALMS];
            let mut count = 0usize;
            self.evaluate_async_reachable(module, &mut seen, &mut count, &mut pending, &mut owner)?;
            if pending.is_object() {
                self.chain_namespace(promise, pending, module, owner, true)?;
            } else {
                self.settle(promise, promise::FULFILLED, namespace)?;
            }
            return Ok(());
        }
        match self.module_status(module) {
            2 => {
                let namespace = self.namespace_of(module)?;
                self.settle(promise, promise::FULFILLED, namespace)?;
                return Ok(());
            }
            3 => {
                let reason = self.module_completion(module);
                self.settle(promise, promise::REJECTED, reason)?;
                return Ok(());
            }
            6 => return Err(self.throw_error_of(ErrorKind::Syntax)),
            0 => {
                self.deferred_ready(module, false)?;
                let mut gate = Value::UNDEFINED;
                self.evaluate_module_gated(module, false, &mut gate)?;
                if gate.is_object() {
                    // Everything runnable ran; the rest waits behind this
                    // gate, retried when it settles.
                    self.chain_namespace(promise, gate, module, module, false)?;
                    return Ok(());
                }
                // A dependency that awaited is still in flight: the import
                // settles only when it does, and with its error if it errs.
                let mut pending = Value::UNDEFINED;
                let mut owner = module;
                let mut seen = [u32::MAX; MAX_UNIT_REALMS];
                let mut count = 0usize;
                self.evaluate_async_reachable(
                    module,
                    &mut seen,
                    &mut count,
                    &mut pending,
                    &mut owner,
                )?;
                if pending.is_object() {
                    self.chain_namespace(promise, pending, module, owner, false)?;
                    return Ok(());
                }
            }
            _ => {}
        }
        // Done, or answering a promise of its own: the settled body hands
        // over its namespace; a still-pending one is waited on through its
        // completion promise, and one mid-evaluation settles now — its
        // readers run as reactions, after the body ends.
        if self.module_status(module) == 2 {
            let namespace = self.namespace_of(module)?;
            self.settle(promise, promise::FULFILLED, namespace)?;
            return Ok(());
        }
        let completion = self.module_completion(module);
        if completion.is_object()
            && object::is_promise(self.heap, completion.as_handle()).unwrap_or(false)
        {
            self.chain_namespace(promise, completion, module, module, false)?;
            return Ok(());
        }
        let namespace = self.namespace_of(module)?;
        self.settle(promise, promise::FULFILLED, namespace)?;
        Ok(())
    }

    /// Settle `promise` with the module's namespace once `completion` does.
    /// The completion watched belongs to `watched`, whose cycle takes the
    /// error if it rejects.
    fn chain_namespace(
        &mut self,
        promise: Handle,
        completion: Value,
        module: u32,
        watched: u32,
        deferred: bool,
    ) -> Result<(), Completion> {
        // A deferred chain answers the deferred namespace as soon as its
        // watched completion settles; a plain one steps — another awaiting
        // dependency found on settle is waited out in turn.
        let getter = if deferred {
            let getter = object::create_native(
                self.heap,
                Value::object(self.realm.function_prototype),
                native::NAMESPACE_GET,
                0,
            )
            .map_err(|_| self.heap_failure())?;
            let binding = self.new_array()?;
            self.set_element(
                binding,
                0,
                Value::number(crate::softfloat::from_u64(u64::from(module))),
            )?;
            self.set_element(
                binding,
                1,
                Value::number(crate::softfloat::from_u64(u64::from(u32::MAX))),
            )?;
            self.set_element(binding, 2, Value::number(crate::softfloat::from_u64(4)))?;
            self.set_length(binding, 3)?;
            object::set_bound_value(self.heap, getter, binding).map_err(|_| self.heap_failure())?;
            getter
        } else {
            let getter = object::create_native(
                self.heap,
                Value::object(self.realm.function_prototype),
                native::DYNAMIC_IMPORT_STEP,
                0,
            )
            .map_err(|_| self.heap_failure())?;
            let binding = self.new_array()?;
            self.set_element(
                binding,
                0,
                Value::number(crate::softfloat::from_u64(u64::from(module))),
            )?;
            self.set_length(binding, 1)?;
            object::set_bound_value(self.heap, getter, binding).map_err(|_| self.heap_failure())?;
            getter
        };
        // A rejection marks the module — and its cycle — errored on its
        // way through to the import's promise.
        let rejecter = object::create_native(
            self.heap,
            Value::object(self.realm.function_prototype),
            native::DYNAMIC_IMPORT_REJECTED,
            0,
        )
        .map_err(|_| self.heap_failure())?;
        let held = self.new_array()?;
        self.set_element(
            held,
            0,
            Value::number(crate::softfloat::from_u64(u64::from(watched))),
        )?;
        self.set_length(held, 1)?;
        object::set_bound_value(self.heap, rejecter, held).map_err(|_| self.heap_failure())?;
        let Some(queue) = self.queue.as_deref_mut() else {
            return Err(Completion::Terminated(Termination::NotImplemented));
        };
        promise::react(
            self.heap,
            queue,
            completion.as_handle(),
            Value::object(getter),
            Value::object(rejecter),
            Value::object(promise),
        )
        .map_err(|_| Completion::Terminated(Termination::QuotaExceeded))?;
        Ok(())
    }

    /// Evaluate every module that awaits in a graph, walking every edge,
    /// deferred ones included; the last still-pending completion met is
    /// left in `pending` for the caller to wait on.
    fn evaluate_async_reachable(
        &mut self,
        module: u32,
        seen: &mut [u32; MAX_UNIT_REALMS],
        count: &mut usize,
        pending: &mut Value,
        owner: &mut u32,
    ) -> Result<(), Completion> {
        let mut at = 0usize;
        while at < *count {
            if seen[at] == module {
                return Ok(());
            }
            at += 1;
        }
        if *count >= seen.len() {
            return Ok(());
        }
        seen[*count] = module;
        *count += 1;
        let entry = self.unit_of(module).header().entry_function;
        let flags = self
            .unit_of(module)
            .function(entry)
            .map_or(0, |held| held.flags);
        if flags & crate::bytecode::function_flag::ASYNC != 0 && self.module_status(module) == 0 {
            self.deferred_ready(module, false)?;
            self.evaluate_module_now(module, false)?;
        }
        let completion = self.module_completion(module);
        if completion.is_object()
            && object::is_promise(self.heap, completion.as_handle()).unwrap_or(false)
            && object::promise_state(self.heap, completion.as_handle()) != Ok(promise::FULFILLED)
        {
            *pending = completion;
            *owner = module;
        }
        let base = match &self.modules {
            Some(modules) => modules
                .get(module as usize)
                .map_or(u32::MAX, |instance| instance.import_base),
            None => u32::MAX,
        };
        if base == u32::MAX {
            return Ok(());
        }
        let held = self.unit_of(module).header().import_count;
        let mut import = 0u32;
        while import < held {
            let row = self
                .imports
                .as_ref()
                .and_then(|imports| imports.get((base + import) as usize))
                .copied();
            if let Some((source, _)) = row {
                if source != module {
                    self.evaluate_async_reachable(source, seen, count, pending, owner)?;
                }
            }
            import += 1;
        }
        Ok(())
    }

    /// The unit a specifier names within the staged closure — the staged
    /// variant its type attribute picks, when one does.
    fn resolve_module_name(&self, units: &[u16], marker: u8) -> Option<u32> {
        if units.len() < 3 || units[0] != u16::from(b'.') || units[1] != u16::from(b'/') {
            return None;
        }
        let mut bytes = [0u8; 128];
        let mut at = 0usize;
        for &unit in units.get(2..).unwrap_or(&[]) {
            if unit > 127 || at + 2 >= bytes.len() {
                return None;
            }
            bytes[at] = unit as u8;
            at += 1;
        }
        if marker != 0 {
            bytes[at] = 1;
            bytes[at + 1] = marker;
            at += 2;
        }
        let wanted = bytes.get(..at).unwrap_or(&[]);
        for &(ref held, held_length, unit) in self.module_names {
            if held.get(..held_length) == Some(wanted) {
                return Some(unit);
            }
        }
        None
    }

    fn new_promise(&mut self) -> Result<Handle, Completion> {
        promise::create(self.heap, Value::object(self.realm.promise_prototype))
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))
    }

    /// `Promise.prototype.then`: record the reaction and return the promise
    /// that receives the handler's result.
    fn promise_then(
        &mut self,
        this: Value,
        on_fulfilled: Value,
        on_rejected: Value,
    ) -> Result<Value, Completion> {
        if !this.is_object() || !object::is_promise(self.heap, this.as_handle()).unwrap_or(false) {
            return Err(self.throw_type_error());
        }
        let derived = self.new_promise()?;
        let target = this.as_handle();
        let Some(queue) = self.queue.as_deref_mut() else {
            return Err(Completion::Terminated(Termination::NotImplemented));
        };
        promise::react(
            self.heap,
            queue,
            target,
            on_fulfilled,
            on_rejected,
            Value::object(derived),
        )
        .map_err(|_| Completion::Terminated(Termination::QuotaExceeded))?;
        Ok(Value::object(derived))
    }

    /// `Array.prototype.join`: each element's string, with a separator, where
    /// `null` and `undefined` contribute nothing.
    fn join_array(&mut self, array: Value, separator: Option<Value>) -> Result<Value, Completion> {
        if !array.is_object() {
            return Err(self.throw_type_error());
        }
        let length_key = self.ascii_key(b"length")?;
        let length_value = self.get_property(array, length_key)?;
        let length = value::to_uint32(self.coerce_to_number(length_value)?);
        let separator = match separator {
            Some(value) => value,
            None => self.ascii_string(b",")?,
        };

        let mut result = self.ascii_string(b"")?;
        let mut index = 0u32;
        while index < length {
            if index > 0 {
                result = self.concat_values(result, separator)?;
            }
            let element = self.get_property(array, Key::Index(index))?;
            if !element.is_nullish() {
                let text = self.coerce_to_string(element)?;
                result = self.concat_values(result, text)?;
            }
            index += 1;
        }
        Ok(result)
    }

    fn concat_values(&mut self, left: Value, right: Value) -> Result<Value, Completion> {
        let joined = string::concat(self.heap, left.as_handle(), right.as_handle())
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(Value::string(joined))
    }

    /// Construct with `new`.
    fn construct(&mut self, callee: Value, arguments: &[Value]) -> Result<Value, Completion> {
        let new_target = self.pending_new_target;
        self.pending_new_target = Value::UNDEFINED;
        if !callee.is_object() {
            return Err(self.throw_type_error());
        }
        let function = callee.as_handle();
        if !object::is_constructor(self.heap, function).unwrap_or(false) {
            return Err(self.throw_type_error());
        }

        if object::is_native(self.heap, function).unwrap_or(false) {
            let native = object::function_code(self.heap, function)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            if native == native::BOUND_FUNCTION {
                // The target constructs, with the bound arguments first, for
                // the `new.target` the bound function was given — or for the
                // target itself when it was the one named.
                let record = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                let target = self.element(record, 0)?;
                let bound_count = self.length_of(record)?.saturating_sub(2);
                let mut values = [Value::UNDEFINED; MAX_ARGUMENTS];
                let mut count = 0usize;
                let mut index = 0u32;
                while index < bound_count && count < values.len() {
                    values[count] = self.element(record, 2 + index)?;
                    count += 1;
                    index += 1;
                }
                for &argument in arguments {
                    if count >= values.len() {
                        break;
                    }
                    values[count] = argument;
                    count += 1;
                }
                self.pending_new_target =
                    if new_target.is_undefined() || value::same_value(new_target, callee) {
                        target
                    } else {
                        new_target
                    };
                return self.construct(target, values.get(..count).unwrap_or(&[]));
            }
            if native == native::DEFAULT_CONSTRUCTOR {
                // The written-nothing constructor: the instance, with the
                // arguments forwarded to the parent when the class derives,
                // and the class's instance fields either way.
                let flags = object::function_flags(self.heap, function).unwrap_or(0);
                if flags & object::function_flag::DERIVED != 0 {
                    let parent = object::prototype(self.heap, function)
                        .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                    // A native base makes its own exotic instance — a
                    // promise, an error, an array — which then takes the
                    // subclass's prototype.
                    if parent.is_object()
                        && object::is_native(self.heap, parent.as_handle()).unwrap_or(false)
                        && object::is_constructor(self.heap, parent.as_handle()).unwrap_or(false)
                    {
                        let instance = self.construct(parent, arguments)?;
                        if instance.is_object() {
                            let prototype_key = self.ascii_key(b"prototype")?;
                            let subclass_proto = self.get_property(callee, prototype_key)?;
                            if subclass_proto.is_object() {
                                let _ = object::set_prototype(
                                    self.heap,
                                    instance.as_handle(),
                                    subclass_proto,
                                );
                            }
                        }
                        self.run_class_fields(callee, instance)?;
                        return Ok(instance);
                    }
                    let instance = self.new_instance(callee)?;
                    self.pending_new_target = new_target;
                    self.super_call_native_or_enterless(parent, instance, arguments)?;
                    self.run_class_fields(callee, instance)?;
                    return Ok(instance);
                }
                let instance = self.new_instance(callee)?;
                self.run_class_fields(callee, instance)?;
                return Ok(instance);
            }
            if native == native::SYMBOL {
                // `Symbol` has a [[Construct]] — a class may extend it — but
                // `new` on it, directly or through `super()`, refuses.
                return Err(self.throw_type_error());
            }
            if let Some(kind) = Realm::kind_of(native) {
                return self.error_from_arguments(kind, arguments);
            }
            if native == native::PROMISE {
                return self.construct_promise(arguments.first().copied());
            }
            if Self::builds_from_source(native) {
                return Err(self.throw_type_error());
            }
            if native == native::WEAK_REF {
                return self
                    .construct_weak_ref(arguments.first().copied().unwrap_or(Value::UNDEFINED));
            }
            if native == native::DATE {
                return self.construct_date(arguments);
            }
            if native == native::PROXY {
                return self.construct_proxy(
                    arguments.first().copied().unwrap_or(Value::UNDEFINED),
                    arguments.get(1).copied().unwrap_or(Value::UNDEFINED),
                );
            }
            if native == native::PROXY_CALL {
                let (target, handler) = self.proxy_parts(callee)?;
                let trap = self.proxy_trap(handler, b"construct")?;
                if trap.is_undefined() {
                    self.pending_new_target = if new_target.is_undefined() {
                        target
                    } else {
                        new_target
                    };
                    return self.construct(target, arguments);
                }
                let list = self.create_array()?;
                for &argument in arguments {
                    self.append_element(list, Some(argument))?;
                }
                let made = self.call_value(trap, handler, &[target, list, callee])?;
                if !made.is_object() {
                    return Err(self.throw_type_error());
                }
                return Ok(made);
            }
            if native == native::ARRAY_BUFFER || native == native::SHARED_ARRAY_BUFFER {
                return self.construct_array_buffer(
                    arguments.first().copied().unwrap_or(Value::UNDEFINED),
                    arguments.get(1).copied().unwrap_or(Value::UNDEFINED),
                    native == native::SHARED_ARRAY_BUFFER,
                );
            }
            if native == native::TYPED_ARRAY {
                return self.construct_typed_array(callee, arguments);
            }
            if native == native::DATA_VIEW {
                return self
                    .construct_data_view(arguments.first().copied().unwrap_or(Value::UNDEFINED));
            }
            // `new` on the library's other constructors behaves as the call
            // does, except that a primitive result is wrapped: `new String(x)`
            // is an object carrying the string `String(x)` answers.
            if matches!(
                native,
                native::OBJECT
                    | native::STRING
                    | native::NUMBER
                    | native::BOOLEAN
                    | native::ARRAY
                    | native::REG_EXP
                    | native::MAP
                    | native::SET
                    | native::WEAK_MAP
                    | native::WEAK_SET
            ) {
                let value = self.call_native(native, Value::UNDEFINED, arguments)?;
                if value.is_object() {
                    return Ok(value);
                }
                return self.coerce_to_object(value);
            }
            return Err(Completion::Terminated(Termination::NotImplemented));
        }

        // The new object's prototype is `new.target`'s `prototype` — the
        // constructor's own when nothing else was named — or, when that is
        // not an object, the ordinary one of `new.target`'s realm.
        let source = if new_target.is_object() {
            new_target
        } else {
            callee
        };
        let prototype_key = self.ascii_key(b"prototype")?;
        let prototype = self.get_property(source, prototype_key)?;
        let prototype = if prototype.is_object() {
            prototype
        } else {
            Value::object(self.realm_of_function(source.as_handle()).object_prototype)
        };
        let instance = object::create(self.heap, prototype)
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        let this = Value::object(instance);
        self.pending_new_target = new_target;
        let returned = self.call_constructor_value(callee, this, arguments)?;
        // A constructor that returns an object returns that object instead.
        if returned.is_object() {
            Ok(returned)
        } else {
            Ok(this)
        }
    }

    /// `new Promise(executor)`: call the executor with functions that settle
    /// the new promise, and reject it if the executor throws.
    fn construct_promise(&mut self, executor: Option<Value>) -> Result<Value, Completion> {
        let promise = self.new_promise()?;
        let Some(executor) = executor else {
            return Err(self.throw_type_error());
        };
        if !executor.is_object()
            || !object::is_callable(self.heap, executor.as_handle()).unwrap_or(false)
        {
            return Err(self.throw_type_error());
        }

        let resolve = self.settle_function(native::PROMISE_SETTLE_FULFILLED, promise)?;
        let reject = self.settle_function(native::PROMISE_SETTLE_REJECTED, promise)?;
        match self.call_value(executor, Value::UNDEFINED, &[resolve, reject]) {
            Ok(_) => {}
            Err(Completion::Throw(reason)) => {
                self.settle(promise, promise::REJECTED, reason)?;
            }
            Err(other) => return Err(other),
        }
        Ok(Value::object(promise))
    }

    /// A function that settles `promise`, which is what an executor is handed.
    fn settle_function(&mut self, id: u32, promise: Handle) -> Result<Value, Completion> {
        let function = object::create_native(
            self.heap,
            Value::object(self.realm.function_prototype),
            id,
            0,
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        object::set_bound_value(self.heap, function, Value::object(promise))
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(Value::object(function))
    }

    /// A `finally` half bound to its callback.
    fn finally_function(&mut self, id: u32, callback: Value) -> Result<Value, Completion> {
        let function = object::create_native(
            self.heap,
            Value::object(self.realm.function_prototype),
            id,
            0,
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        object::set_bound_value(self.heap, function, callback)
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(Value::object(function))
    }

    /// Build an error object of `kind`, with the realm's prototype for it.
    fn create_error(&mut self, kind: ErrorKind, message: Value) -> Result<Value, Completion> {
        let prototype = self.realm.prototype_of(kind);
        let object = object::create(self.heap, Value::object(prototype))
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        if !message.is_undefined() {
            let text = self.coerce_to_string(message)?;
            let key = self.ascii_key(b"message")?;
            object::define_own_property(
                self.heap,
                object,
                key,
                Descriptor::data(text, attribute::WRITABLE | attribute::CONFIGURABLE),
            )
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
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
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
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

    /// A `SuppressedError`: `error` is what was thrown last, `suppressed`
    /// what it displaced.
    fn suppressed_error(
        &mut self,
        error: Value,
        suppressed: Value,
        message: Value,
    ) -> Result<Value, Completion> {
        let made = self.create_error(ErrorKind::Suppressed, message)?;
        let handle = made.as_handle();
        for (name, value) in [(&b"error"[..], error), (&b"suppressed"[..], suppressed)] {
            let key = self.ascii_key(name)?;
            object::define_own_property(
                self.heap,
                handle,
                key,
                Descriptor::data(value, attribute::WRITABLE | attribute::CONFIGURABLE),
            )
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        }
        Ok(made)
    }

    /// Dispose every resource a stack holds, last first. A disposer's throw
    /// suppresses whatever was thrown before it — the exception that was
    /// already propagating, or an earlier disposer's — and what remains is
    /// thrown once the stack is empty.
    fn dispose_stack(&mut self, stack: Value, pending: Option<Value>) -> Result<(), Completion> {
        let mut pending = pending;
        if stack.is_object() {
            let count = self.length_of(stack)? / DISPOSABLE_STRIDE;
            let mut index = count;
            while index > 0 {
                index -= 1;
                let resource = self.element(stack, index * DISPOSABLE_STRIDE)?;
                let method = self.element(stack, index * DISPOSABLE_STRIDE + 1)?;
                match self.call_value(method, resource, &[]) {
                    Ok(_) => {}
                    Err(Completion::Throw(thrown)) => {
                        pending = Some(match pending {
                            Some(earlier) => {
                                self.suppressed_error(thrown, earlier, Value::UNDEFINED)?
                            }
                            None => thrown,
                        });
                    }
                    Err(other) => return Err(other),
                }
            }
            self.set_length(stack, 0)?;
        }
        match pending {
            Some(thrown) => Err(Completion::Throw(thrown)),
            None => Ok(()),
        }
    }

    /// Record a resource, its disposer, and how the disposal is awaited:
    /// undefined for none, true for the disposer's result, false for an
    /// await of undefined after a synchronous disposer.
    fn push_disposable(
        &mut self,
        stack: Value,
        resource: Value,
        method: Value,
        hint: Value,
    ) -> Result<(), Completion> {
        let length = self.length_of(stack)?;
        self.set_element(stack, length, resource)?;
        self.set_element(stack, length + 1, method)?;
        self.set_element(stack, length + 2, hint)?;
        self.set_length(stack, length + DISPOSABLE_STRIDE)
    }

    /// Dispose from the top of the stack down to the first result that must
    /// be awaited, returning that result and the pending exception — the
    /// stack itself standing for each where there is none. A run of null
    /// resources awaits once, unless a disposer below them awaits instead.
    fn dispose_stack_next(
        &mut self,
        stack: Value,
        pending: Value,
    ) -> Result<(Value, Value), Completion> {
        let mut pending = pending;
        if !stack.is_object() {
            return Ok((stack, pending));
        }
        loop {
            let length = self.length_of(stack)?;
            if length < DISPOSABLE_STRIDE {
                self.set_length(stack, 0)?;
                return Ok((stack, pending));
            }
            let base = length - DISPOSABLE_STRIDE;
            let resource = self.element(stack, base)?;
            let method = self.element(stack, base + 1)?;
            let hint = self.element(stack, base + 2)?;
            self.set_length(stack, base)?;
            if method.is_undefined() {
                // Null resources: skip the run of them, then await undefined
                // unless the next disposer's own result is awaited.
                let mut next = base;
                while next >= DISPOSABLE_STRIDE {
                    let below = self.element(stack, next - DISPOSABLE_STRIDE + 1)?;
                    if !below.is_undefined() {
                        break;
                    }
                    next -= DISPOSABLE_STRIDE;
                }
                self.set_length(stack, next)?;
                if next >= DISPOSABLE_STRIDE {
                    let below_hint = self.element(stack, next - DISPOSABLE_STRIDE + 2)?;
                    if below_hint.is_boolean() && below_hint.as_boolean() {
                        continue;
                    }
                }
                return Ok((Value::UNDEFINED, pending));
            }
            match self.call_value(method, resource, &[]) {
                Ok(result) => {
                    if hint.is_boolean() && hint.as_boolean() {
                        return Ok((result, pending));
                    }
                    if hint.is_boolean() {
                        return Ok((Value::UNDEFINED, pending));
                    }
                }
                Err(Completion::Throw(thrown)) => {
                    pending = self.fold_pending(stack, pending, thrown)?;
                }
                Err(other) => return Err(other),
            }
        }
    }

    /// The pending exception once `thrown` joins it: `thrown` alone where
    /// `pending` is the stack — nothing pending — else a SuppressedError.
    fn fold_pending(
        &mut self,
        stack: Value,
        pending: Value,
        thrown: Value,
    ) -> Result<Value, Completion> {
        if value::same_value(pending, stack) {
            return Ok(thrown);
        }
        self.suppressed_error(thrown, pending, Value::UNDEFINED)
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

/// Whether a code unit is whitespace to the string grammar: the whitespace
/// characters, the line terminators, and the Unicode space separators — the
/// set `StringToBigInt` and `StringToNumber` skip at either end.
fn is_string_white_space(unit: u16) -> bool {
    matches!(
        unit,
        0x0009 | 0x000A | 0x000B | 0x000C | 0x000D | 0x0020 | 0x00A0 | 0x2028 | 0x2029 | 0xFEFF
    ) || crate::unicode_id::is_space_separator(u32::from(unit))
}

/// How an instruction affected control.
/// How a private member resolved against an access site.
enum PrivateResolution {
    /// A field the receiver itself holds, under its storage key.
    Field(Descriptor, Key),
    /// A method or accessor a class object holds, under its storage key.
    Member(Descriptor, Key),
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

/// The preference `ToPrimitive` starts with.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Hint {
    Default,
    Number,
    String,
}

/// The most arguments one call passes.
/// Own keys one operation may walk at once.
const MAX_OWN_KEYS: usize = 128;
/// Steps one match may take before the task is out of fuel.
const REGEXP_FUEL: u32 = 1_000_000;
/// What an iterator produces at each step.
const ITERATE_VALUES: u8 = 0;
const ITERATE_KEYS: u8 = 1;
const ITERATE_ENTRIES: u8 = 2;
const ITERATE_CODE_POINTS: u8 = 3;
/// A Map's live [key, value] pairs, and a Set's live [value, value] pairs.
const ITERATE_MAP_ENTRIES: u8 = 4;
const ITERATE_SET_ENTRIES: u8 = 5;

const MAX_ARGUMENTS: usize = 16;
/// Interpreter loops one host stack may nest: a native that runs a callback
/// which reaches another native that runs a callback, so far and no further.
const MAX_NESTED_ENTRIES: u32 = 64;

/// Spare environment capacity a dynamic function keeps for the bindings
/// sloppy direct eval code declares at run time.
const EVAL_VAR_SPARE: u32 = 16;
/// The longest string the interpreter stages on its own stack.
const MAX_STRING_UNITS: usize = 256;
/// The most own keys one spread copies.
const MAX_COPIED_KEYS: usize = 64;

include!("natives.rs");

/// The seed `Math.random` starts from: an arbitrary constant, so a machine
/// draws the same sequence every run.
const RANDOM_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// Nesting `JSON.parse` and `JSON.stringify` admit before refusing with a
/// RangeError, which bounds the host stack they recurse on.
const JSON_DEPTH: u32 = 128;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// `new WeakRef(target)`: the target rides as a hidden property, held
    /// as strongly as any reference — a collection is never observed here.
    fn construct_weak_ref(&mut self, target: Value) -> Result<Value, Completion> {
        if !target.is_object() && !matches!(target.tag(), Tag::Symbol) {
            return Err(self.throw_type_error());
        }
        let made = self.new_instance_of(self.realm.weak_ref_prototype)?;
        let key = self.ascii_key(b"\0target")?;
        object::define_own_property(
            self.heap,
            made,
            key,
            Descriptor::data(target, attribute::WRITABLE),
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(Value::object(made))
    }

    /// An ordinary object over a prototype, for a native constructor.
    fn new_instance_of(&mut self, prototype: Handle) -> Result<Handle, Completion> {
        object::create(self.heap, Value::object(prototype))
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))
    }

    // JSON.parse

    /// `JSON.parse(text, reviver)`.
    fn json_parse(&mut self, text: Value, reviver: Value) -> Result<Value, Completion> {
        let source = self.string_handle(text)?;
        let length = string::length(self.heap, source).map_err(|_| self.heap_failure())?;
        let mut at = 0u32;
        self.json_skip_space(source, length, &mut at)?;
        let value = self.json_value(source, length, &mut at, 0)?;
        self.json_skip_space(source, length, &mut at)?;
        if at != length {
            return Err(self.throw_error_of(ErrorKind::Syntax));
        }
        if !self.is_callable_value(reviver) {
            return Ok(value);
        }
        let root = object::create(self.heap, Value::object(self.realm.object_prototype))
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        let empty = self.ascii_key(b"")?;
        object::define_own_property(
            self.heap,
            root,
            empty,
            Descriptor::data(value, attribute::DEFAULT),
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        self.json_internalize(Value::object(root), empty, reviver, 0)
    }

    fn json_unit(&mut self, source: Handle, at: u32) -> Result<Option<u16>, Completion> {
        string::unit_at(self.heap, source, at).map_err(|_| self.heap_failure())
    }

    fn json_skip_space(
        &mut self,
        source: Handle,
        length: u32,
        at: &mut u32,
    ) -> Result<(), Completion> {
        while *at < length {
            match self.json_unit(source, *at)? {
                Some(0x20 | 0x09 | 0x0A | 0x0D) => *at += 1,
                _ => break,
            }
        }
        Ok(())
    }

    fn json_expect_word(
        &mut self,
        source: Handle,
        length: u32,
        at: &mut u32,
        word: &[u8],
    ) -> Result<(), Completion> {
        for &byte in word {
            if *at >= length || self.json_unit(source, *at)? != Some(u16::from(byte)) {
                return Err(self.throw_error_of(ErrorKind::Syntax));
            }
            *at += 1;
        }
        Ok(())
    }

    fn json_value(
        &mut self,
        source: Handle,
        length: u32,
        at: &mut u32,
        depth: u32,
    ) -> Result<Value, Completion> {
        if depth > JSON_DEPTH {
            return Err(self.throw_error_of(ErrorKind::Range));
        }
        let Some(unit) = self.json_unit(source, *at)? else {
            return Err(self.throw_error_of(ErrorKind::Syntax));
        };
        match unit {
            0x7B => {
                *at += 1;
                let made = object::create(self.heap, Value::object(self.realm.object_prototype))
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                self.json_skip_space(source, length, at)?;
                if self.json_unit(source, *at)? == Some(0x7D) {
                    *at += 1;
                    return Ok(Value::object(made));
                }
                loop {
                    self.json_skip_space(source, length, at)?;
                    if self.json_unit(source, *at)? != Some(0x22) {
                        return Err(self.throw_error_of(ErrorKind::Syntax));
                    }
                    let name = self.json_string(source, length, at)?;
                    self.json_skip_space(source, length, at)?;
                    if self.json_unit(source, *at)? != Some(0x3A) {
                        return Err(self.throw_error_of(ErrorKind::Syntax));
                    }
                    *at += 1;
                    self.json_skip_space(source, length, at)?;
                    let value = self.json_value(source, length, at, depth + 1)?;
                    let key = self.coerce_to_key(name)?;
                    object::define_own_property(
                        self.heap,
                        made,
                        key,
                        Descriptor::data(value, attribute::DEFAULT),
                    )
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                    self.json_skip_space(source, length, at)?;
                    match self.json_unit(source, *at)? {
                        Some(0x2C) => *at += 1,
                        Some(0x7D) => {
                            *at += 1;
                            return Ok(Value::object(made));
                        }
                        _ => return Err(self.throw_error_of(ErrorKind::Syntax)),
                    }
                }
            }
            0x5B => {
                *at += 1;
                let array = self.create_array()?;
                self.json_skip_space(source, length, at)?;
                if self.json_unit(source, *at)? == Some(0x5D) {
                    *at += 1;
                    return Ok(array);
                }
                loop {
                    self.json_skip_space(source, length, at)?;
                    let value = self.json_value(source, length, at, depth + 1)?;
                    self.append_element(array, Some(value))?;
                    self.json_skip_space(source, length, at)?;
                    match self.json_unit(source, *at)? {
                        Some(0x2C) => *at += 1,
                        Some(0x5D) => {
                            *at += 1;
                            return Ok(array);
                        }
                        _ => return Err(self.throw_error_of(ErrorKind::Syntax)),
                    }
                }
            }
            0x22 => self.json_string(source, length, at),
            0x74 => {
                self.json_expect_word(source, length, at, b"true")?;
                Ok(Value::TRUE)
            }
            0x66 => {
                self.json_expect_word(source, length, at, b"false")?;
                Ok(Value::FALSE)
            }
            0x6E => {
                self.json_expect_word(source, length, at, b"null")?;
                Ok(Value::NULL)
            }
            0x2D | 0x30..=0x39 => self.json_number(source, length, at),
            _ => Err(self.throw_error_of(ErrorKind::Syntax)),
        }
    }

    fn json_number(
        &mut self,
        source: Handle,
        length: u32,
        at: &mut u32,
    ) -> Result<Value, Completion> {
        let mut digits = [0u16; 512];
        let mut count = 0usize;
        let mut take = |unit: u16, count: &mut usize| -> bool {
            if *count < digits.len() {
                digits[*count] = unit;
                *count += 1;
                true
            } else {
                false
            }
        };
        let mut unit = self.json_unit(source, *at)?;
        if unit == Some(0x2D) {
            take(0x2D, &mut count);
            *at += 1;
            unit = self.json_unit(source, *at)?;
        }
        // The integer part: a lone zero, or a nonzero digit and any more.
        match unit {
            Some(0x30) => {
                take(0x30, &mut count);
                *at += 1;
            }
            Some(0x31..=0x39) => {
                while let Some(digit @ 0x30..=0x39) = self.json_unit(source, *at)? {
                    if !take(digit, &mut count) {
                        return Err(self.throw_error_of(ErrorKind::Range));
                    }
                    *at += 1;
                }
            }
            _ => return Err(self.throw_error_of(ErrorKind::Syntax)),
        }
        if self.json_unit(source, *at)? == Some(0x2E) {
            take(0x2E, &mut count);
            *at += 1;
            let mut any = false;
            while let Some(digit @ 0x30..=0x39) = self.json_unit(source, *at)? {
                if !take(digit, &mut count) {
                    return Err(self.throw_error_of(ErrorKind::Range));
                }
                *at += 1;
                any = true;
            }
            if !any {
                return Err(self.throw_error_of(ErrorKind::Syntax));
            }
        }
        if matches!(self.json_unit(source, *at)?, Some(0x65 | 0x45)) {
            take(0x65, &mut count);
            *at += 1;
            if let Some(sign @ (0x2B | 0x2D)) = self.json_unit(source, *at)? {
                take(sign, &mut count);
                *at += 1;
            }
            let mut any = false;
            while let Some(digit @ 0x30..=0x39) = self.json_unit(source, *at)? {
                if !take(digit, &mut count) {
                    return Err(self.throw_error_of(ErrorKind::Range));
                }
                *at += 1;
                any = true;
            }
            if !any {
                return Err(self.throw_error_of(ErrorKind::Syntax));
            }
        }
        let _ = length;
        Ok(Value::number(value::string_to_number(&digits[..count])))
    }

    /// A JSON string, its opening quote at `at`.
    fn json_string(
        &mut self,
        source: Handle,
        length: u32,
        at: &mut u32,
    ) -> Result<Value, Completion> {
        *at += 1;
        let mut chunk = [0u16; 256];
        let mut filled = 0usize;
        let mut built: Option<Handle> = None;
        loop {
            if *at >= length {
                return Err(self.throw_error_of(ErrorKind::Syntax));
            }
            let Some(unit) = self.json_unit(source, *at)? else {
                return Err(self.throw_error_of(ErrorKind::Syntax));
            };
            *at += 1;
            let out = match unit {
                0x22 => break,
                0x5C => {
                    let Some(escape) = self.json_unit(source, *at)? else {
                        return Err(self.throw_error_of(ErrorKind::Syntax));
                    };
                    *at += 1;
                    match escape {
                        0x22 => 0x22,
                        0x5C => 0x5C,
                        0x2F => 0x2F,
                        0x62 => 0x08,
                        0x66 => 0x0C,
                        0x6E => 0x0A,
                        0x72 => 0x0D,
                        0x74 => 0x09,
                        0x75 => {
                            let mut code = 0u16;
                            for _ in 0..4 {
                                let Some(hex) = self.json_unit(source, *at)? else {
                                    return Err(self.throw_error_of(ErrorKind::Syntax));
                                };
                                *at += 1;
                                let digit = match hex {
                                    0x30..=0x39 => hex - 0x30,
                                    0x41..=0x46 => hex - 0x41 + 10,
                                    0x61..=0x66 => hex - 0x61 + 10,
                                    _ => return Err(self.throw_error_of(ErrorKind::Syntax)),
                                };
                                code = (code << 4) | digit;
                            }
                            code
                        }
                        _ => return Err(self.throw_error_of(ErrorKind::Syntax)),
                    }
                }
                0x00..=0x1F => return Err(self.throw_error_of(ErrorKind::Syntax)),
                other => other,
            };
            if filled == chunk.len() {
                let piece = self.make_string(&chunk)?.as_handle();
                built = Some(match built {
                    Some(so_far) => {
                        string::concat(self.heap, so_far, piece).map_err(|_| self.heap_failure())?
                    }
                    None => piece,
                });
                filled = 0;
            }
            chunk[filled] = out;
            filled += 1;
        }
        let piece = self.make_string(&chunk[..filled])?.as_handle();
        let whole = match built {
            Some(so_far) => {
                string::concat(self.heap, so_far, piece).map_err(|_| self.heap_failure())?
            }
            None => piece,
        };
        Ok(Value::string(whole))
    }

    /// InternalizeJSONProperty: the reviver over every value, leaves first.
    fn json_internalize(
        &mut self,
        holder: Value,
        key: Key,
        reviver: Value,
        depth: u32,
    ) -> Result<Value, Completion> {
        if depth > JSON_DEPTH {
            return Err(self.throw_error_of(ErrorKind::Range));
        }
        let value = self.get_property(holder, key)?;
        if value.is_object() {
            if self.is_array(value)? {
                let length = self.length_of(value)?;
                let mut index = 0u32;
                while index < length {
                    let element =
                        self.json_internalize(value, Key::Index(index), reviver, depth + 1)?;
                    if element.is_undefined() {
                        self.delete_property(value, Key::Index(index))?;
                    } else {
                        object::define_own_property(
                            self.heap,
                            value.as_handle(),
                            Key::Index(index),
                            Descriptor::data(element, attribute::DEFAULT),
                        )
                        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                    }
                    index += 1;
                }
            } else {
                let mut keys = [Key::Index(0); MAX_OWN_KEYS];
                let count = object::own_keys(self.heap, value.as_handle(), &mut keys)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                for &name in keys.iter().take(count) {
                    if matches!(name, Key::Symbol(_)) || !self.is_enumerable(value, name)? {
                        continue;
                    }
                    let element = self.json_internalize(value, name, reviver, depth + 1)?;
                    if element.is_undefined() {
                        self.delete_property(value, name)?;
                    } else {
                        object::define_own_property(
                            self.heap,
                            value.as_handle(),
                            name,
                            Descriptor::data(element, attribute::DEFAULT),
                        )
                        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                    }
                }
            }
        }
        let name = self.key_to_value(key)?;
        let name = self.coerce_to_string(name)?;
        self.call_value(reviver, holder, &[name, value])
    }

    // JSON.stringify

    /// `JSON.stringify(value, replacer, space)`.
    fn json_stringify(
        &mut self,
        value: Value,
        replacer: Value,
        space: Value,
    ) -> Result<Value, Completion> {
        let mut state = JsonState {
            replacer_function: Value::UNDEFINED,
            property_list: Value::UNDEFINED,
            gap: [0u16; 10],
            gap_length: 0,
            stack: [Handle::new(0, 0); JSON_DEPTH as usize],
            depth: 0,
        };
        if replacer.is_object() {
            if self.is_callable_value(replacer) {
                state.replacer_function = replacer;
            } else if self.is_array(replacer)? {
                // A property list: strings and numbers, wrapped or not, once
                // each, in order.
                let list = self.create_array()?;
                let length = self.length_of(replacer)?;
                let mut index = 0u32;
                while index < length {
                    let entry = self.element(replacer, index)?;
                    index += 1;
                    let item = match entry.tag() {
                        Tag::String => Some(entry),
                        Tag::Number => Some(self.coerce_to_string(entry)?),
                        Tag::Object => match object::wrapper_value(self.heap, entry.as_handle()) {
                            Ok(Some(inner)) if matches!(inner.tag(), Tag::String | Tag::Number) => {
                                Some(self.coerce_to_string(entry)?)
                            }
                            _ => None,
                        },
                        _ => None,
                    };
                    let Some(item) = item else {
                        continue;
                    };
                    let count = self.length_of(list)?;
                    let mut seen = false;
                    let mut scan = 0u32;
                    while scan < count {
                        let held = self.element(list, scan)?;
                        if self.strict_equals(held, item)? {
                            seen = true;
                            break;
                        }
                        scan += 1;
                    }
                    if !seen {
                        self.append_element(list, Some(item))?;
                    }
                }
                state.property_list = list;
            }
        }
        // The gap: up to ten spaces for a number, the first ten units of a
        // string, wrappers unwrapped first.
        let space = if space.is_object() {
            match object::wrapper_value(self.heap, space.as_handle()) {
                Ok(Some(inner)) if matches!(inner.tag(), Tag::Number) => {
                    Value::number(self.coerce_to_number(space)?)
                }
                Ok(Some(inner)) if matches!(inner.tag(), Tag::String) => {
                    self.coerce_to_string(space)?
                }
                _ => space,
            }
        } else {
            space
        };
        if matches!(space.tag(), Tag::Number) {
            let count = value::truncate(space.as_number()).clamp(0.0, 10.0) as usize;
            let mut index = 0usize;
            while index < count {
                state.gap[index] = 0x20;
                index += 1;
            }
            state.gap_length = count;
        } else if matches!(space.tag(), Tag::String) {
            let handle = space.as_handle();
            let length = string::length(self.heap, handle).map_err(|_| self.heap_failure())?;
            let count = (length as usize).min(10);
            let mut index = 0usize;
            while index < count {
                state.gap[index] = string::unit_at(self.heap, handle, index as u32)
                    .map_err(|_| self.heap_failure())?
                    .unwrap_or(0x20);
                index += 1;
            }
            state.gap_length = count;
        }
        let wrapper = object::create(self.heap, Value::object(self.realm.object_prototype))
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        let empty = self.ascii_key(b"")?;
        object::define_own_property(
            self.heap,
            wrapper,
            empty,
            Descriptor::data(value, attribute::DEFAULT),
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        let empty_name = self.ascii_string(b"")?;
        match self.json_property(empty_name, Value::object(wrapper), &mut state, 0)? {
            Some(text) => Ok(text),
            None => Ok(Value::UNDEFINED),
        }
    }

    /// SerializeJSONProperty: the text a property serialises to, or nothing
    /// where it is left out.
    fn json_property(
        &mut self,
        name: Value,
        holder: Value,
        state: &mut JsonState,
        indent: usize,
    ) -> Result<Option<Value>, Completion> {
        let key = self.coerce_to_key(name)?;
        let mut value = self.get_property(holder, key)?;
        if value.is_object() || matches!(value.tag(), Tag::BigInt) {
            let to_json_key = self.ascii_key(b"toJSON")?;
            let to_json = self.get_property(value, to_json_key)?;
            if self.is_callable_value(to_json) {
                value = self.call_value(to_json, value, &[name])?;
            }
        }
        if !state.replacer_function.is_undefined() {
            let replacer = state.replacer_function;
            value = self.call_value(replacer, holder, &[name, value])?;
        }
        if value.is_object() {
            if let Ok(Some(inner)) = object::wrapper_value(self.heap, value.as_handle()) {
                match inner.tag() {
                    Tag::Number => value = Value::number(self.coerce_to_number(value)?),
                    Tag::String => value = self.coerce_to_string(value)?,
                    Tag::Boolean | Tag::BigInt => value = inner,
                    _ => {}
                }
            }
        }
        match value.tag() {
            Tag::Null => return self.ascii_string(b"null").map(Some),
            Tag::Boolean => {
                return self
                    .ascii_string(if value.as_boolean() {
                        b"true"
                    } else {
                        b"false"
                    })
                    .map(Some);
            }
            Tag::String => return self.json_quote(value.as_handle()).map(Some),
            Tag::Number => {
                if value.as_number().is_finite() {
                    return self.coerce_to_string(value).map(Some);
                }
                return self.ascii_string(b"null").map(Some);
            }
            Tag::BigInt => return Err(self.throw_type_error()),
            _ => {}
        }
        if value.is_object() && !self.is_callable_value(value) {
            if self.is_array(value)? {
                return self.json_array(value, state, indent).map(Some);
            }
            return self.json_object(value, state, indent).map(Some);
        }
        Ok(None)
    }

    /// Enter a value's serialisation, refusing a cycle and a depth beyond
    /// the admitted bound.
    fn json_enter(&mut self, value: Value, state: &mut JsonState) -> Result<(), Completion> {
        let handle = value.as_handle();
        for &seen in state.stack.iter().take(state.depth) {
            if seen == handle {
                return Err(self.throw_type_error());
            }
        }
        if state.depth >= state.stack.len() {
            return Err(self.throw_error_of(ErrorKind::Range));
        }
        state.stack[state.depth] = handle;
        state.depth += 1;
        Ok(())
    }

    fn json_join(&mut self, so_far: Value, piece: Value) -> Result<Value, Completion> {
        string::concat(self.heap, so_far.as_handle(), piece.as_handle())
            .map(Value::string)
            .map_err(|_| self.heap_failure())
    }

    /// A line break followed by the indentation, where a gap is set.
    fn json_break(&mut self, state: &JsonState, indent: usize) -> Result<Value, Completion> {
        if state.gap_length == 0 {
            return self.ascii_string(b"");
        }
        let mut units = [0u16; 512];
        units[0] = 0x0A;
        let mut written = 1usize;
        for _ in 0..indent {
            for &unit in &state.gap[..state.gap_length] {
                if written < units.len() {
                    units[written] = unit;
                    written += 1;
                }
            }
        }
        self.make_string(&units[..written])
    }

    fn json_object(
        &mut self,
        value: Value,
        state: &mut JsonState,
        indent: usize,
    ) -> Result<Value, Completion> {
        self.json_enter(value, state)?;
        let mut keys = [Key::Index(0); MAX_OWN_KEYS];
        let mut names = [Value::UNDEFINED; MAX_OWN_KEYS];
        let mut count = 0usize;
        if state.property_list.is_object() {
            let list = state.property_list;
            let length = self.length_of(list)?;
            let mut index = 0u32;
            while index < length && count < names.len() {
                names[count] = self.element(list, index)?;
                count += 1;
                index += 1;
            }
        } else {
            let found = object::own_keys(self.heap, value.as_handle(), &mut keys)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            for &key in keys.iter().take(found) {
                if matches!(key, Key::Symbol(_)) || !self.is_enumerable(value, key)? {
                    continue;
                }
                if count < names.len() {
                    let name = self.key_to_value(key)?;
                    names[count] = self.coerce_to_string(name)?;
                    count += 1;
                }
            }
        }
        let mut out = self.ascii_string(b"{")?;
        let mut any = false;
        let separator = if state.gap_length == 0 {
            b":" as &[u8]
        } else {
            b": "
        };
        for &name in names.iter().take(count) {
            let Some(text) = self.json_property(name, value, state, indent + 1)? else {
                continue;
            };
            if any {
                let comma = self.ascii_string(b",")?;
                out = self.json_join(out, comma)?;
            }
            let brk = self.json_break(state, indent + 1)?;
            out = self.json_join(out, brk)?;
            let quoted = self.json_quote(name.as_handle())?;
            out = self.json_join(out, quoted)?;
            let colon = self.ascii_string(separator)?;
            out = self.json_join(out, colon)?;
            out = self.json_join(out, text)?;
            any = true;
        }
        if any {
            let brk = self.json_break(state, indent)?;
            out = self.json_join(out, brk)?;
        }
        let close = self.ascii_string(b"}")?;
        out = self.json_join(out, close)?;
        state.depth -= 1;
        Ok(out)
    }

    fn json_array(
        &mut self,
        value: Value,
        state: &mut JsonState,
        indent: usize,
    ) -> Result<Value, Completion> {
        self.json_enter(value, state)?;
        let length = self.length_of(value)?;
        let mut out = self.ascii_string(b"[")?;
        let mut index = 0u32;
        while index < length {
            if index > 0 {
                let comma = self.ascii_string(b",")?;
                out = self.json_join(out, comma)?;
            }
            let brk = self.json_break(state, indent + 1)?;
            out = self.json_join(out, brk)?;
            let name = self.coerce_to_string(Value::number(f64::from(index)))?;
            let text = match self.json_property(name, value, state, indent + 1)? {
                Some(text) => text,
                None => self.ascii_string(b"null")?,
            };
            out = self.json_join(out, text)?;
            index += 1;
        }
        if length > 0 {
            let brk = self.json_break(state, indent)?;
            out = self.json_join(out, brk)?;
        }
        let close = self.ascii_string(b"]")?;
        out = self.json_join(out, close)?;
        state.depth -= 1;
        Ok(out)
    }

    /// QuoteJSONString: the string in quotes, its escapes written, and a
    /// lone surrogate written as an escape so the text is well formed.
    fn json_quote(&mut self, text: Handle) -> Result<Value, Completion> {
        let length = string::length(self.heap, text).map_err(|_| self.heap_failure())?;
        let mut chunk = [0u16; 256];
        let mut filled = 0usize;
        chunk[0] = 0x22;
        filled += 1;
        let mut out: Option<Handle> = None;
        let mut index = 0u32;
        while index <= length {
            let mut piece = [0u16; 8];
            let count;
            if index == length {
                piece[0] = 0x22;
                count = 1;
            } else {
                let unit = string::unit_at(self.heap, text, index)
                    .map_err(|_| self.heap_failure())?
                    .unwrap_or(0);
                let escaped: Option<u8> = match unit {
                    0x08 => Some(b'b'),
                    0x09 => Some(b't'),
                    0x0A => Some(b'n'),
                    0x0C => Some(b'f'),
                    0x0D => Some(b'r'),
                    0x22 => Some(b'"'),
                    0x5C => Some(b'\\'),
                    _ => None,
                };
                if let Some(letter) = escaped {
                    piece[0] = 0x5C;
                    piece[1] = u16::from(letter);
                    count = 2;
                } else {
                    let lone = match unit {
                        0xD800..=0xDBFF => {
                            let next = string::unit_at(self.heap, text, index + 1)
                                .map_err(|_| self.heap_failure())?
                                .unwrap_or(0);
                            !(0xDC00..=0xDFFF).contains(&next)
                        }
                        0xDC00..=0xDFFF => {
                            let previous = if index == 0 {
                                0
                            } else {
                                string::unit_at(self.heap, text, index - 1)
                                    .map_err(|_| self.heap_failure())?
                                    .unwrap_or(0)
                            };
                            !(0xD800..=0xDBFF).contains(&previous)
                        }
                        _ => false,
                    };
                    if unit < 0x20 || lone {
                        piece[0] = 0x5C;
                        piece[1] = 0x75;
                        for (place, shift) in [(2usize, 12u32), (3, 8), (4, 4), (5, 0)] {
                            let digit = ((unit >> shift) & 0xF) as u8;
                            piece[place] = u16::from(if digit < 10 {
                                b'0' + digit
                            } else {
                                b'a' + digit - 10
                            });
                        }
                        count = 6;
                    } else {
                        piece[0] = unit;
                        count = 1;
                    }
                }
            }
            if filled + count > chunk.len() {
                let flushed = self.make_string(&chunk[..filled])?.as_handle();
                out = Some(match out {
                    Some(so_far) => string::concat(self.heap, so_far, flushed)
                        .map_err(|_| self.heap_failure())?,
                    None => flushed,
                });
                filled = 0;
            }
            chunk[filled..filled + count].copy_from_slice(&piece[..count]);
            filled += count;
            index += 1;
        }
        let flushed = self.make_string(&chunk[..filled])?.as_handle();
        let whole = match out {
            Some(so_far) => {
                string::concat(self.heap, so_far, flushed).map_err(|_| self.heap_failure())?
            }
            None => flushed,
        };
        Ok(Value::string(whole))
    }
}

/// What one `JSON.stringify` carries down its recursion.
struct JsonState {
    replacer_function: Value,
    property_list: Value,
    gap: [u16; 10],
    gap_length: usize,
    stack: [Handle; JSON_DEPTH as usize],
    depth: usize,
}

/// One millisecond, one second, one minute, one hour, and one day, in the
/// milliseconds a time value counts.
const MS_PER_SECOND: f64 = 1000.0;
const MS_PER_MINUTE: f64 = 60_000.0;
const MS_PER_HOUR: f64 = 3_600_000.0;
const MS_PER_DAY: f64 = 86_400_000.0;
/// The farthest a time value may lie from the epoch.
const TIME_RANGE: f64 = 8.64e15;

/// The fields of a time value, in UTC.
#[derive(Clone, Copy)]
pub struct DateFields {
    pub year: i32,
    pub month: i32,
    pub date: i32,
    pub weekday: i32,
    pub hours: i32,
    pub minutes: i32,
    pub seconds: i32,
    pub milliseconds: i32,
}

/// Which text a date is asked for.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DateForm {
    Full,
    Utc,
    Date,
    Time,
    Iso,
}

/// TimeClip: a time value within range, made an integer, or NaN.
pub fn time_clip(time: f64) -> f64 {
    if !time.is_finite() || value::floor(if time < 0.0 { -time } else { time }) > TIME_RANGE {
        return f64::NAN;
    }
    let integral = value::truncate(time);
    if integral == 0.0 {
        0.0
    } else {
        integral
    }
}

/// Floor division of whole numbers held as doubles: the arithmetic below
/// stays in f64 and i32, since a small target has no 64-bit division.
fn floor_div(numerator: f64, denominator: f64) -> f64 {
    value::floor(numerator / denominator)
}

/// Days from the epoch to `day` of `month` (0..12) in `year`, proleptic
/// Gregorian.
fn days_from_civil(year: f64, month: f64, day: f64) -> f64 {
    // Howard Hinnant's algorithm, with March as the year's first month.
    let year = if month <= 1.0 { year - 1.0 } else { year };
    let era = floor_div(year, 400.0);
    let year_of_era = year - era * 400.0;
    let month_of_era = (month + 10.0) - floor_div(month + 10.0, 12.0) * 12.0;
    let day_of_year = floor_div(153.0 * month_of_era + 2.0, 5.0) + day - 1.0;
    let day_of_era = year_of_era * 365.0 + floor_div(year_of_era, 4.0)
        - floor_div(year_of_era, 100.0)
        + day_of_year;
    era * 146_097.0 + day_of_era - 719_468.0
}

/// The year, month (0..12), and day (1..) a day count from the epoch names.
fn civil_from_days(days: f64) -> (f64, f64, f64) {
    let shifted = days + 719_468.0;
    let era = floor_div(shifted, 146_097.0);
    let day_of_era = shifted - era * 146_097.0;
    let year_of_era = floor_div(
        day_of_era - floor_div(day_of_era, 1460.0) + floor_div(day_of_era, 36_524.0)
            - floor_div(day_of_era, 146_096.0),
        365.0,
    );
    let year = year_of_era + era * 400.0;
    let day_of_year = day_of_era
        - (365.0 * year_of_era + floor_div(year_of_era, 4.0) - floor_div(year_of_era, 100.0));
    let month_of_era = floor_div(5.0 * day_of_year + 2.0, 153.0);
    let day = day_of_year - floor_div(153.0 * month_of_era + 2.0, 5.0) + 1.0;
    let month = if month_of_era < 10.0 {
        month_of_era + 2.0
    } else {
        month_of_era - 10.0
    };
    let year = if month <= 1.0 { year + 1.0 } else { year };
    (year, month, day)
}

/// The fields a finite time value has.
pub fn date_fields(time: f64) -> DateFields {
    let day = value::floor(time / MS_PER_DAY);
    let within = time - day * MS_PER_DAY;
    let (year, month, date) = civil_from_days(day);
    let weekday = (day + 4.0) - floor_div(day + 4.0, 7.0) * 7.0;
    DateFields {
        year: year as i32,
        month: month as i32,
        date: date as i32,
        weekday: weekday as i32,
        hours: value::floor(within / MS_PER_HOUR) as i32,
        minutes: (value::floor(within / MS_PER_MINUTE) as i32) % 60,
        seconds: (value::floor(within / MS_PER_SECOND) as i32) % 60,
        milliseconds: (within as i32) % 1000,
    }
}

/// MakeDate over MakeDay and MakeTime, every part a finite number already.
fn make_date(
    year: f64,
    month: f64,
    date: f64,
    hours: f64,
    minutes: f64,
    seconds: f64,
    ms: f64,
) -> f64 {
    let parts = [year, month, date, hours, minutes, seconds, ms];
    if parts.iter().any(|part| !part.is_finite()) {
        return f64::NAN;
    }
    let year = value::truncate(year);
    let month = value::truncate(month);
    let whole_year = year + value::floor(month / 12.0);
    let month_in_year = month - value::floor(month / 12.0) * 12.0;
    if whole_year.abs() > 400_000.0 {
        return f64::NAN;
    }
    let days = days_from_civil(whole_year, month_in_year, 1.0) + value::truncate(date) - 1.0;
    let time = value::truncate(hours) * MS_PER_HOUR
        + value::truncate(minutes) * MS_PER_MINUTE
        + value::truncate(seconds) * MS_PER_SECOND
        + value::truncate(ms);
    days * MS_PER_DAY + time
}

// Fixed-size arrays, not slices: a table of references would carry
// relocations a loaded module cannot bear.
const DAY_NAMES: [[u8; 3]; 7] = [
    *b"Sun", *b"Mon", *b"Tue", *b"Wed", *b"Thu", *b"Fri", *b"Sat",
];
const MONTH_NAMES: [[u8; 3]; 12] = [
    *b"Jan", *b"Feb", *b"Mar", *b"Apr", *b"May", *b"Jun", *b"Jul", *b"Aug", *b"Sep", *b"Oct",
    *b"Nov", *b"Dec",
];

fn put_digits(out: &mut [u8], at: &mut usize, value: i32, width: usize) {
    let mut digits = [0u8; 20];
    let mut count = 0usize;
    let mut remaining = value.unsigned_abs();
    loop {
        digits[count] = b'0' + (remaining % 10) as u8;
        count += 1;
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    while count < width {
        digits[count] = b'0';
        count += 1;
    }
    while count > 0 {
        count -= 1;
        if *at < out.len() {
            out[*at] = digits[count];
            *at += 1;
        }
    }
}

fn put_text(out: &mut [u8], at: &mut usize, text: &[u8]) {
    for &byte in text {
        if *at < out.len() {
            out[*at] = byte;
            *at += 1;
        }
    }
}

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// "Now": a logical instant, absent a clock capability. Each reading
    /// is one tick after the last, kept on the global so it survives the
    /// machine's rebuilds — deterministic, monotonic, and telling nothing
    /// of the world outside but how often it was asked.
    fn date_now(&mut self) -> f64 {
        let Ok(key) = self.ascii_key(b"\0clock") else {
            return 0.0;
        };
        let held = object::get_own_property(self.heap, self.realm.global, key)
            .ok()
            .flatten()
            .map_or(0.0, |descriptor| {
                if matches!(descriptor.value.tag(), Tag::Number) {
                    descriptor.value.as_number()
                } else {
                    0.0
                }
            });
        let next = held + 1.0;
        let _ = object::define_own_property(
            self.heap,
            self.realm.global,
            key,
            Descriptor::data(Value::number(next), attribute::WRITABLE),
        );
        next
    }

    /// The time value a date holds, or a TypeError for anything else.
    fn date_time_of(&mut self, this: Value) -> Result<f64, Completion> {
        if this.is_object() {
            let key = self.ascii_key(b"\0time")?;
            let held = object::get_own_property(self.heap, this.as_handle(), key)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            if let Some(descriptor) = held {
                if matches!(descriptor.value.tag(), Tag::Number) {
                    return Ok(descriptor.value.as_number());
                }
            }
        }
        Err(self.throw_type_error())
    }

    fn date_set_time(&mut self, this: Value, time: f64) -> Result<(), Completion> {
        let key = self.ascii_key(b"\0time")?;
        object::define_own_property(
            self.heap,
            this.as_handle(),
            key,
            Descriptor::data(Value::number(time), attribute::WRITABLE),
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(())
    }

    /// `new Date(...)`: no argument is now, one is a time value — a date's
    /// own, a string to parse, or a number — and more are components.
    fn construct_date(&mut self, arguments: &[Value]) -> Result<Value, Completion> {
        let time = match arguments.len() {
            0 => self.date_now(),
            1 => {
                let only = arguments[0];
                let own = if only.is_object() {
                    let key = self.ascii_key(b"\0time")?;
                    object::get_own_property(self.heap, only.as_handle(), key)
                        .map_err(|_| Completion::Terminated(Termination::Malformed))?
                        .map(|descriptor| descriptor.value)
                } else {
                    None
                };
                match own {
                    Some(held) if matches!(held.tag(), Tag::Number) => held.as_number(),
                    _ => {
                        let primitive = self.coerce_to_primitive(only, Hint::Default)?;
                        if matches!(primitive.tag(), Tag::String) {
                            self.date_parse(primitive.as_handle())?
                        } else {
                            time_clip(self.coerce_to_number(primitive)?)
                        }
                    }
                }
            }
            _ => {
                let made = self.date_from_components(arguments)?;
                made.as_number()
            }
        };
        let made = self.new_instance_of(self.realm.date_prototype)?;
        let value = Value::object(made);
        self.date_set_time(value, time)?;
        Ok(value)
    }

    /// A time value from year, month, and optional date, hours, minutes,
    /// seconds, and milliseconds — `Date.UTC`, and `new Date` with more
    /// than one argument. A two-digit year lies in the twentieth century.
    fn date_from_components(&mut self, arguments: &[Value]) -> Result<Value, Completion> {
        let mut parts = [f64::NAN, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0];
        for (index, slot) in parts.iter_mut().enumerate() {
            if let Some(&argument) = arguments.get(index) {
                *slot = self.coerce_to_number(argument)?;
            }
        }
        if arguments.is_empty() {
            return Ok(Value::number(f64::NAN));
        }
        if parts[0].is_finite() {
            let whole = value::truncate(parts[0]);
            if (0.0..=99.0).contains(&whole) {
                parts[0] = 1900.0 + whole;
            }
        }
        let time = make_date(
            parts[0], parts[1], parts[2], parts[3], parts[4], parts[5], parts[6],
        );
        Ok(Value::number(time_clip(time)))
    }

    /// The text of a time value: the ISO form, the `toString` form and its
    /// date and time halves, or the UTC form.
    fn date_to_string(&mut self, time: f64, form: DateForm) -> Result<Value, Completion> {
        if time.is_nan() {
            return self.ascii_string(b"Invalid Date");
        }
        let fields = date_fields(time);
        let mut out = [0u8; 64];
        let mut at = 0usize;
        match form {
            DateForm::Iso => {
                let year = fields.year;
                if !(0..=9999).contains(&year) {
                    put_text(&mut out, &mut at, if year < 0 { b"-" } else { b"+" });
                    put_digits(&mut out, &mut at, year, 6);
                } else {
                    put_digits(&mut out, &mut at, year, 4);
                }
                put_text(&mut out, &mut at, b"-");
                put_digits(&mut out, &mut at, fields.month + 1, 2);
                put_text(&mut out, &mut at, b"-");
                put_digits(&mut out, &mut at, fields.date, 2);
                put_text(&mut out, &mut at, b"T");
                put_digits(&mut out, &mut at, fields.hours, 2);
                put_text(&mut out, &mut at, b":");
                put_digits(&mut out, &mut at, fields.minutes, 2);
                put_text(&mut out, &mut at, b":");
                put_digits(&mut out, &mut at, fields.seconds, 2);
                put_text(&mut out, &mut at, b".");
                put_digits(&mut out, &mut at, fields.milliseconds, 3);
                put_text(&mut out, &mut at, b"Z");
            }
            DateForm::Utc => {
                put_text(&mut out, &mut at, &DAY_NAMES[fields.weekday as usize % 7]);
                put_text(&mut out, &mut at, b", ");
                put_digits(&mut out, &mut at, fields.date, 2);
                put_text(&mut out, &mut at, b" ");
                put_text(&mut out, &mut at, &MONTH_NAMES[fields.month as usize % 12]);
                put_text(&mut out, &mut at, b" ");
                self.put_year(&mut out, &mut at, fields.year);
                put_text(&mut out, &mut at, b" ");
                self.put_clock(&mut out, &mut at, &fields);
                put_text(&mut out, &mut at, b" GMT");
            }
            DateForm::Full | DateForm::Date | DateForm::Time => {
                if form != DateForm::Time {
                    put_text(&mut out, &mut at, &DAY_NAMES[fields.weekday as usize % 7]);
                    put_text(&mut out, &mut at, b" ");
                    put_text(&mut out, &mut at, &MONTH_NAMES[fields.month as usize % 12]);
                    put_text(&mut out, &mut at, b" ");
                    put_digits(&mut out, &mut at, fields.date, 2);
                    put_text(&mut out, &mut at, b" ");
                    self.put_year(&mut out, &mut at, fields.year);
                }
                if form == DateForm::Full {
                    put_text(&mut out, &mut at, b" ");
                }
                if form != DateForm::Date {
                    self.put_clock(&mut out, &mut at, &fields);
                    put_text(&mut out, &mut at, b" GMT+0000 (Coordinated Universal Time)");
                }
            }
        }
        self.ascii_string(&out[..at])
    }

    fn put_year(&self, out: &mut [u8], at: &mut usize, year: i32) {
        if year < 0 {
            put_text(out, at, b"-");
            put_digits(out, at, -year, 4);
        } else {
            put_digits(out, at, year, 4);
        }
    }

    fn put_clock(&self, out: &mut [u8], at: &mut usize, fields: &DateFields) {
        put_digits(out, at, fields.hours, 2);
        put_text(out, at, b":");
        put_digits(out, at, fields.minutes, 2);
        put_text(out, at, b":");
        put_digits(out, at, fields.seconds, 2);
    }

    /// The time value a string denotes in the ISO date-time form —
    /// `YYYY-MM-DDTHH:mm:ss.sssZ`, with the time, the seconds, the
    /// milliseconds, and the offset optional, and an expanded year with a
    /// sign — or NaN for anything else.
    fn date_parse(&mut self, text: Handle) -> Result<f64, Completion> {
        let length = string::length(self.heap, text).map_err(|_| self.heap_failure())?;
        let mut units = [0u16; 40];
        if length as usize > units.len() {
            return Ok(f64::NAN);
        }
        string::copy_units(self.heap, text, &mut units[..length as usize])
            .map_err(|_| self.heap_failure())?;
        let units = &units[..length as usize];
        let mut at = 0usize;
        let digits = |units: &[u16], at: &mut usize, count: usize| -> Option<i32> {
            let mut value = 0i32;
            for _ in 0..count {
                let unit = *units.get(*at)?;
                if !(0x30..=0x39).contains(&unit) {
                    return None;
                }
                value = value * 10 + i32::from(unit - 0x30);
                *at += 1;
            }
            Some(value)
        };
        let eat = |units: &[u16], at: &mut usize, unit: u16| -> bool {
            if units.get(*at).copied() == Some(unit) {
                *at += 1;
                true
            } else {
                false
            }
        };
        let year = match units.first().copied() {
            Some(0x2B) => {
                at += 1;
                digits(units, &mut at, 6)
            }
            Some(0x2D) => {
                at += 1;
                digits(units, &mut at, 6).map(|year| -year)
            }
            _ => digits(units, &mut at, 4),
        };
        let Some(year) = year else {
            return Ok(f64::NAN);
        };
        let mut month = 1i32;
        let mut day = 1i32;
        if eat(units, &mut at, 0x2D) {
            let Some(parsed) = digits(units, &mut at, 2) else {
                return Ok(f64::NAN);
            };
            month = parsed;
            if eat(units, &mut at, 0x2D) {
                let Some(parsed) = digits(units, &mut at, 2) else {
                    return Ok(f64::NAN);
                };
                day = parsed;
            }
        }
        let (mut hours, mut minutes, mut seconds, mut ms) = (0i32, 0i32, 0i32, 0i32);
        let mut offset = 0i32;
        if eat(units, &mut at, 0x54) {
            let (Some(h), true, Some(m)) = (
                digits(units, &mut at, 2),
                eat(units, &mut at, 0x3A),
                digits(units, &mut at, 2),
            ) else {
                return Ok(f64::NAN);
            };
            hours = h;
            minutes = m;
            if eat(units, &mut at, 0x3A) {
                let Some(s) = digits(units, &mut at, 2) else {
                    return Ok(f64::NAN);
                };
                seconds = s;
                if eat(units, &mut at, 0x2E) {
                    let Some(fraction) = digits(units, &mut at, 3) else {
                        return Ok(f64::NAN);
                    };
                    ms = fraction;
                }
            }
            if !eat(units, &mut at, 0x5A) {
                if let Some(sign @ (0x2B | 0x2D)) = units.get(at).copied() {
                    at += 1;
                    let (Some(oh), true, Some(om)) = (
                        digits(units, &mut at, 2),
                        eat(units, &mut at, 0x3A),
                        digits(units, &mut at, 2),
                    ) else {
                        return Ok(f64::NAN);
                    };
                    offset = (oh * 60 + om) * if sign == 0x2D { -1 } else { 1 };
                }
            }
        }
        if at != units.len() {
            return Ok(f64::NAN);
        }
        if !(1..=12).contains(&month)
            || !(1..=31).contains(&day)
            || hours > 24
            || minutes > 59
            || seconds > 59
            || (hours == 24 && (minutes > 0 || seconds > 0 || ms > 0))
        {
            return Ok(f64::NAN);
        }
        let time = make_date(
            f64::from(year),
            f64::from(month - 1),
            f64::from(day),
            f64::from(hours),
            f64::from(minutes),
            f64::from(seconds),
            f64::from(ms),
        ) - f64::from(offset) * MS_PER_MINUTE;
        Ok(time_clip(time))
    }
}

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    // Proxy

    /// `new Proxy(target, handler)`: a callable target makes a callable
    /// proxy, a constructor a constructor; either interposes the handler.
    fn construct_proxy(&mut self, target: Value, handler: Value) -> Result<Value, Completion> {
        if !target.is_object() || !handler.is_object() {
            return Err(self.throw_type_error());
        }
        let callable = self.is_callable_value(target);
        let made = if callable {
            let mut flags = 0u8;
            if object::is_constructor(self.heap, target.as_handle()).unwrap_or(false) {
                flags |= object::function_flag::CONSTRUCTOR;
            }
            object::create_native(
                self.heap,
                Value::object(self.realm.function_prototype),
                native::PROXY_CALL,
                flags,
            )
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?
        } else {
            object::create(self.heap, Value::NULL)
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?
        };
        for (name, held) in [(&b"\0target"[..], target), (&b"\0handler"[..], handler)] {
            let key = self.ascii_key(name)?;
            object::define_own_property(
                self.heap,
                made,
                key,
                Descriptor::data(held, attribute::WRITABLE),
            )
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        }
        object::set_exotic_kind(self.heap, made, object::exotic::PROXY)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        Ok(Value::object(made))
    }

    /// A proxy's target and handler.
    fn proxy_parts(&mut self, proxy: Value) -> Result<(Value, Value), Completion> {
        if !proxy.is_object() {
            return Err(self.throw_type_error());
        }
        let mut parts = [Value::UNDEFINED; 2];
        for (slot, name) in parts.iter_mut().zip([&b"\0target"[..], &b"\0handler"[..]]) {
            let key = self.ascii_key(name)?;
            *slot = object::get_own_property(self.heap, proxy.as_handle(), key)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?
                .map_or(Value::UNDEFINED, |descriptor| descriptor.value);
        }
        // A revoked proxy has neither: every operation on it is a TypeError.
        if !parts[1].is_object() {
            return Err(self.throw_type_error());
        }
        Ok((parts[0], parts[1]))
    }

    /// The own keys a proxy reports: its `ownKeys` trap's list, each a string
    /// or a symbol, or its target's own keys where it has no trap.
    fn proxy_own_keys(&mut self, proxy: Value, out: &mut [Key]) -> Result<usize, Completion> {
        let (target, handler) = self.proxy_parts(proxy)?;
        let trap = self.proxy_trap(handler, b"ownKeys")?;
        if trap.is_undefined() {
            return object::own_keys(self.heap, target.as_handle(), out).map_err(Self::key_failure);
        }
        let list = self.call_value(trap, handler, &[target])?;
        if !list.is_object() {
            return Err(self.throw_type_error());
        }
        let count = self.length_of(list)?;
        let mut written = 0usize;
        let mut index = 0u32;
        while index < count {
            let element = self.element(list, index)?;
            index += 1;
            if !element.is_string() && !matches!(element.tag(), Tag::Symbol) {
                return Err(self.throw_type_error());
            }
            let key = self.coerce_to_key(element)?;
            let Some(slot) = out.get_mut(written) else {
                return Err(Completion::Terminated(Termination::QuotaExceeded));
            };
            *slot = key;
            written += 1;
        }
        Ok(written)
    }

    /// Whether a proxy reports an own enumerable property under `key`: the
    /// `getOwnPropertyDescriptor` trap's answer, or the target's own record.
    fn proxy_own_enumerable(&mut self, proxy: Value, key: Key) -> Result<bool, Completion> {
        let (target, handler) = self.proxy_parts(proxy)?;
        let trap = self.proxy_trap(handler, b"getOwnPropertyDescriptor")?;
        if trap.is_undefined() {
            return self.is_enumerable(target, key);
        }
        let name = self.key_to_value(key)?;
        let descriptor = self.call_value(trap, handler, &[target, name])?;
        if descriptor.is_undefined() {
            return Ok(false);
        }
        if !descriptor.is_object() {
            return Err(self.throw_type_error());
        }
        let field = self.ascii_key(b"enumerable")?;
        let enumerable = self.get_property(descriptor, field)?;
        self.coerce_to_boolean(enumerable)
    }

    /// Whether an object has a property, through a proxy's `has` trap
    /// where the object is one.
    fn has_property_of(&mut self, object: Value, key: Key) -> Result<bool, Completion> {
        if !object.is_object() {
            return Ok(false);
        }
        match object::exotic_kind(self.heap, object.as_handle()).unwrap_or(0) {
            object::exotic::PROXY => return self.proxy_has(object, key),
            object::exotic::DEFERRED if !self.hidden_key(key) => {
                self.deferred_trigger(object, Some(key))?;
            }
            _ if self.deferred_live && !self.hidden_key(key) => {
                self.deferred_chain_trigger(object, key)?;
            }
            object::exotic::TYPED_ARRAY => {
                // An integer index is a property while it is in bounds; any
                // other numeric key names nothing, on the view or beyond it.
                if let Key::Index(index) = key {
                    return Ok(self
                        .typed_array_length(object)?
                        .is_some_and(|count| index < count));
                }
                if self.is_canonical_numeric_key(key)? {
                    return Ok(false);
                }
            }
            _ => {}
        }
        object::has_property(self.heap, object.as_handle(), key)
            .map_err(|_| Completion::Terminated(Termination::Malformed))
    }

    /// Whether a name is a canonical numeric string that is not an integer
    /// index — `"NaN"`, `"-0"`, `"1.5"`, `"Infinity"` — which a typed array
    /// treats as one of its own, and never holds.
    fn is_canonical_numeric_key(&mut self, key: Key) -> Result<bool, Completion> {
        let Key::Name(handle) = key else {
            return Ok(false);
        };
        let first = string::unit_at(self.heap, handle, 0)
            .map_err(|_| self.heap_failure())?
            .unwrap_or(0);
        if !(first == u16::from(b'-')
            || first == u16::from(b'N')
            || first == u16::from(b'I')
            || (u16::from(b'0')..=u16::from(b'9')).contains(&first))
        {
            return Ok(false);
        }
        let text = Value::string(handle);
        let number = self.coerce_to_number(text)?;
        let minus_zero = self.ascii_string(b"-0")?;
        if self.strict_equals(text, minus_zero)? {
            return Ok(true);
        }
        let back = self.coerce_to_string(Value::number(number))?;
        self.strict_equals(back, text)
    }

    /// Find the environment that binds `name`, walking outwards: the
    /// environment module's own walk, except that an object environment's
    /// HasProperty goes through a proxy's `has` trap.
    fn resolve_chain(
        &mut self,
        environment: Handle,
        name: Handle,
    ) -> Result<Option<env::Resolution>, Completion> {
        let mut current = environment;
        let mut depth = 0u32;
        loop {
            let kind = env::kind(self.heap, current)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            if kind == EnvironmentKind::Object {
                let object = env::binding_object(self.heap, current)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                if self.has_property_of(object, Key::Name(name))? {
                    return Ok(Some(env::Resolution {
                        environment: current,
                        index: u32::MAX,
                        depth,
                    }));
                }
            } else if let Some(index) = env::index_of(self.heap, current, name)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?
            {
                return Ok(Some(env::Resolution {
                    environment: current,
                    index,
                    depth,
                }));
            }
            let parent = env::parent(self.heap, current)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            if !parent.is_object() {
                return Ok(None);
            }
            depth += 1;
            if depth > env::MAX_SCOPE_DEPTH {
                return Err(Completion::Terminated(Termination::Malformed));
            }
            current = parent.as_handle();
        }
    }

    /// The handler's trap of a name, or undefined where it has none.
    fn proxy_trap(&mut self, handler: Value, name: &[u8]) -> Result<Value, Completion> {
        let key = self.ascii_key(name)?;
        let trap = self.get_property(handler, key)?;
        if trap.is_nullish() {
            return Ok(Value::UNDEFINED);
        }
        if !self.is_callable_value(trap) {
            return Err(self.throw_type_error());
        }
        Ok(trap)
    }

    fn proxy_get(&mut self, proxy: Value, key: Key) -> Result<Value, Completion> {
        let (target, handler) = self.proxy_parts(proxy)?;
        let trap = self.proxy_trap(handler, b"get")?;
        if trap.is_undefined() {
            return self.get_property(target, key);
        }
        let name = self.key_to_value(key)?;
        self.call_value(trap, handler, &[target, name, proxy])
    }

    fn proxy_set(&mut self, proxy: Value, key: Key, value: Value) -> Result<bool, Completion> {
        let (target, handler) = self.proxy_parts(proxy)?;
        let trap = self.proxy_trap(handler, b"set")?;
        if trap.is_undefined() {
            self.set_property(target, key, value)?;
            return Ok(true);
        }
        let name = self.key_to_value(key)?;
        let answer = self.call_value(trap, handler, &[target, name, value, proxy])?;
        self.coerce_to_boolean(answer)
    }

    fn proxy_has(&mut self, proxy: Value, key: Key) -> Result<bool, Completion> {
        let (target, handler) = self.proxy_parts(proxy)?;
        let trap = self.proxy_trap(handler, b"has")?;
        if trap.is_undefined() {
            return object::has_property(self.heap, target.as_handle(), key)
                .map_err(|_| Completion::Terminated(Termination::Malformed));
        }
        let name = self.key_to_value(key)?;
        let answer = self.call_value(trap, handler, &[target, name])?;
        self.coerce_to_boolean(answer)
    }

    fn proxy_delete(&mut self, proxy: Value, key: Key) -> Result<bool, Completion> {
        let (target, handler) = self.proxy_parts(proxy)?;
        let trap = self.proxy_trap(handler, b"deleteProperty")?;
        if trap.is_undefined() {
            return self.delete_property(target, key);
        }
        let name = self.key_to_value(key)?;
        let answer = self.call_value(trap, handler, &[target, name])?;
        self.coerce_to_boolean(answer)
    }

    /// Define a data property through the `defineProperty` trap, the
    /// descriptor handed over as the object it would be.
    fn proxy_define(
        &mut self,
        proxy: Value,
        key: Key,
        value: Value,
        attributes: u8,
    ) -> Result<bool, Completion> {
        let (target, handler) = self.proxy_parts(proxy)?;
        let trap = self.proxy_trap(handler, b"defineProperty")?;
        if trap.is_undefined() {
            return object::define_own_property(
                self.heap,
                target.as_handle(),
                key,
                Descriptor::data(value, attributes),
            )
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted));
        }
        let descriptor = object::create(self.heap, Value::object(self.realm.object_prototype))
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        for (name, held) in [
            (&b"value"[..], value),
            (
                &b"writable"[..],
                Value::boolean(attributes & attribute::WRITABLE != 0),
            ),
            (
                &b"enumerable"[..],
                Value::boolean(attributes & attribute::ENUMERABLE != 0),
            ),
            (
                &b"configurable"[..],
                Value::boolean(attributes & attribute::CONFIGURABLE != 0),
            ),
        ] {
            let field = self.ascii_key(name)?;
            object::define_own_property(
                self.heap,
                descriptor,
                field,
                Descriptor::data(held, attribute::DEFAULT),
            )
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        }
        let name = self.key_to_value(key)?;
        let answer = self.call_value(trap, handler, &[target, name, Value::object(descriptor)])?;
        self.coerce_to_boolean(answer)
    }

    // ArrayBuffer, Uint8Array, DataView

    /// The array of numbers an ArrayBuffer holds its bytes in.
    fn array_buffer_bytes(&mut self, buffer: Value) -> Result<Value, Completion> {
        if buffer.is_object()
            && object::exotic_kind(self.heap, buffer.as_handle()).unwrap_or(0)
                == object::exotic::ARRAY_BUFFER
        {
            let key = self.ascii_key(b"\0bytes")?;
            if let Some(descriptor) = object::get_own_property(self.heap, buffer.as_handle(), key)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?
            {
                return Ok(descriptor.value);
            }
        }
        Err(self.throw_type_error())
    }

    /// ToIndex: an integer in 0..=2^32-1, or a RangeError.
    fn coerce_to_index(&mut self, value: Value) -> Result<u32, Completion> {
        if value.is_undefined() {
            return Ok(0);
        }
        let wanted = value::truncate(self.coerce_to_number(value)?);
        if !(0.0..=4_294_967_295.0).contains(&wanted) {
            return Err(self.throw_error_of(ErrorKind::Range));
        }
        Ok(wanted as u32)
    }

    /// `new ArrayBuffer(length, { maxByteLength })`, or the shared kind.
    fn construct_array_buffer(
        &mut self,
        length: Value,
        options: Value,
        shared: bool,
    ) -> Result<Value, Completion> {
        let count = self.coerce_to_index(length)?;
        let mut maximum = Value::UNDEFINED;
        if options.is_object() {
            let max_key = self.ascii_key(b"maxByteLength")?;
            let wanted = self.get_property(options, max_key)?;
            if !wanted.is_undefined() {
                let limit = self.coerce_to_index(wanted)?;
                if limit < count {
                    return Err(self.throw_error_of(ErrorKind::Range));
                }
                maximum = Value::number(f64::from(limit));
            }
        }
        let bound = match maximum {
            value if value.is_undefined() => count,
            value => value::to_uint32(value.as_number()),
        };
        if bound > 1_048_576 {
            return Err(self.throw_error_of(ErrorKind::Range));
        }
        let bytes = self.create_array()?;
        let mut index = 0u32;
        while index < count {
            self.append_element(bytes, Some(Value::number(0.0)))?;
            index += 1;
        }
        let prototype = if shared {
            self.realm.shared_array_buffer_prototype
        } else {
            self.realm.array_buffer_prototype
        };
        let made = self.new_instance_of(prototype)?;
        for (name, held) in [(&b"\0bytes"[..], bytes), (&b"\0max"[..], maximum)] {
            let key = self.ascii_key(name)?;
            object::define_own_property(
                self.heap,
                made,
                key,
                Descriptor::data(held, attribute::WRITABLE),
            )
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        }
        object::set_exotic_kind(self.heap, made, object::exotic::ARRAY_BUFFER)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        Ok(Value::object(made))
    }

    /// A buffer's `maxByteLength` record: undefined where it is fixed.
    /// Whether a buffer was made immutable: its bytes never change again.
    fn array_buffer_immutable(&mut self, buffer: Value) -> Result<bool, Completion> {
        self.array_buffer_bytes(buffer)?;
        let key = self.ascii_key(b"\0immutable")?;
        Ok(object::get_own_property(self.heap, buffer.as_handle(), key)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?
            .is_some_and(|descriptor| descriptor.value.is_boolean()))
    }

    fn array_buffer_maximum(&mut self, buffer: Value) -> Result<Value, Completion> {
        self.array_buffer_bytes(buffer)?;
        let key = self.ascii_key(b"\0max")?;
        Ok(object::get_own_property(self.heap, buffer.as_handle(), key)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?
            .map_or(Value::UNDEFINED, |descriptor| descriptor.value))
    }

    /// `buffer.resize(newLength)`: the bytes grow with zeros or shrink, within
    /// the maximum the buffer was made with.
    fn array_buffer_resize(&mut self, buffer: Value, length: Value) -> Result<(), Completion> {
        let bytes = self.array_buffer_bytes(buffer)?;
        let maximum = self.array_buffer_maximum(buffer)?;
        if maximum.is_undefined() {
            return Err(self.throw_type_error());
        }
        let wanted = self.coerce_to_index(length)?;
        if wanted > value::to_uint32(maximum.as_number()) {
            return Err(self.throw_error_of(ErrorKind::Range));
        }
        let current = self.length_of(bytes)?;
        if wanted <= current {
            self.set_length(bytes, wanted)?;
        } else {
            let mut index = current;
            while index < wanted {
                self.append_element(bytes, Some(Value::number(0.0)))?;
                index += 1;
            }
        }
        Ok(())
    }

    /// The parts of a typed array: its buffer, kind, byte offset, and fixed
    /// element count — none for a view that tracks its buffer's length.
    fn typed_array_parts(
        &mut self,
        view: Value,
    ) -> Result<(Value, u8, u32, Option<u32>), Completion> {
        if !view.is_object()
            || object::exotic_kind(self.heap, view.as_handle()).unwrap_or(0)
                != object::exotic::TYPED_ARRAY
        {
            return Err(self.throw_type_error());
        }
        let mut parts = [Value::UNDEFINED; 4];
        for (slot, name) in parts.iter_mut().zip([
            &b"\0buffer"[..],
            &b"\0kind"[..],
            &b"\0offset"[..],
            &b"\0length"[..],
        ]) {
            let key = self.ascii_key(name)?;
            *slot = object::get_own_property(self.heap, view.as_handle(), key)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?
                .map_or(Value::UNDEFINED, |descriptor| descriptor.value);
        }
        let kind = value::to_uint32(parts[1].as_number()) as u8;
        let offset = value::to_uint32(parts[2].as_number());
        let fixed = if parts[3].is_undefined() {
            None
        } else {
            Some(value::to_uint32(parts[3].as_number()))
        };
        Ok((parts[0], kind, offset, fixed))
    }

    /// A typed array's element count, or none where its buffer has shrunk
    /// out from under it.
    fn typed_array_length(&mut self, view: Value) -> Result<Option<u32>, Completion> {
        let (buffer, kind, offset, fixed) = self.typed_array_parts(view)?;
        let bytes = self.array_buffer_bytes(buffer)?;
        let available = self.length_of(bytes)?;
        let size = crate::realm::typed_array_element_size(kind);
        if offset > available {
            return Ok(None);
        }
        match fixed {
            Some(count) => {
                let needed = u64::from(offset) + u64::from(count) * u64::from(size);
                if needed > u64::from(available) {
                    Ok(None)
                } else {
                    Ok(Some(count))
                }
            }
            None => Ok(Some((available - offset) / size)),
        }
    }

    /// A typed array over a buffer: its instance object, of the kind's
    /// prototype unless `prototype` names another.
    fn make_typed_array(
        &mut self,
        kind: u8,
        buffer: Value,
        offset: u32,
        fixed: Option<u32>,
    ) -> Result<Value, Completion> {
        let prototype = self
            .realm
            .typed_array_prototypes
            .get(usize::from(kind))
            .copied()
            .unwrap_or(self.realm.typed_array_prototype);
        let made = self.new_instance_of(prototype)?;
        let length = match fixed {
            Some(count) => Value::number(f64::from(count)),
            None => Value::UNDEFINED,
        };
        for (name, held) in [
            (&b"\0buffer"[..], buffer),
            (&b"\0kind"[..], Value::number(f64::from(kind))),
            (&b"\0offset"[..], Value::number(f64::from(offset))),
            (&b"\0length"[..], length),
        ] {
            let key = self.ascii_key(name)?;
            object::define_own_property(
                self.heap,
                made,
                key,
                Descriptor::data(held, attribute::WRITABLE),
            )
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        }
        object::set_exotic_kind(self.heap, made, object::exotic::TYPED_ARRAY)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        Ok(Value::object(made))
    }

    /// The kind a typed array constructor makes.
    fn typed_array_kind_of(&mut self, constructor: Value) -> Result<u8, Completion> {
        if !constructor.is_object() {
            return Err(self.throw_type_error());
        }
        let key = self.ascii_key(b"\0kind")?;
        let held = object::get_own_property(self.heap, constructor.as_handle(), key)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?
            .map_or(Value::UNDEFINED, |descriptor| descriptor.value);
        if !held.is_number() {
            return Err(self.throw_type_error());
        }
        Ok(value::to_uint32(held.as_number()) as u8)
    }

    /// `new Int8Array(...)` and its kin: over a length, a buffer with an
    /// offset and length, another typed array, an iterable, or an array-like.
    fn construct_typed_array(
        &mut self,
        callee: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let kind = self.typed_array_kind_of(callee)?;
        let size = crate::realm::typed_array_element_size(kind);
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        if !first.is_object() {
            let count = self.coerce_to_index(first)?;
            let buffer = self.construct_array_buffer(
                Value::number(f64::from(count) * f64::from(size)),
                Value::UNDEFINED,
                false,
            )?;
            return self.make_typed_array(kind, buffer, 0, Some(count));
        }
        let exotic = object::exotic_kind(self.heap, first.as_handle()).unwrap_or(0);
        if exotic == object::exotic::ARRAY_BUFFER {
            let offset =
                self.coerce_to_index(arguments.get(1).copied().unwrap_or(Value::UNDEFINED))?;
            if offset % size != 0 {
                return Err(self.throw_error_of(ErrorKind::Range));
            }
            let length = arguments.get(2).copied().unwrap_or(Value::UNDEFINED);
            let bytes = self.array_buffer_bytes(first)?;
            let available = self.length_of(bytes)?;
            let maximum = self.array_buffer_maximum(first)?;
            let fixed = if length.is_undefined() {
                if maximum.is_undefined() {
                    // A fixed buffer's view is fixed at what is there now.
                    if available < offset || (available - offset) % size != 0 {
                        return Err(self.throw_error_of(ErrorKind::Range));
                    }
                    Some((available - offset) / size)
                } else {
                    if offset > available {
                        return Err(self.throw_error_of(ErrorKind::Range));
                    }
                    None
                }
            } else {
                let count = self.coerce_to_index(length)?;
                if u64::from(offset) + u64::from(count) * u64::from(size) > u64::from(available) {
                    return Err(self.throw_error_of(ErrorKind::Range));
                }
                Some(count)
            };
            return self.make_typed_array(kind, first, offset, fixed);
        }
        // A typed array, an iterable, or an array-like: its values, copied.
        let values = self.new_array()?;
        if exotic == object::exotic::TYPED_ARRAY {
            let count = self.typed_array_length(first)?.unwrap_or(0);
            let mut index = 0u32;
            while index < count {
                let held = self.typed_array_read(first, index)?;
                self.append_element(values, Some(held))?;
                index += 1;
            }
        } else if let Some(iterator) = self.iterator_of(first)? {
            while let Some(element) = self.iterator_step(iterator)? {
                self.append_element(values, Some(element))?;
            }
        } else {
            let count = self.length_of(first)?;
            let mut index = 0u32;
            while index < count {
                let held = self.element(first, index)?;
                self.append_element(values, Some(held))?;
                index += 1;
            }
        }
        let count = self.length_of(values)?;
        let buffer = self.construct_array_buffer(
            Value::number(f64::from(count) * f64::from(size)),
            Value::UNDEFINED,
            false,
        )?;
        let made = self.make_typed_array(kind, buffer, 0, Some(count))?;
        let mut index = 0u32;
        while index < count {
            let held = self.element(values, index)?;
            self.typed_array_write(made, index, held)?;
            index += 1;
        }
        Ok(made)
    }

    fn construct_data_view(&mut self, buffer: Value) -> Result<Value, Completion> {
        let bytes = self.array_buffer_bytes(buffer)?;
        let length = self.length_of(bytes)?;
        let made = self.new_instance_of(self.realm.data_view_prototype)?;
        for (name, held) in [
            (&b"buffer"[..], buffer),
            (&b"byteLength"[..], Value::number(f64::from(length))),
            (&b"byteOffset"[..], Value::number(0.0)),
        ] {
            let key = self.ascii_key(name)?;
            object::define_own_property(
                self.heap,
                made,
                key,
                Descriptor::data(held, attribute::CONFIGURABLE),
            )
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        }
        object::set_exotic_kind(self.heap, made, object::exotic::DATA_VIEW)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        Ok(Value::object(made))
    }

    /// Read one element: the bytes at its place, little-endian, as the
    /// kind's value. Undefined past the end, or where the view is out of
    /// its buffer's bounds.
    fn typed_array_read(&mut self, view: Value, index: u32) -> Result<Value, Completion> {
        let Some(count) = self.typed_array_length(view)? else {
            return Ok(Value::UNDEFINED);
        };
        if index >= count {
            return Ok(Value::UNDEFINED);
        }
        let (buffer, kind, offset, _) = self.typed_array_parts(view)?;
        let bytes = self.array_buffer_bytes(buffer)?;
        let size = crate::realm::typed_array_element_size(kind);
        let start = offset + index * size;
        let mut raw = 0u64;
        let mut byte = 0u32;
        while byte < size {
            let held = self.element(bytes, start + byte)?;
            let value = value::to_uint32(held.as_number()) & 0xFF;
            raw |= u64::from(value) << (8 * byte);
            byte += 1;
        }
        let number = match kind {
            0 => f64::from(raw as u8 as i8),
            1 | 2 => f64::from(raw as u8),
            3 => f64::from(raw as u16 as i16),
            4 => f64::from(raw as u16),
            5 => f64::from(raw as u32 as i32),
            6 => f64::from(raw as u32),
            7 => crate::softfloat::from_f32_bits(raw as u32),
            8 => f64::from_bits(raw),
            _ => {
                let negative = kind == 9 && raw & (1u64 << 63) != 0;
                let magnitude = if negative { raw.wrapping_neg() } else { raw };
                let mut big = crate::bigint::Number::ZERO;
                big.limbs[0] = magnitude as u32;
                big.limbs[1] = (magnitude >> 32) as u32;
                big.length = if big.limbs[1] != 0 {
                    2
                } else if big.limbs[0] != 0 {
                    1
                } else {
                    0
                };
                big.negative = negative && big.length != 0;
                return self.big_int_value(&big);
            }
        };
        Ok(Value::number(number))
    }

    /// Write one element: the value converted to the kind first — which may
    /// run a program's `valueOf` — then stored, if the index is in bounds.
    fn typed_array_write(
        &mut self,
        view: Value,
        index: u32,
        value: Value,
    ) -> Result<(), Completion> {
        let (buffer, kind, offset, _) = self.typed_array_parts(view)?;
        // Immutable bytes take no write.
        if self.array_buffer_immutable(buffer).unwrap_or(false) {
            return Err(self.throw_type_error());
        }
        let raw: u64 = if kind >= 9 {
            let big = self.big_int_of(value)?;
            let number = self.big_int_operand(big)?;
            let magnitude = u64::from(number.limbs.first().copied().unwrap_or(0))
                | (u64::from(number.limbs.get(1).copied().unwrap_or(0)) << 32);
            if number.negative {
                magnitude.wrapping_neg()
            } else {
                magnitude
            }
        } else {
            let number = self.coerce_to_number(value)?;
            match kind {
                0 | 1 => u64::from(value::to_uint32(number) & 0xFF),
                2 => {
                    // Clamped: to the nearest, ties to even, within 0..=255.
                    let clamped = if number.is_nan() || number <= 0.0 {
                        0.0
                    } else if number >= 255.0 {
                        255.0
                    } else {
                        let floor = value::floor(number);
                        let fraction = number - floor;
                        if fraction < 0.5 {
                            floor
                        } else if fraction > 0.5 {
                            floor + 1.0
                        } else if floor - value::floor(floor / 2.0) * 2.0 == 0.0 {
                            floor
                        } else {
                            floor + 1.0
                        }
                    };
                    clamped as u64
                }
                3 | 4 => u64::from(value::to_uint32(number) & 0xFFFF),
                5 | 6 => u64::from(value::to_uint32(number)),
                7 => u64::from(crate::softfloat::to_f32_bits(number)),
                _ => number.to_bits(),
            }
        };
        let Some(count) = self.typed_array_length(view)? else {
            return Ok(());
        };
        if index >= count {
            return Ok(());
        }
        let bytes = self.array_buffer_bytes(buffer)?;
        let size = crate::realm::typed_array_element_size(kind);
        let start = offset + index * size;
        let mut byte = 0u32;
        while byte < size {
            let piece = (raw >> (8 * byte)) & 0xFF;
            self.set_element(bytes, start + byte, Value::number(piece as f64))?;
            byte += 1;
        }
        Ok(())
    }

    /// `%TypedArray%.of`, `from`, the accessors, and the methods.
    fn typed_array_native(
        &mut self,
        id: u32,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        match id {
            native::TYPED_ARRAY_OF | native::TYPED_ARRAY_FROM => {
                if !this.is_object()
                    || !object::is_constructor(self.heap, this.as_handle()).unwrap_or(false)
                {
                    return Err(self.throw_type_error());
                }
                let values = self.new_array()?;
                if id == native::TYPED_ARRAY_OF {
                    for &argument in arguments {
                        self.append_element(values, Some(argument))?;
                    }
                } else {
                    let map = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
                    if !map.is_undefined() && !self.is_callable_value(map) {
                        return Err(self.throw_type_error());
                    }
                    let receiver = arguments.get(2).copied().unwrap_or(Value::UNDEFINED);
                    if let Some(iterator) = self.iterator_of(first)? {
                        while let Some(element) = self.iterator_step(iterator)? {
                            self.append_element(values, Some(element))?;
                        }
                    } else {
                        let source = self.coerce_to_object(first)?;
                        let count = self.length_of(source)?;
                        let mut index = 0u32;
                        while index < count {
                            let held = self.element(source, index)?;
                            self.append_element(values, Some(held))?;
                            index += 1;
                        }
                    }
                    if !map.is_undefined() {
                        let count = self.length_of(values)?;
                        let mut index = 0u32;
                        while index < count {
                            let held = self.element(values, index)?;
                            let position = Value::number(f64::from(index));
                            let mapped = self.call_value(map, receiver, &[held, position])?;
                            self.set_element(values, index, mapped)?;
                            index += 1;
                        }
                    }
                }
                let count = self.length_of(values)?;
                self.pending_new_target = this;
                let made = self.construct(this, &[Value::number(f64::from(count))])?;
                let mut index = 0u32;
                while index < count {
                    let held = self.element(values, index)?;
                    let key = Key::Index(index);
                    self.set_property(made, key, held)?;
                    index += 1;
                }
                Ok(made)
            }
            native::TYPED_ARRAY_LENGTH => {
                let count = self.typed_array_length(this)?.unwrap_or(0);
                Ok(Value::number(f64::from(count)))
            }
            native::TYPED_ARRAY_BYTE_LENGTH => {
                let (_, kind, _, _) = self.typed_array_parts(this)?;
                let count = self.typed_array_length(this)?.unwrap_or(0);
                let size = crate::realm::typed_array_element_size(kind);
                Ok(Value::number(f64::from(count) * f64::from(size)))
            }
            native::TYPED_ARRAY_BYTE_OFFSET => {
                let (_, _, offset, _) = self.typed_array_parts(this)?;
                if self.typed_array_length(this)?.is_none() {
                    return Ok(Value::number(0.0));
                }
                Ok(Value::number(f64::from(offset)))
            }
            native::TYPED_ARRAY_BUFFER => {
                let (buffer, _, _, _) = self.typed_array_parts(this)?;
                Ok(buffer)
            }
            native::TYPED_ARRAY_TAG => {
                if !this.is_object()
                    || object::exotic_kind(self.heap, this.as_handle()).unwrap_or(0)
                        != object::exotic::TYPED_ARRAY
                {
                    return Ok(Value::UNDEFINED);
                }
                let (_, kind, _, _) = self.typed_array_parts(this)?;
                let (name, length) = crate::realm::typed_array_name(kind);
                self.ascii_string(name.get(..length).unwrap_or(&[]))
            }
            native::TYPED_ARRAY_VALUES | native::TYPED_ARRAY_KEYS | native::TYPED_ARRAY_ENTRIES => {
                if self.typed_array_length(this)?.is_none() {
                    return Err(self.throw_type_error());
                }
                let kind = match id {
                    native::TYPED_ARRAY_KEYS => ITERATE_KEYS,
                    native::TYPED_ARRAY_ENTRIES => ITERATE_ENTRIES,
                    _ => ITERATE_VALUES,
                };
                let handle = object::create_iterator(
                    self.heap,
                    Value::object(self.realm.iterator_prototype),
                    this,
                    kind,
                )
                .map_err(|_| self.heap_failure())?;
                Ok(Value::object(handle))
            }
            native::TYPED_ARRAY_SUBARRAY => {
                let (buffer, kind, offset, fixed) = self.typed_array_parts(this)?;
                let count = self.typed_array_length(this)?.unwrap_or(0);
                let size = crate::realm::typed_array_element_size(kind);
                let begin = self.relative_index(first, count, 0)?;
                let end_value = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
                let new_fixed = if fixed.is_none() && end_value.is_undefined() {
                    None
                } else {
                    let end = self.relative_index(end_value, count, count)?;
                    Some(end.saturating_sub(begin))
                };
                self.make_typed_array(kind, buffer, offset + begin * size, new_fixed)
            }
            native::TYPED_ARRAY_SET => {
                let target_offset =
                    self.coerce_to_index(arguments.get(1).copied().unwrap_or(Value::UNDEFINED))?;
                let count = self.typed_array_length(this)?.unwrap_or(0);
                let source = self.coerce_to_object(first)?;
                let source_count = if object::exotic_kind(self.heap, source.as_handle())
                    .unwrap_or(0)
                    == object::exotic::TYPED_ARRAY
                {
                    self.typed_array_length(source)?.unwrap_or(0)
                } else {
                    self.length_of(source)?
                };
                if u64::from(target_offset) + u64::from(source_count) > u64::from(count) {
                    return Err(self.throw_error_of(ErrorKind::Range));
                }
                let mut index = 0u32;
                while index < source_count {
                    let held = self.element(source, index)?;
                    self.typed_array_write(this, target_offset + index, held)?;
                    index += 1;
                }
                Ok(Value::UNDEFINED)
            }
            native::TYPED_ARRAY_FILL => {
                let (_, kind, _, _) = self.typed_array_parts(this)?;
                let count = self.typed_array_length(this)?.unwrap_or(0);
                let value = if kind >= 9 {
                    self.big_int_of(first)?
                } else {
                    Value::number(self.coerce_to_number(first)?)
                };
                let start = self.relative_index(
                    arguments.get(1).copied().unwrap_or(Value::UNDEFINED),
                    count,
                    0,
                )?;
                let end = self.relative_index(
                    arguments.get(2).copied().unwrap_or(Value::UNDEFINED),
                    count,
                    count,
                )?;
                let mut index = start;
                while index < end {
                    self.typed_array_write(this, index, value)?;
                    index += 1;
                }
                Ok(this)
            }
            native::ARRAY_BUFFER_RESIZE => {
                if self.array_buffer_immutable(this)? {
                    return Err(self.throw_type_error());
                }
                self.array_buffer_resize(this, first)?;
                Ok(Value::UNDEFINED)
            }
            native::ARRAY_BUFFER_IMMUTABLE => {
                Ok(Value::boolean(self.array_buffer_immutable(this)?))
            }
            native::ARRAY_BUFFER_TRANSFER_TO_IMMUTABLE => {
                // The bytes move to a fresh buffer nothing can change; the
                // old buffer keeps them, its claim on immutability gone —
                // detachment is not modelled here, only the new buffer's
                // permanence is.
                let bytes = self.array_buffer_bytes(this)?;
                let count = self.length_of(bytes)?;
                let held =
                    object::create(self.heap, Value::object(self.realm.array_buffer_prototype))
                        .map_err(|_| self.heap_failure())?;
                let target = Value::object(held);
                object::set_exotic_kind(self.heap, held, object::exotic::ARRAY_BUFFER)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                let copied = self.new_array()?;
                let mut index = 0u32;
                while index < count {
                    let value = self.element(bytes, index)?;
                    self.set_element(copied, index, value)?;
                    index += 1;
                }
                self.set_length(copied, count)?;
                let bytes_key = self.ascii_key(b"\0bytes")?;
                object::define_own_property(
                    self.heap,
                    held,
                    bytes_key,
                    Descriptor::data(copied, 0),
                )
                .map_err(|_| self.heap_failure())?;
                let marker = self.ascii_key(b"\0immutable")?;
                object::define_own_property(
                    self.heap,
                    held,
                    marker,
                    Descriptor::data(Value::boolean(true), 0),
                )
                .map_err(|_| self.heap_failure())?;
                Ok(target)
            }
            native::ARRAY_BUFFER_BYTE_LENGTH => {
                let bytes = self.array_buffer_bytes(this)?;
                let count = self.length_of(bytes)?;
                Ok(Value::number(f64::from(count)))
            }
            native::ARRAY_BUFFER_MAX_BYTE_LENGTH => {
                let maximum = self.array_buffer_maximum(this)?;
                if maximum.is_undefined() {
                    let bytes = self.array_buffer_bytes(this)?;
                    let count = self.length_of(bytes)?;
                    return Ok(Value::number(f64::from(count)));
                }
                Ok(maximum)
            }
            native::ARRAY_BUFFER_RESIZABLE => {
                let maximum = self.array_buffer_maximum(this)?;
                Ok(Value::boolean(!maximum.is_undefined()))
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
    }
}

/// Realms one machine may hold, and units whose realm it tracks.
const MAX_REALMS: usize = 4;
const MAX_UNIT_REALMS: usize = 320;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// Make the current realm the one the running frame's unit belongs to.
    fn sync_realm(&mut self) {
        if self.depth == 0 {
            return;
        }
        let module = self.frames[self.depth as usize - 1].module as usize;
        let index = self.unit_realm.get(module).copied().unwrap_or(0);
        if let Some(realm) = self.realms.get(usize::from(index)).copied().flatten() {
            self.realm = realm;
        }
    }

    /// Which realm a function belongs to: a closure's unit's, a native's by
    /// the function prototype it was made over.
    fn realm_index_of_function(&self, function: Handle) -> u8 {
        if object::is_native(self.heap, function).unwrap_or(false) {
            let prototype = object::prototype(self.heap, function).unwrap_or(Value::UNDEFINED);
            if prototype.is_object() {
                for (index, realm) in self.realms.iter().enumerate() {
                    if let Some(realm) = realm {
                        if realm.function_prototype == prototype.as_handle() {
                            return u8::try_from(index).unwrap_or(0);
                        }
                    }
                }
            }
            return 0;
        }
        let module = object::function_module(self.heap, function).unwrap_or(0) as usize;
        self.unit_realm.get(module).copied().unwrap_or(0)
    }

    fn realm_of_function(&self, function: Handle) -> Realm {
        let index = self.realm_index_of_function(function);
        self.realms
            .get(usize::from(index))
            .copied()
            .flatten()
            .unwrap_or(self.realm)
    }

    /// `$262.createRealm()`: a fresh realm beside this one, with its own
    /// host object, answering that object.
    fn create_realm(&mut self) -> Result<Value, Completion> {
        let Some(slot) = self.realms.iter().position(|realm| realm.is_none()) else {
            return Err(self.throw_error_of(ErrorKind::Range));
        };
        let made = match crate::realm::create(self.heap, self.atoms) {
            Ok(made) => made,
            Err(_) => return Err(Completion::Terminated(Termination::HeapExhausted)),
        };
        if crate::realm::install_print(self.heap, self.atoms, &made).is_err()
            || crate::realm::install_262(self.heap, self.atoms, &made).is_err()
            || crate::realm::install_random(self.heap, self.atoms, &made).is_err()
        {
            return Err(Completion::Terminated(Termination::HeapExhausted));
        }
        self.realms[slot] = Some(made);
        let key = self.ascii_key(b"$262")?;
        let host = object::get_own_property(self.heap, made.global, key)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?
            .map_or(Value::UNDEFINED, |descriptor| descriptor.value);
        Ok(host)
    }
}

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// OrdinarySet through a primitive's prototype chain: a setter runs with
    /// the primitive as receiver, a proxy's `set` trap sees the write, and
    /// anything else answers false — nothing was taken.
    fn primitive_write(
        &mut self,
        target: Value,
        key: Key,
        value: Value,
    ) -> Result<bool, Completion> {
        let mut current = match target.tag() {
            Tag::String => Value::object(self.realm.string_prototype),
            Tag::Number => Value::object(self.realm.number_prototype),
            Tag::Boolean => Value::object(self.realm.boolean_prototype),
            Tag::Symbol => Value::object(self.realm.symbol_prototype),
            Tag::BigInt => Value::object(self.realm.big_int_prototype),
            _ => return Ok(false),
        };
        let mut depth = 0u32;
        while current.is_object() && depth < object::MAX_PROTOTYPE_DEPTH {
            let handle = current.as_handle();
            if object::exotic_kind(self.heap, handle).unwrap_or(0) == object::exotic::PROXY {
                let (proxy_target, handler) = self.proxy_parts(current)?;
                let trap = self.proxy_trap(handler, b"set")?;
                if trap.is_undefined() {
                    current = proxy_target;
                    depth += 1;
                    continue;
                }
                let name = self.key_to_value(key)?;
                let answer =
                    self.call_value(trap, handler, &[proxy_target, name, value, target])?;
                return self.coerce_to_boolean(answer);
            }
            let own = object::get_own_property(self.heap, handle, key)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            if let Some(descriptor) = own {
                if matches!(descriptor.kind, object::DescriptorKind::Accessor)
                    && descriptor.setter.is_object()
                {
                    self.call_value(descriptor.setter, target, &[value])?;
                    return Ok(true);
                }
                return Ok(false);
            }
            current = object::prototype(self.heap, handle)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?;
            depth += 1;
        }
        Ok(false)
    }
}
