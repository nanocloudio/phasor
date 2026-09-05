//! `Reflect`.

use super::*;

/// `Reflect`.
pub(super) fn build_reflect(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    function_prototype: Handle,
    global: Handle,
    method_attributes: u8,
    object_prototype: Handle,
) -> Result<(), ObjectError> {
    // `Reflect`: the object operations as callables over ordinary objects.
    let reflect = object::create(heap, Value::object(object_prototype))?;
    let entries = [
        Entry::Method(b"get", native::REFLECT_GET),
        Entry::Method(b"set", native::REFLECT_SET),
        Entry::Method(b"has", native::REFLECT_HAS),
        Entry::Method(b"deleteProperty", native::REFLECT_DELETE),
        Entry::Method(b"ownKeys", native::REFLECT_OWN_KEYS),
        Entry::Method(b"getPrototypeOf", native::REFLECT_GET_PROTOTYPE),
        Entry::Method(b"setPrototypeOf", native::REFLECT_SET_PROTOTYPE),
        Entry::Method(b"isExtensible", native::REFLECT_IS_EXTENSIBLE),
        Entry::Method(b"preventExtensions", native::REFLECT_PREVENT_EXTENSIONS),
        Entry::Method(b"defineProperty", native::REFLECT_DEFINE_PROPERTY),
        Entry::Method(
            b"getOwnPropertyDescriptor",
            native::REFLECT_GET_OWN_DESCRIPTOR,
        ),
        Entry::Method(b"apply", native::REFLECT_APPLY),
        Entry::Method(b"construct", native::REFLECT_CONSTRUCT),
    ];
    install(heap, atoms, reflect, function_prototype, &entries)?;
    define(
        heap,
        atoms,
        global,
        b"Reflect",
        Value::object(reflect),
        method_attributes,
    )?;

    Ok(())
}
