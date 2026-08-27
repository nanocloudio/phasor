# Lexical Grammar and Source Limits

Source: `modules/common/lex.rs`, `modules/common/source.rs`,
`modules/common/numeric.rs`, `modules/common/unicode_id.rs`.

This document defines the token surface Phasor admits, the exact bounds every
source transfer is checked against, and the way that surface is versioned. It
covers phase 1 of the front end: decoding and lexing. Syntactic production,
static semantics, and lowering are the phases after it, and `phasor_compile`
hosts them all.

## 1. Feature versioning

Phasor never claims a named ECMAScript edition. The build admits an ordered
feature list — `modules/common/feature.rs` — and the digest of that list sits in
every unit image, beside the format digest. An image compiled under one feature
list is refused by an isolate built against another, before any section of it is
read.

A feature record is the pair `(name, version)`. Names are lower-case, stable,
and never reused for different meaning. The lexical layer publishes these
records:

| Feature | Version | Admits |
|---|---|---|
| `lex.source-text` | 1 | UTF-8 transport, UTF-16 semantics, line terminators, white space |
| `lex.comment` | 1 | Single-line, multi-line, and leading hashbang comments |
| `lex.identifier` | 1 | `ID_Start`/`ID_Continue` identifiers, `$`, `_`, Unicode escapes, private names |
| `lex.punctuator` | 1 | The punctuator set listed in section 6 |
| `lex.numeric` | 1 | Decimal, hexadecimal, octal, and binary literals with separators |
| `lex.bigint` | 1 | The `n` suffix on integer literals |
| `lex.string` | 1 | String literals and their escape sequences |
| `lex.template` | 1 | Template literals and nested substitutions |
| `lex.regexp-literal` | 1 | Regular-expression literal framing, not the pattern grammar |

The whole list — lexical, syntax, and runtime records — is what the digest
covers; each layer's own records are documented with that layer. The tokenizer
implements exactly the surface these records describe.

Absent from version 1, and therefore a diagnostic rather than a silent
acceptance: HTML-like comments, legacy octal literals, legacy octal escape
sequences, and the regular-expression pattern grammar. Each becomes a separate
feature record with its own version when it is implemented. Raising any version
changes the feature digest and invalidates every unit compiled under the old
one.

## 2. Source admission and decoding

A source transfer arrives as bytes with a declared total, a logical digest, and
the limits it was admitted under. The compiler reserves storage for the declared
total before accepting the transfer and rejects it if the total exceeds any
limit in section 3. Nothing is lexed before the transfer commits.

Decoding rules:

- Transport encoding is UTF-8. A byte sequence that is not well-formed UTF-8 is
  rejected with `invalid-utf8` and its byte offset. There is no replacement
  character substitution, because substitution would change the program that a
  digest identifies.
- A leading UTF-8 encoded U+FEFF is consumed as a byte-order mark. Any later
  U+FEFF is white space.
- Language semantics are UTF-16. A code point above U+FFFF occupies two UTF-16
  code units, and every position exposed to a program counts code units, not
  bytes or code points.
- A lone surrogate in a file enters only through a `\uXXXX` escape inside a
  string or template literal, where it is preserved exactly. Source staged
  from a string — an `eval` or `Function` argument — carries a string's lone
  surrogates as three-byte sequences, which the decoder admits as the code
  units they are.

Line terminators are U+000A, U+000D, U+2028, and U+2029. The pair CR LF is one
terminator. White space is U+0009, U+000B, U+000C, U+0020, U+00A0, U+FEFF, and
any code point with the Unicode `Space_Separator` property.

## 3. Source limits

Every limit below is a hard ceiling compiled into the front end. A graph may
admit a smaller value; it cannot admit a larger one. Exceeding a ceiling is an
ordinary diagnostic, never a panic, truncation, or partial unit.

| Limit | Ceiling | Diagnostic |
|---|---|---|
| Source bytes per unit | 1048576 | `source-too-large` |
| UTF-16 code units per unit | 1048576 | `source-too-large` |
| Lines per unit | 262144 | `too-many-lines` |
| Bytes per line | 65536 | `line-too-long` |
| Tokens per unit | 262144 | `too-many-tokens` |
| UTF-16 code units per identifier | 256 | `identifier-too-long` |
| UTF-16 code units per string literal | 65536 | `literal-too-long` |
| UTF-16 code units per template part | 65536 | `literal-too-long` |
| Source bytes per numeric literal | 4096 | `numeric-literal-too-long` |
| Source bytes per regular-expression literal | 4096 | `regexp-literal-too-long` |
| Template substitution nesting | 16 | `template-nesting-too-deep` |

Positions are byte offsets into the committed transfer, held as unsigned 32-bit
values, which the byte ceiling keeps in range. The lexer maintains a line-start
table of byte offsets and a per-line count of the UTF-16 code units that precede
each line, so a line and column pair is derived on demand rather than carried on
every token. The table has one entry per line and is bounded by the line
ceiling.

## 4. Bounded, resumable scanning

The compiler scans a bounded number of source bytes per module step and stores
its cursor in fmod state. A step ends at a token boundary or inside a
long literal at a code-unit boundary, and the next step resumes from the stored
cursor with identical results. No scan loop runs until the input is exhausted.

Fuel is charged for consumed source bytes, produced tokens, and code units
copied into literal storage, so a single pathological token cannot hide
unbounded work behind one token count.

## 5. Goal symbols and lexer context

The lexer is driven by the parser and has no ambiguity of its own. The parser
requests one of four goal symbols before each token:

| Goal | Requested when | Distinguishes |
|---|---|---|
| `Div` | An expression has just completed | `/` and `/=` as operators |
| `RegExp` | An operand is expected | `/` as the start of a literal |
| `TemplateTail` | A substitution has just closed with `}` | `}` as template continuation |
| `HashbangOrDiv` | At offset zero only | A leading `#!` comment |

Requesting the wrong goal is a compiler defect, not a source error: the lexer
never guesses. The private-name token `#name` is produced under `Div` and
`RegExp` alike and is rejected by the parser where it is not permitted.

Every token records whether a line terminator appeared in the trivia preceding
it. The parser uses that flag for automatic semicolon insertion, and for the
restricted productions that forbid a terminator, such as the one between
`return` and its argument. The lexer itself inserts nothing.

## 6. Token surface

### Identifiers and reserved words

An identifier starts with `ID_Start`, `$`, or `_`, and continues with
`ID_Continue`, `$`, U+200C, or U+200D. A `\uXXXX` or `\u{...}` escape
contributes the code point it denotes, which must itself satisfy the start or
continue property. An escape that denotes a code point valid in neither position
is `invalid-identifier-escape`.

Reserved words are recognised as their own token kind: `await`, `break`, `case`,
`catch`, `class`, `const`, `continue`, `debugger`, `default`, `delete`, `do`,
`else`, `enum`, `export`, `extends`, `false`, `finally`, `for`, `function`,
`if`, `import`, `in`, `instanceof`, `new`, `null`, `return`, `super`, `switch`,
`this`, `throw`, `true`, `try`, `typeof`, `var`, `void`, `while`, `with`, and
`yield`. The contextual words `as`, `async`, `from`, `get`, `let`, `of`, `set`,
`static`, and `target` are ordinary identifiers to the lexer; only the parser
gives them meaning. A name written with an escape is never a keyword token, because `\u0069f` is a
legal property name and an illegal identifier reference. The token records that
it spells a reserved word, and the parser reports `escaped-reserved-word` where
a reference was expected.

A private name is `#` immediately followed by an identifier with no intervening
trivia.

### Punctuators

```text
{ } ( ) [ ] ; , : ~ ? . ... ?. =>
< > <= >= == != === !==
+ - * / % ** ++ --
<< >> >>> & | ^ ! && || ??
= += -= *= /= %= **= <<= >>= >>>= &= |= ^= &&= ||= ??=
```

`?.` followed by a decimal digit is scanned as `?` then `.`, so that
`a?.5:b` remains a conditional expression. The longest match wins everywhere
else.

### Numeric literals

Decimal literals accept an integer part, a fraction, and an exponent. Radix
literals are `0x`/`0X`, `0o`/`0O`, and `0b`/`0B`, each with at least one digit
of the corresponding radix. An underscore separator is permitted between two
digits and nowhere else: a leading, trailing, or doubled separator is
`invalid-numeric-separator`.

A `0` followed directly by a decimal digit is `legacy-octal-literal`. A literal
followed immediately by an identifier start or a decimal digit is
`invalid-numeric-terminator`, so `3in` and `0x1g` are rejected rather than split.

An integer literal, in any radix, may carry the suffix `n` and becomes a BigInt
token. A separator, fraction, exponent, or leading zero before `n` is
`invalid-bigint-literal`.

Decimal values are converted to IEEE-754 binary64 with correct rounding at
compile time, so the same source yields the same number on every target. A BigInt literal is carried through the front end as its digits and the radix
they were written in, and becomes an exact integer when the image runs.

### String literals

A string literal is delimited by `'` or `"` and may contain:

- single-character escapes `\'`, `\"`, `\\`, `\b`, `\f`, `\n`, `\r`, `\t`,
  `\v`, and `\0` where no digit follows;
- `\xHH`;
- `\uHHHH`, including a lone surrogate, preserved exactly;
- `\u{...}` with a value up to U+10FFFF; and
- a line continuation, which is a backslash followed by a line terminator and
  contributes nothing.

A line feed or carriage return inside a literal is `unterminated-string`, and so
is reaching the end of source. U+2028 and U+2029 are ordinary string characters,
because a string literal is a superset of a JSON string. An octal or `\8`/`\9` escape is
`legacy-octal-escape`. A malformed hexadecimal or code-point escape is
`invalid-escape`, and a value above U+10FFFF is `invalid-code-point`.

### Template literals

A template is scanned as a sequence of tokens: `` ` `` opens it, each part
carries both its cooked value and its raw text, `${` opens a substitution, and
the matching `}` is scanned under the `TemplateTail` goal. Substitutions nest up
to the ceiling in section 3; the lexer holds one bounded depth counter and no
recursion.

In an untagged template an invalid escape is a diagnostic. In a tagged template
it is not: the part has no cooked value, keeps its raw text, and the parser
records the absent cooked value. Reaching the end of source inside a template is
`unterminated-template`.

### Regular-expression literals

Under the `RegExp` goal, `/` opens a literal that continues to the closing `/`
that is neither escaped nor inside a character class, followed by identifier
continue characters as flags. The lexer validates the framing, the flag
characters, and the absence of a duplicate flag; the pattern itself is
retained as source and compiled by the pattern grammar when the image runs,
which is where the admitted flags and constructs are decided — see the
[library](library.md).

An unterminated literal, an embedded line terminator — escaped or not — or an
empty pattern that would be read as a comment is `invalid-regexp-literal`.

### Comments

A single-line comment runs from `//` to the next line terminator, which is not
part of it. A multi-line comment runs from `/*` to `*/` and is treated as a line
terminator for the purposes of section 5 when it contains one. An unterminated
multi-line comment is `unterminated-comment`. A hashbang comment is admitted
only at offset zero. The sequences `<!--` and `-->` are not comments; they lex
as punctuators and fail in the parser.

## 7. Adversarial input

Every rule above is enforced on untrusted bytes. The front end has no
recursion over source structure, no unbounded buffer, no arithmetic that can
overflow a position, and no path that can panic on malformed input. Reaching a
ceiling, meeting an ill-formed byte sequence, or meeting an unterminated
construct produces a diagnostic that names a stable code and a byte span, and
leaves the compiler able to accept the next request.

The diagnostic codes named here are defined, with their severities and
arguments, in the [diagnostic vocabulary](../reference/diagnostics.md).
