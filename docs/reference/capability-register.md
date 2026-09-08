# Capability Register

Source: `modules/common/binding.rs`, `modules/common/capability.rs`.

Every capability a program can reach is listed here. An interface is granted
as a whole and admitted a member at a time, so a deployment gives reading
without writing by granting one binding and not the other.

## How a capability is named

A binding's identity is the digest of `<interface>#<member>`. The interface
follows the [WebAssembly System Interface](https://github.com/WebAssembly/WASI)
where an interface for the thing exists, so an adapter written to it means
something outside this project and a schema has a published definition to be
the digest of. Where no such interface exists the name carries the `phasor:`
prefix and is this project's own.

An image states what it requires through an import carrying the `phasor:`
scheme, and the deployment checks each requirement against what it granted
before anything runs. A capability that was not granted is
`binding-not-granted` at admission, never `undefined` at the call.

## The interfaces

| Interface | Member | Class | Adapter | What it grants |
|---|---|---|---|---|
| `wasi:clocks/wall-clock` | `now` | snapshot | `phasor_time` | The time, in milliseconds, as of the task boundary |
| `wasi:clocks/monotonic-clock` | `sleep` | async | `phasor_time` | Being told later, which is what a timer is built on |
| `wasi:random/random` | `random` | async | `phasor_entropy` | A number the platform's own source produced |
| `wasi:filesystem/types` | `read` | async | `phasor_fs` | The bytes of a file under the root |
| | `write` | async | | Bytes into a file under the root, created if absent |
| | `open` | async | | A handle over a file under the root |
| | `readAt` | async | | Bytes through a handle, continuing where it left off |
| | `close` | async | | Releasing a handle |
| | `size` | async | | A file's length in bytes |
| `phasor:store/keyvalue` | `read` | async | `phasor_store` | The bytes held under a key |
| | `write` | async | | Bytes under a key, within the store's capacity |
| | `list` | async | | The keys the store holds |
| | `delete` | async | | Removing a key |
| | `open` | async | | A handle over an entry |
| | `readAt` | async | | Bytes through a handle |

The shell names each interface by a short namespace a program calls it
through: `clock`, `entropy`, `fs`, `store`. That name is the JavaScript
surface and has nothing to do with admission; the identifier above is what
the deployment grants and what the digest covers.

## How each class is served

**Snapshot.** The host supplies the value at the task boundary, before any of
the program runs, and reading it is local. That is what lets a clock be a
capability and still answer at once, with no channel round trip inside a
synchronous language operation.

**Asynchronous.** The call returns a promise and completes through a later
job. Bytes travel behind the fixed frame in both directions, and a provider
that answers with a resource answers with a handle the issuing binding
checks.

## Where the façade lives

A façade is a module of the closure, linked ahead of the program. Nothing
new was needed for that: the isolate already runs linked closures, so a
deployment compiles its surface, links it first, and the program imports from
it. `tests/e2e/closure.sh` proves it end to end.

The shell is the special case, because it runs scripts rather than closures:
it carries the façade's source in its own image and compiles it at session
start. That is what the module ceiling binds — the loader admits a megabyte of
code per module and the shell is within a few hundred bytes of it — so the
shell's surface cannot grow, while a closure's can.

## What is not here

There is no network interface, so there is no `fetch`. There is no process,
environment, or subprocess interface. There is no interface for spawning
another isolate. Each is absent rather than half-present: a program finds
nothing, and a deployment cannot grant what does not exist.

## Adding one

A new capability is four things, in this order: an entry in this register
with its identifier and class; an adapter fmod that serves it and declares
whatever Fluxor contract it needs in its own manifest; a grant in the host
that admits it; and three tests — a probe for the mechanism, a lane through
a real graph, and a lane proving the ungranted case fails closed. The third
is not optional: a capability whose refusal is untested is a capability whose
refusal is a guess.
