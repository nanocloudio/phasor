# Capability Bindings

Source: `modules/common/binding.rs`, `modules/common/vm.rs`.

This document defines how a program reaches anything outside itself. The engine
holds the seam; the router that admits and correlates a call, and the adapters
that answer one, are separate fmods on the graph — `phasor_host_router`, and
`phasor_time`, `phasor_entropy`, or in tests `phasor_fault_host`.
`examples/capabilities` wires them together.

This is why there is no `Math.random` and no `Date`: a program that could help
itself to the time or to unpredictability would have authority nobody granted
it. A deployment that wants a program to have either wires the adapter that
provides it, under a quota, and the program reaches it through the binding it
was granted and through nothing else.

## 1. Nothing is ambient

A program has no clock, no randomness, no storage, no network, and no host
object. It reaches the outside only through a binding the deployment admitted,
and an admitted binding is reachable only under the name the deployment gave it.
A name that was not granted resolves to nothing, and using it is a reference
error rather than a quiet failure.

A binding names what it is and what schema its payloads follow, both as digests,
and how many calls may be outstanding on it at once. The engine holds no
provider address, no credential, and no transport handle, so there is nothing
for a program to reach even if it could name one.

## 2. A call is asynchronous by construction

Calling a binding does not wait. The engine records the call, hands the program
a promise, and returns; the call record goes into an outbox the host drains. A
synchronous language operation therefore never blocks a module step, and the
engine never needs a thread.

The call record carries a correlation identifier, the binding index, a trace
context, and the digest of the payload the caller staged. It carries no pointer,
no address, and no credential, so what crosses the boundary is exactly what the
deployment can see and check. On the wire it is 56 bytes with explicit fields;
a completion is 32.

## 3. Completions

A host answers a call by its correlation identifier, saying whether the provider
answered or refused. That settles the promise the program is holding, and any
reaction on it runs as a job, so a completion never runs program code at the
moment it arrives.

An identifier is answered once. A repeated or unknown completion is refused
rather than settling something twice, and a settled promise stays settled. A
completion must also carry back the trace the call went out under: one that
does not match is refused, so a correlation holds on both fields or not at all.

A refusal is typed. The cause says whether the provider denied the call, was
unavailable, timed out, was handed something malformed, failed internally, or
was busy, and whether the same call is worth making again. The program sees an
ordinary error carrying `cause` and `retryable`, so it can tell a refusal from
a failure without parsing text.

## 3a. A call that is never answered

A provider that never answers must not hold a task open. An isolate that has
waited long enough answers its own outstanding calls with a timeout, and the
program sees an ordinary rejection. A graph that wired no call port at all is
the same case decided immediately: the calls are refused as unavailable rather
than staged for a port that does not exist.

## 4. Quotas

A binding's in-flight limit is what bounds the work a program can ask for. A
call over the limit rejects its promise rather than throwing: exceeding a quota
is a condition the program can handle, not a defect in it. A full pending table
or a full outbox is an isolate-level quota, and those end the task.

The promises of outstanding calls are collection roots. Nothing else refers to
them until their completion arrives, and a collection that dropped them would
lose the program's only handle on work it is waiting for.
