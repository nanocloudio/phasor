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
        }
    }
}

/// A created realm.
#[derive(Clone, Copy, Debug)]
pub struct Realm {
    pub global: Handle,
    pub environment: Handle,
    /// The prototype an object literal starts with.
    pub object_prototype: Handle,
    /// The prototype an array literal starts with.
    pub array_prototype: Handle,
    /// The prototype a function starts with.
    pub function_prototype: Handle,
    /// `Error.prototype`.
    pub error_prototype: Handle,
    /// The prototypes of the error kinds, in the order of `ErrorKind`.
    pub error_prototypes: [Handle; 7],
    /// `Promise.prototype`.
    pub promise_prototype: Handle,
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
    /// `Symbol.iterator`, the name a program asks an object to iterate by.
    pub iterator_symbol: Handle,
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
            _ => None,
        }
    }
}

/// Create a realm: a global object carrying `globalThis`, `undefined`, `NaN`,
/// and `Infinity`, and the environment whose bindings are its properties.
pub fn create(heap: &mut Heap<'_>, atoms: &mut Atoms<'_>) -> Result<Realm, ObjectError> {
    // The intrinsic prototypes come first, because everything else has one.
    let object_prototype = object::create(heap, Value::NULL)?;
    let function_prototype = object::create(heap, Value::object(object_prototype))?;
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

    // `Error.prototype` and one prototype per error kind, each carrying its
    // name and an empty message, which is what the specification puts there.
    let error_prototype = object::create(heap, Value::object(object_prototype))?;
    let empty = crate::string::create_ascii(heap, b"")?;
    define(
        heap,
        atoms,
        error_prototype,
        b"message",
        Value::string(empty),
        method_attributes,
    )?;
    let to_string = object::create_native(
        heap,
        Value::object(function_prototype),
        native::ERROR_TO_STRING,
        0,
    )?;
    let key = key_of(heap, atoms, b"toString")?;
    object::define_own_property(
        heap,
        error_prototype,
        key,
        Descriptor::data(Value::object(to_string), method_attributes),
    )?;

    let kinds = [
        ErrorKind::Error,
        ErrorKind::Type,
        ErrorKind::Range,
        ErrorKind::Reference,
        ErrorKind::Syntax,
        ErrorKind::Eval,
        ErrorKind::Uri,
    ];
    let mut error_prototypes = [error_prototype; 7];
    let global = object::create(heap, Value::object(object_prototype))?;
    // The global object's shape is known here: the intrinsics, the value
    // properties, and the error constructors.
    object::reserve(heap, global, 24)?;
    for (index, kind) in kinds.iter().enumerate() {
        let prototype = if matches!(kind, ErrorKind::Error) {
            error_prototype
        } else {
            object::create(heap, Value::object(error_prototype))?
        };
        let name = crate::string::create_ascii(heap, kind.name())?;
        define(
            heap,
            atoms,
            prototype,
            b"name",
            Value::string(name),
            method_attributes,
        )?;
        error_prototypes[index] = prototype;

        // The constructor, which the program reaches by name.
        let constructor = object::create_native(
            heap,
            Value::object(function_prototype),
            kind.native(),
            object::function_flag::CONSTRUCTOR,
        )?;
        let key = key_of(heap, atoms, b"prototype")?;
        object::define_own_property(
            heap,
            constructor,
            key,
            Descriptor::data(Value::object(prototype), 0),
        )?;
        let key = key_of(heap, atoms, b"constructor")?;
        object::define_own_property(
            heap,
            prototype,
            key,
            Descriptor::data(Value::object(constructor), method_attributes),
        )?;
        define(
            heap,
            atoms,
            global,
            kind.name(),
            Value::object(constructor),
            method_attributes,
        )?;
        // The constructor's own `name`, which is what says which error a
        // failed expectation was.
        define(
            heap,
            atoms,
            constructor,
            b"name",
            Value::string(name),
            attribute::CONFIGURABLE,
        )?;
    }

    // `undefined`, `NaN`, and `Infinity` are not writable, not enumerable, and
    // not configurable.
    define(heap, atoms, global, b"undefined", Value::UNDEFINED, 0)?;
    define(heap, atoms, global, b"NaN", Value::number(f64::NAN), 0)?;
    define(
        heap,
        atoms,
        global,
        b"Infinity",
        Value::number(f64::INFINITY),
        0,
    )?;
    // `globalThis` is writable and configurable but not enumerable.
    define(
        heap,
        atoms,
        global,
        b"globalThis",
        Value::object(global),
        attribute::WRITABLE | attribute::CONFIGURABLE,
    )?;

    // Promises: a prototype carrying `then`, and a constructor carrying
    // `resolve` and `reject`.
    let promise_prototype = object::create(heap, Value::object(object_prototype))?;
    let then = object::create_native(
        heap,
        Value::object(function_prototype),
        native::PROMISE_THEN,
        0,
    )?;
    let key = key_of(heap, atoms, b"then")?;
    object::define_own_property(
        heap,
        promise_prototype,
        key,
        Descriptor::data(Value::object(then), method_attributes),
    )?;

    let promise_constructor = object::create_native(
        heap,
        Value::object(function_prototype),
        native::PROMISE,
        object::function_flag::CONSTRUCTOR,
    )?;
    let key = key_of(heap, atoms, b"prototype")?;
    object::define_own_property(
        heap,
        promise_constructor,
        key,
        Descriptor::data(Value::object(promise_prototype), 0),
    )?;
    let key = key_of(heap, atoms, b"constructor")?;
    object::define_own_property(
        heap,
        promise_prototype,
        key,
        Descriptor::data(Value::object(promise_constructor), method_attributes),
    )?;
    for (name, id) in [
        (&b"resolve"[..], native::PROMISE_RESOLVE),
        (&b"reject"[..], native::PROMISE_REJECT),
    ] {
        let function = object::create_native(heap, Value::object(function_prototype), id, 0)?;
        let key = key_of(heap, atoms, name)?;
        object::define_own_property(
            heap,
            promise_constructor,
            key,
            Descriptor::data(Value::object(function), method_attributes),
        )?;
    }
    define(
        heap,
        atoms,
        global,
        b"Promise",
        Value::object(promise_constructor),
        method_attributes,
    )?;

    // Symbols. A well-known symbol is an ordinary symbol the realm keeps a
    // handle to, so a program can only reach it through the name it is given.
    let symbol_prototype = object::create(heap, Value::object(object_prototype))?;
    let iterator_symbol = crate::string::create_symbol_ascii(heap, b"Symbol.iterator")?;
    let symbol_constructor =
        object::create_native(heap, Value::object(function_prototype), native::SYMBOL, 0)?;
    method(
        heap,
        atoms,
        symbol_prototype,
        b"toString",
        native::SYMBOL_TO_STRING,
        function_prototype,
    )?;
    accessor(
        heap,
        atoms,
        symbol_prototype,
        b"description",
        native::SYMBOL_DESCRIPTION,
        function_prototype,
    )?;
    define(
        heap,
        atoms,
        symbol_constructor,
        b"prototype",
        Value::object(symbol_prototype),
        0,
    )?;
    define(
        heap,
        atoms,
        symbol_constructor,
        b"iterator",
        Value::symbol(iterator_symbol),
        0,
    )?;
    define(
        heap,
        atoms,
        symbol_prototype,
        b"constructor",
        Value::object(symbol_constructor),
        method_attributes,
    )?;
    define(
        heap,
        atoms,
        global,
        b"Symbol",
        Value::object(symbol_constructor),
        method_attributes,
    )?;

    // The prototypes a method called on a primitive reaches.
    let string_prototype = object::create(heap, Value::object(object_prototype))?;
    let number_prototype = object::create(heap, Value::object(object_prototype))?;
    let boolean_prototype = object::create(heap, Value::object(object_prototype))?;
    let iterator_prototype = object::create(heap, Value::object(object_prototype))?;
    let big_int_prototype = object::create(heap, Value::object(object_prototype))?;
    let regexp_prototype = object::create(heap, Value::object(object_prototype))?;
    object::reserve(heap, regexp_prototype, 6)?;
    constructor(
        heap,
        atoms,
        global,
        b"RegExp",
        native::REG_EXP,
        regexp_prototype,
        function_prototype,
    )?;
    for (name, id) in [
        (&b"exec"[..], native::REG_EXP_EXEC),
        (&b"test"[..], native::REG_EXP_TEST),
        (&b"toString"[..], native::REG_EXP_TO_STRING),
    ] {
        method(heap, atoms, regexp_prototype, name, id, function_prototype)?;
    }

    // `BigInt` is callable but not constructible: there is no wrapper to make
    // with `new`, only the conversion.
    let big_int_constructor =
        object::create_native(heap, Value::object(function_prototype), native::BIG_INT, 0)?;
    define(
        heap,
        atoms,
        big_int_constructor,
        b"prototype",
        Value::object(big_int_prototype),
        0,
    )?;
    define(
        heap,
        atoms,
        big_int_prototype,
        b"constructor",
        Value::object(big_int_constructor),
        method_attributes,
    )?;
    for (name, id) in [
        (&b"toString"[..], native::BIG_INT_TO_STRING),
        (&b"valueOf"[..], native::BIG_INT_VALUE_OF),
    ] {
        method(heap, atoms, big_int_prototype, name, id, function_prototype)?;
    }
    define(
        heap,
        atoms,
        global,
        b"BigInt",
        Value::object(big_int_constructor),
        method_attributes,
    )?;

    // Every iterator the engine makes is iterable by itself, which is what a
    // `for` loop over an iterator needs.
    method(
        heap,
        atoms,
        iterator_prototype,
        b"next",
        native::ITERATOR_NEXT,
        function_prototype,
    )?;
    let self_iterator = object::create_native(
        heap,
        Value::object(function_prototype),
        native::ITERATOR_SELF,
        0,
    )?;
    object::define_own_property(
        heap,
        iterator_prototype,
        Key::Symbol(iterator_symbol),
        Descriptor::data(Value::object(self_iterator), method_attributes),
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

    let environment = env::create_object_environment(heap, Value::UNDEFINED, global)
        .map_err(|_| ObjectError::Heap(crate::heap::HeapError::ArenaFull))?;
    Ok(Realm {
        global,
        environment,
        object_prototype,
        array_prototype,
        function_prototype,
        error_prototype,
        error_prototypes,
        promise_prototype,
        string_prototype,
        number_prototype,
        boolean_prototype,
        symbol_prototype,
        big_int_prototype,
        regexp_prototype,
        iterator_prototype,
        iterator_symbol,
    })
}

/// `Object`, its statics, and the methods every object inherits.
fn build_object_intrinsics(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    global: Handle,
    object_prototype: Handle,
    function_prototype: Handle,
) -> Result<(), ObjectError> {
    object::reserve(heap, object_prototype, 8)?;
    let handle = constructor(
        heap,
        atoms,
        global,
        b"Object",
        native::OBJECT,
        object_prototype,
        function_prototype,
    )?;
    for (name, id) in [
        (&b"keys"[..], native::OBJECT_KEYS),
        (&b"values"[..], native::OBJECT_VALUES),
        (&b"entries"[..], native::OBJECT_ENTRIES),
        (&b"assign"[..], native::OBJECT_ASSIGN),
    ] {
        method(heap, atoms, handle, name, id, function_prototype)?;
    }
    for (name, id) in [
        (&b"freeze"[..], native::OBJECT_FREEZE),
        (&b"isFrozen"[..], native::OBJECT_IS_FROZEN),
        (&b"getPrototypeOf"[..], native::OBJECT_GET_PROTOTYPE_OF),
        (&b"setPrototypeOf"[..], native::OBJECT_SET_PROTOTYPE_OF),
    ] {
        method(heap, atoms, handle, name, id, function_prototype)?;
    }
    for (name, id) in [
        (&b"defineProperty"[..], native::OBJECT_DEFINE_PROPERTY),
        (
            &b"getOwnPropertyNames"[..],
            native::OBJECT_GET_OWN_PROPERTY_NAMES,
        ),
        (&b"create"[..], native::OBJECT_CREATE),
        (&b"is"[..], native::OBJECT_IS),
        (
            &b"getOwnPropertyDescriptor"[..],
            native::OBJECT_GET_OWN_PROPERTY_DESCRIPTOR,
        ),
    ] {
        method(heap, atoms, handle, name, id, function_prototype)?;
    }
    for (name, id) in [
        (&b"hasOwnProperty"[..], native::OBJECT_HAS_OWN_PROPERTY),
        (&b"isPrototypeOf"[..], native::OBJECT_IS_PROTOTYPE_OF),
        (
            &b"propertyIsEnumerable"[..],
            native::OBJECT_PROPERTY_IS_ENUMERABLE,
        ),
    ] {
        method(heap, atoms, object_prototype, name, id, function_prototype)?;
    }
    Ok(())
}

/// `Array`, its statics, and the methods an array inherits.
fn build_array_intrinsics(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    global: Handle,
    array_prototype: Handle,
    function_prototype: Handle,
    iterator_symbol: Handle,
) -> Result<(), ObjectError> {
    object::reserve(heap, array_prototype, 28)?;
    let handle = constructor(
        heap,
        atoms,
        global,
        b"Array",
        native::ARRAY,
        array_prototype,
        function_prototype,
    )?;
    for (name, id) in [
        (&b"isArray"[..], native::ARRAY_IS_ARRAY),
        (&b"of"[..], native::ARRAY_OF),
        (&b"from"[..], native::ARRAY_FROM),
    ] {
        method(heap, atoms, handle, name, id, function_prototype)?;
    }
    for (name, id) in [
        (&b"push"[..], native::ARRAY_PUSH),
        (&b"pop"[..], native::ARRAY_POP),
        (&b"shift"[..], native::ARRAY_SHIFT),
        (&b"unshift"[..], native::ARRAY_UNSHIFT),
    ] {
        method(heap, atoms, array_prototype, name, id, function_prototype)?;
    }
    for (name, id) in [
        (&b"slice"[..], native::ARRAY_SLICE),
        (&b"indexOf"[..], native::ARRAY_INDEX_OF),
        (&b"includes"[..], native::ARRAY_INCLUDES),
        (&b"concat"[..], native::ARRAY_CONCAT),
    ] {
        method(heap, atoms, array_prototype, name, id, function_prototype)?;
    }
    for (name, id) in [
        (&b"map"[..], native::ARRAY_MAP),
        (&b"filter"[..], native::ARRAY_FILTER),
        (&b"reduce"[..], native::ARRAY_REDUCE),
        (&b"forEach"[..], native::ARRAY_FOR_EACH),
    ] {
        method(heap, atoms, array_prototype, name, id, function_prototype)?;
    }
    for (name, id) in [
        (&b"some"[..], native::ARRAY_SOME),
        (&b"every"[..], native::ARRAY_EVERY),
        (&b"find"[..], native::ARRAY_FIND),
        (&b"findIndex"[..], native::ARRAY_FIND_INDEX),
    ] {
        method(heap, atoms, array_prototype, name, id, function_prototype)?;
    }
    for (name, id) in [
        (&b"reverse"[..], native::ARRAY_REVERSE),
        (&b"fill"[..], native::ARRAY_FILL),
        (&b"sort"[..], native::ARRAY_SORT),
    ] {
        method(heap, atoms, array_prototype, name, id, function_prototype)?;
    }
    for (name, id) in [
        (&b"values"[..], native::ARRAY_VALUES),
        (&b"keys"[..], native::ARRAY_KEYS),
        (&b"entries"[..], native::ARRAY_ENTRIES),
    ] {
        method(heap, atoms, array_prototype, name, id, function_prototype)?;
    }
    // An array is iterated by its values, which is what `Symbol.iterator` says.
    let values = object::create_native(
        heap,
        Value::object(function_prototype),
        native::ARRAY_VALUES,
        0,
    )?;
    object::define_own_property(
        heap,
        array_prototype,
        Key::Symbol(iterator_symbol),
        Descriptor::data(
            Value::object(values),
            attribute::WRITABLE | attribute::CONFIGURABLE,
        ),
    )?;
    Ok(())
}

/// `String` and the methods a string reaches.
fn build_string_intrinsics(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    global: Handle,
    string_prototype: Handle,
    function_prototype: Handle,
    iterator_symbol: Handle,
) -> Result<(), ObjectError> {
    object::reserve(heap, string_prototype, 28)?;
    let handle = constructor(
        heap,
        atoms,
        global,
        b"String",
        native::STRING,
        string_prototype,
        function_prototype,
    )?;
    method(
        heap,
        atoms,
        handle,
        b"fromCharCode",
        native::STRING_FROM_CHAR_CODE,
        function_prototype,
    )?;
    for (name, id) in [
        (&b"charAt"[..], native::STRING_CHAR_AT),
        (&b"charCodeAt"[..], native::STRING_CHAR_CODE_AT),
        (&b"codePointAt"[..], native::STRING_CODE_POINT_AT),
        (&b"at"[..], native::STRING_AT),
    ] {
        method(heap, atoms, string_prototype, name, id, function_prototype)?;
    }
    for (name, id) in [
        (&b"indexOf"[..], native::STRING_INDEX_OF),
        (&b"lastIndexOf"[..], native::STRING_LAST_INDEX_OF),
        (&b"includes"[..], native::STRING_INCLUDES),
        (&b"startsWith"[..], native::STRING_STARTS_WITH),
    ] {
        method(heap, atoms, string_prototype, name, id, function_prototype)?;
    }
    for (name, id) in [
        (&b"endsWith"[..], native::STRING_ENDS_WITH),
        (&b"slice"[..], native::STRING_SLICE),
        (&b"substring"[..], native::STRING_SUBSTRING),
        (&b"split"[..], native::STRING_SPLIT),
    ] {
        method(heap, atoms, string_prototype, name, id, function_prototype)?;
    }
    for (name, id) in [
        (&b"toUpperCase"[..], native::STRING_TO_UPPER_CASE),
        (&b"toLowerCase"[..], native::STRING_TO_LOWER_CASE),
        (&b"trim"[..], native::STRING_TRIM),
        (&b"repeat"[..], native::STRING_REPEAT),
    ] {
        method(heap, atoms, string_prototype, name, id, function_prototype)?;
    }
    for (name, id) in [
        (&b"padStart"[..], native::STRING_PAD_START),
        (&b"padEnd"[..], native::STRING_PAD_END),
        (&b"concat"[..], native::STRING_CONCAT),
        (&b"replace"[..], native::STRING_REPLACE),
    ] {
        method(heap, atoms, string_prototype, name, id, function_prototype)?;
    }
    for (name, id) in [
        (&b"toString"[..], native::STRING_TO_STRING),
        (&b"valueOf"[..], native::STRING_TO_STRING),
        (&b"match"[..], native::STRING_MATCH),
        (&b"search"[..], native::STRING_SEARCH),
    ] {
        method(heap, atoms, string_prototype, name, id, function_prototype)?;
    }
    // A string is iterated by its code points, not by its code units.
    let values = object::create_native(
        heap,
        Value::object(function_prototype),
        native::STRING_VALUES,
        0,
    )?;
    object::define_own_property(
        heap,
        string_prototype,
        Key::Symbol(iterator_symbol),
        Descriptor::data(
            Value::object(values),
            attribute::WRITABLE | attribute::CONFIGURABLE,
        ),
    )?;
    Ok(())
}

/// `Number`, `Boolean`, and the conversions that live on the global object.
fn build_number_intrinsics(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    global: Handle,
    number_prototype: Handle,
    boolean_prototype: Handle,
    function_prototype: Handle,
) -> Result<(), ObjectError> {
    object::reserve(heap, number_prototype, 6)?;
    let handle = constructor(
        heap,
        atoms,
        global,
        b"Number",
        native::NUMBER,
        number_prototype,
        function_prototype,
    )?;
    for (name, id) in [
        (&b"isInteger"[..], native::NUMBER_IS_INTEGER),
        (&b"isFinite"[..], native::NUMBER_IS_FINITE),
        (&b"isNaN"[..], native::NUMBER_IS_NAN),
        (&b"isSafeInteger"[..], native::NUMBER_IS_SAFE_INTEGER),
    ] {
        method(heap, atoms, handle, name, id, function_prototype)?;
    }
    for (name, id) in [
        (&b"parseInt"[..], native::PARSE_INT),
        (&b"parseFloat"[..], native::PARSE_FLOAT),
    ] {
        method(heap, atoms, handle, name, id, function_prototype)?;
    }
    for (name, value) in [
        (&b"MAX_SAFE_INTEGER"[..], 9_007_199_254_740_991.0f64),
        (&b"MIN_SAFE_INTEGER"[..], -9_007_199_254_740_991.0f64),
        (&b"EPSILON"[..], f64::EPSILON),
        (&b"POSITIVE_INFINITY"[..], f64::INFINITY),
    ] {
        define(heap, atoms, handle, name, Value::number(value), 0)?;
    }
    for (name, value) in [
        (&b"NEGATIVE_INFINITY"[..], f64::NEG_INFINITY),
        (&b"NaN"[..], f64::NAN),
        (&b"MAX_VALUE"[..], f64::MAX),
        (&b"MIN_VALUE"[..], f64::MIN_POSITIVE),
    ] {
        define(heap, atoms, handle, name, Value::number(value), 0)?;
    }
    for (name, id) in [
        (&b"toString"[..], native::NUMBER_TO_STRING),
        (&b"toFixed"[..], native::NUMBER_TO_FIXED),
        (&b"valueOf"[..], native::NUMBER_VALUE_OF),
    ] {
        method(heap, atoms, number_prototype, name, id, function_prototype)?;
    }

    constructor(
        heap,
        atoms,
        global,
        b"Boolean",
        native::BOOLEAN,
        boolean_prototype,
        function_prototype,
    )?;
    // `Function` exists so a function's constructor, `Function.prototype`,
    // and `f instanceof Function` all answer; only building one from source
    // is refused, because the compiler lives outside the machine.
    constructor(
        heap,
        atoms,
        global,
        b"Function",
        native::FUNCTION,
        function_prototype,
        function_prototype,
    )?;
    for (name, id) in [
        (&b"toString"[..], native::BOOLEAN_TO_STRING),
        (&b"valueOf"[..], native::BOOLEAN_VALUE_OF),
    ] {
        method(heap, atoms, boolean_prototype, name, id, function_prototype)?;
    }

    // `eval` is a function like any other; only the machine knows that a call
    // to it pauses for the compiler.
    {
        let function =
            object::create_native(heap, Value::object(function_prototype), native::EVAL, 0)?;
        define(
            heap,
            atoms,
            global,
            b"eval",
            Value::object(function),
            attribute::WRITABLE | attribute::CONFIGURABLE,
        )?;
    }
    for (name, id) in [
        (&b"parseInt"[..], native::PARSE_INT),
        (&b"parseFloat"[..], native::PARSE_FLOAT),
        (&b"isNaN"[..], native::IS_NAN),
        (&b"isFinite"[..], native::IS_FINITE),
    ] {
        method(heap, atoms, global, name, id, function_prototype)?;
    }
    Ok(())
}

/// `Math`, which holds no state and no authority: every function of it is a
/// pure function of its arguments. There is no `random`, because randomness is
/// a capability rather than something an engine may help itself to.
fn build_math(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    global: Handle,
    object_prototype: Handle,
    function_prototype: Handle,
) -> Result<(), ObjectError> {
    let math = object::create(heap, Value::object(object_prototype))?;
    object::reserve(heap, math, 20)?;
    for (name, id) in [
        (&b"abs"[..], native::MATH_ABS),
        (&b"floor"[..], native::MATH_FLOOR),
        (&b"ceil"[..], native::MATH_CEIL),
        (&b"round"[..], native::MATH_ROUND),
    ] {
        method(heap, atoms, math, name, id, function_prototype)?;
    }
    for (name, id) in [
        (&b"trunc"[..], native::MATH_TRUNC),
        (&b"sqrt"[..], native::MATH_SQRT),
        (&b"pow"[..], native::MATH_POW),
        (&b"sign"[..], native::MATH_SIGN),
    ] {
        method(heap, atoms, math, name, id, function_prototype)?;
    }
    for (name, id) in [
        (&b"min"[..], native::MATH_MIN),
        (&b"max"[..], native::MATH_MAX),
        (&b"hypot"[..], native::MATH_HYPOT),
    ] {
        method(heap, atoms, math, name, id, function_prototype)?;
    }
    for (name, value) in [
        (&b"PI"[..], core::f64::consts::PI),
        (&b"E"[..], core::f64::consts::E),
        (&b"LN2"[..], core::f64::consts::LN_2),
        (&b"SQRT2"[..], core::f64::consts::SQRT_2),
    ] {
        define(heap, atoms, math, name, Value::number(value), 0)?;
    }
    define(
        heap,
        atoms,
        global,
        b"Math",
        Value::object(math),
        attribute::WRITABLE | attribute::CONFIGURABLE,
    )?;
    Ok(())
}

/// What every function carries: the ways of calling it with a receiver of the
/// caller's choosing.
fn build_function_intrinsics(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    function_prototype: Handle,
) -> Result<(), ObjectError> {
    object::reserve(heap, function_prototype, 4)?;
    // `caller` and `arguments` on the function prototype are poisoned: every
    // function inherits accessors that refuse, which is what keeps a call's
    // caller from being observable.
    {
        let thrower = object::create_native(
            heap,
            Value::object(function_prototype),
            native::THROW_TYPE_ERROR,
            0,
        )?;
        for name in [&b"caller"[..], &b"arguments"[..]] {
            let key = key_of(heap, atoms, name)?;
            object::define_own_property(
                heap,
                function_prototype,
                key,
                Descriptor::accessor(
                    Value::object(thrower),
                    Value::object(thrower),
                    attribute::CONFIGURABLE,
                ),
            )?;
        }
    }
    for (name, id) in [
        (&b"call"[..], native::FUNCTION_PROTOTYPE_CALL),
        (&b"apply"[..], native::FUNCTION_PROTOTYPE_APPLY),
        (&b"bind"[..], native::FUNCTION_PROTOTYPE_BIND),
    ] {
        method(
            heap,
            atoms,
            function_prototype,
            name,
            id,
            function_prototype,
        )?;
    }
    Ok(())
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

/// Build a constructor: a callable that a program reaches by name, carrying its
/// prototype, with the prototype carrying it back.
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
