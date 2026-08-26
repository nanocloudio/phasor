# Phasor common sources

This tree contains portable language-engine cores shared verbatim by Fluxor
modules and the shadow-tracked host harness. It is published eventually as the
`phasor-common` source artefact; it is not a Cargo crate or a second runtime.

- `lex.rs` is the tokenizer: goal-driven scanning, bounded positions, and
  stable diagnostics.
- `parse.rs` is the expression parser, which drives the lexer and builds the
  syntax arena.
- `arena.rs` is the append-only syntax arena and its node vocabulary.
- `bytecode.rs` defines the instruction encoding, the unit image, and the
  format digest.
- `emit.rs` encodes instructions and assembles canonical unit images.
- `lower.rs` lowers a parsed expression into a verified unit image.
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
- `vm.rs` is the interpreter: frames, registers, and the instruction loop.
- `realm.rs` builds a global object and the intrinsic prototypes.
- `vectors.rs` holds the execution conformance vectors as one byte blob.
- `policy.rs` holds the isolate's resource policy, lifecycle states, and the
  closed set of task outcomes.
- `replay.rs` records a run's identity, its inputs, and a digest of its result.
- `module.rs` holds module identity, imports, closures, and their states.
- `link.rs` orders a closure and checks that every import resolved.
- `job.rs` is the bounded job queue.
- `promise.rs` holds promise state, reactions, and settling.
- `binding.rs` is the typed seam to a capability: admitted bindings, in-flight
  calls, and completions.
- `source.rs` holds source limits, UTF-8 decoding, and the line table that maps
  a byte offset to a line and column.
- `numeric.rs` converts numeric literals to binary64 with correct rounding,
  using no floating-point arithmetic so every target agrees.
- `unicode_id.rs` holds the generated `ID_Start`, `ID_Continue`, and
  `Space_Separator` ranges.
- `diagnostic.rs` holds the stable diagnostic codes and the fixed-width record.
- `eval_core.rs` is the original bounded mechanism seed. It is deliberately too
  narrow to carry an ECMAScript edition claim.

These sources hold no pointers in static data: a module image is loaded at an
arbitrary address, so a static that contained a reference would need a
relocation the loader does not apply.
