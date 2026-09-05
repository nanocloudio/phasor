# Module Identity and Import Resolution

Source: `modules/common/module.rs`, `modules/common/link.rs`.

This document defines what identifies a module, how a specifier becomes a
module, what a closure is, the states a module passes through, how a closure is
ordered, and how one is evaluated.

## 1. Identity is content

A module is identified by the digest of the exact bytes that define it, whether
those are source text or a verified image. Two modules with the same bytes are
the same module however they were fetched, and a module whose bytes change is a
different module rather than a new version of the same one.

That is why nothing here holds a path, a URL, or a version. A deployment may
resolve a specifier however it likes, but what it produces is bytes, and those
bytes are the identity.

## 2. Resolution is a separate step

A specifier in source is text. Turning it into a module key is the resolver's
job, and the resolver belongs to the deployment: it decides what a bare
specifier means, what is reachable, and what is refused. The engine records
which import resolved to which key and refuses to link until every import has
one.

Nothing in the engine fetches anything. A caller registers bytes it already
holds; a module that was never registered is a missing dependency, not a request
to go and find one.

## 3. Closures

A closure is every module reachable from an entry, registered together with the
imports that connect them. Registering the same key twice is idempotent, which
is what makes a diamond harmless; registering different content under one key is
a rejection, because a key is content.

A closure has a maximum size, and exceeding it is an ordinary rejection. It also
has a digest: every module's key in evaluation order. That digest identifies a
linked program and changes when any module in it changes, which is what lets a
deployment admit exactly the program it reviewed.

## 4. States

```text
New ──▶ Linking ──▶ Linked ──▶ Evaluating ──▶ Evaluated
          │            │            │
          └────────────┴────────────┴──────▶ Failed
```

A module is linked before it is evaluated, and a failure is remembered: a second
attempt fails the same way rather than running the body again. Any other
transition is a rejection rather than an undefined state.

## 5. Linking

Linking walks the imports from the entry and assigns the order the modules
evaluate in. The walk is iterative and uses a caller-provided stack rather than
the machine's own, so a deep or wide import graph is a bounded failure rather
than an overrun.

Cycles are admitted, because ECMAScript modules may import each other. A module
already being visited is not visited again, and the order that comes out is the
depth-first finishing order: every dependency that can be evaluated first
appears before its importer, and a shared dependency appears once, before both.

A link that fails leaves nothing claiming to be linked. Modules the walk reached
are in the linking state, which is not the linked state, and a caller checks
that before evaluating anything.

## 6. Rejections

Every way a link can fail has a name: an unresolved specifier, bytes that do not
match the key they were registered under, two different modules under one key, a
closure that is too large, an import the closure does not contain, an import
attribute the deployment does not admit, an operation in the wrong state, and a
full registry. None of them is a panic, and none of them leaves a partially
linked program behind.


## 7. What a module compiles to

A module compiles to an ordinary unit image with three additions: a flag saying
it is one, a table of what it imports, and a table of what it exports. An import
record names the specifier and the name it asks for; an export record names the
name other modules use and the slot in this module's environment that holds it.

A module's top level is a scope of its own rather than the global object, so its
`var`s, its declarations, and its functions live in an environment the module is
given before it runs.

## 8. The closure a linker produces

`phasor_link` takes a stream of compiled modules — each one its specifier and
its image — and produces one container: every module of the closure, in an order
where a module comes after everything it imports. It registers each module
under the digest of its specifier, resolves every import against that
registry, and orders through the same walk as above, started from every
module in stream order (`link_all`), so the entry — the module nothing
imports, which a stream carries last — is ordered last. It checks that every
image is admissible, that every specifier names a module the stream carried,
and that every imported name is one that module exports. A closure whose
imports do not resolve is not produced at all.

The container carries the format digest, the feature digest, and a digest of
everything after its header, so a closure that changed on the way is refused
rather than read. Nothing in it is a path or a URL: what a specifier resolves to
is what the stream said it was, which is what makes a closure a closed thing.

## 9. Evaluation

Every module's environment is made before any of them runs, which is what lets
one module read another's exports once it has run. The modules then run in the
closure's order.

An import is not a copy. Reading one goes through the module that exports it,
every time, so it sees what that module holds now — and a read of a binding the
exporting module has not initialised yet is the reference error the temporal
dead zone gives, which is exactly what a cycle should produce. Assigning to an
imported name is refused where it is written.

`import * as name` names the module itself: an object whose properties read the
module's slots when they are read, so a namespace is as live as the bindings
behind it.

The value a closure produces is what its entry module exports as `default`.
