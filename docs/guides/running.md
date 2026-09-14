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

Nothing is ambient. The realm has no clock, no randomness, and no storage
until a grant admits one, and each interface arrives as a namespace of the
members that were granted:

```sh
phasor --grant clock -e 'clock.now()'
phasor --grant entropy -e 'entropy.random().then(n => n)'
phasor --grant store -e 'store.write("k", "bytes").then(() => store.read("k"))'
```

`clock.now()` answers at once because a clock is a fact the adapter supplies
at the task boundary rather than a call. Everything else is a call and answers
a promise. `store.open(key)` answers a handle the program passes back to
`store.readAt`; one it invents is refused. The shell prints what it granted,
and the graph is `packaging/cli/linux.yaml`.

The store is the deployment's, not the program's. It is served by whatever is
wired behind Fluxor's `storage.object` contract, which on Linux is the local
versioned store and opens the directory `FLUXOR_STORE_DIR` names:

```sh
export FLUXOR_STORE_DIR=~/.local/share/phasor
phasor --grant store -e 'store.write("k", "kept")'
phasor --grant store -e 'store.read("k")'          # -> kept, a run later
```

With none configured a write is refused rather than kept somewhere it would
not survive, which is the honest answer: the adapter holds nothing itself, so
there is no second place the bytes could have gone.

Files come from `--grant fs`, and reach only the directory the graph runs in:

```sh
phasor --grant fs -e 'fs.read("README.md").then(t => t.length)'
phasor --grant fs -e 'fs.write("note.txt", "written by a program")'
```

HTTP is its own grant, because the protocol is its own provider's. `--grant
http` brings `fetch`, and it goes to the authority the URL names: the applet
grants every origin, an `https://` URL goes down the graph's TLS leg and an
`http://` one down its plain leg.

```sh
phasor --grant http -e 'fetch("http://localhost:8080/hello.txt").then(r => r.text())'
phasor --grant http -e 'fetch("https://nanocloud.io/").then(r => r.status)'
```

A graph that grants only some origins lists them in `phasor_http`'s
`origins`; a URL naming any other is refused by the adapter before anything
is dialled, and a bare path goes to the first one granted. The TLS leg asks
the platform whether a chain is good, so it reaches what the host itself
would; `fluxor exec --ca <file>` widens that for one run, for a peer chaining
to some other authority.

The connection is a separate grant and a lower one. `--grant net` gives bytes
to an authority and nothing that reads them:

```sh
phasor --grant net -e 'net.connect("localhost:8080").then(h => net.endpoint())'
```

`connect` names `host:port`, or names nothing and goes to the authority the
graph gave the adapter; `net.endpoint()` answers with whichever it was.
Whether a connection is reached over TLS is the graph's to say and nothing a
program can see: `examples/net/https.yaml` is the same shell with Fluxor's
`tls` between the provider and the socket.

Without the grant there is no `net` and no `fetch` either: the surface
defines it over the binding, so a program that was granted nothing finds
nothing.

A WebSocket is its own grant, because it is its own protocol: the upgrade,
the accept it verifies, the masking and the frame codec are the provider's,
and the surface keeps only the shape the language promises.

```sh
phasor --grant websocket -e '
new Promise(done => {
  const ws = new WebSocket("/chat");
  ws.addEventListener("open", () => ws.send("hello"));
  ws.addEventListener("message", e => { ws.close(); done(e.data); });
})'
```

The origin is the deployment's here too: a `ws://` URL naming another host is
refused rather than answered by the one that was wired.

Granting the clock also brings timers, because being told later is what the
clock's `sleep` is:

```sh
phasor --grant clock -e 'new Promise(r => setTimeout(() => r("later"), 50))'
```

Before any of that, every session already has the standard surface —
`console`, `TextEncoder`, `TextDecoder`, `btoa`, `atob`, `URL`,
`URLSearchParams`, `Event`, `EventTarget`, `AbortController`,
`queueMicrotask`, `structuredClone`. It is JavaScript compiled and run in the
realm ahead of your program, not engine code, and `--bare` leaves it out.
`docs/reference/capability-register.md` lists every capability and how each is
named.

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
