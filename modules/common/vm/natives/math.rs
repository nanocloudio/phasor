//! `Math`.

use super::*;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// The natives of `Math`, each a pure function of its arguments.
    pub(in crate::vm) fn math_native(
        &mut self,
        id: u32,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        let value = self.coerce_to_number(first)?;
        let result = match id {
            native::MATH_ABS => {
                if value < 0.0 {
                    -value
                } else {
                    value
                }
            }
            native::MATH_FLOOR => value::floor(value),
            native::MATH_CEIL => value::ceil(value),
            native::MATH_ROUND => value::floor(value + 0.5),
            native::MATH_TRUNC => value::truncate(value),
            native::MATH_SQRT => crate::numeric::sqrt(value),
            native::MATH_SIGN => {
                if value.is_nan() {
                    value
                } else if value > 0.0 {
                    1.0
                } else if value < 0.0 {
                    -1.0
                } else {
                    value
                }
            }
            native::MATH_POW => {
                let exponent =
                    self.coerce_to_number(arguments.get(1).copied().unwrap_or(Value::UNDEFINED))?;
                crate::numeric::power(value, exponent)
            }
            native::MATH_MIN | native::MATH_MAX => {
                let mut best = if id == native::MATH_MIN {
                    f64::INFINITY
                } else {
                    f64::NEG_INFINITY
                };
                for &argument in arguments {
                    let number = self.coerce_to_number(argument)?;
                    if number.is_nan() {
                        best = f64::NAN;
                        break;
                    }
                    let better = if id == native::MATH_MIN {
                        number < best
                    } else {
                        number > best
                    };
                    if better {
                        best = number;
                    }
                }
                best
            }
            native::MATH_SIN => crate::numeric::sin(value),
            native::MATH_COS => crate::numeric::cos(value),
            native::MATH_TAN => crate::numeric::tan(value),
            native::MATH_ASIN => crate::numeric::asin(value),
            native::MATH_ACOS => crate::numeric::acos(value),
            native::MATH_ATAN => crate::numeric::atan(value),
            native::MATH_ATAN2 => {
                let x =
                    self.coerce_to_number(arguments.get(1).copied().unwrap_or(Value::UNDEFINED))?;
                crate::numeric::atan2(value, x)
            }
            native::MATH_EXP => crate::numeric::exp(value),
            native::MATH_LOG => crate::numeric::log(value),
            native::MATH_LOG2 => crate::numeric::log_2(value),
            native::MATH_LOG10 => crate::numeric::log_10(value),
            native::MATH_CBRT => crate::numeric::cbrt(value),
            native::MATH_HYPOT => {
                let mut total = 0.0f64;
                for &argument in arguments {
                    let number = self.coerce_to_number(argument)?;
                    total += number * number;
                }
                crate::numeric::sqrt(total)
            }
            _ => return Err(Completion::Terminated(Termination::NotImplemented)),
        };
        Ok(Value::number(result))
    }
}
