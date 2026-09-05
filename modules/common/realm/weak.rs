//! `WeakRef`, `WeakMap`, and `WeakSet`.

use super::*;

/// `WeakRef`, `WeakMap`, and `WeakSet`.
pub(super) fn build_weak(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    function_prototype: Handle,
    global: Handle,
    object_prototype: Handle,
    to_string_tag_symbol: Handle,
) -> Result<(Handle, Handle, Handle), ObjectError> {
    // `WeakRef`: a reference the collector could clear, except that this
    // engine never observes a collection, so `deref` always answers the
    // target — which the specification allows.
    let weak_ref_prototype = object::create(heap, Value::object(object_prototype))?;
    constructor(
        heap,
        atoms,
        global,
        b"WeakRef",
        native::WEAK_REF,
        weak_ref_prototype,
        function_prototype,
    )?;
    method(
        heap,
        atoms,
        weak_ref_prototype,
        b"deref",
        native::WEAK_REF_DEREF,
        function_prototype,
    )?;
    let weak_tag = crate::string::create_ascii(heap, b"WeakRef")?;
    object::define_own_property(
        heap,
        weak_ref_prototype,
        Key::Symbol(to_string_tag_symbol),
        Descriptor::data(Value::string(weak_tag), attribute::CONFIGURABLE),
    )?;

    // `WeakMap` and `WeakSet`: the same storage as `Map` and `Set` under a
    // brand of their own, keyed by objects and symbols only, with nothing
    // that walks or counts the members.
    let weak_map_prototype = object::create(heap, Value::object(object_prototype))?;
    let weak_set_prototype = object::create(heap, Value::object(object_prototype))?;
    constructor(
        heap,
        atoms,
        global,
        b"WeakMap",
        native::WEAK_MAP,
        weak_map_prototype,
        function_prototype,
    )?;
    constructor(
        heap,
        atoms,
        global,
        b"WeakSet",
        native::WEAK_SET,
        weak_set_prototype,
        function_prototype,
    )?;
    let entries = [
        Entry::Method(b"get", native::WEAK_MAP_GET),
        Entry::Method(b"set", native::WEAK_MAP_SET),
        Entry::Method(b"has", native::WEAK_MAP_HAS),
        Entry::Method(b"delete", native::WEAK_MAP_DELETE),
    ];
    install(
        heap,
        atoms,
        weak_map_prototype,
        function_prototype,
        &entries,
    )?;
    let entries = [
        Entry::Method(b"add", native::WEAK_SET_ADD),
        Entry::Method(b"has", native::WEAK_SET_HAS),
        Entry::Method(b"delete", native::WEAK_SET_DELETE),
    ];
    install(
        heap,
        atoms,
        weak_set_prototype,
        function_prototype,
        &entries,
    )?;
    for (prototype, name) in [
        (weak_map_prototype, &b"WeakMap"[..]),
        (weak_set_prototype, &b"WeakSet"[..]),
    ] {
        let tag = crate::string::create_ascii(heap, name)?;
        object::define_own_property(
            heap,
            prototype,
            Key::Symbol(to_string_tag_symbol),
            Descriptor::data(Value::string(tag), attribute::CONFIGURABLE),
        )?;
    }
    Ok((weak_ref_prototype, weak_map_prototype, weak_set_prototype))
}
