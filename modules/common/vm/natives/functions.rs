//! The natives of `Function.prototype`.

use super::*;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// `call`, `apply`, and `bind`, which are how a receiver is chosen.
    pub(in crate::vm) fn function_native(
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
                // A bound function constructs exactly when its target does.
                let flags = if this.is_object()
                    && object::is_constructor(self.heap, this.as_handle()).unwrap_or(false)
                {
                    object::function_flag::CONSTRUCTOR
                } else {
                    0
                };
                let bound = object::create_native(
                    self.heap,
                    Value::object(self.realm.function_prototype),
                    native::BOUND_FUNCTION,
                    flags,
                )
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                object::set_bound_value(self.heap, bound, record)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                // A bound function's `length` and `name` come from what it was
                // bound to and are settled here: they are the one pair the
                // lazy path cannot work out from the callable alone, because
                // the answer is the target's, less what is already bound.
                self.name_bound_function(bound, this, written.saturating_sub(2))?;
                Ok(Value::object(bound))
            }
            native::BOUND_FUNCTION => {
                let Some(function) = self.current_native else {
                    return Err(Completion::MALFORMED);
                };
                let record = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::MALFORMED)?;
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
}
