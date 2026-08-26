# Engine Internals

This page is the map of the engine's moving parts: what happens between a
source arriving and a result leaving, and where each part is specified. Every
part lives as `no_std` shared source under `modules/common/` and is compiled
into the fmods that need it.

## Front end

`phasor_compile` performs a bounded pipeline over an admitted source transfer:

1. cooperative lexing and parsing into an indexed syntax arena;
2. declarations, scopes, and the strictness each function's prologue declares;
3. ahead-of-time lowering of every function to bytecode;
4. verification; and
5. canonical serialization as a unit image.

The lexer and parser cooperate because `/` versus a regular-expression
literal, automatic semicolon insertion, templates, and contextual keywords
require syntactic context. Source positions are byte offsets plus line-start
indexes; JavaScript string semantics are UTF-16 even though source transport
is UTF-8. All functions compile ahead of execution: lazy compilation would put
compiler state and source retention into the isolate and make latency depend
on the first call. Each phase has admitted node, byte, and traversal bounds,
and the temporary syntax arena is discarded once the unit commits.

The token surface and its limits are in the
[lexical grammar](lexical-grammar.md); the parsed surface and its feature
versioning are in the [expression grammar](expression-grammar.md).

## Bytecode and verification

Phasor uses a compact accumulator-and-register bytecode: an accumulator
carries common expression results, indexed frame registers hold locals,
temporaries, and arguments, and each function record declares its register
count, argument count, exception regions, and safe points. Narrow operands are
the default, with explicit width prefixes for larger ones. There is no JIT.

The verifier rejects unknown opcodes, malformed operands, invalid control-flow
targets, unreachable code, out-of-range registers or constants, unbalanced
context operations, missing safe points on backward edges, and any declared
bound inconsistent with the instruction stream. Call depth is enforced at run
time against the admitted frame table, not claimed statically. The format
digest is derived from the encoding itself, so any change to the instruction
set changes the digest and every image compiled under the old one fails
admission. The full format is in [bytecode.md](bytecode.md).

## Values, objects, and strings

`Value` is a fixed-width tagged representation: immediate `undefined`, `null`,
booleans, and binary64 numbers; strings, symbols, BigInts, and objects are
generation-checked heap handles. No raw heap pointer is stored in bytecode,
sent over a channel, or retained across compaction. Strings preserve UTF-16
indexing; property keys are interned atoms. Every object is an ordinary
object — prototype, extensibility, and an insertion-ordered property table —
and every walk of a prototype chain is bounded. The details are in
[values.md](values.md), [strings-and-heap.md](strings-and-heap.md), and
[objects.md](objects.md).

## Heap and collection

The isolate owns one contiguous, caller-supplied arena and a handle table with
hard maxima; allocation never falls back to a system heap. Collection is an
incremental mark-and-compact driven in bounded slices at safe points, charged
to the same budget as instructions, with roots enumerated from the frames,
registers, realm, atoms, queues, and pending calls. Handle generations retire
rather than wrap, so a stale handle cannot become valid again. The collector
is specified in [collection.md](collection.md).

## Execution

The machine is rebuilt over module-owned storage every step and restores its
saved state, which is what lets one task span many bounded steps without a
thread. A JavaScript call pushes a frame rather than recursing on the host
stack, so recursion depth is a declared bound. Jobs run one at a time to
completion; promises settle through a bounded queue; host calls leave as typed
records and return as completions that settle the promise they belong to.
Dynamic source — `eval` and the `Function` constructor — pauses the machine
for the host that carries the compiler. The interpreter is specified in
[interpreter.md](interpreter.md), the isolate contract in
[isolate.md](isolate.md), jobs in [jobs.md](jobs.md), and the host boundary in
[capabilities.md](capabilities.md).

## Metering

Fuel is charged per instruction, with collection slices and regular-expression
matching charged to the same budget, so one construct cannot hide unbounded
work behind one count. Safe points exist at function entry, calls, returns,
and loop backedges; a deadline or cancellation is observed only at safe points
and produces an uncatchable termination. Wall-clock time never defines
behaviour: the host reports time to the control block, and replay uses
recorded inputs under the same accounting, as [replay.md](replay.md)
specifies.

## Adopted engine lessons

- V8 Ignition motivates accumulator-plus-register bytecode and compact
  baseline interpretation: <https://v8.dev/docs/ignition>.
- SpiderMonkey's parser-to-GC-free Stencil separation motivates immutable
  compiled units before heap instantiation:
  <https://firefox-source-docs.mozilla.org/js/>.
- QuickJS demonstrates compact bytecode, precomputed frame bounds, atoms, and
  a small embeddable engine, while its warning that bytecode is engine-version
  bound reinforces exact digest admission: <https://bellard.org/quickjs/>.
- Hermes demonstrates the deployment value of ahead-of-time compact bytecode:
  <https://github.com/facebook/hermes>.
- ECMAScript Jobs require one active job per Agent and run-to-completion;
  Fluxor scheduling is adapted beneath that rule, not substituted for it:
  <https://tc39.es/ecma262/2024/#sec-jobs-and-host-operations-to-enqueue-jobs>.
