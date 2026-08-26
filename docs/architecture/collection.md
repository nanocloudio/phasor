# Bounded Heap and Collection

Source: `modules/common/heap.rs`, `modules/common/gc.rs`.

This document defines how the heap reclaims memory. Collection is mark-compact
and runs in slices, so a collection can be spread over several module steps and
charged to each of their budgets.

## 1. Why compaction is cheap here

A cell is addressed only through its slot: nothing anywhere holds its address.
Moving a cell therefore rewrites one offset in the handle table and nothing
else. There is no reference to fix up, no remembered set, and no pinning, which
is what makes a compacting collector the simple choice rather than the ambitious
one.

Each cell carries an eight-byte header naming its slot and its length. That is
what lets the compactor walk the arena in address order without sorting the
handle table.

## 2. Slices, and why there is no write barrier

A collection has two phases. Marking starts from the caller's roots and follows
every reference; each slice visits a bounded number of cells. Compaction then
walks the arena once, moving live cells down and discarding the rest; each slice
moves a bounded number of bytes.

The mutator does not run between slices. A collection begins at a safe point and
finishes before execution resumes, so no reference can be created while marking
is in progress and no write barrier is needed. A variant that did interleave
execution with marking would need a Dijkstra insertion barrier and the shading
it implies; that is a separate design, not a tuning of this one.

Nothing may be allocated while a collection is in progress. An allocation
attempted then is refused with a distinct result rather than being served from
a region the compactor is about to move.

## 3. When a collection happens

A host that attaches collection storage to a machine gives it a slice size and a
headroom: the free arena below which the machine collects rather than waiting to
fail. The machine then collects at an instruction boundary in its outermost
loop, which is the only place where every live value is in the accumulator, a
register, a frame, the realm, the interned names, or an outstanding call. A
nested evaluation holds values on the interpreter's own stack that the roots do
not name, so no collection happens there.

The work is charged to the same budget as instructions, so a program that makes
a collection necessary pays for it. A collection always finishes once started: a
half-marked or half-compacted heap is not a heap.

A machine with no collection storage does not collect. It runs until the arena
is full and then reports that, which is what a host that attached nothing asked
for.

## 4. Roots

The interpreter's roots are its accumulator, every register of every live frame,
each frame's environment and receiver, the realm's global object, environment,
and intrinsic prototypes, and the interned names. A caller that drives the heap
directly supplies its own.

A register that a compiled expression has finished with still names its last
value until something overwrites it, so the compiler matters here: it takes a
register for an operand only once that operand's own subexpression has released
its registers. A long chain of operators therefore needs one register rather
than one per link, and a finished intermediate is not held alive by a register
nothing will read again.

The mark worklist is caller-provided and may be too small. When it overflows, a
cell stays grey and the collector finds it by scanning the handle table instead:
the order changes, nothing is lost, and the collection still terminates.

## 5. What a program observes

A collection is not observable to a program: it reclaims only what nothing can
reach. A handle to a reclaimed cell is stale for good, because the slot's
generation advances when it is freed and never wraps.

Exhaustion stays an ordinary result. A heap that is full after a collection
reports that the allocation did not happen; it does not fault, and it does not
take more than the arena it was given.
