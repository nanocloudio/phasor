# Phasor Specification

## 1. Purpose

Phasor is the NanoCloudIO project's portable JavaScript engine. It turns source
or verified bytecode into bounded execution inside Fluxor graphs.

This document defines the architectural contract. The admitted language
surface is declared feature by feature and measured with conformance tests;
this page is about what every feature is held to, whatever the surface.

## 2. Ownership boundary

Phasor owns:

- ECMAScript lexical, syntactic, and execution semantics;
- values, objects, functions, environments, exceptions, and jobs;
- verified Phasor bytecode and its content identity;
- isolate-local heap management and garbage-collection policy;
- deterministic instruction accounting and language diagnostics; and
- the typed bridge between script imports and admitted Fluxor capabilities.

Phasor does not own:

- graph scheduling, module loading, placement, or platform abstraction;
- a host operating-system runtime or container lifecycle;
- DOM, filesystem, network, clock, randomness, process, or environment globals;
- HTTP or application protocol mechanics;
- identity, authorization, durable storage, or application policy; or
- general deterministic pipelines already owned by Chronicle.

Fluxor supplies bounded graph execution and capability providers. Wave supplies
protocol mechanics. Chronicle remains the preferred engine for ahead-of-time,
deterministic processing where dynamic JavaScript semantics are unnecessary.

## 3. Execution contract

Every invocation declares finite limits for source, bytecode, stack, heap,
instructions, jobs, provider calls, output, and wall-clock deadline. Exhausting
a limit is an ordinary typed result. It must not panic, corrupt isolate state,
silently discard work, or acquire more authority.

The module step performs a bounded amount of work. Long executions yield at
verified safe points and resume from isolate-owned state. Garbage collection is
incremental, explicitly scheduled, and charged to the same budget.

## 4. Authority contract

Scripts receive no ambient authority. Clock, randomness, storage, network,
identity, and other effects are imported through named, typed Fluxor bindings.
The deployment graph and policy decide which bindings exist.

Provider selection and deployment location never change JavaScript language
semantics. Provider failures cross the boundary as typed completion results
with trace correlation; they are not converted into implicit retries.

## 5. Source, bytecode, and identity

Source, compiler inputs, bytecode, and dependency closures are immutable and
content-addressed. Bytecode is tied to the exact Phasor format digest that
verified it. A mismatched or malformed program fails closed before execution.

There is no compatibility window or parallel bytecode-version dispatch. A
format change produces a different digest and requires recompilation.

Dynamic source evaluation goes through the same parser, verifier, limits,
provenance, and capability policy as deployment-time compilation: the machine
pauses on `eval` and the `Function` constructor, and the host that carries the
compiler compiles the source into a unit of its own before the machine enters
it. A host that carries no compiler answers the pause with the syntax error
the call would produce.

## 6. Language conformance

Phasor reports conformance by ECMAScript feature area and Test262 selection.
Passing a project-specific example does not imply support for the surrounding
language edition. Extensions must not silently change standard syntax or
coercion behavior.

The admitted grammar is versioned by feature rather than described as a
language edition, and its ordered digest sits in every unit image, so an image
compiled against a different surface is refused rather than run. Anything
outside the admitted grammar is refused by name.

## 7. Portability

Language cores under `modules/common/` are `no_std`, allocate only within the
bounded Phasor heap their caller supplies, and are independent of Fluxor ABI
types. Fluxor
application modules wrap those cores with channels, lifecycle, parameters,
telemetry, and backpressure.

Every executable Phasor component, including authoring tools and conformance
oracles, is an fmod. The project has no production, test, example, or tool Cargo
crate. Tests may orchestrate graphs externally but language behavior and
assertions execute in fixture fmods.

The same source and bytecode identities must be usable on every declared target.
Target-specific acceleration may replace an implementation only when observable
language semantics, limits, and failure behavior remain equivalent.

## 8. Observability and security

Metrics describe accepted programs, completed evaluations, syntax failures,
budget exhaustion, heap pressure, provider calls, cancellations, and dropped
telemetry. Script source, values, credentials, and provider payloads are not
logged by default.

All input is untrusted. Parsers, verifiers, bytecode execution, module loading,
and capability responses must fail without panics for malformed bytes. Fuzzing
and adversarial corpus tests accompany each expanding input surface.
