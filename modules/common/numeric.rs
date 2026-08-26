//! Correctly rounded conversion of ECMAScript numeric literals to binary64.
//!
//! The conversion is exact on every target: the same literal always produces
//! the same double, and the result is the one ECMAScript requires, which is the
//! nearest representable value with ties resolved to even.
//!
//! Every literal goes through a fixed-size decimal buffer that is shifted by
//! powers of two until the significand can be read directly. The conversion
//! needs no allocation, no wide-integer arithmetic, and no floating-point
//! arithmetic, so it runs identically on targets without hardware doubles.

const MANTISSA_BITS: u32 = 52;
const EXPONENT_BITS: u32 = 11;
const EXPONENT_BIAS: i32 = -1023;
const MAX_DIGITS: usize = 800;
const MAX_SHIFT: u32 = 60;

/// Binary shift that moves the decimal point by one decimal place, indexed by
/// the number of decimal places remaining.
#[rustfmt::skip]
static POWER_TABLE: [u32; 9] = [1, 3, 6, 9, 13, 16, 19, 23, 26];

/// Digits gained by multiplying by two to the power of the index, with the
/// digits of five to that power as the tie-breaking prefix and its length.
/// The prefixes are stored inline rather than as slices so the table needs no
/// relocation when a module image is loaded at an arbitrary address.
#[rustfmt::skip]
static LEFT_SHIFT_CHEATS: [(u32, u8, [u8; 42]); 61] = [
    (0, 0, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (1, 1, [5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (1, 2, [2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (1, 3, [1, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (2, 3, [6, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (2, 4, [3, 1, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (2, 5, [1, 5, 6, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (3, 5, [7, 8, 1, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (3, 6, [3, 9, 0, 6, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (3, 7, [1, 9, 5, 3, 1, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (4, 7, [9, 7, 6, 5, 6, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (4, 8, [4, 8, 8, 2, 8, 1, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (4, 9, [2, 4, 4, 1, 4, 0, 6, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (4, 10, [1, 2, 2, 0, 7, 0, 3, 1, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (5, 10, [6, 1, 0, 3, 5, 1, 5, 6, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (5, 11, [3, 0, 5, 1, 7, 5, 7, 8, 1, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (5, 12, [1, 5, 2, 5, 8, 7, 8, 9, 0, 6, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (6, 12, [7, 6, 2, 9, 3, 9, 4, 5, 3, 1, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (6, 13, [3, 8, 1, 4, 6, 9, 7, 2, 6, 5, 6, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (6, 14, [1, 9, 0, 7, 3, 4, 8, 6, 3, 2, 8, 1, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (7, 14, [9, 5, 3, 6, 7, 4, 3, 1, 6, 4, 0, 6, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (7, 15, [4, 7, 6, 8, 3, 7, 1, 5, 8, 2, 0, 3, 1, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (7, 16, [2, 3, 8, 4, 1, 8, 5, 7, 9, 1, 0, 1, 5, 6, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (7, 17, [1, 1, 9, 2, 0, 9, 2, 8, 9, 5, 5, 0, 7, 8, 1, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (8, 17, [5, 9, 6, 0, 4, 6, 4, 4, 7, 7, 5, 3, 9, 0, 6, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (8, 18, [2, 9, 8, 0, 2, 3, 2, 2, 3, 8, 7, 6, 9, 5, 3, 1, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (8, 19, [1, 4, 9, 0, 1, 1, 6, 1, 1, 9, 3, 8, 4, 7, 6, 5, 6, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (9, 19, [7, 4, 5, 0, 5, 8, 0, 5, 9, 6, 9, 2, 3, 8, 2, 8, 1, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (9, 20, [3, 7, 2, 5, 2, 9, 0, 2, 9, 8, 4, 6, 1, 9, 1, 4, 0, 6, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (9, 21, [1, 8, 6, 2, 6, 4, 5, 1, 4, 9, 2, 3, 0, 9, 5, 7, 0, 3, 1, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (10, 21, [9, 3, 1, 3, 2, 2, 5, 7, 4, 6, 1, 5, 4, 7, 8, 5, 1, 5, 6, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (10, 22, [4, 6, 5, 6, 6, 1, 2, 8, 7, 3, 0, 7, 7, 3, 9, 2, 5, 7, 8, 1, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (10, 23, [2, 3, 2, 8, 3, 0, 6, 4, 3, 6, 5, 3, 8, 6, 9, 6, 2, 8, 9, 0, 6, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (10, 24, [1, 1, 6, 4, 1, 5, 3, 2, 1, 8, 2, 6, 9, 3, 4, 8, 1, 4, 4, 5, 3, 1, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (11, 24, [5, 8, 2, 0, 7, 6, 6, 0, 9, 1, 3, 4, 6, 7, 4, 0, 7, 2, 2, 6, 5, 6, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (11, 25, [2, 9, 1, 0, 3, 8, 3, 0, 4, 5, 6, 7, 3, 3, 7, 0, 3, 6, 1, 3, 2, 8, 1, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (11, 26, [1, 4, 5, 5, 1, 9, 1, 5, 2, 2, 8, 3, 6, 6, 8, 5, 1, 8, 0, 6, 6, 4, 0, 6, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (12, 26, [7, 2, 7, 5, 9, 5, 7, 6, 1, 4, 1, 8, 3, 4, 2, 5, 9, 0, 3, 3, 2, 0, 3, 1, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (12, 27, [3, 6, 3, 7, 9, 7, 8, 8, 0, 7, 0, 9, 1, 7, 1, 2, 9, 5, 1, 6, 6, 0, 1, 5, 6, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (12, 28, [1, 8, 1, 8, 9, 8, 9, 4, 0, 3, 5, 4, 5, 8, 5, 6, 4, 7, 5, 8, 3, 0, 0, 7, 8, 1, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (13, 28, [9, 0, 9, 4, 9, 4, 7, 0, 1, 7, 7, 2, 9, 2, 8, 2, 3, 7, 9, 1, 5, 0, 3, 9, 0, 6, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (13, 29, [4, 5, 4, 7, 4, 7, 3, 5, 0, 8, 8, 6, 4, 6, 4, 1, 1, 8, 9, 5, 7, 5, 1, 9, 5, 3, 1, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (13, 30, [2, 2, 7, 3, 7, 3, 6, 7, 5, 4, 4, 3, 2, 3, 2, 0, 5, 9, 4, 7, 8, 7, 5, 9, 7, 6, 5, 6, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (13, 31, [1, 1, 3, 6, 8, 6, 8, 3, 7, 7, 2, 1, 6, 1, 6, 0, 2, 9, 7, 3, 9, 3, 7, 9, 8, 8, 2, 8, 1, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (14, 31, [5, 6, 8, 4, 3, 4, 1, 8, 8, 6, 0, 8, 0, 8, 0, 1, 4, 8, 6, 9, 6, 8, 9, 9, 4, 1, 4, 0, 6, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (14, 32, [2, 8, 4, 2, 1, 7, 0, 9, 4, 3, 0, 4, 0, 4, 0, 0, 7, 4, 3, 4, 8, 4, 4, 9, 7, 0, 7, 0, 3, 1, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (14, 33, [1, 4, 2, 1, 0, 8, 5, 4, 7, 1, 5, 2, 0, 2, 0, 0, 3, 7, 1, 7, 4, 2, 2, 4, 8, 5, 3, 5, 1, 5, 6, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (15, 33, [7, 1, 0, 5, 4, 2, 7, 3, 5, 7, 6, 0, 1, 0, 0, 1, 8, 5, 8, 7, 1, 1, 2, 4, 2, 6, 7, 5, 7, 8, 1, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    (15, 34, [3, 5, 5, 2, 7, 1, 3, 6, 7, 8, 8, 0, 0, 5, 0, 0, 9, 2, 9, 3, 5, 5, 6, 2, 1, 3, 3, 7, 8, 9, 0, 6, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0]),
    (15, 35, [1, 7, 7, 6, 3, 5, 6, 8, 3, 9, 4, 0, 0, 2, 5, 0, 4, 6, 4, 6, 7, 7, 8, 1, 0, 6, 6, 8, 9, 4, 5, 3, 1, 2, 5, 0, 0, 0, 0, 0, 0, 0]),
    (16, 35, [8, 8, 8, 1, 7, 8, 4, 1, 9, 7, 0, 0, 1, 2, 5, 2, 3, 2, 3, 3, 8, 9, 0, 5, 3, 3, 4, 4, 7, 2, 6, 5, 6, 2, 5, 0, 0, 0, 0, 0, 0, 0]),
    (16, 36, [4, 4, 4, 0, 8, 9, 2, 0, 9, 8, 5, 0, 0, 6, 2, 6, 1, 6, 1, 6, 9, 4, 5, 2, 6, 6, 7, 2, 3, 6, 3, 2, 8, 1, 2, 5, 0, 0, 0, 0, 0, 0]),
    (16, 37, [2, 2, 2, 0, 4, 4, 6, 0, 4, 9, 2, 5, 0, 3, 1, 3, 0, 8, 0, 8, 4, 7, 2, 6, 3, 3, 3, 6, 1, 8, 1, 6, 4, 0, 6, 2, 5, 0, 0, 0, 0, 0]),
    (16, 38, [1, 1, 1, 0, 2, 2, 3, 0, 2, 4, 6, 2, 5, 1, 5, 6, 5, 4, 0, 4, 2, 3, 6, 3, 1, 6, 6, 8, 0, 9, 0, 8, 2, 0, 3, 1, 2, 5, 0, 0, 0, 0]),
    (17, 38, [5, 5, 5, 1, 1, 1, 5, 1, 2, 3, 1, 2, 5, 7, 8, 2, 7, 0, 2, 1, 1, 8, 1, 5, 8, 3, 4, 0, 4, 5, 4, 1, 0, 1, 5, 6, 2, 5, 0, 0, 0, 0]),
    (17, 39, [2, 7, 7, 5, 5, 5, 7, 5, 6, 1, 5, 6, 2, 8, 9, 1, 3, 5, 1, 0, 5, 9, 0, 7, 9, 1, 7, 0, 2, 2, 7, 0, 5, 0, 7, 8, 1, 2, 5, 0, 0, 0]),
    (17, 40, [1, 3, 8, 7, 7, 7, 8, 7, 8, 0, 7, 8, 1, 4, 4, 5, 6, 7, 5, 5, 2, 9, 5, 3, 9, 5, 8, 5, 1, 1, 3, 5, 2, 5, 3, 9, 0, 6, 2, 5, 0, 0]),
    (18, 40, [6, 9, 3, 8, 8, 9, 3, 9, 0, 3, 9, 0, 7, 2, 2, 8, 3, 7, 7, 6, 4, 7, 6, 9, 7, 9, 2, 5, 5, 6, 7, 6, 2, 6, 9, 5, 3, 1, 2, 5, 0, 0]),
    (18, 41, [3, 4, 6, 9, 4, 4, 6, 9, 5, 1, 9, 5, 3, 6, 1, 4, 1, 8, 8, 8, 2, 3, 8, 4, 8, 9, 6, 2, 7, 8, 3, 8, 1, 3, 4, 7, 6, 5, 6, 2, 5, 0]),
    (18, 42, [1, 7, 3, 4, 7, 2, 3, 4, 7, 5, 9, 7, 6, 8, 0, 7, 0, 9, 4, 4, 1, 1, 9, 2, 4, 4, 8, 1, 3, 9, 1, 9, 0, 6, 7, 3, 8, 2, 8, 1, 2, 5]),
    (19, 42, [8, 6, 7, 3, 6, 1, 7, 3, 7, 9, 8, 8, 4, 0, 3, 5, 4, 7, 2, 0, 5, 9, 6, 2, 2, 4, 0, 6, 9, 5, 9, 5, 3, 3, 6, 9, 1, 4, 0, 6, 2, 5]),
];

/// A decimal significand with an explicit decimal point, held in fixed storage.
struct Decimal {
    digits: [u8; MAX_DIGITS],
    count: usize,
    point: i32,
    truncated: bool,
}

impl Decimal {
    const fn new() -> Self {
        Self {
            digits: [0; MAX_DIGITS],
            count: 0,
            point: 0,
            truncated: false,
        }
    }

    fn push(&mut self, digit: u8) {
        if self.count == 0 && digit == 0 {
            self.point -= 1;
            return;
        }
        if self.count < MAX_DIGITS {
            self.digits[self.count] = digit;
            self.count += 1;
        } else if digit != 0 {
            self.truncated = true;
        }
    }

    fn trim(&mut self) {
        while self.count > 0 && self.digits[self.count - 1] == 0 {
            self.count -= 1;
        }
        if self.count == 0 {
            self.point = 0;
        }
    }

    fn digit(&self, index: usize) -> u8 {
        match self.digits.get(index) {
            Some(&digit) => digit,
            None => 0,
        }
    }

    /// Divide by two to the power `shift`, for `shift` at most `MAX_SHIFT`.
    fn shift_right(&mut self, shift: u32) {
        let mut read = 0usize;
        let mut write = 0usize;
        let mut carry = 0u64;

        while carry >> shift == 0 {
            if read >= self.count {
                if carry == 0 {
                    self.count = 0;
                    return;
                }
                while carry >> shift == 0 {
                    carry *= 10;
                    read += 1;
                }
                break;
            }
            carry = carry * 10 + u64::from(self.digit(read));
            read += 1;
        }
        self.point -= i32::try_from(read).unwrap_or(i32::MAX) - 1;

        let mask = (1u64 << shift) - 1;
        while read < self.count {
            let next = u64::from(self.digit(read));
            read += 1;
            let quotient = carry >> shift;
            carry &= mask;
            if let Some(slot) = self.digits.get_mut(write) {
                *slot = u8::try_from(quotient).unwrap_or(0);
                write += 1;
            }
            carry = carry * 10 + next;
        }
        while carry > 0 {
            let quotient = carry >> shift;
            carry &= mask;
            if write < MAX_DIGITS {
                self.digits[write] = u8::try_from(quotient).unwrap_or(0);
                write += 1;
            } else if quotient > 0 {
                self.truncated = true;
            }
            carry *= 10;
        }
        self.count = write;
        self.trim();
    }

    /// Multiply by two to the power `shift`, for `shift` at most `MAX_SHIFT`.
    fn shift_left(&mut self, shift: u32) {
        let (mut delta, cutoff_length, cutoff) = match LEFT_SHIFT_CHEATS.get(shift as usize) {
            Some(&(delta, length, cutoff)) => (delta as usize, length as usize, cutoff),
            None => return,
        };
        if self.prefix_is_less_than(&cutoff, cutoff_length) {
            delta -= 1;
        }

        let mut read = self.count;
        let mut write = self.count + delta;
        let mut carry = 0u64;

        while read > 0 {
            read -= 1;
            carry += u64::from(self.digit(read)) << shift;
            let quotient = carry / 10;
            let remainder = carry - quotient * 10;
            write -= 1;
            if write < MAX_DIGITS {
                self.digits[write] = u8::try_from(remainder).unwrap_or(0);
            } else if remainder != 0 {
                self.truncated = true;
            }
            carry = quotient;
        }
        while carry > 0 {
            let quotient = carry / 10;
            let remainder = carry - quotient * 10;
            if write == 0 {
                break;
            }
            write -= 1;
            if write < MAX_DIGITS {
                self.digits[write] = u8::try_from(remainder).unwrap_or(0);
            } else if remainder != 0 {
                self.truncated = true;
            }
            carry = quotient;
        }

        self.count += delta;
        if self.count > MAX_DIGITS {
            self.count = MAX_DIGITS;
        }
        self.point += i32::try_from(delta).unwrap_or(0);
        self.trim();
    }

    /// Whether the significand is lexically smaller than `cutoff`, which
    /// decides whether a left shift gains one digit or two.
    fn prefix_is_less_than(&self, cutoff: &[u8], length: usize) -> bool {
        let mut index = 0usize;
        while index < length {
            if index >= self.count {
                return true;
            }
            let digit = self.digit(index);
            let expected = match cutoff.get(index) {
                Some(&expected) => expected,
                None => return false,
            };
            if digit != expected {
                return digit < expected;
            }
            index += 1;
        }
        false
    }

    fn shift(&mut self, places: i32) {
        if self.count == 0 {
            return;
        }
        let mut places = places;
        while places > i32::try_from(MAX_SHIFT).unwrap_or(0) {
            self.shift_left(MAX_SHIFT);
            places -= i32::try_from(MAX_SHIFT).unwrap_or(0);
        }
        while places < -i32::try_from(MAX_SHIFT).unwrap_or(0) {
            self.shift_right(MAX_SHIFT);
            places += i32::try_from(MAX_SHIFT).unwrap_or(0);
        }
        if places > 0 {
            self.shift_left(u32::try_from(places).unwrap_or(0));
        } else if places < 0 {
            self.shift_right(u32::try_from(-places).unwrap_or(0));
        }
    }

    /// Whether the digits after `place` round the integer part upwards.
    fn should_round_up(&self, place: usize) -> bool {
        if place >= self.count {
            return false;
        }
        if self.digit(place) == 5 && place + 1 == self.count {
            if self.truncated {
                return true;
            }
            return place > 0 && self.digit(place - 1) % 2 != 0;
        }
        self.digit(place) >= 5
    }

    /// The integer part, rounded to nearest with ties to even.
    fn rounded_integer(&self) -> u64 {
        if self.point > 20 {
            return u64::MAX;
        }
        if self.point < 0 {
            // Every digit sits below the units place, so the integer part is
            // zero and no digit can round it up.
            return 0;
        }
        let places = self.point as usize;
        let mut value = 0u64;
        let mut index = 0usize;
        while index < places && index < self.count {
            value = value
                .saturating_mul(10)
                .saturating_add(u64::from(self.digit(index)));
            index += 1;
        }
        while index < places {
            value = value.saturating_mul(10);
            index += 1;
        }
        if self.should_round_up(places) {
            value = value.saturating_add(1);
        }
        value
    }

    /// Consume the decimal and return the nearest double, with ties to even.
    fn into_f64(mut self) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        if self.point > 310 {
            return f64::INFINITY;
        }
        if self.point < -330 {
            return 0.0;
        }

        let mut exponent = 0i32;
        while self.point > 0 {
            let places = usize::try_from(self.point).unwrap_or(usize::MAX);
            let shift = match POWER_TABLE.get(places) {
                Some(&shift) => shift,
                None => 27,
            };
            self.shift(-i32::try_from(shift).unwrap_or(0));
            exponent += i32::try_from(shift).unwrap_or(0);
        }
        while self.point < 0 || (self.point == 0 && self.digit(0) < 5) {
            let places = usize::try_from(-self.point).unwrap_or(usize::MAX);
            let shift = match POWER_TABLE.get(places) {
                Some(&shift) => shift,
                None => 27,
            };
            self.shift(i32::try_from(shift).unwrap_or(0));
            exponent -= i32::try_from(shift).unwrap_or(0);
        }

        // The significand is now in [0.5, 1), so the value is significand
        // times two to the exponent.
        exponent -= 1;

        if exponent < EXPONENT_BIAS + 1 {
            let places = EXPONENT_BIAS + 1 - exponent;
            self.shift(-places);
            exponent += places;
        }
        if exponent - EXPONENT_BIAS >= (1 << EXPONENT_BITS) - 1 {
            return f64::INFINITY;
        }

        self.shift(i32::try_from(MANTISSA_BITS).unwrap_or(0) + 1);
        let mut mantissa = self.rounded_integer();
        if mantissa == 2 << MANTISSA_BITS {
            mantissa >>= 1;
            exponent += 1;
            if exponent - EXPONENT_BIAS >= (1 << EXPONENT_BITS) - 1 {
                return f64::INFINITY;
            }
        }
        if mantissa & (1 << MANTISSA_BITS) == 0 {
            exponent = EXPONENT_BIAS;
        }

        let biased = u64::try_from(exponent - EXPONENT_BIAS).unwrap_or(0);
        let bits = (mantissa & ((1 << MANTISSA_BITS) - 1))
            | ((biased & ((1 << EXPONENT_BITS) - 1)) << MANTISSA_BITS);
        f64::from_bits(bits)
    }
}

/// A numeric literal broken into the parts the lexer already recognised.
///
/// `integer` and `fraction` hold significant digits with separators removed.
/// `exponent` is the value of the literal's exponent part.
#[derive(Clone, Copy, Debug)]
pub struct DecimalLiteral<'a> {
    pub integer: &'a [u8],
    pub fraction: &'a [u8],
    pub exponent: i32,
}

/// The double denoted by a decimal literal.
pub fn decimal_value(literal: DecimalLiteral<'_>) -> f64 {
    let mut decimal = Decimal::new();
    for &byte in literal.integer {
        let digit = byte.wrapping_sub(b'0');
        if digit <= 9 {
            decimal.push(digit);
            decimal.point += 1;
        }
    }
    for &byte in literal.fraction {
        let digit = byte.wrapping_sub(b'0');
        if digit <= 9 {
            decimal.push(digit);
        }
    }
    decimal.trim();
    decimal.point = decimal.point.saturating_add(literal.exponent);
    decimal.into_f64()
}

/// The double denoted by an integer literal in radix 2, 8, or 16.
///
/// Digits beyond the significand contribute a sticky bit, so rounding is the
/// same nearest-with-ties-to-even rule the decimal path uses.
pub fn radix_value(digits: &[u8], radix: u32) -> f64 {
    let bits_per_digit = match radix {
        2 => 1u32,
        8 => 3,
        16 => 4,
        _ => return f64::NAN,
    };

    let mut significand = 0u64;
    let mut exponent = 0i32;
    let mut sticky = false;
    let mut started = false;

    for &byte in digits {
        let Some(digit) = digit_value(byte, radix) else {
            continue;
        };
        if !started {
            if digit == 0 {
                continue;
            }
            started = true;
            significand = u64::from(digit);
            continue;
        }
        if significand >> (64 - bits_per_digit) == 0 {
            significand = (significand << bits_per_digit) | u64::from(digit);
        } else {
            exponent += i32::try_from(bits_per_digit).unwrap_or(0);
            sticky |= digit != 0;
        }
    }

    if !started {
        return 0.0;
    }
    round_binary(significand, sticky, exponent)
}

fn digit_value(byte: u8, radix: u32) -> Option<u32> {
    let value = match byte {
        b'0'..=b'9' => u32::from(byte - b'0'),
        b'a'..=b'f' => u32::from(byte - b'a') + 10,
        b'A'..=b'F' => u32::from(byte - b'A') + 10,
        _ => return None,
    };
    if value < radix {
        Some(value)
    } else {
        None
    }
}

/// Round `significand` times two to the `exponent`, where `sticky` records that
/// non-zero bits were discarded below the significand.
fn round_binary(significand: u64, sticky: bool, exponent: i32) -> f64 {
    let mut significand = significand;
    let mut exponent = exponent;
    let mut sticky = sticky;

    // Normalise so the significand occupies exactly 53 bits.
    let width = 64 - significand.leading_zeros();
    if width > 53 {
        let drop = width - 53;
        let mask = (1u64 << drop) - 1;
        let discarded = significand & mask;
        let halfway = 1u64 << (drop - 1);
        significand >>= drop;
        exponent += i32::try_from(drop).unwrap_or(0);
        let round_up = if discarded > halfway {
            true
        } else if discarded == halfway {
            sticky || significand & 1 == 1
        } else {
            false
        };
        sticky |= discarded != 0;
        if round_up {
            significand += 1;
            if significand >> 53 != 0 {
                significand >>= 1;
                exponent += 1;
            }
        }
    } else if width < 53 {
        let gain = 53 - width;
        significand <<= gain;
        exponent -= i32::try_from(gain).unwrap_or(0);
    }
    let _ = sticky;

    // The significand is 53 bits, so the unbiased exponent of the leading bit
    // is exponent + 52.
    let unbiased = exponent + 52;
    if unbiased > 1023 {
        return f64::INFINITY;
    }
    if unbiased < -1074 {
        return 0.0;
    }
    if unbiased < -1022 {
        let shift = u32::try_from(-1022 - unbiased).unwrap_or(64);
        if shift >= 64 {
            return 0.0;
        }
        let subnormal = significand >> shift;
        return f64::from_bits(subnormal);
    }
    let biased = u64::try_from(unbiased + 1023).unwrap_or(0);
    let bits = (significand & ((1 << MANTISSA_BITS) - 1)) | (biased << MANTISSA_BITS);
    f64::from_bits(bits)
}

/// The signed 32-bit integer a double denotes exactly, if it denotes one.
///
/// The test is made on the bits rather than by converting, so it holds on
/// targets with no floating-point instructions.
pub fn exact_i32(value: f64) -> Option<i32> {
    let bits = value.to_bits();
    let negative = bits >> 63 == 1;
    let exponent = ((bits >> 52) & 0x7FF) as i32;
    let fraction = bits & ((1u64 << 52) - 1);

    if exponent == 0 {
        // Zero, of either sign, or a subnormal, which is never an integer.
        return if fraction == 0 { Some(0) } else { None };
    }
    if exponent == 0x7FF {
        return None;
    }
    let unbiased = exponent - 1023;
    if !(0..=31).contains(&unbiased) {
        return None;
    }

    let mantissa = fraction | (1u64 << 52);
    let shift = 52 - unbiased;
    if shift > 0 && mantissa & ((1u64 << shift) - 1) != 0 {
        return None;
    }
    let magnitude = mantissa >> shift;
    if negative {
        if magnitude > 1u64 << 31 {
            return None;
        }
        if magnitude == 1u64 << 31 {
            return Some(i32::MIN);
        }
        i32::try_from(magnitude).ok().map(|value| -value)
    } else {
        i32::try_from(magnitude).ok()
    }
}

/// `Number::exponentiate`, which is what `**` computes.
///
/// The special cases follow the specification exactly. An integral exponent is
/// computed by repeated squaring; any other is computed from a base-two
/// logarithm and exponential, which the specification admits as an
/// implementation-approximated result.
/// The Number `parseInt` reads from the front of a string.
///
/// Leading white space and an optional sign are admitted, then digits in the
/// radix; anything after them is ignored, and a string with no digits at all is
/// NaN. A radix of 16 admits the `0x` prefix, and so does a radix of zero,
/// which the caller resolves to 16 or 10.
pub fn parse_int_prefix(units: &[u16], radix: u32) -> f64 {
    let mut index = 0usize;
    while index < units.len() && crate::value::is_string_whitespace_unit(units[index]) {
        index += 1;
    }
    let mut negative = false;
    if index < units.len() && (units[index] == u16::from(b'-') || units[index] == u16::from(b'+')) {
        negative = units[index] == u16::from(b'-');
        index += 1;
    }
    let mut radix = radix;
    if (radix == 16 || radix == 10)
        && index + 1 < units.len()
        && units[index] == u16::from(b'0')
        && (units[index + 1] == u16::from(b'x') || units[index + 1] == u16::from(b'X'))
    {
        radix = 16;
        index += 2;
    }
    if !(2..=36).contains(&radix) {
        return f64::NAN;
    }
    let mut digits = [0u8; 128];
    let mut count = 0usize;
    while index < units.len() && count < digits.len() {
        let unit = units[index];
        let digit = match unit {
            0x30..=0x39 => (unit - 0x30) as u32,
            0x41..=0x5A => (unit - 0x41) as u32 + 10,
            0x61..=0x7A => (unit - 0x61) as u32 + 10,
            _ => break,
        };
        if digit >= radix {
            break;
        }
        digits[count] = digit as u8 + if digit < 10 { b'0' } else { b'a' - 10 };
        count += 1;
        index += 1;
    }
    if count == 0 {
        return f64::NAN;
    }
    let value = radix_value(digits.get(..count).unwrap_or(&[]), radix);
    if negative {
        -value
    } else {
        value
    }
}

/// The Number `parseFloat` reads from the front of a string.
pub fn parse_float_prefix(units: &[u16]) -> f64 {
    let mut index = 0usize;
    while index < units.len() && crate::value::is_string_whitespace_unit(units[index]) {
        index += 1;
    }
    let start = index;
    if index < units.len() && (units[index] == u16::from(b'-') || units[index] == u16::from(b'+')) {
        index += 1;
    }
    let mut seen_digit = false;
    while index < units.len() && (0x30..=0x39).contains(&units[index]) {
        seen_digit = true;
        index += 1;
    }
    if index < units.len() && units[index] == u16::from(b'.') {
        index += 1;
        while index < units.len() && (0x30..=0x39).contains(&units[index]) {
            seen_digit = true;
            index += 1;
        }
    }
    if seen_digit && index < units.len() && (units[index] | 0x20) == u16::from(b'e') {
        let mark = index;
        index += 1;
        if index < units.len()
            && (units[index] == u16::from(b'-') || units[index] == u16::from(b'+'))
        {
            index += 1;
        }
        let mut exponent_digit = false;
        while index < units.len() && (0x30..=0x39).contains(&units[index]) {
            exponent_digit = true;
            index += 1;
        }
        if !exponent_digit {
            index = mark;
        }
    }
    if !seen_digit {
        return f64::NAN;
    }
    crate::value::string_to_number(units.get(start..index).unwrap_or(&[]))
}

/// The square root, to the precision the format holds.
///
/// Newton's method from a scaled estimate, using only the four operations the
/// softfloat layer supplies, so it holds on a target with no floating-point
/// instructions.
pub fn sqrt(value: f64) -> f64 {
    if value.is_nan() || value < 0.0 {
        return f64::NAN;
    }
    if value == 0.0 || value.is_infinite() {
        return value;
    }
    // Halve the exponent for a first estimate, which Newton's method then
    // refines; the iteration doubles the correct digits each turn.
    let bits = value.to_bits();
    let exponent = ((bits >> 52) & 0x7FF) as i64;
    let estimate_bits = if exponent == 0 {
        // Subnormal: start from the value itself rather than from its exponent.
        bits
    } else {
        let halved = ((exponent - 1023) / 2 + 1023) as u64;
        (halved << 52) | ((bits & ((1u64 << 52) - 1)) / 2)
    };
    let mut estimate = f64::from_bits(estimate_bits);
    if estimate <= 0.0 {
        estimate = value;
    }
    let mut turn = 0;
    while turn < 24 {
        let next = (estimate + value / estimate) / 2.0;
        if next == estimate {
            break;
        }
        estimate = next;
        turn += 1;
    }
    estimate
}

/// The digits of an integer-valued number in a radix, written into `out`.
///
/// Only the integer part is written, which is what a radix conversion of an
/// integral value needs; a fraction is dropped rather than approximated.
pub fn radix_text(value: f64, radix: u32, out: &mut [u16]) -> usize {
    if value.is_nan() {
        return write_ascii(b"NaN", out);
    }
    if value.is_infinite() {
        return write_ascii(
            if value < 0.0 {
                b"-Infinity"
            } else {
                b"Infinity"
            },
            out,
        );
    }
    let negative = value < 0.0;
    let mut magnitude = crate::value::truncate(if negative { -value } else { value });
    let mut digits = [0u8; 64];
    let mut count = 0usize;
    let radix_value = f64::from(radix);
    if magnitude == 0.0 {
        digits[0] = b'0';
        count = 1;
    }
    while magnitude >= 1.0 && count < digits.len() {
        let quotient = crate::value::truncate(magnitude / radix_value);
        let digit = magnitude - quotient * radix_value;
        let digit = crate::value::to_uint32(digit) as u8;
        digits[count] = if digit < 10 {
            b'0' + digit
        } else {
            b'a' + digit - 10
        };
        count += 1;
        magnitude = quotient;
    }
    let mut written = 0usize;
    if negative {
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

fn write_ascii(text: &[u8], out: &mut [u16]) -> usize {
    let mut written = 0usize;
    for &byte in text {
        if let Some(slot) = out.get_mut(written) {
            *slot = u16::from(byte);
            written += 1;
        }
    }
    written
}

pub fn power(base: f64, exponent: f64) -> f64 {
    if exponent.is_nan() {
        return f64::NAN;
    }
    if exponent == 0.0 {
        return 1.0;
    }
    if base.is_nan() {
        return f64::NAN;
    }

    let base_bits = base.to_bits();
    let negative_base = base_bits >> 63 == 1;
    let magnitude = f64::from_bits(base_bits & !(1 << 63));
    let exponent_negative = exponent.to_bits() >> 63 == 1;

    if exponent.is_infinite() {
        if magnitude == 1.0 {
            return f64::NAN;
        }
        let large = magnitude > 1.0;
        return if large != exponent_negative {
            f64::INFINITY
        } else {
            0.0
        };
    }

    // An odd integral exponent keeps the sign of a negative base.
    let integral = exact_i32(exponent);
    let odd = integral.is_some_and(|value| value % 2 != 0);

    if magnitude.is_infinite() {
        return if exponent_negative {
            if negative_base && odd {
                -0.0
            } else {
                0.0
            }
        } else if negative_base && odd {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    if magnitude == 0.0 {
        return if exponent_negative {
            if negative_base && odd {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }
        } else if negative_base && odd {
            -0.0
        } else {
            0.0
        };
    }
    if negative_base && integral.is_none() {
        // A negative base with a non-integral exponent has no real result.
        return f64::NAN;
    }

    let magnitude_result = if let Some(power) = integral {
        integer_power(magnitude, power)
    } else {
        exp2(exponent * log2(magnitude))
    };
    if negative_base && odd {
        f64::from_bits(magnitude_result.to_bits() ^ (1 << 63))
    } else {
        magnitude_result
    }
}

/// A positive base raised to an integral power, by repeated squaring.
///
/// The squaring carries a second term, so the rounding error of each
/// multiplication does not accumulate through the chain. Without it, a long
/// chain drifts by several places in the last digits.
fn integer_power(base: f64, exponent: i32) -> f64 {
    let mut remaining = exponent.unsigned_abs();
    let mut factor = (base, 0.0f64);
    let mut result = (1.0f64, 0.0f64);
    while remaining > 0 {
        if remaining & 1 == 1 {
            result = multiply_pair(result, factor);
        }
        remaining >>= 1;
        if remaining > 0 {
            factor = multiply_pair(factor, factor);
        }
    }
    if exponent < 0 {
        // The reciprocal is refined against both terms, so a negative exponent
        // does not round twice.
        let estimate = 1.0 / result.0;
        let (product, error) = exact_product(result.0, estimate);
        if !product.is_finite() {
            return 1.0 / (result.0 + result.1);
        }
        let residual = ((1.0 - product) - error) - result.1 * estimate;
        estimate + estimate * residual
    } else {
        result.0 + result.1
    }
}

/// Split a double into two halves whose product with another split double is
/// exact. The constant is two to the twenty-seventh plus one.
fn split(value: f64) -> (f64, f64) {
    let scaled = 134_217_729.0 * value;
    let high = scaled - (scaled - value);
    (high, value - high)
}

/// The exact product of two doubles, as a rounded value and its error.
fn exact_product(left: f64, right: f64) -> (f64, f64) {
    let product = left * right;
    if !product.is_finite() {
        return (product, 0.0);
    }
    let (left_high, left_low) = split(left);
    let (right_high, right_low) = split(right);
    if !left_high.is_finite() || !right_high.is_finite() {
        return (product, 0.0);
    }
    let error =
        ((left_high * right_high - product) + left_high * right_low + left_low * right_high)
            + left_low * right_low;
    (product, error)
}

/// Multiply two values each carried as a leading term and a correction.
fn multiply_pair(left: (f64, f64), right: (f64, f64)) -> (f64, f64) {
    let (high, low) = exact_product(left.0, right.0);
    if !high.is_finite() {
        return (high, 0.0);
    }
    let low = low + (left.0 * right.1 + left.1 * right.0);
    let value = high + low;
    (value, low - (value - high))
}

/// The base-two logarithm of a positive finite value.
fn log2(value: f64) -> f64 {
    let bits = value.to_bits();
    let biased = ((bits >> 52) & 0x7FF) as i32;
    let (mantissa_bits, exponent) = if biased == 0 {
        // Scale a subnormal into the normal range first.
        let scaled = value * 9_007_199_254_740_992.0;
        let bits = scaled.to_bits();
        (bits, ((bits >> 52) & 0x7FF) as i32 - 1023 - 53)
    } else {
        (bits, biased - 1023)
    };
    // The significand in the range one to two.
    let mantissa = f64::from_bits((mantissa_bits & ((1u64 << 52) - 1)) | (1023u64 << 52));

    // log2(m) through the area hyperbolic tangent series, which converges
    // quickly because the argument stays below a third.
    let z = (mantissa - 1.0) / (mantissa + 1.0);
    let square = z * z;
    let mut term = z;
    let mut sum = z;
    let mut divisor = 3.0f64;
    let mut index = 0;
    while index < 12 {
        term *= square;
        sum += term / divisor;
        divisor += 2.0;
        index += 1;
    }
    // Two over the natural logarithm of two.
    f64::from(exponent) + sum * core::f64::consts::LOG2_E * 2.0
}

/// Two raised to a finite power.
fn exp2(value: f64) -> f64 {
    if value >= 1024.0 {
        return f64::INFINITY;
    }
    if value <= -1075.0 {
        return 0.0;
    }
    let whole = truncate_towards_negative(value);
    let fraction = value - whole;

    // Two to the fraction through the exponential series.
    let x = fraction * core::f64::consts::LN_2;
    let mut term = 1.0f64;
    let mut sum = 1.0f64;
    let mut index = 1u32;
    while index < 18 {
        term *= x / f64::from(index);
        sum += term;
        index += 1;
    }
    scale_by_power_of_two(sum, whole as i32)
}

/// Round towards negative infinity, exactly, on the bits.
fn truncate_towards_negative(value: f64) -> f64 {
    let bits = value.to_bits();
    let biased = ((bits >> 52) & 0x7FF) as i32;
    let truncated = if biased >= 1075 {
        value
    } else if biased < 1023 {
        if bits >> 63 == 1 {
            -0.0
        } else {
            0.0
        }
    } else {
        let drop = 1075 - biased;
        f64::from_bits(bits & !((1u64 << drop) - 1))
    };
    if truncated > value {
        truncated - 1.0
    } else {
        truncated
    }
}

/// Multiply by two to an integral power, without an exponent instruction.
fn scale_by_power_of_two(value: f64, power: i32) -> f64 {
    let mut result = value;
    let mut remaining = power;
    while remaining > 512 {
        result *= f64::from_bits(0x7FE0_0000_0000_0000);
        remaining -= 1023;
    }
    while remaining < -512 {
        result *= f64::from_bits(0x0010_0000_0000_0000);
        remaining += 1022;
    }
    let biased = (remaining + 1023) as u64;
    result * f64::from_bits(biased << 52)
}
