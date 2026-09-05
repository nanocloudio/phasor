# Bytecode, Verification, and Content Identity

Source: `modules/common/bytecode.rs`, `modules/common/emit.rs`,
`modules/common/lower.rs`, `modules/common/lower/`, `modules/common/verify.rs`,
`modules/common/digest.rs`.

This document defines the instruction encoding, the unit image, how a program
is lowered into it, what the verifier proves before anything executes, and how
an artefact is identified. The machine that runs the result is defined in
[interpreter.md](interpreter.md).

## 1. Machine model

The machine is an accumulator-and-register machine. An accumulator carries the
result of the expression in progress, and indexed frame registers hold locals,
temporaries, and arguments. Binary and comparison instructions read their left
operand from a register and their right operand from the accumulator, and leave
the result in the accumulator, which is why ordinary expression code needs no
operand for its common cases.

A function declares its register count, argument count, frame extent, context
depth, exception regions, and safe points. Those declarations are part of what
the verifier checks the code against, so the isolate can size a frame from the
record rather than by scanning the code.

## 2. Instruction encoding

An instruction is an optional width prefix, one opcode byte, then its operands.
Operands are one byte by default; the `Wide` prefix makes them two and the
`ExtraWide` prefix four. Every operand of one instruction has the same width.

Each opcode has a fixed operand list, and each operand has a kind the verifier
checks it against: a frame register, a constant index, a signed immediate, a
count, a signed jump displacement from the start of the jumping instruction, or
a context depth.

Width selection is a rule rather than a search, so encoding is deterministic:

- an instruction takes the narrowest width that holds all of its operands;
- a backward jump takes the narrowest width its known displacement fits; and
- a forward jump is always `Wide`, because its displacement is unknown when it
  is written, and a displacement that does not fit is a build failure rather
  than a silent re-encoding.

## 3. The unit image

A unit is one canonical byte image: little-endian fixed-width integers, explicit
offsets, no implicit padding, and no pointers. Sections appear in one order:

```text
header | functions | constants | constant data | code | exception regions | safe points | imports | exports | eval sites
```

The header carries the magic `PHBC`, the format digest, the feature digest, the
section counts, the entry function, and the unit's flags, of which one says the
unit is a module. The import and export tables exist for a module — see
[modules.md](modules.md) — and the eval-site records say, for each direct
`eval`, which bindings are visible there. Function records are fixed-width, so a function is addressed
by index without a table scan. Constant records carry a kind and two payload
words: a Number holds the two halves of its binary64 bits, and a string, key, or
BigInt holds an offset and length into the constant data. Strings and keys are
stored as little-endian UTF-16 code units, which is what ECMAScript string
semantics index.

Parsing an image validates the magic, the format digest, and every section
offset before any accessor runs, so a reader cannot address outside the image.

## 4. Lowering

The compiler walks the syntax arena once and emits into caller-provided storage.
Registers follow a stack discipline: an expression takes registers above the ones
its parent holds and releases them when it finishes, and the high-water mark
becomes the function's declared register count. Constants are interned by
content, so the same text appears once and identical sources produce identical
tables.

Calls pass their receiver and arguments in consecutive registers: the operand
naming the first register is the receiver, the count includes it, and the
verifier checks the whole window against the frame. A construct passes its
arguments the same way without a receiver.

An identifier reference lowers to a context slot when a scope declares it and
to a global load otherwise (§4a). Short-circuiting constructs lower to branches: `&&`, `||`, `??`, their
assignment forms, the conditional operator, and each optional link in a chain.
A template lowers to a running concatenation, a string literal to a constant
load, and a numeric literal to an immediate when its value is exactly a signed
32-bit integer and to a constant otherwise.

A construct the parser accepts but the lowering cannot express — a label the
builder ran out of room for, a target no reference form covers — is reported as
`lowering-not-admitted` rather than silently dropped, where it is written. A
spread walks whatever its operand iterates, so it lowers to an iteration rather
than to an opcode of its own. A BigInt literal is carried as its digits and
radix and becomes an exact integer when the image runs.

## 4a. Names, scopes, and closures

Names are resolved when a program is lowered, not while it runs. Every scope a
program declares is a record in a tree that outlives the walk, so a function
body lowered after the code around it still sees exactly the scopes it was
written inside. A name found there becomes a context slot, addressed by how many
contexts out it is and which slot it is; a name found nowhere becomes a global,
which the isolate resolves.

A call gives the callee an environment with a slot for each of its parameters,
its `var` declarations, and its own name where it has one. A block that declares
`let`, `const`, or a function pushes a context of its own and pops it on the way
out; a block that declares nothing costs nothing. A slot starts uninitialised,
and reading one that has not been initialised is a reference error, which is
what the temporal dead zone requires. A `for` loop whose header declares with
`let` gets bindings of its own each turn, so a closure made in the body keeps
the value that turn had.

Assigning to a `const` is refused where it is written: a program that does it
can never be right, so there is nothing for the runtime to decide.

A `finally` block runs on every path out of its `try` — falling off the end,
catching, throwing on, and any `break`, `continue`, or `return` that escapes —
and it is written into the code at each of those points rather than jumped to,
so no path can skip it and no return address has to be tracked at run time.

## 5. Content identity

Identity is SHA-256 over canonical bytes.

The **format digest** is derived from the encoding itself: the format tag, the
prefix bytes, the record sizes, and every assigned opcode with its operand kinds
are hashed. Changing an opcode number, an operand kind, or a record size
therefore changes the digest, and an image compiled under the old format fails
admission rather than being reinterpreted. There is no compatibility window and
no version dispatch.

The **feature digest** is derived from the admitted feature list in
`modules/common/feature.rs`: every entry's name and version, in order. It says
what language the image was compiled against, which the format digest does not —
the same encoding can carry two different languages. Adding, removing, or
versioning a feature changes the digest, and an image compiled against a
different list is refused as `feature-list-mismatch`. Both digests sit in the
unit header, and both are checked before any section is read.

The **logical digest** is the digest of the whole unit image. Because the image
is canonical, the same source produces the same digest on every target, which is
what makes an artefact addressable by content.

## 6. What the verifier proves

Nothing executes before verification passes. For every function the verifier:

- decodes every instruction from the start of the code, so instruction
  boundaries are established rather than assumed, and rejects unknown opcodes,
  truncated operands, and a prefix that does not precede an opcode;
- checks every register operand against the declared register count, every
  constant operand against the constant table, every context depth against the
  declared depth, and the whole argument window of a call against the frame;
- requires that the code end with a terminator, so control cannot run off the
  end;
- resolves every branch and requires its target to be inside the function and on
  an instruction boundary;
- requires the target of every backward edge to be a declared safe point, which
  is what makes a loop interruptible by a deadline, a cancellation, or a
  collection slice;
- checks that safe points are ascending instruction boundaries;
- checks that exception regions are ordered, disjoint, bounded by the code, and
  that their start and handler are instruction boundaries, and that the register
  receiving a thrown value is in the frame; and
- checks that context pushes and pops balance: it carries a context depth along
  the code, records the depth at each branch target, requires every path
  reaching an instruction to agree on it, and requires depth zero at every
  return.

An instruction that no path reaches is rejected rather than skipped, because a
depth cannot be proved for code with no predecessor.

Dynamic call depth is deliberately not claimed to be provable here. The isolate
checks the graph-admitted call-stack limit before every call and construct.
