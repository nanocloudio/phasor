# The Library the Engine Implements

Source: `modules/common/realm.rs`, `modules/common/vm.rs`.

This document defines what a program finds in its realm before it has done
anything: the intrinsic objects, what each of them holds, and the rule that
decides whether something belongs here at all.

## 1. What belongs here

A function belongs in the library when it is a pure function of its arguments
and the heap. `Math.floor` is; `Math.random` is not, and is absent. There is no
clock, no locale, no environment, and no input: every one of those is authority,
and authority reaches a program only through an admitted binding — see
`capabilities.md`.

That rule is why the library is small enough to describe on one page, and why a
realm costs about twenty kilobytes of heap rather than a megabyte.

## 2. The intrinsics

| Object | Holds |
|---|---|
| `Object` | `keys`, `values`, `entries`, `assign`, `freeze`, `isFrozen`, `getPrototypeOf`, `setPrototypeOf`, `defineProperty`, `getOwnPropertyDescriptor`, `getOwnPropertyNames`, `create`, `is` |
| `Object.prototype` | `toString`, `valueOf`, `hasOwnProperty`, `isPrototypeOf`, `propertyIsEnumerable` |
| `Array` | `isArray`, `of`, `from` |
| `Array.prototype` | `push`, `pop`, `shift`, `unshift`, `slice`, `indexOf`, `includes`, `concat`, `join`, `map`, `filter`, `reduce`, `forEach`, `some`, `every`, `find`, `findIndex`, `reverse`, `fill`, `sort`, `values`, `keys`, `entries`, `toString`, `[Symbol.iterator]` |
| `String` | `fromCharCode` |
| `String.prototype` | `charAt`, `charCodeAt`, `codePointAt`, `at`, `indexOf`, `lastIndexOf`, `includes`, `startsWith`, `endsWith`, `slice`, `substring`, `split`, `toUpperCase`, `toLowerCase`, `trim`, `repeat`, `padStart`, `padEnd`, `concat`, `replace`, `toString`, `valueOf`, `[Symbol.iterator]` |
| `Number` | `isInteger`, `isFinite`, `isNaN`, `isSafeInteger`, `parseInt`, `parseFloat`, and the value constants |
| `Number.prototype` | `toString` with a radix, `toFixed`, `valueOf` |
| `Boolean.prototype` | `toString`, `valueOf` |
| `Symbol` | `iterator`, and the constructor that makes one |
| `BigInt` | the conversion, and `toString` with a radix and `valueOf` on its prototype |
| `Symbol.prototype` | `toString`, `description` |
| `Math` | `abs`, `floor`, `ceil`, `round`, `trunc`, `sqrt`, `pow`, `sign`, `min`, `max`, `hypot`, and `PI`, `E`, `LN2`, `SQRT2` |
| `Function` | the constructor, which compiles its body — see §8 |
| `Function.prototype` | `call`, `apply`, `bind`, and the poisoned `caller` and `arguments` accessors, which refuse |
| `Promise` | `resolve`, `reject`, and `then` on its prototype — see `jobs.md` |
| `Error` and its kinds | `name`, `message`, `toString`; the kinds are `TypeError`, `RangeError`, `ReferenceError`, `SyntaxError`, `EvalError`, and `URIError` |
| The global object | the above by name, plus `eval`, `parseInt`, `parseFloat`, `isNaN`, `isFinite`, `undefined`, `NaN`, `Infinity`, `globalThis` |

`sort` is an insertion sort: it is stable, it allocates nothing, and the arrays
an isolate this size holds are small. `toUpperCase` and `toLowerCase` convert
the ASCII and Latin-1 letters with a simple one-to-one mapping and leave
everything else as it is, rather than changing it wrongly.

## 3. Symbols

A symbol is a value whose identity is itself. Its description is held the way a
string's units are held and is only ever read: two symbols with the same
description are different values, which is the whole point of one.

A symbol is a property key like a name, and a symbol-keyed property is invisible
to `Object.keys`, to `for (x in y)`, and to `JSON`-shaped walks. `Symbol.iterator`
is an ordinary symbol the realm keeps a handle to, so a program can only reach
it through the name it is given.

## 4. Iteration

`for (x of y)` asks `y` for its iterator, and then asks the iterator for values
until it says it is done. A spread in an array literal or a call does the same.
Nothing about that path is special-cased for arrays: an object of a program's
own that carries `[Symbol.iterator]` is iterated by exactly the same code.

Arrays and strings carry iterators the engine implements itself: an array is
iterated by its values, and a string by its code points, so a surrogate pair is
one step rather than two. Every iterator the engine makes is itself iterable, so
a `for` loop over an iterator works.

`for (x in y)` walks the enumerable string keys of an object and its prototypes,
each name once however many times it appears in the chain.

## 5. BigInts

A BigInt is exact. It is a heap cell holding a sign and a little-endian
sequence of 32-bit limbs, and every operation allocates its result, so an
operand is never written to. Nothing rounds: an operation that would need more
limbs than the build admits is a range error rather than a wrong answer.

A BigInt never mixes with a Number in arithmetic. `1n + 1` is a type error,
because either direction of conversion would lose something — the exactness or
the range. Comparison is different: `1n == 1` and `1n < 1.5` compare
mathematical values, and they do it exactly, on the integer part and then on
what the fraction adds, rather than by rounding the BigInt to a double.
`Number(1n)` and `BigInt(1)` are the explicit conversions, and they are allowed
to do what the implicit ones may not.

The bitwise operations treat a value as an infinite two's-complement sequence,
so `-5n & 3n` is `3n`. There is no unsigned shift, because a value with no
width has no unsigned form.

Division on the smallest target is done a bit at a time on 32-bit values: it
has no 64-bit division instruction and no library to call for one, and a module
image may not depend on a symbol the loader will not resolve.

## 5a. Functions, prototypes, and instances

An ordinary function has a `prototype` object, and `new f()` makes an object
that inherits from it — that is what makes `A.prototype.method = ...` reach
every instance already made and every one still to come. The object is built
the first time anything asks for it rather than when the closure is made, so a
callback in a loop costs one object rather than two, and it carries
`constructor` back to the function. Neither property is enumerable.

An arrow function has no `prototype` and cannot be constructed: it has no
`this` of its own to bind. Every function carries a `length` — its declared
parameter count — and a `name`, and a non-arrow function whose body mentions
`arguments` finds there the arguments its call actually supplied: an ordinary
object with the values, a `length` of its own, and the array iterator, so
`for (x of arguments)` walks it.

`Object.defineProperty` takes either a value or a pair of accessors, and a
property defined with `get` or `set` calls them on every read or write, with the
object as the receiver.

### Listing keys is bounded

`Object.keys`, `Object.values`, `Object.entries`, `Object.assign`, the spread of
an object, and `for (x in y)` list at most 128 keys of one object. An object
with more than that holds them all and reads them all back by name; it is only
listing them that does not fit, and that is a quota the task ends on rather than
a list shorter than the object.

## 5b. What a program does not find

There is no `JSON`, `Map`, `Set`, `WeakMap`, `Date`, `console`, `setTimeout`,
or typed array. The first four are data structures a program can write for
itself; the rest are authority — a clock, a timer, an output stream — and
authority reaches a program only through an admitted binding. Each is
`undefined` rather than half-present, so a program that wants one finds out at
once.

## 6. Primitives and their prototypes

A method called on a primitive receives the primitive as its receiver rather
than a wrapper made for the occasion. `new String('a')` does make a wrapper,
which carries the primitive in an internal slot and its length as a property.
`Object(x)` makes one too. Everywhere else, a primitive stays a primitive.

## 7. Regular expressions

Source: `modules/common/regexp.rs`.

A pattern is compiled once into a small program of bytes, and the matcher runs
that program against UTF-16 code units. Backtracking uses an explicit stack in
storage the host attached rather than the machine's own, so a pathological
pattern costs fuel and storage instead of the stack. Matching is charged to the
task's fuel: a pattern that would run for ever ends the task, exactly as a loop
that would run for ever does.

The admitted grammar is: characters and the escapes `\n \t \r \f \v \0 \xHH
\uHHHH \cX`, `.`, character classes with ranges and negation, the class escapes
`\d \D \w \W \s \S`, the boundaries `\b \B`, the anchors `^ $`, groups both
capturing and not, alternation, the quantifiers `* + ? {n} {n,} {n,m}` in greedy
and lazy forms, backreferences, and lookahead both positive and negative.

Not admitted, and refused rather than mis-read: lookbehind, named groups and the
`\k` that names one, Unicode property escapes, and the `u`, `v`, and `d` flags.
The flags `g`, `i`, `m`, `s`, and `y` are admitted. Semantics are over code
units, which is what the absence of `u` means; `ignoreCase` folds the ASCII and
Latin-1 letters with a simple one-to-one mapping.

A counted repetition is written out rather than counted at run time, so
`{n,m}` costs `m` copies of its body and a pattern that asks for more than the
build admits is refused.

`RegExp.prototype` carries `exec`, `test`, and `toString`, and an expression
carries `source`, `flags`, `lastIndex`, and one boolean per flag.
`String.prototype` carries `match`, `search`, and takes a pattern in `replace`
and `split`. A `$` in a replacement names part of the match: `$&`, `` $` ``,
`$'`, `$$`, and `$1` to `$99`.

## 8. Dynamic source

`eval` and the `Function` constructor go through the same compiler, verifier,
and limits as everything else. The machine pauses on the call; the host that
carries the compiler compiles the source into a unit of its own and the
machine enters it as a frame, so the eval'd code answers the call the way any
frame answers its caller. Eval'd code runs as global code: its `var`
declarations land on the global object and its free names resolve there. A
source that does not compile throws a `SyntaxError` the program can catch, a
non-string argument to `eval` answers itself unchanged, and a host composed
without the compiler answers every pause with the `SyntaxError` the call would
produce — dynamic source is a composition choice, never an ambient service.
