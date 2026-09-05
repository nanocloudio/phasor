//! `Array` and `Array.prototype`.

use super::*;

/// `Array`, its statics, and the methods an array inherits.
pub(super) fn build_array_intrinsics(
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
    let entries = [
        Entry::Method(b"isArray", native::ARRAY_IS_ARRAY),
        Entry::Method(b"of", native::ARRAY_OF),
        Entry::Method(b"from", native::ARRAY_FROM),
    ];
    install(heap, atoms, handle, function_prototype, &entries)?;
    let entries = [
        Entry::Method(b"push", native::ARRAY_PUSH),
        Entry::Method(b"pop", native::ARRAY_POP),
        Entry::Method(b"shift", native::ARRAY_SHIFT),
        Entry::Method(b"unshift", native::ARRAY_UNSHIFT),
    ];
    install(heap, atoms, array_prototype, function_prototype, &entries)?;
    let entries = [
        Entry::Method(b"slice", native::ARRAY_SLICE),
        Entry::Method(b"indexOf", native::ARRAY_INDEX_OF),
        Entry::Method(b"includes", native::ARRAY_INCLUDES),
        Entry::Method(b"concat", native::ARRAY_CONCAT),
    ];
    install(heap, atoms, array_prototype, function_prototype, &entries)?;
    let entries = [
        Entry::Method(b"map", native::ARRAY_MAP),
        Entry::Method(b"filter", native::ARRAY_FILTER),
        Entry::Method(b"reduce", native::ARRAY_REDUCE),
        Entry::Method(b"forEach", native::ARRAY_FOR_EACH),
    ];
    install(heap, atoms, array_prototype, function_prototype, &entries)?;
    let entries = [
        Entry::Method(b"some", native::ARRAY_SOME),
        Entry::Method(b"every", native::ARRAY_EVERY),
        Entry::Method(b"find", native::ARRAY_FIND),
        Entry::Method(b"findIndex", native::ARRAY_FIND_INDEX),
    ];
    install(heap, atoms, array_prototype, function_prototype, &entries)?;
    let entries = [
        Entry::Method(b"reverse", native::ARRAY_REVERSE),
        Entry::Method(b"fill", native::ARRAY_FILL),
        Entry::Method(b"sort", native::ARRAY_SORT),
    ];
    install(heap, atoms, array_prototype, function_prototype, &entries)?;
    let entries = [
        Entry::Method(b"values", native::ARRAY_VALUES),
        Entry::Method(b"keys", native::ARRAY_KEYS),
        Entry::Method(b"entries", native::ARRAY_ENTRIES),
    ];
    install(heap, atoms, array_prototype, function_prototype, &entries)?;
    // An array is iterated by its values: `Symbol.iterator` names the very
    // function `values` does.
    let values_key = key_of(heap, atoms, b"values")?;
    let values = object::get_own_property(heap, array_prototype, values_key)?
        .map_or(Value::UNDEFINED, |found| found.value);
    object::define_own_property(
        heap,
        array_prototype,
        Key::Symbol(iterator_symbol),
        Descriptor::data(values, attribute::WRITABLE | attribute::CONFIGURABLE),
    )?;
    Ok(())
}
