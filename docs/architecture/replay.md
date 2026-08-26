# Deterministic Replay

Source: `modules/common/replay.rs`.

This document defines what a deterministic run is, what a recording holds, and
what may not change a result.

## 1. The claim

A program that makes no provider call and runs under no deadline is a function
of its image and its policy. Nothing outside those two can reach it: it has no
clock, no randomness, no environment, and no host object, because the language
core owns none of them and the deployment grants none by default.

That claim is only worth as much as the checking of it, so a run produces a
recording: the identity of the image, the digest of the policy, everything that
arrived from outside in the order it arrived, and a digest of what the run
produced. Two runs match when all four match.

## 2. What is deliberately not recorded

Heap size, collection timing, slice size, and the number of module steps a run
took are not part of a recording. None of them may change a result, so recording
them would hide a defect rather than expose one: if a program's outcome changes
when the heap is smaller or the slices are shorter, that is a bug in the engine,
and the way to find it is to vary those and compare outcomes.

The on-graph probe does exactly that: it runs each program with a wide heap and
unbounded slices, then with a quarter of the heap and three-instruction slices,
and requires the same outcome.

## 3. What a recording holds

- The profile: deterministic, or observed when provider calls or a deadline are
  admitted.
- The image's logical digest, which is the identity of the exact bytes that ran.
- The policy digest, because a different budget can end a run differently.
- The events: a time the host reported, a completion the host supplied with the
  digest of its payload, or a cancellation.
- The outcome digest: the outcome kind and the text of the value produced, so
  two runs that built equal values in different cells agree.

A recording that saw more events than it can hold says so, and then reproduces
nothing. An incomplete recording that silently compared equal would be worse
than no recording at all.

## 4. Outcome identity

A value is identified by its printed form rather than by where it lives. Two
runs of the same program allocate different cells in different orders, which is
exactly what a collection can change, so an identity based on addresses would
report a difference where the language sees none.
