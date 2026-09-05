//! The natives of `Array` and `Array.prototype`.

use super::*;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// The natives of `Array`.
    pub(in crate::vm) fn array_native(
        &mut self,
        id: u32,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        let second = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
        match id {
            native::ARRAY => {
                let array = self.new_array()?;
                if arguments.len() == 1 && matches!(first.tag(), Tag::Number) {
                    let length = value::to_uint32(first.as_number());
                    self.set_length(array, length)?;
                    return Ok(array);
                }
                for (index, &value) in arguments.iter().enumerate() {
                    let index = u32::try_from(index).unwrap_or(0);
                    self.set_element(array, index, value)?;
                }
                self.set_length(array, u32::try_from(arguments.len()).unwrap_or(0))?;
                Ok(array)
            }
            native::ARRAY_IS_ARRAY => {
                let array = self.is_array(first)?;
                Ok(Value::boolean(array))
            }
            native::ARRAY_OF => {
                let array = self.new_array()?;
                for (index, &value) in arguments.iter().enumerate() {
                    self.set_element(array, u32::try_from(index).unwrap_or(0), value)?;
                }
                self.set_length(array, u32::try_from(arguments.len()).unwrap_or(0))?;
                Ok(array)
            }
            native::ARRAY_FROM => {
                let array = self.new_array()?;
                let mut written = 0u32;
                let mapper = second;
                if let Some(iterator) = self.iterator_of(first)? {
                    loop {
                        let Some(value) = self.iterator_step(iterator)? else {
                            break;
                        };
                        let value = if mapper.is_undefined() {
                            value
                        } else {
                            let index =
                                Value::number(crate::softfloat::from_u64(u64::from(written)));
                            self.call_with(mapper, Value::UNDEFINED, &[value, index])?
                        };
                        self.set_element(array, written, value)?;
                        written += 1;
                    }
                } else {
                    let length = self.length_of(first)?;
                    while written < length {
                        let value = self.element(first, written)?;
                        let value = if mapper.is_undefined() {
                            value
                        } else {
                            let index =
                                Value::number(crate::softfloat::from_u64(u64::from(written)));
                            self.call_with(mapper, Value::UNDEFINED, &[value, index])?
                        };
                        self.set_element(array, written, value)?;
                        written += 1;
                    }
                }
                self.set_length(array, written)?;
                Ok(array)
            }
            native::ARRAY_PUSH => {
                let mut length = self.length_of(this)?;
                for &value in arguments {
                    self.set_element(this, length, value)?;
                    length += 1;
                }
                self.set_length(this, length)?;
                Ok(Value::number(crate::softfloat::from_u64(u64::from(length))))
            }
            native::ARRAY_POP => {
                let length = self.length_of(this)?;
                if length == 0 {
                    return Ok(Value::UNDEFINED);
                }
                let value = self.element(this, length - 1)?;
                self.delete_element(this, length - 1)?;
                self.set_length(this, length - 1)?;
                Ok(value)
            }
            native::ARRAY_SHIFT => {
                let length = self.length_of(this)?;
                if length == 0 {
                    return Ok(Value::UNDEFINED);
                }
                let value = self.element(this, 0)?;
                let mut index = 1u32;
                while index < length {
                    let moved = self.element(this, index)?;
                    self.set_element(this, index - 1, moved)?;
                    index += 1;
                }
                self.delete_element(this, length - 1)?;
                self.set_length(this, length - 1)?;
                Ok(value)
            }
            native::ARRAY_UNSHIFT => {
                let length = self.length_of(this)?;
                let count = u32::try_from(arguments.len()).unwrap_or(0);
                let mut index = length;
                while index > 0 {
                    index -= 1;
                    let moved = self.element(this, index)?;
                    self.set_element(this, index + count, moved)?;
                }
                for (offset, &value) in arguments.iter().enumerate() {
                    self.set_element(this, u32::try_from(offset).unwrap_or(0), value)?;
                }
                let total = length + count;
                self.set_length(this, total)?;
                Ok(Value::number(crate::softfloat::from_u64(u64::from(total))))
            }
            native::ARRAY_SLICE => {
                let length = self.length_of(this)?;
                let start = self.relative_index(first, length, 0)?;
                let end = self.relative_index(second, length, length)?;
                let array = self.new_array()?;
                let mut index = start;
                let mut written = 0u32;
                while index < end {
                    let value = self.element(this, index)?;
                    self.set_element(array, written, value)?;
                    written += 1;
                    index += 1;
                }
                self.set_length(array, written)?;
                Ok(array)
            }
            native::ARRAY_INDEX_OF | native::ARRAY_INCLUDES => {
                let length = self.length_of(this)?;
                let mut index = 0u32;
                while index < length {
                    let value = self.element(this, index)?;
                    let same = if id == native::ARRAY_INCLUDES {
                        value::same_value_zero(value, first)
                    } else {
                        self.strict_equals(value, first)?
                    };
                    if same {
                        return Ok(if id == native::ARRAY_INCLUDES {
                            Value::boolean(true)
                        } else {
                            Value::number(crate::softfloat::from_u64(u64::from(index)))
                        });
                    }
                    index += 1;
                }
                Ok(if id == native::ARRAY_INCLUDES {
                    Value::boolean(false)
                } else {
                    Value::number(-1.0)
                })
            }
            native::ARRAY_CONCAT => {
                let array = self.new_array()?;
                let mut written = 0u32;
                let length = self.length_of(this)?;
                let mut index = 0u32;
                while index < length {
                    let value = self.element(this, index)?;
                    self.set_element(array, written, value)?;
                    written += 1;
                    index += 1;
                }
                for &argument in arguments {
                    if self.is_array(argument)? {
                        let length = self.length_of(argument)?;
                        let mut index = 0u32;
                        while index < length {
                            let value = self.element(argument, index)?;
                            self.set_element(array, written, value)?;
                            written += 1;
                            index += 1;
                        }
                    } else {
                        self.set_element(array, written, argument)?;
                        written += 1;
                    }
                }
                self.set_length(array, written)?;
                Ok(array)
            }
            native::ARRAY_MAP
            | native::ARRAY_FILTER
            | native::ARRAY_FOR_EACH
            | native::ARRAY_SOME
            | native::ARRAY_EVERY
            | native::ARRAY_FIND
            | native::ARRAY_FIND_INDEX => self.array_walk(id, this, first, second),
            native::ARRAY_REDUCE => {
                let length = self.length_of(this)?;
                let mut index = 0u32;
                let mut accumulator = if arguments.len() > 1 {
                    second
                } else {
                    if length == 0 {
                        return Err(self.throw_type_error());
                    }
                    index = 1;
                    self.element(this, 0)?
                };
                while index < length {
                    let value = self.element(this, index)?;
                    let position = Value::number(crate::softfloat::from_u64(u64::from(index)));
                    accumulator = self.call_with(
                        first,
                        Value::UNDEFINED,
                        &[accumulator, value, position, this],
                    )?;
                    index += 1;
                }
                Ok(accumulator)
            }
            native::ARRAY_REVERSE => {
                let length = self.length_of(this)?;
                let mut low = 0u32;
                let mut high = length.saturating_sub(1);
                while low < high {
                    let left = self.element(this, low)?;
                    let right = self.element(this, high)?;
                    self.set_element(this, low, right)?;
                    self.set_element(this, high, left)?;
                    low += 1;
                    high -= 1;
                }
                Ok(this)
            }
            native::ARRAY_FILL => {
                let length = self.length_of(this)?;
                let start = self.relative_index(second, length, 0)?;
                let end = self.relative_index(
                    arguments.get(2).copied().unwrap_or(Value::UNDEFINED),
                    length,
                    length,
                )?;
                let mut index = start;
                while index < end {
                    self.set_element(this, index, first)?;
                    index += 1;
                }
                Ok(this)
            }
            native::ARRAY_SORT => self.sort_array(this, first),
            native::ARRAY_VALUES | native::ARRAY_KEYS | native::ARRAY_ENTRIES => {
                let kind = match id {
                    native::ARRAY_KEYS => ITERATE_KEYS,
                    native::ARRAY_ENTRIES => ITERATE_ENTRIES,
                    _ => ITERATE_VALUES,
                };
                let handle = object::create_iterator(
                    self.heap,
                    Value::object(self.realm.iterator_prototype),
                    this,
                    kind,
                )
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                Ok(Value::object(handle))
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
    }
}
