//! `Object` and `Object.prototype`.

use super::*;

/// `Object`, its statics, and the methods every object inherits.
pub(super) fn build_object_intrinsics(
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
    let entries = [
        Entry::Method(b"keys", native::OBJECT_KEYS),
        Entry::Method(b"values", native::OBJECT_VALUES),
        Entry::Method(b"entries", native::OBJECT_ENTRIES),
        Entry::Method(b"assign", native::OBJECT_ASSIGN),
    ];
    install(heap, atoms, handle, function_prototype, &entries)?;
    let entries = [
        Entry::Method(b"freeze", native::OBJECT_FREEZE),
        Entry::Method(b"isFrozen", native::OBJECT_IS_FROZEN),
        Entry::Method(b"preventExtensions", native::OBJECT_PREVENT_EXTENSIONS),
        Entry::Method(b"isExtensible", native::OBJECT_IS_EXTENSIBLE),
        Entry::Method(b"seal", native::OBJECT_SEAL),
        Entry::Method(b"isSealed", native::OBJECT_IS_SEALED),
        Entry::Method(b"getPrototypeOf", native::OBJECT_GET_PROTOTYPE_OF),
        Entry::Method(b"setPrototypeOf", native::OBJECT_SET_PROTOTYPE_OF),
    ];
    install(heap, atoms, handle, function_prototype, &entries)?;
    let entries = [
        Entry::Method(b"defineProperty", native::OBJECT_DEFINE_PROPERTY),
        Entry::Method(b"defineProperties", native::OBJECT_DEFINE_PROPERTIES),
        Entry::Method(
            b"getOwnPropertyNames",
            native::OBJECT_GET_OWN_PROPERTY_NAMES,
        ),
        Entry::Method(b"create", native::OBJECT_CREATE),
        Entry::Method(b"is", native::OBJECT_IS),
        Entry::Method(
            b"getOwnPropertyDescriptor",
            native::OBJECT_GET_OWN_PROPERTY_DESCRIPTOR,
        ),
        Entry::Method(
            b"getOwnPropertySymbols",
            native::OBJECT_GET_OWN_PROPERTY_SYMBOLS,
        ),
    ];
    install(heap, atoms, handle, function_prototype, &entries)?;
    let entries = [
        Entry::Method(b"hasOwnProperty", native::OBJECT_HAS_OWN_PROPERTY),
        Entry::Method(b"isPrototypeOf", native::OBJECT_IS_PROTOTYPE_OF),
        Entry::Method(b"__defineGetter__", native::OBJECT_DEFINE_GETTER),
        Entry::Method(b"__defineSetter__", native::OBJECT_DEFINE_SETTER),
        Entry::Method(b"__lookupGetter__", native::OBJECT_LOOKUP_GETTER),
        Entry::Method(b"__lookupSetter__", native::OBJECT_LOOKUP_SETTER),
        Entry::Method(
            b"propertyIsEnumerable",
            native::OBJECT_PROPERTY_IS_ENUMERABLE,
        ),
    ];
    install(heap, atoms, object_prototype, function_prototype, &entries)?;
    Ok(())
}
