//! The natives of `RegExp.prototype`.

use super::*;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// The natives of `RegExp`, and the string methods that take a pattern.
    pub(in crate::vm) fn regexp_native(
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
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?
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
                            .map_err(|_| Completion::HEAP_EXHAUSTED)?
                            as usize;
                        let mut units = [0u16; 16];
                        let room = length.min(units.len());
                        string::copy_units(
                            self.heap,
                            text,
                            units.get_mut(..room).unwrap_or(&mut []),
                        )
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                        crate::regexp::flags_of(units.get(..room).unwrap_or(&[]))
                            .map_err(|_| self.throw_error_of(ErrorKind::Syntax))?
                    }
                    _ => 0,
                };
                let length = string::length(self.heap, pattern)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)? as usize;
                let mut units = [0u16; 512];
                let room = length.min(units.len());
                string::copy_units(self.heap, pattern, units.get_mut(..room).unwrap_or(&mut []))
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                let text = string::concat(self.heap, text, slash.as_handle())
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                let text = string::concat(self.heap, text, flags.as_handle())
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
}
