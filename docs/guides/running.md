# Running Phasor

Phasor is a set of Fluxor modules. Running it means composing those modules into
a graph and running that graph. Every graph below is written out here, so
nothing outside this page is needed. A graph is written to a file rather than
piped into the runner because the program's own input arrives on standard
input.

## Build

Install the suite CLI once, then build the modules:

```sh
make -C ../fluxor install
make build
```

## Run as a command

The shell is the engine as a command: one applet fmod over the `cli` stack,
installed once and dispatched by `fluxor exec`, or by name through a busybox
link.

```sh
fluxor install packaging/cli/workload.toml --link ~/.cargo/bin
printf 'function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); } fib(18)' | phasor
# 2584
phasor -e '[1, 2, 3].map(n => n * n)'
# 1,4,9
phasor -i
# > let a = 20
# undefined
# > a * 2 + 2
# 42
```

A script on standard input runs once, and the value of its last expression
is printed when it is not `undefined`; `print(x)` writes a line. `-i` reads
a line at a time and keeps one realm between lines, continuing a line that
has not parsed to its end. Every input is a bounded task: a runaway loop
ends as `phasor: fuel-exhausted` with a non-zero status, and `--steps <n>`
sets the budget up to the compiled-in ceiling.

Nothing is ambient. The realm has no clock and no randomness until
`--grant clock` or `--grant entropy` admits the binding, answered by the
adapter the applet's graph wires to it; the shell prints what it granted.
The graph is `packaging/cli/linux.yaml`.

The terminal is the program's: what the runtime says while the shell runs is
filed per run and read back with `fluxor applet logs phasor`, and
`fluxor exec -v phasor -- …` shows it live.

## Evaluate an expression

The expression evaluator is the smallest end-to-end path: one expression on
its input stream, the result on its output.

```sh
cat > /tmp/evaluate.yaml <<'EOF'
target: linux
tick_us: 100
platform:
  cli: {}
modules:
  - name: phasor_eval
wiring:
  - from: cli_in.stdin_out
    to: phasor_eval.source_in
  - from: phasor_eval.result_out
    to: cli_out.bytes_in
  - from: phasor_eval.exit
    to: cli_out.exit_in
EOF

printf '40 + 2' | fluxor run /tmp/evaluate.yaml
# 42
```

## Run a program

The full path is three modules: the compiler, the isolate, and the edge that
turns results and diagnostics into text. This is the graph behind
`examples/cli`:

```sh
cat > /tmp/cli.yaml <<'EOF'
target: linux
tick_us: 100
platform:
  cli: {}
modules:
  - name: phasor_compile
  - name: phasor_isolate
  - name: phasor_cli
wiring:
  - from: cli_in.stdin_out
    to: phasor_compile.source_in
  - from: phasor_compile.image_out
    to: phasor_isolate.image_in
  - from: phasor_isolate.result_out
    to: phasor_cli.result_in
  - from: phasor_compile.diagnostic
    to: phasor_cli.diagnostic_in
  - from: phasor_isolate.diagnostic
    to: phasor_cli.runtime_in
  - from: phasor_cli.stdout
    to: cli_out.bytes_in
  - from: phasor_cli.stderr
    to: cli_out.err_in
  - from: phasor_cli.exit
    to: cli_out.exit_in
EOF

printf 'function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); } fib(18)' \
  | fluxor run /tmp/cli.yaml
# 2584
```

A program that fails says why, in words, on standard error, and exits
non-zero — whether the failure is a refusal (`let x = ;` answers
`phasor: expected-expression at 8..9`), a throw the program did not catch, or
a budget the run exhausted (`while (true) {}` answers
`phasor: fuel-exhausted`). The program's value goes to standard output and
nothing else does.

## Compile on one node, run on another

The compiler and the isolate are separate components, and the unit image between
them is content-addressed. That makes a split deployment ordinary rather than
special. Write the two graphs, build them, and join them with a pipe:

```sh
cat > /tmp/compile.yaml <<'EOF'
target: linux
tick_us: 100
platform:
  cli: {}
modules:
  - name: phasor_compile
wiring:
  - from: cli_in.stdin_out
    to: phasor_compile.source_in
  - from: phasor_compile.image_out
    to: cli_out.bytes_in
  - from: phasor_compile.exit
    to: cli_out.exit_in
EOF

cat > /tmp/run.yaml <<'EOF'
target: linux
tick_us: 100
platform:
  cli: {}
modules:
  - name: phasor_isolate
wiring:
  - from: cli_in.stdin_out
    to: phasor_isolate.image_in
  - from: phasor_isolate.result_out
    to: cli_out.bytes_in
  - from: phasor_isolate.exit
    to: cli_out.exit_in
EOF

fluxor build /tmp/compile.yaml
fluxor build /tmp/run.yaml

RUNTIME=target/aarch64-unknown-linux-gnu/release/fluxor-linux
printf '1 + 2 * 3' \
  | "$RUNTIME" --config target/linux/compile/config.bin \
               --modules target/linux/compile/modules.bin 2>/dev/null \
  | "$RUNTIME" --config target/linux/run/config.bin \
               --modules target/linux/run/modules.bin 2>/dev/null
# 7
```

The second node verifies the image it receives before anything runs. An image
that changed on the way does not run: its bytes are its identity.

## Run in WebAssembly

A graph with `target: wasm` builds into one self-contained bundle: kernel,
modules, and config in a single `.wasm` file. It needs the Fluxor wasm kernel at
`target/wasm/firmware.wasm`.

```sh
cat > /tmp/interpreter.yaml <<'EOF'
target: wasm
tick_us: 100
modules:
  - name: phasor_vm_probe
wiring: []
EOF

fluxor build /tmp/interpreter.yaml
# target/wasm/wasm/wasm/interpreter.wasm
```

A WebAssembly host supplies the imports the kernel names: the current time,
logging, panics, entropy, and module instantiation. The same module binary runs
there as on any other target.
