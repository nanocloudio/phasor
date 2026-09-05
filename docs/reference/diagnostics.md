# Diagnostic Vocabulary

Source: `modules/common/diagnostic.rs`.

A diagnostic is the only thing a rejected or stopped program produces. It is a
value, not text: the phase that found the problem emits a stable code, a
severity, a source span, and bounded arguments, and the edge that faces a
human renders them. The request, transfer, and lexical codes come from the
tokenizer, the syntactic codes from the parser, the lowering and bytecode
codes from the compiler and the verifier, and the termination codes from the
isolate. Each phase owns a range of its own, so a code says where a problem
was found as well as what it was.

## 1. Diagnostic record

| Field | Width | Meaning |
|---|---|---|
| `request` | u64 | The compile request the diagnostic belongs to |
| `code` | u16 | A value from the tables in sections 3 and 4 |
| `severity` | u8 | `error` or `fatal`, as defined in section 2 |
| `argument_count` | u8 | Number of populated argument slots, at most 4 |
| `offset` | u32 | Byte offset of the span start in the committed source |
| `length` | u32 | Span length in bytes, zero where a point is meant |
| `arguments` | 4 × u32 | Integers, or codes from an enumeration the code names |

The record is fixed width and carries no pointer, no allocation, and no text.
Its size is the same on every target.

Arguments are integers only. A diagnostic never carries source bytes,
identifier text, literal contents, or a rendered message, because a compiler
that copies source into an observable record leaks the program to anything
permitted to read diagnostics. A renderer that wants to quote the offending
text asks the holder of the source for the span, under whatever policy governs
that source.

Spans are byte offsets, matching the positions the lexer maintains. A renderer
converts a span to a line and column through the line-start table published
with the unit; the compiler does not carry line and column on every record.

## 2. Severity

| Severity | Meaning | Effect |
|---|---|---|
| `error` | The unit is not compilable | No artefact is produced; the compiler stays available for the next request |
| `fatal` | The request itself is unusable | The transfer is abandoned; nothing further about it is reported |

There is no warning severity. A construct is either admitted by the feature
list or rejected, so there is no state in which the compiler proceeds while
disapproving. Adding a warning severity later would require a policy for what a
graph does with one, and no such policy exists.

At most one `fatal` is emitted per request. Errors are emitted in ascending
span order, up to an admitted per-request maximum, after which the compiler
emits `too-many-diagnostics` and stops reporting for that request. It does not
stop checking: the unit is rejected either way.

## 3. Request and transfer codes

Range `0x0000`-`0x00FF`. These concern the request, its transfer, and its
admitted bounds rather than any construct in the program.

| Code | Name | Severity | Arguments |
|---|---|---|---|
| `0x0001` | `transfer-incomplete` | fatal | received bytes, declared total |
| `0x0002` | `transfer-overflow` | fatal | offending offset, declared total |
| `0x0003` | `digest-mismatch` | fatal | none |
| `0x0004` | `feature-digest-mismatch` | fatal | none |
| `0x0005` | `unsupported-goal` | fatal | requested goal |
| `0x0006` | `compile-budget-exhausted` | fatal | phase, bytes consumed |
| `0x0007` | `too-many-diagnostics` | error | admitted maximum |
| `0x0010` | `source-too-large` | error | measured size, admitted maximum |
| `0x0011` | `too-many-lines` | error | measured count, admitted maximum |
| `0x0012` | `line-too-long` | error | measured length, admitted maximum |
| `0x0013` | `too-many-tokens` | error | measured count, admitted maximum |
| `0x0014` | `identifier-too-long` | error | measured length, admitted maximum |
| `0x0015` | `literal-too-long` | error | measured length, admitted maximum |
| `0x0016` | `numeric-literal-too-long` | error | measured length, admitted maximum |
| `0x0017` | `regexp-literal-too-long` | error | measured length, admitted maximum |
| `0x0018` | `template-nesting-too-deep` | error | measured depth, admitted maximum |
| `0x0019` | `feature-not-admitted` | error | feature index |

`compile-budget-exhausted` is fatal because a compiler that ran out of fuel has
not finished checking, and a partial error list would misdescribe the program.
It is distinct from a size limit: a unit under every ceiling can still exhaust
the fuel a caller granted it.

## 4. Lexical codes

Range `0x0100`-`0x01FF`. Every span names the construct that failed, not the
character that revealed it: an unterminated string spans from its opening quote.

| Code | Name | Severity | Arguments |
|---|---|---|---|
| `0x0100` | `invalid-utf8` | fatal | byte offset, offending byte |
| `0x0101` | `invalid-character` | error | code point |
| `0x0102` | `unterminated-comment` | error | none |
| `0x0103` | `hashbang-not-at-start` | error | none |
| `0x0110` | `invalid-identifier-escape` | error | code point |
| `0x0111` | `escaped-reserved-word` | error | none |
| `0x0120` | `invalid-numeric-separator` | error | none |
| `0x0121` | `legacy-octal-literal` | error | none |
| `0x0122` | `invalid-numeric-terminator` | error | code point |
| `0x0123` | `missing-radix-digits` | error | radix |
| `0x0124` | `invalid-bigint-literal` | error | none |
| `0x0130` | `unterminated-string` | error | none |
| `0x0131` | `invalid-escape` | error | escape kind |
| `0x0132` | `legacy-octal-escape` | error | none |
| `0x0133` | `invalid-code-point` | error | none |
| `0x0140` | `unterminated-template` | error | none |
| `0x0150` | `invalid-regexp-literal` | error | none |
| `0x0151` | `invalid-regexp-flag` | error | code point |
| `0x0152` | `duplicate-regexp-flag` | error | code point |
| `0x0153` | `regexp-pattern-unsupported` | error | none |

`invalid-utf8` is fatal rather than an error because the byte sequence that
follows an ill-formed one cannot be interpreted, and a digest identifies exactly
the bytes that were sent.

Enumerated arguments:

- goal: `0` `Div`, `1` `RegExp`, `2` `TemplateTail`, `3` `HashbangOrDiv`.
- phase: `0` decode, `1` lex, `2` parse, `3` static semantics, `4` lower,
  `5` verify, `6` serialise.
- radix: the numeric radix itself, `2`, `8`, or `16`.
- escape kind: `0` hexadecimal, `1` unicode, `2` code point, `3` unrecognised.

## 5. Syntactic codes

Range `0x0200`-`0x02FF`. A span names the construct that failed rather than the
token that revealed it, so an invalid assignment target spans the target.

| Code | Name | Severity | Arguments |
|---|---|---|---|
| `0x0200` | `unexpected-token` | error | none |
| `0x0201` | `unexpected-end-of-source` | error | none |
| `0x0202` | `expected-expression` | error | none |
| `0x0203` | `expected-close-paren` | error | none |
| `0x0204` | `expected-close-bracket` | error | none |
| `0x0205` | `expected-close-brace` | error | none |
| `0x0206` | `expected-colon` | error | none |
| `0x0207` | `expected-property-name` | error | none |
| `0x0208` | `invalid-assignment-target` | error | none |
| `0x0209` | `optional-chain-assignment` | error | none |
| `0x020A` | `exponent-of-unary` | error | none |
| `0x020B` | `private-name-out-of-context` | error | none |
| `0x020C` | `expression-too-deep` | error | measured depth, admitted maximum |
| `0x020D` | `too-many-syntax-nodes` | error | arena kind, admitted maximum |
| `0x020E` | `syntax-not-admitted` | error | syntax feature |
| `0x020F` | `missing-initialiser` | error | none |
| `0x0210` | `invalid-arrow-parameters` | error | none |
| `0x0211` | `duplicate-binding` | error | none |
| `0x0212` | `assignment-to-constant` | error | none |
| `0x0213` | `undeclared-label` | error | none |
| `0x0214` | `illegal-break-or-continue` | error | none |
| `0x0215` | `return-outside-function` | error | none |
| `0x0216` | `strict-assignment-to-restricted-name` | error | none |
| `0x0217` | `strict-invalid-parameter` | error | none |
| `0x0218` | `eval-restricted-declaration` | error | none |

Enumerated arguments:

- arena kind: `0` nodes, `1` lists, `2` numbers, `3` scratch stack.

`assignment-to-constant` covers an imported name as well as a `const`: both
belong to something other than the code assigning to them.
- syntax feature: `0` arrow function, `1` function expression, `2` class
  expression, `3` async, `4` yield, `5` super, `6` import, `7` `new.target`,
  `8` destructuring, `9` method definition, `10` regular-expression pattern,
  `11` statement, `12` class fields.

## 6. Bytecode codes

Range `0x0400`-`0x04FF`. A span is a byte offset into the function's code, and
the first argument is the function index unless the table says otherwise.

| Code | Name | Severity | Arguments |
|---|---|---|---|
| `0x0400` | `unknown-opcode` | error | function index |
| `0x0401` | `truncated-operand` | error | function index |
| `0x0402` | `misplaced-prefix` | error | function index |
| `0x0403` | `register-out-of-range` | error | register, declared count |
| `0x0404` | `constant-out-of-range` | error | index, table size |
| `0x0405` | `invalid-jump-target` | error | target offset |
| `0x0406` | `backward-jump-without-safe-point` | error | target offset |
| `0x0407` | `invalid-exception-region` | error | function index |
| `0x0408` | `overlapping-exception-regions` | error | function index |
| `0x0409` | `context-depth-mismatch` | error | reached depth, recorded depth |
| `0x040A` | `context-depth-out-of-range` | error | depth, declared depth |
| `0x040B` | `falls-off-end` | error | function index |
| `0x040C` | `invalid-safe-point` | error | function index |
| `0x040D` | `unreachable-code` | error | function index |
| `0x040E` | `inconsistent-declared-bounds` | error | function index |
| `0x040F` | `malformed-image` | error | image failure |
| `0x0410` | `bytecode-format-mismatch` | error | image failure |
| `0x0411` | `verifier-storage-too-small` | fatal | function index |
| `0x0412` | `code-too-large` | error | none |
| `0x0413` | `too-many-constants` | error | none |
| `0x0414` | `too-many-registers` | error | none |
| `0x0415` | `jump-too-far` | error | none |
| `0x0416` | `lowering-not-admitted` | error | none |
| `0x0417` | `image-not-admitted` | error | image feature |
| `0x0418` | `feature-list-mismatch` | error | image failure |

`verifier-storage-too-small` is fatal because verification did not finish, and a
partial result would say nothing about the rest of the image.

Enumerated arguments:

- image failure: `0` magic, `1` format digest, `2` truncated, `3` overflow,
  `4` feature digest.
- image feature: `0` BigInt (retired with its code).

`feature-list-mismatch` means the image was compiled against a different
admitted language. The encoding may match exactly; the semantics behind it do
not, so the image is refused rather than reinterpreted.

`image-not-admitted` is retired: it named a construct the machine could not
run, refused when the image was admitted rather than when the construct was
reached. Every construct the admitted feature list names runs, so no path
raises it, and the number is kept rather than reassigned (§8).
`lowering-not-admitted` is the same rule at the front end: source the lowering
cannot express is refused where it is written, so a program either compiles
whole or not at all.

## 6a. Termination codes

Range `0x0500`-`0x05FF`. A termination is not a defect in a program's text, so
it carries no span: it is what happened when the program ran.

| Code | Name | Severity | Arguments |
|---|---|---|---|
| `0x0500` | `fuel-exhausted` | error | none |
| `0x0501` | `quota-exceeded` | error | none |
| `0x0502` | `cancelled` | error | none |
| `0x0503` | `deadline-reached` | error | none |
| `0x0504` | `stack-overflow` | error | none |
| `0x0505` | `registers-exhausted` | error | none |
| `0x0506` | `heap-exhausted` | error | none |
| `0x0507` | `not-implemented` | error | none |
| `0x0508` | `malformed-image-at-run-time` | error | none |
| `0x0509` | `rejected` | error | the typed cause |
| `0x050A` | `uncaught-throw` | error | none |

## 7. Reserved ranges

| Range | Phase |
|---|---|
| `0x0300`-`0x03FF` | Static semantics and early errors |
| `0x0600`-`0xFEFF` | Unassigned |
| `0xFF00`-`0xFFFF` | Reserved; never assigned, so an all-ones field is invalid |

## 8. Stability

A code, once assigned, keeps its number, its name, and its meaning. A construct
that stops being rejected retires its code rather than reusing the number for
something else, and a retired number is never reassigned. Argument order and
meaning are part of the code: a diagnostic that needs different arguments is a
new code.

Renderers key on the number, not the name. The name exists so that a code is
legible in a design document, in a conformance vector, and in a graph
configuration; it is not transmitted.

The token surface these codes describe is defined in the
[lexical grammar](../architecture/lexical-grammar.md), the parsed surface in the
[expression grammar](../architecture/expression-grammar.md), and the verified surface
in the [bytecode format](../architecture/bytecode.md).
