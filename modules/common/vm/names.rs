//! Name resolution: bindings through the environment chain, the global
//! lexical record, context slots, and what a direct eval declares.

use super::*;

/// Spare environment capacity a dynamic function keeps for the bindings
/// sloppy direct eval code declares at run time.
pub(super) const EVAL_VAR_SPARE: u32 = 16;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// The global lexical binding of a name, if a script declared one: the
    /// record holding it and its index, walking the chain of records beneath
    /// the head down to the global object's environment.
    pub(super) fn global_lexical_find(
        &mut self,
        name: Handle,
    ) -> Result<Option<(Handle, u32)>, Completion> {
        let mut current = self.realm.lexical;
        let mut depth = 0u32;
        loop {
            if env::kind(self.heap, current).map_err(|_| Completion::MALFORMED)?
                != EnvironmentKind::Declarative
            {
                return Ok(None);
            }
            if let Some(index) =
                env::index_of(self.heap, current, name).map_err(|_| Completion::MALFORMED)?
            {
                return Ok(Some((current, index)));
            }
            let parent = env::parent(self.heap, current).map_err(|_| Completion::MALFORMED)?;
            if !parent.is_object() || depth > env::MAX_SCOPE_DEPTH {
                return Ok(None);
            }
            current = parent.as_handle();
            depth += 1;
        }
    }

    /// Declare a global lexical binding, threading a fresh record beneath
    /// the head when it has no room left.
    pub(super) fn global_lexical_declare(
        &mut self,
        name: Handle,
        flags: u8,
    ) -> Result<(), Completion> {
        let head = self.realm.lexical;
        if self.declare_in_chain(head, name, flags)? {
            return Ok(());
        }
        let parent = env::parent(self.heap, head).map_err(|_| Completion::MALFORMED)?;
        let fresh = env::create(
            self.heap,
            EnvironmentKind::Declarative,
            parent,
            crate::realm::GLOBAL_LEXICAL_CAPACITY * 4,
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        env::set_parent(self.heap, head, Value::object(fresh))
            .map_err(|_| Completion::MALFORMED)?;
        env::declare(self.heap, fresh, name, flags).map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(())
    }

    /// Declare a name in the first declarative record of a chain with room,
    /// answering whether one had any.
    pub(super) fn declare_in_chain(
        &mut self,
        head: Handle,
        name: Handle,
        flags: u8,
    ) -> Result<bool, Completion> {
        let mut current = head;
        let mut depth = 0u32;
        loop {
            if env::kind(self.heap, current).map_err(|_| Completion::MALFORMED)?
                != EnvironmentKind::Declarative
            {
                return Ok(false);
            }
            if env::declare(self.heap, current, name, flags).is_ok() {
                return Ok(true);
            }
            let parent = env::parent(self.heap, current).map_err(|_| Completion::MALFORMED)?;
            if !parent.is_object() || depth > env::MAX_SCOPE_DEPTH {
                return Ok(false);
            }
            current = parent.as_handle();
            depth += 1;
        }
    }

    /// Read a global lexical binding, if the name has one: its dead zone is
    /// the ReferenceError the specification makes it.
    pub(super) fn global_lexical_read(&mut self, key: Key) -> Result<Option<Value>, Completion> {
        let Key::Name(name) = key else {
            return Ok(None);
        };
        let Some((environment, index)) = self.global_lexical_find(name)? else {
            return Ok(None);
        };
        match env::slot_value(self.heap, environment, index) {
            Ok(value) => Ok(Some(value)),
            Err(env::EnvironmentError::Uninitialised) => Err(self.throw_reference_error()),
            Err(_) => Err(Completion::MALFORMED),
        }
    }

    /// Write a global lexical binding, if the name has one, answering whether
    /// it did: a const refuses with a TypeError, a binding still in its dead
    /// zone with a ReferenceError.
    pub(super) fn global_lexical_write(
        &mut self,
        key: Key,
        value: Value,
    ) -> Result<bool, Completion> {
        let Key::Name(name) = key else {
            return Ok(false);
        };
        let Some((environment, index)) = self.global_lexical_find(name)? else {
            return Ok(false);
        };
        let flags =
            env::binding_flags(self.heap, environment, index).map_err(|_| Completion::MALFORMED)?;
        if flags & env::binding::INITIALISED == 0 {
            return Err(self.throw_reference_error());
        }
        if flags & env::binding::MUTABLE == 0 {
            return Err(self.throw_type_error());
        }
        env::set_slot(self.heap, environment, index, value).map_err(|_| Completion::MALFORMED)?;
        self.accumulator = value;
        Ok(true)
    }

    /// Whether a script has declared the name with `var` or as a function.
    pub(super) fn global_var_name_declared(&mut self, name: Handle) -> Result<bool, Completion> {
        let mut current = self.realm.var_names;
        let mut depth = 0u32;
        loop {
            if env::index_of(self.heap, current, name)
                .map_err(|_| Completion::MALFORMED)?
                .is_some()
            {
                return Ok(true);
            }
            let parent = env::parent(self.heap, current).map_err(|_| Completion::MALFORMED)?;
            if !parent.is_object() || depth > env::MAX_SCOPE_DEPTH {
                return Ok(false);
            }
            current = parent.as_handle();
            depth += 1;
        }
    }

    /// Record a name a script declared with `var` or as a function.
    pub(super) fn global_var_name_declare(&mut self, name: Handle) -> Result<(), Completion> {
        if self.global_var_name_declared(name)? {
            return Ok(());
        }
        let head = self.realm.var_names;
        if self.declare_in_chain(head, name, env::binding::MUTABLE)? {
            return Ok(());
        }
        let parent = env::parent(self.heap, head).map_err(|_| Completion::MALFORMED)?;
        let fresh = env::create(
            self.heap,
            EnvironmentKind::Declarative,
            parent,
            crate::realm::GLOBAL_LEXICAL_CAPACITY * 4,
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        env::set_parent(self.heap, head, Value::object(fresh))
            .map_err(|_| Completion::MALFORMED)?;
        env::declare(self.heap, fresh, name, env::binding::MUTABLE)
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(())
    }

    /// Resolve a name through the environment chain the way run-time lookup
    /// must: an object environment whose object's `Symbol.unscopables`
    /// blocks the name is stepped past rather than matched.
    pub(super) fn resolve_name(
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
                .map_err(|_| Completion::MALFORMED)?;
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
                        .map_err(|_| Completion::MALFORMED)?;
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

    /// Find a name through the environment chain the way a direct eval's
    /// world requires: a binding an eval created is found by its text, an
    /// object environment by its property, and anything else falls to the
    /// global object. `None` is an unresolvable name.
    pub(super) fn dynamic_read(
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
    pub(super) fn dynamic_read_base(
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
                                .map_err(|_| Completion::MALFORMED)?;
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
            .map_err(|_| Completion::MALFORMED)?;
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
    pub(super) fn shadowing_binding(
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
                        .map_err(|_| Completion::MALFORMED)?;
                    return self.get_property(object, key).map(Some);
                }
                self.slot_read(found.environment, found.index).map(Some)
            }
            Ok(_) => Ok(None),
            Err(_) => Err(Completion::MALFORMED),
        }
    }

    /// Write through a shadowing named binding, answering whether one took
    /// the value.
    pub(super) fn shadowing_store(
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
                        .map_err(|_| Completion::MALFORMED)?;
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
    pub(super) fn prepare_shadowable(
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
                        .map_err(|_| Completion::MALFORMED)?;
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
            current =
                env::parent(self.heap, current.as_handle()).map_err(|_| Completion::MALFORMED)?;
            remaining -= 1;
        }
        Ok(current)
    }

    pub(super) fn read_prepared(
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
            let object =
                env::binding_object(self.heap, handle).map_err(|_| Completion::MALFORMED)?;
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
                .map_err(|_| Completion::MALFORMED)?
                .unwrap_or(slot)
        } else {
            slot
        };
        self.slot_read(handle, index)
    }

    pub(super) fn write_prepared(
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
            let object =
                env::binding_object(self.heap, handle).map_err(|_| Completion::MALFORMED)?;
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
                .map_err(|_| Completion::MALFORMED)?
                .unwrap_or(slot)
        } else {
            slot
        };
        self.slot_write(handle, index, value)
    }

    /// Read a binding by index: its dead zone is a ReferenceError.
    pub(super) fn slot_read(
        &mut self,
        environment: Handle,
        index: u32,
    ) -> Result<Value, Completion> {
        match env::slot_value(self.heap, environment, index) {
            Ok(value) => Ok(value),
            Err(env::EnvironmentError::Uninitialised) => Err(self.throw_reference_error()),
            Err(_) => Err(Completion::MALFORMED),
        }
    }

    /// Write a binding by index: its dead zone is a ReferenceError, and an
    /// immutable binding — a `const` — refuses with a TypeError.
    pub(super) fn slot_write(
        &mut self,
        environment: Handle,
        index: u32,
        value: Value,
    ) -> Result<(), Completion> {
        match env::set_slot(self.heap, environment, index, value) {
            Ok(()) => Ok(()),
            Err(env::EnvironmentError::Uninitialised) => Err(self.throw_reference_error()),
            Err(env::EnvironmentError::Immutable) => Err(self.throw_type_error()),
            Err(_) => Err(Completion::MALFORMED),
        }
    }

    /// Assign a name the way sloppy code does: into the binding that holds
    /// it, or as a new property of the global object.
    pub(super) fn dynamic_write(
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
                                .map_err(|_| Completion::MALFORMED)?;
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
                .map_err(|_| Completion::MALFORMED)?;
            if !present {
                return Err(self.throw_reference_error());
            }
        }
        self.set_property(Value::object(self.realm.global), key, value)
    }

    /// Declare a `var` from eval code in the nearest variable environment: a
    /// function or arrow environment when the chain holds one, the global
    /// object otherwise. A binding that exists is left exactly as it is.
    pub(super) fn declare_eval_var(
        &mut self,
        environment: Value,
        key: Key,
    ) -> Result<(), Completion> {
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
                            .map_err(|_| Completion::MALFORMED)?;
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
                            .map_err(|_| Completion::MALFORMED)?;
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
            .map_err(|_| Completion::MALFORMED)?;
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
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
    pub(super) fn dynamic_delete(
        &mut self,
        environment: Value,
        key: Key,
    ) -> Result<bool, Completion> {
        if let Key::Name(name) = key {
            if environment.is_object() {
                match self.resolve_name(environment.as_handle(), name) {
                    Ok(Some(found)) => {
                        if found.index != u32::MAX {
                            let flags =
                                env::binding_flags(self.heap, found.environment, found.index)
                                    .map_err(|_| Completion::MALFORMED)?;
                            if flags & env::binding::PERMANENT != 0 {
                                return Ok(false);
                            }
                            return env::remove(self.heap, found.environment, found.index)
                                .map(|()| true)
                                .map_err(|_| Completion::MALFORMED);
                        }
                        let object = env::binding_object(self.heap, found.environment)
                            .map_err(|_| Completion::MALFORMED)?;
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

    // Environments.

    pub(super) fn context_slot(
        &mut self,
        frame: &Frame,
        index: u32,
        depth: u32,
    ) -> Result<Value, Completion> {
        let mut environment = frame.environment;
        let mut remaining = depth;
        while remaining > 0 {
            if !environment.is_object() {
                return Err(self.throw_reference_error());
            }
            environment = env::parent(self.heap, environment.as_handle())
                .map_err(|_| Completion::MALFORMED)?;
            remaining -= 1;
        }
        if !environment.is_object() {
            return Err(self.throw_reference_error());
        }
        match env::slot_value(self.heap, environment.as_handle(), index) {
            Ok(value) => Ok(value),
            Err(env::EnvironmentError::Uninitialised) => Err(self.throw_reference_error()),
            Err(_) => Err(Completion::MALFORMED),
        }
    }

    /// Count a context the running frame entered or left.
    pub(super) fn adjust_contexts(&mut self, change: i32) {
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
    pub(super) fn set_frame_environment(&mut self, environment: Value) {
        if let Some(frame) = self.frames.get_mut(self.depth.saturating_sub(1) as usize) {
            frame.environment = environment;
        }
    }

    /// The `this` of the nearest enclosing function environment.
    /// Make a template array immutable: every element loses write and
    /// reshape, and the array takes no more.
    pub(super) fn freeze_template(&mut self, array: Value) -> Result<(), Completion> {
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
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let _ = object::prevent_extensions(self.heap, array.as_handle());
        Ok(())
    }

    /// Point the frame's function environment at a replacement `this` — a
    /// `super()` whose parent overrode its return.
    pub(super) fn rebind_environment_this(
        &mut self,
        environment: Value,
        this: Value,
    ) -> Result<(), Completion> {
        let mut current = environment;
        let mut depth = 0u32;
        while current.is_object() && depth <= env::MAX_SCOPE_DEPTH {
            let handle = current.as_handle();
            if env::kind(self.heap, handle) == Ok(EnvironmentKind::Function) {
                env::set_this(self.heap, handle, this).map_err(|_| Completion::MALFORMED)?;
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

    pub(super) fn this_value(&mut self, frame: &Frame) -> Result<Value, Completion> {
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
                return env::this_value(self.heap, handle).map_err(|_| Completion::MALFORMED);
            }
            environment = env::parent(self.heap, handle).map_err(|_| Completion::MALFORMED)?;
            depth += 1;
        }
        // Outside any function, `this` is what the host started the task
        // with: the entry frame's receiver, not the current frame's, which a
        // top-level arrow called through `call` would otherwise read.
        Ok(self.frames.first().map_or(frame.this, |entry| entry.this))
    }

    pub(super) fn init_context_slot(
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
                .map_err(|_| Completion::MALFORMED)?;
            remaining -= 1;
        }
        if !environment.is_object() {
            return Err(self.throw_reference_error());
        }
        env::initialise(self.heap, environment.as_handle(), index, value)
            .map_err(|_| Completion::MALFORMED)
    }

    pub(super) fn set_context_slot(
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
                .map_err(|_| Completion::MALFORMED)?;
            remaining -= 1;
        }
        if !environment.is_object() {
            return Err(self.throw_reference_error());
        }
        match env::set_slot(self.heap, environment.as_handle(), index, value) {
            Ok(()) => Ok(()),
            Err(env::EnvironmentError::Uninitialised) => Err(self.throw_reference_error()),
            Err(env::EnvironmentError::Immutable) => Err(self.throw_type_error()),
            Err(_) => Err(Completion::MALFORMED),
        }
    }

    /// The template object a tagged-template site answers, made once per site and frozen.
    pub(super) fn op_cache_template(&mut self, frame: &Frame, operands: &[u32; 3]) -> Step {
        let site = operands[0];
        let built = self.accumulator;
        let registry_key = self.ascii_key(b"\0tpl")?;
        let held = object::get_own_property(self.heap, self.realm.global, registry_key)
            .map_err(|_| Completion::MALFORMED)?;
        let registry = match held {
            Some(descriptor) if descriptor.value.is_object() => descriptor.value,
            _ => {
                let made = object::create(self.heap, Value::NULL)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                object::define_own_property(
                    self.heap,
                    self.realm.global,
                    registry_key,
                    Descriptor::data(Value::object(made), attribute::WRITABLE),
                )
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
            .map_err(|_| Completion::MALFORMED)?;
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
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                    self.freeze_template(raw)?;
                }
                object::define_own_property(
                    self.heap,
                    registry.as_handle(),
                    site_key,
                    Descriptor::data(built, attribute::WRITABLE),
                )
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                self.accumulator = built;
            }
        }

        Ok(())
    }

    /// A script's top-level function declaration, on the global object.
    pub(super) fn op_declare_global_function(&mut self, operands: &[u32; 3]) -> Step {
        let key = self.constant_key(operands[0])?;
        let check_only = operands[1] == 0;
        let from_eval = operands[1] == 2;
        let existing = object::get_own_property(self.heap, self.realm.global, key)
            .map_err(|_| Completion::MALFORMED)?;
        match existing {
            None if check_only => {
                let extensible = object::is_extensible(self.heap, self.realm.global)
                    .map_err(|_| Completion::MALFORMED)?;
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
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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

        Ok(())
    }

    /// Bind `this` in a derived constructor once `super()` has made it.
    pub(super) fn op_bind_this(&mut self, frame: &Frame) -> Result<Flow, Completion> {
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
                env::this_value(self.heap, home.as_handle()).map_err(|_| Completion::MALFORMED)?
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

        Ok(Flow::Continue)
    }

    /// A script's top-level `var`, on the global object.
    pub(super) fn op_declare_global(&mut self, operands: &[u32; 3]) -> Step {
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
            .map_err(|_| Completion::MALFORMED)?;
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
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            if !admitted {
                return Err(self.throw_type_error());
            }
        }

        Ok(())
    }

    /// Store to a global name the compiler resolved.
    pub(super) fn op_sta_global_resolved(
        &mut self,
        frame: &Frame,
        operands: &[u32; 3],
    ) -> Result<Flow, Completion> {
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
            .map_err(|_| Completion::MALFORMED)?;
        if !present {
            return Err(self.throw_error_of(ErrorKind::Reference));
        }
        self.set_property_of(Value::object(self.realm.global), key, value, true)?;
        self.accumulator = value;

        Ok(Flow::Continue)
    }

    /// A script's lexical name checked against what earlier scripts declared.
    pub(super) fn op_check_global_lexical(&mut self, operands: &[u32; 3]) -> Step {
        let key = self.constant_key(operands[0])?;
        if let Key::Name(name) = key {
            let taken =
                self.global_lexical_find(name)?.is_some() || self.global_var_name_declared(name)?;
            if taken {
                return Err(self.throw_error_of(ErrorKind::Syntax));
            }
        }
        // HasRestrictedGlobalProperty: a non-configurable global
        // property may not be shadowed by a lexical.
        let existing = object::get_own_property(self.heap, self.realm.global, key)
            .map_err(|_| Completion::MALFORMED)?;
        if let Some(held) = existing {
            if held.attributes & attribute::CONFIGURABLE == 0 {
                return Err(self.throw_error_of(ErrorKind::Syntax));
            }
        }

        Ok(())
    }

    /// Store to an unresolved global in strict code, which throws when it is not there.
    pub(super) fn op_sta_global_strict(&mut self, operands: &[u32; 3]) -> Result<Flow, Completion> {
        let key = self.constant_key(operands[0])?;
        let value = self.accumulator;
        if self.global_lexical_write(key, value)? {
            return Ok(Flow::Continue);
        }
        let present = object::has_property(self.heap, self.realm.global, key)
            .map_err(|_| Completion::MALFORMED)?;
        if !present {
            // Strict assignment never creates a binding: a name the
            // global object lost — or never had — is a reference
            // error, not a new property.
            return Err(self.throw_error_of(ErrorKind::Reference));
        }
        let value = self.accumulator;
        self.set_property_of(Value::object(self.realm.global), key, value, true)?;
        self.accumulator = value;

        Ok(Flow::Continue)
    }

    /// Initialise a script's top-level lexical binding.
    pub(super) fn op_init_global_lexical(&mut self, operands: &[u32; 3]) -> Step {
        let key = self.constant_key(operands[0])?;
        let value = self.accumulator;
        let mut done = false;
        if let Key::Name(name) = key {
            if let Some((environment, index)) = self.global_lexical_find(name)? {
                env::initialise(self.heap, environment, index, value)
                    .map_err(|_| Completion::MALFORMED)?;
                done = true;
            }
        }
        if !done {
            self.set_property(Value::object(self.realm.global), key, value)?;
        }
        self.accumulator = value;

        Ok(())
    }

    /// Read a global, or undefined where it is not there.
    pub(super) fn op_lda_global_or_undefined(
        &mut self,
        operands: &[u32; 3],
    ) -> Result<Flow, Completion> {
        let key = self.constant_key(operands[0])?;
        if let Some(value) = self.global_lexical_read(key)? {
            self.accumulator = value;
            return Ok(Flow::Continue);
        }
        let present = object::has_property(self.heap, self.realm.global, key)
            .map_err(|_| Completion::MALFORMED)?;
        self.accumulator = if present {
            self.get_property(Value::object(self.realm.global), key)?
        } else {
            Value::UNDEFINED
        };

        Ok(Flow::Continue)
    }
}
