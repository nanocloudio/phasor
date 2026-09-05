# Jobs and Promises

Source: `modules/common/job.rs`, `modules/common/promise.rs`,
`modules/common/vm/host.rs`, `modules/common/vm/coroutines.rs`.

This document defines how work is deferred and how a promise settles: the
queue, the promise state machine, `then`, the `Promise` constructor, its
statics, and the adoption of a thenable. An async function is a coroutine
over the same machinery: `await` records a reaction on the awaited promise and
suspends the frame, and the reaction's job resumes it.

## 1. The queue

Jobs run one at a time, to completion, in the order they were enqueued. The
queue is a bounded ring in caller-provided storage: it never grows, and a full
queue is an ordinary failure, because the number of jobs a task may create is
part of what a deployment admits.

The machine runs jobs only when a host asks it to, and a host asks between
slices, after the running job has finished or yielded at a safe point. Nothing
runs a job in the middle of an expression, which is what makes the ordering a
program observes the specification's rather than an artefact of when the
engine happened to look.

## 2. Promises

A promise is an ordinary object with three more internal slots: its state, the
value it settled with, and the reactions waiting on it. It settles once; a later
attempt changes nothing and says so, which is what the specification's
already-resolved flag does.

`then` records a reaction and returns the promise that receives the handler's
result. When the promise is already settled, the reaction becomes a job
immediately, so a handler still never runs during the call that attached it.
When it is pending, the reaction is recorded and becomes a job when the promise
settles.

A reaction with no handler passes the outcome through: the derived promise
settles with the same value and the same state. That is what makes `then()` with
no arguments a well-defined link in a chain.

A job that throws settles the promise derived from it as rejected. Nothing else
observes the throw, which is what keeps one job's failure from ending the task.

## 3. The constructor

`new Promise(executor)` calls the executor with two functions that settle the
new promise. Each carries the promise it settles on the function object itself,
because it is called as a plain function and has no receiver to carry it. An
executor that throws rejects the promise, and an executor that settles nothing
leaves it pending for ever, which is exactly what the language says.

`Promise.resolve` and `Promise.reject` produce an already-settled promise.
`Promise.withResolvers` produces a pending one with its two settling functions
beside it. `Promise.all`, `allSettled`, `race`, and `any` combine an iterable
of promises into one, each with the specification's ordering and its own
rejection rule; `catch` and `finally` on the prototype are `then` with one
handler fixed.
`Promise.withResolvers` produces a pending one with its two settling functions
beside it. `Promise.all`, `allSettled`, `race`, and `any` combine an iterable
of promises into one, each with the specification's ordering and its own
rejection rule; `catch` and `finally` on the prototype are `then` with one
handler fixed.

Resolving is not settling. A promise resolved with a value that has a callable
`then` follows it rather than holding it: a job is queued that calls that `then`
with functions which settle the promise that adopted it. That is what makes a
handler returning a promise chain, and what makes an ordinary object with a
`then` behave like one. A promise resolved with itself can never settle, so it
rejects with a type error instead of waiting for ever.

## 4. What a host must provide

A machine has no queue unless a host attaches one. Without it, promises have
nowhere to schedule, and the operations that would need one stop the task rather
than running a handler at the wrong time.
