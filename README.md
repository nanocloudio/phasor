# Phasor

Phasor is a portable, capability-secure, resource-bounded JavaScript engine
built on Fluxor. It owns language parsing and execution semantics. Fluxor owns
module scheduling, channels, capabilities, placement, and platform providers.

Phasor is entirely Fluxor-native: every executable component builds as a
position-independent `.fmod` module over shared `no_std` source artefacts. It
does not publish a host runtime, embed a process abstraction, or expose ambient
operating-system authority.

## What it does

Source compiles to verified, content-addressed bytecode; the isolate admits an
image against a policy and runs it under explicit heap, stack, instruction, and
job budgets. Exhausting a budget is an ordinary typed outcome, never a crash.
Programs get the language — statements, functions, closures, objects and
prototypes, exceptions, iterators, symbols, BigInt, regular expressions,
promises, ECMAScript modules with live bindings, `eval` and the `Function`
constructor — and none of the host: there is no ambient clock, randomness,
filesystem, or network. Authority reaches a program only as a typed Fluxor
binding the deployment graph wired in.

The admitted grammar is declared feature by feature, and anything outside it is
refused by name rather than mis-parsed. Conformance is measured against
Test262 by feature area, executed on the same module substrate a deployment
runs, and held to a per-file baseline.

## Layout

```text
modules/common/             allocation-free language and VM cores
modules/app/                Fluxor stream components: compiler, linker, isolate, router, adapters
modules/fixtures/           on-graph conformance probes and Test262 oracles
packaging/cli/              the shell's applet graph and workload manifest
docs/                       canonical architecture and guarantees
tests/                      shadow-tracked graph orchestration only
examples/                   shadow-tracked runnable Fluxor graphs
```

The test and example trees are versioned in `.git-shadow/`, not the primary
repository. Read both `git status` and `git shadow status` while working.

## Lifecycle

Install the suite CLI once:

```sh
make -C ../fluxor install
```

Then use the standard lifecycle:

```sh
make build
make test
make lint
make ci
```

Operational commands remain explicit:

```sh
fluxor modules build --all --strict
printf '40 + 2' | fluxor run examples/addition/linux.yaml
```

The engine as a command is the `phasor` applet: a script on standard input,
`-e` for an expression, `-i` for a REPL over one realm, and `--grant` for the
clock, entropy, store, filesystem and network bindings, which are the only
authority a program can have.

```sh
fluxor install packaging/cli/workload.toml --link ~/.cargo/bin
printf '40 + 2' | phasor
```

The repository contains no Cargo packages. Language assertions and conformance
oracles execute as fixture fmods; shell scripts only compose graphs and inspect
their status.

Start with the [documentation overview](docs/overview.md): the
[run guide](docs/guides/running.md) brings a graph up, and the
[specification](docs/specification.md) states the ownership and execution
contracts.
