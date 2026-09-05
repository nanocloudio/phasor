//! `Promise`: its prototype and its constructor.

use super::*;

/// `Promise`: its prototype and its constructor.
pub(super) fn build_promises(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    function_prototype: Handle,
    global: Handle,
    method_attributes: u8,
    object_prototype: Handle,
) -> Result<(Handle, Handle), ObjectError> {
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
    let entries = [
        Entry::Method(b"catch", native::PROMISE_CATCH),
        Entry::Method(b"finally", native::PROMISE_FINALLY),
    ];
    install(heap, atoms, promise_prototype, function_prototype, &entries)?;

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
        (&b"withResolvers"[..], native::PROMISE_WITH_RESOLVERS),
        (&b"all"[..], native::PROMISE_ALL),
        (&b"race"[..], native::PROMISE_RACE),
        (&b"allSettled"[..], native::PROMISE_ALL_SETTLED),
        (&b"any"[..], native::PROMISE_ANY),
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
    Ok((promise_prototype, promise_constructor))
}
