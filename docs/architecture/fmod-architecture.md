# Fmod Architecture

## Design rules

1. Every executable product, tool, adapter, and conformance component is an
   `.fmod`. There are no Cargo crates in the Phasor design.
2. An fmod boundary requires independent lifecycle, authority, placement, or
   backpressure. It is not a substitute for a Rust module boundary.
3. No JavaScript object reference crosses an fmod boundary. Cross-boundary data
   is immutable or value-serialized.
4. One isolate fmod owns one ECMAScript Agent. Fluxor may interleave physical
   steps, but no other JavaScript job enters that Agent before the active job
   completes.
5. All queues, arenas, tables, frames, compilation units, imports, jobs, and
   provider calls have admitted maxima.
6. No fmod receives ambient filesystem, network, process, environment, clock,
   entropy, identity, or storage authority.

## Canonical graph

```text
                         AUTHORING / ADMISSION

 source ──▶ phasor_compile ──▶ unit image ──▶ phasor_link ──▶ linked closure
                 │
            diagnostic

                              EXECUTION

 unit image or linked closure ──▶ phasor_isolate ──▶ result, diagnostic, exit
 control records ────────────────▶      │  ▲
                                        │  └── completion ◀── phasor_host_router
                                        └───── call ───────▶        │      ▲
                                                             request│      │reply
                                                                    ▼      │
                                                          explicitly wired adapter fmods
                                                                    │
                                                            Fluxor providers
```

Every arrow is a stream of records on a channel of its own, not a single
frame. Compilation is ahead of execution. The compiler and linker are fmods so
the same admitted transformation can run locally, on an edge appliance, or as
a managed graph. `eval` and the `Function` constructor pause the machine for
the host: a host composed with the compiler compiles the source into a unit of
its own and enters it, and one composed without it answers the pause with the
syntax error the call would produce. A synchronous language operation never
depends on an unrelated remote compiler graph.

## Why these boundaries

### Compiler is one composite fmod

Lexing, parsing, scope analysis, bytecode emission, and verification share
source positions, atom ids, scope ids, and a temporary syntax arena. Splitting
them into `lexer.fmod -> parser.fmod -> emitter.fmod` would create a large
private wire language, copy ephemeral trees through channels, and make
ECMAScript's context-sensitive lexical decisions depend on message boundaries.

`phasor_compile` is therefore a composite module. Its internal components have
message-shaped seams and private state, but compilation state never leaves the
fmod until it is an immutable unit image or a `Diagnostic`.

### Isolate is one composite fmod

The interpreter, realms, environments, object heap, garbage collector,
execution contexts, Promise jobs, and pending host calls share object identity
and ordering. Separating GC or jobs into other fmods would require raw pointers
or distributed stop-the-world coordination. Both are rejected.

The isolate yields physically at bytecode safe points while retaining the
active ECMAScript job. During such a yield it may flush already-committed output
or perform an admitted GC slice, but it does not start another task or apply
a host completion inside a job. This preserves JavaScript run-to-completion
over Fluxor's cooperative scheduler.

### ECMAScript modules are not fmods

An ECMAScript module is a language record, not a deployment fault boundary.
Modules may be cyclic, share live bindings, execute in one Realm, and exchange
object identity. `phasor_link` places their immutable code and evaluation
order in one linked closure; `phasor_isolate` creates their live environments.

An application may choose several isolate fmods for real isolation. Values then
cross using bounded frames; functions, prototypes, weak references, and
ordinary object identity never cross.

## Scheduling contract

Each `module_step` does a bounded amount of work: at most one staged read or
write per port, at most one slice of instructions, at most one bounded run of
jobs, at most one collection slice. The state that carries a task from one
step to the next lives in the module's own storage, and the machine is rebuilt
over it on every step — see [isolate.md](isolate.md).

An isolate's task moves through the lifecycle `policy.rs` defines — `Empty`,
`Ready`, `Running`, `Suspended`, `Idle`, `Stopped` — and its module steps
through four phases: staging the image, advancing the task, publishing the
result and its diagnostic, and done. A task ends with one of the closed set of
outcomes in [isolate.md](isolate.md); a physical yield is not one of them.

- A physical yield is a scheduling event, not a JavaScript job boundary.
- A host binding returns a Promise and lets the current job complete. Its
  bounded pending-call record survives in the isolate. The completion settles
  the promise, and any reaction runs as a job; it never resumes an active job.
- A graph may narrow the fuel and the call-wait ceilings the isolate compiles
  in but can never widen them.
- State advances only after the corresponding channel write commits.

Promise jobs live in a bounded FIFO inside the isolate. Fluxor channels deliver
host events; they do not become the Promise job queue. After each slice the
isolate runs a bounded number of queued jobs in order, and a job that throws
settles the promise derived from it rather than ending the task. Between
slices the isolate drains its control and completion inboxes before it
advances, so a cancellation or an answer is seen at the next safe point even
when the result outbox is backpressured. Pending calls that were never answered
are answered by the isolate itself with a timeout.

## Authority and host calls

JavaScript sees host bindings the isolate admitted: each is a name digest, a
schema digest, and an in-flight limit. A binding never contains a syscall
number, device handle, URL, credential, or provider address.

`phasor_host_router` admits a call by its binding index alone, correlates the
answer with the call by request identifier and trace context, and bounds how
many calls may be outstanding. It rejects identity claims in frames, accepts a
reply only for a call it forwarded, and routes to one statically wired adapter
port. The router does not dynamically discover providers and cannot widen
authority. Each adapter:

- owns exactly one semantic binding family;
- validates request and response bounds;
- invokes only its declared Fluxor capability;
- preserves request id and trace context; and
- returns a typed completion rather than throwing across the channel.

Direct isolate-to-adapter wiring is preferred when multiplexing is unnecessary.

Synchronous language operations never wait across a channel. The time adapter
answers an admitted clock binding; the entropy adapter answers an admitted seed
binding. Neither fact is ambient: a program with no wired binding has no clock
and no randomness. The realm's `Date` can do arithmetic on time values but
cannot tell the time — "now" is the epoch until a clock is granted — and
`Math` carries no `random` unless a host installs one.

Phasor supplies these fact adapters. HTTP, messaging, storage, identity, and
application-domain adapters belong to the projects that own those meanings.

## Artifact transport

Every variable-size artefact — a source, a unit image, a linked closure, a
result — travels as one ordered stream on a channel of its own, ended by the
writer's hang-up. The channel gives the transport its guarantees: one writer,
ordered delivery, and atomic record writes, so a backpressured writer retries
an entire record rather than a suffix, and partial reads are staged until a
whole record exists.

The receiver declares its capacity and fails closed: a stream larger than the
declared maximum is drained and refused, never truncated into a shorter
artefact. Nothing acts on a partial stream — an image is staged whole before
admission, and admission verifies the digests the artefact carries before
anything runs, so an artefact that changed in transit is refused by identity
rather than trusted by arrival. Fixed-size records — call frames, completion
frames, diagnostic frames, control records — are read whole for the same
reason: a partial record is not a record.

Compiler and linker construction state may advance before output; only
publication crosses a channel.

## Failure model

| Failure | Outcome |
|---|---|
| Invalid source | `Diagnostic`; no compiled artefact |
| Invalid or mismatched bytecode | `ImageRejected`; nothing runs |
| JavaScript throw | Catchable language completion; uncaught, the task ends `Threw` |
| Stack limit required by language semantics | Catchable `RangeError` where conformant |
| Fuel, deadline, cancellation, or hard heap quota | Uncatchable termination |
| Bounded queue full | Channel backpressure; no input consumed |
| Adapter rejection | Binding-defined rejected completion |
| Broken module invariant | Module fault with stable reason; no partial result |

Administrative termination is uncatchable so a script cannot defeat policy by
catching and retrying it.

## Deployment profiles

- **Core:** compiler, linker, isolate, router; no `Atomics`, `Intl`,
  `FinalizationRegistry`, or WebAssembly, and no source compiled at run time.
- **Deterministic:** Core plus pinned inputs; time and entropy absent or
  replayed; every host completion and reported time recorded — see
  [replay.md](replay.md).
- **Connected:** Core plus explicitly selected adapter fmods.
- **Dynamic source:** an isolate composed with the compiler, so `eval` and
  `Function` compile in place; never an ambient `eval` service.

Profiles are graph compositions and module variants, not Cargo features or
alternate host runtimes.

## Repository shape

```text
modules/
  common/                       shared no_std source; never executable alone
  app/
    phasor_compile/             composite compiler fmod
    phasor_link/                immutable module linker fmod
    phasor_isolate/             VM + heap + GC + realms + jobs fmod
    phasor_host_router/         admitted host-call routing fmod
    phasor_cli/                 the human edge: results and rendered diagnostics
    phasor_eval/                the bounded expression evaluator
    phasor_shell/               the shell: the engine as a command, over the cli stack
    phasor_time/                standard clock adapter fmod
    phasor_entropy/             standard entropy adapter fmod
  fixtures/
    phasor_*_probe/             per-area language and machine assertions
    phasor_fault_host/          host completion fault injector fmod
    phasor_test262/             Test262 front-end oracle fmod
    phasor_run262/              Test262 execution oracle fmod
packaging/cli/                  the shell's applet graph and workload manifest
tests/                          shadow graph scenarios and orchestration only
examples/                       shadow runnable graph compositions
```

Test scripts may start graphs and compare process status, but contain no parser,
VM, heap, oracle, or language semantics. Those responsibilities are fixture
fmods, so tests exercise the same execution substrate as deployments.
CI rejects any `Cargo.toml` or `Cargo.lock` anywhere in the repository.
