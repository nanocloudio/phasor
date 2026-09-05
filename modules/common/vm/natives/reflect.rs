//! `Reflect`.

use super::*;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// `Reflect`: the object operations, refusing anything but an object.
    pub(in crate::vm) fn reflect_native(
        &mut self,
        id: u32,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let target = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        if !target.is_object() {
            return Err(self.throw_type_error());
        }
        let second = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
        match id {
            native::REFLECT_GET => {
                let key = self.coerce_to_key(second)?;
                if self.hidden_key(key) {
                    return Ok(Value::UNDEFINED);
                }
                let receiver = arguments.get(2).copied().unwrap_or(target);
                self.super_get(target, key, receiver)
            }
            native::REFLECT_SET => {
                let key = self.coerce_to_key(second)?;
                let value = arguments.get(2).copied().unwrap_or(Value::UNDEFINED);
                // A receiver of its own routes the write through OrdinarySet:
                // the receiver's own record, or a proxy's traps, take it.
                if let Some(&receiver) = arguments.get(3) {
                    if !(receiver.is_object() && receiver.as_handle() == target.as_handle()) {
                        let done = self.set_with_receiver(target, key, value, receiver)?;
                        return Ok(Value::boolean(done));
                    }
                }
                // A namespace refuses every write; the refusal is the answer,
                // not a throw — though a binding in its dead zone still is.
                if matches!(
                    object::exotic_kind(self.heap, target.as_handle()).unwrap_or(0),
                    object::exotic::NAMESPACE | object::exotic::DEFERRED
                ) {
                    if self.deferred_done(target) {
                        self.namespace_touch(target, key)?;
                    }
                    return Ok(Value::boolean(false));
                }
                self.set_property_of(target, key, value, false)?;
                Ok(Value::boolean(true))
            }
            native::REFLECT_HAS => {
                let key = self.coerce_to_key(second)?;
                let present = !self.hidden_key(key)
                    && object::has_property(self.heap, target.as_handle(), key)
                        .map_err(|_| Completion::MALFORMED)?;
                Ok(Value::boolean(present))
            }
            native::REFLECT_DELETE => {
                let key = self.coerce_to_key(second)?;
                self.materialise_function_facts(target, key)?;
                let removed = object::delete(self.heap, target.as_handle(), key)
                    .map_err(|_| Completion::MALFORMED)?;
                Ok(Value::boolean(removed))
            }
            native::REFLECT_OWN_KEYS => self.own_entries(target, native::REFLECT_OWN_KEYS),
            native::REFLECT_GET_PROTOTYPE => {
                object::prototype(self.heap, target.as_handle()).map_err(|_| Completion::MALFORMED)
            }
            native::REFLECT_SET_PROTOTYPE => {
                if !second.is_object() && !second.is_null() {
                    return Err(self.throw_type_error());
                }
                let admitted = object::set_prototype(self.heap, target.as_handle(), second)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                Ok(Value::boolean(admitted))
            }
            native::REFLECT_IS_EXTENSIBLE => {
                let extensible = object::is_extensible(self.heap, target.as_handle())
                    .map_err(|_| Completion::MALFORMED)?;
                Ok(Value::boolean(extensible))
            }
            native::REFLECT_PREVENT_EXTENSIONS => {
                object::prevent_extensions(self.heap, target.as_handle())
                    .map_err(|_| Completion::MALFORMED)?;
                Ok(Value::boolean(true))
            }
            native::REFLECT_DEFINE_PROPERTY => {
                match self.object_native(
                    native::OBJECT_DEFINE_PROPERTY,
                    Value::UNDEFINED,
                    arguments,
                ) {
                    Ok(_) => Ok(Value::boolean(true)),
                    Err(Completion::Throw(_)) => Ok(Value::boolean(false)),
                    Err(other) => Err(other),
                }
            }
            native::REFLECT_GET_OWN_DESCRIPTOR => self.object_native(
                native::OBJECT_GET_OWN_PROPERTY_DESCRIPTOR,
                Value::UNDEFINED,
                arguments,
            ),
            native::REFLECT_APPLY => {
                let list = arguments.get(2).copied().unwrap_or(Value::UNDEFINED);
                let mut values = [Value::UNDEFINED; MAX_ARGUMENTS];
                let count = if list.is_object() {
                    let length = self.length_of(list)?;
                    let count = (length as usize).min(values.len());
                    let mut index = 0usize;
                    while index < count {
                        values[index] = self.element(list, u32::try_from(index).unwrap_or(0))?;
                        index += 1;
                    }
                    count
                } else {
                    0
                };
                self.call_value(target, second, &values[..count])
            }
            native::REFLECT_CONSTRUCT => {
                let list = second;
                let mut values = [Value::UNDEFINED; MAX_ARGUMENTS];
                let count = if list.is_object() {
                    let length = self.length_of(list)?;
                    let count = (length as usize).min(values.len());
                    let mut index = 0usize;
                    while index < count {
                        values[index] = self.element(list, u32::try_from(index).unwrap_or(0))?;
                        index += 1;
                    }
                    count
                } else {
                    0
                };
                if let Some(&named) = arguments.get(2) {
                    if !named.is_object()
                        || !object::is_constructor(self.heap, named.as_handle()).unwrap_or(false)
                    {
                        return Err(self.throw_type_error());
                    }
                    self.pending_new_target = named;
                }
                self.construct(target, &values[..count])
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
    }
}
