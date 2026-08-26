# Values and Numeric Coercion

Source: `modules/common/value.rs`, `modules/common/softfloat.rs`,
`modules/common/numeric.rs`.

This document defines how a value is represented, how the numeric coercions
behave, and how binary64 arithmetic is made available on every declared target.
The conversions that read a string's contents take the code units as a slice, so
this layer needs no heap; the heap and the object model are defined in
`strings-and-heap.md` and `objects.md`.

## 1. Representation

A value is a tag and a payload word. The tag names one of `undefined`, `null`,
`boolean`, `number`, `string`, `symbol`, `bigint`, and `object`. A Number's
payload is the bits of its binary64 representation, with one canonical NaN so
that a value copied through a record compares the same everywhere. A string,
symbol, BigInt, or object payload is a handle: an index and a generation.

No value holds a pointer. A value can therefore be copied into a frame, a
register, or a record without anything to fix up, and a stale handle cannot
become valid again when a slot is reused, because generations retire rather than
wrapping.

## 2. Binary64 on every target

ECMAScript arithmetic is IEEE-754 binary64 with round-to-nearest-even. That is
exactly specified: the same operands give the same result, so a program's
arithmetic does not depend on where it runs.

On a target whose instruction set has doubles, the engine uses them. On a target
without them, and with no library to call, the compiler emits calls to routines
the engine supplies, which compute the same results from integer operations
alone: addition, subtraction, multiplication, division, remainder, the ordered
comparisons, and the conversions between doubles and integers. They use nothing
wider than a 64-bit integer, so they need no further routine to satisfy them.

Results are bit-identical to a hardware unit, including subnormals, the signed
zeroes, the infinities, and NaN.

## 3. Numeric coercion

The conversions follow the specification exactly:

- `ToBoolean` and `ToNumber` over the primitives that need no heap. A string or
  a BigInt returns nothing here, because deciding those needs its contents.
- `ToInt32`, `ToUint32`, and the narrower widths: truncate towards zero, then
  take the value modulo two to the width. The modulo is computed on the bits, so
  a value far above the width converts correctly rather than saturating, which
  is what a machine conversion instruction would do.
- `ToIntegerOrInfinity`, `ToLength`, and an exact truncation towards zero.
- The numeric operators, the bitwise operators, and the shifts, each defined on
  the converted integer and returned as a Number.
- `SameValue`, `SameValueZero`, and strict equality, which differ precisely in
  how they treat NaN and the signed zeroes.

`ToNumber` on a string follows the `StringNumericLiteral` grammar rather than
the source numeric literal grammar: leading and trailing white space and line
terminators are trimmed, an empty or blank string is zero, a sign and `Infinity`
are admitted, the radix prefixes are admitted, a BigInt suffix and a legacy
octal literal are not, and anything malformed is NaN.
