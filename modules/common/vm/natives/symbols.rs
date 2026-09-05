//! The natives of `Symbol`.

use super::*;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    pub(in crate::vm) fn symbol_native(
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
                    .map_err(|_| Completion::HEAP_EXHAUSTED)? as usize;
                let mut units = [0u16; 128];
                let room = length.min(units.len());
                string::copy_units(
                    self.heap,
                    description.as_handle(),
                    units.get_mut(..room).unwrap_or(&mut []),
                )
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                let handle = string::create_symbol(self.heap, units.get(..room).unwrap_or(&[]))
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                Ok(Value::symbol(handle))
            }
            native::SYMBOL_TO_STRING => {
                let receiver = self.primitive_this(this)?;
                self.symbol_text(receiver)
            }
            native::SYMBOL_VALUE_OF => {
                let receiver = self.primitive_this(this)?;
                if !matches!(receiver.tag(), Tag::Symbol) {
                    return Err(self.throw_type_error());
                }
                Ok(receiver)
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
}
