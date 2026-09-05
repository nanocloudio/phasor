# Grammar and Syntax Arena

Source: `modules/common/parse.rs`, `modules/common/parse/`, `modules/common/arena.rs`.

This document defines the surface Phasor parses, the tree it builds, and the
bounds that parse works within. A source is a script or a module: a list of
statements, in which expressions, declarations, functions, classes,
generators, async functions, and destructuring patterns appear, and — in a
module — `import` and `export` declarations at the top level. What remains
outside the admitted grammar — `import.meta`, a `super` reference outside a
method, `new.target` outside a function, `await` and `yield` outside their
contexts, `with` in strict code — is refused by name rather than mis-parsed.

## 1. Feature versioning

The parser publishes feature records in the same form the lexical layer uses:
the pair `(name, version)`. The list lives in `modules/common/feature.rs`, and
its ordered digest sits in every unit image, so an image compiled against a
different list is refused rather than run.

| Feature | Version | Admits |
|---|---|---|
| `syntax.primary` | 3 | Identifier references, `this`, `null`, `true`, `false`, numeric, BigInt, string and template literals |
| `syntax.array` | 3 | Array literals with elisions, spread elements, and a trailing comma |
| `syntax.object` | 3 | Object literals with named, string, numeric, computed, shorthand, and spread properties, accessors, and plain, generator, and async methods |
| `syntax.member` | 4 | `.`, `[]`, calls with spread arguments, `new` (over a tagged template too), tagged templates, `import(…)` resolving against the closure the host staged, `import.defer(…)` answering a namespace whose module runs on first meaningful use, and `import.source(…)`, answered by a rejecting promise |
| `syntax.optional-chain` | 1 | `?.`, `?.[`, and `?.(` |
| `syntax.operators` | 1 | Unary, update, binary, relational, equality, bitwise, logical, and nullish operators |
| `syntax.conditional` | 1 | `test ? consequent : alternate` |
| `syntax.assignment` | 1 | Simple, compound, and logical assignment to a resolvable target |
| `syntax.sequence` | 1 | The comma operator |
| `syntax.statements` | 2 | Blocks, `;`, expression statements, `if`, `while`, `do`, `for`, `continue`, `break`, `return`, `throw`, `try`, `switch`, labels, `debugger`, and `with` in sloppy code |
| `syntax.declarations` | 4 | `var`, `let`, and `const`, binding a name or a destructuring pattern, with an optional initialiser; `using`, binding a resource disposed when its block, loop, or module is left; `await using`, in async code, whose disposal is awaited |
| `syntax.functions` | 2 | Function declarations, function expressions, and arrow functions, with patterns, defaults, and rest parameters |
| `syntax.async` | 1 | Async functions, async arrows, async methods, `await`, and top-level await in modules |
| `syntax.generator` | 1 | `function*`, generator methods, `yield`, and `yield*` |
| `syntax.class` | 2 | Class declarations and expressions: heritage, `super` reads, writes, and calls, methods, accessors, fields, `accessor` auto-accessor fields, private members, static blocks, and decorators — `@name`, `@a.b`, `@a.#b`, `@(expression)`, with at most one trailing call — on the class and its elements, each evaluated in source order and its answer left unapplied, which the proposal reads as keeping the value decorated |
| `syntax.iteration` | 1 | `for (x of y)`, `for (x in y)`, and a spread in an array literal or a call |
| `syntax.regexp` | 1 | A regular-expression literal, whose pattern is compiled when the image runs |
| `syntax.modules` | 2 | `import` and `export` at a module's top level: named, default, and namespace forms, with any IdentifierName as an exported or imported name |

Constructs outside that list are rejected by name rather than mis-parsed, so a
program never appears to be accepted with different meaning. Each refusal
carries an argument identifying what was written.

## 1a. Statements

A script is a list of statements. Automatic semicolon insertion is implemented
as the specification states it: a semicolon is inserted before a `}`, at the end
of the source, and wherever a line terminator separates the offending token from
what came before it. `return`, `throw`, `break`, `continue`, and the arrow's
`=>` are restricted productions, so a line terminator in the wrong place ends
the statement rather than continuing it.

`let` is not a keyword. It begins a declaration only where what follows it can
begin a binding, so `let x = 1` declares and `let + 1` reads a variable.

An arrow function is resolved by the cover grammar rather than by backtracking:
the head is parsed as an expression, and `=>` is what says it was a parameter
list. A head that cannot be one is `invalid-arrow-parameters`, reported at the
head rather than at the arrow.

## 2. Goal-driven scanning

The parser never lets the lexer guess. It requests the `RegExp` goal wherever an
operand may begin, the `Div` goal wherever an operator or continuation may
follow, and the `TemplateTail` goal for the `}` that closes a template
substitution. One lookahead token is held at a time; where a construct needs the
other reading of a token already scanned, the parser rewinds the lexer to that
token's own start and re-scans it, which crosses no trivia and so leaves the
line table exact.

Automatic semicolon insertion is decided in the statement grammar (§1a) from
the line-terminator flag each token carries. Inside an expression the one place
a terminator matters is a postfix `++` or `--`: a terminator before it ends the
expression instead.

## 3. The syntax arena

Nodes are fixed-width records in caller-provided storage, addressed by index. A
node holds its kind, flag bits, a source span, and three payload words whose
meaning depends on the kind: a child index, a source span, a list, an operator,
or an index into the Number values the parse collected.

The arena is append-only, and a node is pushed only once all of its children
exist. Nothing is back-patched, so a committed node never changes and a
completed arena is immutable by construction. Child sequences of unknown length
are built on a scratch stack and copied into the list storage when their extent
is known, which keeps each list contiguous even when list construction nests.

No node holds a pointer. An arena is therefore position-independent: it can be
moved, mapped, or handed to a later phase without relocation.

## 4. Bounds

| Limit | Ceiling | Diagnostic |
|---|---|---|
| Parser recursion entries | 512 | `expression-too-deep` |
| Syntax nodes per unit | 262144 | `too-many-syntax-nodes` |

The depth limit counts parser recursion entries rather than source nesting
levels, because one nested parenthesis descends through several grammar
functions. The parser is recursive, so a deployment admits a depth its stack can
hold; the limit is what makes that a stated bound rather than an assumption.

Exhausting node, list, number, or scratch storage is the same ordinary
diagnostic, with an argument naming which storage ran out. A parse that fails
leaves the arena holding whatever it had already committed, and nothing partial
is published.

## 5. Early errors the parser enforces

These are rejected during the parse rather than deferred:

- an assignment or update target that is not an identifier, member access,
  index access, or destructuring pattern;
- an assignment to any part of an optional chain;
- `**` applied to an unparenthesised unary expression;
- `??` mixed with `&&` or `||` without parentheses;
- an arrow head that cannot be a parameter list; and
- a private name outside a class body.

The remaining early errors — duplicate bindings, assignment to a constant, an
undeclared label, a `break` or `continue` with nowhere to go, a `return`
outside a function, the strict-mode restrictions on `eval`, `arguments`, and
parameter names, and the restrictions on `super`, `new.target`, and `await` —
are enforced when the tree is lowered, where the scopes that decide them exist.
Each has its own code in the [diagnostic vocabulary](../reference/diagnostics.md).
