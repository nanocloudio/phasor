//! Generators, async functions, `await`, and disposal: capturing a frame,
//! suspending it, and resuming it.

use super::*;

/// The elements one dispose-stack entry takes: resource, disposer, hint.
pub(super) const DISPOSABLE_STRIDE: u32 = 3;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// Suspend the top frame on an awaited value: the frame's state moves to
    /// the heap, reactions on the value's promise resume it, and the async
    /// call's own promise is left in the accumulator for the caller.
    pub(super) fn suspend_await(&mut self, value: Value) -> Result<(), Completion> {
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
        .map_err(|_| Completion::QUOTA_EXCEEDED)?;
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
    pub(super) fn promise_for(&mut self, value: Value) -> Result<Handle, Completion> {
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
    pub(super) fn capture_frame(
        &mut self,
        frame: &Frame,
        keeper: Value,
    ) -> Result<Handle, Completion> {
        let coroutine =
            object::create(self.heap, Value::NULL).map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        Ok(coroutine)
    }

    /// Suspend the top frame at a `yield`: the generator keeps the frame,
    /// and the yielded value is what the resumer receives.
    /// Whether a captured coroutine is suspended at a delegating `yield*`,
    /// which is what lets `return` and `throw` resume it for forwarding.
    pub(super) fn coroutine_is_star(&mut self, coroutine: Value) -> Result<bool, Completion> {
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
    pub(super) fn coroutine_is_delegate(&mut self, coroutine: Value) -> Result<bool, Completion> {
        if !coroutine.is_object() {
            return Ok(false);
        }
        let delegate_key = self.ascii_key(b"delegate")?;
        let held = self.get_property(coroutine, delegate_key)?;
        Ok(matches!(held.tag(), Tag::Boolean) && held.as_boolean())
    }

    pub(super) fn suspend_yield_delegate(&mut self) -> Result<(), Completion> {
        self.suspend_yield_inner(true, true)
    }

    pub(super) fn suspend_yield(&mut self, star: bool) -> Result<(), Completion> {
        self.suspend_yield_inner(star, false)
    }

    pub(super) fn suspend_yield_inner(
        &mut self,
        star: bool,
        delegate: bool,
    ) -> Result<(), Completion> {
        let frame = self.frames[self.depth as usize - 1];
        let generator = frame.promise;
        if !generator.is_object() {
            return Err(Completion::MALFORMED);
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
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        if delegate {
            let delegate_key = self.ascii_key(b"delegate")?;
            object::define_own_property(
                self.heap,
                coroutine,
                delegate_key,
                Descriptor::data(Value::boolean(true), attribute::WRITABLE),
            )
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        object::set_generator(
            self.heap,
            generator.as_handle(),
            object::generator_state::SUSPENDED | async_bit,
            Value::object(coroutine),
        )
        .map_err(|_| Completion::MALFORMED)?;
        self.depth -= 1;
        self.top = frame.base;
        self.sync_realm();
        Ok(())
    }

    /// The async bit a generator carries, preserved across state moves.
    pub(super) fn generator_async_bit(&self, generator: Value) -> u8 {
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
    pub(super) fn await_return(
        &mut self,
        generator: Value,
        value: Value,
    ) -> Result<(), Completion> {
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
            .map_err(|_| Completion::QUOTA_EXCEEDED)
    }

    /// Serve the request at the head of an async generator's queue — next,
    /// throw, or return — as the generator's state allows: resuming it at a
    /// yield, completing one that has not started, or answering one that has
    /// finished, a `return` awaiting its value first.
    pub(super) fn serve_async_request(
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
    pub(super) fn settle_pending_next(
        &mut self,
        generator: Value,
        outcome: Result<Value, Value>,
        done: bool,
    ) -> Result<(), Completion> {
        self.settle_pending_next_with(generator, outcome, done, true)
    }

    /// The request at the head of an async generator's queue, if one waits.
    pub(super) fn head_request(
        &mut self,
        generator: Value,
    ) -> Result<Option<(Value, u32)>, Completion> {
        if !generator.is_object() {
            return Ok(None);
        }
        let key = self.ascii_key(b"\0next")?;
        let queue_value = object::get_own_property(self.heap, generator.as_handle(), key)
            .map_err(|_| Completion::MALFORMED)?
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
    pub(super) fn settle_pending_next_with(
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
            .map_err(|_| Completion::MALFORMED)?;
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
            queue.push(job).map_err(|_| Completion::QUOTA_EXCEEDED)?;
        }
        Ok(())
    }

    /// How many `next` requests wait on an async generator.
    pub(super) fn pending_next_count(&mut self, generator: Handle) -> Result<u32, Completion> {
        let key = self.ascii_key(b"\0next")?;
        let held = object::get_own_property(self.heap, generator, key)
            .map_err(|_| Completion::MALFORMED)?;
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
    pub(super) fn drain_async_generator(&mut self, generator: Value) -> Result<(), Completion> {
        if !generator.is_object() {
            return Ok(());
        }
        let handle = generator.as_handle();
        if self.pending_next_count(handle)? == 0 {
            return Ok(());
        }
        let Some((raw_state, coroutine)) =
            object::generator(self.heap, handle).map_err(|_| Completion::MALFORMED)?
        else {
            return Ok(());
        };
        let Some((argument, operation)) = self.head_request(generator)? else {
            return Ok(());
        };
        self.serve_async_request(generator, raw_state, coroutine, operation, argument)
    }

    /// What a queued request asked for: its argument and its operation.
    pub(super) fn pending_request(&mut self, pending: Value) -> Result<(Value, u32), Completion> {
        if !pending.is_object() {
            return Ok((Value::UNDEFINED, native::GENERATOR_NEXT));
        }
        let arg_key = self.ascii_key(b"\0arg")?;
        let kind_key = self.ascii_key(b"\0kind")?;
        let argument = object::get_own_property(self.heap, pending.as_handle(), arg_key)
            .map_err(|_| Completion::MALFORMED)?
            .map_or(Value::UNDEFINED, |descriptor| descriptor.value);
        let kind = object::get_own_property(self.heap, pending.as_handle(), kind_key)
            .map_err(|_| Completion::MALFORMED)?
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
    pub(super) fn push_pending_next(
        &mut self,
        generator: Handle,
        pending: Value,
    ) -> Result<(), Completion> {
        let key = self.ascii_key(b"\0next")?;
        let held = object::get_own_property(self.heap, generator, key)
            .map_err(|_| Completion::MALFORMED)?;
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
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                queue
            }
        };
        self.append_element(queue, Some(pending))?;
        Ok(())
    }

    /// Make a generator object over the top frame, which was just pushed and
    /// given its arguments and has not run: the call's answer.
    pub(super) fn suspend_start(&mut self) -> Result<Value, Completion> {
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
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
        .map_err(|_| Completion::MALFORMED)?;
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
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        let this_iterator = object::create_native(
            self.heap,
            Value::object(self.realm.function_prototype),
            native::ITERATOR_SELF,
            0,
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        Ok(Value::object(generator))
    }

    /// Resume, finish, or throw into a generator, answering the iteration
    /// result its resumer receives.
    pub(super) fn generator_resume(
        &mut self,
        generator: Value,
        operation: u32,
        argument: Value,
    ) -> Result<Value, Completion> {
        if !generator.is_object() {
            return Err(self.throw_type_error());
        }
        let handle = generator.as_handle();
        let Some((raw_state, coroutine)) =
            object::generator(self.heap, handle).map_err(|_| Completion::MALFORMED)?
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
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
        let Some((after, after_coroutine)) =
            object::generator(self.heap, handle).map_err(|_| Completion::MALFORMED)?
        else {
            return Err(Completion::MALFORMED);
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
    pub(super) fn run_class_fields(
        &mut self,
        constructor: Value,
        this: Value,
    ) -> Result<(), Completion> {
        if !constructor.is_object() {
            return Ok(());
        }
        let key = self.ascii_key(b"\0fields")?;
        let held = object::get_own_property(self.heap, constructor.as_handle(), key)
            .map_err(|_| Completion::MALFORMED)?;
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
    pub(super) fn define_field(
        &mut self,
        this: Value,
        name: Value,
        value: Value,
    ) -> Result<(), Completion> {
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
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            if !admitted {
                // CreateDataPropertyOrThrow: a frozen or non-extensible
                // instance refuses the field.
                return Err(self.throw_type_error());
            }
        }
        Ok(())
    }

    /// One `{value, done}` iteration result.
    pub(super) fn iteration_result(
        &mut self,
        value: Value,
        done: bool,
    ) -> Result<Value, Completion> {
        let result = object::create(self.heap, Value::object(self.realm.object_prototype))
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let value_key = self.ascii_key(b"value")?;
        let done_key = self.ascii_key(b"done")?;
        object::define_own_property(
            self.heap,
            result,
            value_key,
            Descriptor::data(value, attribute::DEFAULT),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        object::define_own_property(
            self.heap,
            result,
            done_key,
            Descriptor::data(Value::boolean(done), attribute::DEFAULT),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(Value::object(result))
    }

    /// Resume a suspended async frame with a settled value, running it until
    /// it finishes or suspends again.
    pub(super) fn resume_coroutine(
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
    pub(super) fn restore_coroutine(
        &mut self,
        coroutine: Value,
        value: Value,
        kind: u8,
        direct: bool,
    ) -> Result<Option<Completion>, Completion> {
        if !coroutine.is_object() {
            return Err(Completion::MALFORMED);
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

    pub(super) fn new_promise(&mut self) -> Result<Handle, Completion> {
        promise::create(self.heap, Value::object(self.realm.promise_prototype))
            .map_err(|_| Completion::HEAP_EXHAUSTED)
    }

    /// `Promise.prototype.then`: record the reaction and return the promise
    /// that receives the handler's result.
    pub(super) fn promise_then(
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
        .map_err(|_| Completion::QUOTA_EXCEEDED)?;
        Ok(Value::object(derived))
    }

    /// A `SuppressedError`: `error` is what was thrown last, `suppressed`
    /// what it displaced.
    pub(super) fn suppressed_error(
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
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        Ok(made)
    }

    /// Dispose every resource a stack holds, last first. A disposer's throw
    /// suppresses whatever was thrown before it — the exception that was
    /// already propagating, or an earlier disposer's — and what remains is
    /// thrown once the stack is empty.
    pub(super) fn dispose_stack(
        &mut self,
        stack: Value,
        pending: Option<Value>,
    ) -> Result<(), Completion> {
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
    pub(super) fn push_disposable(
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
    pub(super) fn dispose_stack_next(
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
    pub(super) fn fold_pending(
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

    /// Push a resource on the dispose stack for `await using`.
    pub(super) fn op_add_disposable_async(
        &mut self,
        frame: &Frame,
        operands: &[u32; 3],
    ) -> Result<Flow, Completion> {
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

        Ok(Flow::Continue)
    }

    /// Push a resource on the dispose stack for `using`.
    pub(super) fn op_add_disposable(
        &mut self,
        frame: &Frame,
        operands: &[u32; 3],
    ) -> Result<Flow, Completion> {
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

        Ok(Flow::Continue)
    }
}
