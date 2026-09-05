//! Proxy objects: the traps and the invariants they run under.

use super::*;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    // Proxy

    /// `new Proxy(target, handler)`: a callable target makes a callable
    /// proxy, a constructor a constructor; either interposes the handler.
    pub(super) fn construct_proxy(
        &mut self,
        target: Value,
        handler: Value,
    ) -> Result<Value, Completion> {
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
            .map_err(|_| Completion::HEAP_EXHAUSTED)?
        } else {
            object::create(self.heap, Value::NULL).map_err(|_| Completion::HEAP_EXHAUSTED)?
        };
        for (name, held) in [(&b"\0target"[..], target), (&b"\0handler"[..], handler)] {
            let key = self.ascii_key(name)?;
            object::define_own_property(
                self.heap,
                made,
                key,
                Descriptor::data(held, attribute::WRITABLE),
            )
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        object::set_exotic_kind(self.heap, made, object::exotic::PROXY)
            .map_err(|_| Completion::MALFORMED)?;
        Ok(Value::object(made))
    }

    /// A proxy's target and handler.
    pub(super) fn proxy_parts(&mut self, proxy: Value) -> Result<(Value, Value), Completion> {
        if !proxy.is_object() {
            return Err(self.throw_type_error());
        }
        let mut parts = [Value::UNDEFINED; 2];
        for (slot, name) in parts.iter_mut().zip([&b"\0target"[..], &b"\0handler"[..]]) {
            let key = self.ascii_key(name)?;
            *slot = object::get_own_property(self.heap, proxy.as_handle(), key)
                .map_err(|_| Completion::MALFORMED)?
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
    pub(super) fn proxy_own_keys(
        &mut self,
        proxy: Value,
        out: &mut [Key],
    ) -> Result<usize, Completion> {
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
                return Err(Completion::QUOTA_EXCEEDED);
            };
            *slot = key;
            written += 1;
        }
        Ok(written)
    }

    /// Whether a proxy reports an own enumerable property under `key`: the
    /// `getOwnPropertyDescriptor` trap's answer, or the target's own record.
    pub(super) fn proxy_own_enumerable(
        &mut self,
        proxy: Value,
        key: Key,
    ) -> Result<bool, Completion> {
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

    /// The handler's trap of a name, or undefined where it has none.
    pub(super) fn proxy_trap(&mut self, handler: Value, name: &[u8]) -> Result<Value, Completion> {
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

    pub(super) fn proxy_get(&mut self, proxy: Value, key: Key) -> Result<Value, Completion> {
        let (target, handler) = self.proxy_parts(proxy)?;
        let trap = self.proxy_trap(handler, b"get")?;
        if trap.is_undefined() {
            return self.get_property(target, key);
        }
        let name = self.key_to_value(key)?;
        self.call_value(trap, handler, &[target, name, proxy])
    }

    pub(super) fn proxy_set(
        &mut self,
        proxy: Value,
        key: Key,
        value: Value,
    ) -> Result<bool, Completion> {
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

    pub(super) fn proxy_has(&mut self, proxy: Value, key: Key) -> Result<bool, Completion> {
        let (target, handler) = self.proxy_parts(proxy)?;
        let trap = self.proxy_trap(handler, b"has")?;
        if trap.is_undefined() {
            return object::has_property(self.heap, target.as_handle(), key)
                .map_err(|_| Completion::MALFORMED);
        }
        let name = self.key_to_value(key)?;
        let answer = self.call_value(trap, handler, &[target, name])?;
        self.coerce_to_boolean(answer)
    }

    pub(super) fn proxy_delete(&mut self, proxy: Value, key: Key) -> Result<bool, Completion> {
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
    pub(super) fn proxy_define(
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
            .map_err(|_| Completion::HEAP_EXHAUSTED);
        }
        let descriptor = object::create(self.heap, Value::object(self.realm.object_prototype))
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        let name = self.key_to_value(key)?;
        let answer = self.call_value(trap, handler, &[target, name, Value::object(descriptor)])?;
        self.coerce_to_boolean(answer)
    }
}
