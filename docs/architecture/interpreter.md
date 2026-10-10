# Environments, Functions, and the Interpreter

Source: `modules/common/env.rs`, `modules/common/vm.rs`, `modules/common/vm/`,
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

The global environment is two records. The outer names the global object, and
a lookup that reaches it asks the object for the property, which is how a
global reference reaches a property of the global object. Above it sits the
global lexical record, where every script's top-level `let`, `const`, and
`class` binds by name: a binding there is visible to every later script and
to an indirect `eval`, is never a property of the global object, cannot be
deleted, and keeps its dead zone until its declaration runs. A script
instantiates its declarations before its first statement — every lexical name
checked against the global lexicals, the names earlier scripts declared with
`var`, and the non-configurable properties of the global object, and every
`var` and function name checked against the global lexicals — so a collision
is a `SyntaxError` thrown before any binding is made. A record that fills is
followed by a fresh one beneath it. Every walk outwards is bounded by an
admitted scope depth.

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

A call written in bytecode pushes a frame and continues the same loop, so a
deep JavaScript call chain is bounded by the admitted frame count rather than
by the machine's own stack. A call a *native* makes — an accessor, a sort
comparator, a `valueOf` reached through `ToPrimitive`, a promise reaction —
nests a bounded inner loop instead: it still pushes ordinary frames, spends
the same fuel, and charges what it burns against the outer slice, and how many
such loops one host stack may nest is itself a declared bound, so nesting ends
as `StackOverflow` rather than wherever the platform's stack happens to.

Fuel is spent per instruction. Exhausting it stops execution with a termination
the program cannot catch, as do a full frame stack, a full register file, and an
exhausted heap.

Collection is spent from the same budget, a slice at a time, so what a loop
costs in fuel is mostly whether it allocates: a turn that makes nothing costs
its dispatches, some seventeen for `s += i`, and a turn that makes anything
costs that and the collector's share, forty and up. The two things a plain
loop reaches for are therefore made not to allocate. A named access — `o.s`,
a global, a method — keys from the image: the name's units are read out of
the constant pool and interned, so a key is looked up, not made, and no
string is created to be collected. And a `for (let …)` whose span holds no
function or class, in a program with no direct eval, copies no context
between turns: nothing could tell one turn's binding from the next, so the
loop keeps the one it pushed; a `for (const x of …)` does the same, taking
each turn's value into the binding it made ahead of the loop. A number keys
without becoming a string: an array index as the index it is, any other by
its decimal form interned from a buffer, so `a[i]` makes nothing either. And
a `for`-`of` over an iterator the engine made itself — an array's, a
string's, a Map's or a Set's — steps it without the result object the
protocol would make and the loop would only read and drop: the loop asks
first (`StepNativeIterator`), and calls `next` as the protocol says only when
the iterator is not the engine's, which is what keeps a generator or a
program's own iterator on the ordinary path. `IteratorNext` — destructuring,
a spread — steps the same way, so what those still make is the iterator
object, and a spread its argument array: one allocation a use. And an array's element is read
where it was appended: `object::find` tries record `i + 1` for index `i` —
record 0 being `length` — before it scans, so a read of an element costs
one comparison however long the array, where it cost a scan of everything
before it. `tests/e2e/density.sh` holds all of this, measured marginally
over two loop sizes.

Fuel is not time. Measured in instructions the processor runs, a turn of a
plain loop costs some 6,000 for its thirteen bytecodes — a few hundred per
bytecode, in decoding, bounds, the function record and the frame — and a
collection slice costs far less than the 512 fuel it is charged. So the
changes above moved time less than they moved fuel: a third off the script
loop, two thirds off an array walk, a few percent off an allocation.
`tests/load/run.sh`, which times the script loop, went from 413,000 to
634,000 iterations a second on the Pi 5 (2,419 to 1,577 ns each), and its
capability-call rate did not move. What bounds wall-clock from here is the
cost of a dispatch, not of what a program allocates.

So the dispatch was made cheaper where it was paying for nothing. The loop
fetched the running function's record — twelve reads out of the image —
for every instruction; it now fetches it when the frame on top changes.
It copied the whole frame for every instruction, a memmove past the
inline threshold; it now holds a copy and refreshes it when the frame is
written, which every write to a frame other than the loop's own program
counter says (`frame_changed`), and a nested run says on its way out. It
judged the heap's pressure every instruction; it does so every sixteenth,
since pressure changes only when something is allocated, and a collection
left open is still carried on every instruction. And the decoder read its
opcode and signature from two matches over the whole opcode set and its
operands through a loop of bounds checks; it reads both from a table built
at compile time, and narrow operands — nearly all of them — in one checked
slice. A turn of the plain loop fell from 6,000 instructions to 2,700, and
the load lane reads 1,167,000 iterations a second.

Below that, what a value costs to move. A value is nine bytes in a cell — a
tag and an eight-byte payload — and reading it as a nine-byte field, or
writing a handle through a slice whose length the compiler cannot see, was
a call to memcpy on every load and store of a binding; they are read and
written as a byte and a word now. A small integer literal was assembled
into a double in software on every target, where the conversion is exactly
specified and a hard-float target has the instruction; it is a cast there.
And the string branch of `ToNumber` carried its buffer into the frame of
every instruction, so it is a function of its own. What is left per opcode
is the dispatch itself — some 120 instructions of loop, bounds, decode and
the switch — and, for a binding, the environment record: a `var` in a
function was a context slot, not a register, so `s = r` was two records
read and written, ~160 instructions each.

So a function nothing inside can capture keeps its bindings in registers.
The lowering decides it once the declarations are known: a plain parameter
list, no function or class within the body, no `eval` it might direct, no
`with`, no `arguments`, and nothing lexical at the function's own level —
so every binding is a parameter or a `var`, with no dead zone to keep. The
arguments are in the first registers already, which is where the parameters
resolve; the `var`s start `undefined` behind them, which the prologue makes
true since a call may have supplied more arguments than there are
parameters; a named function expression's own name is the callee. Blocks
inside keep their contexts, and a closure anywhere in the body keeps the
whole function on contexts. Its scope is still a context for the names
outside it — a call makes the callee a record whatever it binds, which is
where `this` and `new.target` live — only one with no slots. `s = r` is a
`Ldar` and a `Star`.

Then the operators. `+`, `-`, `*`, `/` and the four relational comparisons
went through the general path whatever their operands — two `ToPrimitive`,
the string and BigInt checks, two `ToNumber`, and for a comparison the
integer routine even where the target compares doubles in hardware. Two
numbers now go straight to the arithmetic, and `compare_numbers` uses the
instruction on a hard-float target. And `step` is a large function with a
large frame: entering and leaving it saved twelve registers and probed six
kilobytes of stack, for every instruction, `Ldar` included. The dispatch
loop now does the instructions loops are made of — register moves, small
literals, a jump on a boolean, number arithmetic and comparison, `++` and
`--` on a number — itself, where their operands make them unable to throw,
call out or allocate (`step_fast`); anything else goes to `step` as before.
`decode` is inlined, so an instruction is not returned through memory and
copied out again — on bcm2712 the module target is `aarch64-unknown-none`,
which assumes strict alignment, and that copy was a call to memcpy. A turn
of the plain loop is now 1,650 instructions, and the load lane reads
1,550,000 iterations a second.

That assumption was the last of it. `aarch64-unknown-none` builds for a part
whose MMU may be off, so LLVM never emits an unaligned access: a struct copy
it cannot prove aligned becomes a call to memcpy, and a word read from a
byte slice becomes a chain of byte loads. An engine that keeps its image,
heap cells and environment records as byte slices pays that on nearly every
read. Module code never runs with the MMU off — on the Pi 5 and on QEMU virt
the kernel enables it before any module is loaded — so Fluxor builds bcm2712
modules without strict alignment (Fluxor `f1cc70a7`). The load lane reads
2,280,000 iterations a second, and code that touches the heap, environments
or calls spends a fifth to a third less time; the bare arithmetic loop,
already free of copies, is unchanged.

Two things in the profile are not the engine's. The kernel hashes module
state once a step (its attestation), and the isolate reads the clock twice
a step; both are per step, so the slice length is what sets their share —
and the slice is a policy knob rather than a dispatch cost: at 32,768 fuel
the loop runs a third faster again and capability calls take half as long
to come back, since each step holds the scheduler for longer. It stays at
4,096.

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

A call in tail position of a strict function's `return` is the
specification's proper tail call: the running frame is given up before the
callee's frame is made, so a chain of such calls — a recursion however deep
— holds one frame. The compiler marks the position, through a conditional,
a comma, or a logical operator's chosen operand; the machine takes the short
path only for a callee it would enter as a frame, and only from a frame with
nothing waiting on how it ends — never from a construction, a generator, an
async function, or under a finaliser still to run.

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

The error constructors are there too: `Error` and each of its kinds, from
`TypeError` to `AggregateError`, each with its own prototype carrying its
name, and `Error.prototype.toString`. They may be called or constructed.

`JSON` is a parser and a serialiser, pure functions of their arguments.
`Date` is arithmetic over time values in UTC: what it cannot do on its own is
tell the time, which is a capability rather than something an engine may help
itself to — without a clock from the host, "now" is the epoch, the same
answer every run.
