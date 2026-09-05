//! The machine's seams to its host: bindings and calls, completions, jobs,
//! the collector, and what the host retains.

use super::*;

pub(super) const MAX_ARGUMENTS: usize = 16;

/// The seed `Math.random` starts from: an arbitrary constant, so a machine
/// draws the same sequence every run.
pub(super) const RANDOM_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

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
            None => return Err(Completion::QUOTA_EXCEEDED),
        }
        Ok(Value::object(promise))
    }

    /// The digest of a call's arguments, which is what crosses the boundary in
    /// place of the values themselves.
    pub(super) fn payload_digest(
        &mut self,
        arguments: &[Value],
    ) -> Result<crate::digest::Digest, Completion> {
        let mut hasher = crate::digest::Hasher::new();
        for &argument in arguments {
            let text = self.coerce_to_string(argument)?;
            let handle = text.as_handle();
            let length = string::length(self.heap, handle).map_err(|_| Completion::MALFORMED)?;
            hasher.update(&length.to_le_bytes());
            let mut index = 0u32;
            while index < length {
                let unit = string::unit_at(self.heap, handle, index)
                    .map_err(|_| Completion::MALFORMED)?
                    .unwrap_or(0);
                hasher.update(&unit.to_le_bytes());
                index += 1;
            }
        }
        Ok(hasher.finish())
    }
}
