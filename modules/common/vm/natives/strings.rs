//! The natives of `String` and `String.prototype`.

use super::*;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// The natives of `String`.
    pub(in crate::vm) fn string_native(
        &mut self,
        id: u32,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        let second = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
        if id == native::STRING {
            if arguments.is_empty() {
                return self.ascii_string(b"");
            }
            if matches!(first.tag(), Tag::Symbol) {
                return self.symbol_text(first);
            }
            return self.coerce_to_string(first);
        }
        if id == native::STRING_FROM_CHAR_CODE {
            let mut units = [0u16; MAX_ARGUMENTS];
            let mut count = 0usize;
            for &argument in arguments {
                let number = self.coerce_to_number(argument)?;
                if let Some(slot) = units.get_mut(count) {
                    *slot = value::to_uint16(number);
                    count += 1;
                }
            }
            return self.make_string(units.get(..count).unwrap_or(&[]));
        }

        if id == native::STRING_TO_STRING {
            // thisStringValue: the primitive, a wrapper's value, or a
            // TypeError — never a coercion, which would call back here.
            return self.this_primitive_of(this, Tag::String);
        }
        let receiver = self.primitive_this(this)?;
        let text = self.string_handle(receiver)?;
        let length = string::length(self.heap, text).map_err(|_| Completion::HEAP_EXHAUSTED)?;
        match id {
            native::STRING_TO_STRING => Ok(Value::string(text)),
            native::STRING_CHAR_AT | native::STRING_AT => {
                let number = if first.is_undefined() {
                    0.0
                } else {
                    self.coerce_to_number(first)?
                };
                let index = value::truncate(number);
                let index = if id == native::STRING_AT && index < 0.0 {
                    f64::from(length) + index
                } else {
                    index
                };
                if index < 0.0 || index >= f64::from(length) {
                    return if id == native::STRING_AT {
                        Ok(Value::UNDEFINED)
                    } else {
                        self.ascii_string(b"")
                    };
                }
                let unit = string::unit_at(self.heap, text, value::to_uint32(index))
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?
                    .unwrap_or(0);
                self.make_string(&[unit])
            }
            native::STRING_CHAR_CODE_AT => {
                let index = self.index_argument(first)?;
                match string::unit_at(self.heap, text, index)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?
                {
                    Some(unit) => Ok(Value::number(crate::softfloat::from_u64(u64::from(unit)))),
                    None => Ok(Value::number(f64::NAN)),
                }
            }
            native::STRING_CODE_POINT_AT => {
                let index = self.index_argument(first)?;
                match string::code_point_at(self.heap, text, index)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?
                {
                    Some((point, _)) => {
                        Ok(Value::number(crate::softfloat::from_u64(u64::from(point))))
                    }
                    None => Ok(Value::UNDEFINED),
                }
            }
            native::STRING_INDEX_OF | native::STRING_LAST_INDEX_OF | native::STRING_INCLUDES => {
                let needle = self.string_handle(first)?;
                let found = if id == native::STRING_LAST_INDEX_OF {
                    string::last_index_of(self.heap, text, needle)
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?
                } else {
                    let from = if second.is_undefined() {
                        0
                    } else {
                        self.index_argument(second)?
                    };
                    string::index_of(self.heap, text, needle, from)
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?
                };
                if id == native::STRING_INCLUDES {
                    return Ok(Value::boolean(found.is_some()));
                }
                Ok(match found {
                    Some(index) => Value::number(crate::softfloat::from_u64(u64::from(index))),
                    None => Value::number(-1.0),
                })
            }
            native::STRING_STARTS_WITH | native::STRING_ENDS_WITH => {
                let needle = self.string_handle(first)?;
                let needle_length =
                    string::length(self.heap, needle).map_err(|_| Completion::HEAP_EXHAUSTED)?;
                let at = if id == native::STRING_STARTS_WITH {
                    if second.is_undefined() {
                        0
                    } else {
                        self.index_argument(second)?
                    }
                } else {
                    let end = if second.is_undefined() {
                        length
                    } else {
                        self.index_argument(second)?.min(length)
                    };
                    match end.checked_sub(needle_length) {
                        Some(at) => at,
                        None => return Ok(Value::boolean(false)),
                    }
                };
                let matched = string::matches_at(self.heap, text, needle, at)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                Ok(Value::boolean(matched))
            }
            native::STRING_SLICE => {
                let start = self.relative_index(first, length, 0)?;
                let end = self.relative_index(second, length, length)?;
                let handle = string::slice(self.heap, text, start, end.max(start))
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                Ok(Value::string(handle))
            }
            native::STRING_SUBSTRING => {
                let start = self.clamped_index(first, length, 0)?;
                let end = self.clamped_index(second, length, length)?;
                let (start, end) = if start <= end {
                    (start, end)
                } else {
                    (end, start)
                };
                let handle = string::slice(self.heap, text, start, end)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                Ok(Value::string(handle))
            }
            native::STRING_TO_UPPER_CASE | native::STRING_TO_LOWER_CASE => {
                let handle =
                    string::convert_case(self.heap, text, id == native::STRING_TO_UPPER_CASE)
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                Ok(Value::string(handle))
            }
            native::STRING_TRIM => {
                let handle = string::trim(self.heap, text, true, true)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                Ok(Value::string(handle))
            }
            native::STRING_REPEAT => {
                let number = self.coerce_to_number(first)?;
                if number < 0.0 || !number.is_finite() {
                    return Err(self.throw_error_of(ErrorKind::Range));
                }
                let count = value::to_uint32(value::truncate(number));
                let handle = string::repeat(self.heap, text, count)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                Ok(Value::string(handle))
            }
            native::STRING_PAD_START | native::STRING_PAD_END => {
                let target = self.index_argument(first)?;
                let filler = if second.is_undefined() {
                    self.ascii_string(b" ")?
                } else {
                    self.coerce_to_string(second)?
                };
                let handle = string::pad(
                    self.heap,
                    text,
                    target,
                    filler.as_handle(),
                    id == native::STRING_PAD_START,
                )
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                Ok(Value::string(handle))
            }
            native::STRING_CONCAT => {
                let mut result = text;
                for &argument in arguments {
                    let other = self.string_handle(argument)?;
                    result = string::concat(self.heap, result, other)
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                }
                Ok(Value::string(result))
            }
            native::STRING_REPLACE
                if first.is_object()
                    && object::regexp_program(self.heap, first.as_handle())
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?
                        .is_some() =>
            {
                self.replace_with_pattern(text, first, second)
            }
            native::STRING_SPLIT
                if first.is_object()
                    && object::regexp_program(self.heap, first.as_handle())
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?
                        .is_some() =>
            {
                self.split_by_pattern(text, first)
            }
            native::STRING_REPLACE => {
                let needle = self.string_handle(first)?;
                let Some(at) = string::index_of(self.heap, text, needle, 0)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?
                else {
                    return Ok(Value::string(text));
                };
                let needle_length =
                    string::length(self.heap, needle).map_err(|_| Completion::HEAP_EXHAUSTED)?;
                let replacement = if self.is_callable_value(second) {
                    let matched = string::slice(self.heap, text, at, at + needle_length)
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                    let position = Value::number(crate::softfloat::from_u64(u64::from(at)));
                    let outcome = self.call_with(
                        second,
                        Value::UNDEFINED,
                        &[Value::string(matched), position, Value::string(text)],
                    )?;
                    self.string_handle(outcome)?
                } else {
                    self.string_handle(second)?
                };
                let head = string::slice(self.heap, text, 0, at)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                let tail = string::slice(self.heap, text, at + needle_length, length)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                let joined = string::concat(self.heap, head, replacement)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                let joined = string::concat(self.heap, joined, tail)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                Ok(Value::string(joined))
            }
            native::STRING_SPLIT => {
                let array = self.new_array()?;
                if first.is_undefined() {
                    self.set_element(array, 0, Value::string(text))?;
                    self.set_length(array, 1)?;
                    return Ok(array);
                }
                let separator = self.string_handle(first)?;
                let separator_length =
                    string::length(self.heap, separator).map_err(|_| Completion::HEAP_EXHAUSTED)?;
                let mut written = 0u32;
                let mut start = 0u32;
                if separator_length == 0 {
                    // An empty separator splits into single code units.
                    while start < length {
                        let piece = string::slice(self.heap, text, start, start + 1)
                            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                        self.set_element(array, written, Value::string(piece))?;
                        written += 1;
                        start += 1;
                    }
                    self.set_length(array, written)?;
                    return Ok(array);
                }
                loop {
                    let found = string::index_of(self.heap, text, separator, start)
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                    let at = found.unwrap_or(length);
                    let piece = string::slice(self.heap, text, start, at)
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                    self.set_element(array, written, Value::string(piece))?;
                    written += 1;
                    match found {
                        Some(at) => start = at + separator_length,
                        None => break,
                    }
                }
                self.set_length(array, written)?;
                Ok(array)
            }
            native::STRING_VALUES => {
                let handle = object::create_iterator(
                    self.heap,
                    Value::object(self.realm.iterator_prototype),
                    Value::string(text),
                    ITERATE_CODE_POINTS,
                )
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                Ok(Value::object(handle))
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
    }
}
