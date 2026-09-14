//! The realm: a global object and the bindings a program starts with.
//!
//! A realm here is deliberately small. It holds the value properties the
//! specification puts on the global object and nothing else: no constructors,
//! no built-in functions, and no host objects, because those are either not
//! implemented or belong to a capability rather than to the language.

use crate::env;
use crate::heap::Heap;
use crate::object::{self, attribute, Descriptor, ObjectError};
use crate::string::{Atoms, Key};
use crate::value::{Handle, Value};

// One realm across these files: each child builds one family of intrinsics
// at the point `create` calls it, so the order of every definition — and
// so every own-key order — is the order written here.
#[path = "realm/arrays.rs"]
mod arrays;
#[path = "realm/buffers.rs"]
mod buffers;
#[path = "realm/collections.rs"]
mod collections;
#[path = "realm/date.rs"]
mod date;
#[path = "realm/errors.rs"]
mod errors;
#[path = "realm/functions.rs"]
mod functions;
#[path = "realm/iterators.rs"]
mod iterators;
#[path = "realm/json.rs"]
mod json;
#[path = "realm/math.rs"]
mod math;
#[path = "realm/numbers.rs"]
mod numbers;
#[path = "realm/objects.rs"]
mod objects;
#[path = "realm/promises.rs"]
mod promises;
#[path = "realm/proxy.rs"]
mod proxy;
#[path = "realm/reflect.rs"]
mod reflect;
#[path = "realm/strings.rs"]
mod strings;
#[path = "realm/symbols.rs"]
mod symbols;
#[path = "realm/weak.rs"]
mod weak;
use arrays::*;
use buffers::*;
use collections::*;
use date::*;
use errors::*;
use functions::*;
use iterators::*;
use json::*;
use math::*;
use numbers::*;
use objects::*;
use promises::*;
use proxy::*;
use reflect::*;
use strings::*;
use symbols::*;
use weak::*;

/// The functions the engine implements itself.
pub mod native {
    /// `Object.prototype.toString`.
    pub const OBJECT_TO_STRING: u32 = 0;
    /// `Object.prototype.valueOf`.
    pub const OBJECT_VALUE_OF: u32 = 1;
    /// `Array.prototype.toString`, which joins with commas.
    pub const ARRAY_TO_STRING: u32 = 2;
    /// `Array.prototype.join`.
    pub const ARRAY_JOIN: u32 = 3;
    /// `Error.prototype.toString`.
    pub const ERROR_TO_STRING: u32 = 4;
    /// The error constructors, which differ only in the prototype they install.
    pub const ERROR: u32 = 8;
    pub const TYPE_ERROR: u32 = 9;
    pub const RANGE_ERROR: u32 = 10;
    pub const REFERENCE_ERROR: u32 = 11;
    pub const SYNTAX_ERROR: u32 = 12;
    /// `Promise.prototype.then`.
    pub const PROMISE_THEN: u32 = 16;
    /// `Promise.resolve` and `Promise.reject`.
    pub const PROMISE_RESOLVE: u32 = 17;
    pub const PROMISE_REJECT: u32 = 18;
    /// The `Promise` constructor.
    pub const PROMISE: u32 = 19;
    /// The functions an executor is called with, each bound to its promise.
    pub const PROMISE_SETTLE_FULFILLED: u32 = 20;
    pub const PROMISE_SETTLE_REJECTED: u32 = 21;
    // Symbols.
    pub const SYMBOL: u32 = 24;
    pub const SYMBOL_TO_STRING: u32 = 25;
    pub const SYMBOL_DESCRIPTION: u32 = 26;
    pub const SYMBOL_FOR: u32 = 27;

    // Wrappers and their constructors.
    pub const OBJECT: u32 = 32;
    pub const ARRAY: u32 = 33;
    pub const STRING: u32 = 34;
    pub const NUMBER: u32 = 35;
    pub const BOOLEAN: u32 = 36;
    pub const FUNCTION_PROTOTYPE_CALL: u32 = 37;
    pub const FUNCTION_PROTOTYPE_APPLY: u32 = 38;
    pub const FUNCTION_PROTOTYPE_BIND: u32 = 39;
    pub const BOUND_FUNCTION: u32 = 40;
    pub const BIG_INT: u32 = 41;
    pub const BIG_INT_TO_STRING: u32 = 42;
    pub const BIG_INT_VALUE_OF: u32 = 43;

    // Regular expressions.
    pub const REG_EXP: u32 = 44;
    pub const REG_EXP_EXEC: u32 = 45;
    pub const REG_EXP_TEST: u32 = 46;
    pub const REG_EXP_TO_STRING: u32 = 47;
    pub const STRING_MATCH: u32 = 119;
    pub const STRING_SEARCH: u32 = 120;
    pub const STRING_MATCH_ALL: u32 = 121;
    /// Reads one export through a module namespace, so what it answers is what
    /// the module holds now.
    pub const NAMESPACE_GET: u32 = 122;

    // `Object` statics and methods.
    pub const OBJECT_KEYS: u32 = 48;
    pub const OBJECT_VALUES: u32 = 49;
    pub const OBJECT_ENTRIES: u32 = 50;
    pub const OBJECT_ASSIGN: u32 = 51;
    pub const OBJECT_FREEZE: u32 = 52;
    pub const OBJECT_IS_FROZEN: u32 = 53;
    pub const OBJECT_GET_PROTOTYPE_OF: u32 = 54;
    pub const OBJECT_SET_PROTOTYPE_OF: u32 = 55;
    pub const OBJECT_DEFINE_PROPERTY: u32 = 56;
    pub const OBJECT_GET_OWN_PROPERTY_NAMES: u32 = 57;
    pub const OBJECT_CREATE: u32 = 58;
    pub const OBJECT_HAS_OWN_PROPERTY: u32 = 59;
    pub const OBJECT_IS_PROTOTYPE_OF: u32 = 60;
    pub const OBJECT_PROPERTY_IS_ENUMERABLE: u32 = 61;
    pub const OBJECT_IS: u32 = 62;

    // `Array` statics and methods.
    pub const ARRAY_IS_ARRAY: u32 = 64;
    pub const ARRAY_OF: u32 = 65;
    pub const ARRAY_FROM: u32 = 66;
    pub const ARRAY_PUSH: u32 = 67;
    pub const ARRAY_POP: u32 = 68;
    pub const ARRAY_SHIFT: u32 = 69;
    pub const ARRAY_UNSHIFT: u32 = 70;
    pub const ARRAY_SLICE: u32 = 71;
    pub const ARRAY_INDEX_OF: u32 = 72;
    pub const ARRAY_INCLUDES: u32 = 73;
    pub const ARRAY_CONCAT: u32 = 74;
    pub const ARRAY_MAP: u32 = 75;
    pub const ARRAY_FILTER: u32 = 76;
    pub const ARRAY_REDUCE: u32 = 77;
    pub const ARRAY_FOR_EACH: u32 = 78;
    pub const ARRAY_SOME: u32 = 79;
    pub const ARRAY_EVERY: u32 = 80;
    pub const ARRAY_FIND: u32 = 81;
    pub const ARRAY_FIND_INDEX: u32 = 82;
    pub const ARRAY_REVERSE: u32 = 83;
    pub const ARRAY_VALUES: u32 = 84;
    pub const ARRAY_KEYS: u32 = 85;
    pub const ARRAY_ENTRIES: u32 = 86;
    pub const ARRAY_FILL: u32 = 87;
    pub const ARRAY_SORT: u32 = 88;

    // `String` statics and methods.
    pub const STRING_FROM_CHAR_CODE: u32 = 96;
    pub const STRING_CHAR_AT: u32 = 97;
    pub const STRING_CHAR_CODE_AT: u32 = 98;
    pub const STRING_CODE_POINT_AT: u32 = 99;
    pub const STRING_INDEX_OF: u32 = 100;
    pub const STRING_LAST_INDEX_OF: u32 = 101;
    pub const STRING_INCLUDES: u32 = 102;
    pub const STRING_STARTS_WITH: u32 = 103;
    pub const STRING_ENDS_WITH: u32 = 104;
    pub const STRING_SLICE: u32 = 105;
    pub const STRING_SUBSTRING: u32 = 106;
    pub const STRING_TO_UPPER_CASE: u32 = 107;
    pub const STRING_TO_LOWER_CASE: u32 = 108;
    pub const STRING_TRIM: u32 = 109;
    pub const STRING_SPLIT: u32 = 110;
    pub const STRING_REPEAT: u32 = 111;
    pub const STRING_PAD_START: u32 = 112;
    pub const STRING_PAD_END: u32 = 113;
    pub const STRING_CONCAT: u32 = 114;
    pub const STRING_AT: u32 = 115;
    pub const STRING_REPLACE: u32 = 116;
    pub const STRING_TO_STRING: u32 = 117;
    pub const STRING_VALUES: u32 = 118;

    // `Number` statics and methods.
    pub const NUMBER_IS_INTEGER: u32 = 128;
    pub const NUMBER_IS_FINITE: u32 = 129;
    pub const NUMBER_IS_NAN: u32 = 130;
    pub const NUMBER_IS_SAFE_INTEGER: u32 = 131;
    pub const NUMBER_TO_STRING: u32 = 132;
    pub const NUMBER_TO_FIXED: u32 = 133;
    pub const NUMBER_VALUE_OF: u32 = 134;
    pub const PARSE_INT: u32 = 135;
    pub const PARSE_FLOAT: u32 = 136;
    pub const IS_NAN: u32 = 137;
    pub const IS_FINITE: u32 = 138;
    pub const BOOLEAN_TO_STRING: u32 = 139;
    pub const BOOLEAN_VALUE_OF: u32 = 140;

    // `Math`.
    pub const MATH_ABS: u32 = 144;
    pub const MATH_FLOOR: u32 = 145;
    pub const MATH_CEIL: u32 = 146;
    pub const MATH_ROUND: u32 = 147;
    pub const MATH_TRUNC: u32 = 148;
    pub const MATH_SQRT: u32 = 149;
    pub const MATH_POW: u32 = 150;
    pub const MATH_MIN: u32 = 151;
    pub const MATH_MAX: u32 = 152;
    pub const MATH_SIGN: u32 = 153;
    pub const MATH_HYPOT: u32 = 154;

    // Iteration.
    pub const ITERATOR_NEXT: u32 = 160;
    pub const ITERATOR_SELF: u32 = 161;
    /// The `Function` constructor. Building a function from source needs the
    /// compiler, which lives outside the machine, so calling it is refused.
    pub const FUNCTION: u32 = 162;
    /// `Object.getOwnPropertyDescriptor`.
    pub const OBJECT_GET_OWN_PROPERTY_DESCRIPTOR: u32 = 163;
    /// `eval`. The machine pauses on it; the host compiles and enters the
    /// unit, so the compiler stays outside the machine.
    pub const EVAL: u32 = 164;
    /// `EvalError` and `URIError`, kept for their names: nothing in the
    /// engine throws either, but a program may.
    pub const EVAL_ERROR: u32 = 165;
    pub const URI_ERROR: u32 = 166;
    /// The thrower behind `Function.prototype.caller` and `.arguments`:
    /// reading or writing either is a type error.
    pub const THROW_TYPE_ERROR: u32 = 167;
    /// `Function.prototype` called as a function: any arguments, undefined.
    pub const FUNCTION_PROTOTYPE: u32 = 168;
    /// Resume a suspended async frame with the fulfilled value.
    pub const ASYNC_RESUME_FULFILLED: u32 = 169;
    /// Resume a suspended async frame by throwing the rejection reason at
    /// its `await`.
    pub const ASYNC_RESUME_REJECTED: u32 = 170;
    /// A host-installed `print`: records what was printed in the machine for
    /// the host to read. Never part of the standard realm.
    pub const PRINT: u32 = 171;
    /// A class's default constructor: base does nothing, derived forwards
    /// its arguments to the parent constructor.
    pub const DEFAULT_CONSTRUCTOR: u32 = 172;
    /// `next` on a generator: resume its frame with the argument.
    pub const GENERATOR_NEXT: u32 = 173;
    /// `return` on a generator: finish it with the argument.
    pub const GENERATOR_RETURN: u32 = 174;
    /// `throw` on a generator: throw the argument in at its `yield`.
    pub const GENERATOR_THROW: u32 = 175;
    /// Deliver an async generator's awaited yield value to its pending
    /// `next`.
    pub const ASYNC_GEN_YIELD_FULFILLED: u32 = 180;
    /// A yielded value that rejects finishes the async generator and
    /// rejects its pending `next`.
    pub const ASYNC_GEN_YIELD_REJECTED: u32 = 181;
    /// Resume an async generator for the next queued request.
    pub const ASYNC_GEN_DRAIN: u32 = 182;
    /// `Promise.prototype.catch`: `then` with only a rejection handler.
    pub const PROMISE_CATCH: u32 = 183;
    /// `Promise.prototype.finally`: the callback runs either way and the
    /// settlement passes through.
    pub const PROMISE_FINALLY: u32 = 184;
    /// The pass-through half of `finally`: run the callback, answer the
    /// original value.
    pub const PROMISE_FINALLY_STEP: u32 = 185;
    /// The rejection half of `finally`: run the callback, rethrow.
    pub const PROMISE_FINALLY_RETHROW: u32 = 186;
    /// `Promise.all`.
    pub const PROMISE_ALL: u32 = 187;
    /// `Promise.race`.
    pub const PROMISE_RACE: u32 = 188;
    /// `Promise.allSettled`.
    pub const PROMISE_ALL_SETTLED: u32 = 189;
    /// `Promise.any`.
    pub const PROMISE_ANY: u32 = 190;
    /// One combinator element fulfilled.
    pub const COMBINE_FULFILLED: u32 = 191;
    /// One combinator element rejected.
    pub const COMBINE_REJECTED: u32 = 192;
    /// `Object.prototype.__defineGetter__`.
    pub const OBJECT_DEFINE_GETTER: u32 = 193;
    /// `Object.prototype.__defineSetter__`.
    pub const OBJECT_DEFINE_SETTER: u32 = 194;
    /// `Object.prototype.__lookupGetter__`.
    pub const OBJECT_LOOKUP_GETTER: u32 = 195;
    /// `Object.prototype.__lookupSetter__`.
    pub const OBJECT_LOOKUP_SETTER: u32 = 196;
    /// `Reflect.get`.
    pub const REFLECT_GET: u32 = 197;
    /// `Reflect.set`.
    pub const REFLECT_SET: u32 = 198;
    /// `Reflect.has`.
    pub const REFLECT_HAS: u32 = 199;
    /// `Reflect.deleteProperty`.
    pub const REFLECT_DELETE: u32 = 200;
    /// `Reflect.ownKeys`.
    pub const REFLECT_OWN_KEYS: u32 = 201;
    /// `Reflect.getPrototypeOf`.
    pub const REFLECT_GET_PROTOTYPE: u32 = 202;
    /// `Reflect.setPrototypeOf`.
    pub const REFLECT_SET_PROTOTYPE: u32 = 203;
    /// `Reflect.isExtensible`.
    pub const REFLECT_IS_EXTENSIBLE: u32 = 204;
    /// `Reflect.preventExtensions`.
    pub const REFLECT_PREVENT_EXTENSIONS: u32 = 205;
    /// `Reflect.defineProperty`.
    pub const REFLECT_DEFINE_PROPERTY: u32 = 206;
    /// `Reflect.getOwnPropertyDescriptor`.
    pub const REFLECT_GET_OWN_DESCRIPTOR: u32 = 207;
    /// `Reflect.apply`.
    pub const REFLECT_APPLY: u32 = 208;
    /// `Reflect.construct`.
    pub const REFLECT_CONSTRUCT: u32 = 209;
    /// `Object.getOwnPropertySymbols`.
    pub const OBJECT_GET_OWN_PROPERTY_SYMBOLS: u32 = 210;
    /// `Map`.
    pub const MAP: u32 = 211;
    /// `Set`.
    pub const SET: u32 = 212;
    /// `Map.prototype.get`.
    pub const MAP_GET: u32 = 213;
    /// `Map.prototype.set`.
    pub const MAP_SET: u32 = 214;
    /// `Map.prototype.has`.
    pub const MAP_HAS: u32 = 215;
    /// `Map.prototype.delete`.
    pub const MAP_DELETE: u32 = 216;
    /// `Map.prototype.clear`.
    pub const MAP_CLEAR: u32 = 217;
    /// `Map.prototype.forEach`.
    pub const MAP_FOR_EACH: u32 = 218;
    /// The `Map.prototype.size` getter.
    pub const MAP_SIZE: u32 = 219;
    /// `Map.prototype.entries`, also `Symbol.iterator`.
    pub const MAP_ENTRIES: u32 = 220;
    /// `Map.prototype.keys`.
    pub const MAP_KEYS: u32 = 221;
    /// `Map.prototype.values`.
    pub const MAP_VALUES: u32 = 222;
    /// `Set.prototype.add`.
    pub const SET_ADD: u32 = 223;
    /// `Set.prototype.has`.
    pub const SET_HAS: u32 = 224;
    /// `Set.prototype.delete`.
    pub const SET_DELETE: u32 = 225;
    /// `Set.prototype.clear`.
    pub const SET_CLEAR: u32 = 226;
    /// `Set.prototype.forEach`.
    pub const SET_FOR_EACH: u32 = 227;
    /// The `Set.prototype.size` getter.
    pub const SET_SIZE: u32 = 228;
    /// `Set.prototype.entries`.
    pub const SET_ENTRIES: u32 = 229;
    /// `Set.prototype.values`, also `keys` and `Symbol.iterator`.
    pub const SET_VALUES: u32 = 230;
    /// `encodeURI`.
    pub const ENCODE_URI: u32 = 231;
    /// `encodeURIComponent`.
    pub const ENCODE_URI_COMPONENT: u32 = 232;
    /// `decodeURI`.
    pub const DECODE_URI: u32 = 233;
    /// `decodeURIComponent`.
    pub const DECODE_URI_COMPONENT: u32 = 234;
    /// `Math.sin`, and the transcendental family after it.
    pub const MATH_SIN: u32 = 235;
    pub const MATH_COS: u32 = 236;
    pub const MATH_TAN: u32 = 237;
    pub const MATH_ASIN: u32 = 238;
    pub const MATH_ACOS: u32 = 239;
    pub const MATH_ATAN: u32 = 240;
    pub const MATH_ATAN2: u32 = 241;
    pub const MATH_EXP: u32 = 242;
    pub const MATH_LOG: u32 = 243;
    pub const MATH_LOG2: u32 = 244;
    pub const MATH_LOG10: u32 = 245;
    pub const MATH_CBRT: u32 = 246;
    /// `Object.defineProperties`.
    pub const OBJECT_DEFINE_PROPERTIES: u32 = 247;
    /// `Symbol.prototype.valueOf`.
    pub const SYMBOL_VALUE_OF: u32 = 248;
    /// `SuppressedError`.
    pub const SUPPRESSED_ERROR: u32 = 249;
    /// `Number.prototype.toExponential`.
    pub const NUMBER_TO_EXPONENTIAL: u32 = 250;
    /// `Number.prototype.toPrecision`.
    pub const NUMBER_TO_PRECISION: u32 = 251;
    /// The reaction that answers an async generator's `return` request once
    /// the value it was given has been awaited.
    pub const ASYNC_GEN_RETURN_FULFILLED: u32 = 252;
    /// As above, for a value whose await rejected.
    pub const ASYNC_GEN_RETURN_REJECTED: u32 = 253;
    /// `next`, `return`, and `throw` of the wrapper that lets a sync iterator
    /// stand where an async one is wanted: each answers a promise settled
    /// once the sync result's value has been awaited.
    pub const ASYNC_FROM_SYNC_NEXT: u32 = 254;
    pub const ASYNC_FROM_SYNC_RETURN: u32 = 255;
    pub const ASYNC_FROM_SYNC_THROW: u32 = 256;
    /// The wrapper's reactions: an awaited value becomes an iteration result,
    /// not done or done; a rejection closes the sync iterator, or passes.
    pub const ASYNC_FROM_SYNC_MORE: u32 = 257;
    pub const ASYNC_FROM_SYNC_DONE: u32 = 258;
    pub const ASYNC_FROM_SYNC_CLOSE: u32 = 259;
    pub const ASYNC_FROM_SYNC_PASS: u32 = 260;
    /// `GeneratorFunction` and `AsyncGeneratorFunction`: the constructors
    /// generator functions answer to, refusing to build from source as
    /// `Function` does.
    pub const GENERATOR_FUNCTION: u32 = 261;
    pub const ASYNC_GENERATOR_FUNCTION: u32 = 262;
    /// `$262.evalScript`, which a conformance host installs: the source
    /// compiles as a script of its own and runs as global code.
    pub const EVAL_SCRIPT: u32 = 263;
    /// `AggregateError`.
    pub const AGGREGATE_ERROR: u32 = 264;
    /// `WeakRef`, and `WeakRef.prototype.deref`.
    pub const WEAK_REF: u32 = 265;
    pub const WEAK_REF_DEREF: u32 = 266;
    /// `Math.random`: a deterministic sequence unless the host seeds it.
    pub const MATH_RANDOM: u32 = 267;
    /// `JSON.parse` and `JSON.stringify`.
    pub const JSON_PARSE: u32 = 268;
    pub const JSON_STRINGIFY: u32 = 269;
    /// `Date`, its statics, and its prototype's methods. Time is UTC
    /// throughout: the host's zone is not a thing the engine knows.
    pub const DATE: u32 = 270;
    pub const DATE_NOW: u32 = 271;
    pub const DATE_UTC: u32 = 272;
    pub const DATE_PARSE: u32 = 273;
    pub const DATE_GET_TIME: u32 = 274;
    pub const DATE_GET_FULL_YEAR: u32 = 275;
    pub const DATE_GET_MONTH: u32 = 276;
    pub const DATE_GET_DATE: u32 = 277;
    pub const DATE_GET_DAY: u32 = 278;
    pub const DATE_GET_HOURS: u32 = 279;
    pub const DATE_GET_MINUTES: u32 = 280;
    pub const DATE_GET_SECONDS: u32 = 281;
    pub const DATE_GET_MILLISECONDS: u32 = 282;
    pub const DATE_GET_TIMEZONE_OFFSET: u32 = 283;
    pub const DATE_TO_STRING: u32 = 284;
    pub const DATE_TO_ISO_STRING: u32 = 285;
    pub const DATE_TO_UTC_STRING: u32 = 286;
    pub const DATE_TO_DATE_STRING: u32 = 287;
    pub const DATE_TO_TIME_STRING: u32 = 288;
    pub const DATE_TO_JSON: u32 = 289;
    pub const DATE_SET_TIME: u32 = 290;
    pub const DATE_TO_PRIMITIVE: u32 = 291;
    pub const DATE_SET_FULL_YEAR: u32 = 292;
    pub const DATE_SET_MONTH: u32 = 293;
    pub const DATE_SET_DATE: u32 = 294;
    pub const DATE_SET_HOURS: u32 = 295;
    pub const DATE_SET_MINUTES: u32 = 296;
    pub const DATE_SET_SECONDS: u32 = 297;
    pub const DATE_SET_MILLISECONDS: u32 = 298;
    /// `Proxy`, and the body of a proxy over a callable target.
    pub const PROXY: u32 = 299;
    pub const PROXY_CALL: u32 = 300;
    /// `ArrayBuffer` and `ArrayBuffer.prototype.slice`, `Uint8Array`, and
    /// `DataView`: bytes held as numbers in an array of their own.
    pub const ARRAY_BUFFER: u32 = 301;
    pub const ARRAY_BUFFER_SLICE: u32 = 302;
    /// Every typed array constructor: the kind is a record on the constructor.
    pub const TYPED_ARRAY: u32 = 303;
    pub const DATA_VIEW: u32 = 304;
    /// The `Symbol.species` getter, which answers its receiver.
    pub const SPECIES_GETTER: u32 = 305;
    /// `$262.createRealm`: a fresh realm of intrinsics beside this one.
    pub const CREATE_REALM: u32 = 306;
    pub const WEAK_MAP: u32 = 307;
    pub const WEAK_SET: u32 = 308;
    pub const WEAK_MAP_GET: u32 = 309;
    pub const WEAK_MAP_SET: u32 = 310;
    pub const WEAK_MAP_HAS: u32 = 311;
    pub const WEAK_MAP_DELETE: u32 = 312;
    pub const WEAK_SET_ADD: u32 = 313;
    pub const WEAK_SET_HAS: u32 = 314;
    pub const WEAK_SET_DELETE: u32 = 315;
    /// `Proxy.revocable` and the `revoke` function it hands back.
    pub const PROXY_REVOCABLE: u32 = 316;
    pub const PROXY_REVOKE: u32 = 317;
    /// `AsyncFunction`, the constructor async functions are instances of.
    pub const ASYNC_FUNCTION: u32 = 318;
    /// `%TypedArray%`, its statics, and the accessors and methods of its
    /// prototype; the buffer family's additions.
    pub const TYPED_ARRAY_BASE: u32 = 319;
    pub const TYPED_ARRAY_OF: u32 = 320;
    pub const TYPED_ARRAY_FROM: u32 = 321;
    pub const TYPED_ARRAY_LENGTH: u32 = 322;
    pub const TYPED_ARRAY_BYTE_LENGTH: u32 = 323;
    pub const TYPED_ARRAY_BYTE_OFFSET: u32 = 324;
    pub const TYPED_ARRAY_BUFFER: u32 = 325;
    pub const TYPED_ARRAY_VALUES: u32 = 326;
    pub const TYPED_ARRAY_KEYS: u32 = 327;
    pub const TYPED_ARRAY_ENTRIES: u32 = 328;
    pub const TYPED_ARRAY_TAG: u32 = 329;
    pub const TYPED_ARRAY_SUBARRAY: u32 = 330;
    pub const TYPED_ARRAY_SET: u32 = 331;
    pub const TYPED_ARRAY_FILL: u32 = 332;
    pub const ARRAY_BUFFER_RESIZE: u32 = 333;
    pub const ARRAY_BUFFER_BYTE_LENGTH: u32 = 334;
    pub const ARRAY_BUFFER_MAX_BYTE_LENGTH: u32 = 335;
    pub const ARRAY_BUFFER_RESIZABLE: u32 = 336;
    pub const SHARED_ARRAY_BUFFER: u32 = 337;
    /// The getter and setter behind an `accessor` field, each carrying the
    /// hidden name it reads or writes on its receiver.
    pub const ACCESSOR_GET: u32 = 338;
    pub const ACCESSOR_SET: u32 = 339;
    pub const ARRAY_BUFFER_IMMUTABLE: u32 = 340;
    pub const ARRAY_BUFFER_TRANSFER_TO_IMMUTABLE: u32 = 341;
    pub const DYNAMIC_IMPORT_REJECTED: u32 = 342;
    pub const DYNAMIC_IMPORT_STEP: u32 = 343;
    pub const PROMISE_WITH_RESOLVERS: u32 = 344;
    /// `__decodeUtf8`: UTF-8 bytes, one per unit, decoded into text.
    pub const DECODE_UTF8: u32 = 345;
    /// `__encodeUtf8`: text encoded as UTF-8, one byte per unit.
    pub const ENCODE_UTF8: u32 = 346;
    pub const OBJECT_PREVENT_EXTENSIONS: u32 = 176;
    pub const OBJECT_IS_EXTENSIBLE: u32 = 177;
    pub const OBJECT_SEAL: u32 = 178;
    pub const OBJECT_IS_SEALED: u32 = 179;

    /// What a native reports as its `length`: the parameter count the
    /// specification declares for it.
    ///
    /// A bound function never reaches here: `bind` settles its `length` from
    /// the target's own. A host binding takes whatever its manifest declares,
    /// which this cannot know, so it falls to one.
    pub const fn arity(id: u32) -> u32 {
        match id {
            // Nothing declared: a method that reads only its receiver, an
            // iterator step, or a constructor that takes no argument.
            self::OBJECT_TO_STRING
            | self::OBJECT_VALUE_OF
            | self::ARRAY_TO_STRING
            | self::ERROR_TO_STRING
            | self::SYMBOL
            | self::SYMBOL_TO_STRING
            | self::SYMBOL_DESCRIPTION
            | self::FUNCTION_PROTOTYPE
            | self::THROW_TYPE_ERROR
            | self::BIG_INT_TO_STRING
            | self::BIG_INT_VALUE_OF
            | self::REG_EXP_TO_STRING
            | self::NAMESPACE_GET
            | self::ARRAY_OF
            | self::ARRAY_POP
            | self::ARRAY_SHIFT
            | self::ARRAY_REVERSE
            | self::ARRAY_VALUES
            | self::ARRAY_KEYS
            | self::ARRAY_ENTRIES
            | self::STRING_TO_UPPER_CASE
            | self::STRING_TO_LOWER_CASE
            | self::STRING_TRIM
            | self::STRING_TO_STRING
            | self::STRING_VALUES
            | self::NUMBER_VALUE_OF
            | self::BOOLEAN_TO_STRING
            | self::BOOLEAN_VALUE_OF
            | self::ITERATOR_NEXT
            | self::ITERATOR_SELF
            | self::MAP
            | self::SET
            | self::WEAK_MAP
            | self::WEAK_SET
            | self::PROXY_REVOKE => 0,
            // Two declared.
            self::PROMISE_THEN
            | self::REG_EXP
            | self::OBJECT_ASSIGN
            | self::OBJECT_SET_PROTOTYPE_OF
            | self::OBJECT_CREATE
            | self::OBJECT_IS
            | self::OBJECT_GET_OWN_PROPERTY_DESCRIPTOR
            | self::ARRAY_SLICE
            | self::STRING_SLICE
            | self::STRING_SUBSTRING
            | self::STRING_SPLIT
            | self::STRING_REPLACE
            | self::PARSE_INT
            | self::MATH_POW
            | self::MATH_MIN
            | self::MATH_MAX
            | self::MATH_HYPOT
            | self::OBJECT_DEFINE_PROPERTIES
            | self::FUNCTION_PROTOTYPE_APPLY
            | self::MAP_SET
            | self::WEAK_MAP_SET
            | self::PROXY_REVOCABLE => 2,
            // Three declared.
            self::OBJECT_DEFINE_PROPERTY | self::TYPED_ARRAY => 3,
            // One declared, which is what the great majority take.
            _ => 1,
        }
    }

    /// The first identifier a host binding takes. A binding's own index is
    /// added to it, so every admitted binding is its own callable function.
    pub const BINDING_BASE: u32 = 1024;
}

/// The error kinds the engine itself throws.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    Error,
    Type,
    Range,
    Reference,
    Syntax,
    Eval,
    Uri,
    Suppressed,
    Aggregate,
}

impl ErrorKind {
    /// The constructor identifier that builds this kind.
    pub const fn native(self) -> u32 {
        match self {
            Self::Error => native::ERROR,
            Self::Type => native::TYPE_ERROR,
            Self::Range => native::RANGE_ERROR,
            Self::Reference => native::REFERENCE_ERROR,
            Self::Syntax => native::SYNTAX_ERROR,
            Self::Eval => native::EVAL_ERROR,
            Self::Uri => native::URI_ERROR,
            Self::Suppressed => native::SUPPRESSED_ERROR,
            Self::Aggregate => native::AGGREGATE_ERROR,
        }
    }

    /// The name the specification gives this kind.
    pub const fn name(self) -> &'static [u8] {
        match self {
            Self::Error => b"Error",
            Self::Type => b"TypeError",
            Self::Range => b"RangeError",
            Self::Reference => b"ReferenceError",
            Self::Syntax => b"SyntaxError",
            Self::Eval => b"EvalError",
            Self::Uri => b"URIError",
            Self::Suppressed => b"SuppressedError",
            Self::Aggregate => b"AggregateError",
        }
    }
}

/// A created realm.
#[derive(Clone, Copy, Debug)]
pub struct Realm {
    pub global: Handle,
    pub environment: Handle,
    /// The global lexical environment: every script's top-level `let`,
    /// `const`, and `class` binds here by name, above the global object.
    /// A record that fills is followed by a fresh one threaded beneath it.
    pub lexical: Handle,
    /// The names scripts have declared with `var` or as functions — the
    /// global environment's [[VarNames]] — held as bindings of a record.
    pub var_names: Handle,
    /// The prototype an object literal starts with.
    pub object_prototype: Handle,
    /// The prototype an array literal starts with.
    pub array_prototype: Handle,
    /// The prototype a function starts with.
    pub function_prototype: Handle,
    /// `Error.prototype`.
    pub error_prototype: Handle,
    /// The prototypes of the error kinds, in the order of `ErrorKind`.
    pub error_prototypes: [Handle; 9],
    /// `Promise.prototype`.
    pub promise_prototype: Handle,
    /// `Promise` itself, which awaiting compares a promise's `constructor`
    /// against to await it directly.
    pub promise_constructor: Handle,
    /// `String.prototype`, `Number.prototype`, `Boolean.prototype`, and
    /// `Symbol.prototype`, which a method called on a primitive reaches.
    pub string_prototype: Handle,
    pub number_prototype: Handle,
    pub boolean_prototype: Handle,
    pub symbol_prototype: Handle,
    /// `BigInt.prototype`.
    pub big_int_prototype: Handle,
    /// `RegExp.prototype`.
    pub regexp_prototype: Handle,
    /// The prototype every iterator this engine makes shares.
    pub iterator_prototype: Handle,
    /// What a generator function is an instance of: its `prototype` names
    /// the object generator instances default to.
    pub generator_function_prototype: Handle,
    /// The default prototype of a generator object.
    pub generator_object_prototype: Handle,
    /// What an async generator function is an instance of.
    pub async_generator_function_prototype: Handle,
    /// What an async function is an instance of.
    pub async_function_prototype: Handle,
    /// The default prototype of an async generator object.
    pub async_generator_object_prototype: Handle,
    /// `Map.prototype` and `Set.prototype`.
    pub map_prototype: Handle,
    /// `WeakRef.prototype`: a WeakRef holds its target as strongly as any
    /// reference, since when a target is collected is unobservable here.
    pub weak_ref_prototype: Handle,
    /// `Date.prototype`: a date is an ordinary object over it holding its
    /// time value, milliseconds since the epoch in UTC.
    pub date_prototype: Handle,
    /// `ArrayBuffer.prototype`, `Uint8Array.prototype`, and
    /// `DataView.prototype`.
    pub array_buffer_prototype: Handle,
    pub data_view_prototype: Handle,
    /// `%TypedArray%.prototype`, and the prototype of each kind under it,
    /// indexed by kind.
    pub typed_array_prototype: Handle,
    pub typed_array_prototypes: [Handle; TYPED_ARRAY_KINDS],
    /// `SharedArrayBuffer.prototype`: a buffer no thread shares here, since
    /// the engine has none, but one the family constructs over.
    pub shared_array_buffer_prototype: Handle,
    pub set_prototype: Handle,
    /// `WeakMap.prototype` and `WeakSet.prototype`: keyed by objects and
    /// symbols only, and never enumerated, so a collection that is never
    /// observed to happen costs nothing to leave undone.
    pub weak_map_prototype: Handle,
    pub weak_set_prototype: Handle,
    /// `Symbol.iterator`, the name a program asks an object to iterate by.
    pub iterator_symbol: Handle,
    pub async_iterator_symbol: Handle,
    /// `Symbol.dispose` and `Symbol.asyncDispose`, the names a `using`
    /// declaration disposes a resource by.
    pub dispose_symbol: Handle,
    pub async_dispose_symbol: Handle,
    pub has_instance_symbol: Handle,
    pub to_primitive_symbol: Handle,
    pub to_string_tag_symbol: Handle,
    pub species_symbol: Handle,
    pub unscopables_symbol: Handle,
    /// The three hint strings `Symbol.toPrimitive` receives, made once.
    pub hint_default: Handle,
    pub hint_number: Handle,
    pub hint_string: Handle,
}

impl Realm {
    /// The prototype an error of this kind is created with.
    pub fn prototype_of(&self, kind: ErrorKind) -> Handle {
        let index = match kind {
            ErrorKind::Error => 0,
            ErrorKind::Type => 1,
            ErrorKind::Range => 2,
            ErrorKind::Reference => 3,
            ErrorKind::Syntax => 4,
            ErrorKind::Eval => 5,
            ErrorKind::Uri => 6,
            ErrorKind::Suppressed => 7,
            ErrorKind::Aggregate => 8,
        };
        self.error_prototypes[index]
    }

    /// The error kind a constructor identifier builds.
    pub const fn kind_of(native: u32) -> Option<ErrorKind> {
        match native {
            self::native::ERROR => Some(ErrorKind::Error),
            self::native::TYPE_ERROR => Some(ErrorKind::Type),
            self::native::RANGE_ERROR => Some(ErrorKind::Range),
            self::native::REFERENCE_ERROR => Some(ErrorKind::Reference),
            self::native::SYNTAX_ERROR => Some(ErrorKind::Syntax),
            self::native::EVAL_ERROR => Some(ErrorKind::Eval),
            self::native::URI_ERROR => Some(ErrorKind::Uri),
            self::native::SUPPRESSED_ERROR => Some(ErrorKind::Suppressed),
            self::native::AGGREGATE_ERROR => Some(ErrorKind::Aggregate),
            _ => None,
        }
    }
}

/// Create a realm: a global object carrying `globalThis`, `undefined`, `NaN`,
/// and `Infinity`, and the environment whose bindings are its properties.
pub fn create(heap: &mut Heap<'_>, atoms: &mut Atoms<'_>) -> Result<Realm, ObjectError> {
    // The intrinsic prototypes come first, because everything else has one.
    let object_prototype = object::create(heap, Value::NULL)?;
    // The function prototype is itself callable — a function that accepts
    // any arguments and answers undefined — which is what the specification
    // makes it.
    let function_prototype = object::create_native(
        heap,
        Value::object(object_prototype),
        native::FUNCTION_PROTOTYPE,
        0,
    )?;
    let array_prototype = object::create(heap, Value::object(object_prototype))?;

    let method_attributes = attribute::WRITABLE | attribute::CONFIGURABLE;
    for (target, name, id) in [
        (object_prototype, &b"toString"[..], native::OBJECT_TO_STRING),
        (object_prototype, &b"valueOf"[..], native::OBJECT_VALUE_OF),
        (array_prototype, &b"toString"[..], native::ARRAY_TO_STRING),
        (array_prototype, &b"join"[..], native::ARRAY_JOIN),
    ] {
        let function = object::create_native(heap, Value::object(function_prototype), id, 0)?;
        let key = key_of(heap, atoms, name)?;
        object::define_own_property(
            heap,
            target,
            key,
            Descriptor::data(Value::object(function), method_attributes),
        )?;
    }

    let (error_prototype, error_prototypes, global) = build_errors(
        heap,
        atoms,
        function_prototype,
        method_attributes,
        object_prototype,
    )?;
    let (promise_prototype, promise_constructor) = build_promises(
        heap,
        atoms,
        function_prototype,
        global,
        method_attributes,
        object_prototype,
    )?;
    let symbols = build_symbols(
        heap,
        atoms,
        function_prototype,
        global,
        method_attributes,
        object_prototype,
    )?;
    // The bundle travels whole to the builders that need several of these;
    // the rest of this function names them one by one.
    let Symbols {
        symbol_prototype,
        iterator: iterator_symbol,
        async_iterator: async_iterator_symbol,
        dispose: dispose_symbol,
        async_dispose: async_dispose_symbol,
        to_primitive: to_primitive_symbol,
        to_string_tag: to_string_tag_symbol,
        species: species_symbol,
        unscopables: unscopables_symbol,
        hint_default,
        hint_number,
        hint_string,
        has_instance: has_instance_symbol,
    } = symbols;
    let Prototypes {
        string: string_prototype,
        number: number_prototype,
        boolean: boolean_prototype,
        iterator: iterator_prototype,
        generator_function: generator_function_prototype,
        generator_object: generator_object_prototype,
        async_generator_function: async_generator_function_prototype,
        async_generator_object: async_generator_object_prototype,
        async_function: async_function_prototype,
        big_int: big_int_prototype,
        regexp: regexp_prototype,
    } = build_functions(
        heap,
        atoms,
        function_prototype,
        global,
        method_attributes,
        object_prototype,
        symbols,
    )?;
    build_iterators(
        heap,
        atoms,
        function_prototype,
        iterator_prototype,
        iterator_symbol,
        method_attributes,
    )?;
    let (map_prototype, set_prototype) = build_collections(
        heap,
        atoms,
        function_prototype,
        global,
        iterator_symbol,
        method_attributes,
        object_prototype,
    )?;
    build_reflect(
        heap,
        atoms,
        function_prototype,
        global,
        method_attributes,
        object_prototype,
    )?;
    build_object_intrinsics(heap, atoms, global, object_prototype, function_prototype)?;
    build_array_intrinsics(
        heap,
        atoms,
        global,
        array_prototype,
        function_prototype,
        iterator_symbol,
    )?;
    build_string_intrinsics(
        heap,
        atoms,
        global,
        string_prototype,
        function_prototype,
        iterator_symbol,
    )?;
    // The string prototype's own `length` is zero: it is the empty string's
    // shape, whatever carries it.
    define(
        heap,
        atoms,
        string_prototype,
        b"length",
        Value::number(0.0),
        0,
    )?;
    build_number_intrinsics(
        heap,
        atoms,
        global,
        number_prototype,
        boolean_prototype,
        function_prototype,
    )?;
    build_math(heap, atoms, global, object_prototype, function_prototype)?;
    build_function_intrinsics(heap, atoms, function_prototype)?;
    let (weak_ref_prototype, weak_map_prototype, weak_set_prototype) = build_weak(
        heap,
        atoms,
        function_prototype,
        global,
        object_prototype,
        to_string_tag_symbol,
    )?;
    let date_prototype = build_date(
        heap,
        atoms,
        function_prototype,
        global,
        object_prototype,
        to_primitive_symbol,
        to_string_tag_symbol,
    )?;
    build_proxy(heap, atoms, function_prototype, global)?;
    let (
        array_buffer_prototype,
        shared_array_buffer_prototype,
        typed_array_prototype,
        typed_array_prototypes,
        data_view_prototype,
    ) = build_buffers(
        heap,
        atoms,
        function_prototype,
        global,
        object_prototype,
        symbols,
    )?;
    build_json(
        heap,
        atoms,
        function_prototype,
        global,
        object_prototype,
        to_string_tag_symbol,
    )?;
    let environment = env::create_object_environment(heap, Value::UNDEFINED, global)
        .map_err(|_| ObjectError::Heap(crate::heap::HeapError::ArenaFull))?;
    let lexical = env::create(
        heap,
        env::EnvironmentKind::Declarative,
        Value::object(environment),
        GLOBAL_LEXICAL_CAPACITY,
    )
    .map_err(|_| ObjectError::Heap(crate::heap::HeapError::ArenaFull))?;
    let var_names = env::create(
        heap,
        env::EnvironmentKind::Declarative,
        Value::UNDEFINED,
        GLOBAL_LEXICAL_CAPACITY,
    )
    .map_err(|_| ObjectError::Heap(crate::heap::HeapError::ArenaFull))?;
    Ok(Realm {
        global,
        environment,
        lexical,
        var_names,
        object_prototype,
        array_prototype,
        function_prototype,
        error_prototype,
        error_prototypes,
        promise_prototype,
        promise_constructor,
        string_prototype,
        number_prototype,
        boolean_prototype,
        symbol_prototype,
        big_int_prototype,
        regexp_prototype,
        iterator_prototype,
        generator_function_prototype,
        generator_object_prototype,
        async_generator_function_prototype,
        async_generator_object_prototype,
        async_function_prototype,
        map_prototype,
        weak_ref_prototype,
        date_prototype,
        array_buffer_prototype,
        data_view_prototype,
        typed_array_prototype,
        typed_array_prototypes,
        shared_array_buffer_prototype,
        set_prototype,
        weak_map_prototype,
        weak_set_prototype,
        iterator_symbol,
        async_iterator_symbol,
        dispose_symbol,
        async_dispose_symbol,
        has_instance_symbol,
        to_primitive_symbol,
        to_string_tag_symbol,
        species_symbol,
        unscopables_symbol,
        hint_default,
        hint_number,
        hint_string,
    })
}

/// Give a host's realm a `print` function, which records its argument in the
/// machine for the host to read. A deployment realm never carries it: it
/// exists for conformance oracles whose test protocol reports through it.
/// Bindings the global lexical record and the var-name record start with;
/// a record that fills gets another threaded beneath it.
pub const GLOBAL_LEXICAL_CAPACITY: u32 = 32;

/// Install `$262`, the host object a conformance runner provides: its
/// `evalScript` compiles a source as a script of its own — declaration
/// instantiation and all — and `global` names the global object.
pub fn install_262(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    realm: &Realm,
) -> Result<(), ObjectError> {
    let host = object::create(heap, Value::object(realm.object_prototype))?;
    method(
        heap,
        atoms,
        host,
        b"evalScript",
        native::EVAL_SCRIPT,
        realm.function_prototype,
    )?;
    method(
        heap,
        atoms,
        host,
        b"createRealm",
        native::CREATE_REALM,
        realm.function_prototype,
    )?;
    define(
        heap,
        atoms,
        host,
        b"global",
        Value::object(realm.global),
        attribute::WRITABLE | attribute::CONFIGURABLE,
    )?;
    define(
        heap,
        atoms,
        realm.global,
        b"$262",
        Value::object(host),
        attribute::WRITABLE | attribute::CONFIGURABLE,
    )
}

/// Install `Math.random`, which a host that grants randomness does: the
/// realm on its own has none, and a program granted nothing finds
/// `Math.random` undefined. What is installed draws from a sequence the
/// machine seeds, replayable exactly.
pub fn install_random(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    realm: &Realm,
) -> Result<(), ObjectError> {
    let key = key_of(heap, atoms, b"Math")?;
    let math = object::get_own_property(heap, realm.global, key)?
        .map(|descriptor| descriptor.value)
        .unwrap_or(Value::UNDEFINED);
    if !math.is_object() {
        return Ok(());
    }
    method(
        heap,
        atoms,
        math.as_handle(),
        b"random",
        native::MATH_RANDOM,
        realm.function_prototype,
    )
}

pub fn install_print(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    realm: &Realm,
) -> Result<(), ObjectError> {
    method(
        heap,
        atoms,
        realm.global,
        b"print",
        native::PRINT,
        realm.function_prototype,
    )
}

/// Put a native method on an object under an ASCII name.
fn method(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    target: Handle,
    name: &[u8],
    id: u32,
    function_prototype: Handle,
) -> Result<(), ObjectError> {
    let function = object::create_native(heap, Value::object(function_prototype), id, 0)?;
    let key = key_of(heap, atoms, name)?;
    object::define_own_property(
        heap,
        target,
        key,
        Descriptor::data(
            Value::object(function),
            attribute::WRITABLE | attribute::CONFIGURABLE,
        ),
    )?;
    Ok(())
}

/// One property an intrinsic carries, as a row of an `install` table.
#[derive(Clone, Copy)]
pub enum Entry<'a> {
    /// A native method under an ASCII name: writable and configurable.
    Method(&'a [u8], u32),
    /// A native getter under an ASCII name: configurable, no setter.
    Getter(&'a [u8], u32),
}

/// Put every row of `entries` on `target`, in order.
///
/// The table is a by-value local array at every call site, `let entries =
/// [...]`, never `&[...]` and never a `static` or `const` item: a borrowed
/// literal of constant rows is promoted to read-only data whose byte-string
/// pointers need relocations, and a loaded module image gets none.
pub fn install(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    target: Handle,
    function_prototype: Handle,
    entries: &[Entry<'_>],
) -> Result<(), ObjectError> {
    for entry in entries {
        match *entry {
            Entry::Method(name, id) => method(heap, atoms, target, name, id, function_prototype)?,
            Entry::Getter(name, id) => accessor(heap, atoms, target, name, id, function_prototype)?,
        }
    }
    Ok(())
}

/// Put a native getter on an object under an ASCII name.
fn accessor(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    target: Handle,
    name: &[u8],
    id: u32,
    function_prototype: Handle,
) -> Result<(), ObjectError> {
    let getter = object::create_native(heap, Value::object(function_prototype), id, 0)?;
    let key = key_of(heap, atoms, name)?;
    object::define_own_property(
        heap,
        target,
        key,
        Descriptor::accessor(
            Value::object(getter),
            Value::UNDEFINED,
            attribute::CONFIGURABLE,
        ),
    )?;
    Ok(())
}

/// The typed array kinds, in the order their constructors are made.
pub const TYPED_ARRAY_KINDS: usize = 11;

/// The names of the typed array kinds, padded to one width: bytes, not
/// slices, so the table carries no pointer a loaded module would have to
/// relocate.
const TYPED_ARRAY_NAMES: [[u8; 17]; TYPED_ARRAY_KINDS] = [
    *b"Int8Array\0\0\0\0\0\0\0\0",
    *b"Uint8Array\0\0\0\0\0\0\0",
    *b"Uint8ClampedArray",
    *b"Int16Array\0\0\0\0\0\0\0",
    *b"Uint16Array\0\0\0\0\0\0",
    *b"Int32Array\0\0\0\0\0\0\0",
    *b"Uint32Array\0\0\0\0\0\0",
    *b"Float32Array\0\0\0\0\0",
    *b"Float64Array\0\0\0\0\0",
    *b"BigInt64Array\0\0\0\0",
    *b"BigUint64Array\0\0\0",
];

/// The name of a typed array kind: the padded bytes and how many count.
pub fn typed_array_name(kind: u8) -> ([u8; 17], usize) {
    let name = TYPED_ARRAY_NAMES
        .get(usize::from(kind))
        .copied()
        .unwrap_or(TYPED_ARRAY_NAMES[TYPED_ARRAY_KINDS - 1]);
    let mut length = 0usize;
    while length < name.len() && name[length] != 0 {
        length += 1;
    }
    (name, length)
}

/// Bytes one element of a kind takes.
pub const fn typed_array_element_size(kind: u8) -> u32 {
    match kind {
        0..=2 => 1,
        3 | 4 => 2,
        5..=7 => 4,
        _ => 8,
    }
}

/// Build a constructor: a callable that a program reaches by name, carrying its
/// prototype, with the prototype carrying it back.
/// A constructor no global names, reached through the `constructor` of
/// its prototype: `prototype` and `name` as any constructor carries.
fn intrinsic_constructor(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    name: &[u8],
    id: u32,
    prototype: Handle,
    function_prototype: Handle,
) -> Result<Handle, ObjectError> {
    let handle = object::create_native(
        heap,
        Value::object(function_prototype),
        id,
        object::function_flag::CONSTRUCTOR,
    )?;
    define(
        heap,
        atoms,
        handle,
        b"prototype",
        Value::object(prototype),
        0,
    )?;
    define(
        heap,
        atoms,
        prototype,
        b"constructor",
        Value::object(handle),
        attribute::CONFIGURABLE,
    )?;
    let mut units = [0u16; 32];
    let mut length = 0usize;
    for &byte in name {
        if length < units.len() {
            units[length] = u16::from(byte);
            length += 1;
        }
    }
    let text = atoms.intern(heap, units.get(..length).unwrap_or(&[]))?;
    define(
        heap,
        atoms,
        handle,
        b"name",
        Value::string(text),
        attribute::CONFIGURABLE,
    )?;
    Ok(handle)
}

fn constructor(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    global: Handle,
    name: &[u8],
    id: u32,
    prototype: Handle,
    function_prototype: Handle,
) -> Result<Handle, ObjectError> {
    let attributes = attribute::WRITABLE | attribute::CONFIGURABLE;
    let handle = object::create_native(
        heap,
        Value::object(function_prototype),
        id,
        object::function_flag::CONSTRUCTOR,
    )?;
    define(
        heap,
        atoms,
        handle,
        b"prototype",
        Value::object(prototype),
        0,
    )?;
    define(
        heap,
        atoms,
        prototype,
        b"constructor",
        Value::object(handle),
        attributes,
    )?;
    define(heap, atoms, global, name, Value::object(handle), attributes)?;
    // Its `name` is the name it was defined under, which the harness of a
    // conformance suite reads to say what it expected.
    let mut units = [0u16; 32];
    let mut length = 0usize;
    for &byte in name {
        if length < units.len() {
            units[length] = u16::from(byte);
            length += 1;
        }
    }
    let text = atoms.intern(heap, units.get(..length).unwrap_or(&[]))?;
    define(
        heap,
        atoms,
        handle,
        b"name",
        Value::string(text),
        attribute::CONFIGURABLE,
    )?;
    Ok(handle)
}

fn define(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    global: Handle,
    name: &[u8],
    value: Value,
    attributes: u8,
) -> Result<(), ObjectError> {
    let key = key_of(heap, atoms, name)?;
    object::define_own_property(heap, global, key, Descriptor::data(value, attributes))?;
    Ok(())
}

/// Intern an ASCII name as a property key.
pub fn key_of(heap: &mut Heap<'_>, atoms: &mut Atoms<'_>, name: &[u8]) -> Result<Key, ObjectError> {
    let mut units = [0u16; 32];
    let mut length = 0usize;
    for &byte in name {
        if length < units.len() {
            units[length] = u16::from(byte);
            length += 1;
        }
    }
    let handle = atoms.intern(heap, units.get(..length).unwrap_or(&[]))?;
    Ok(Key::Name(handle))
}
