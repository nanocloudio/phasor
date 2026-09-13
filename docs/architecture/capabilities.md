# Capability Bindings

Source: `modules/common/binding.rs`, `modules/common/capability.rs`,
`modules/common/vm/host.rs`.

This document defines how a program reaches anything outside itself. The engine
holds the seam; the router that admits and correlates a call, and the adapters
that answer one, are separate fmods on the graph — `phasor_host_router`, and
`phasor_time`, `phasor_entropy`, or in tests `phasor_fault_host`.
`examples/capabilities` wires them together.

This is why there is no `Math.random` and why `Date` cannot tell the time: a
program that could help itself to the clock or to unpredictability would have
authority nobody granted it. `Date` does arithmetic on time values and answers
the epoch for "now". A deployment that wants a program to have either wires
the adapter that provides it, under a quota, and the program reaches it through
the binding it was granted and through nothing else.

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

## 1a. The three layers

The language is one implementation and never varies. A capability is a typed
binding an adapter serves, named as
[the register](../reference/capability-register.md) says. Between them is the
façade: the JavaScript a program actually calls, which is a program itself —
`modules/common/facade.rs` holds its source, and a host compiles and runs it
in the realm before the program. `console` is there, and `fetch` would be, and
neither is engine code. A façade reaches the outside through the same admitted
bindings a program does, so it can acquire nothing on a program's behalf.

## 1b. What an image requires, and what a deployment grants

An image states what it needs. `import { read } from "phasor:store/keyvalue"`
is an ordinary import with a scheme, so the requirement is recorded in the
image's own import table, where nothing outside the image can claim one for
it. A deployment reads those requirements when it admits the image, checks
each against what it granted, and refuses the image whole with
`binding-not-granted` rather than starting a program that discovers its
capability is `undefined`.

The name both ends agree on is the digest of `<interface>#<member>`, with the
interface carrying its scheme. An interface is a namespace of members, and
each member is one binding: `read` and `write` on `phasor:store/keyvalue` are
granted separately, so a deployment can give a program reading without
writing. [The register](../reference/capability-register.md) holds the
names and the grammar they are held to.

A granted requirement also resolves: the import names the binding that serves
it, so the table the gate checks against and the table the image's imports
resolve against are one table. Admitting an image for a capability it could
not then reach would answer the requirement with the `undefined` the check
exists to prevent.

## 2. A call is asynchronous by construction

Calling a binding does not wait. The engine records the call, hands the program
a promise, and returns; the call record goes into an outbox the host drains. A
synchronous language operation therefore never blocks a module step, and the
engine never needs a thread.

The call record carries a correlation identifier, the binding index, a trace
context, and the digest and length of the payload the caller staged. It carries
no pointer, no address, and no credential, so what crosses the boundary is
exactly what the deployment can see and check. On the wire the record is 56
bytes with explicit fields and a completion is 32; the payload's bytes follow
the frame on the same port, and the digest in the record is what makes them
the caller's bytes rather than whatever arrived.

## 2a. What a provider answers with

A completion answers with nothing, a number, bytes, or a resource. Bytes
travel behind the frame as a call's payload does. A resource is a handle: an
index and a generation the binding table issues and checks, meaningful only
to the capability that opened it. A program holds a handle and passes it back;
the engine resolves it to the provider's own identifier, which never reaches
the program. A handle from another binding, one that was released, or one a
program invented resolves to nothing, so a forged handle names nothing rather
than naming someone else's resource. Releasing a handle advances its slot's
generation, which never wraps.

## 2b. Not everything is a call

A binding declares how it is served. An asynchronous binding returns a promise
and completes through a later job, which is every provider reached over a
channel. A snapshot binding is not called at all: the host supplies its value
at the task boundary, before any of the program runs, and reading it is local
and answers at once.

That is what lets a clock be a capability without a channel round trip. A
program that was granted one reads `clock.now()` and gets a number, not a
promise, and the value is the one the adapter supplied for that task. A
synchronous language operation still never waits on a channel.

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
