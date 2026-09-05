//! `Map` and `Set`.

use super::*;

/// `Map` and `Set`.
pub(super) fn build_collections(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    function_prototype: Handle,
    global: Handle,
    iterator_symbol: Handle,
    method_attributes: u8,
    object_prototype: Handle,
) -> Result<(Handle, Handle), ObjectError> {
    // `Map` and `Set`: the keyed collections, deterministic in insertion
    // order and free of any ambient authority.
    let map_prototype = object::create(heap, Value::object(object_prototype))?;
    let set_prototype = object::create(heap, Value::object(object_prototype))?;
    constructor(
        heap,
        atoms,
        global,
        b"Map",
        native::MAP,
        map_prototype,
        function_prototype,
    )?;
    constructor(
        heap,
        atoms,
        global,
        b"Set",
        native::SET,
        set_prototype,
        function_prototype,
    )?;
    let entries = [
        Entry::Method(b"get", native::MAP_GET),
        Entry::Method(b"set", native::MAP_SET),
        Entry::Method(b"has", native::MAP_HAS),
        Entry::Method(b"delete", native::MAP_DELETE),
        Entry::Method(b"clear", native::MAP_CLEAR),
        Entry::Method(b"forEach", native::MAP_FOR_EACH),
        Entry::Method(b"entries", native::MAP_ENTRIES),
        Entry::Method(b"keys", native::MAP_KEYS),
        Entry::Method(b"values", native::MAP_VALUES),
    ];
    install(heap, atoms, map_prototype, function_prototype, &entries)?;
    let entries = [
        Entry::Method(b"add", native::SET_ADD),
        Entry::Method(b"has", native::SET_HAS),
        Entry::Method(b"delete", native::SET_DELETE),
        Entry::Method(b"clear", native::SET_CLEAR),
        Entry::Method(b"forEach", native::SET_FOR_EACH),
        Entry::Method(b"entries", native::SET_ENTRIES),
        Entry::Method(b"values", native::SET_VALUES),
        Entry::Method(b"keys", native::SET_VALUES),
    ];
    install(heap, atoms, set_prototype, function_prototype, &entries)?;
    for (prototype, getter_id) in [
        (map_prototype, native::MAP_SIZE),
        (set_prototype, native::SET_SIZE),
    ] {
        let getter = object::create_native(heap, Value::object(function_prototype), getter_id, 0)?;
        let key = key_of(heap, atoms, b"size")?;
        object::define_own_property(
            heap,
            prototype,
            key,
            Descriptor::accessor(
                Value::object(getter),
                Value::UNDEFINED,
                attribute::CONFIGURABLE,
            ),
        )?;
    }
    for (prototype, iter_id) in [
        (map_prototype, native::MAP_ENTRIES),
        (set_prototype, native::SET_VALUES),
    ] {
        let function = object::create_native(heap, Value::object(function_prototype), iter_id, 0)?;
        object::define_own_property(
            heap,
            prototype,
            Key::Symbol(iterator_symbol),
            Descriptor::data(Value::object(function), method_attributes),
        )?;
    }
    Ok((map_prototype, set_prototype))
}
