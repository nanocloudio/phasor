//! The error prototypes and constructors, the global object, and its value properties.

use super::*;

/// The error prototypes and constructors, the global object, and its value properties.
pub(super) fn build_errors(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    function_prototype: Handle,
    method_attributes: u8,
    object_prototype: Handle,
) -> Result<(Handle, [Handle; 9], Handle), ObjectError> {
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
        ErrorKind::Suppressed,
        ErrorKind::Aggregate,
    ];
    let mut error_prototypes = [error_prototype; 9];
    let global = object::create(heap, Value::object(object_prototype))?;
    // The global object's shape is known here: the intrinsics, the value
    // properties, and the error constructors.
    object::reserve(heap, global, 25)?;
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
    Ok((error_prototype, error_prototypes, global))
}
