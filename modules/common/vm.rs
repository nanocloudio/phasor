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
    outbox_length: usize,
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
            outbox_length: 0,
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
}

impl ModuleInstance {
    pub const EMPTY: Self = Self {
        environment: Value::UNDEFINED,
        import_base: 0,
        namespace: Value::UNDEFINED,
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
    heap: &'a mut Heap<'h>,
    atoms: &'a mut Atoms<'atoms>,
    frames: &'a mut [Frame],
    registers: &'a mut [Value],
    depth: u32,
    top: u32,
    accumulator: Value,
    fuel: u64,
    realm: Realm,
    control: Control,
    /// The job queue, when the host gave the machine one. Without it, promises
    /// have nowhere to schedule and say so rather than running a handler at the
    /// wrong time.
    queue: Option<&'a mut Queue<'a>>,
    /// Instructions left in the current slice.
    slice: u64,
    /// Whether a task has been started and not yet finished.
    started: bool,
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
            current_native: None,
            roots_storage: None,
            collection_slice: 0,
            collection_headroom: 0,
            collections: 0,
            trace: 0,
            retained: Value::UNDEFINED,
            pending_eval: Value::UNDEFINED,
            regexp_choices: None,
            regexp_undo: None,
            regexp_subject: None,
            bindings: None,
            outbox: None,
            outbox_length: 0,
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
        self.modules = Some(modules);
        self.imports = Some(imports);
    }

    /// The unit a module runs.
    fn unit_of(&self, module: u32) -> &Unit<'u> {
        match self.units.get(module as usize) {
            Some(unit) => unit,
            None => &self.units[0],
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
        let environment = self.module_environment(source);
        if !environment.is_object() {
            return Err(self.throw_reference_error());
        }
        match env::slot_value(self.heap, environment.as_handle(), slot) {
            Ok(value) => Ok(value),
            Err(env::EnvironmentError::Uninitialised) => Err(self.throw_reference_error()),
            Err(_) => Err(Completion::Terminated(Termination::Malformed)),
        }
    }

    /// The object that names a module's exports.
    ///
    /// Each of its properties reads the module's slot when it is read, so a
    /// namespace shows what the module holds now rather than what it held when
    /// the namespace was made.
    fn namespace_of(&mut self, module: u32) -> Result<Value, Completion> {
        if let Some(modules) = &self.modules {
            if let Some(instance) = modules.get(module as usize) {
                if instance.namespace.is_object() {
                    return Ok(instance.namespace);
                }
            }
        }
        let object = object::create(self.heap, Value::object(self.realm.object_prototype))
            .map_err(|_| self.heap_failure())?;
        let namespace = Value::object(object);
        let count = self.unit_of(module).header().export_count;
        let mut index = 0u32;
        while index < count {
            let Some(record) = self.unit_of(module).export(index) else {
                break;
            };
            let mut units = [0u16; 64];
            let length = {
                let unit = self.unit_of(module);
                let Some(constant) = unit.constant(record.name) else {
                    break;
                };
                unit.constant_units(&constant, &mut units).unwrap_or(0)
            };
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
                Value::number(crate::softfloat::from_u64(u64::from(module))),
            )?;
            self.set_element(
                binding,
                1,
                Value::number(crate::softfloat::from_u64(u64::from(record.slot))),
            )?;
            self.set_length(binding, 2)?;
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
        if let Some(modules) = self.modules.as_deref_mut() {
            if let Some(instance) = modules.get_mut(module as usize) {
                instance.namespace = namespace;
            }
        }
        Ok(namespace)
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
            outbox_length: self.outbox_length,
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
        self.outbox_length = snapshot.outbox_length;
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
                let then_key = self.ascii_key(b"then")?;
                let then = self.get_property(job.target, then_key)?;
                if !self.is_callable_value(then) {
                    // It stopped being a thenable between the resolution and
                    // now, so it is an ordinary value after all.
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
            let then = self.get_property(value, then_key)?;
            if self.is_callable_value(then) {
                let job = Job {
                    kind: JobKind::Adopt,
                    target: value,
                    argument: Value::UNDEFINED,
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
            }
            depth += 1;
        }
        for handle in [
            self.realm.global,
            self.realm.environment,
            self.realm.object_prototype,
            self.realm.array_prototype,
            self.realm.function_prototype,
            self.realm.error_prototype,
            self.realm.promise_prototype,
            self.realm.string_prototype,
            self.realm.number_prototype,
            self.realm.boolean_prototype,
            self.realm.symbol_prototype,
            self.realm.iterator_prototype,
            self.realm.big_int_prototype,
            self.realm.iterator_symbol,
        ] {
            if let Some(slot) = out.get_mut(written) {
                *slot = handle;
            }
            written += 1;
        }
        for &handle in &self.realm.error_prototypes {
            if let Some(slot) = out.get_mut(written) {
                *slot = handle;
            }
            written += 1;
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
            Value::object(self.realm.environment),
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
        self.started = true;
        Ok(())
    }

    /// Begin the unit's entry function without running it.
    pub fn start(&mut self) -> Result<(), Completion> {
        let entry = self.unit_of(self.entry_module).header().entry_function;
        let environment = Value::object(self.realm.environment);
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

    /// Enter the unit the host compiled for the pending eval.
    ///
    /// The unit runs as global code — its top level reads and declares on the
    /// global object — and its completion value answers the `eval` call.
    pub fn enter_eval(&mut self, unit: u32) -> Result<(), Completion> {
        self.pending_eval = Value::UNDEFINED;
        let entry = self.unit_of(unit).header().entry_function;
        let environment = Value::object(self.realm.environment);
        self.push_frame(
            entry,
            environment,
            Value::object(self.realm.global),
            Value::UNDEFINED,
            unit,
        )
    }

    /// Refuse the pending eval: the source did not compile, and the `eval`
    /// call throws a syntax error the program can catch.
    pub fn fail_eval(&mut self) -> Option<Completion> {
        self.pending_eval = Value::UNDEFINED;
        let completion = self.throw_error_of(ErrorKind::Syntax);
        let Completion::Throw(thrown) = completion else {
            return Some(completion);
        };
        self.unwind(thrown, 1)
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
            let outcome = self.call_native(native, this, arguments);
            self.current_native = previous;
            return outcome;
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
        match self.execute() {
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
            if native == crate::realm::native::EVAL && !construct {
                let source = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                if source.is_string() {
                    self.pending_eval = source;
                } else {
                    self.accumulator = source;
                }
                return Ok(true);
            }
            // `Function(...)` is an eval in a wrapper: the parameters and the
            // body are assembled into a function expression, and the value
            // that expression evaluates to answers the call.
            if native == crate::realm::native::FUNCTION {
                let source = self.function_source(arguments)?;
                self.pending_eval = source;
                return Ok(true);
            }
            return Ok(false);
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

    /// The source text `Function(parameters..., body)` denotes.
    fn function_source(&mut self, arguments: &[Value]) -> Result<Value, Completion> {
        let mut source = self.ascii_string(b"(function anonymous(")?;
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
            Value::object(self.realm.object_prototype)
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
        let (slots, arrow, strict) = match self.unit_of(module).function(code) {
            Some(function) => (
                function.context_slots,
                function.flags & record_flag::ARROW != 0,
                function.flags & record_flag::STRICT != 0,
            ),
            None => (0, false, false),
        };
        // An arrow has no `this` of its own, so its environment is an ordinary
        // declarative one and `this` resolves to the enclosing function's.
        let kind = if arrow {
            EnvironmentKind::Declarative
        } else {
            EnvironmentKind::Function
        };
        let record = env::create(self.heap, kind, closure, slots)
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        if !arrow {
            // A call with no receiver binds the global object — unless the
            // body is strict code, which takes `this` exactly as passed.
            let bound = if this.is_nullish() && !strict {
                Value::object(self.realm.global)
            } else {
                this
            };
            env::set_this(self.heap, record, bound)
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        }
        self.declare_slots(record, slots)?;
        Ok(Value::object(record))
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
        };
        self.depth += 1;
        self.top = end;
        Ok(())
    }

    /// Run until the frame that was current on entry returns.
    fn execute(&mut self) -> Completion {
        let floor = self.depth;
        match self.drive(floor, false) {
            Some(completion) => completion,
            // A nested evaluation is never sliced, so it always finishes.
            None => Completion::Terminated(Termination::Malformed),
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
                // The machine is waiting for the host to compile an eval
                // source; nothing runs until it is entered or refused.
                return None;
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
                    self.depth -= 1;
                    self.top = returning.base;
                    // A constructor that returns anything but an object answers
                    // the instance it was building.
                    self.accumulator = if returning.construct && !value.is_object() {
                        returning.this
                    } else {
                        value
                    };
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
                if self.either_is_big_int(left_value, right_value) {
                    self.accumulator =
                        self.big_int_arithmetic(instruction.opcode, left_value, right_value)?;
                    return Ok(Flow::Continue);
                }
                let left = self.coerce_to_number(left_value)?;
                let right = self.coerce_to_number(right_value)?;
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
                if matches!(self.accumulator.tag(), Tag::BigInt) =>
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
                // A symbol is already a key; anything else becomes one by
                // becoming a string.
                if !matches!(value.tag(), Tag::Symbol) {
                    self.accumulator = self.coerce_to_string(value)?;
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
                let present = object::has_property(self.heap, target.as_handle(), key)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                self.accumulator = Value::boolean(present);
            }

            Op::GetNamedProperty => {
                let target = self.register(frame, operands[0]);
                let key = self.constant_key(operands[1])?;
                self.accumulator = self.get_property(target, key)?;
            }
            Op::GetKeyedProperty => {
                let target = self.register(frame, operands[0]);
                let key_value = self.accumulator;
                let key = self.coerce_to_key(key_value)?;
                self.accumulator = self.get_property(target, key)?;
            }
            Op::SetNamedProperty => {
                let target = self.register(frame, operands[0]);
                let key = self.constant_key(operands[1])?;
                let value = self.accumulator;
                self.set_property(target, key, value)?;
            }
            Op::SetKeyedProperty => {
                let target = self.register(frame, operands[0]);
                let key_value = self.register(frame, operands[1]);
                let key = self.coerce_to_key(key_value)?;
                let value = self.accumulator;
                self.set_property(target, key, value)?;
            }
            Op::DeleteNamedProperty => {
                let target = self.accumulator;
                let key = self.constant_key(operands[0])?;
                self.accumulator = Value::boolean(self.delete_property(target, key)?);
            }
            Op::DeleteKeyedProperty => {
                let target = self.register(frame, operands[0]);
                let key_value = self.accumulator;
                let key = self.coerce_to_key(key_value)?;
                self.accumulator = Value::boolean(self.delete_property(target, key)?);
            }

            Op::LdaGlobal => {
                let key = self.constant_key(operands[0])?;
                let present = object::has_property(self.heap, self.realm.global, key)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                if !present {
                    return Err(self.throw_reference_error());
                }
                self.accumulator = self.get_property(Value::object(self.realm.global), key)?;
            }
            Op::LdaGlobalOrUndefined => {
                let key = self.constant_key(operands[0])?;
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
                self.set_property(Value::object(self.realm.global), key, value)?;
            }
            Op::DeclareGlobal => {
                let key = self.constant_key(operands[0])?;
                let present = object::has_property(self.heap, self.realm.global, key)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                if !present {
                    // A `var` that is declared and never assigned still reads
                    // as `undefined` rather than as an unresolvable name.
                    object::define_own_property(
                        self.heap,
                        self.realm.global,
                        key,
                        Descriptor::data(
                            Value::UNDEFINED,
                            attribute::WRITABLE | attribute::ENUMERABLE,
                        ),
                    )
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                }
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
                let arrow = self
                    .unit()
                    .function(index)
                    .is_some_and(|record| record.flags & record_flag::ARROW != 0);
                let flags = if arrow {
                    0
                } else {
                    object::function_flag::CONSTRUCTOR
                };
                // The closure belongs to the module whose code made it, so it
                // runs that module's unit wherever it is called from.
                let function = object::create_function_in(
                    self.heap,
                    Value::object(self.realm.function_prototype),
                    index,
                    frame.environment,
                    flags,
                    frame.module,
                )
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                self.accumulator = Value::object(function);
            }
            Op::SetPrototype => {
                let target = self.register(frame, operands[0]);
                let value = self.accumulator;
                if target.is_object() && (value.is_object() || value.is_null()) {
                    object::set_prototype(self.heap, target.as_handle(), value)
                        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                }
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
                    return Ok(Flow::Enter);
                }
                let result = self.call_value(callee, receiver, arguments)?;
                self.accumulator = result;
            }
            Op::GetIterator => {
                let value = self.accumulator;
                match self.iterator_of(value)? {
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

            Op::Call | Op::CallProperty => {
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
                if self.enter_call(callee, receiver, &arguments[..passed], false)? {
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
                    && object::function_code(self.heap, callee.as_handle())
                        == Ok(crate::realm::native::FUNCTION)
                {
                    let source = self.function_source(&arguments[..passed])?;
                    self.pending_eval = source;
                    return Ok(Flow::Continue);
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
            Op::JumpIfNullish | Op::JumpIfNotNullish => {
                let nullish = self.accumulator.is_nullish();
                if nullish == matches!(instruction.opcode, Op::JumpIfNullish) {
                    return Ok(Flow::Jump(self.jump_target(frame, signed[0])));
                }
            }
            Op::Return => return Ok(Flow::Return(self.accumulator)),
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
            return Err(Completion::Terminated(Termination::HeapExhausted));
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
                    return Err(Completion::Terminated(Termination::HeapExhausted));
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
                let mut digits = [0u8; 256];
                let length = self
                    .unit()
                    .constant_bytes(&constant)
                    .map(|bytes| {
                        let length = bytes.len().min(digits.len());
                        digits[..length].copy_from_slice(&bytes[..length]);
                        length
                    })
                    .ok_or(Completion::Terminated(Termination::Malformed))?;
                // The constant carries its radix in front of its digits.
                let radix = u32::from(digits.first().copied().unwrap_or(10));
                let number =
                    crate::bigint::from_digits(digits.get(1..length).unwrap_or(&[]), radix, false)
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
            (Tag::Object, Tag::Number | Tag::String) => {
                let primitive = self.coerce_to_primitive(left, Hint::Default)?;
                self.loose_equals(primitive, right)
            }
            (Tag::Number | Tag::String, Tag::Object) => {
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
        if object::is_callable(self.heap, handle) != Ok(true)
            || object::is_native(self.heap, handle).unwrap_or(true)
        {
            return Ok(());
        }
        let length_key = self.ascii_key(b"length")?;
        let name_key = self.ascii_key(b"name")?;
        if key != length_key && key != name_key {
            return Ok(());
        }
        if object::get_own_property(self.heap, handle, key)
            .unwrap_or(None)
            .is_some()
        {
            return Ok(());
        }
        let value = if key == length_key {
            let code = object::function_code(self.heap, handle).unwrap_or(0);
            let module = object::function_module(self.heap, handle).unwrap_or(0);
            let count = self
                .unit_of(module)
                .function(code)
                .map_or(0, |record| record.argument_count);
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

    fn set_property(&mut self, target: Value, key: Key, value: Value) -> Result<(), Completion> {
        if target.is_nullish() {
            return Err(self.throw_type_error());
        }
        if !target.is_object() {
            // A write to a primitive is discarded outside strict mode.
            return Ok(());
        }
        // Assigning an array's `length` drops what is now past the end.
        if let Key::Name(_) = key {
            if self.is_array(target)? {
                let length_key = self.ascii_key(b"length")?;
                if key == length_key {
                    let old = self.length_of(target)?;
                    let wanted = self.coerce_to_number(value)?;
                    let new = value::to_uint32(wanted);
                    let outcome = object::set(self.heap, target.as_handle(), key, value)
                        .map_err(|_| Completion::Terminated(Termination::Malformed))?;
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
        let outcome = object::set(self.heap, target.as_handle(), key, value)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        match outcome {
            Assignment::Done | Assignment::Refused => {
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

    fn define_property(&mut self, target: Value, key: Key, value: Value) -> Result<(), Completion> {
        if !target.is_object() {
            return Err(self.throw_type_error());
        }
        object::define_own_property(
            self.heap,
            target.as_handle(),
            key,
            Descriptor::data(value, attribute::DEFAULT),
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(())
    }

    fn delete_property(&mut self, target: Value, key: Key) -> Result<bool, Completion> {
        if !target.is_object() {
            return Ok(true);
        }
        object::delete(self.heap, target.as_handle(), key)
            .map_err(|_| Completion::Terminated(Termination::Malformed))
    }

    fn copy_data_properties(&mut self, target: Value, source: Value) -> Result<(), Completion> {
        if !source.is_object() || !target.is_object() {
            return Ok(());
        }
        let mut keys = [Key::Index(0); MAX_COPIED_KEYS];
        let written = object::own_keys(self.heap, source.as_handle(), &mut keys)
            .map_err(Self::key_failure)?;
        let mut index = 0usize;
        while index < written {
            let key = keys[index];
            let value = self.get_property(source, key)?;
            self.define_property(target, key, value)?;
            index += 1;
        }
        Ok(())
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
    fn this_value(&mut self, frame: &Frame) -> Result<Value, Completion> {
        let mut environment = frame.environment;
        let mut depth = 0u32;
        while environment.is_object() && depth <= env::MAX_SCOPE_DEPTH {
            let handle = environment.as_handle();
            if env::kind(self.heap, handle) == Ok(EnvironmentKind::Function) {
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

    /// The natives of `Object`.
    fn object_native(
        &mut self,
        id: u32,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        let second = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
        match id {
            native::OBJECT => {
                if first.is_nullish() {
                    let object =
                        object::create(self.heap, Value::object(self.realm.object_prototype))
                            .map_err(|_| self.heap_failure())?;
                    return Ok(Value::object(object));
                }
                self.coerce_to_object(first)
            }
            native::OBJECT_KEYS | native::OBJECT_VALUES | native::OBJECT_ENTRIES => {
                self.own_entries(first, id)
            }
            native::OBJECT_ASSIGN => {
                let target = self.coerce_to_object(first)?;
                for &source in arguments.get(1..).unwrap_or(&[]) {
                    if source.is_nullish() {
                        continue;
                    }
                    let source = self.coerce_to_object(source)?;
                    let mut keys = [Key::Index(0); MAX_OWN_KEYS];
                    let count = object::own_keys(self.heap, source.as_handle(), &mut keys)
                        .map_err(Self::key_failure)?;
                    for &key in keys.get(..count).unwrap_or(&[]) {
                        if !self.is_enumerable(source, key)? {
                            continue;
                        }
                        let value = self.get_property(source, key)?;
                        self.set_property(target, key, value)?;
                    }
                }
                Ok(target)
            }
            native::OBJECT_FREEZE => {
                if first.is_object() {
                    object::prevent_extensions(self.heap, first.as_handle())
                        .map_err(|_| self.heap_failure())?;
                    let mut keys = [Key::Index(0); MAX_OWN_KEYS];
                    let count = object::own_keys(self.heap, first.as_handle(), &mut keys)
                        .map_err(Self::key_failure)?;
                    for &key in keys.get(..count).unwrap_or(&[]) {
                        let Some(descriptor) =
                            object::get_own_property(self.heap, first.as_handle(), key)
                                .map_err(|_| self.heap_failure())?
                        else {
                            continue;
                        };
                        let frozen = Descriptor {
                            attributes: descriptor.attributes
                                & !(attribute::WRITABLE | attribute::CONFIGURABLE),
                            ..descriptor
                        };
                        object::define_own_property(self.heap, first.as_handle(), key, frozen)
                            .map_err(|_| self.heap_failure())?;
                    }
                }
                Ok(first)
            }
            native::OBJECT_IS_FROZEN => {
                if !first.is_object() {
                    return Ok(Value::boolean(true));
                }
                if object::is_extensible(self.heap, first.as_handle())
                    .map_err(|_| self.heap_failure())?
                {
                    return Ok(Value::boolean(false));
                }
                let mut keys = [Key::Index(0); MAX_OWN_KEYS];
                let count = object::own_keys(self.heap, first.as_handle(), &mut keys)
                    .map_err(Self::key_failure)?;
                for &key in keys.get(..count).unwrap_or(&[]) {
                    let Some(descriptor) =
                        object::get_own_property(self.heap, first.as_handle(), key)
                            .map_err(|_| self.heap_failure())?
                    else {
                        continue;
                    };
                    if descriptor.has(attribute::WRITABLE)
                        || descriptor.has(attribute::CONFIGURABLE)
                    {
                        return Ok(Value::boolean(false));
                    }
                }
                Ok(Value::boolean(true))
            }
            native::OBJECT_GET_PROTOTYPE_OF => {
                let object = self.coerce_to_object(first)?;
                object::prototype(self.heap, object.as_handle()).map_err(|_| self.heap_failure())
            }
            native::OBJECT_SET_PROTOTYPE_OF => {
                if first.is_object() {
                    object::set_prototype(self.heap, first.as_handle(), second)
                        .map_err(|_| self.heap_failure())?;
                }
                Ok(first)
            }
            native::OBJECT_DEFINE_PROPERTY => {
                if !first.is_object() {
                    return Err(self.throw_type_error());
                }
                let key = self.coerce_to_key(second)?;
                let descriptor = arguments.get(2).copied().unwrap_or(Value::UNDEFINED);
                if !descriptor.is_object() {
                    return Err(self.throw_type_error());
                }
                // A redefinition changes only what the descriptor states: an
                // absent field keeps what the property already has.
                let current = object::get_own_property(self.heap, first.as_handle(), key)
                    .map_err(|_| self.heap_failure())?;
                let mut attributes = current.map_or(0, |existing| existing.attributes);
                let mut described_value = false;
                let mut described_accessor = false;
                let mut value = current.map_or(Value::UNDEFINED, |existing| existing.value);
                let mut getter = current.map_or(Value::UNDEFINED, |existing| existing.getter);
                let mut setter = current.map_or(Value::UNDEFINED, |existing| existing.setter);
                for (name, bit) in [
                    (&b"writable"[..], attribute::WRITABLE),
                    (&b"enumerable"[..], attribute::ENUMERABLE),
                    (&b"configurable"[..], attribute::CONFIGURABLE),
                ] {
                    let field = self.ascii_key(name)?;
                    if object::has_property(self.heap, descriptor.as_handle(), field)
                        .map_err(|_| self.heap_failure())?
                    {
                        if bit == attribute::WRITABLE {
                            described_value = true;
                        }
                        let flag = self.get_property(descriptor, field)?;
                        if self.coerce_to_boolean(flag)? {
                            attributes |= bit;
                        } else {
                            attributes &= !bit;
                        }
                    }
                }
                for (name, slot) in [(&b"get"[..], &mut getter), (&b"set"[..], &mut setter)] {
                    let field = self.ascii_key(name)?;
                    if object::has_property(self.heap, descriptor.as_handle(), field)
                        .map_err(|_| self.heap_failure())?
                    {
                        described_accessor = true;
                        *slot = self.get_property(descriptor, field)?;
                    }
                }
                let value_field = self.ascii_key(b"value")?;
                if object::has_property(self.heap, descriptor.as_handle(), value_field)
                    .map_err(|_| self.heap_failure())?
                {
                    described_value = true;
                    value = self.get_property(descriptor, value_field)?;
                }
                if described_value && described_accessor {
                    return Err(self.throw_type_error());
                }
                let accessor = described_accessor
                    || (!described_value
                        && current.is_some_and(|existing| {
                            matches!(existing.kind, object::DescriptorKind::Accessor)
                        }));
                let completed = if accessor {
                    Descriptor::accessor(getter, setter, attributes)
                } else {
                    Descriptor::data(value, attributes)
                };
                let admitted =
                    object::define_own_property(self.heap, first.as_handle(), key, completed)
                        .map_err(|_| self.heap_failure())?;
                if !admitted {
                    return Err(self.throw_type_error());
                }
                Ok(first)
            }
            native::OBJECT_GET_OWN_PROPERTY_NAMES => self.own_entries(first, native::OBJECT_KEYS),
            native::OBJECT_CREATE => {
                let prototype = if first.is_nullish() {
                    Value::NULL
                } else {
                    first
                };
                let object =
                    object::create(self.heap, prototype).map_err(|_| self.heap_failure())?;
                Ok(Value::object(object))
            }
            native::OBJECT_IS => Ok(Value::boolean(value::same_value(first, second))),
            native::OBJECT_GET_OWN_PROPERTY_DESCRIPTOR => {
                if !first.is_object() {
                    return Err(self.throw_type_error());
                }
                let key = self.coerce_to_key(second)?;
                let Some(found) = object::get_own_property(self.heap, first.as_handle(), key)
                    .map_err(|_| self.heap_failure())?
                else {
                    return Ok(Value::UNDEFINED);
                };
                let result = object::create(self.heap, Value::object(self.realm.object_prototype))
                    .map_err(|_| self.heap_failure())?;
                let result = Value::object(result);
                if matches!(found.kind, object::DescriptorKind::Data) {
                    let value_key = self.ascii_key(b"value")?;
                    self.set_property(result, value_key, found.value)?;
                    let writable_key = self.ascii_key(b"writable")?;
                    let writable = Value::boolean(found.has(attribute::WRITABLE));
                    self.set_property(result, writable_key, writable)?;
                } else {
                    let get_key = self.ascii_key(b"get")?;
                    self.set_property(result, get_key, found.getter)?;
                    let set_key = self.ascii_key(b"set")?;
                    self.set_property(result, set_key, found.setter)?;
                }
                let enumerable_key = self.ascii_key(b"enumerable")?;
                let enumerable = Value::boolean(found.has(attribute::ENUMERABLE));
                self.set_property(result, enumerable_key, enumerable)?;
                let configurable_key = self.ascii_key(b"configurable")?;
                let configurable = Value::boolean(found.has(attribute::CONFIGURABLE));
                self.set_property(result, configurable_key, configurable)?;
                Ok(result)
            }
            native::OBJECT_HAS_OWN_PROPERTY => {
                let object = self.coerce_to_object(this)?;
                let key = self.coerce_to_key(first)?;
                let present = object::get_own_property(self.heap, object.as_handle(), key)
                    .map_err(|_| self.heap_failure())?
                    .is_some();
                Ok(Value::boolean(present))
            }
            native::OBJECT_IS_PROTOTYPE_OF => {
                if !first.is_object() || !this.is_object() {
                    return Ok(Value::boolean(false));
                }
                let mut current = object::prototype(self.heap, first.as_handle())
                    .map_err(|_| self.heap_failure())?;
                let mut depth = 0u32;
                while current.is_object() && depth < object::MAX_PROTOTYPE_DEPTH {
                    if current.as_handle() == this.as_handle() {
                        return Ok(Value::boolean(true));
                    }
                    current = object::prototype(self.heap, current.as_handle())
                        .map_err(|_| self.heap_failure())?;
                    depth += 1;
                }
                Ok(Value::boolean(false))
            }
            native::OBJECT_PROPERTY_IS_ENUMERABLE => {
                let object = self.coerce_to_object(this)?;
                let key = self.coerce_to_key(first)?;
                Ok(Value::boolean(self.is_enumerable(object, key)?))
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
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
        let result = self.new_array()?;
        let mut keys = [Key::Index(0); MAX_OWN_KEYS];
        let count = object::own_keys(self.heap, object.as_handle(), &mut keys)
            .map_err(Self::key_failure)?;
        let mut written = 0u32;
        for &key in keys.get(..count).unwrap_or(&[]) {
            if matches!(key, Key::Symbol(_)) {
                continue;
            }
            if kind != native::OBJECT_GET_OWN_PROPERTY_NAMES && !self.is_enumerable(object, key)? {
                continue;
            }
            let name = self.key_to_value(key)?;
            let entry = match kind {
                native::OBJECT_KEYS | native::OBJECT_GET_OWN_PROPERTY_NAMES => name,
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

    /// The natives of `Array`.
    fn array_native(
        &mut self,
        id: u32,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        let second = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
        match id {
            native::ARRAY => {
                let array = self.new_array()?;
                if arguments.len() == 1 && matches!(first.tag(), Tag::Number) {
                    let length = value::to_uint32(first.as_number());
                    self.set_length(array, length)?;
                    return Ok(array);
                }
                for (index, &value) in arguments.iter().enumerate() {
                    let index = u32::try_from(index).unwrap_or(0);
                    self.set_element(array, index, value)?;
                }
                self.set_length(array, u32::try_from(arguments.len()).unwrap_or(0))?;
                Ok(array)
            }
            native::ARRAY_IS_ARRAY => {
                if !first.is_object() {
                    return Ok(Value::boolean(false));
                }
                let prototype = object::prototype(self.heap, first.as_handle())
                    .map_err(|_| self.heap_failure())?;
                Ok(Value::boolean(
                    prototype.is_object() && prototype.as_handle() == self.realm.array_prototype,
                ))
            }
            native::ARRAY_OF => {
                let array = self.new_array()?;
                for (index, &value) in arguments.iter().enumerate() {
                    self.set_element(array, u32::try_from(index).unwrap_or(0), value)?;
                }
                self.set_length(array, u32::try_from(arguments.len()).unwrap_or(0))?;
                Ok(array)
            }
            native::ARRAY_FROM => {
                let array = self.new_array()?;
                let mut written = 0u32;
                let mapper = second;
                if let Some(iterator) = self.iterator_of(first)? {
                    loop {
                        let Some(value) = self.iterator_step(iterator)? else {
                            break;
                        };
                        let value = if mapper.is_undefined() {
                            value
                        } else {
                            let index =
                                Value::number(crate::softfloat::from_u64(u64::from(written)));
                            self.call_with(mapper, Value::UNDEFINED, &[value, index])?
                        };
                        self.set_element(array, written, value)?;
                        written += 1;
                    }
                } else {
                    let length = self.length_of(first)?;
                    while written < length {
                        let value = self.element(first, written)?;
                        let value = if mapper.is_undefined() {
                            value
                        } else {
                            let index =
                                Value::number(crate::softfloat::from_u64(u64::from(written)));
                            self.call_with(mapper, Value::UNDEFINED, &[value, index])?
                        };
                        self.set_element(array, written, value)?;
                        written += 1;
                    }
                }
                self.set_length(array, written)?;
                Ok(array)
            }
            native::ARRAY_PUSH => {
                let mut length = self.length_of(this)?;
                for &value in arguments {
                    self.set_element(this, length, value)?;
                    length += 1;
                }
                self.set_length(this, length)?;
                Ok(Value::number(crate::softfloat::from_u64(u64::from(length))))
            }
            native::ARRAY_POP => {
                let length = self.length_of(this)?;
                if length == 0 {
                    return Ok(Value::UNDEFINED);
                }
                let value = self.element(this, length - 1)?;
                self.delete_element(this, length - 1)?;
                self.set_length(this, length - 1)?;
                Ok(value)
            }
            native::ARRAY_SHIFT => {
                let length = self.length_of(this)?;
                if length == 0 {
                    return Ok(Value::UNDEFINED);
                }
                let value = self.element(this, 0)?;
                let mut index = 1u32;
                while index < length {
                    let moved = self.element(this, index)?;
                    self.set_element(this, index - 1, moved)?;
                    index += 1;
                }
                self.delete_element(this, length - 1)?;
                self.set_length(this, length - 1)?;
                Ok(value)
            }
            native::ARRAY_UNSHIFT => {
                let length = self.length_of(this)?;
                let count = u32::try_from(arguments.len()).unwrap_or(0);
                let mut index = length;
                while index > 0 {
                    index -= 1;
                    let moved = self.element(this, index)?;
                    self.set_element(this, index + count, moved)?;
                }
                for (offset, &value) in arguments.iter().enumerate() {
                    self.set_element(this, u32::try_from(offset).unwrap_or(0), value)?;
                }
                let total = length + count;
                self.set_length(this, total)?;
                Ok(Value::number(crate::softfloat::from_u64(u64::from(total))))
            }
            native::ARRAY_SLICE => {
                let length = self.length_of(this)?;
                let start = self.relative_index(first, length, 0)?;
                let end = self.relative_index(second, length, length)?;
                let array = self.new_array()?;
                let mut index = start;
                let mut written = 0u32;
                while index < end {
                    let value = self.element(this, index)?;
                    self.set_element(array, written, value)?;
                    written += 1;
                    index += 1;
                }
                self.set_length(array, written)?;
                Ok(array)
            }
            native::ARRAY_INDEX_OF | native::ARRAY_INCLUDES => {
                let length = self.length_of(this)?;
                let mut index = 0u32;
                while index < length {
                    let value = self.element(this, index)?;
                    let same = if id == native::ARRAY_INCLUDES {
                        value::same_value_zero(value, first)
                    } else {
                        self.strict_equals(value, first)?
                    };
                    if same {
                        return Ok(if id == native::ARRAY_INCLUDES {
                            Value::boolean(true)
                        } else {
                            Value::number(crate::softfloat::from_u64(u64::from(index)))
                        });
                    }
                    index += 1;
                }
                Ok(if id == native::ARRAY_INCLUDES {
                    Value::boolean(false)
                } else {
                    Value::number(-1.0)
                })
            }
            native::ARRAY_CONCAT => {
                let array = self.new_array()?;
                let mut written = 0u32;
                let length = self.length_of(this)?;
                let mut index = 0u32;
                while index < length {
                    let value = self.element(this, index)?;
                    self.set_element(array, written, value)?;
                    written += 1;
                    index += 1;
                }
                for &argument in arguments {
                    if self.is_array(argument)? {
                        let length = self.length_of(argument)?;
                        let mut index = 0u32;
                        while index < length {
                            let value = self.element(argument, index)?;
                            self.set_element(array, written, value)?;
                            written += 1;
                            index += 1;
                        }
                    } else {
                        self.set_element(array, written, argument)?;
                        written += 1;
                    }
                }
                self.set_length(array, written)?;
                Ok(array)
            }
            native::ARRAY_MAP
            | native::ARRAY_FILTER
            | native::ARRAY_FOR_EACH
            | native::ARRAY_SOME
            | native::ARRAY_EVERY
            | native::ARRAY_FIND
            | native::ARRAY_FIND_INDEX => self.array_walk(id, this, first, second),
            native::ARRAY_REDUCE => {
                let length = self.length_of(this)?;
                let mut index = 0u32;
                let mut accumulator = if arguments.len() > 1 {
                    second
                } else {
                    if length == 0 {
                        return Err(self.throw_type_error());
                    }
                    index = 1;
                    self.element(this, 0)?
                };
                while index < length {
                    let value = self.element(this, index)?;
                    let position = Value::number(crate::softfloat::from_u64(u64::from(index)));
                    accumulator = self.call_with(
                        first,
                        Value::UNDEFINED,
                        &[accumulator, value, position, this],
                    )?;
                    index += 1;
                }
                Ok(accumulator)
            }
            native::ARRAY_REVERSE => {
                let length = self.length_of(this)?;
                let mut low = 0u32;
                let mut high = length.saturating_sub(1);
                while low < high {
                    let left = self.element(this, low)?;
                    let right = self.element(this, high)?;
                    self.set_element(this, low, right)?;
                    self.set_element(this, high, left)?;
                    low += 1;
                    high -= 1;
                }
                Ok(this)
            }
            native::ARRAY_FILL => {
                let length = self.length_of(this)?;
                let start = self.relative_index(second, length, 0)?;
                let end = self.relative_index(
                    arguments.get(2).copied().unwrap_or(Value::UNDEFINED),
                    length,
                    length,
                )?;
                let mut index = start;
                while index < end {
                    self.set_element(this, index, first)?;
                    index += 1;
                }
                Ok(this)
            }
            native::ARRAY_SORT => self.sort_array(this, first),
            native::ARRAY_VALUES | native::ARRAY_KEYS | native::ARRAY_ENTRIES => {
                let kind = match id {
                    native::ARRAY_KEYS => ITERATE_KEYS,
                    native::ARRAY_ENTRIES => ITERATE_ENTRIES,
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
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
    }

    fn is_array(&mut self, value: Value) -> Result<bool, Completion> {
        if !value.is_object() {
            return Ok(false);
        }
        let prototype =
            object::prototype(self.heap, value.as_handle()).map_err(|_| self.heap_failure())?;
        Ok(prototype.is_object() && prototype.as_handle() == self.realm.array_prototype)
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

    /// The natives of `String`.
    fn string_native(
        &mut self,
        id: u32,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        let second = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
        if id == native::STRING {
            if arguments.is_empty() {
                return self.ascii_string(b"");
            }
            if matches!(first.tag(), Tag::Symbol) {
                return self.symbol_text(first);
            }
            return self.coerce_to_string(first);
        }
        if id == native::STRING_FROM_CHAR_CODE {
            let mut units = [0u16; MAX_ARGUMENTS];
            let mut count = 0usize;
            for &argument in arguments {
                let number = self.coerce_to_number(argument)?;
                if let Some(slot) = units.get_mut(count) {
                    *slot = value::to_uint16(number);
                    count += 1;
                }
            }
            return self.make_string(units.get(..count).unwrap_or(&[]));
        }

        let receiver = self.primitive_this(this)?;
        let text = self.string_handle(receiver)?;
        let length = string::length(self.heap, text).map_err(|_| self.heap_failure())?;
        match id {
            native::STRING_TO_STRING => Ok(Value::string(text)),
            native::STRING_CHAR_AT | native::STRING_AT => {
                let number = if first.is_undefined() {
                    0.0
                } else {
                    self.coerce_to_number(first)?
                };
                let index = value::truncate(number);
                let index = if id == native::STRING_AT && index < 0.0 {
                    f64::from(length) + index
                } else {
                    index
                };
                if index < 0.0 || index >= f64::from(length) {
                    return if id == native::STRING_AT {
                        Ok(Value::UNDEFINED)
                    } else {
                        self.ascii_string(b"")
                    };
                }
                let unit = string::unit_at(self.heap, text, value::to_uint32(index))
                    .map_err(|_| self.heap_failure())?
                    .unwrap_or(0);
                self.make_string(&[unit])
            }
            native::STRING_CHAR_CODE_AT => {
                let index = self.index_argument(first)?;
                match string::unit_at(self.heap, text, index).map_err(|_| self.heap_failure())? {
                    Some(unit) => Ok(Value::number(crate::softfloat::from_u64(u64::from(unit)))),
                    None => Ok(Value::number(f64::NAN)),
                }
            }
            native::STRING_CODE_POINT_AT => {
                let index = self.index_argument(first)?;
                match string::code_point_at(self.heap, text, index)
                    .map_err(|_| self.heap_failure())?
                {
                    Some((point, _)) => {
                        Ok(Value::number(crate::softfloat::from_u64(u64::from(point))))
                    }
                    None => Ok(Value::UNDEFINED),
                }
            }
            native::STRING_INDEX_OF | native::STRING_LAST_INDEX_OF | native::STRING_INCLUDES => {
                let needle = self.string_handle(first)?;
                let found = if id == native::STRING_LAST_INDEX_OF {
                    string::last_index_of(self.heap, text, needle)
                        .map_err(|_| self.heap_failure())?
                } else {
                    let from = if second.is_undefined() {
                        0
                    } else {
                        self.index_argument(second)?
                    };
                    string::index_of(self.heap, text, needle, from)
                        .map_err(|_| self.heap_failure())?
                };
                if id == native::STRING_INCLUDES {
                    return Ok(Value::boolean(found.is_some()));
                }
                Ok(match found {
                    Some(index) => Value::number(crate::softfloat::from_u64(u64::from(index))),
                    None => Value::number(-1.0),
                })
            }
            native::STRING_STARTS_WITH | native::STRING_ENDS_WITH => {
                let needle = self.string_handle(first)?;
                let needle_length =
                    string::length(self.heap, needle).map_err(|_| self.heap_failure())?;
                let at = if id == native::STRING_STARTS_WITH {
                    if second.is_undefined() {
                        0
                    } else {
                        self.index_argument(second)?
                    }
                } else {
                    let end = if second.is_undefined() {
                        length
                    } else {
                        self.index_argument(second)?.min(length)
                    };
                    match end.checked_sub(needle_length) {
                        Some(at) => at,
                        None => return Ok(Value::boolean(false)),
                    }
                };
                let matched = string::matches_at(self.heap, text, needle, at)
                    .map_err(|_| self.heap_failure())?;
                Ok(Value::boolean(matched))
            }
            native::STRING_SLICE => {
                let start = self.relative_index(first, length, 0)?;
                let end = self.relative_index(second, length, length)?;
                let handle = string::slice(self.heap, text, start, end.max(start))
                    .map_err(|_| self.heap_failure())?;
                Ok(Value::string(handle))
            }
            native::STRING_SUBSTRING => {
                let start = self.clamped_index(first, length, 0)?;
                let end = self.clamped_index(second, length, length)?;
                let (start, end) = if start <= end {
                    (start, end)
                } else {
                    (end, start)
                };
                let handle =
                    string::slice(self.heap, text, start, end).map_err(|_| self.heap_failure())?;
                Ok(Value::string(handle))
            }
            native::STRING_TO_UPPER_CASE | native::STRING_TO_LOWER_CASE => {
                let handle =
                    string::convert_case(self.heap, text, id == native::STRING_TO_UPPER_CASE)
                        .map_err(|_| self.heap_failure())?;
                Ok(Value::string(handle))
            }
            native::STRING_TRIM => {
                let handle =
                    string::trim(self.heap, text, true, true).map_err(|_| self.heap_failure())?;
                Ok(Value::string(handle))
            }
            native::STRING_REPEAT => {
                let number = self.coerce_to_number(first)?;
                if number < 0.0 || !number.is_finite() {
                    return Err(self.throw_error_of(ErrorKind::Range));
                }
                let count = value::to_uint32(value::truncate(number));
                let handle =
                    string::repeat(self.heap, text, count).map_err(|_| self.heap_failure())?;
                Ok(Value::string(handle))
            }
            native::STRING_PAD_START | native::STRING_PAD_END => {
                let target = self.index_argument(first)?;
                let filler = if second.is_undefined() {
                    self.ascii_string(b" ")?
                } else {
                    self.coerce_to_string(second)?
                };
                let handle = string::pad(
                    self.heap,
                    text,
                    target,
                    filler.as_handle(),
                    id == native::STRING_PAD_START,
                )
                .map_err(|_| self.heap_failure())?;
                Ok(Value::string(handle))
            }
            native::STRING_CONCAT => {
                let mut result = text;
                for &argument in arguments {
                    let other = self.string_handle(argument)?;
                    result = string::concat(self.heap, result, other)
                        .map_err(|_| self.heap_failure())?;
                }
                Ok(Value::string(result))
            }
            native::STRING_REPLACE
                if first.is_object()
                    && object::regexp_program(self.heap, first.as_handle())
                        .map_err(|_| self.heap_failure())?
                        .is_some() =>
            {
                self.replace_with_pattern(text, first, second)
            }
            native::STRING_SPLIT
                if first.is_object()
                    && object::regexp_program(self.heap, first.as_handle())
                        .map_err(|_| self.heap_failure())?
                        .is_some() =>
            {
                self.split_by_pattern(text, first)
            }
            native::STRING_REPLACE => {
                let needle = self.string_handle(first)?;
                let Some(at) = string::index_of(self.heap, text, needle, 0)
                    .map_err(|_| self.heap_failure())?
                else {
                    return Ok(Value::string(text));
                };
                let needle_length =
                    string::length(self.heap, needle).map_err(|_| self.heap_failure())?;
                let replacement = if self.is_callable_value(second) {
                    let matched = string::slice(self.heap, text, at, at + needle_length)
                        .map_err(|_| self.heap_failure())?;
                    let position = Value::number(crate::softfloat::from_u64(u64::from(at)));
                    let outcome = self.call_with(
                        second,
                        Value::UNDEFINED,
                        &[Value::string(matched), position, Value::string(text)],
                    )?;
                    self.string_handle(outcome)?
                } else {
                    self.string_handle(second)?
                };
                let head =
                    string::slice(self.heap, text, 0, at).map_err(|_| self.heap_failure())?;
                let tail = string::slice(self.heap, text, at + needle_length, length)
                    .map_err(|_| self.heap_failure())?;
                let joined = string::concat(self.heap, head, replacement)
                    .map_err(|_| self.heap_failure())?;
                let joined =
                    string::concat(self.heap, joined, tail).map_err(|_| self.heap_failure())?;
                Ok(Value::string(joined))
            }
            native::STRING_SPLIT => {
                let array = self.new_array()?;
                if first.is_undefined() {
                    self.set_element(array, 0, Value::string(text))?;
                    self.set_length(array, 1)?;
                    return Ok(array);
                }
                let separator = self.string_handle(first)?;
                let separator_length =
                    string::length(self.heap, separator).map_err(|_| self.heap_failure())?;
                let mut written = 0u32;
                let mut start = 0u32;
                if separator_length == 0 {
                    // An empty separator splits into single code units.
                    while start < length {
                        let piece = string::slice(self.heap, text, start, start + 1)
                            .map_err(|_| self.heap_failure())?;
                        self.set_element(array, written, Value::string(piece))?;
                        written += 1;
                        start += 1;
                    }
                    self.set_length(array, written)?;
                    return Ok(array);
                }
                loop {
                    let found = string::index_of(self.heap, text, separator, start)
                        .map_err(|_| self.heap_failure())?;
                    let at = found.unwrap_or(length);
                    let piece = string::slice(self.heap, text, start, at)
                        .map_err(|_| self.heap_failure())?;
                    self.set_element(array, written, Value::string(piece))?;
                    written += 1;
                    match found {
                        Some(at) => start = at + separator_length,
                        None => break,
                    }
                }
                self.set_length(array, written)?;
                Ok(array)
            }
            native::STRING_VALUES => {
                let handle = object::create_iterator(
                    self.heap,
                    Value::object(self.realm.iterator_prototype),
                    Value::string(text),
                    ITERATE_CODE_POINTS,
                )
                .map_err(|_| self.heap_failure())?;
                Ok(Value::object(handle))
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
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

    /// The natives of `Number`, `Boolean`, and the global conversions.
    fn number_native(
        &mut self,
        id: u32,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        match id {
            native::NUMBER => {
                if arguments.is_empty() {
                    return Ok(Value::number(0.0));
                }
                if matches!(first.tag(), Tag::BigInt) {
                    // The conversion is explicit here, so it is allowed to
                    // round; an arithmetic operation that mixed them would not.
                    let number = self.big_int_operand(first)?;
                    return Ok(Value::number(number.to_f64()));
                }
                Ok(Value::number(self.coerce_to_number(first)?))
            }
            native::BOOLEAN => Ok(Value::boolean(self.coerce_to_boolean(first)?)),
            native::NUMBER_IS_INTEGER | native::NUMBER_IS_SAFE_INTEGER => {
                let integral = matches!(first.tag(), Tag::Number)
                    && value::is_integral(first.as_number())
                    && (id == native::NUMBER_IS_INTEGER
                        || first.as_number().abs() <= 9_007_199_254_740_991.0);
                Ok(Value::boolean(integral))
            }
            native::NUMBER_IS_FINITE => Ok(Value::boolean(
                matches!(first.tag(), Tag::Number) && first.as_number().is_finite(),
            )),
            native::NUMBER_IS_NAN => Ok(Value::boolean(
                matches!(first.tag(), Tag::Number) && first.as_number().is_nan(),
            )),
            native::IS_NAN => {
                let number = self.coerce_to_number(first)?;
                Ok(Value::boolean(number.is_nan()))
            }
            native::IS_FINITE => {
                let number = self.coerce_to_number(first)?;
                Ok(Value::boolean(number.is_finite()))
            }
            native::PARSE_INT | native::PARSE_FLOAT => {
                let text = self.string_handle(first)?;
                let radix = if id == native::PARSE_INT {
                    let value = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
                    let number = self.coerce_to_number(value)?;
                    let radix = value::to_uint32(number);
                    if radix == 0 {
                        10
                    } else {
                        radix
                    }
                } else {
                    10
                };
                self.parse_number(text, radix, id == native::PARSE_FLOAT)
            }
            native::NUMBER_TO_STRING => {
                let receiver = self.primitive_this(this)?;
                let number = self.coerce_to_number(receiver)?;
                let radix = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                if radix.is_undefined() {
                    return self.coerce_to_string(Value::number(number));
                }
                let radix = value::to_uint32(self.coerce_to_number(radix)?);
                if !(2..=36).contains(&radix) {
                    return Err(self.throw_error_of(ErrorKind::Range));
                }
                if radix == 10 {
                    return self.coerce_to_string(Value::number(number));
                }
                let mut units = [0u16; 72];
                let written = crate::numeric::radix_text(number, radix, &mut units);
                self.make_string(units.get(..written).unwrap_or(&[]))
            }
            native::NUMBER_TO_FIXED => {
                let receiver = self.primitive_this(this)?;
                let number = self.coerce_to_number(receiver)?;
                let digits = value::to_uint32(self.coerce_to_number(first)?);
                if digits > 100 {
                    return Err(self.throw_error_of(ErrorKind::Range));
                }
                let mut units = [0u16; 128];
                let written = crate::dtoa::fixed(number, digits, &mut units);
                self.make_string(units.get(..written).unwrap_or(&[]))
            }
            native::NUMBER_VALUE_OF | native::BOOLEAN_VALUE_OF => self.primitive_this(this),
            native::BOOLEAN_TO_STRING => {
                let receiver = self.primitive_this(this)?;
                self.coerce_to_string(receiver)
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
    }

    /// The natives of `Math`, each a pure function of its arguments.
    fn math_native(&mut self, id: u32, arguments: &[Value]) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        let value = self.coerce_to_number(first)?;
        let result = match id {
            native::MATH_ABS => {
                if value < 0.0 {
                    -value
                } else {
                    value
                }
            }
            native::MATH_FLOOR => value::floor(value),
            native::MATH_CEIL => value::ceil(value),
            native::MATH_ROUND => value::floor(value + 0.5),
            native::MATH_TRUNC => value::truncate(value),
            native::MATH_SQRT => crate::numeric::sqrt(value),
            native::MATH_SIGN => {
                if value.is_nan() {
                    value
                } else if value > 0.0 {
                    1.0
                } else if value < 0.0 {
                    -1.0
                } else {
                    value
                }
            }
            native::MATH_POW => {
                let exponent =
                    self.coerce_to_number(arguments.get(1).copied().unwrap_or(Value::UNDEFINED))?;
                crate::numeric::power(value, exponent)
            }
            native::MATH_MIN | native::MATH_MAX => {
                let mut best = if id == native::MATH_MIN {
                    f64::INFINITY
                } else {
                    f64::NEG_INFINITY
                };
                for &argument in arguments {
                    let number = self.coerce_to_number(argument)?;
                    if number.is_nan() {
                        best = f64::NAN;
                        break;
                    }
                    let better = if id == native::MATH_MIN {
                        number < best
                    } else {
                        number > best
                    };
                    if better {
                        best = number;
                    }
                }
                best
            }
            native::MATH_HYPOT => {
                let mut total = 0.0f64;
                for &argument in arguments {
                    let number = self.coerce_to_number(argument)?;
                    total += number * number;
                }
                crate::numeric::sqrt(total)
            }
            _ => return Err(Completion::Terminated(Termination::NotImplemented)),
        };
        Ok(Value::number(result))
    }

    /// `Symbol`, and what a symbol carries.
    fn symbol_native(
        &mut self,
        id: u32,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        match id {
            native::SYMBOL => {
                let description = if first.is_undefined() {
                    self.ascii_string(b"")?
                } else {
                    self.coerce_to_string(first)?
                };
                let length = string::length(self.heap, description.as_handle())
                    .map_err(|_| self.heap_failure())? as usize;
                let mut units = [0u16; 128];
                let room = length.min(units.len());
                string::copy_units(
                    self.heap,
                    description.as_handle(),
                    units.get_mut(..room).unwrap_or(&mut []),
                )
                .map_err(|_| self.heap_failure())?;
                let handle = string::create_symbol(self.heap, units.get(..room).unwrap_or(&[]))
                    .map_err(|_| self.heap_failure())?;
                Ok(Value::symbol(handle))
            }
            native::SYMBOL_TO_STRING => {
                let receiver = self.primitive_this(this)?;
                self.symbol_text(receiver)
            }
            native::SYMBOL_DESCRIPTION => {
                let receiver = self.primitive_this(this)?;
                if !matches!(receiver.tag(), Tag::Symbol) {
                    return Err(self.throw_type_error());
                }
                Ok(Value::string(receiver.as_handle()))
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
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

    /// `call`, `apply`, and `bind`, which are how a receiver is chosen.
    fn function_native(
        &mut self,
        id: u32,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        match id {
            native::FUNCTION_PROTOTYPE_CALL => {
                let rest = arguments.get(1..).unwrap_or(&[]);
                self.call_value(this, first, rest)
            }
            native::FUNCTION_PROTOTYPE_APPLY => {
                let list = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
                if list.is_nullish() {
                    return self.call_value(this, first, &[]);
                }
                let length = self.length_of(list)?;
                let mut values = [Value::UNDEFINED; MAX_ARGUMENTS];
                let count = (length as usize).min(values.len());
                let mut index = 0usize;
                while index < count {
                    values[index] = self.element(list, u32::try_from(index).unwrap_or(0))?;
                    index += 1;
                }
                self.call_value(this, first, values.get(..count).unwrap_or(&[]))
            }
            native::FUNCTION_PROTOTYPE_BIND => {
                if !self.is_callable_value(this) {
                    return Err(self.throw_type_error());
                }
                // A bound function is a native that carries what it was bound
                // to: the target and the receiver, in an ordinary array.
                let record = self.new_array()?;
                self.set_element(record, 0, this)?;
                self.set_element(record, 1, first)?;
                let mut written = 2u32;
                for &argument in arguments.get(1..).unwrap_or(&[]) {
                    self.set_element(record, written, argument)?;
                    written += 1;
                }
                self.set_length(record, written)?;
                let bound = object::create_native(
                    self.heap,
                    Value::object(self.realm.function_prototype),
                    native::BOUND_FUNCTION,
                    0,
                )
                .map_err(|_| self.heap_failure())?;
                object::set_bound_value(self.heap, bound, record)
                    .map_err(|_| self.heap_failure())?;
                Ok(Value::object(bound))
            }
            native::BOUND_FUNCTION => {
                let Some(function) = self.current_native else {
                    return Err(Completion::Terminated(Termination::Malformed));
                };
                let record = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                let target = self.element(record, 0)?;
                let receiver = self.element(record, 1)?;
                let bound_count = self.length_of(record)?.saturating_sub(2);
                let mut values = [Value::UNDEFINED; MAX_ARGUMENTS];
                let mut count = 0usize;
                let mut index = 0u32;
                while index < bound_count && count < values.len() {
                    values[count] = self.element(record, index + 2)?;
                    count += 1;
                    index += 1;
                }
                for &argument in arguments {
                    if count < values.len() {
                        values[count] = argument;
                        count += 1;
                    }
                }
                self.call_value(target, receiver, values.get(..count).unwrap_or(&[]))
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
    }

    /// One step of an iterator the engine made itself.
    fn iterator_native(
        &mut self,
        id: u32,
        this: Value,
        _arguments: &[Value],
    ) -> Result<Value, Completion> {
        if id == native::ITERATOR_SELF {
            return Ok(this);
        }
        if !this.is_object() {
            return Err(self.throw_type_error());
        }
        let Some((target, index, kind)) =
            object::iterator_state(self.heap, this.as_handle()).map_err(|_| self.heap_failure())?
        else {
            return Err(self.throw_type_error());
        };
        let result = object::create(self.heap, Value::object(self.realm.object_prototype))
            .map_err(|_| self.heap_failure())?;
        let result = Value::object(result);
        let value_key = self.ascii_key(b"value")?;
        let done_key = self.ascii_key(b"done")?;

        let (value, done, next) = if kind == ITERATE_CODE_POINTS {
            let handle = target.as_handle();
            match string::code_point_at(self.heap, handle, index)
                .map_err(|_| self.heap_failure())?
            {
                Some((_, width)) => {
                    let piece = string::slice(self.heap, handle, index, index + width)
                        .map_err(|_| self.heap_failure())?;
                    (Value::string(piece), false, index + width)
                }
                None => (Value::UNDEFINED, true, index),
            }
        } else {
            let length = self.length_of(target)?;
            if index >= length {
                (Value::UNDEFINED, true, index)
            } else {
                let position = Value::number(crate::softfloat::from_u64(u64::from(index)));
                let value = match kind {
                    ITERATE_KEYS => position,
                    ITERATE_ENTRIES => {
                        let pair = self.new_array()?;
                        let element = self.element(target, index)?;
                        self.set_element(pair, 0, position)?;
                        self.set_element(pair, 1, element)?;
                        self.set_length(pair, 2)?;
                        pair
                    }
                    _ => self.element(target, index)?,
                };
                (value, false, index + 1)
            }
        };
        object::set_iterator_index(self.heap, this.as_handle(), next)
            .map_err(|_| self.heap_failure())?;
        self.define_property(result, value_key, value)?;
        self.define_property(result, done_key, Value::boolean(done))?;
        Ok(result)
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
                let mut digits = [0u8; 256];
                let mut count = 0usize;
                let mut index = 0usize;
                let mut negative = false;
                while index < length && count < digits.len() {
                    let unit =
                        string::unit_at(self.heap, handle, u32::try_from(index).unwrap_or(0))
                            .map_err(|_| self.heap_failure())?
                            .unwrap_or(0);
                    if index == 0 && (unit == u16::from(b'-') || unit == u16::from(b'+')) {
                        negative = unit == u16::from(b'-');
                        index += 1;
                        continue;
                    }
                    if unit > 0x7F {
                        return Err(self.throw_error_of(ErrorKind::Syntax));
                    }
                    digits[count] = unit as u8;
                    count += 1;
                    index += 1;
                }
                let number =
                    crate::bigint::from_digits(digits.get(..count).unwrap_or(&[]), 10, negative)
                        .map_err(|_| self.throw_error_of(ErrorKind::Syntax))?;
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
            at += 1;
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

    /// The natives of `RegExp`, and the string methods that take a pattern.
    fn regexp_native(
        &mut self,
        id: u32,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        match id {
            native::REG_EXP => {
                // `RegExp(x)` answers `x` when it is already one, and compiles
                // it otherwise.
                if first.is_object()
                    && object::regexp_program(self.heap, first.as_handle())
                        .map_err(|_| self.heap_failure())?
                        .is_some()
                    && arguments.len() == 1
                {
                    return Ok(first);
                }
                let pattern = self.string_handle(first)?;
                let flags = match arguments.get(1).copied() {
                    Some(value) if !value.is_undefined() => {
                        let text = self.string_handle(value)?;
                        let length = string::length(self.heap, text)
                            .map_err(|_| self.heap_failure())?
                            as usize;
                        let mut units = [0u16; 16];
                        let room = length.min(units.len());
                        string::copy_units(
                            self.heap,
                            text,
                            units.get_mut(..room).unwrap_or(&mut []),
                        )
                        .map_err(|_| self.heap_failure())?;
                        crate::regexp::flags_of(units.get(..room).unwrap_or(&[]))
                            .map_err(|_| self.throw_error_of(ErrorKind::Syntax))?
                    }
                    _ => 0,
                };
                let length =
                    string::length(self.heap, pattern).map_err(|_| self.heap_failure())? as usize;
                let mut units = [0u16; 512];
                let room = length.min(units.len());
                string::copy_units(self.heap, pattern, units.get_mut(..room).unwrap_or(&mut []))
                    .map_err(|_| self.heap_failure())?;
                self.create_regexp(units.get(..room).unwrap_or(&[]), flags)
            }
            native::REG_EXP_EXEC | native::REG_EXP_TEST => {
                let subject = self.string_handle(first)?;
                let (global, sticky) = self.regexp_kind(this)?;
                let start = if global || sticky {
                    let key = self.ascii_key(b"lastIndex")?;
                    let value = self.get_property(this, key)?;
                    value::to_uint32(self.coerce_to_number(value)?)
                } else {
                    0
                };
                let outcome = self.match_regexp(this, subject, start, sticky)?;
                if global || sticky {
                    let key = self.ascii_key(b"lastIndex")?;
                    let next = outcome.map_or(0, |slots| slots[1]);
                    let value = Value::number(crate::softfloat::from_u64(u64::from(next)));
                    self.set_property(this, key, value)?;
                }
                let Some(slots) = outcome else {
                    return Ok(if id == native::REG_EXP_TEST {
                        Value::boolean(false)
                    } else {
                        Value::NULL
                    });
                };
                if id == native::REG_EXP_TEST {
                    return Ok(Value::boolean(true));
                }
                self.match_result(subject, &slots, this)
            }
            native::REG_EXP_TO_STRING => {
                let source_key = self.ascii_key(b"source")?;
                let flags_key = self.ascii_key(b"flags")?;
                let source = self.get_property(this, source_key)?;
                let flags = self.get_property(this, flags_key)?;
                let slash = self.ascii_string(b"/")?;
                let source = self.coerce_to_string(source)?;
                let flags = self.coerce_to_string(flags)?;
                let text = string::concat(self.heap, slash.as_handle(), source.as_handle())
                    .map_err(|_| self.heap_failure())?;
                let text = string::concat(self.heap, text, slash.as_handle())
                    .map_err(|_| self.heap_failure())?;
                let text = string::concat(self.heap, text, flags.as_handle())
                    .map_err(|_| self.heap_failure())?;
                Ok(Value::string(text))
            }
            native::STRING_MATCH | native::STRING_SEARCH => {
                let receiver = self.primitive_this(this)?;
                let subject = self.string_handle(receiver)?;
                let pattern = self.as_regexp(first)?;
                let (global, sticky) = self.regexp_kind(pattern)?;
                if id == native::STRING_SEARCH || !global {
                    let outcome = self.match_regexp(pattern, subject, 0, sticky)?;
                    let Some(slots) = outcome else {
                        return Ok(if id == native::STRING_SEARCH {
                            Value::number(-1.0)
                        } else {
                            Value::NULL
                        });
                    };
                    if id == native::STRING_SEARCH {
                        return Ok(Value::number(crate::softfloat::from_u64(u64::from(
                            slots[0],
                        ))));
                    }
                    return self.match_result(subject, &slots, pattern);
                }
                // A global match answers every match's text, and nothing about
                // the groups, which is what the specification says.
                let array = self.new_array()?;
                let mut written = 0u32;
                let mut start = 0u32;
                loop {
                    let Some(slots) = self.match_regexp(pattern, subject, start, false)? else {
                        break;
                    };
                    let text = string::slice(self.heap, subject, slots[0], slots[1])
                        .map_err(|_| self.heap_failure())?;
                    self.set_element(array, written, Value::string(text))?;
                    written += 1;
                    start = if slots[1] > slots[0] {
                        slots[1]
                    } else {
                        slots[1] + 1
                    };
                }
                self.set_length(array, written)?;
                if written == 0 {
                    return Ok(Value::NULL);
                }
                Ok(array)
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
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
                let outcome = self.call_with(
                    replacement,
                    Value::UNDEFINED,
                    arguments.get(..count).unwrap_or(&[]),
                )?;
                self.string_handle(outcome)?
            } else {
                let text = self.string_handle(replacement)?;
                self.expand_replacement(subject, text, &slots, groups)?
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
        Ok(array)
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

    /// Call one of the functions the engine implements itself.
    fn call_native(
        &mut self,
        id: u32,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        match id {
            native::OBJECT_TO_STRING => self.ascii_string(b"[object Object]"),
            // Building a function from source needs the compiler, which is
            // not in the machine: refusing is a type error the program can
            // catch, not a termination.
            native::FUNCTION => Err(self.throw_type_error()),
            native::THROW_TYPE_ERROR => Err(self.throw_type_error()),
            native::EVAL => {
                // On the host's own stack there is no way to pause for the
                // compiler; a non-string answers itself, and a string is
                // refused rather than half-run.
                let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                if first.is_string() {
                    Err(self.throw_type_error())
                } else {
                    Ok(first)
                }
            }
            native::OBJECT_VALUE_OF => Ok(this),
            native::ARRAY_TO_STRING => self.join_array(this, None),
            native::ERROR_TO_STRING => {
                let name_key = self.ascii_key(b"name")?;
                let message_key = self.ascii_key(b"message")?;
                let name = self.get_property(this, name_key)?;
                let message = self.get_property(this, message_key)?;
                let name = if name.is_undefined() {
                    self.ascii_string(b"Error")?
                } else {
                    self.coerce_to_string(name)?
                };
                let message = if message.is_undefined() {
                    self.ascii_string(b"")?
                } else {
                    self.coerce_to_string(message)?
                };
                let message_length = string::length(self.heap, message.as_handle())
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                if message_length == 0 {
                    return Ok(name);
                }
                let separator = self.ascii_string(b": ")?;
                let joined = self.concat_values(name, separator)?;
                self.concat_values(joined, message)
            }
            native::ERROR
            | native::TYPE_ERROR
            | native::RANGE_ERROR
            | native::REFERENCE_ERROR
            | native::SYNTAX_ERROR
            | native::EVAL_ERROR
            | native::URI_ERROR => {
                // Called without `new`, an error constructor builds an error
                // just the same.
                let kind = Realm::kind_of(id)
                    .ok_or(Completion::Terminated(Termination::NotImplemented))?;
                let message = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                self.create_error(kind, message)
            }
            native::ARRAY_JOIN => {
                let separator = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                let separator = if separator.is_undefined() {
                    None
                } else {
                    Some(self.coerce_to_string(separator)?)
                };
                self.join_array(this, separator)
            }
            native::PROMISE_THEN => {
                let on_fulfilled = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                let on_rejected = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
                self.promise_then(this, on_fulfilled, on_rejected)
            }
            native::PROMISE_RESOLVE | native::PROMISE_REJECT => {
                let value = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                let handle = self.new_promise()?;
                if id == native::PROMISE_RESOLVE {
                    self.resolve(handle, value)?;
                } else {
                    self.settle(handle, promise::REJECTED, value)?;
                }
                Ok(Value::object(handle))
            }
            native::PROMISE_SETTLE_FULFILLED | native::PROMISE_SETTLE_REJECTED => {
                // A resolve or reject function carries the promise it settles.
                // It is called as a plain function, so the promise comes from
                // the function object rather than from a receiver.
                let Some(function) = self.current_native else {
                    return Err(Completion::Terminated(Termination::Malformed));
                };
                let bound = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                if !bound.is_object() {
                    return Ok(Value::UNDEFINED);
                }
                let value = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                if id == native::PROMISE_SETTLE_FULFILLED {
                    self.resolve(bound.as_handle(), value)?;
                } else {
                    self.settle(bound.as_handle(), promise::REJECTED, value)?;
                }
                Ok(Value::UNDEFINED)
            }
            native::SYMBOL | native::SYMBOL_TO_STRING | native::SYMBOL_DESCRIPTION => {
                self.symbol_native(id, this, arguments)
            }
            native::OBJECT
            | native::OBJECT_GET_OWN_PROPERTY_DESCRIPTOR
            | native::OBJECT_KEYS..=native::OBJECT_IS => self.object_native(id, this, arguments),
            native::ARRAY | native::ARRAY_IS_ARRAY..=native::ARRAY_SORT => {
                self.array_native(id, this, arguments)
            }
            native::STRING | native::STRING_FROM_CHAR_CODE..=native::STRING_VALUES => {
                self.string_native(id, this, arguments)
            }
            native::NUMBER
            | native::BOOLEAN
            | native::NUMBER_IS_INTEGER..=native::BOOLEAN_VALUE_OF => {
                self.number_native(id, this, arguments)
            }
            native::MATH_ABS..=native::MATH_HYPOT => self.math_native(id, arguments),
            native::FUNCTION_PROTOTYPE_CALL..=native::BOUND_FUNCTION => {
                self.function_native(id, this, arguments)
            }
            native::ITERATOR_NEXT | native::ITERATOR_SELF => {
                self.iterator_native(id, this, arguments)
            }
            native::BIG_INT => {
                let value = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                let primitive = self.coerce_to_primitive(value, Hint::Number)?;
                self.big_int_of(primitive)
            }
            native::BIG_INT_TO_STRING => {
                let receiver = self.primitive_this(this)?;
                let radix = match arguments.first().copied() {
                    Some(value) if !value.is_undefined() => {
                        let radix = value::to_uint32(self.coerce_to_number(value)?);
                        if !(2..=36).contains(&radix) {
                            return Err(self.throw_error_of(ErrorKind::Range));
                        }
                        radix
                    }
                    _ => 10,
                };
                self.big_int_text(receiver, radix)
            }
            native::BIG_INT_VALUE_OF => self.primitive_this(this),
            native::NAMESPACE_GET => {
                // The getter carries which module and which slot it reads.
                let Some(function) = self.current_native else {
                    return Err(Completion::Terminated(Termination::Malformed));
                };
                let binding = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                let module = value::to_uint32(self.element(binding, 0)?.as_number());
                let slot = value::to_uint32(self.element(binding, 1)?.as_number());
                let environment = self.module_environment(module);
                if !environment.is_object() {
                    return Ok(Value::UNDEFINED);
                }
                match env::slot_value(self.heap, environment.as_handle(), slot) {
                    Ok(value) => Ok(value),
                    Err(env::EnvironmentError::Uninitialised) => Err(self.throw_reference_error()),
                    Err(_) => Ok(Value::UNDEFINED),
                }
            }
            native::REG_EXP
            | native::REG_EXP_EXEC
            | native::REG_EXP_TEST
            | native::REG_EXP_TO_STRING
            | native::STRING_MATCH
            | native::STRING_SEARCH => self.regexp_native(id, this, arguments),
            id if id >= native::BINDING_BASE => {
                self.host_call(id - native::BINDING_BASE, arguments)
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
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

    /// Create a pending promise with the realm's prototype.
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
            if let Some(kind) = Realm::kind_of(native) {
                let message = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                return self.create_error(kind, message);
            }
            if native == native::PROMISE {
                return self.construct_promise(arguments.first().copied());
            }
            if native == native::FUNCTION {
                return Err(self.throw_type_error());
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
            ) {
                let value = self.call_native(native, Value::UNDEFINED, arguments)?;
                if value.is_object() {
                    return Ok(value);
                }
                return self.coerce_to_object(value);
            }
            return Err(Completion::Terminated(Termination::NotImplemented));
        }

        // The new object's prototype is the constructor's `prototype`
        // property, or the ordinary one when that is not an object.
        let prototype_key = self.ascii_key(b"prototype")?;
        let prototype = self.get_property(callee, prototype_key)?;
        let prototype = if prototype.is_object() {
            prototype
        } else {
            Value::object(self.realm.object_prototype)
        };
        let instance = object::create(self.heap, prototype)
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        let this = Value::object(instance);
        let returned = self.call_value(callee, this, arguments)?;
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

/// How an instruction affected control.
enum Flow {
    Continue,
    Jump(u32),
    Return(Value),
    /// A frame was pushed: the loop runs it, and what it returns lands in the
    /// accumulator without the host stack growing.
    Enter,
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

const MAX_ARGUMENTS: usize = 16;
/// The longest string the interpreter stages on its own stack.
const MAX_STRING_UNITS: usize = 256;
/// The most own keys one spread copies.
const MAX_COPIED_KEYS: usize = 64;
