//! `JSON`.

use super::*;

/// `JSON`.
pub(super) fn build_json(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    function_prototype: Handle,
    global: Handle,
    object_prototype: Handle,
    to_string_tag_symbol: Handle,
) -> Result<(), ObjectError> {
    // `JSON`: a parser and a serialiser, pure functions of their arguments.
    let json = object::create(heap, Value::object(object_prototype))?;
    object::reserve(heap, json, 4)?;
    method(
        heap,
        atoms,
        json,
        b"parse",
        native::JSON_PARSE,
        function_prototype,
    )?;
    method(
        heap,
        atoms,
        json,
        b"stringify",
        native::JSON_STRINGIFY,
        function_prototype,
    )?;
    let json_tag = crate::string::create_ascii(heap, b"JSON")?;
    object::define_own_property(
        heap,
        json,
        Key::Symbol(to_string_tag_symbol),
        Descriptor::data(Value::string(json_tag), attribute::CONFIGURABLE),
    )?;
    define(
        heap,
        atoms,
        global,
        b"JSON",
        Value::object(json),
        attribute::WRITABLE | attribute::CONFIGURABLE,
    )?;
    Ok(())
}
