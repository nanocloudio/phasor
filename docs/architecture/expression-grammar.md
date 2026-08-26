# Grammar and Syntax Arena

Source: `modules/common/parse.rs`, `modules/common/arena.rs`.

This document defines the surface Phasor parses, the tree it builds, and the
bounds that parse works within. A source is a script or a module: a list of
statements, in which expressions, declarations, and functions appear, and — in
a module — `import` and `export` declarations at the top level. Classes,
generators, `async`, and destructuring patterns are outside the admitted
grammar, and each is refused by name rather than mis-parsed.

## 1. Feature versioning

The parser publishes feature records in the same form the lexical layer uses:
the pair `(name, version)`. The list lives in `modules/common/feature.rs`, and
its ordered digest sits in every unit image, so an image compiled against a
different list is refused rather than run.

| Feature | Version | Admits |
|---|---|---|
| `syntax.primary` | 3 | Identifier references, `this`, `null`, `true`, `false`, numeric, BigInt, string and template literals |
| `syntax.array` | 3 | Array literals with elisions, spread elements, and a trailing comma |
| `syntax.object` | 1 | Object literals with named, string, numeric, computed, shorthand, and spread properties |
| `syntax.member` | 3 | `.`, `[]`, calls with spread arguments, `new`, and tagged templates |
| `syntax.optional-chain` | 1 | `?.`, `?.[`, and `?.(` |
| `syntax.operators` | 1 | Unary, update, binary, relational, equality, bitwise, logical, and nullish operators |
| `syntax.conditional` | 1 | `test ? consequent : alternate` |
| `syntax.assignment` | 1 | Simple, compound, and logical assignment to a resolvable target |
| `syntax.sequence` | 1 | The comma operator |
| `syntax.statements` | 1 | Blocks, `;`, expression statements, `if`, `while`, `do`, `for`, `continue`, `break`, `return`, `throw`, `try`, `switch`, labels, `debugger` |
| `syntax.declarations` | 1 | `var`, `let`, and `const`, with a name and an optional initialiser |
| `syntax.functions` | 1 | Function declarations, function expressions, and arrow functions, with simple parameters |
| `syntax.iteration` | 1 | `for (x of y)`, `for (x in y)`, and a spread in an array literal or a call |
| `syntax.regexp` | 1 | A regular-expression literal, whose pattern is compiled when the image runs |
| `syntax.modules` | 1 | `import` and `export` at a module's top level: named, default, and namespace forms |

A parameter default, a rest parameter, and a destructuring pattern are refused
where they are written.

Constructs outside that list are rejected by name rather than mis-parsed, so a
program never appears to be accepted with different meaning. Each carries an
argument identifying what was written: arrow functions, function expressions,
class expressions, `async` and `await`, `yield`, `super`, `import`,
`new.target`, destructuring patterns, method definitions, regular-expression
patterns, and every statement keyword.

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

Automatic semicolon insertion belongs to the statement grammar, which does not
exist yet. The one place a line terminator already matters is a postfix `++` or
`--`: a terminator before it ends the expression instead, and the token records
whether one appeared.

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
| Parser recursion entries | 128 | `expression-too-deep` |
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

- an assignment or update target that is not an identifier, member access, or
  index access;
- an assignment to any part of an optional chain;
- `**` applied to an unparenthesised unary expression;
- `??` mixed with `&&` or `||` without parentheses; and
- a private name, which has no admitted context while classes are unparsed.

The remaining early errors belong to the static-semantics phase, which is not
implemented.
