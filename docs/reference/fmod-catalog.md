# Fmod Catalogue

Every executable part of Phasor is an fmod. This page is the register of them:
what is built, what each one owns, what crosses its ports, and what each one
takes as a parameter. Names are ecosystem-unique. Port frames are bounded
binary records with explicit lengths and checked offsets.

## Engine fmods

| Fmod | Kind | Inputs | Outputs | Owns |
|---|---|---|---|---|
| `phasor_compile` | Transformer | `source_in` | `image_out`, `diagnostic`, `exit` | Lexing, parsing, early errors, bytecode emission, verification, image serialisation. The `goal` parameter says whether the source is a script or a module |
| `phasor_link` | Transformer | `unit_in` | `closure_out`, `exit` | Module closure: admitting each image, resolving every specifier and imported name, ordering the closure, writing the container |
| `phasor_isolate` | EventHandler | `image_in`, `completion_in`, `control_in` | `result_out`, `call_out`, `diagnostic`, `exit` | Realm, machine, objects, heap, collection, jobs, promises, pending calls, module evaluation |
| `phasor_host_router` | Protocol | `call_in`, `reply_in` | `completion_out`, `request_out` | Binding admission, routing, correlation, bounded in-flight table |
| `phasor_cli` | Cli | `result_in`, `diagnostic_in`, `runtime_in` | `stdout`, `stderr`, `exit` | Human framing: the only place that turns a diagnostic's numbers into words |
| `phasor_time` | Adapter | `request_in` | `reply_out` | A time observation from the `source` its parameter names — monotonic milliseconds or microseconds, or Unix milliseconds — under the `quota` it sets |
| `phasor_entropy` | Adapter | `request_in` | `reply_out` | A random number from the platform's own source, 32 or 53 bits wide as the `width` parameter says, under a `quota` |
| `phasor_eval` | Transformer | `source_in` | `result_out`, `exit` | The bounded expression evaluator, kept as the smallest end-to-end path |
| `phasor_shell` | Cli | `args`, `stdin`, `clock_reply`, `entropy_reply` | `stdout`, `exit`, `clock_call`, `entropy_call` | The shell: a script, `-e`, or a REPL over one realm, each input a bounded task; `--grant clock` and `--grant entropy` admit the two bindings its graph wires directly to the adapters; `--steps` sets the fuel. Installed as the `phasor` applet from `packaging/cli/` |

A source stream ending in a hang-up is one source; an image stream ending in a
hang-up is one image. Nothing carries a length prefix it could lie about, and
an image is content-addressed, so what runs is the bytes that arrived.

`phasor_compile` and `phasor_isolate` are separable: the two halves of
`examples/split` run in different processes with the image passing between them.

## Adapter and fixture fmods

| Fmod | Purpose | Shipping rule |
|---|---|---|
| `phasor_fault_host` | Answers a router's calls the way its `mode` parameter says: with a `value`, a denial, an unavailable provider, or silence, after a `grace` | Fixture only |
| `phasor_test262` | Tokenizes and parses a batch of Test262 cases on the graph and reports per-area evidence | Fixture only |
| `phasor_run262` | Compiles and runs a batch of Test262 cases behind the harness and answers one verdict per case | Fixture only |
| `phasor_lex_probe`, `phasor_parse_probe`, `phasor_compile_probe`, `phasor_bytecode_probe` | Front-end assertions | Fixture only |
| `phasor_value_probe`, `phasor_string_probe`, `phasor_object_probe`, `phasor_gc_probe` | Value, string, object, and collection assertions | Fixture only |
| `phasor_vm_probe`, `phasor_error_probe`, `phasor_promise_probe`, `phasor_control_probe` | Machine, unwinding, job, and budget assertions | Fixture only |
| `phasor_binding_probe`, `phasor_module_probe`, `phasor_replay_probe`, `phasor_invariant_probe` | Capability, module, replay, and invariant assertions | Fixture only |
| `phasor_conformance_probe`, `phasor_eval_probe` | Vector and expression-evaluator assertions | Fixture only |

Every probe reports a count and an exit status, and a graph gate asserts the
exact text. A fixture never ships in a production bundle.

## Frame contracts

| Frame | Producer -> consumer | Fields |
|---|---|---|
| Source | provider -> `phasor_compile` | the source bytes, ended by a hang-up |
| Unit image | `phasor_compile` -> `phasor_isolate` | magic, format digest, feature digest, header, functions, constants, code, exception regions, safe points, imports, exports |
| Module record | provider -> `phasor_link` | a specifier and an image, each with its length in front |
| Linked closure | `phasor_link` -> `phasor_isolate` | magic, format digest, feature digest, a digest of the payload, and every module's specifier and image in evaluation order |
| `Diagnostic` | any phase -> `phasor_cli` | code, severity, span, and up to four arguments — 32 bytes, and never text |
| `CallRecord` | `phasor_isolate` -> `phasor_host_router` -> adapter | request id, binding index, trace context, payload digest — 56 bytes |
| `CompletionRecord` | adapter -> `phasor_host_router` -> `phasor_isolate` | request id, disposition, typed cause, trace context, optional number — 32 bytes |
| Result | `phasor_isolate` -> caller | the result's text, or `rejected: <cause>` |

A call record carries no address, no credential, no pointer, and no display
text. The caller's identity is which port the record arrived on. A completion
must carry back the trace the call went out under: the isolate refuses one that
does not match, and the router refuses to route it.

## Ceilings

Every fmod's work is bounded per step and its state carries the rest.

| Fmod | Bounded by |
|---|---|
| `phasor_compile` | source bytes staged per step, syntax nodes, constants, code bytes |
| `phasor_link` | modules in a closure, bytes of stream and closure |
| `phasor_isolate` | `steps` a program may run and `call_wait` steps before an unanswered call times out, both parameters clamped to the compiled-in ceiling; instructions per slice, jobs per slice, collection slice, and calls in flight, all compiled in |
| `phasor_host_router` | calls in flight, frames staged per output |
| `phasor_fault_host`, `phasor_time`, `phasor_entropy` | replies staged, and the quota an adapter was given |
| `phasor_cli` | bytes of result and of rendered text |
| `phasor_shell` | `--steps` a program may run, clamped to the ceiling; instructions per slice, jobs per slice, collection slice, calls in flight, units a session may make, and what `print` may write between steps, all compiled in |

No step loops until a variable-sized input is exhausted. The isolate rebuilds
its machine over its own storage on each step, which is what lets a task that
is waiting on an answer survive between steps without holding the scheduler.

## Port and backpressure rules

- A whole record is reassembled before it is interpreted; a partial record is
  not a record.
- An image is invisible until it verifies. An image that does not verify is
  refused, and so is one that asks for a construct this build does not run.
- A call is taken only when there is room to answer it, which is what keeps an
  answer from being dropped for want of space.
- The router never drops or silently reorders a completion. A call it cannot
  admit or forward is answered immediately with the reason.
- A call that cannot be answered is not left open: a provider that never
  answers is timed out by the isolate, and a graph that wired no call port has
  its calls refused as unavailable.

## Ports the isolate deliberately does not have

The isolate has no `task` inbox and no `lifecycle` outbox. One image is one
task, and the evidence a run leaves is its result and its exit status.
