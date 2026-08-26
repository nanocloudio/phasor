# Heap, Strings, and Property Keys

Source: `modules/common/heap.rs`, `modules/common/string.rs`,
`modules/common/dtoa.rs`.

This document defines how cells are allocated and addressed, how a string is
stored and compared, what a property key is, and how a Number becomes a string.
Collection is defined with the heap it moves: see `collection.md`. A heap with
no collector attached reports exhaustion rather than reclaiming.

## 1. The heap

A heap is one caller-provided byte arena and a handle table. Every cell lives in
the arena and is addressed through a table entry, so nothing outside the heap
holds a pointer into it and an arena could be compacted by rewriting entries
alone.

A handle is an index and a generation. A slot's generation advances when the
slot is retired, and it retires permanently rather than wrapping, so a handle
kept past its cell's life names nothing: reading through it is a stale-handle
result, not a read of whatever now occupies the slot.

Allocation is a bump through the arena, eight-byte aligned so a payload can hold
a value or a 64-bit field directly. Cells are zeroed before they are handed out.
An allocation that does not fit, or that has no free slot, is an ordinary
failure.

## 2. Strings

A string cell stores its length in UTF-16 code units and its units either as one
byte each, where every unit fits in a byte, or as two bytes each. That choice is
storage only: length, indexing, comparison, and concatenation are all in UTF-16
code units, which is what ECMAScript string semantics expose. A supplementary
code point therefore occupies two units, exactly as a program would observe.

Concatenation produces a new cell and widens to two-byte storage when either
operand needs it. Comparison is by code unit and then by length, which is the
order the relational operators use.

## 3. Property keys

A key is an array index, an interned name, or a symbol. A string is an array
index when it is the canonical decimal text of a value below the largest index:
`0`, or a digit sequence with no leading zero. That is exactly the set of
strings that address an element rather than a named property.

Names are interned: one handle per distinct string, kept in an open-addressed
table over caller-provided storage. Interning is what makes a key comparison a
handle comparison, which is what the object model needs. A full table is an
ordinary failure rather than a rehash, because the table's size is admitted.

## 4. Number to string

`Number::toString` needs the shortest decimal string that reads back as the same
double. That is a property of the exact binary value, so the digit generator is
exact: it holds the value and its neighbours' midpoints as a rational in
fixed-size integers wide enough for any double, scales until the first digit is
due, and emits digits until the remainder identifies the value uniquely. It uses
no floating-point arithmetic, so every target produces the same digits.

Two details decide the last digit. The interval is closed when the significand
is even, because a decimal exactly on a boundary still reads back as this value.
Where two shortest strings are equally close, the specification takes the even
one.

The layout around those digits follows the specification: an integer form up to
twenty-one digits, a fixed form down to a millionth, and an exponential form
outside those.
