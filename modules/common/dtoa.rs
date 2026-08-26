//! The shortest decimal digits that identify a double.
//!
//! `Number::toString` needs the shortest decimal string that reads back as the
//! same double, which is a property of the exact binary value rather than of an
//! approximation. The generator here is exact: it works in fixed-size integers
//! wide enough to hold the scaled value, so it needs no floating-point
//! arithmetic and gives the same digits on every target.
//!
//! The method is the standard one: represent the value and its two neighbours'
//! midpoints as an exact rational, scale until the first digit is about to be
//! produced, then emit digits until the remainder identifies the value
//! uniquely.

/// Limbs in a working integer. The largest value in play is the scaled
/// denominator for a subnormal, which needs a little over 1100 bits.
const LIMBS: usize = 40;

/// A fixed-size unsigned integer in 32-bit limbs, least significant first.
#[derive(Clone, Copy)]
struct Integer {
    limbs: [u32; LIMBS],
    /// Number of limbs in use.
    length: usize,
}

impl Integer {
    const fn zero() -> Self {
        Self {
            limbs: [0; LIMBS],
            length: 0,
        }
    }

    fn from_u64(value: u64) -> Self {
        let mut integer = Self::zero();
        if value == 0 {
            return integer;
        }
        integer.limbs[0] = (value & 0xFFFF_FFFF) as u32;
        integer.limbs[1] = (value >> 32) as u32;
        integer.length = if integer.limbs[1] == 0 { 1 } else { 2 };
        integer
    }

    const fn is_zero(&self) -> bool {
        self.length == 0
    }

    fn trim(&mut self) {
        while self.length > 0 && self.limbs[self.length - 1] == 0 {
            self.length -= 1;
        }
    }

    fn shift_left(&mut self, bits: u32) {
        if self.is_zero() || bits == 0 {
            return;
        }
        let limb_shift = (bits / 32) as usize;
        let bit_shift = bits % 32;
        if limb_shift > 0 {
            let mut index = self.length;
            while index > 0 {
                index -= 1;
                if index + limb_shift < LIMBS {
                    self.limbs[index + limb_shift] = self.limbs[index];
                }
            }
            let mut clear = 0usize;
            while clear < limb_shift && clear < LIMBS {
                self.limbs[clear] = 0;
                clear += 1;
            }
            self.length = (self.length + limb_shift).min(LIMBS);
        }
        if bit_shift > 0 {
            let mut carry = 0u32;
            let mut index = 0usize;
            while index < self.length {
                let value = (u64::from(self.limbs[index]) << bit_shift) | u64::from(carry);
                self.limbs[index] = (value & 0xFFFF_FFFF) as u32;
                carry = (value >> 32) as u32;
                index += 1;
            }
            if carry != 0 && self.length < LIMBS {
                self.limbs[self.length] = carry;
                self.length += 1;
            }
        }
        self.trim();
    }

    fn multiply_small(&mut self, factor: u32) {
        if self.is_zero() || factor == 1 {
            return;
        }
        let mut carry = 0u64;
        let mut index = 0usize;
        while index < self.length {
            let value = u64::from(self.limbs[index]) * u64::from(factor) + carry;
            self.limbs[index] = (value & 0xFFFF_FFFF) as u32;
            carry = value >> 32;
            index += 1;
        }
        while carry != 0 && self.length < LIMBS {
            self.limbs[self.length] = (carry & 0xFFFF_FFFF) as u32;
            carry >>= 32;
            self.length += 1;
        }
        self.trim();
    }

    /// Multiply by ten to the power `places`.
    fn multiply_pow10(&mut self, places: u32) {
        let mut remaining = places;
        while remaining >= 9 {
            self.multiply_small(1_000_000_000);
            remaining -= 9;
        }
        if remaining > 0 {
            let mut factor = 1u32;
            let mut index = 0;
            while index < remaining {
                factor *= 10;
                index += 1;
            }
            self.multiply_small(factor);
        }
    }

    fn compare(&self, other: &Self) -> core::cmp::Ordering {
        use core::cmp::Ordering;
        if self.length != other.length {
            return if self.length < other.length {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        let mut index = self.length;
        while index > 0 {
            index -= 1;
            if self.limbs[index] != other.limbs[index] {
                return if self.limbs[index] < other.limbs[index] {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
        }
        Ordering::Equal
    }

    fn add_assign(&mut self, other: &Self) {
        let mut carry = 0u64;
        let length = self.length.max(other.length);
        let mut index = 0usize;
        while index < length || carry != 0 {
            if index >= LIMBS {
                break;
            }
            let left = if index < self.length {
                u64::from(self.limbs[index])
            } else {
                0
            };
            let right = if index < other.length {
                u64::from(other.limbs[index])
            } else {
                0
            };
            let sum = left + right + carry;
            self.limbs[index] = (sum & 0xFFFF_FFFF) as u32;
            carry = sum >> 32;
            index += 1;
        }
        self.length = index.max(self.length).min(LIMBS);
        self.trim();
    }

    /// Subtract `other`, which must not be larger.
    fn subtract_assign(&mut self, other: &Self) {
        let mut borrow = 0i64;
        let mut index = 0usize;
        while index < self.length {
            let right = if index < other.length {
                i64::from(other.limbs[index])
            } else {
                0
            };
            let mut difference = i64::from(self.limbs[index]) - right - borrow;
            if difference < 0 {
                difference += 1 << 32;
                borrow = 1;
            } else {
                borrow = 0;
            }
            self.limbs[index] = (difference & 0xFFFF_FFFF) as u32;
            index += 1;
        }
        self.trim();
    }
}

/// The shortest digits that identify a double, with the decimal exponent of the
/// first digit: the value is `0.d1d2... * 10^exponent`.
#[derive(Clone, Copy, Debug)]
pub struct Shortest {
    pub digits: [u8; 24],
    pub length: usize,
    pub exponent: i32,
}

/// Produce the shortest digits for a finite, non-zero, positive double.
pub fn shortest(value: f64) -> Shortest {
    let bits = value.to_bits();
    let biased = ((bits >> 52) & 0x7FF) as i32;
    let fraction = bits & ((1u64 << 52) - 1);
    let (significand, exponent) = if biased == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1u64 << 52), biased - 1075)
    };

    // The value is significand * 2^exponent. Represent it as numerator over
    // denominator, with the distances to the neighbouring doubles as `above`
    // and `below`.
    let mut numerator = Integer::from_u64(significand);
    let mut denominator = Integer::from_u64(1);
    let mut above = Integer::from_u64(1);
    let mut below = Integer::from_u64(1);

    // A power-of-two significand is closer to its lower neighbour, so the two
    // half-distances differ.
    let boundary = fraction == 0 && biased > 1;
    // An even significand rounds to itself at a midpoint, so a decimal exactly
    // on the boundary still identifies this value and the interval is closed.
    let closed = significand % 2 == 0;

    if exponent >= 0 {
        numerator.shift_left(exponent as u32 + 1);
        denominator.shift_left(1);
        above.shift_left(exponent as u32);
        below.shift_left(exponent as u32);
        if boundary {
            numerator.shift_left(1);
            denominator.shift_left(1);
            above.shift_left(1);
        }
    } else {
        numerator.shift_left(1);
        denominator.shift_left((-exponent) as u32 + 1);
        if boundary {
            numerator.shift_left(1);
            denominator.shift_left(1);
            above.shift_left(1);
        }
    }

    // Scale so that the first digit is the first one produced: find the
    // decimal exponent by trial, starting from an estimate.
    let mut decimal_exponent = estimate_exponent(significand, exponent);
    if decimal_exponent > 0 {
        denominator.multiply_pow10(decimal_exponent as u32);
    } else if decimal_exponent < 0 {
        let places = (-decimal_exponent) as u32;
        numerator.multiply_pow10(places);
        above.multiply_pow10(places);
        below.multiply_pow10(places);
    }

    // Fix the estimate: the value must be below one and at least one tenth.
    let mut high = numerator;
    high.add_assign(&above);
    if is_above(&high, &denominator, closed) {
        denominator.multiply_small(10);
        decimal_exponent += 1;
    } else {
        let mut scaled = numerator;
        scaled.multiply_small(10);
        let mut scaled_high = scaled;
        let mut above_scaled = above;
        above_scaled.multiply_small(10);
        scaled_high.add_assign(&above_scaled);
        if !is_above(&scaled_high, &denominator, closed) {
            numerator = scaled;
            above = above_scaled;
            below.multiply_small(10);
            decimal_exponent -= 1;
        }
    }

    // Generate digits until the remaining interval identifies the value.
    let mut result = Shortest {
        digits: [0; 24],
        length: 0,
        exponent: decimal_exponent,
    };
    loop {
        numerator.multiply_small(10);
        above.multiply_small(10);
        below.multiply_small(10);

        let mut digit = 0u8;
        while numerator.compare(&denominator) != core::cmp::Ordering::Less && digit < 10 {
            numerator.subtract_assign(&denominator);
            digit += 1;
        }

        let low = is_below(&numerator, &below, closed);
        let mut sum = numerator;
        sum.add_assign(&above);
        let high = is_above(&sum, &denominator, closed);

        if result.length < result.digits.len() {
            result.digits[result.length] = digit;
            result.length += 1;
        }

        if low || high || result.length >= result.digits.len() {
            // Round the last digit towards the closer boundary.
            let round_up = if low && high {
                let mut doubled = numerator;
                doubled.multiply_small(2);
                match doubled.compare(&denominator) {
                    core::cmp::Ordering::Greater => true,
                    core::cmp::Ordering::Less => false,
                    // Exactly between two admissible strings: the
                    // specification takes the even one.
                    core::cmp::Ordering::Equal => digit % 2 == 1,
                }
            } else {
                high
            };
            if round_up {
                let mut index = result.length;
                loop {
                    if index == 0 {
                        // Every digit was a nine: the value carries into a new
                        // leading digit.
                        result.digits[0] = 1;
                        result.length = 1;
                        result.exponent += 1;
                        break;
                    }
                    index -= 1;
                    if result.digits[index] == 9 {
                        result.length -= 1;
                        continue;
                    }
                    result.digits[index] += 1;
                    result.length = index + 1;
                    break;
                }
            }
            return result;
        }
    }
}

/// Whether the remaining value has passed the upper boundary. The comparison
/// is inclusive when the significand is even, because a decimal exactly on the
/// boundary rounds back to this value.
fn is_above(value: &Integer, limit: &Integer, closed: bool) -> bool {
    match value.compare(limit) {
        core::cmp::Ordering::Greater => true,
        core::cmp::Ordering::Equal => closed,
        core::cmp::Ordering::Less => false,
    }
}

/// Whether the remaining value has fallen below the lower boundary.
fn is_below(value: &Integer, limit: &Integer, closed: bool) -> bool {
    match value.compare(limit) {
        core::cmp::Ordering::Less => true,
        core::cmp::Ordering::Equal => closed,
        core::cmp::Ordering::Greater => false,
    }
}

/// An estimate of the decimal exponent, refined by the caller.
///
/// The estimate uses only integer arithmetic: the base-two exponent of the
/// value scaled by a fixed-point approximation of the base-ten logarithm of
/// two.
fn estimate_exponent(significand: u64, exponent: i32) -> i32 {
    let bits = 64 - significand.leading_zeros() as i32;
    let binary_exponent = exponent + bits;
    // log10(2) as a fraction: 78913 / 262144 is within a part in ten million.
    let scaled = i64::from(binary_exponent) * 78_913;
    let estimate = (scaled >> 18) as i32;
    estimate + i32::from(scaled & 0x3FFFF != 0 && binary_exponent > 0)
}

/// The text of `value` with exactly `digits` digits after the point, which is
/// what `Number.prototype.toFixed` produces.
///
/// The rounding is done on the decimal digits the shortest representation
/// gives, extended with zeros where it is shorter than asked for, so the result
/// depends on the value alone and not on any host formatting.
pub fn fixed(value: f64, digits: u32, out: &mut [u16]) -> usize {
    if value.is_nan() {
        return put(b"NaN", out);
    }
    if value.is_infinite() {
        return put(
            if value < 0.0 {
                b"-Infinity"
            } else {
                b"Infinity"
            },
            out,
        );
    }
    // Negative zero formats as `0.00`, which is what the specification says.
    let negative = value < 0.0;
    let magnitude = if value < 0.0 { -value } else { value };
    if magnitude >= 1e21 {
        // Beyond this the specification hands the value to the ordinary
        // conversion, which is the one place `toFixed` is not fixed-point.
        return shortest_text(value, out);
    }

    // The digits of the magnitude, as a decimal string with its point position.
    let (mut decimal, mut point) = if magnitude == 0.0 {
        ([0u8; 40], 1i32)
    } else {
        let shortest = shortest(magnitude);
        let mut buffer = [0u8; 40];
        let mut index = 0usize;
        while index < shortest.length && index < buffer.len() {
            buffer[index] = shortest.digits[index];
            index += 1;
        }
        (buffer, shortest.exponent)
    };
    let mut length = if magnitude == 0.0 {
        decimal[0] = 0;
        1usize
    } else {
        let shortest = shortest(magnitude);
        shortest.length
    };

    // Round at the position the caller asked for, carrying where a digit
    // overflows, which may add a digit in front.
    let keep = point + digits as i32;
    if keep < 0 {
        decimal = [0u8; 40];
        decimal[0] = 0;
        length = 1;
        point = 1;
    } else if (keep as usize) < length {
        let at = keep as usize;
        let round_up = decimal[at] >= 5;
        length = at;
        if round_up {
            let mut index = length;
            loop {
                if index == 0 {
                    // Every digit carried: the number gains one in front.
                    let mut shifted = [0u8; 40];
                    shifted[0] = 1;
                    let mut copy = 0usize;
                    while copy < length && copy + 1 < shifted.len() {
                        shifted[copy + 1] = decimal[copy];
                        copy += 1;
                    }
                    decimal = shifted;
                    length += 1;
                    point += 1;
                    break;
                }
                index -= 1;
                if decimal[index] == 9 {
                    decimal[index] = 0;
                    continue;
                }
                decimal[index] += 1;
                break;
            }
        }
        if length == 0 {
            decimal[0] = 0;
            length = 1;
            point = 1;
        }
    }

    let mut written = 0usize;
    if negative {
        written += put_at(b"-", out, written);
    }
    // The integer part.
    if point <= 0 {
        written += put_at(b"0", out, written);
    } else {
        let mut index = 0i32;
        while index < point {
            let digit = if (index as usize) < length {
                decimal[index as usize]
            } else {
                0
            };
            written += put_digit(digit, out, written);
            index += 1;
        }
    }
    if digits == 0 {
        return written;
    }
    written += put_at(b".", out, written);
    let mut place = 0u32;
    while place < digits {
        let index = point + place as i32;
        let digit = if index < 0 || (index as usize) >= length {
            0
        } else {
            decimal[index as usize]
        };
        written += put_digit(digit, out, written);
        place += 1;
    }
    written
}

/// The text of a Number, as `ToString` defines it: the shortest digits that
/// identify the value, in fixed or exponential form as its exponent decides.
pub fn shortest_text(value: f64, out: &mut [u16]) -> usize {
    if value.is_nan() {
        return put_text(out, b"NaN");
    }
    if value == 0.0 {
        return put_text(out, b"0");
    }
    let negative = value.to_bits() >> 63 == 1;
    let magnitude = f64::from_bits(value.to_bits() & !(1 << 63));
    if magnitude.is_infinite() {
        return put_text(out, if negative { b"-Infinity" } else { b"Infinity" });
    }

    let shortest = shortest(magnitude);
    let digits = shortest.digits.get(..shortest.length).unwrap_or(&[]);
    let k = shortest.length as i32;
    let n = shortest.exponent;

    let mut written = 0usize;
    if negative {
        written += write_ascii(&mut out[written..], b"-");
    }

    if k <= n && n <= 21 {
        // Digits followed by zeroes.
        written += write_digits(&mut out[written..], digits);
        written += write_repeat(&mut out[written..], b'0', (n - k) as usize);
        return written;
    }
    if 0 < n && n <= 21 {
        // A decimal point inside the digits.
        written += write_digits(&mut out[written..], digits.get(..n as usize).unwrap_or(&[]));
        written += write_ascii(&mut out[written..], b".");
        written += write_digits(&mut out[written..], digits.get(n as usize..).unwrap_or(&[]));
        return written;
    }
    if -6 < n && n <= 0 {
        // A leading zero, a point, then the digits after some zeroes.
        written += write_ascii(&mut out[written..], b"0.");
        written += write_repeat(&mut out[written..], b'0', (-n) as usize);
        written += write_digits(&mut out[written..], digits);
        return written;
    }

    // Exponential form.
    written += write_digits(&mut out[written..], digits.get(..1).unwrap_or(&[]));
    if k > 1 {
        written += write_ascii(&mut out[written..], b".");
        written += write_digits(&mut out[written..], digits.get(1..).unwrap_or(&[]));
    }
    written += write_ascii(&mut out[written..], if n > 0 { b"e+" } else { b"e-" });
    let exponent = (n - 1).unsigned_abs();
    written += write_number(&mut out[written..], exponent);
    written
}

fn write_number(out: &mut [u16], value: u32) -> usize {
    let mut digits = [0u8; 10];
    let mut count = 0usize;
    let mut remaining = value;
    loop {
        digits[count] = u8::try_from(remaining % 10).unwrap_or(0);
        count += 1;
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    let mut written = 0usize;
    while written < count {
        if let Some(slot) = out.get_mut(written) {
            *slot = u16::from(b'0' + digits[count - 1 - written]);
            written += 1;
        } else {
            break;
        }
    }
    written
}

/// Write ASCII text into a unit buffer.
fn put_text(out: &mut [u16], text: &[u8]) -> usize {
    put(text, out)
}

fn write_ascii(out: &mut [u16], text: &[u8]) -> usize {
    let mut written = 0usize;
    for &byte in text {
        if let Some(slot) = out.get_mut(written) {
            *slot = u16::from(byte);
            written += 1;
        }
    }
    written
}

fn write_digits(out: &mut [u16], digits: &[u8]) -> usize {
    let mut written = 0usize;
    for &digit in digits {
        if let Some(slot) = out.get_mut(written) {
            *slot = u16::from(b'0' + digit);
            written += 1;
        }
    }
    written
}

fn write_repeat(out: &mut [u16], byte: u8, count: usize) -> usize {
    let mut written = 0usize;
    while written < count {
        if let Some(slot) = out.get_mut(written) {
            *slot = u16::from(byte);
            written += 1;
        } else {
            break;
        }
    }
    written
}

fn put(text: &[u8], out: &mut [u16]) -> usize {
    put_at(text, out, 0)
}

fn put_at(text: &[u8], out: &mut [u16], at: usize) -> usize {
    let mut written = 0usize;
    for &byte in text {
        if let Some(slot) = out.get_mut(at + written) {
            *slot = u16::from(byte);
            written += 1;
        }
    }
    written
}

fn put_digit(digit: u8, out: &mut [u16], at: usize) -> usize {
    match out.get_mut(at) {
        Some(slot) => {
            *slot = u16::from(b'0' + digit.min(9));
            1
        }
        None => 0,
    }
}
