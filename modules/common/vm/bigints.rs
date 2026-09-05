//! BigInt arithmetic, comparison, and text, over the limb cells in `bigint`.

use super::*;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    pub(super) fn either_is_big_int(&self, left: Value, right: Value) -> bool {
        matches!(left.tag(), Tag::BigInt) || matches!(right.tag(), Tag::BigInt)
    }

    /// The limbs of a value that must be a BigInt.
    pub(super) fn big_int_operand(
        &mut self,
        value: Value,
    ) -> Result<crate::bigint::Number, Completion> {
        if !matches!(value.tag(), Tag::BigInt) {
            return Err(self.throw_type_error());
        }
        crate::bigint::read(self.heap, value.as_handle()).map_err(|_| Completion::MALFORMED)
    }

    /// What a failed BigInt operation means to a program: a numeral too big
    /// for the engine's limbs is a range error, because the text named a
    /// number the engine cannot hold; anything else is text that is not a
    /// numeral at all.
    pub(super) fn big_int_failure(&mut self, error: crate::bigint::BigIntError) -> Completion {
        match error {
            crate::bigint::BigIntError::TooLarge => self.throw_error_of(ErrorKind::Range),
            _ => self.throw_error_of(ErrorKind::Syntax),
        }
    }

    pub(super) fn big_int_value(
        &mut self,
        number: &crate::bigint::Number,
    ) -> Result<Value, Completion> {
        let handle =
            crate::bigint::write(self.heap, number).map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(Value::big_int(handle))
    }

    pub(super) fn big_int_arithmetic(
        &mut self,
        opcode: Opcode,
        left: Value,
        right: Value,
    ) -> Result<Value, Completion> {
        let left = self.big_int_operand(left)?;
        let right = self.big_int_operand(right)?;
        let outcome = match opcode {
            Opcode::Add => crate::bigint::add(&left, &right),
            Opcode::Sub => crate::bigint::subtract(&left, &right),
            Opcode::Mul => crate::bigint::multiply(&left, &right),
            Opcode::Div => crate::bigint::divide(&left, &right).map(|(quotient, _)| quotient),
            Opcode::Mod => crate::bigint::divide(&left, &right).map(|(_, remainder)| remainder),
            Opcode::Exp => crate::bigint::power(&left, &right),
            _ => return Err(self.throw_type_error()),
        };
        match outcome {
            Ok(number) => self.big_int_value(&number),
            Err(crate::bigint::BigIntError::DivisionByZero) => {
                Err(self.throw_error_of(ErrorKind::Range))
            }
            Err(crate::bigint::BigIntError::NegativeExponent) => {
                Err(self.throw_error_of(ErrorKind::Range))
            }
            Err(crate::bigint::BigIntError::TooLarge) => Err(self.throw_error_of(ErrorKind::Range)),
            Err(_) => Err(Completion::MALFORMED),
        }
    }

    pub(super) fn big_int_bitwise(
        &mut self,
        opcode: Opcode,
        left: Value,
        right: Value,
    ) -> Result<Value, Completion> {
        let left = self.big_int_operand(left)?;
        let right = self.big_int_operand(right)?;
        let outcome = match opcode {
            Opcode::BitAnd => crate::bigint::bitwise(crate::bigint::Bitwise::And, &left, &right),
            Opcode::BitOr => crate::bigint::bitwise(crate::bigint::Bitwise::Or, &left, &right),
            Opcode::BitXor => crate::bigint::bitwise(crate::bigint::Bitwise::Xor, &left, &right),
            Opcode::ShiftLeft | Opcode::ShiftRight => {
                let Some(count) = right.to_i64() else {
                    return Err(self.throw_error_of(ErrorKind::Range));
                };
                let left_shift = matches!(opcode, Opcode::ShiftLeft) == (count >= 0);
                let magnitude = count.unsigned_abs();
                if left_shift {
                    crate::bigint::shift_left(&left, magnitude)
                } else {
                    crate::bigint::shift_right(&left, magnitude)
                }
            }
            // An unsigned shift has no meaning for a value with no width.
            _ => return Err(self.throw_type_error()),
        };
        match outcome {
            Ok(number) => self.big_int_value(&number),
            Err(crate::bigint::BigIntError::TooLarge) => Err(self.throw_error_of(ErrorKind::Range)),
            Err(_) => Err(Completion::MALFORMED),
        }
    }

    /// How a BigInt orders against another value, exactly.
    ///
    /// A comparison with a Number is a comparison of mathematical values, so it
    /// is made on the integer part and then on what the fraction adds, rather
    /// than by rounding the BigInt to a double.
    pub(super) fn big_int_ordering(
        &mut self,
        left: Value,
        right: Value,
    ) -> Result<Option<core::cmp::Ordering>, Completion> {
        if matches!(left.tag(), Tag::BigInt) && matches!(right.tag(), Tag::BigInt) {
            let left = self.big_int_operand(left)?;
            let right = self.big_int_operand(right)?;
            return Ok(Some(crate::bigint::compare(&left, &right)));
        }
        let (big, other, flipped) = if matches!(left.tag(), Tag::BigInt) {
            (left, right, false)
        } else {
            (right, left, true)
        };
        let big = self.big_int_operand(big)?;
        let ordering = match other.tag() {
            Tag::String => {
                // A string compares as the BigInt it denotes, and as nothing at
                // all when it denotes none.
                let Ok(parsed) = self.big_int_of(other) else {
                    return Ok(None);
                };
                let parsed = self.big_int_operand(parsed)?;
                Some(crate::bigint::compare(&big, &parsed))
            }
            _ => {
                let number = self.coerce_to_number(other)?;
                if number.is_nan() {
                    None
                } else if number.is_infinite() {
                    Some(if number > 0.0 {
                        core::cmp::Ordering::Less
                    } else {
                        core::cmp::Ordering::Greater
                    })
                } else {
                    let integer = value::truncate(number);
                    let fraction = number - integer;
                    let other =
                        crate::bigint::from_f64(integer).map_err(|_| Completion::MALFORMED)?;
                    let ordering = crate::bigint::compare(&big, &other);
                    Some(if ordering != core::cmp::Ordering::Equal {
                        ordering
                    } else if fraction > 0.0 {
                        core::cmp::Ordering::Less
                    } else if fraction < 0.0 {
                        core::cmp::Ordering::Greater
                    } else {
                        core::cmp::Ordering::Equal
                    })
                }
            }
        };
        Ok(match (ordering, flipped) {
            (Some(ordering), true) => Some(ordering.reverse()),
            (ordering, _) => ordering,
        })
    }

    /// The text of a BigInt, which is its digits with no suffix.
    pub(super) fn big_int_text(&mut self, value: Value, radix: u32) -> Result<Value, Completion> {
        let number = self.big_int_operand(value)?;
        // A value of the admitted width is at most this many digits in binary,
        // which is the longest text any radix produces.
        let mut units = [0u16; crate::bigint::MAX_LIMBS * 32 + 2];
        let written = crate::bigint::text(&number, radix, &mut units);
        self.make_string(units.get(..written).unwrap_or(&[]))
    }

    /// `BigInt(value)`, which admits an integral Number, a string of digits, a
    /// boolean, or a BigInt.
    pub(super) fn big_int_of(&mut self, value: Value) -> Result<Value, Completion> {
        match value.tag() {
            Tag::BigInt => Ok(value),
            Tag::Boolean => {
                let mut number = crate::bigint::Number::ZERO;
                if value.as_boolean() {
                    number.limbs[0] = 1;
                    number.length = 1;
                }
                self.big_int_value(&number)
            }
            Tag::Number => {
                let number = crate::bigint::from_f64(value.as_number())
                    .map_err(|_| self.throw_error_of(ErrorKind::Range))?;
                self.big_int_value(&number)
            }
            Tag::String => {
                let handle = value.as_handle();
                let length = string::length(self.heap, handle)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)? as usize;
                // The numeral is what lies between the whitespace at either
                // end, so the end is found before the digits are read.
                let mut at = 0usize;
                let mut end = length;
                while at < end && is_string_white_space(self.string_unit(handle, at)?) {
                    at += 1;
                }
                while end > at && is_string_white_space(self.string_unit(handle, end - 1)?) {
                    end -= 1;
                }
                // An empty string, and one that is nothing but whitespace, is
                // zero. Everything else must be a numeral.
                if at == end {
                    return self.big_int_value(&crate::bigint::Number::ZERO);
                }
                let first = self.string_unit(handle, at)?;
                let signed = first == u16::from(b'-') || first == u16::from(b'+');
                let negative = first == u16::from(b'-');
                if signed {
                    at += 1;
                }
                // A radix prefix, on an unsigned numeral only.
                let mut radix = 10u32;
                if !signed && end - at >= 2 && self.string_unit(handle, at)? == u16::from(b'0') {
                    radix = match self.string_unit(handle, at + 1)? {
                        0x78 | 0x58 => 16,
                        0x6F | 0x4F => 8,
                        0x62 | 0x42 => 2,
                        _ => 10,
                    };
                    if radix != 10 {
                        at += 2;
                    }
                }
                let mut accumulator = crate::bigint::Accumulator::new(radix);
                while at < end {
                    let unit = self.string_unit(handle, at)?;
                    // The string grammar has no separators: only a literal
                    // written in source may carry them.
                    if unit > 0x7F || unit == u16::from(b'_') {
                        return Err(self.throw_error_of(ErrorKind::Syntax));
                    }
                    accumulator
                        .push(unit as u8)
                        .map_err(|error| self.big_int_failure(error))?;
                    at += 1;
                }
                // A sign or a radix prefix with no digits behind it is not a
                // numeral.
                if accumulator.digits() == 0 {
                    return Err(self.throw_error_of(ErrorKind::Syntax));
                }
                let number = accumulator.finish_signed(negative);
                self.big_int_value(&number)
            }
            _ => Err(self.throw_type_error()),
        }
    }
}
