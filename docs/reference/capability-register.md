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
| `wasi:sockets/tcp` | `connect` | async | `phasor_net` | A handle over a connection to the one wired endpoint |
| | `send` | async | | Bytes onto a connection |
| | `receive` | async | | Bytes off a connection, up to a length |
| | `close` | async | | Ending a connection |
| | `endpoint` | async | | What the deployment called the endpoint it wired |

The shell names each interface by a short namespace a program calls it
through: `clock`, `entropy`, `fs`, `store`, `net`. That name is the
JavaScript surface and has nothing to do with admission; the identifier
above is what the deployment grants and what the digest covers.

## How each class is served

**Snapshot.** The host supplies the value at the task boundary, before any of
the program runs, and reading it is local. That is what lets a clock be a
capability and still answer at once, with no channel round trip inside a
synchronous language operation.

Whether the transport underneath is encrypted is the deployment's to say and
no part of the interface: a graph that puts Fluxor's `tls` between the adapter
and the socket changes neither the binding nor the façade, and a program
cannot tell which graph it is running in. `examples/net/https.yaml` is that
graph, and what it will trust is stated there rather than anywhere a program
can reach.

**Asynchronous.** The call returns a promise and completes through a later
job. Bytes travel behind the fixed frame in both directions, and a provider
that answers with a resource answers with a handle the issuing binding
checks.

## What a payload is

Bytes, in both directions, and nothing more.

A string crossing the seam is one byte a character: the unit `0x41` is the
byte `0x41`, and a unit above a byte is refused rather than truncated or
re-encoded behind the program's back. The answer arrives under the same rule,
so what a program receives it can send back unchanged. A typed array crosses
as the bytes it views.

Text is a *reading* of bytes rather than what they are, so the seam does not
perform it: the surface decodes a response body, and encodes a request body,
where knowing that it is text belongs. That is why a body that is not text
survives at all — decoding at the seam would replace every byte that is not
valid UTF-8 and lose what arrived, with nothing to say it had happened.

A provider checks the shape of what it is given rather than reading it as
though it were right: a call that does not have the fields its member takes is
refused as malformed, which the frame below makes checkable.

Arguments are length-prefixed, not separated:

```text
count: u16 LE, then for each argument: length: u32 LE, bytes
```

A separator has to be a byte that cannot occur inside a field, and a call
carries both text that may hold U+0000 and bytes that may hold `0x00`, so no
such byte exists. A call with no arguments carries no payload at all.

## Where the façade lives

A façade is a module of the closure, linked ahead of the program. Nothing
new was needed for that: the isolate already runs linked closures, so a
deployment compiles its surface, links it first, and the program imports from
it. `tests/e2e/closure.sh` proves it end to end.

The shell runs scripts rather than closures and so has nowhere to link one.
It reads the surface over a port instead: `phasor_surface` writes the source
once and hangs up, and the shell compiles it through the same front end and
verifier as any other program. The surface therefore grows to that module's
room rather than the host's, which is what the loader's megabyte of code per
module would otherwise bind.

The surface builds on what is there. `fetch` exists only when the `net`
binding was granted, because it is written over that binding and the façade
declines to define a function it cannot serve. A program that was granted
nothing finds no `fetch` at all rather than one that fails at the call.

`WebSocket` is the same argument twice over. It is an HTTP request that
changes protocol, and the protocol above it is frames the surface writes and
reads, so it needs no capability of its own — but a client must mask every
frame it sends with something a peer cannot predict, so it needs the
randomness a deployment granted. Granted `net` without `entropy`, or either
without the other, there is no `WebSocket`.

## What a deployment trusts

Whether the transport under a capability is encrypted, and what it will
authenticate, is stated in the graph and nowhere a program can reach. Fluxor's
`tls` takes a posture by name — `pinned`, `ca_dns`, `ca_uri`,
`insecure_no_verify` — and that closed set is the whole vocabulary. It stays
the whole vocabulary: a posture is added as a member whose name says what it
does not authenticate, never as a flag that weakens one. `verify_hostname:
off` and `allow_expired` are how a configuration surface becomes forty
switches and a hundred reachable combinations, most of them wrong.

A posture that a target cannot honour is refused when the graph is built, not
discovered at runtime. `ca_dns` with lifetime enforcement declares that it
needs the `time.wall` capability, and silicon that does not offer one cannot
build the graph at all.

### The ledger

Four things are tolerated today so that anything works at all. Each is named,
each is held by `tests/e2e/tls.sh`, and each has the one change that deletes
it — not a setting that hides it.

| Tolerated | Why | Deleted by |
|---|---|---|
| Certificate lifetimes unchecked (`clock_policy: unchecked`) | The silicon reports no `time.wall`, so the validity window is not a check that can run | A target with a trusted clock |
| No public certificate authority reaches us | Only P-256 and ML-DSA signatures are verified; every well-known issuer signs with something else | P-384 and RSA verification |
| One trust anchor, so an issuer is pinned rather than a root trusted | The anchor is a single certificate, not a store | A multi-anchor store, with the bundle mounted by the deployment |
| A privately issued leaf with no extended key usage is refused | The certificate omits `serverAuth` | Reissuing that certificate — this one is the world being wrong, not the code |

The lane asserts the tolerations as well as the refusals: an expired
certificate is served, and it says so out loud. When a target gains a trusted
clock that line fails, which is the point of writing it down — the gap closes
loudly rather than being forgotten.

Until all four are gone, `ca_dns` does not mean the same thing on every
target, and anything relying on it says which target it means.

## What is not here

The network interface is one endpoint, and the deployment names it: the graph
gives `phasor_net` an address, a port, and optionally an authority — the name
that endpoint answers to, which a program may write in a URL and which travels
as the request's authority. `connect` takes no arguments because there is
nothing for a program to choose. `fetch` refuses a URL whose
host is not that endpoint rather than sending it there anyway. There is no
resolver, no listening socket, and no second endpoint without a second
adapter. There is no process, environment, or subprocess interface, and no
interface for spawning another isolate. Each is absent rather than
half-present: a program finds nothing, and a deployment cannot grant what does
not exist.

## Adding one

A new capability is four things, in this order: an entry in this register
with its identifier and class; an adapter fmod that serves it and declares
whatever Fluxor contract it needs in its own manifest; a grant in the host
that admits it; and three tests — a probe for the mechanism, a lane through
a real graph, and a lane proving the ungranted case fails closed. The third
is not optional: a capability whose refusal is untested is a capability whose
refusal is a guess.
