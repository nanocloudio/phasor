# Phasor common sources

This tree holds the portable language-engine cores. Every fmod mounts the
files it needs by path and compiles them in, so the same source runs on every
target; it is not a Cargo crate and not a runtime of its own.

- `lex.rs` is the tokenizer: goal-driven scanning, bounded positions, and
  stable diagnostics.
- `parse.rs` is the parser, which drives the lexer and builds the syntax
  arena. It is the parent of `parse/`: `expressions`, `primary`, `classes`,
  `statements`, `functions`, `patterns`, and `modules`.
- `arena.rs` is the append-only syntax arena and its node vocabulary.
- `bytecode.rs` defines the instruction encoding, the unit image, and the
  format digest.
- `emit.rs` encodes instructions and assembles canonical unit images.
- `lower.rs` lowers a parsed script, module, or eval body into a verified
  unit image. It is the parent of `lower/`, one file per concern of the same
  lowering: `scopes`, `references`, `constants`, `functions`, `expressions`,
  `calls`, `patterns`, `classes`, `statements`, `loops`, `exceptions`,
  `modules`, and `prologues`.
- `frontend.rs` is the one pipeline from source to image: it chains the
  lexer, the parser, and the lowering over borrowed storage under a goal.
- `verify.rs` is the verifier every image passes before it may be executed.
- `digest.rs` is SHA-256, the content-identity function.
- `value.rs` holds the tagged value representation and the numeric coercions.
- `softfloat.rs` supplies binary64 arithmetic on targets without
  double-precision instructions, with bit-identical results.
- `heap.rs` is the bounded arena, the handle table, and the collector's marking
  and compaction steps.
- `gc.rs` knows what each cell kind points at and drives a collection in slices.
- `string.rs` holds string cells, property keys, interning, and
  `Number::toString`.
- `dtoa.rs` produces the shortest decimal digits that identify a double.
- `object.rs` is the ordinary object model: properties, descriptors, and the
  prototype chain.
- `env.rs` holds environment records and the scope chain.
- `vm.rs` is the interpreter: frames, registers, the instruction loop, and
  the unwinding. It is the parent of `vm/`, one file per subsystem of the same
  machine: `calls`, `properties`, `privates`, `coercion`, `names`, `eval`, `modules`,
  `coroutines`, `iteration`, `host`, `regexps`, `bigints`, `json`, `date`,
  `proxy`, `buffers`, `realms`, and `natives/`, which holds the functions the
  engine implements itself, one file per library area.
- `realm.rs` builds a global object and the intrinsic prototypes. It is the
  parent of `realm/`, one file per family of intrinsics, each built at the
  point `create` calls it: `errors`, `promises`, `symbols`, `functions`,
  `iterators`, `collections`, `reflect`, `weak`, `date`, `proxy`,
  `buffers`, `json`, `objects`, `arrays`, `strings`, `numbers`, and `math`.
- `bigint.rs` holds BigInt cells: limbs, arithmetic, comparison, and text.
- `regexp.rs` compiles a pattern to a program and runs a match over it.
- `closure.rs` is the linked-closure container the linker writes and the
  isolate reads.
- `evalsite.rs` holds the eval-site records: what a direct `eval` may see.
- `feature.rs` names the admitted syntax features and digests the list.
- `wire.rs` moves bytes between a module and its ports: staging a stream,
  taking a frame, pushing what is staged. With `probe.rs` and `entry.rs` it is
  one of the three files here that name the Fluxor ABI.
- `probe.rs` is the skeleton every fixture probe runs on.
- `entry.rs` is the `entry!` macro that writes an fmod's loader-facing entry
  points.
- `text.rs` writes ASCII text and decimal numbers into bounded buffers.
- `vectors.rs` holds the execution conformance vectors as one byte blob.
- `policy.rs` holds the isolate's resource policy, lifecycle states, and the
  closed set of task outcomes.
- `agent.rs` is the one bring-up of a machine over a host's storage: `fresh`
  over new storage, `adopt` over storage a previous step left, every seam
  attached in one order under a `policy::Policy` — the bindings a host
  admits, a closure's instances, a compiler asked in place, and the sink a
  host-installed `print` writes to.
- `replay.rs` records a run's identity, its inputs, and a digest of its result.
- `module.rs` holds module identity, imports, closures, and their states.
- `link.rs` orders a closure and checks that every import resolved: `link`
  from one entry, `link_all` from every module in registration order, which
  is what the linker's stream needs.
- `job.rs` is the bounded job queue.
- `promise.rs` holds promise state, reactions, and settling.
- `binding.rs` is the typed seam to a capability: admitted bindings and how
  each is served, in-flight calls, the records that cross with their payload
  lengths, and the generation-checked handles a provider's resources come
  back as.
- `facade.rs` holds the standard surface as JavaScript source: console,
  text encoding, base64, events, abort, timers, and structured clone. A host
  compiles it and runs it in the realm before the program, so it is a
  program rather than engine code and takes no privilege of its own.
- `capability.rs` reads what an image says it requires: every import whose
  specifier carries the `phasor:` scheme, which a deployment checks against
  what it granted before anything runs.
- `source.rs` holds source limits, UTF-8 decoding, and the line table that maps
  a byte offset to a line and column.
- `numeric.rs` converts numeric literals to binary64 with correct rounding,
  using no floating-point arithmetic so every target agrees.
- `unicode_id.rs` holds the generated `ID_Start`, `ID_Continue`, and
  `Space_Separator` ranges.
- `diagnostic.rs` holds the stable diagnostic codes and the fixed-width record.
- `eval_core.rs` is the bounded expression evaluator behind `phasor_eval`:
  integer arithmetic over one expression under a byte budget, deliberately
  narrower than the language.

These sources hold no pointers in static data: a module image is loaded at an
arbitrary address, so a static that contained a reference would need a
relocation the loader does not apply.
