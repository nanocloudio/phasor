# Phasor Documentation

Phasor is a capability-secure, resource-bounded JavaScript engine built on
Fluxor. Programs compile to verified, content-addressed bytecode and run inside
isolate modules with explicit heap, stack, instruction, and job budgets; every
effect a program has on the world leaves as a typed call on a wired channel.
The engine ships as position-independent `.fmod` modules over shared `no_std`
cores: the cores build and are proven on the microcontroller target, and the
engine graphs run on application processors, Linux hosts, and browser-hosted
WASM bundles — the same content-addressed image on each.

## Start Here

- [guides/running.md](guides/running.md) — compose and run the graphs, config included
- [architecture/fmod-architecture.md](architecture/fmod-architecture.md) — the module graph, its boundaries, and why they sit where they do
- [architecture/isolate.md](architecture/isolate.md) — what one isolate is, its resource policy, and its outcomes
- [specification.md](specification.md) — the ownership and execution contract

## Architecture

How the engine works. These are the authoritative references.

- [architecture/fmod-architecture.md](architecture/fmod-architecture.md) — graph shape, fmod boundaries, scheduling and authority contracts
- [architecture/engine-internals.md](architecture/engine-internals.md) — the pipeline from source to result, and the engine lineage it draws on
- [architecture/lexical-grammar.md](architecture/lexical-grammar.md) — tokens, source limits, and the diagnostic seam
- [architecture/expression-grammar.md](architecture/expression-grammar.md) — the parsed surface, feature versioning, and the syntax arena
- [architecture/bytecode.md](architecture/bytecode.md) — the instruction format, the verifier, and content identity
- [architecture/values.md](architecture/values.md) — tagged values and numeric coercion
- [architecture/strings-and-heap.md](architecture/strings-and-heap.md) — heap cells, strings, and property keys
- [architecture/objects.md](architecture/objects.md) — ordinary objects, descriptors, and prototype lookup
- [architecture/interpreter.md](architecture/interpreter.md) — frames, environments, closures, and the instruction loop
- [architecture/isolate.md](architecture/isolate.md) — isolate lifecycle, resource policy, and the outcome vocabulary
- [architecture/collection.md](architecture/collection.md) — the bounded heap and incremental collection
- [architecture/jobs.md](architecture/jobs.md) — the job queue and promise settlement
- [architecture/modules.md](architecture/modules.md) — module identity, linking, and live bindings
- [architecture/library.md](architecture/library.md) — the intrinsics a realm carries, and the rule that decides what belongs
- [architecture/capabilities.md](architecture/capabilities.md) — host bindings, call records, and completions
- [architecture/replay.md](architecture/replay.md) — the deterministic replay profile

## Guides

- [guides/running.md](guides/running.md) — build the modules and run the graphs, from one expression to capability wiring

## Reference

- [reference/fmod-catalog.md](reference/fmod-catalog.md) — every fmod the project ships, its ports, and its parameters
- [reference/diagnostics.md](reference/diagnostics.md) — the diagnostic vocabulary, code by code

## Conformance

Language behaviour is measured against Test262, by feature area, on the same
module substrate a deployment runs. `tests/conformance/test262.sh` drives the
front end over the corpus's language tree; `tests/conformance/run262.sh`
executes every case within the admitted grammar behind the Test262 harness and
holds the results to a per-file baseline. The admitted surface itself is
declared feature by feature in
[architecture/expression-grammar.md](architecture/expression-grammar.md), and
anything outside it is refused by name.
