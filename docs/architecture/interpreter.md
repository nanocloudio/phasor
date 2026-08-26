# Environments, Functions, and the Interpreter

Source: `modules/common/env.rs`, `modules/common/vm.rs`,
`modules/common/realm.rs`, `modules/common/object.rs`.

This document defines how bindings are stored, what a function is, how the
interpreter runs verified bytecode, how a throw unwinds, and what a realm starts
with.

## 1. Environments

An environment record is a heap cell holding a parent, a kind, and a flat array
of bindings. A binding carries its name, whether it may be assigned, and whether
it has been initialised, so a read before initialisation is a distinct outcome
from a read of a name that is not bound: that distinction is the temporal dead
zone.

A global environment holds no bindings of its own. It names an object, and a
lookup that reaches it asks the object for the property, which is how a global
reference reaches a property of the global object. Every walk outwards is
bounded by an admitted scope depth.

## 2. Functions

A function is an ordinary object with two more internal slots: the code it runs
and the environment it closed over. A call creates a function environment whose
parent is that closure, so a function reaches the bindings that surrounded it
however far it travels. Arguments arrive in the callee's first registers.

Some functions are implemented by the engine rather than by bytecode. Those
carry an identifier instead of a code index and are called the same way, which
is what lets `toString` and `valueOf` exist before there is a way to write them
in the language.

## 3. The interpreter

One machine is one running Agent: a frame stack, a register file, an
accumulator, and a fuel budget, all over caller-provided storage. Execution is a
loop over verified instructions, so no instruction re-checks what the verifier
proved: an operand index is used directly.

Every operation the language defines as calling something is performed by
pushing a frame and continuing the same loop. That covers an accessor, a
`valueOf` reached through `ToPrimitive`, and a native method. There is no host
recursion for a JavaScript call, so a deep call chain is bounded by the admitted
frame count rather than by the machine's own stack.

Fuel is spent per instruction. Exhausting it stops execution with a termination
the program cannot catch, as do a full frame stack, a full register file, and an
exhausted heap.

## 4. Semantics implemented

The operators are the specification's: `ToPrimitive` with the hint each context
requires, string concatenation when either operand of `+` is a string, the
abstract relational comparison over strings and numbers, strict and loose
equality, `typeof` including the unresolvable reference that `typeof` alone
admits, the bitwise operators over the integer conversions, and the
short-circuiting operators.

Property access is the ordinary object behaviour, plus a string's own `length`
and its indexed code units. Reading a property of `null` or `undefined` throws,
as does calling something that is not callable.

`**` follows the specification's special cases exactly. Its general result is
computed from a logarithm and an exponential, which the specification admits as
implementation-approximated, so it may differ from another engine in the last
place.

## 5. Throwing and unwinding

A throw looks for the innermost exception region of the current function that
covers the instruction that threw. Finding one puts the thrown value in the
register the region names and continues at its handler. Finding none pops the
frame and asks the caller, and so on outwards; a throw that leaves the run is
reported as the run's completion, with the value it carried.

The verifier makes that search safe: it has already checked that regions are
ordered and disjoint, that a handler is an instruction boundary, and that the
register is inside the frame. A handler is reachable through its region's edge,
so the verifier gives it the context depth its region started at rather than
treating it as unreachable code.

The engine's own errors are ordinary error objects: reading a property of `null`
or `undefined`, calling something that is not callable, constructing something
that is not a constructor, and using `instanceof` with a non-callable right
operand all throw a `TypeError`, and an unresolvable global reference throws a
`ReferenceError`. Each carries the realm's prototype for its kind, so a program
sees the name it expects.

## 6. The realm

A realm is a global object and the intrinsic prototypes. It holds `undefined`,
`NaN`, and `Infinity` as non-writable, non-configurable properties, and
`globalThis`, and the intrinsics `library.md` lists.

The error constructors are there too: `Error`, `TypeError`, `RangeError`,
`ReferenceError`, and `SyntaxError`, each with its own prototype carrying its
name, and `Error.prototype.toString`. They may be called or constructed.

`JSON` is not there, and neither is a `Date`: one is a parser and a serialiser
that nothing in the engine needs, and the other is a clock, which is a
capability rather than something an engine may help itself to.
