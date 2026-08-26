# Isolate Lifecycle and Resource Policy

Source: `modules/common/policy.rs`, `modules/common/vm.rs`.

This document defines what an isolate is admitted against, the states it moves
through, the outcomes a task can end with, and how a task is stopped.
`phasor_isolate` takes an image on one port, answers on another, puts its
calls on a third, and says on a fourth why a run produced no value.

## 1. One isolate is one Agent

An isolate owns one ECMAScript Agent: one heap, one execution stack, one job
queue, and its realms. Only one task runs at a time, and a physical yield does
not let another task in, because run-to-completion is a language guarantee
rather than a scheduling preference.

## 2. Policy

An isolate is admitted against a policy: heap bytes and cells, instructions,
frames, registers, jobs, outstanding host calls, image bytes, a wall-clock
deadline, and the size of a collection slice. Each field is a hard maximum for
one task.

Every field has a compiled-in ceiling. A deployment may lower any field and can
never raise one, so no configuration can widen what the build admits. Admission
proves once that the storage handed to the isolate satisfies the policy; after
that, no step has to check whether its own storage is large enough, because the
inequality has already been established.

## 3. States

```text
Empty ──▶ Ready ──▶ Running ──▶ Idle ──▶ Running
             ▲          │  ▲       │
             └──────────┘  └───────┘
                        Suspended
```

`Empty` has storage but no image. `Ready` has a verified image. `Running` has a
task in progress. `Suspended` is a task stopped at a safe point waiting for a
host completion, which is the only way a task waits. `Idle` has finished a task
and can take another. `Stopped` admits nothing further. The transitions above
are the whole set; anything else is a rejection rather than an undefined state.

## 4. Outcomes

A task ends with exactly one outcome, and the set is closed:

| Outcome | Meaning | Catchable | Isolate may run again |
|---|---|---|---|
| `Returned` | The program produced a value | yes | yes |
| `Threw` | The program threw and did not catch it | yes | yes |
| `FuelExhausted` | The instruction budget ran out | no | yes |
| `DeadlineReached` | The wall-clock deadline passed | no | yes |
| `Cancelled` | The host asked for the task to stop | no | no |
| `HeapExhausted` | An allocation could not be satisfied inside the quota | no | no |
| `StackOverflow` | The call stack reached its admitted depth | no | no |
| `QuotaExceeded` | A bounded table, queue, or buffer was full | no | no |
| `ImageRejected` | The image did not pass admission | no | no |

The administrative outcomes are uncatchable on purpose. A script that could
catch its own cancellation could defeat the policy by catching and continuing,
so the language's exception machinery never sees them.

Exhausting fuel or a deadline leaves the isolate able to run another task,
because neither says anything is wrong with the isolate itself. Exhausting the
heap or the stack does not, because the state that ran out is the isolate's own.

## 4a. An outcome is not a fault

Every outcome in the table above is something the isolate was asked to produce.
A program that ran out of fuel, overflowed its stack, threw, or was refused has
been executed correctly — the isolate is not the thing that failed. So the fmod
completes normally in each of those cases, writes the reason as a diagnostic
frame on its `diagnostic` port, and writes a non-zero exit status. A module
error is reserved for the isolate itself being at fault, and reporting one
instead would fault the module, stop the graph, and leave whatever was waiting
downstream waiting forever.

The diagnostic port carries numbers: a code from the `0x0500` range (see
`../reference/diagnostics.md`), a severity, and a position. Turning those into
a sentence is `phasor_cli`'s job and no one else's. It is a port of its own
rather than a second writer on the compiler's, because a port has one writer and
which phase spoke is worth keeping.

## 4b. What bounds a program

The instruction budget is a parameter, `steps`, rather than a constant: a graph
that runs bigger programs says how much bigger. The frame and register tables
are compiled in, and they are what a program's recursion depth is bounded by —
a JavaScript call is a frame in this table, never a host stack frame, so a
runaway recursion is `StackOverflow` at a declared depth rather than a stack
that ran into something.

## 5. Slices, cancellation, and deadlines

A task runs in slices. `resume` runs at most the number of instructions it is
given and returns either the task's outcome or the fact that the task is still
running, with its whole state in the machine. That is what lets one task span
several module steps without a thread and without a host stack.

Cancellation and the deadline are observed only at safe points: a backward edge,
a call, and a return. A task therefore never stops in the middle of an
operation, and a loop is always interruptible, because the verifier has already
proved that every backward edge lands on a declared safe point.

The engine never reads a clock. The host reports the current time to the control
block and sets the deadline in its own units, and the engine compares the two.
Time stays a capability rather than an ambient power, and the comparison is
deterministic for a replay.

A slice ending is not an outcome: the task has not finished and nothing has been
decided about it. Fuel exhaustion, cancellation, and a passed deadline are
outcomes, and they are uncatchable.

## 5a. The machine is rebuilt, not held

A task that is waiting on the outside must survive between module steps, and a
machine cannot be held across them: it borrows the storage that lives in the
module's own state. So it is rebuilt. The heap, the atom table, the job queue,
the binding table, and the machine each save the state that is not in their
storage, and each can take up storage they were already using without clearing
it — the slot table is the handle table, and clearing it would invalidate every
handle the module still holds.

What this buys is that a step is bounded by its own slice rather than by how
long an answer takes, and that a task waiting for a completion costs nothing
while it waits.

## 6. What a step must not do

- No step may allocate outside the admitted arena, or grow a table beyond its
  admitted size.
- No step may run unbounded work: a long operation yields at a safe point and
  resumes from isolate-owned state.
- No outcome may be a panic. Every limit named here has a value in the table
  above, and reaching it produces that value.
