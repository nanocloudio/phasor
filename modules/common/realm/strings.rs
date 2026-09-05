//! `String` and `String.prototype`.

use super::*;

/// `String` and the methods a string reaches.
pub(super) fn build_string_intrinsics(
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
    let entries = [
        Entry::Method(b"charAt", native::STRING_CHAR_AT),
        Entry::Method(b"charCodeAt", native::STRING_CHAR_CODE_AT),
        Entry::Method(b"codePointAt", native::STRING_CODE_POINT_AT),
        Entry::Method(b"at", native::STRING_AT),
    ];
    install(heap, atoms, string_prototype, function_prototype, &entries)?;
    let entries = [
        Entry::Method(b"indexOf", native::STRING_INDEX_OF),
        Entry::Method(b"lastIndexOf", native::STRING_LAST_INDEX_OF),
        Entry::Method(b"includes", native::STRING_INCLUDES),
        Entry::Method(b"startsWith", native::STRING_STARTS_WITH),
    ];
    install(heap, atoms, string_prototype, function_prototype, &entries)?;
    let entries = [
        Entry::Method(b"endsWith", native::STRING_ENDS_WITH),
        Entry::Method(b"slice", native::STRING_SLICE),
        Entry::Method(b"substring", native::STRING_SUBSTRING),
        Entry::Method(b"split", native::STRING_SPLIT),
    ];
    install(heap, atoms, string_prototype, function_prototype, &entries)?;
    let entries = [
        Entry::Method(b"toUpperCase", native::STRING_TO_UPPER_CASE),
        Entry::Method(b"toLowerCase", native::STRING_TO_LOWER_CASE),
        Entry::Method(b"trim", native::STRING_TRIM),
        Entry::Method(b"repeat", native::STRING_REPEAT),
    ];
    install(heap, atoms, string_prototype, function_prototype, &entries)?;
    let entries = [
        Entry::Method(b"padStart", native::STRING_PAD_START),
        Entry::Method(b"padEnd", native::STRING_PAD_END),
        Entry::Method(b"concat", native::STRING_CONCAT),
        Entry::Method(b"replace", native::STRING_REPLACE),
    ];
    install(heap, atoms, string_prototype, function_prototype, &entries)?;
    let entries = [
        Entry::Method(b"toString", native::STRING_TO_STRING),
        Entry::Method(b"valueOf", native::STRING_TO_STRING),
        Entry::Method(b"match", native::STRING_MATCH),
        Entry::Method(b"search", native::STRING_SEARCH),
    ];
    install(heap, atoms, string_prototype, function_prototype, &entries)?;
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
