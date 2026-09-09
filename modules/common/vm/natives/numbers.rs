//! The natives of `Number`, `Boolean`, and the global conversions.

use super::*;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// The natives of `Number`, `Boolean`, and the global conversions.
    pub(in crate::vm) fn number_native(
        &mut self,
        id: u32,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        match id {
            native::NUMBER => {
                if arguments.is_empty() {
                    return Ok(Value::number(0.0));
                }
                if matches!(first.tag(), Tag::BigInt) {
                    // The conversion is explicit here, so it is allowed to
                    // round; an arithmetic operation that mixed them would not.
                    let number = self.big_int_operand(first)?;
                    return Ok(Value::number(number.to_f64()));
                }
                Ok(Value::number(self.coerce_to_number(first)?))
            }
            native::BOOLEAN => Ok(Value::boolean(self.coerce_to_boolean(first)?)),
            native::NUMBER_IS_INTEGER | native::NUMBER_IS_SAFE_INTEGER => {
                let integral = matches!(first.tag(), Tag::Number)
                    && value::is_integral(first.as_number())
                    && (id == native::NUMBER_IS_INTEGER
                        || first.as_number().abs() <= 9_007_199_254_740_991.0);
                Ok(Value::boolean(integral))
            }
            native::NUMBER_IS_FINITE => Ok(Value::boolean(
                matches!(first.tag(), Tag::Number) && first.as_number().is_finite(),
            )),
            native::NUMBER_IS_NAN => Ok(Value::boolean(
                matches!(first.tag(), Tag::Number) && first.as_number().is_nan(),
            )),
            native::IS_NAN => {
                let number = self.coerce_to_number(first)?;
                Ok(Value::boolean(number.is_nan()))
            }
            native::IS_FINITE => {
                let number = self.coerce_to_number(first)?;
                Ok(Value::boolean(number.is_finite()))
            }
            native::PARSE_INT | native::PARSE_FLOAT => {
                let text = self.string_handle(first)?;
                let radix = if id == native::PARSE_INT {
                    let value = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
                    let number = self.coerce_to_number(value)?;
                    // Zero stands for "no radix was given" all the way down:
                    // only an absent radix admits a `0x` prefix, so resolving
                    // it to ten here would make `parseInt("0x1f", 10)` read a
                    // hexadecimal number a program asked to read as decimal.
                    value::to_uint32(number)
                } else {
                    10
                };
                self.parse_number(text, radix, id == native::PARSE_FLOAT)
            }
            native::NUMBER_TO_STRING => {
                let receiver = self.this_primitive_of(this, Tag::Number)?;
                let number = self.coerce_to_number(receiver)?;
                let radix = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                if radix.is_undefined() {
                    return self.coerce_to_string(Value::number(number));
                }
                let radix = value::to_uint32(self.coerce_to_number(radix)?);
                if !(2..=36).contains(&radix) {
                    return Err(self.throw_error_of(ErrorKind::Range));
                }
                if radix == 10 {
                    return self.coerce_to_string(Value::number(number));
                }
                let mut units = [0u16; 72];
                let written = crate::numeric::radix_text(number, radix, &mut units);
                self.make_string(units.get(..written).unwrap_or(&[]))
            }
            native::NUMBER_TO_FIXED => {
                let receiver = self.this_primitive_of(this, Tag::Number)?;
                let number = self.coerce_to_number(receiver)?;
                let digits = value::to_uint32(self.coerce_to_number(first)?);
                if digits > 100 {
                    return Err(self.throw_error_of(ErrorKind::Range));
                }
                let mut units = [0u16; 128];
                let written = crate::dtoa::fixed(number, digits, &mut units);
                self.make_string(units.get(..written).unwrap_or(&[]))
            }
            native::NUMBER_TO_EXPONENTIAL => {
                let receiver = self.this_primitive_of(this, Tag::Number)?;
                let number = self.coerce_to_number(receiver)?;
                let digits = if first.is_undefined() {
                    None
                } else {
                    let requested = self.coerce_to_number(first)?;
                    let requested = value::truncate(requested);
                    if !number.is_finite() {
                        None
                    } else if !(0.0..=100.0).contains(&requested) {
                        return Err(self.throw_error_of(ErrorKind::Range));
                    } else {
                        Some(requested as u32)
                    }
                };
                let mut units = [0u16; 128];
                let written = crate::dtoa::exponential(number, digits, &mut units);
                self.make_string(units.get(..written).unwrap_or(&[]))
            }
            native::NUMBER_TO_PRECISION => {
                let receiver = self.this_primitive_of(this, Tag::Number)?;
                let number = self.coerce_to_number(receiver)?;
                if first.is_undefined() {
                    return self.coerce_to_string(Value::number(number));
                }
                let requested = value::truncate(self.coerce_to_number(first)?);
                if !number.is_finite() {
                    return self.coerce_to_string(Value::number(number));
                }
                if !(1.0..=100.0).contains(&requested) {
                    return Err(self.throw_error_of(ErrorKind::Range));
                }
                let mut units = [0u16; 128];
                let written = crate::dtoa::precision(number, requested as u32, &mut units);
                self.make_string(units.get(..written).unwrap_or(&[]))
            }
            native::NUMBER_VALUE_OF => self.this_primitive_of(this, Tag::Number),
            native::BOOLEAN_VALUE_OF => self.this_primitive_of(this, Tag::Boolean),
            native::BOOLEAN_TO_STRING => {
                let receiver = self.this_primitive_of(this, Tag::Boolean)?;
                self.coerce_to_string(receiver)
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
    }
}
