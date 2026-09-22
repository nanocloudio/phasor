//! The machine's seams to its host: bindings and calls, completions, jobs,
//! the collector, and what the host retains.

use super::*;
use crate::binding::{Answer, Class};

pub(super) const MAX_ARGUMENTS: usize = 16;

/// The seed `Math.random` starts from: an arbitrary constant, so a machine
/// draws the same sequence every run.
pub(super) const RANDOM_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// Collections in a row that may end with the heap still under pressure
/// before the task ends as `HeapExhausted`.
///
/// Pressure is the heap being within its headroom of full. One collection
/// that does not relieve it is a live set that has grown into the headroom;
/// a program can do that on its way to freeing something. This many in a
/// row is a program whose live set stays there, and every further
/// collection would reclaim what one allocation takes back. Eight bounds
/// the cost of finding that out at eight collections, on any arena.
const PRESSED_COLLECTIONS: u32 = 8;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
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
    /// The callable that reaches one binding. A program holding it can make
    /// the call the deployment admitted and nothing else: the binding index is
    /// bound into the function, not passed to it.
    pub fn binding_function(&mut self, binding: u32) -> Result<Value, Completion> {
        let function = object::create_native(
            self.heap,
            Value::object(self.realm.function_prototype),
            native::BINDING_BASE + binding,
            0,
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(Value::object(function))
    }

    pub fn define_binding(&mut self, name: &[u8], binding: u32) -> Result<(), Completion> {
        let function = object::create_native(
            self.heap,
            Value::object(self.realm.function_prototype),
            native::BINDING_BASE + binding,
            0,
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(())
    }

    /// Make an admitted binding reachable as a member of a namespace object,
    /// which is how an image's capability import is answered: the interface
    /// is one object, its methods are the bindings the deployment granted.
    ///
    /// A namespace is an ordinary object with ordinary properties. Nothing
    /// about it is privileged: it is reachable because it was granted, and
    /// what it holds is exactly what was admitted.
    pub fn define_binding_in(
        &mut self,
        namespace: Handle,
        name: &[u8],
        binding: u32,
    ) -> Result<(), Completion> {
        let function = object::create_native(
            self.heap,
            Value::object(self.realm.function_prototype),
            native::BINDING_BASE + binding,
            0,
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let key = self.ascii_key(name)?;
        object::define_own_property(
            self.heap,
            namespace,
            key,
            Descriptor::data(
                Value::object(function),
                attribute::WRITABLE | attribute::CONFIGURABLE,
            ),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(())
    }

    /// Supply the wall-clock reading for the task about to run.
    ///
    /// Kept on the global, like the logical tick it displaces, because the
    /// machine is rebuilt between advances and the global is what survives.
    /// A deployment that granted no clock never calls this, and `Date.now`
    /// falls back to the tick — which is the capability property, not a
    /// missing feature: a program given no clock cannot read the hour.
    pub fn set_wall_clock(&mut self, millis: f64) -> Result<(), Completion> {
        let key = self.ascii_key(b"\0wall")?;
        object::define_own_property(
            self.heap,
            self.realm.global,
            key,
            Descriptor::data(Value::number(millis), attribute::WRITABLE),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(())
    }

    /// Put an empty namespace object on the global under `name`, for the
    /// bindings of one interface to be defined in.
    pub fn define_namespace(&mut self, name: &[u8]) -> Result<Handle, Completion> {
        let namespace = object::create(self.heap, Value::object(self.realm.object_prototype))
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let key = self.ascii_key(name)?;
        object::define_own_property(
            self.heap,
            self.realm.global,
            key,
            Descriptor::data(
                Value::object(namespace),
                attribute::WRITABLE | attribute::CONFIGURABLE,
            ),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(namespace)
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
        self.payload_length = 0;
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
        self.apply_completion_with(record, &[])
    }

    /// Apply a completion whose answer carried bytes: the payload the host
    /// read off the port, behind the frame.
    pub fn apply_completion_with(
        &mut self,
        record: &CompletionRecord,
        payload: &[u8],
    ) -> Result<(), CallError> {
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

        // Which binding answered, so a resource it opened is recorded
        // against it: a handle is meaningful only to the capability that
        // issued it.
        self.completing_binding = self
            .bindings
            .as_deref()
            .and_then(|bindings| bindings.binding_of(record.request))
            .unwrap_or(u32::MAX);
        // A completion the program cannot be given is still a completion. If
        // the answer cannot be built — the heap is spent, most often, because
        // the payload is large — the call settles as rejected rather than
        // returning here. Returning strands it: the pending slot stays live,
        // the count of calls in flight never comes down, and the promise a
        // program is holding never settles at all. A refusal it can catch is
        // the lesser of the two, and the only one it can act on.
        let (disposition, value) = match record.disposition {
            Disposition::Fulfilled => match self.answered_value(record, payload) {
                Ok(value) => (Disposition::Fulfilled, value),
                Err(_) => (
                    Disposition::Rejected,
                    self.create_cause_error(Cause::Internal)
                        .unwrap_or(Value::UNDEFINED),
                ),
            },
            Disposition::Rejected => match self.create_cause_error(record.cause) {
                Ok(value) => (Disposition::Rejected, value),
                Err(_) => (Disposition::Rejected, Value::UNDEFINED),
            },
        };
        self.complete_call(record.request, disposition, value)
    }

    /// The value a fulfilled completion settles with: nothing, a number, the
    /// payload as text, or the handle of a resource the provider opened.
    ///
    /// A handle is a number the program can hold and pass back, and nothing
    /// more: it is meaningful only to the binding that issued it, and the
    /// table checks its generation, so one kept past its resource's life
    /// names nothing rather than naming whatever took the slot.
    pub(super) fn answered_value(
        &mut self,
        record: &CompletionRecord,
        payload: &[u8],
    ) -> Result<Value, Completion> {
        match record.answer {
            Answer::None => Ok(Value::UNDEFINED),
            Answer::Number(number) => Ok(Value::number(number)),
            Answer::Payload(length) => {
                let bytes = payload.get(..length as usize).unwrap_or(&[]);
                self.payload_string(bytes)
            }
            Answer::Resource(token) => {
                // The provider's own identifier never reaches the program:
                // the table records it and answers with a handle, which is an
                // index and a generation the binding checks on every use.
                let binding = self.completing_binding;
                let Some(bindings) = self.bindings.as_deref_mut() else {
                    return Ok(Value::UNDEFINED);
                };
                match bindings.open(binding, token) {
                    Ok(handle) => Ok(Value::number(handle.pack() as f64)),
                    Err(_) => Err(Completion::QUOTA_EXCEEDED),
                }
            }
        }
    }

    /// A string cell holding a payload's bytes, one unit a byte, which is how
    /// a payload reaches a program. No byte is a program error and none is
    /// interpreted: what the provider answered is what the string holds.
    pub(super) fn payload_string(&mut self, bytes: &[u8]) -> Result<Value, Completion> {
        // One byte, one unit. A payload is bytes, and the string a program
        // receives is those bytes exactly — not a reading of them. Decoding
        // as UTF-8 here would replace every byte that is not valid UTF-8
        // with U+FFFD and lose what arrived, so a program could not read a
        // file, a stored value, or a response body that is not text.
        //
        // Text is recovered by decoding, which the surface does with
        // TextDecoder, and the bytes survive for everything that is not
        // text. `charCodeAt` gives a byte back, which is the convention
        // btoa and atob already use.
        const BLOCK: usize = 512;
        let mut units = [0u16; BLOCK];
        let mut count = 0usize;
        let mut joined: Option<Handle> = None;
        for &byte in bytes {
            units[count] = u16::from(byte);
            count += 1;
            if count == BLOCK {
                joined = Some(self.join_units(joined, units.get(..count).unwrap_or(&[]))?);
                count = 0;
            }
        }
        let handle = self.join_units(joined, units.get(..count).unwrap_or(&[]))?;
        Ok(Value::string(handle))
    }

    /// Append a block of units to the string built so far.
    fn join_units(&mut self, left: Option<Handle>, units: &[u16]) -> Result<Handle, Completion> {
        let block = string::create(self.heap, units).map_err(|_| Completion::HEAP_EXHAUSTED)?;
        match left {
            None => Ok(block),
            Some(held) => {
                string::concat(self.heap, held, block).map_err(|_| Completion::HEAP_EXHAUSTED)
            }
        }
    }

    /// The error a rejected completion settles with: an ordinary error object
    /// carrying the cause and whether the same call could succeed again.
    pub(super) fn create_cause_error(&mut self, cause: Cause) -> Result<Value, Completion> {
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
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        object::define_own_property(
            self.heap,
            error.as_handle(),
            retry_key,
            Descriptor::data(Value::boolean(cause.retryable()), attribute::DEFAULT),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(error)
    }

    /// What a host-installed `print` reported: 0 nothing, 1 the async test
    /// protocol's completion line, 2 anything else.
    pub fn print_status(&self) -> u8 {
        self.print_status
    }

    /// Put a `print` function on the realm's global object, as a conformance
    /// host or a shell does: the host grants output, and what is printed goes
    /// to the sink it attached.
    pub fn install_print(&mut self) -> Result<(), Completion> {
        crate::realm::install_print(self.heap, self.atoms, &self.realm)
            .map_err(|_| Completion::HEAP_EXHAUSTED)
    }

    /// Attach the buffer a host-installed `print` writes into, as UTF-8 with
    /// a line feed after each call. `length` is how much of it is filled, and
    /// persists in the host's storage between steps. A call that does not fit
    /// is dropped whole: the buffer is the host's quota on output.
    pub fn attach_print(&mut self, sink: &'a mut [u8], length: &'a mut usize) {
        self.print_sink = Some((sink, length));
    }

    /// What `print` has written and the host has not taken.
    pub fn printed(&self) -> &[u8] {
        match &self.print_sink {
            Some((sink, length)) => sink.get(..**length).unwrap_or(&[]),
            None => &[],
        }
    }

    /// Forget what `print` wrote, once the host has taken it.
    pub fn take_printed(&mut self) {
        if let Some((_, length)) = &mut self.print_sink {
            **length = 0;
        }
    }

    /// Write one printed string to the attached sink, if any.
    pub(in crate::vm) fn emit_print(&mut self, text: Handle) -> Result<(), Completion> {
        let Some((sink, length)) = &mut self.print_sink else {
            return Ok(());
        };
        let count = string::length(self.heap, text).map_err(|_| Completion::MALFORMED)?;
        let start = **length;
        let mut at = start;
        let mut index = 0u32;
        while index < count {
            let unit = string::unit_at(self.heap, text, index)
                .map_err(|_| Completion::MALFORMED)?
                .unwrap_or(0);
            let low = if index + 1 < count {
                string::unit_at(self.heap, text, index + 1)
                    .map_err(|_| Completion::MALFORMED)?
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
            let Some(out) = sink.get_mut(at..at + needed) else {
                // The call does not fit: the sink keeps what it held, and the
                // buffer is the host's quota on output.
                **length = start;
                return Ok(());
            };
            match needed {
                1 => out[0] = code_point as u8,
                2 => {
                    out[0] = 0xC0 | (code_point >> 6) as u8;
                    out[1] = 0x80 | (code_point & 0x3F) as u8;
                }
                3 => {
                    out[0] = 0xE0 | (code_point >> 12) as u8;
                    out[1] = 0x80 | ((code_point >> 6) & 0x3F) as u8;
                    out[2] = 0x80 | (code_point & 0x3F) as u8;
                }
                _ => {
                    out[0] = 0xF0 | (code_point >> 18) as u8;
                    out[1] = 0x80 | ((code_point >> 12) & 0x3F) as u8;
                    out[2] = 0x80 | ((code_point >> 6) & 0x3F) as u8;
                    out[3] = 0x80 | (code_point & 0x3F) as u8;
                }
            }
            at += needed;
            index += if paired { 2 } else { 1 };
        }
        let Some(end) = sink.get_mut(at) else {
            **length = start;
            return Ok(());
        };
        *end = b'\n';
        **length = at + 1;
        Ok(())
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

    /// Collect if the heap is running low, at an instruction boundary where
    /// every live value is in the accumulator, a register, a frame, the realm,
    /// the interned names, or an outstanding call.
    ///
    /// The work is charged to the same budget as instructions, so a program
    /// that makes a collection necessary pays for it.
    pub(super) fn maybe_collect(&mut self) -> Option<Completion> {
        self.roots_storage.as_ref()?;
        if !self.pressed() {
            self.pressed_collections = 0;
            return None;
        }
        if let Some(outcome) = self.collect_now() {
            return Some(outcome);
        }
        if !self.pressed() {
            self.pressed_collections = 0;
            return None;
        }
        // The collection ran and the heap is still under pressure: what is
        // live is what is live. Left alone, the next allocation would press
        // again and collect again, and a program whose live set has grown to
        // within the headroom of its arena would spend the rest of its fuel
        // collecting a few bytes at a time and end as `FuelExhausted` — the
        // wrong answer, and on a small arena an expensive one. A few in a row
        // is a program passing through the headroom; more is one living
        // there, and that is a full heap.
        self.pressed_collections = self.pressed_collections.saturating_add(1);
        if self.pressed_collections > PRESSED_COLLECTIONS {
            return Some(Completion::HEAP_EXHAUSTED);
        }
        None
    }

    /// Whether the heap is low enough to collect. A heap runs out of two
    /// things: the arena and the handle table. A program that makes many
    /// small cells — a call's environment, say — exhausts the table long
    /// before the bytes, so both are watched.
    fn pressed(&self) -> bool {
        let slots = self.heap.slot_capacity();
        let slot_headroom = (slots / 8).max(16);
        self.heap.free() < self.collection_headroom || self.heap.free_slots() < slot_headroom
    }

    /// Collect immediately, whatever the pressure heuristic says.
    ///
    /// An allocation that failed with garbage still reclaimable — a table
    /// that doubled away from its old copies faster than the headroom check
    /// watched — collects here and retries, so a failure means the live data
    /// truly does not fit.
    pub(super) fn collect_now(&mut self) -> Option<Completion> {
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

    pub(super) fn next_job(&mut self) -> Option<Job> {
        match &mut self.queue {
            Some(queue) => queue.pop(),
            None => None,
        }
    }

    pub(super) fn run_job(&mut self, job: Job) -> Result<(), Completion> {
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
    pub(super) fn resolve(&mut self, promise: Handle, value: Value) -> Result<(), Completion> {
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
                return queue.push(job).map_err(|_| Completion::QUOTA_EXCEEDED);
            }
        }
        self.settle(promise, promise::FULFILLED, value)
    }

    pub(super) fn settle(
        &mut self,
        promise: Handle,
        state: u8,
        value: Value,
    ) -> Result<(), Completion> {
        let Some(queue) = self.queue.as_deref_mut() else {
            return Err(Completion::Terminated(Termination::NotImplemented));
        };
        promise::settle(self.heap, queue, promise, state, value)
            .map(|_| ())
            .map_err(|_| Completion::QUOTA_EXCEEDED)
    }

    /// The control block, which a host sets between slices.
    pub fn control(&mut self) -> &mut Control {
        &mut self.control
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

    /// Make a call on an admitted binding.
    ///
    /// The engine records the call and hands back a promise; it does not wait,
    /// and it does not touch a provider.
    pub(super) fn host_call(
        &mut self,
        binding: u32,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        // A snapshot binding is not called at all: the host supplied its value
        // before the task ran, so the read is local and answers at once. That
        // is what lets a clock be a capability without a channel round trip.
        if let Some(descriptor) = self
            .bindings
            .as_deref()
            .and_then(|bindings| bindings.binding(binding))
        {
            if descriptor.class == Class::Snapshot {
                return Ok(Value::number(descriptor.snapshot));
            }
        }
        let promise = self.new_promise()?;
        let (payload_length, payload) = self.stage_payload(binding, arguments)?;

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
            payload_length,
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
            None => return Err(Completion::QUOTA_EXCEEDED),
        }
        Ok(Value::object(promise))
    }

    /// Stage a call's arguments as its payload: each argument as text, one
    /// per NUL-separated field, in the buffer the host attached. Answers how
    /// many bytes were staged.
    ///
    /// A host that attached no payload buffer stages nothing, and its calls
    /// carry their arguments' digest alone — which is every binding whose
    /// answer is a number.
    pub(super) fn stage_payload(
        &mut self,
        binding: u32,
        arguments: &[Value],
    ) -> Result<(u32, crate::digest::Digest), Completion> {
        // Arguments are length-prefixed, not separated. A separator has to be
        // a byte that cannot occur in a field, and no such byte exists here:
        // a JavaScript string may hold U+0000 and a byte argument may hold
        // 0x00, so a reader splitting on one would take a field the caller
        // did not write. The frame is
        //
        //     count: u16 LE, then for each argument: length: u32 LE, bytes
        //
        // which says where every field ends without reserving any value.
        let mut hasher = crate::digest::Hasher::new();
        let start = self.payload_length;
        let mut at = start;
        // A call with no arguments carries no payload at all, rather than a
        // frame saying so. A provider that takes fixed-size records and never
        // reads a payload would otherwise find two bytes of one behind every
        // call, and read the next record from the wrong place.
        if arguments.is_empty() {
            return Ok((0, hasher.finish()));
        }
        let count = u16::try_from(arguments.len()).map_err(|_| Completion::QUOTA_EXCEEDED)?;
        self.put_staged(&mut at, &mut hasher, &count.to_le_bytes())?;
        for &argument in arguments {
            // A number that resolves as one of this binding's handles crosses
            // as the provider's own identifier for the resource. A forged,
            // stale, or another binding's handle resolves to nothing, and the
            // argument crosses as the number it is — which the provider will
            // not recognise.
            let argument = match self.resolve_handle(binding, argument) {
                Some(token) => Value::number(token as f64),
                None => argument,
            };
            // Bytes a program already holds cross as themselves. Anything
            // else is the text it reads as, in UTF-8.
            if let Some((cells, start, span)) = self.view_bytes(argument)? {
                self.put_staged(&mut at, &mut hasher, &span.to_le_bytes())?;
                let mut index = 0u32;
                while index < span {
                    let held = self.element(cells, start + index)?;
                    let byte =
                        [u8::try_from(value::to_uint32(held.as_number()) & 0xFF).unwrap_or(0)];
                    self.put_staged(&mut at, &mut hasher, &byte)?;
                    index += 1;
                }
                continue;
            }
            // A string is its bytes, one unit one byte, which is the same
            // convention the answer arrives under and the one btoa and atob
            // already use. A unit above a byte is not a byte: it is refused
            // rather than truncated or re-encoded behind the program's back,
            // because either would put different bytes on the wire than the
            // ones it is holding. Text above Latin-1 is encoded by the
            // surface, which is where knowing it is text belongs.
            let text = self.coerce_to_string(argument)?;
            let handle = text.as_handle();
            let units = string::length(self.heap, handle).map_err(|_| Completion::MALFORMED)?;
            self.put_staged(&mut at, &mut hasher, &units.to_le_bytes())?;
            let mut index = 0u32;
            while index < units {
                let unit = string::unit_at(self.heap, handle, index)
                    .map_err(|_| Completion::MALFORMED)?
                    .unwrap_or(0);
                let Ok(byte) = u8::try_from(unit) else {
                    return Err(Completion::MALFORMED);
                };
                self.put_staged(&mut at, &mut hasher, &[byte])?;
                index += 1;
            }
        }
        let staged = at - start;
        self.payload_length = at;
        let length = u32::try_from(staged).map_err(|_| Completion::QUOTA_EXCEEDED)?;
        Ok((length, hasher.finish()))
    }

    /// The byte cells a typed array views, where the argument is one: the
    /// array holding them, where this view starts, and how far it runs.
    ///
    /// A value that is not a view is not an error here — it is text, and the
    /// caller reads it as text.
    fn view_bytes(&mut self, value: Value) -> Result<Option<(Value, u32, u32)>, Completion> {
        if !value.is_object()
            || object::exotic_kind(self.heap, value.as_handle()).unwrap_or(0)
                != object::exotic::TYPED_ARRAY
        {
            return Ok(None);
        }
        let Some(count) = self.typed_array_length(value)? else {
            return Ok(Some((Value::UNDEFINED, 0, 0)));
        };
        let (buffer, kind, offset, _) = self.typed_array_parts(value)?;
        let cells = self.array_buffer_bytes(buffer)?;
        let span = count.saturating_mul(crate::realm::typed_array_element_size(kind));
        Ok(Some((cells, offset, span)))
    }

    /// Put bytes into the staging buffer, and into the digest whether or not
    /// a buffer holds them: a host that stages no payload still carries what
    /// the call said, and one that does carries both.
    fn put_staged(
        &mut self,
        at: &mut usize,
        hasher: &mut crate::digest::Hasher,
        bytes: &[u8],
    ) -> Result<(), Completion> {
        hasher.update(bytes);
        if let Some(out) = self.payload_out.as_deref_mut() {
            let Some(slot) = out.get_mut(*at..*at + bytes.len()) else {
                return Err(Completion::QUOTA_EXCEEDED);
            };
            slot.copy_from_slice(bytes);
            *at += bytes.len();
        }
        Ok(())
    }

    /// The provider's identifier for a handle a program passed, when the
    /// value is one this binding issued and has not released.
    pub(super) fn resolve_handle(&self, binding: u32, value: Value) -> Option<u64> {
        if !matches!(value.tag(), Tag::Number) {
            return None;
        }
        let number = value.as_number();
        // no_std has no `fract`: a handle is a whole number in range, and
        // the round trip through the integer proves it.
        if !(0.0..=9_007_199_254_740_992.0).contains(&number) {
            return None;
        }
        let packed = number as u64;
        if packed as f64 != number {
            return None;
        }
        let handle = crate::binding::Handle::unpack(packed);
        self.bindings.as_deref()?.resolve(binding, handle).ok()
    }

    /// Attach the buffer a call's payload is staged in. A host that attaches
    /// one can carry bytes; one that does not carries digests alone.
    pub fn attach_payloads(&mut self, out: &'a mut [u8]) {
        self.payload_out = Some(out);
        self.payload_length = 0;
    }

    /// The payload bytes staged behind the call records the host has not
    /// taken, in the order the records were made.
    pub fn call_payloads(&self) -> &[u8] {
        match &self.payload_out {
            Some(out) => out.get(..self.payload_length).unwrap_or(&[]),
            None => &[],
        }
    }

    /// Record a resource a provider opened, answering the packed handle the
    /// program holds it by.
    pub fn open_resource(&mut self, binding: u32, token: u64) -> Result<u64, CallError> {
        let bindings = self.bindings.as_deref_mut().ok_or(CallError::NotAdmitted)?;
        bindings
            .open(binding, token)
            .map(crate::binding::Handle::pack)
    }
}
