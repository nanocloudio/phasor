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

 SourceStream --> phasor_compile --> UnitStream --> phasor_link --> ImageStream
                    |                                      |
               Diagnostic                            Diagnostic

                              EXECUTION

 LinkedImage ----\
 Task ------------> phasor_isolate -----------------------> Result
 HostCompletion --/       |            |
                          |            `------------------> Lifecycle
                          `--> HostCall --> phasor_host_router
                                               |      |
                                      adapter request  adapter response
                                               |      |
                                     explicitly wired adapter fmods
                                               |
                                         Fluxor providers
```

Large arrows are transactional record streams, not single channel frames.
Compilation is ahead of execution. The compiler and linker remain fmods so the
same admitted transformation can run locally, on an edge appliance, or as a
managed graph. `eval` and the `Function` constructor pause the machine for the
host: a host composed with the compiler components compiles the source into a
unit of its own and enters it, and one composed without them answers the pause
with the syntax error the call would produce. A synchronous language operation
never depends on an unrelated remote compiler graph.

## Why these boundaries

### Compiler is one composite fmod

Lexing, parsing, scope analysis, bytecode emission, and verification share
source positions, atom ids, scope ids, and a temporary syntax arena. Splitting
them into `lexer.fmod -> parser.fmod -> emitter.fmod` would create a large
private wire language, copy ephemeral trees through channels, and make
ECMAScript's context-sensitive lexical decisions depend on message boundaries.

`phasor_compile` is therefore a composite module. Its internal components have
message-shaped seams and private state, but compilation state never leaves the
fmod until it is an immutable `CompiledUnit` or `Diagnostic`.

### Isolate is one composite fmod

The interpreter, realms, environments, object heap, garbage collector,
execution contexts, Promise jobs, and pending host calls share object identity
and ordering. Separating GC or jobs into other fmods would require raw pointers
or distributed stop-the-world coordination. Both are rejected.

The isolate yields physically at bytecode safe points while retaining the
active ECMAScript job. During such a yield it may flush already-committed output
or perform an admitted GC slice, but it does not dequeue another task or apply
a host completion. This preserves JavaScript run-to-completion over Fluxor's
cooperative scheduler.

### ECMAScript modules are not fmods

An ECMAScript module is a language record, not a deployment fault boundary.
Modules may be cyclic, share live bindings, execute in one Realm, and exchange
object identity. `phasor_link` places their immutable code and instantiation
plan in one `LinkedImage`; `phasor_isolate` creates their live environments.

An application may choose several isolate fmods for real isolation. Values then
cross using bounded structured-clone frames; functions, prototypes, weak
references, and ordinary object identity never cross.

## Scheduling contract

Each `module_step` has independent ceilings for bytecode instructions, input
bytes, output bytes, GC work, jobs created, and host-call transitions.

Isolate lifetime and JavaScript execution are separate state machines:

```text
isolate: Empty -> Loading -> Ready -> Terminating -> Done
                    |        `---------------------> Faulted

agent:   Quiescent -> RunningJob <-> RunningJobYielded -> Checkpoint
            ^                                            |
            `--------------------------------------------'
```

- `RunningJob -> RunningJobYielded` is a physical scheduling yield, not a JavaScript job
  boundary.
- An asynchronous host binding returns a Promise and lets the current Job
  complete. Its bounded pending-call record survives in the isolate. The
  completion enqueues the binding-defined Job; it never resumes an active Job.
- A task request may narrow graph-admitted fuel, heap, and deadline maxima but
  can never widen them.
- State advances only after the corresponding channel write commits.

Promise jobs live in a bounded FIFO inside the isolate. Fluxor channels deliver
host events; they do not become the Promise job queue. At the end of a host task,
the isolate performs an ECMAScript Job checkpoint and drains jobs in specified
order. If physical slice fuel expires, the same Job resumes before any input is
handled. Between Jobs the isolate must service admitted completion and control
inboxes even when a result outbox is backpressured. Pending calls are cancelled
or detached during termination according to their binding contract.

Each host task source has a graph-configured priority and FIFO. Selection among
sources is deterministic for a given profile. Completion arrival order is an
input to Connected profiles and is recorded by Deterministic profiles.

## Authority and host calls

JavaScript sees host bindings described by the linked image. A binding maps a
namespace and method id to a typed `HostCall` schema and a declared authority.
It never contains a syscall number, device handle, URL credential, or provider
address.

`phasor_host_router` derives caller identity and its binding grants from the
input port and graph configuration. It rejects identity claims in frames,
validates `(binding, method, request)` against that port's admitted table, and
routes to one statically wired adapter port. The router
does not dynamically discover providers and cannot widen authority. Each
adapter:

- owns exactly one semantic binding family;
- validates request and response bounds;
- invokes only its declared Fluxor capability;
- preserves request id and trace context; and
- returns a typed completion rather than throwing across the channel.

The router assigns correlation slots with generation counters, applies
per-port and per-binding quotas, and accepts a response only from the adapter
port that owns that slot. Direct isolate-to-adapter wiring is preferred when
multiplexing is unnecessary.

Synchronous language operations never wait across a channel. The time adapter
answers an admitted clock binding; the entropy adapter answers an admitted
seed binding. Neither fact is ambient: a program with no wired binding has no
clock and no randomness, which is why the realm carries no `Date` and no
`Math.random`.

Phasor may supply these fact adapters. HTTP, messaging,
storage, identity, and application-domain adapters belong to the projects that
own those meanings.

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
admission, and admission verifies the digest the artefact carries before
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
| Invalid or mismatched bytecode | `ArtifactRejected`; isolate remains unloaded |
| JavaScript throw | Catchable language completion |
| Stack limit required by language semantics | Catchable `RangeError` where conformant |
| Fuel, deadline, cancellation, or hard heap quota | Uncatchable `Terminated` |
| Bounded queue full | Channel backpressure; no input consumed |
| Adapter rejection | Binding-defined rejected completion |
| Broken module invariant | Module fault with stable reason; no partial result |

Administrative termination is uncatchable so a script cannot defeat policy by
catching and retrying it.

## Deployment profiles

- **Core:** compiler, linker, isolate, router; no dynamic source, WeakRef,
  FinalizationRegistry, SharedArrayBuffer, Atomics, WebAssembly, or Intl.
- **Deterministic:** Core plus pinned inputs; time and entropy absent or replayed;
  all host completion order recorded.
- **Connected:** Core plus explicitly selected adapter fmods.
- **Dynamic source:** an isolate composed with the compiler components, so
  `eval` and `Function` compile in place; never an ambient `eval` service.

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
    phasor_eval/                the smallest end-to-end evaluation path
    phasor_time/                standard clock adapter fmod
    phasor_entropy/             standard entropy adapter fmod
  fixtures/
    phasor_*_probe/             per-area language and machine assertions
    phasor_fault_host/          host completion fault injector fmod
    phasor_test262/             Test262 front-end oracle fmod
    phasor_run262/              Test262 execution oracle fmod
tests/                          shadow graph scenarios and orchestration only
examples/                       shadow runnable graph compositions
```

Test scripts may start graphs and compare process status, but contain no parser,
VM, heap, oracle, or language semantics. Those responsibilities are fixture
fmods, so tests exercise the same execution substrate as deployments.
CI rejects any `Cargo.toml` or `Cargo.lock` anywhere in the repository.
