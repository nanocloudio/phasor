//! Tagged values and the numeric part of ECMAScript coercion.
//!
//! A value is a tag and a payload word. Numbers carry their binary64 bits;
//! strings, symbols, BigInts, and objects carry a generation-checked heap
//! handle, never a pointer, so a value can be copied, stored in a frame, or
//! written into a record without anything to fix up.
//!
//! The coercions here are the ones that need no heap: the numeric conversions,
//! the numeric operators, and the comparisons over primitives. Conversions that
//! read a string's contents take the code units as a slice, so this module
//! stays independent of how a string is stored.

use crate::softfloat;

/// What a value is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Tag {
    Undefined = 0,
    Null = 1,
    Boolean = 2,
    Number = 3,
    String = 4,
    Symbol = 5,
    BigInt = 6,
    Object = 7,
}

/// A generation-checked reference to a heap cell.
///
/// The generation retires rather than wrapping, so a stale handle cannot become
/// valid again when a slot is reused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Handle {
    pub index: u32,
    pub generation: u32,
}

impl Handle {
    pub const fn new(index: u32, generation: u32) -> Self {
        Self { index, generation }
    }

    /// The handle as one 64-bit word: the generation above the index.
    pub const fn pack(self) -> u64 {
        ((self.generation as u64) << 32) | self.index as u64
    }

    /// The handle a `pack` produced.
    pub const fn unpack(bits: u64) -> Self {
        Self {
            index: (bits & 0xFFFF_FFFF) as u32,
            generation: (bits >> 32) as u32,
        }
    }
}

/// One ECMAScript value.
#[derive(Clone, Copy, Debug)]
pub struct Value {
    tag: Tag,
    payload: u64,
}

impl Value {
    pub const UNDEFINED: Self = Self {
        tag: Tag::Undefined,
        payload: 0,
    };
    pub const NULL: Self = Self {
        tag: Tag::Null,
        payload: 0,
    };
    pub const TRUE: Self = Self {
        tag: Tag::Boolean,
        payload: 1,
    };
    pub const FALSE: Self = Self {
        tag: Tag::Boolean,
        payload: 0,
    };

    pub const fn boolean(value: bool) -> Self {
        if value {
            Self::TRUE
        } else {
            Self::FALSE
        }
    }

    pub fn number(value: f64) -> Self {
        Self {
            tag: Tag::Number,
            payload: canonical_bits(value),
        }
    }

    pub const fn string(handle: Handle) -> Self {
        Self {
            tag: Tag::String,
            payload: handle.pack(),
        }
    }

    pub const fn symbol(handle: Handle) -> Self {
        Self {
            tag: Tag::Symbol,
            payload: handle.pack(),
        }
    }

    pub const fn big_int(handle: Handle) -> Self {
        Self {
            tag: Tag::BigInt,
            payload: handle.pack(),
        }
    }

    pub const fn object(handle: Handle) -> Self {
        Self {
            tag: Tag::Object,
            payload: handle.pack(),
        }
    }

    pub const fn tag(&self) -> Tag {
        self.tag
    }

    pub const fn is_undefined(&self) -> bool {
        matches!(self.tag, Tag::Undefined)
    }

    pub const fn is_null(&self) -> bool {
        matches!(self.tag, Tag::Null)
    }

    /// Whether the value is `null` or `undefined`, which is what `??` and an
    /// optional chain test.
    pub const fn is_nullish(&self) -> bool {
        matches!(self.tag, Tag::Undefined | Tag::Null)
    }

    pub const fn is_number(&self) -> bool {
        matches!(self.tag, Tag::Number)
    }

    pub const fn is_object(&self) -> bool {
        matches!(self.tag, Tag::Object)
    }

    pub const fn is_boolean(&self) -> bool {
        matches!(self.tag, Tag::Boolean)
    }

    pub const fn is_string(&self) -> bool {
        matches!(self.tag, Tag::String)
    }

    /// The boolean a `Boolean` value holds.
    pub const fn as_boolean(&self) -> bool {
        self.payload != 0
    }

    /// The Number a `Number` value holds, or `NaN`.
    pub fn as_number(&self) -> f64 {
        match self.tag {
            Tag::Number => f64::from_bits(self.payload),
            _ => f64::NAN,
        }
    }

    /// The handle a reference value holds.
    pub const fn as_handle(&self) -> Handle {
        Handle::unpack(self.payload)
    }
}

/// A `Number` payload is the value's binary64 bits, with one canonical NaN so
/// that a value copied through a record compares the same everywhere.
fn canonical_bits(value: f64) -> u64 {
    if value.is_nan() {
        0x7FF8_0000_0000_0000
    } else {
        value.to_bits()
    }
}

impl Value {
    /// Build a Number value from an integer without a conversion instruction.
    pub fn from_i32(value: i32) -> Self {
        Self::number(softfloat::from_i64(i64::from(value)))
    }
}

/// A fixed-width field read, checked once, so no access fails at run time.
pub fn field<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
    <[u8; N]>::try_from(bytes.get(at..at + N)?).ok()
}

impl Handle {
    /// The index a cell field holds when it names no handle.
    pub const NONE_INDEX: u32 = u32::MAX;

    /// Write the handle as its index and generation, little-endian.
    pub fn write_at(self, out: &mut [u8], at: usize) {
        if let Some(field) = out.get_mut(at..at + 8) {
            field[..4].copy_from_slice(&self.index.to_le_bytes());
            field[4..].copy_from_slice(&self.generation.to_le_bytes());
        }
    }

    /// Write the "no handle" sentinel.
    pub fn write_none_at(out: &mut [u8], at: usize) {
        if let Some(field) = out.get_mut(at..at + 8) {
            field[..4].copy_from_slice(&Self::NONE_INDEX.to_le_bytes());
            field[4..].copy_from_slice(&0u32.to_le_bytes());
        }
    }

    /// Read a handle written by `write_at`; `None` for the sentinel or a
    /// field that is not there.
    pub fn read_at(bytes: &[u8], at: usize) -> Option<Self> {
        let field: [u8; 8] = field(bytes, at)?;
        let index = u32::from_le_bytes([field[0], field[1], field[2], field[3]]);
        if index == Self::NONE_INDEX {
            return None;
        }
        let generation = u32::from_le_bytes([field[4], field[5], field[6], field[7]]);
        Some(Self::new(index, generation))
    }
}

impl Value {
    /// Bytes one encoded value takes: a tag and eight payload bytes.
    pub const ENCODED: usize = 9;
    /// Bytes the short form takes: a tag, a 32-bit index, a 16-bit
    /// generation. Only an object reference survives it.
    pub const ENCODED_SHORT: usize = 7;

    /// Encode the value into `out` at `at`. A field that is not there is
    /// left alone.
    pub fn encode_at(self, out: &mut [u8], at: usize) {
        if let Some(field) = out.get_mut(at..at + Self::ENCODED) {
            field[0] = self.tag as u8;
            field[1..].copy_from_slice(&self.payload.to_le_bytes());
        }
    }

    /// Decode a value written by `encode_at`; `undefined` for a field that
    /// is not there or a tag that is not one.
    pub fn decode_at(bytes: &[u8], at: usize) -> Self {
        let Some(field) = field::<9>(bytes, at) else {
            return Self::UNDEFINED;
        };
        let payload = u64::from_le_bytes([
            field[1], field[2], field[3], field[4], field[5], field[6], field[7], field[8],
        ]);
        let handle = Handle::unpack(payload);
        match field[0] {
            1 => Self::NULL,
            2 => Self::boolean(payload != 0),
            3 => Self::number(f64::from_bits(payload)),
            4 => Self::string(handle),
            5 => Self::symbol(handle),
            6 => Self::big_int(handle),
            7 => Self::object(handle),
            _ => Self::UNDEFINED,
        }
    }

    /// Encode the short form: a tag and a handle whose generation is
    /// truncated to sixteen bits, which is all an accessor slot holds.
    pub fn encode_short_at(self, out: &mut [u8], at: usize) {
        if let Some(field) = out.get_mut(at..at + Self::ENCODED_SHORT) {
            field[0] = self.tag as u8;
            let handle = self.as_handle();
            field[1..5].copy_from_slice(&handle.index.to_le_bytes());
            field[5..7].copy_from_slice(&(handle.generation as u16).to_le_bytes());
        }
    }

    /// Decode the short form: an object reference, or `undefined`.
    pub fn decode_short_at(bytes: &[u8], at: usize) -> Self {
        let Some(field) = field::<7>(bytes, at) else {
            return Self::UNDEFINED;
        };
        if field[0] != Tag::Object as u8 {
            return Self::UNDEFINED;
        }
        let index = u32::from_le_bytes([field[1], field[2], field[3], field[4]]);
        let generation = u32::from(u16::from_le_bytes([field[5], field[6]]));
        Self::object(Handle::new(index, generation))
    }
}

/// The result of `typeof`, before it is turned into a string.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TypeOf {
    Undefined,
    Object,
    Boolean,
    Number,
    String,
    Symbol,
    BigInt,
    Function,
}

/// `typeof` for a value. A callable object reports `function`, which only the
/// object model can decide, so it is passed in.
pub const fn type_of(value: &Value, callable: bool) -> TypeOf {
    match value.tag {
        Tag::Undefined => TypeOf::Undefined,
        Tag::Null => TypeOf::Object,
        Tag::Boolean => TypeOf::Boolean,
        Tag::Number => TypeOf::Number,
        Tag::String => TypeOf::String,
        Tag::Symbol => TypeOf::Symbol,
        Tag::BigInt => TypeOf::BigInt,
        Tag::Object => {
            if callable {
                TypeOf::Function
            } else {
                TypeOf::Object
            }
        }
    }
}

/// `ToBoolean` for the values that need no heap access. A string's result
/// depends on its length and an object is always true, so those are decided by
/// the caller.
pub fn to_boolean_primitive(value: &Value) -> Option<bool> {
    match value.tag {
        Tag::Undefined | Tag::Null => Some(false),
        Tag::Boolean => Some(value.as_boolean()),
        Tag::Number => {
            let number = value.as_number();
            Some(!(number == 0.0 || number.is_nan()))
        }
        Tag::Symbol => Some(true),
        Tag::Object => Some(true),
        Tag::String | Tag::BigInt => None,
    }
}

/// `ToNumber` for the values that need no heap access.
pub fn to_number_primitive(value: &Value) -> Option<f64> {
    match value.tag {
        Tag::Undefined => Some(f64::NAN),
        Tag::Null => Some(0.0),
        Tag::Boolean => Some(if value.as_boolean() { 1.0 } else { 0.0 }),
        Tag::Number => Some(value.as_number()),
        Tag::String | Tag::BigInt | Tag::Symbol | Tag::Object => None,
    }
}

/// `ToInt32`: truncate towards zero, then take the value modulo two to the
/// thirty-second, as a signed quantity.
pub fn to_int32(value: f64) -> i32 {
    to_uint32(value) as i32
}

/// `ToUint32`: truncate towards zero, then take the value modulo two to the
/// thirty-second.
pub fn to_uint32(value: f64) -> u32 {
    let bits = value.to_bits();
    let biased = ((bits >> 52) & 0x7FF) as i32;
    let fraction = bits & ((1u64 << 52) - 1);
    if biased == 0x7FF {
        // NaN and the infinities convert to zero.
        return 0;
    }
    if biased == 0 {
        // Zero and subnormals truncate to zero.
        return 0;
    }
    let significand = fraction | (1u64 << 52);
    let exponent = biased - 1075;
    let magnitude = if exponent >= 0 {
        if exponent >= 64 {
            // Every bit below two to the thirty-second is zero.
            0
        } else {
            significand.wrapping_shl(exponent as u32)
        }
    } else if -exponent >= 64 {
        0
    } else {
        significand >> (-exponent)
    };
    let truncated = (magnitude & 0xFFFF_FFFF) as u32;
    if bits >> 63 == 1 {
        truncated.wrapping_neg()
    } else {
        truncated
    }
}

/// `ToInt16`, `ToUint16`, `ToInt8`, and `ToUint8` follow the same rule at a
/// narrower width.
pub fn to_uint16(value: f64) -> u16 {
    (to_uint32(value) & 0xFFFF) as u16
}

pub fn to_int16(value: f64) -> i16 {
    to_uint16(value) as i16
}

pub fn to_uint8(value: f64) -> u8 {
    (to_uint32(value) & 0xFF) as u8
}

pub fn to_int8(value: f64) -> i8 {
    to_uint8(value) as i8
}

/// `ToIntegerOrInfinity`: truncate towards zero, mapping NaN to zero.
pub fn to_integer_or_infinity(value: f64) -> f64 {
    if value.is_nan() {
        return 0.0;
    }
    if value.is_infinite() || value == 0.0 {
        return value;
    }
    truncate(value)
}

/// Truncate towards zero, exactly.
pub fn truncate(value: f64) -> f64 {
    let bits = value.to_bits();
    let biased = ((bits >> 52) & 0x7FF) as i32;
    if biased >= 1075 {
        // No fractional bits remain.
        return value;
    }
    if biased < 1023 {
        // The magnitude is below one.
        return if bits >> 63 == 1 { -0.0 } else { 0.0 };
    }
    let drop = 1075 - biased;
    let mask = !((1u64 << drop) - 1);
    f64::from_bits(bits & mask)
}

/// The largest integer at or below `value`.
pub fn floor(value: f64) -> f64 {
    let truncated = truncate(value);
    if value < 0.0 && truncated != value {
        truncated - 1.0
    } else {
        truncated
    }
}

/// The smallest integer at or above `value`.
pub fn ceil(value: f64) -> f64 {
    let truncated = truncate(value);
    if value > 0.0 && truncated != value {
        truncated + 1.0
    } else {
        truncated
    }
}

/// `SameValue` over whole values, which is what `Object.is` answers.
pub fn same_value(left: Value, right: Value) -> bool {
    if left.tag() != right.tag() {
        return false;
    }
    match left.tag() {
        Tag::Number => same_value_number(left.as_number(), right.as_number()),
        _ => strict_equals(&left, &right).unwrap_or(false),
    }
}

/// `SameValueZero`, which differs only in treating the two zeroes as one.
pub fn same_value_zero(left: Value, right: Value) -> bool {
    if left.tag() != right.tag() {
        return false;
    }
    match left.tag() {
        Tag::Number => same_value_zero_number(left.as_number(), right.as_number()),
        _ => strict_equals(&left, &right).unwrap_or(false),
    }
}

/// `ToLength`: an integer clamped to the maximum safe integer.
pub fn to_length(value: f64) -> f64 {
    let integer = to_integer_or_infinity(value);
    if integer <= 0.0 {
        return 0.0;
    }
    let maximum = 9_007_199_254_740_991.0;
    if integer > maximum {
        maximum
    } else {
        integer
    }
}

/// Whether a Number is an integral value, which `ToIndex` and array indexing
/// both need.
pub fn is_integral(value: f64) -> bool {
    value.is_finite() && truncate(value) == value
}

/// The numeric operators over Numbers. Each is the specification's operation on
/// binary64, which is exactly defined, so every target agrees.
pub fn add(left: f64, right: f64) -> f64 {
    left + right
}

pub fn subtract(left: f64, right: f64) -> f64 {
    left - right
}

pub fn multiply(left: f64, right: f64) -> f64 {
    left * right
}

pub fn divide(left: f64, right: f64) -> f64 {
    left / right
}

/// `Number::remainder`, which keeps the dividend's sign.
pub fn remainder(left: f64, right: f64) -> f64 {
    softfloat::rem(left, right)
}

pub fn unary_minus(value: f64) -> f64 {
    f64::from_bits(value.to_bits() ^ (1 << 63))
}

pub fn bitwise_not(value: f64) -> f64 {
    softfloat::from_i64(i64::from(!to_int32(value)))
}

pub fn bitwise_and(left: f64, right: f64) -> f64 {
    softfloat::from_i64(i64::from(to_int32(left) & to_int32(right)))
}

pub fn bitwise_or(left: f64, right: f64) -> f64 {
    softfloat::from_i64(i64::from(to_int32(left) | to_int32(right)))
}

pub fn bitwise_xor(left: f64, right: f64) -> f64 {
    softfloat::from_i64(i64::from(to_int32(left) ^ to_int32(right)))
}

pub fn shift_left(left: f64, right: f64) -> f64 {
    let places = to_uint32(right) & 0x1F;
    softfloat::from_i64(i64::from(to_int32(left).wrapping_shl(places)))
}

pub fn shift_right(left: f64, right: f64) -> f64 {
    let places = to_uint32(right) & 0x1F;
    softfloat::from_i64(i64::from(to_int32(left).wrapping_shr(places)))
}

pub fn unsigned_shift_right(left: f64, right: f64) -> f64 {
    let places = to_uint32(right) & 0x1F;
    softfloat::from_u64(u64::from(to_uint32(left).wrapping_shr(places)))
}

/// The strict equality of two Numbers, where NaN equals nothing and the zeroes
/// are equal.
pub fn number_equals(left: f64, right: f64) -> bool {
    softfloat::compare(left, right) == 0
}

/// `SameValue` over Numbers: NaN equals NaN and the zeroes differ.
pub fn same_value_number(left: f64, right: f64) -> bool {
    if left.is_nan() && right.is_nan() {
        return true;
    }
    canonical_bits(left) == canonical_bits(right)
}

/// `SameValueZero` over Numbers: NaN equals NaN and the zeroes are equal.
pub fn same_value_zero_number(left: f64, right: f64) -> bool {
    if left.is_nan() && right.is_nan() {
        return true;
    }
    softfloat::compare(left, right) == 0
}

/// The result of the abstract relational comparison, which is undefined when
/// either operand is NaN.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Comparison {
    Less,
    Equal,
    Greater,
    Undefined,
}

pub fn compare_numbers(left: f64, right: f64) -> Comparison {
    match softfloat::compare(left, right) {
        -1 => Comparison::Less,
        0 => Comparison::Equal,
        1 => Comparison::Greater,
        _ => Comparison::Undefined,
    }
}

/// Strict equality between two values, for the cases that need no heap.
///
/// Two references are equal when they name the same cell; a caller that has
/// interned its strings can rely on that, and one that has not compares the
/// contents itself.
pub fn strict_equals(left: &Value, right: &Value) -> Option<bool> {
    if left.tag() != right.tag() {
        return Some(false);
    }
    match left.tag() {
        Tag::Undefined | Tag::Null => Some(true),
        Tag::Boolean => Some(left.as_boolean() == right.as_boolean()),
        Tag::Number => Some(number_equals(left.as_number(), right.as_number())),
        Tag::Symbol | Tag::Object => Some(left.as_handle() == right.as_handle()),
        Tag::String | Tag::BigInt => {
            if left.as_handle() == right.as_handle() {
                Some(true)
            } else {
                None
            }
        }
    }
}

/// The Number a string denotes, following the `StringNumericLiteral` grammar.
///
/// An empty or blank string is zero, a malformed one is NaN, and the literal
/// grammar is the numeric literal grammar plus a leading sign, `Infinity`, and
/// no BigInt suffix or legacy octal.
/// Whether a code unit is white space where a number may be written, which is
/// what the string-to-number conversions skip.
pub fn is_string_whitespace_unit(unit: u16) -> bool {
    is_string_whitespace(unit)
}

pub fn string_to_number(units: &[u16]) -> f64 {
    let mut start = 0usize;
    let mut end = units.len();
    while start < end && is_string_whitespace(units[start]) {
        start += 1;
    }
    while end > start && is_string_whitespace(units[end - 1]) {
        end -= 1;
    }
    let trimmed = match units.get(start..end) {
        Some(slice) => slice,
        None => return f64::NAN,
    };
    if trimmed.is_empty() {
        return 0.0;
    }

    let (negative, body) = match trimmed[0] {
        0x2B => (false, &trimmed[1..]),
        0x2D => (true, &trimmed[1..]),
        _ => (false, trimmed),
    };
    if body.is_empty() {
        return f64::NAN;
    }

    if equals_ascii(body, b"Infinity") {
        return if negative {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }

    // A radix literal has no sign and no fraction.
    if !negative && body.len() > 2 && body[0] == 0x30 {
        let radix = match body[1] {
            0x78 | 0x58 => 16u32,
            0x6F | 0x4F => 8,
            0x62 | 0x42 => 2,
            _ => 0,
        };
        if radix != 0 {
            let mut digits = [0u8; 1024];
            let mut length = 0usize;
            for &unit in &body[2..] {
                let Some(digit) = ascii_digit(unit, radix) else {
                    return f64::NAN;
                };
                match digits.get_mut(length) {
                    Some(slot) => {
                        *slot = digit;
                        length += 1;
                    }
                    None => return f64::INFINITY,
                }
            }
            if length == 0 {
                return f64::NAN;
            }
            return crate::numeric::radix_value(digits.get(..length).unwrap_or(&[]), radix);
        }
    }

    // Decimal: integer part, fraction, exponent, each optional but at least one
    // digit somewhere.
    let mut integer = [0u8; 1024];
    let mut integer_length = 0usize;
    let mut fraction = [0u8; 1024];
    let mut fraction_length = 0usize;
    let mut exponent = 0i32;
    let mut cursor = 0usize;
    let mut digits_seen = false;

    while cursor < body.len() && (0x30..=0x39).contains(&body[cursor]) {
        if let Some(slot) = integer.get_mut(integer_length) {
            *slot = u8::try_from(body[cursor]).unwrap_or(b'0');
            integer_length += 1;
        }
        digits_seen = true;
        cursor += 1;
    }
    if cursor < body.len() && body[cursor] == 0x2E {
        cursor += 1;
        while cursor < body.len() && (0x30..=0x39).contains(&body[cursor]) {
            if let Some(slot) = fraction.get_mut(fraction_length) {
                *slot = u8::try_from(body[cursor]).unwrap_or(b'0');
                fraction_length += 1;
            }
            digits_seen = true;
            cursor += 1;
        }
    }
    if !digits_seen {
        return f64::NAN;
    }
    if cursor < body.len() && (body[cursor] == 0x65 || body[cursor] == 0x45) {
        cursor += 1;
        let mut exponent_negative = false;
        if cursor < body.len() && (body[cursor] == 0x2B || body[cursor] == 0x2D) {
            exponent_negative = body[cursor] == 0x2D;
            cursor += 1;
        }
        let mut exponent_digits = 0usize;
        while cursor < body.len() && (0x30..=0x39).contains(&body[cursor]) {
            let digit = i32::from(body[cursor] - 0x30);
            exponent = exponent.saturating_mul(10).saturating_add(digit);
            if exponent > 100_000 {
                exponent = 100_000;
            }
            exponent_digits += 1;
            cursor += 1;
        }
        if exponent_digits == 0 {
            return f64::NAN;
        }
        if exponent_negative {
            exponent = -exponent;
        }
    }
    if cursor != body.len() {
        return f64::NAN;
    }

    let value = crate::numeric::decimal_value(crate::numeric::DecimalLiteral {
        integer: integer.get(..integer_length).unwrap_or(&[]),
        fraction: fraction.get(..fraction_length).unwrap_or(&[]),
        exponent,
    });
    if negative {
        unary_minus(value)
    } else {
        value
    }
}

/// The white space and line terminators `ToNumber` trims from a string.
fn is_string_whitespace(unit: u16) -> bool {
    matches!(
        unit,
        0x09 | 0x0A
            | 0x0B
            | 0x0C
            | 0x0D
            | 0x20
            | 0xA0
            | 0x1680
            | 0x2028
            | 0x2029
            | 0x202F
            | 0x205F
            | 0x3000
            | 0xFEFF
    ) || (0x2000..=0x200A).contains(&unit)
}

fn ascii_digit(unit: u16, radix: u32) -> Option<u8> {
    let value = match unit {
        0x30..=0x39 => u32::from(unit) - 0x30,
        0x61..=0x66 => u32::from(unit) - 0x61 + 10,
        0x41..=0x46 => u32::from(unit) - 0x41 + 10,
        _ => return None,
    };
    if value < radix {
        u8::try_from(unit).ok()
    } else {
        None
    }
}

fn equals_ascii(units: &[u16], text: &[u8]) -> bool {
    if units.len() != text.len() {
        return false;
    }
    let mut index = 0usize;
    while index < units.len() {
        if units[index] != u16::from(text[index]) {
            return false;
        }
        index += 1;
    }
    true
}
