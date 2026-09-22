//! The primitive prototypes, the generator and async function prototypes, RegExp, and BigInt.

#![allow(
    unexpected_cfgs,
    reason = "the omit flags belong to the variants of the modules that can leave a library area out; a module that declares no variant receives no matching --check-cfg, and for it every flag is absent, which is the whole language"
)]

use super::*;

/// The prototypes `build_functions` creates, which the realm keeps and the
/// later builders hang their own objects from.
#[derive(Clone, Copy)]
pub(super) struct Prototypes {
    pub string: Handle,
    pub number: Handle,
    pub boolean: Handle,
    pub iterator: Handle,
    pub generator_function: Handle,
    pub generator_object: Handle,
    pub async_generator_function: Handle,
    pub async_generator_object: Handle,
    pub async_function: Handle,
    pub big_int: Handle,
    #[cfg(not(feature = "omit_regexp"))]
    pub regexp: Handle,
}

/// The primitive prototypes, the generator and async function prototypes, RegExp, and BigInt.
pub(super) fn build_functions(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    function_prototype: Handle,
    global: Handle,
    method_attributes: u8,
    object_prototype: Handle,
    symbols: Symbols,
) -> Result<Prototypes, ObjectError> {
    // The prototypes a method called on a primitive reaches.
    let string_prototype = object::create(heap, Value::object(object_prototype))?;
    let number_prototype = object::create(heap, Value::object(object_prototype))?;
    let boolean_prototype = object::create(heap, Value::object(object_prototype))?;
    let iterator_prototype = object::create(heap, Value::object(object_prototype))?;
    // Generator functions answer to a prototype of their own, whose
    // `prototype` property names what generator instances default to.
    let generator_function_prototype = object::create(heap, Value::object(function_prototype))?;
    let generator_object_prototype = object::create(heap, Value::object(iterator_prototype))?;
    define(
        heap,
        atoms,
        generator_function_prototype,
        b"prototype",
        Value::object(generator_object_prototype),
        attribute::CONFIGURABLE,
    )?;
    define(
        heap,
        atoms,
        generator_object_prototype,
        b"constructor",
        Value::object(generator_function_prototype),
        attribute::CONFIGURABLE,
    )?;
    let async_generator_function_prototype =
        object::create(heap, Value::object(function_prototype))?;
    // Async iterators share a prototype of their own beneath the async
    // generator prototype, iterable by itself as the sync one is.
    let async_iterator_prototype = object::create(heap, Value::object(object_prototype))?;
    let async_self_iterator = object::create_native(
        heap,
        Value::object(function_prototype),
        native::ITERATOR_SELF,
        0,
    )?;
    object::define_own_property(
        heap,
        async_iterator_prototype,
        Key::Symbol(symbols.async_iterator),
        Descriptor::data(
            Value::object(async_self_iterator),
            attribute::WRITABLE | attribute::CONFIGURABLE,
        ),
    )?;
    let async_generator_object_prototype =
        object::create(heap, Value::object(async_iterator_prototype))?;
    define(
        heap,
        atoms,
        async_generator_function_prototype,
        b"prototype",
        Value::object(async_generator_object_prototype),
        attribute::CONFIGURABLE,
    )?;
    define(
        heap,
        atoms,
        async_generator_object_prototype,
        b"constructor",
        Value::object(async_generator_function_prototype),
        attribute::CONFIGURABLE,
    )?;
    intrinsic_constructor(
        heap,
        atoms,
        b"GeneratorFunction",
        native::GENERATOR_FUNCTION,
        generator_function_prototype,
        function_prototype,
    )?;
    intrinsic_constructor(
        heap,
        atoms,
        b"AsyncGeneratorFunction",
        native::ASYNC_GENERATOR_FUNCTION,
        async_generator_function_prototype,
        function_prototype,
    )?;
    // Async functions are instances of a prototype of their own beneath
    // `Function.prototype`, reached only through one of them.
    let async_function_prototype = object::create(heap, Value::object(function_prototype))?;
    let async_function_tag = crate::string::create_ascii(heap, b"AsyncFunction")?;
    object::define_own_property(
        heap,
        async_function_prototype,
        Key::Symbol(symbols.to_string_tag),
        Descriptor::data(Value::string(async_function_tag), attribute::CONFIGURABLE),
    )?;
    intrinsic_constructor(
        heap,
        atoms,
        b"AsyncFunction",
        native::ASYNC_FUNCTION,
        async_function_prototype,
        function_prototype,
    )?;
    let big_int_prototype = object::create(heap, Value::object(object_prototype))?;
    // A build without the regular expression engine defines no `RegExp`: the
    // global is absent, as `JSON` is absent from a build without it, and a
    // program finds out the way it finds out about any host object.
    #[cfg(not(feature = "omit_regexp"))]
    let regexp_prototype = {
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
        let entries = [
            Entry::Method(b"exec", native::REG_EXP_EXEC),
            Entry::Method(b"test", native::REG_EXP_TEST),
            Entry::Method(b"toString", native::REG_EXP_TO_STRING),
        ];
        install(heap, atoms, regexp_prototype, function_prototype, &entries)?;
        regexp_prototype
    };

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
    let entries = [
        Entry::Method(b"toString", native::BIG_INT_TO_STRING),
        Entry::Method(b"valueOf", native::BIG_INT_VALUE_OF),
    ];
    install(heap, atoms, big_int_prototype, function_prototype, &entries)?;
    define(
        heap,
        atoms,
        global,
        b"BigInt",
        Value::object(big_int_constructor),
        method_attributes,
    )?;
    Ok(Prototypes {
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
        #[cfg(not(feature = "omit_regexp"))]
        regexp: regexp_prototype,
    })
}

/// What every function carries: the ways of calling it with a receiver of the
/// caller's choosing.
pub(super) fn build_function_intrinsics(
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
    let entries = [
        Entry::Method(b"call", native::FUNCTION_PROTOTYPE_CALL),
        Entry::Method(b"apply", native::FUNCTION_PROTOTYPE_APPLY),
        Entry::Method(b"bind", native::FUNCTION_PROTOTYPE_BIND),
    ];
    install(
        heap,
        atoms,
        function_prototype,
        function_prototype,
        &entries,
    )?;
    Ok(())
}
