//! Arbitrary-precision integers.
//!
//! A BigInt is a heap cell holding a sign and a little-endian sequence of
//! 32-bit limbs. The value is exact: nothing here rounds, and an operation that
//! would need more limbs than a build admits fails rather than wrapping.
//!
//! Every operation allocates its result, so an operand is never written to. A
//! cell holds no reference, which is what lets the collector treat a BigInt the
//! way it treats a string.

use crate::heap::{CellKind, Heap, HeapError};
use crate::value::Handle;

/// Bytes before the limbs: the sign, padding, and the limb count.
const HEADER: usize = 8;
/// Limbs one value may have, which bounds a program's arithmetic at about two
/// thousand bits.
///
/// The bound is what a value costs, not what the arithmetic can express: a
/// working value is held in limbs on the stack, and an isolate's stack is small
/// enough that a bigger one would not fit beside the frames a program is
/// already using.
pub const MAX_LIMBS: usize = 64;
/// Limbs an operation may work through on the stack.
const WORK_LIMBS: usize = MAX_LIMBS + 2;

/// Why an operation did not produce a value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BigIntError {
    Heap(HeapError),
    /// The result would need more limbs than a build admits.
    TooLarge,
    /// Division or remainder by zero.
    DivisionByZero,
    /// A negative exponent, which has no integer value.
    NegativeExponent,
    /// The text is not a BigInt literal.
    Malformed,
}

impl From<HeapError> for BigIntError {
    fn from(error: HeapError) -> Self {
        Self::Heap(error)
    }
}

/// A value read out of the heap, held in limbs on the stack.
#[derive(Clone, Copy)]
pub struct Number {
    pub negative: bool,
    pub limbs: [u32; WORK_LIMBS],
    pub length: usize,
}

impl Number {
    pub const ZERO: Self = Self {
        negative: false,
        limbs: [0; WORK_LIMBS],
        length: 0,
    };

    pub fn is_zero(&self) -> bool {
        self.length == 0
    }

    fn trim(&mut self) {
        while self.length > 0 && self.limbs[self.length - 1] == 0 {
            self.length -= 1;
        }
        if self.length == 0 {
            self.negative = false;
        }
    }

    fn push(&mut self, limb: u32) -> Result<(), BigIntError> {
        if self.length >= WORK_LIMBS {
            return Err(BigIntError::TooLarge);
        }
        self.limbs[self.length] = limb;
        self.length += 1;
        Ok(())
    }

    fn limb(&self, index: usize) -> u32 {
        if index < self.length {
            self.limbs[index]
        } else {
            0
        }
    }

    /// The value as a `f64`, rounded as `Number(bigint)` rounds.
    pub fn to_f64(self) -> f64 {
        let mut value = 0.0f64;
        let mut index = self.length;
        while index > 0 {
            index -= 1;
            value = value * 4_294_967_296.0 + f64::from(self.limbs[index]);
        }
        if self.negative {
            -value
        } else {
            value
        }
    }

    /// Whether the value fits an `i64`, and its value if it does.
    pub fn to_i64(self) -> Option<i64> {
        if self.length > 2 {
            return None;
        }
        let magnitude = u64::from(self.limb(0)) | (u64::from(self.limb(1)) << 32);
        if self.negative {
            if magnitude > (i64::MAX as u64) + 1 {
                return None;
            }
            Some((magnitude as i64).wrapping_neg())
        } else {
            if magnitude > i64::MAX as u64 {
                return None;
            }
            Some(magnitude as i64)
        }
    }
}

/// Read a BigInt cell into limbs.
pub fn read(heap: &Heap<'_>, handle: Handle) -> Result<Number, BigIntError> {
    if heap.kind(handle)? != CellKind::BigInt {
        return Err(BigIntError::Malformed);
    }
    let cell = heap.cell(handle)?;
    if cell.len() < HEADER {
        return Err(BigIntError::Malformed);
    }
    let length = u32::from_le_bytes([cell[4], cell[5], cell[6], cell[7]]) as usize;
    if length > MAX_LIMBS {
        return Err(BigIntError::TooLarge);
    }
    let mut value = Number {
        negative: cell[0] != 0,
        limbs: [0; WORK_LIMBS],
        length,
    };
    let mut index = 0usize;
    while index < length {
        let at = HEADER + index * 4;
        let Some(bytes) = cell.get(at..at + 4) else {
            return Err(BigIntError::Malformed);
        };
        value.limbs[index] = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        index += 1;
    }
    value.trim();
    Ok(value)
}

/// Write limbs into a new BigInt cell.
pub fn write(heap: &mut Heap<'_>, value: &Number) -> Result<Handle, BigIntError> {
    let mut value = *value;
    value.trim();
    if value.length > MAX_LIMBS {
        return Err(BigIntError::TooLarge);
    }
    let size = u32::try_from(HEADER + value.length * 4).map_err(|_| BigIntError::TooLarge)?;
    let handle = heap.allocate(CellKind::BigInt, size)?;
    let cell = heap.cell_mut(handle)?;
    cell[0] = u8::from(value.negative);
    let length = u32::try_from(value.length).unwrap_or(0);
    cell[4..8].copy_from_slice(&length.to_le_bytes());
    let mut index = 0usize;
    while index < value.length {
        let at = HEADER + index * 4;
        let bytes = value.limbs[index].to_le_bytes();
        if let Some(slot) = cell.get_mut(at..at + 4) {
            slot.copy_from_slice(&bytes);
        }
        index += 1;
    }
    Ok(handle)
}

/// A numeral built one digit at a time, for a reader that does not hold the
/// text contiguously.
///
/// Taking digits as they are read is what keeps a numeral exact: there is no
/// buffer to fill, so the limb bound is the only limit, and a numeral past it
/// is `TooLarge` rather than a shorter number that looks like an answer.
#[derive(Clone, Copy)]
pub struct Accumulator {
    value: Number,
    radix: u32,
    digits: usize,
}

impl Accumulator {
    /// A numeral in `radix`, with no digits yet.
    pub const fn new(radix: u32) -> Self {
        Self {
            value: Number::ZERO,
            radix,
            digits: 0,
        }
    }

    /// Take one digit. A separator is skipped and counts as no digit; a
    /// character the radix does not admit is `Malformed`.
    pub fn push(&mut self, byte: u8) -> Result<(), BigIntError> {
        if !(2..=36).contains(&self.radix) {
            return Err(BigIntError::Malformed);
        }
        if byte == b'_' {
            return Ok(());
        }
        let digit = match byte {
            b'0'..=b'9' => u32::from(byte - b'0'),
            b'a'..=b'z' => u32::from(byte - b'a') + 10,
            b'A'..=b'Z' => u32::from(byte - b'A') + 10,
            _ => return Err(BigIntError::Malformed),
        };
        if digit >= self.radix {
            return Err(BigIntError::Malformed);
        }
        multiply_small(&mut self.value, self.radix)?;
        add_small(&mut self.value, digit)?;
        self.digits += 1;
        Ok(())
    }

    /// How many digits have been taken, separators aside. A numeral with none
    /// is not a numeral, which only the caller knows how to answer.
    pub const fn digits(&self) -> usize {
        self.digits
    }

    /// The number the digits denote.
    pub fn finish(mut self) -> Number {
        self.value.trim();
        self.value
    }

    /// The number the digits denote, negated where the text said so. Zero has
    /// no sign.
    pub fn finish_signed(mut self, negative: bool) -> Number {
        self.value.negative = negative && !self.value.is_zero();
        self.finish()
    }
}

/// The value a digit sequence in `radix` denotes.
pub fn from_digits(digits: &[u8], radix: u32, negative: bool) -> Result<Number, BigIntError> {
    if !(2..=36).contains(&radix) {
        return Err(BigIntError::Malformed);
    }
    let mut accumulator = Accumulator::new(radix);
    for &byte in digits {
        accumulator.push(byte)?;
    }
    Ok(accumulator.finish_signed(negative))
}

/// The value an integral `f64` denotes.
pub fn from_f64(value: f64) -> Result<Number, BigIntError> {
    if !value.is_finite() || crate::value::truncate(value) != value {
        return Err(BigIntError::Malformed);
    }
    let negative = value < 0.0;
    let mut magnitude = if negative { -value } else { value };
    let mut number = Number::ZERO;
    // Peel 32 bits at a time from the top, which needs no integer conversion
    // wider than the format holds.
    let mut scale = 1.0f64;
    let mut limbs = 0usize;
    while magnitude / scale >= 4_294_967_296.0 {
        scale *= 4_294_967_296.0;
        limbs += 1;
    }
    loop {
        let quotient = crate::value::truncate(magnitude / scale);
        let limb = quotient as u32;
        number.limbs[limbs] = limb;
        if number.length <= limbs {
            number.length = limbs + 1;
        }
        magnitude -= quotient * scale;
        if limbs == 0 {
            break;
        }
        limbs -= 1;
        scale /= 4_294_967_296.0;
    }
    number.negative = negative;
    number.trim();
    Ok(number)
}

fn multiply_small(value: &mut Number, factor: u32) -> Result<(), BigIntError> {
    let mut carry = 0u64;
    let mut index = 0usize;
    while index < value.length {
        let product = u64::from(value.limbs[index]) * u64::from(factor) + carry;
        value.limbs[index] = (product & 0xFFFF_FFFF) as u32;
        carry = product >> 32;
        index += 1;
    }
    while carry != 0 {
        value.push((carry & 0xFFFF_FFFF) as u32)?;
        carry >>= 32;
    }
    Ok(())
}

fn add_small(value: &mut Number, addend: u32) -> Result<(), BigIntError> {
    let mut carry = u64::from(addend);
    let mut index = 0usize;
    while carry != 0 {
        if index >= value.length {
            value.push(0)?;
        }
        let sum = u64::from(value.limbs[index]) + carry;
        value.limbs[index] = (sum & 0xFFFF_FFFF) as u32;
        carry = sum >> 32;
        index += 1;
    }
    Ok(())
}

/// Divide by a small value in place, answering the remainder.
fn divide_small(value: &mut Number, divisor: u32) -> u32 {
    // Guarded here rather than at every call: a division by zero has no value,
    // and a module image carries no panic path to take.
    if divisor == 0 {
        return 0;
    }
    let mut remainder = 0u32;
    let mut index = value.length;
    while index > 0 {
        index -= 1;
        let (quotient, next) = divide_step(remainder, value.limbs[index], divisor);
        value.limbs[index] = quotient;
        remainder = next;
    }
    value.trim();
    remainder
}

/// Divide `(remainder, limb)` — a 64-bit value whose high half is below the
/// divisor — by `divisor`, answering the quotient limb and the new remainder.
///
/// The division is done a bit at a time on 32-bit values. The smallest target
/// has no 64-bit division instruction and no library to call for one, and a
/// module image may not depend on a symbol the loader will not resolve.
fn divide_step(remainder: u32, limb: u32, divisor: u32) -> (u32, u32) {
    let mut rest = remainder;
    let mut quotient = 0u32;
    let mut bit = 32u32;
    while bit > 0 {
        bit -= 1;
        let carry = (limb >> bit) & 1;
        let overflowed = rest >> 31 == 1;
        let doubled = (rest << 1) | carry;
        if overflowed || doubled >= divisor {
            // When the doubling overflowed, the true value is above the
            // divisor whatever the low bits say.
            rest = doubled.wrapping_sub(divisor);
            quotient |= 1 << bit;
        } else {
            rest = doubled;
        }
    }
    (quotient, rest)
}

/// Compare magnitudes, ignoring sign.
fn compare_magnitude(left: &Number, right: &Number) -> core::cmp::Ordering {
    if left.length != right.length {
        return left.length.cmp(&right.length);
    }
    let mut index = left.length;
    while index > 0 {
        index -= 1;
        if left.limbs[index] != right.limbs[index] {
            return left.limbs[index].cmp(&right.limbs[index]);
        }
    }
    core::cmp::Ordering::Equal
}

/// Compare two values, sign included.
pub fn compare(left: &Number, right: &Number) -> core::cmp::Ordering {
    match (left.negative, right.negative) {
        (false, true) => core::cmp::Ordering::Greater,
        (true, false) => core::cmp::Ordering::Less,
        (false, false) => compare_magnitude(left, right),
        (true, true) => compare_magnitude(right, left),
    }
}

fn add_magnitude(left: &Number, right: &Number) -> Result<Number, BigIntError> {
    let mut result = Number::ZERO;
    let length = left.length.max(right.length);
    let mut carry = 0u64;
    let mut index = 0usize;
    while index < length || carry != 0 {
        let sum = u64::from(left.limb(index)) + u64::from(right.limb(index)) + carry;
        result.push((sum & 0xFFFF_FFFF) as u32)?;
        carry = sum >> 32;
        index += 1;
    }
    result.trim();
    Ok(result)
}

/// Subtract the smaller magnitude from the larger.
fn subtract_magnitude(left: &Number, right: &Number) -> Number {
    let mut result = Number::ZERO;
    let mut borrow = 0i64;
    let mut index = 0usize;
    while index < left.length {
        let difference = i64::from(left.limb(index)) - i64::from(right.limb(index)) - borrow;
        let (limb, next) = if difference < 0 {
            ((difference + (1i64 << 32)) as u32, 1)
        } else {
            (difference as u32, 0)
        };
        result.limbs[index] = limb;
        result.length = index + 1;
        borrow = next;
        index += 1;
    }
    result.trim();
    result
}

pub fn add(left: &Number, right: &Number) -> Result<Number, BigIntError> {
    if left.negative == right.negative {
        let mut result = add_magnitude(left, right)?;
        result.negative = left.negative;
        result.trim();
        return Ok(result);
    }
    match compare_magnitude(left, right) {
        core::cmp::Ordering::Equal => Ok(Number::ZERO),
        core::cmp::Ordering::Greater => {
            let mut result = subtract_magnitude(left, right);
            result.negative = left.negative;
            result.trim();
            Ok(result)
        }
        core::cmp::Ordering::Less => {
            let mut result = subtract_magnitude(right, left);
            result.negative = right.negative;
            result.trim();
            Ok(result)
        }
    }
}

pub fn negate(value: &Number) -> Number {
    let mut result = *value;
    if !result.is_zero() {
        result.negative = !result.negative;
    }
    result
}

pub fn subtract(left: &Number, right: &Number) -> Result<Number, BigIntError> {
    add(left, &negate(right))
}

pub fn multiply(left: &Number, right: &Number) -> Result<Number, BigIntError> {
    if left.is_zero() || right.is_zero() {
        return Ok(Number::ZERO);
    }
    if left.length + right.length > WORK_LIMBS {
        return Err(BigIntError::TooLarge);
    }
    let mut result = Number::ZERO;
    result.length = left.length + right.length;
    let mut index = 0usize;
    while index < left.length {
        let mut carry = 0u64;
        let mut other = 0usize;
        while other < right.length {
            let at = index + other;
            let product = u64::from(left.limbs[index]) * u64::from(right.limbs[other])
                + u64::from(result.limbs[at])
                + carry;
            result.limbs[at] = (product & 0xFFFF_FFFF) as u32;
            carry = product >> 32;
            other += 1;
        }
        let mut at = index + right.length;
        while carry != 0 {
            if at >= WORK_LIMBS {
                return Err(BigIntError::TooLarge);
            }
            let sum = u64::from(result.limbs[at]) + carry;
            result.limbs[at] = (sum & 0xFFFF_FFFF) as u32;
            carry = sum >> 32;
            at += 1;
        }
        index += 1;
    }
    result.negative = left.negative != right.negative;
    result.trim();
    Ok(result)
}

/// Divide, answering the quotient and the remainder.
///
/// The quotient truncates towards zero and the remainder takes the sign of the
/// dividend, which is what the specification says of `/` and `%`.
pub fn divide(left: &Number, right: &Number) -> Result<(Number, Number), BigIntError> {
    if right.is_zero() {
        return Err(BigIntError::DivisionByZero);
    }
    if compare_magnitude(left, right) == core::cmp::Ordering::Less {
        let mut remainder = *left;
        remainder.negative = left.negative && !left.is_zero();
        return Ok((Number::ZERO, remainder));
    }
    if right.length == 1 && right.limbs[0] != 0 {
        let mut quotient = *left;
        quotient.negative = false;
        let remainder_limb = divide_small(&mut quotient, right.limbs[0]);
        quotient.negative = (left.negative != right.negative) && !quotient.is_zero();
        let mut remainder = Number::ZERO;
        if remainder_limb != 0 {
            remainder.limbs[0] = remainder_limb;
            remainder.length = 1;
            remainder.negative = left.negative;
        }
        return Ok((quotient, remainder));
    }

    // Long division, one bit at a time: simple, exact, and fast enough for the
    // sizes an isolate this size works with.
    let mut quotient = Number::ZERO;
    quotient.length = left.length;
    let mut remainder = Number::ZERO;
    let mut bit = left.length * 32;
    while bit > 0 {
        bit -= 1;
        shift_left_one(&mut remainder)?;
        let limb = bit / 32;
        let offset = bit % 32;
        if (left.limb(limb) >> offset) & 1 == 1 {
            remainder.limbs[0] |= 1;
            if remainder.length == 0 {
                remainder.length = 1;
            }
        }
        let mut magnitude = *right;
        magnitude.negative = false;
        if compare_magnitude(&remainder, &magnitude) != core::cmp::Ordering::Less {
            remainder = subtract_magnitude(&remainder, &magnitude);
            quotient.limbs[limb] |= 1 << offset;
        }
    }
    quotient.negative = (left.negative != right.negative) && !quotient.is_zero();
    quotient.trim();
    remainder.negative = left.negative && !remainder.is_zero();
    remainder.trim();
    Ok((quotient, remainder))
}

fn shift_left_one(value: &mut Number) -> Result<(), BigIntError> {
    let mut carry = 0u32;
    let mut index = 0usize;
    while index < value.length {
        let limb = value.limbs[index];
        value.limbs[index] = (limb << 1) | carry;
        carry = limb >> 31;
        index += 1;
    }
    if carry != 0 {
        value.push(carry)?;
    }
    Ok(())
}

pub fn power(base: &Number, exponent: &Number) -> Result<Number, BigIntError> {
    if exponent.negative {
        return Err(BigIntError::NegativeExponent);
    }
    let Some(count) = exponent.to_i64() else {
        return Err(BigIntError::TooLarge);
    };
    let mut result = Number::ZERO;
    result.limbs[0] = 1;
    result.length = 1;
    let mut turn = 0i64;
    while turn < count {
        result = multiply(&result, base)?;
        turn += 1;
    }
    Ok(result)
}

/// Shift left by `count` bits, which is multiplication by a power of two.
pub fn shift_left(value: &Number, count: u64) -> Result<Number, BigIntError> {
    let mut result = *value;
    let mut turn = 0u64;
    while turn < count {
        shift_left_one(&mut result)?;
        turn += 1;
    }
    result.trim();
    Ok(result)
}

/// Shift right by `count` bits, rounding towards negative infinity, which is
/// what an arithmetic shift of a two's-complement value does.
pub fn shift_right(value: &Number, count: u64) -> Result<Number, BigIntError> {
    let mut two = Number::ZERO;
    two.limbs[0] = 2;
    two.length = 1;
    let mut result = *value;
    let mut turn = 0u64;
    while turn < count && !result.is_zero() {
        let (quotient, remainder) = divide(&result, &two)?;
        result = quotient;
        if value.negative && !remainder.is_zero() {
            // Truncation went the wrong way for a negative value.
            let mut one = Number::ZERO;
            one.limbs[0] = 1;
            one.length = 1;
            result = subtract(&result, &one)?;
        }
        turn += 1;
    }
    if value.negative && result.is_zero() {
        let mut minus_one = Number::ZERO;
        minus_one.limbs[0] = 1;
        minus_one.length = 1;
        minus_one.negative = true;
        return Ok(minus_one);
    }
    Ok(result)
}

/// The digits of a value in `radix`, written into `out` as code units.
pub fn text(value: &Number, radix: u32, out: &mut [u16]) -> usize {
    let mut digits = [0u8; MAX_LIMBS * 10 + 2];
    let mut count = 0usize;
    let mut magnitude = *value;
    magnitude.negative = false;
    if magnitude.is_zero() {
        digits[0] = b'0';
        count = 1;
    }
    while !magnitude.is_zero() && count < digits.len() {
        let digit = divide_small(&mut magnitude, radix) as u8;
        digits[count] = if digit < 10 {
            b'0' + digit
        } else {
            b'a' + digit - 10
        };
        count += 1;
    }
    let mut written = 0usize;
    if value.negative {
        if let Some(slot) = out.get_mut(written) {
            *slot = u16::from(b'-');
            written += 1;
        }
    }
    while count > 0 {
        count -= 1;
        if let Some(slot) = out.get_mut(written) {
            *slot = u16::from(digits[count]);
            written += 1;
        }
    }
    written
}

/// The bitwise operations, which work on two's-complement values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Bitwise {
    And,
    Or,
    Xor,
}

/// Apply a bitwise operation, treating each value as an infinite
/// two's-complement sequence: a negative value is its magnitude complemented,
/// with the sign extended past the limbs it needs.
pub fn bitwise(operation: Bitwise, left: &Number, right: &Number) -> Result<Number, BigIntError> {
    let length = left.length.max(right.length) + 1;
    if length > WORK_LIMBS {
        return Err(BigIntError::TooLarge);
    }
    let mut result = Number::ZERO;
    result.length = length;
    let mut left_borrow = 1u64;
    let mut right_borrow = 1u64;
    let mut carry = 1u64;
    let negative = match operation {
        Bitwise::And => left.negative && right.negative,
        Bitwise::Or => left.negative || right.negative,
        Bitwise::Xor => left.negative != right.negative,
    };
    let mut index = 0usize;
    while index < length {
        let left_limb = twos_complement_limb(left, index, &mut left_borrow);
        let right_limb = twos_complement_limb(right, index, &mut right_borrow);
        let combined = match operation {
            Bitwise::And => left_limb & right_limb,
            Bitwise::Or => left_limb | right_limb,
            Bitwise::Xor => left_limb ^ right_limb,
        };
        if negative {
            // Bring the result back to sign and magnitude.
            let sum = u64::from(!combined) + carry;
            result.limbs[index] = (sum & 0xFFFF_FFFF) as u32;
            carry = sum >> 32;
        } else {
            result.limbs[index] = combined;
        }
        index += 1;
    }
    result.negative = negative;
    result.trim();
    Ok(result)
}

/// One limb of a value's two's-complement form.
fn twos_complement_limb(value: &Number, index: usize, borrow: &mut u64) -> u32 {
    let limb = value.limb(index);
    if !value.negative {
        return limb;
    }
    let sum = u64::from(!limb) + *borrow;
    *borrow = sum >> 32;
    (sum & 0xFFFF_FFFF) as u32
}
