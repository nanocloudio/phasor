//! Calls and construction: entering a frame for a function, a native, or a
//! constructor, and what `new` makes.

use super::*;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
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

    pub(super) fn call_value(
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
            let native =
                object::function_code(self.heap, function).map_err(|_| Completion::MALFORMED)?;
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
        let code = object::function_code(self.heap, function).map_err(|_| Completion::MALFORMED)?;
        let closure =
            object::function_environment(self.heap, function).map_err(|_| Completion::MALFORMED)?;

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
    pub(super) fn enter_call(
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
            return self.enter_native_call(function, this, arguments, construct);
        }
        if !construct
            && object::function_flags(self.heap, function).unwrap_or(0)
                & object::function_flag::CLASS
                != 0
        {
            // A class constructor answers only to `new`.
            return Err(self.throw_type_error());
        }
        let code = object::function_code(self.heap, function).map_err(|_| Completion::MALFORMED)?;
        let closure =
            object::function_environment(self.heap, function).map_err(|_| Completion::MALFORMED)?;
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
    pub(super) fn super_builds_from_source(
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
    pub(super) fn tail_call_admitted(&self, frame: &Frame, callee: Value) -> bool {
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

    /// The source text `Function(parameters..., body)` denotes — or, for the
    /// generator constructors, the generator function expression it makes.
    pub(super) fn function_source(
        &mut self,
        native: u32,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
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
    pub(super) fn new_instance(&mut self, callee: Value) -> Result<Value, Completion> {
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
        let instance =
            object::create(self.heap, prototype).map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(Value::object(instance))
    }

    /// Build the environment a called function starts with: a function record
    /// whose parent is the closure and whose bindings are the arguments.
    pub(super) fn prepare_call_environment(
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
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
            env::set_this(self.heap, record, bound).map_err(|_| Completion::HEAP_EXHAUSTED)?;
            // A derived constructor's `this` is dead until `super()` binds
            // it, for an arrow reading through as much as for the body.
            if derived {
                env::mark_this_uninitialised(self.heap, record)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            }
        }
        self.declare_slots(record, slots)?;
        Ok(Value::object(record))
    }

    /// Give a fresh function environment its `new.target`: the callee under
    /// `new`, unless an entry was named ahead of time, and undefined for a
    /// plain call.
    pub(super) fn bind_new_target(
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
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            env::set_function(self.heap, environment.as_handle(), callee)
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        Ok(())
    }

    /// Whether a frame runs an arrow, whose `super()`, `this` and fields all
    /// belong to the enclosing function.
    pub(super) fn frame_is_arrow(&self, frame: &Frame) -> bool {
        self.unit_of(frame.module)
            .function(frame.code)
            .is_some_and(|record| record.flags & record_flag::ARROW != 0)
    }

    /// The constructor a frame's `super()` constructs through, with the
    /// environment holding its `this`: the frame's own function, or for an
    /// arrow the nearest enclosing function's.
    pub(super) fn super_constructor_of(
        &mut self,
        frame: &Frame,
    ) -> Result<(Value, Value), Completion> {
        if !self.frame_is_arrow(frame) {
            return Ok((frame.callee, frame.environment));
        }
        let mut environment = frame.environment;
        let mut depth = 0u32;
        while environment.is_object() && depth <= env::MAX_SCOPE_DEPTH {
            let handle = environment.as_handle();
            if env::kind(self.heap, handle) == Ok(EnvironmentKind::Function) {
                let function =
                    env::function(self.heap, handle).map_err(|_| Completion::MALFORMED)?;
                return Ok((function, environment));
            }
            environment = env::parent(self.heap, handle).map_err(|_| Completion::MALFORMED)?;
            depth += 1;
        }
        Ok((Value::UNDEFINED, Value::UNDEFINED))
    }

    /// Bind `this` for a constructor whose `super()` ran in an arrow: the
    /// environment, and the constructor's own frame where it is still on
    /// the stack.
    pub(super) fn bind_constructor_this(
        &mut self,
        constructor: Value,
        environment: Value,
        this: Value,
    ) -> Result<(), Completion> {
        if environment.is_object() {
            env::set_this(self.heap, environment.as_handle(), this)
                .map_err(|_| Completion::MALFORMED)?;
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
    pub(super) fn new_target_of(&mut self, environment: Value) -> Result<Value, Completion> {
        let mut environment = environment;
        let mut depth = 0u32;
        while environment.is_object() && depth <= env::MAX_SCOPE_DEPTH {
            let handle = environment.as_handle();
            if env::kind(self.heap, handle) == Ok(EnvironmentKind::Function) {
                return env::new_target(self.heap, handle).map_err(|_| Completion::MALFORMED);
            }
            environment = env::parent(self.heap, handle).map_err(|_| Completion::MALFORMED)?;
            depth += 1;
        }
        Ok(Value::UNDEFINED)
    }

    /// Give a context its slots, unnamed and uninitialised.
    ///
    /// The lowering resolved every name to an index already, so a slot needs no
    /// name at run time. It starts uninitialised, which is what puts a `let`
    /// before its declaration in the temporal dead zone.
    pub(super) fn declare_slots(&mut self, record: Handle, slots: u32) -> Result<(), Completion> {
        let mut index = 0u32;
        while index < slots {
            env::declare(self.heap, record, Handle::new(0, 0), env::binding::MUTABLE)
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            index += 1;
        }
        Ok(())
    }

    pub(super) fn push_frame(
        &mut self,
        code: u32,
        environment: Value,
        this: Value,
        callee: Value,
        module: u32,
    ) -> Step {
        let Some(function) = self.unit_of(module).function(code) else {
            return Err(Completion::MALFORMED);
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

    /// Call `function` with `this` and up to four arguments.
    pub(super) fn call_with(
        &mut self,
        function: Value,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        self.call_value(function, this, arguments)
    }

    pub(super) fn is_callable_value(&self, value: Value) -> bool {
        value.is_object() && object::is_callable(self.heap, value.as_handle()) == Ok(true)
    }

    /// Call a native parent constructor for `super()`, with `this` already
    /// made: what a derived class over a library base does.
    /// Answers the instance the constructor continues with: a library base
    /// builds it — internal slots and all — so the parent is constructed
    /// and the eagerly made `this` is put aside.
    pub(super) fn super_call_native(
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
                let grandparent =
                    object::prototype(self.heap, handle).map_err(|_| Completion::MALFORMED)?;
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
    pub(super) fn super_call_native_or_enterless(
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
    pub(super) fn call_constructor_value(
        &mut self,
        callee: Value,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let function = callee.as_handle();
        let code = object::function_code(self.heap, function).map_err(|_| Completion::MALFORMED)?;
        let closure =
            object::function_environment(self.heap, function).map_err(|_| Completion::MALFORMED)?;
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

    /// Construct with `new`.
    pub(super) fn construct(
        &mut self,
        callee: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
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
            return self.construct_native(callee, function, new_target, arguments);
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
        let instance =
            object::create(self.heap, prototype).map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
    pub(super) fn construct_promise(
        &mut self,
        executor: Option<Value>,
    ) -> Result<Value, Completion> {
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
    pub(super) fn settle_function(
        &mut self,
        id: u32,
        promise: Handle,
    ) -> Result<Value, Completion> {
        let function = object::create_native(
            self.heap,
            Value::object(self.realm.function_prototype),
            id,
            0,
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        object::set_bound_value(self.heap, function, Value::object(promise))
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(Value::object(function))
    }

    /// A `finally` half bound to its callback.
    pub(super) fn finally_function(
        &mut self,
        id: u32,
        callback: Value,
    ) -> Result<Value, Completion> {
        let function = object::create_native(
            self.heap,
            Value::object(self.realm.function_prototype),
            id,
            0,
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        object::set_bound_value(self.heap, function, callback)
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(Value::object(function))
    }

    /// An ordinary object over a prototype, for a native constructor.
    pub(super) fn new_instance_of(&mut self, prototype: Handle) -> Result<Handle, Completion> {
        object::create(self.heap, Value::object(prototype)).map_err(|_| Completion::HEAP_EXHAUSTED)
    }

    /// A closure over the running frame: its kind decides its prototype and whether it constructs.
    pub(super) fn op_create_closure(&mut self, frame: &Frame, operands: &[u32; 3]) -> Step {
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
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
            let _ = object::set_prototype(self.heap, function, Value::object(function_home));
            let instances = object::create(self.heap, Value::object(instance_home))
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            let prototype_key = self.ascii_key(b"prototype")?;
            object::define_own_property(
                self.heap,
                function,
                prototype_key,
                Descriptor::data(Value::object(instances), attribute::WRITABLE),
            )
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        // Every closure keeps the home its surroundings had: an
        // arrow reads `super` through it, and any inner function
        // still names the class whose private scope encloses it.
        // Whether `super` itself is admitted was already decided at
        // compile time, so carrying the home is never a widening.
        if frame.callee.is_object() {
            if let Ok(Some(home)) = object::home_object(self.heap, frame.callee.as_handle()) {
                let _ = object::set_home_object(self.heap, function, home);
            }
        }
        self.accumulator = Value::object(function);

        Ok(())
    }

    /// The `arguments` object of the running call.
    pub(super) fn op_create_arguments(&mut self, frame: &Frame, operands: &[u32; 3]) -> Step {
        // An ordinary object, not an array: its `length` does not
        // follow its indices, and `Array.isArray` says no. It borrows
        // the array values iterator so `for (x of arguments)` walks
        // it, which is the one array behaviour it has.
        let count = frame.argument_count;
        let object = object::create(self.heap, Value::object(self.realm.object_prototype))
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        // A simple sloppy parameter list maps the leading indices
        // onto the parameter slots, in both directions.
        let mapped = operands[0].min(count);
        if mapped > 0 {
            object::map_arguments(self.heap, object, frame.environment, mapped)
                .map_err(|_| Completion::MALFORMED)?;
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
            let thrower =
                object::get_own_property(self.heap, self.realm.function_prototype, caller_key)
                    .unwrap_or(None)
                    .map_or(Value::UNDEFINED, |held| held.getter);
            object::Descriptor::accessor(thrower, thrower, 0)
        } else {
            object::Descriptor::data(frame.callee, attribute::WRITABLE | attribute::CONFIGURABLE)
        };
        object::define_own_property(self.heap, object, callee_key, callee)
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let values_key = self.ascii_key(b"values")?;
        let values = self.prototype_property(self.realm.array_prototype, target, values_key)?;
        if values.is_object() {
            object::define_own_property(
                self.heap,
                object,
                Key::Symbol(self.realm.iterator_symbol),
                object::Descriptor::data(values, attribute::WRITABLE | attribute::CONFIGURABLE),
            )
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        self.accumulator = target;

        Ok(())
    }

    /// `new f(...)`: enter a constructor, or make what a native constructs.
    pub(super) fn op_construct(
        &mut self,
        frame: &Frame,
        operands: &[u32; 3],
    ) -> Result<Flow, Completion> {
        let callee = self.register(frame, operands[0]);
        let first = operands[1];
        let count = operands[2];
        let mut arguments = [Value::UNDEFINED; MAX_ARGUMENTS];
        let passed = (count as usize).min(MAX_ARGUMENTS);
        let mut index = 0usize;
        while index < passed {
            arguments[index] = self.register(frame, first + u32::try_from(index).unwrap_or(0));
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
        if callee.is_object() && object::is_native(self.heap, callee.as_handle()).unwrap_or(false) {
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
                    .map_err(|_| Completion::MALFORMED)?;
                if parent.is_object()
                    && object::is_native(self.heap, parent.as_handle()).unwrap_or(false)
                {
                    let parent_native =
                        object::function_code(self.heap, parent.as_handle()).unwrap_or(0);
                    if Self::builds_from_source(parent_native) {
                        let key = self.ascii_key(b"prototype")?;
                        let prototype = self.get_property(callee, key)?;
                        let source = self.function_source(parent_native, &arguments[..passed])?;
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

        Ok(Flow::Continue)
    }

    /// `super(...args)`: the parent constructor over a spread argument list.
    pub(super) fn op_call_super_with_array(
        &mut self,
        frame: &Frame,
        operands: &[u32; 3],
    ) -> Result<Flow, Completion> {
        let arrow = self.frame_is_arrow(frame);
        let (callee, home) = self.super_constructor_of(frame)?;
        if !callee.is_object() {
            return Err(self.throw_type_error());
        }
        let parent =
            object::prototype(self.heap, callee.as_handle()).map_err(|_| Completion::MALFORMED)?;
        let list = self.register(frame, operands[0]);
        let length = self.length_of(list)?;
        // Refused rather than trimmed, for the reason `apply` gives.
        if (length as usize) > MAX_ARGUMENTS {
            return Err(self.throw_error_of(ErrorKind::Range));
        }
        let mut values = [Value::UNDEFINED; MAX_ARGUMENTS];
        let count = (length as usize).min(values.len());
        let mut index = 0usize;
        while index < count {
            values[index] = self.element(list, u32::try_from(index).unwrap_or(0))?;
            index += 1;
        }
        let this = if arrow {
            env::this_value(self.heap, home.as_handle()).map_err(|_| Completion::MALFORMED)?
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
        let made =
            self.super_call_native(parent, this, values.get(..count).unwrap_or(&[]), callee)?;
        if !arrow {
            if let Some(running) = self.frames.get_mut(self.depth as usize - 1) {
                running.this = made;
            }
            let environment = frame.environment;
            self.rebind_environment_this(environment, made)?;
        }
        self.accumulator = made;

        Ok(Flow::Continue)
    }

    /// `super(...)`: enter the parent constructor and bind `this` to what it makes.
    pub(super) fn op_call_super(
        &mut self,
        frame: &Frame,
        operands: &[u32; 3],
    ) -> Result<Flow, Completion> {
        let arrow = self.frame_is_arrow(frame);
        let (callee, home) = self.super_constructor_of(frame)?;
        if !callee.is_object() {
            return Err(self.throw_type_error());
        }
        let parent =
            object::prototype(self.heap, callee.as_handle()).map_err(|_| Completion::MALFORMED)?;
        let first = operands[0];
        let count = operands[1];
        let mut arguments = [Value::UNDEFINED; MAX_ARGUMENTS];
        let passed = (count as usize).min(MAX_ARGUMENTS);
        let mut index = 0usize;
        while index < passed {
            arguments[index] = self.register(frame, first + u32::try_from(index).unwrap_or(0));
            index += 1;
        }
        let this = if arrow {
            env::this_value(self.heap, home.as_handle()).map_err(|_| Completion::MALFORMED)?
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

        Ok(Flow::Continue)
    }

    /// Shape a class constructor: its flags, its prototype object, and the parent it derives from.
    pub(super) fn op_make_class_constructor(&mut self, frame: &Frame, operands: &[u32; 3]) -> Step {
        let constructor = self.accumulator;
        let proto = self.register(frame, operands[0]);
        if !constructor.is_object() || !proto.is_object() {
            return Err(Completion::MALFORMED);
        }
        let function = constructor.as_handle();
        let mut flags = object::function_flag::CONSTRUCTOR
            | object::function_flag::CLASS
            | object::function_flag::STRICT;
        if operands[1] != 0 {
            flags |= object::function_flag::DERIVED;
        }
        object::add_function_flags(self.heap, function, flags)
            .map_err(|_| Completion::MALFORMED)?;
        object::set_home_object(self.heap, function, proto.as_handle())
            .map_err(|_| Completion::MALFORMED)?;
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
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        let prototype_key = self.ascii_key(b"prototype")?;
        object::define_own_property(
            self.heap,
            function,
            prototype_key,
            Descriptor::data(proto, 0),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let constructor_key = self.ascii_key(b"constructor")?;
        object::define_own_property(
            self.heap,
            proto.as_handle(),
            constructor_key,
            Descriptor::data(constructor, attribute::WRITABLE | attribute::CONFIGURABLE),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;

        Ok(())
    }

    /// A call: a bytecode function enters a frame, a native runs in place, a tail call gives up the running frame first.
    pub(super) fn op_call(
        &mut self,
        frame: &Frame,
        opcode: Opcode,
        operands: &[u32; 3],
    ) -> Result<Flow, Completion> {
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
        if matches!(opcode, Opcode::TailCall) && self.tail_call_admitted(frame, callee) {
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

        Ok(Flow::Continue)
    }

    /// A call over a spread argument list.
    pub(super) fn op_call_with_array(
        &mut self,
        frame: &Frame,
        operands: &[u32; 3],
    ) -> Result<Flow, Completion> {
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

        Ok(Flow::Continue)
    }

    /// `new f(...args)` over a spread argument list.
    pub(super) fn op_construct_with_array(
        &mut self,
        frame: &Frame,
        operands: &[u32; 3],
    ) -> Result<Flow, Completion> {
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

        Ok(Flow::Continue)
    }

    /// A suspended sync generator resumes on this loop, its frame standing
    /// in for the call: an eval inside it can pause the machine, and its
    /// yield answers the call. Answers whether it resumed.
    pub(super) fn resume_generator_call(
        &mut self,
        function: Handle,
        native: u32,
        arguments: &[Value],
    ) -> Result<bool, Completion> {
        // A suspended sync generator resumes on this loop, its frame
        // standing in for the call: an eval inside it can pause the
        // machine, and its yield answers the call.
        let generator =
            object::function_environment(self.heap, function).map_err(|_| Completion::MALFORMED)?;
        if generator.is_object() {
            if let Some((raw_state, coroutine)) =
                object::generator(self.heap, generator.as_handle())
                    .map_err(|_| Completion::MALFORMED)?
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
                        let argument = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                        self.restore_coroutine(coroutine, argument, kind, true)?;
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    /// Enter a native callee. `eval` with a string pauses the machine: the
    /// compiler lives in the host, which compiles the source into a unit and
    /// enters it. `call` and `apply` enter their target directly, so an eval
    /// inside it can still pause. Anything the machine does not enter answers
    /// `false`, and the caller runs it in place.
    pub(super) fn enter_native_call(
        &mut self,
        function: Handle,
        this: Value,
        arguments: &[Value],
        construct: bool,
    ) -> Result<bool, Completion> {
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
            && self.resume_generator_call(function, native, arguments)?
        {
            return Ok(true);
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
        Ok(false)
    }

    /// A bound function constructs its target, with the bound arguments
    /// first, for the `new.target` the bound function was given — or for the
    /// target itself when it was the one named.
    pub(super) fn construct_bound(
        &mut self,
        callee: Value,
        function: Handle,
        new_target: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        // The target constructs, with the bound arguments first, for
        // the `new.target` the bound function was given — or for the
        // target itself when it was the one named.
        let record =
            object::function_environment(self.heap, function).map_err(|_| Completion::MALFORMED)?;
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
        self.construct(target, values.get(..count).unwrap_or(&[]))
    }

    /// The written-nothing constructor: the instance, with the arguments
    /// forwarded to the parent when the class derives, and the class's
    /// instance fields either way.
    pub(super) fn construct_default(
        &mut self,
        callee: Value,
        function: Handle,
        new_target: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        // The written-nothing constructor: the instance, with the
        // arguments forwarded to the parent when the class derives,
        // and the class's instance fields either way.
        let flags = object::function_flags(self.heap, function).unwrap_or(0);
        if flags & object::function_flag::DERIVED != 0 {
            let parent =
                object::prototype(self.heap, function).map_err(|_| Completion::MALFORMED)?;
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
                        let _ =
                            object::set_prototype(self.heap, instance.as_handle(), subclass_proto);
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
        Ok(instance)
    }

    /// `new` on a proxy: the `construct` trap, or the target when there is
    /// none.
    pub(super) fn construct_through_proxy(
        &mut self,
        callee: Value,
        new_target: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
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
        Ok(made)
    }

    /// `new` on a function the engine implements itself: each constructor
    /// makes what it makes, and the library's plain constructors behave as
    /// the call does with a primitive result wrapped.
    pub(super) fn construct_native(
        &mut self,
        callee: Value,
        function: Handle,
        new_target: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let native =
            object::function_code(self.heap, function).map_err(|_| Completion::MALFORMED)?;
        if native == native::BOUND_FUNCTION {
            return self.construct_bound(callee, function, new_target, arguments);
        }
        if native == native::DEFAULT_CONSTRUCTOR {
            return self.construct_default(callee, function, new_target, arguments);
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
            return self.construct_weak_ref(arguments.first().copied().unwrap_or(Value::UNDEFINED));
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
            return self.construct_through_proxy(callee, new_target, arguments);
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
        Err(Completion::Terminated(Termination::NotImplemented))
    }

    /// The constructor a class without one gets.
    pub(super) fn op_create_default_constructor(&mut self, operands: &[u32; 3]) -> Step {
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
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        self.accumulator = Value::object(function);

        Ok(())
    }

    /// The prototype a class heritage provides.
    pub(super) fn op_get_heritage_prototype(&mut self, frame: &Frame, operands: &[u32; 3]) -> Step {
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

        Ok(())
    }
}
