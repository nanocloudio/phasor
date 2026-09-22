//! Binary64 arithmetic in integer operations.
//!
//! ECMAScript arithmetic is IEEE-754 binary64 with round-to-nearest-even, which
//! is exactly specified: the same operands give the same result everywhere. On
//! targets whose instruction set provides doubles, the engine uses the ordinary
//! operators. On targets without them, and without a library to call, the
//! compiler emits calls to the routines below, which compute the same results
//! from integer operations alone.
//!
//! Everything here works in 64-bit integers: no wide-integer arithmetic, so no
//! further intrinsic is needed to satisfy these intrinsics.

const SIGN: u64 = 1 << 63;
const EXPONENT_MASK: u64 = 0x7FF << 52;
const MANTISSA_MASK: u64 = (1 << 52) - 1;
const HIDDEN: u64 = 1 << 52;
const QUIET_NAN: u64 = 0x7FF8_0000_0000_0000;

/// A decomposed double.
#[derive(Clone, Copy)]
struct Parts {
    negative: bool,
    /// Unbiased exponent of the significand's lowest bit for a normal value.
    exponent: i32,
    /// Significand including the hidden bit for a normal value.
    significand: u64,
    class: Class,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Class {
    Zero,
    Finite,
    Infinite,
    NotANumber,
}

fn decompose(bits: u64) -> Parts {
    let negative = bits & SIGN != 0;
    let biased = ((bits & EXPONENT_MASK) >> 52) as i32;
    let fraction = bits & MANTISSA_MASK;
    if biased == 0x7FF {
        return Parts {
            negative,
            exponent: 0,
            significand: fraction,
            class: if fraction == 0 {
                Class::Infinite
            } else {
                Class::NotANumber
            },
        };
    }
    if biased == 0 {
        if fraction == 0 {
            return Parts {
                negative,
                exponent: 0,
                significand: 0,
                class: Class::Zero,
            };
        }
        // Subnormal: no hidden bit, exponent fixed at the minimum.
        return Parts {
            negative,
            exponent: -1074,
            significand: fraction,
            class: Class::Finite,
        };
    }
    Parts {
        negative,
        exponent: biased - 1075,
        significand: fraction | HIDDEN,
        class: Class::Finite,
    }
}

/// Assemble a value from a sign, an unbiased exponent of the significand's
/// lowest bit, and a significand that may need normalising, rounding to nearest
/// with ties to even. `sticky` records discarded non-zero bits below the
/// significand.
fn assemble(negative: bool, mut exponent: i32, mut significand: u64, mut sticky: bool) -> u64 {
    if significand == 0 && !sticky {
        return if negative { SIGN } else { 0 };
    }

    // Normalise so the significand occupies exactly 54 bits, keeping one round
    // bit below the 53 the format holds.
    let mut width = 64 - significand.leading_zeros() as i32;
    while width < 54 {
        significand <<= 1;
        exponent -= 1;
        width += 1;
    }
    while width > 54 {
        sticky |= significand & 1 != 0;
        significand >>= 1;
        exponent += 1;
        width -= 1;
    }

    // Subnormal results lose further bits.
    while exponent < -1075 {
        sticky |= significand & 1 != 0;
        significand >>= 1;
        exponent += 1;
    }

    let round_bit = significand & 1;
    let mut value = significand >> 1;
    exponent += 1;
    if round_bit == 1 && (sticky || value & 1 == 1) {
        value += 1;
        if value >> 53 != 0 {
            value >>= 1;
            exponent += 1;
        }
    }

    if value == 0 {
        return if negative { SIGN } else { 0 };
    }

    let biased = exponent + 1075;
    if biased >= 0x7FF {
        return (if negative { SIGN } else { 0 }) | EXPONENT_MASK;
    }
    // A significand without its hidden bit is subnormal, and a subnormal is
    // encoded with a zero exponent field.
    let bits = if value >> 52 == 0 {
        value
    } else {
        (value & MANTISSA_MASK) | ((biased as u64) << 52)
    };
    bits | if negative { SIGN } else { 0 }
}

/// Add two doubles.
pub fn add(left: f64, right: f64) -> f64 {
    f64::from_bits(add_bits(left.to_bits(), right.to_bits()))
}

/// Subtract `right` from `left`.
pub fn sub(left: f64, right: f64) -> f64 {
    f64::from_bits(add_bits(left.to_bits(), right.to_bits() ^ SIGN))
}

fn add_bits(left: u64, right: u64) -> u64 {
    let a = decompose(left);
    let b = decompose(right);

    if a.class == Class::NotANumber || b.class == Class::NotANumber {
        return QUIET_NAN;
    }
    if a.class == Class::Infinite || b.class == Class::Infinite {
        if a.class == Class::Infinite && b.class == Class::Infinite {
            if a.negative != b.negative {
                return QUIET_NAN;
            }
            return left;
        }
        return if a.class == Class::Infinite {
            left
        } else {
            right
        };
    }
    if a.class == Class::Zero && b.class == Class::Zero {
        // Zero plus zero keeps the sign only when both agree.
        return if a.negative == b.negative { left } else { 0 };
    }
    if a.class == Class::Zero {
        return right;
    }
    if b.class == Class::Zero {
        return left;
    }

    // Work three bits below the significand: a guard, a round, and a sticky
    // position. Three extra bits plus a sticky flag are what round-to-nearest
    // needs for an exact addition or subtraction.
    let (high, low) = if a.exponent > b.exponent
        || (a.exponent == b.exponent && a.significand >= b.significand)
    {
        (a, b)
    } else {
        (b, a)
    };

    let mut significand = high.significand << 3;
    let exponent = high.exponent - 3;
    let shift = high.exponent - low.exponent;
    let (aligned, lost) = if shift >= 64 {
        (0u64, low.significand != 0)
    } else {
        let extended = low.significand << 3;
        let dropped = if shift == 0 {
            0
        } else {
            extended & ((1u64 << shift) - 1)
        };
        (extended >> shift, dropped != 0)
    };

    if high.negative == low.negative {
        let (sum, carry) = significand.overflowing_add(aligned);
        if carry {
            let recovered = (sum >> 1) | (1 << 63);
            return assemble(high.negative, exponent + 1, recovered, lost || sum & 1 != 0);
        }
        return assemble(high.negative, exponent, sum, lost);
    }

    // Opposite signs. The bits lost when aligning belong to the value being
    // subtracted, so the exact result is one unit lower with a residue above
    // zero, which is what the sticky flag records.
    let mut sticky = lost;
    let mut subtrahend = aligned;
    if lost {
        if subtrahend == u64::MAX {
            return assemble(high.negative, exponent, significand, true);
        }
        subtrahend += 1;
    }
    if significand < subtrahend {
        // Only possible when the operands were equal before the borrow.
        let difference = subtrahend - significand;
        significand = difference;
        sticky = true;
        return assemble(!high.negative, exponent, significand, sticky);
    }
    let difference = significand - subtrahend;
    if difference == 0 && !sticky {
        return 0;
    }
    assemble(high.negative, exponent, difference, sticky)
}

/// Multiply two doubles.
pub fn mul(left: f64, right: f64) -> f64 {
    f64::from_bits(mul_bits(left.to_bits(), right.to_bits()))
}

fn mul_bits(left: u64, right: u64) -> u64 {
    let a = decompose(left);
    let b = decompose(right);
    let negative = a.negative != b.negative;

    if a.class == Class::NotANumber || b.class == Class::NotANumber {
        return QUIET_NAN;
    }
    if a.class == Class::Infinite || b.class == Class::Infinite {
        if a.class == Class::Zero || b.class == Class::Zero {
            return QUIET_NAN;
        }
        return EXPONENT_MASK | if negative { SIGN } else { 0 };
    }
    if a.class == Class::Zero || b.class == Class::Zero {
        return if negative { SIGN } else { 0 };
    }

    let (high, low) = wide_multiply(a.significand, b.significand);
    let exponent = a.exponent + b.exponent;
    fold(negative, exponent, high, low)
}

/// The 128-bit product of two 64-bit values, as a high and low half.
fn wide_multiply(left: u64, right: u64) -> (u64, u64) {
    let (a_low, a_high) = (left & 0xFFFF_FFFF, left >> 32);
    let (b_low, b_high) = (right & 0xFFFF_FFFF, right >> 32);

    let low_low = a_low * b_low;
    let low_high = a_low * b_high;
    let high_low = a_high * b_low;
    let high_high = a_high * b_high;

    let (middle, carry) = low_high.overflowing_add(high_low);
    let mut high = high_high + (middle >> 32) + if carry { 1 << 32 } else { 0 };
    let (low, overflow) = low_low.overflowing_add(middle << 32);
    if overflow {
        high += 1;
    }
    (high, low)
}

/// Round a 128-bit significand at `exponent` into a double.
fn fold(negative: bool, exponent: i32, high: u64, low: u64) -> u64 {
    if high == 0 {
        return assemble(negative, exponent, low, false);
    }
    // Keep the top 54 bits of the pair and fold the rest into a sticky bit.
    let width = 128 - high.leading_zeros() as i32;
    let drop = width - 54;
    let (significand, sticky) = if drop < 64 {
        let window = (high << (64 - drop as u32)) | (low >> drop as u32);
        let lost = low & ((1u64 << drop as u32) - 1);
        (window, lost != 0)
    } else {
        let extra = (drop - 64) as u32;
        let window = high >> extra;
        let lost_high = if extra == 0 {
            0
        } else {
            high & ((1u64 << extra) - 1)
        };
        (window, lost_high != 0 || low != 0)
    };
    assemble(negative, exponent + drop, significand, sticky)
}

/// Divide `left` by `right`.
pub fn div(left: f64, right: f64) -> f64 {
    f64::from_bits(div_bits(left.to_bits(), right.to_bits()))
}

fn div_bits(left: u64, right: u64) -> u64 {
    let a = decompose(left);
    let b = decompose(right);
    let negative = a.negative != b.negative;

    if a.class == Class::NotANumber || b.class == Class::NotANumber {
        return QUIET_NAN;
    }
    if a.class == Class::Infinite {
        if b.class == Class::Infinite {
            return QUIET_NAN;
        }
        return EXPONENT_MASK | if negative { SIGN } else { 0 };
    }
    if b.class == Class::Infinite {
        return if negative { SIGN } else { 0 };
    }
    if a.class == Class::Zero {
        if b.class == Class::Zero {
            return QUIET_NAN;
        }
        return if negative { SIGN } else { 0 };
    }
    if b.class == Class::Zero {
        return EXPONENT_MASK | if negative { SIGN } else { 0 };
    }

    // Normalise both significands so each has its top bit at the same place;
    // a subnormal operand would otherwise need more quotient bits than the
    // loop produces.
    let (mut remainder, dividend_exponent) = normalise(a.significand, a.exponent);
    let (divisor, divisor_exponent) = normalise(b.significand, b.exponent);
    let mut quotient = 0u64;
    let mut produced = 0;
    while produced < 55 {
        quotient <<= 1;
        if remainder >= divisor {
            remainder -= divisor;
            quotient |= 1;
        }
        // Doubling the remainder cannot overflow: it stays below the divisor,
        // which has at most 53 significant bits.
        remainder <<= 1;
        produced += 1;
    }
    // The first bit produced has weight one, so the significand's lowest bit
    // sits 54 places below the difference of the operands' exponents.
    let exponent = dividend_exponent - divisor_exponent - 54;
    assemble(negative, exponent, quotient, remainder != 0)
}

/// Shift a significand up until its hidden-bit position is set, so both
/// operands of a division have the same width.
fn normalise(significand: u64, exponent: i32) -> (u64, i32) {
    if significand == 0 {
        return (0, exponent);
    }
    let mut significand = significand;
    let mut exponent = exponent;
    while significand < HIDDEN {
        significand <<= 1;
        exponent -= 1;
    }
    (significand, exponent)
}

/// The floating-point remainder of `left` divided by `right`, which is what
/// ECMAScript's `%` computes.
pub fn rem(left: f64, right: f64) -> f64 {
    f64::from_bits(rem_bits(left.to_bits(), right.to_bits()))
}

fn rem_bits(left: u64, right: u64) -> u64 {
    let a = decompose(left);
    let b = decompose(right);

    if a.class == Class::NotANumber
        || b.class == Class::NotANumber
        || a.class == Class::Infinite
        || b.class == Class::Zero
    {
        return QUIET_NAN;
    }
    if b.class == Class::Infinite || a.class == Class::Zero {
        return left;
    }

    // Exact repeated subtraction on the significands: the remainder of a
    // division by a power of two is exact, so no rounding happens here.
    let mut remainder = a.significand;
    let mut exponent = a.exponent;
    let divisor = b.significand;
    let divisor_exponent = b.exponent;

    if exponent < divisor_exponent {
        return left;
    }
    let mut steps = exponent - divisor_exponent;
    loop {
        // Bring the remainder up against the divisor without losing bits.
        while remainder < divisor && steps > 0 {
            let room = remainder.leading_zeros() as i32;
            let lift = if steps < room { steps } else { room };
            if lift == 0 {
                break;
            }
            remainder <<= lift;
            exponent -= lift;
            steps -= lift;
        }
        if remainder >= divisor {
            remainder = reduce(remainder, divisor);
            exponent = exponent.max(divisor_exponent);
        }
        if steps == 0 || remainder == 0 {
            break;
        }
    }
    assemble(a.negative, divisor_exponent.min(exponent), remainder, false)
}

/// The remainder of `value` divided by `divisor`, by shifting and subtracting.
///
/// A machine division of two 64-bit integers would need a library routine,
/// which is exactly what this file exists to avoid.
fn reduce(value: u64, divisor: u64) -> u64 {
    if divisor == 0 || value < divisor {
        return value;
    }
    let mut remainder = value;
    let mut shift = divisor.leading_zeros() - remainder.leading_zeros();
    let mut scaled = divisor << shift;
    loop {
        if remainder >= scaled {
            remainder -= scaled;
        }
        if shift == 0 {
            break;
        }
        shift -= 1;
        scaled >>= 1;
    }
    remainder
}

/// Compare two doubles: `-1`, `0`, `1`, or `2` when they are unordered.
pub fn compare(left: f64, right: f64) -> i32 {
    let a = left.to_bits();
    let b = right.to_bits();
    if is_nan(a) || is_nan(b) {
        return 2;
    }
    let a_zero = a & !SIGN == 0;
    let b_zero = b & !SIGN == 0;
    if a_zero && b_zero {
        return 0;
    }
    let ordered = |bits: u64| -> i64 {
        if bits & SIGN != 0 {
            // Negative values order downwards from the sign bit.
            -((bits & !SIGN) as i64)
        } else {
            bits as i64
        }
    };
    let (a, b) = (ordered(a), ordered(b));
    if a < b {
        -1
    } else if a > b {
        1
    } else {
        0
    }
}

const fn is_nan(bits: u64) -> bool {
    bits & EXPONENT_MASK == EXPONENT_MASK && bits & MANTISSA_MASK != 0
}

/// The double nearest a signed 64-bit integer.
pub fn from_i64(value: i64) -> f64 {
    if value == 0 {
        return 0.0;
    }
    let negative = value < 0;
    let magnitude = value.unsigned_abs();
    f64::from_bits(assemble(negative, 0, magnitude, false))
}

/// The double nearest an unsigned 64-bit integer.
pub fn from_u64(value: u64) -> f64 {
    if value == 0 {
        return 0.0;
    }
    f64::from_bits(assemble(false, 0, value, false))
}

/// Truncate a double towards zero into a signed 64-bit integer, saturating.
pub fn to_i64(value: f64) -> i64 {
    let parts = decompose(value.to_bits());
    match parts.class {
        Class::Zero | Class::NotANumber => 0,
        Class::Infinite => {
            if parts.negative {
                i64::MIN
            } else {
                i64::MAX
            }
        }
        Class::Finite => {
            let magnitude = shift_significand(parts.significand, parts.exponent);
            if parts.negative {
                if magnitude >= 1 << 63 {
                    i64::MIN
                } else {
                    -(magnitude as i64)
                }
            } else if magnitude > i64::MAX as u64 {
                i64::MAX
            } else {
                magnitude as i64
            }
        }
    }
}

/// The integer part of `significand * 2^exponent`, saturating.
fn shift_significand(significand: u64, exponent: i32) -> u64 {
    if exponent >= 0 {
        if exponent >= 64 || significand.leading_zeros() < exponent as u32 {
            return u64::MAX;
        }
        significand << exponent
    } else if -exponent >= 64 {
        0
    } else {
        significand >> (-exponent)
    }
}

// The routines the ARM procedure call standard names for targets that have no
// double-precision instructions. They are the same computations as above; the
// compiler calls them where another target would emit one instruction.
#[cfg(target_arch = "arm")]
mod arm {
    use super::{add, compare, div, from_i64, from_u64, mul, rem, sub, to_i64};

    // `__clzsi2` and `__aeabi_uldivmod` are not defined here. Both are the
    // SDK's, in `runtime/intrinsics.rs`, and an EABI symbol has one owner: a
    // second definition is a duplicate at link, not a fallback. The property
    // a division intrinsic owes a module that handles secrets — a fixed
    // sixty-four iterations whatever the operands, so its time says nothing
    // about the values — is the SDK's to hold, and Fluxor's harness holds it
    // to that in `tests/harness/tests/eabi_divide.rs`.

    #[no_mangle]
    pub extern "C" fn __aeabi_dadd(left: f64, right: f64) -> f64 {
        add(left, right)
    }

    #[no_mangle]
    pub extern "C" fn __aeabi_dsub(left: f64, right: f64) -> f64 {
        sub(left, right)
    }

    #[no_mangle]
    pub extern "C" fn __aeabi_dmul(left: f64, right: f64) -> f64 {
        mul(left, right)
    }

    #[no_mangle]
    pub extern "C" fn __aeabi_ddiv(left: f64, right: f64) -> f64 {
        div(left, right)
    }

    #[no_mangle]
    pub extern "C" fn fmod(left: f64, right: f64) -> f64 {
        rem(left, right)
    }

    #[no_mangle]
    pub extern "C" fn __aeabi_dcmpeq(left: f64, right: f64) -> i32 {
        i32::from(compare(left, right) == 0)
    }

    #[no_mangle]
    pub extern "C" fn __aeabi_dcmplt(left: f64, right: f64) -> i32 {
        i32::from(compare(left, right) == -1)
    }

    #[no_mangle]
    pub extern "C" fn __aeabi_dcmple(left: f64, right: f64) -> i32 {
        i32::from(matches!(compare(left, right), -1 | 0))
    }

    #[no_mangle]
    pub extern "C" fn __aeabi_dcmpge(left: f64, right: f64) -> i32 {
        i32::from(matches!(compare(left, right), 0 | 1))
    }

    #[no_mangle]
    pub extern "C" fn __aeabi_dcmpgt(left: f64, right: f64) -> i32 {
        i32::from(compare(left, right) == 1)
    }

    #[no_mangle]
    pub extern "C" fn __aeabi_dcmpun(left: f64, right: f64) -> i32 {
        i32::from(compare(left, right) == 2)
    }

    /// The ordered comparison libgcc names, which rustc also emits.
    #[no_mangle]
    pub extern "C" fn __ledf2(left: f64, right: f64) -> i32 {
        match compare(left, right) {
            -1 => -1,
            0 => 0,
            _ => 1,
        }
    }

    #[no_mangle]
    pub extern "C" fn __gedf2(left: f64, right: f64) -> i32 {
        match compare(left, right) {
            -1 => -1,
            0 => 0,
            1 => 1,
            _ => -1,
        }
    }

    #[no_mangle]
    pub extern "C" fn __eqdf2(left: f64, right: f64) -> i32 {
        i32::from(compare(left, right) != 0)
    }

    #[no_mangle]
    pub extern "C" fn __nedf2(left: f64, right: f64) -> i32 {
        i32::from(compare(left, right) != 0)
    }

    #[no_mangle]
    pub extern "C" fn __ltdf2(left: f64, right: f64) -> i32 {
        __ledf2(left, right)
    }

    #[no_mangle]
    pub extern "C" fn __gtdf2(left: f64, right: f64) -> i32 {
        __gedf2(left, right)
    }

    #[no_mangle]
    pub extern "C" fn __unorddf2(left: f64, right: f64) -> i32 {
        i32::from(compare(left, right) == 2)
    }

    #[no_mangle]
    pub extern "C" fn __aeabi_i2d(value: i32) -> f64 {
        from_i64(i64::from(value))
    }

    #[no_mangle]
    pub extern "C" fn __aeabi_ui2d(value: u32) -> f64 {
        from_u64(u64::from(value))
    }

    #[no_mangle]
    pub extern "C" fn __aeabi_l2d(value: i64) -> f64 {
        from_i64(value)
    }

    #[no_mangle]
    pub extern "C" fn __aeabi_ul2d(value: u64) -> f64 {
        from_u64(value)
    }

    #[no_mangle]
    pub extern "C" fn __aeabi_d2iz(value: f64) -> i32 {
        let truncated = to_i64(value);
        if truncated > i64::from(i32::MAX) {
            i32::MAX
        } else if truncated < i64::from(i32::MIN) {
            i32::MIN
        } else {
            truncated as i32
        }
    }

    #[no_mangle]
    pub extern "C" fn __aeabi_d2uiz(value: f64) -> u32 {
        let truncated = to_i64(value);
        if truncated < 0 {
            0
        } else if truncated > i64::from(u32::MAX) {
            u32::MAX
        } else {
            truncated as u32
        }
    }

    #[no_mangle]
    pub extern "C" fn __aeabi_d2lz(value: f64) -> i64 {
        to_i64(value)
    }

    #[no_mangle]
    pub extern "C" fn __aeabi_d2ulz(value: f64) -> u64 {
        let truncated = to_i64(value);
        if truncated < 0 {
            0
        } else {
            truncated as u64
        }
    }
}

/// The nearest `f32`, as its bits: the double's mantissa rounded to
/// twenty-three bits, ties to even, with overflow to infinity and the
/// subnormal range handled — in integer steps, since a target without a
/// double unit has no conversion to lean on either.
pub fn to_f32_bits(value: f64) -> u32 {
    let bits = value.to_bits();
    let sign = ((bits >> 63) as u32) << 31;
    let exponent = ((bits >> 52) & 0x7FF) as i32;
    let mantissa = bits & ((1u64 << 52) - 1);
    if exponent == 0x7FF {
        // Infinity, or a NaN kept quiet.
        let payload = if mantissa == 0 {
            0
        } else {
            0x40_0000 | (mantissa >> 29) as u32
        };
        return sign | 0x7F80_0000 | payload;
    }
    let narrowed = exponent - 1023 + 127;
    if narrowed >= 0xFF {
        return sign | 0x7F80_0000;
    }
    if narrowed <= 0 {
        if narrowed < -24 || exponent == 0 {
            return sign;
        }
        // A subnormal single: the whole significand shifted down into the
        // fraction, rounded once.
        let significand = mantissa | (1u64 << 52);
        let shift = (29 + 1 - narrowed) as u32;
        return sign | round_shift(significand, shift);
    }
    // A normal single: a carry out of the rounded fraction steps the
    // exponent, and at the top becomes infinity, as rounding should.
    sign | (((narrowed as u32) << 23) + round_shift(mantissa, 29))
}

/// `value >> shift`, rounded to nearest with ties to even.
fn round_shift(value: u64, shift: u32) -> u32 {
    if shift >= 64 {
        return 0;
    }
    let kept = value >> shift;
    let rest = value & ((1u64 << shift) - 1);
    let half = 1u64 << (shift - 1);
    let up = rest > half || (rest == half && kept & 1 == 1);
    (kept + u64::from(up)) as u32
}

/// The double an `f32`'s bits denote, exactly.
pub fn from_f32_bits(bits: u32) -> f64 {
    let sign = u64::from(bits >> 31) << 63;
    let exponent = (bits >> 23) & 0xFF;
    let mantissa = u64::from(bits & 0x7F_FFFF);
    if exponent == 0xFF {
        return f64::from_bits(sign | (0x7FFu64 << 52) | (mantissa << 29));
    }
    if exponent == 0 {
        if mantissa == 0 {
            return f64::from_bits(sign);
        }
        // A subnormal single is a normal double once its leading bit is
        // found.
        let mut significand = mantissa;
        let mut power = -126i64;
        while significand & (1u64 << 23) == 0 {
            significand <<= 1;
            power -= 1;
        }
        let fraction = significand & 0x7F_FFFF;
        let widened = (power + 1023) as u64;
        return f64::from_bits(sign | (widened << 52) | (fraction << 29));
    }
    let widened = u64::from(exponent) - 127 + 1023;
    f64::from_bits(sign | (widened << 52) | (mantissa << 29))
}
