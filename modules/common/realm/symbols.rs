//! `Symbol`: the well-known symbols, the hint strings, and the constructor.

use super::*;

/// The well-known symbols and the hint strings a realm keeps, which travel
/// together everywhere a builder needs one of them.
#[derive(Clone, Copy)]
pub(super) struct Symbols {
    pub symbol_prototype: Handle,
    pub iterator: Handle,
    pub async_iterator: Handle,
    pub dispose: Handle,
    pub async_dispose: Handle,
    pub to_primitive: Handle,
    pub to_string_tag: Handle,
    pub species: Handle,
    pub unscopables: Handle,
    pub hint_default: Handle,
    pub hint_number: Handle,
    pub hint_string: Handle,
    pub has_instance: Handle,
}

/// `Symbol`: the well-known symbols, the hint strings, and the constructor.
pub(super) fn build_symbols(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    function_prototype: Handle,
    global: Handle,
    method_attributes: u8,
    object_prototype: Handle,
) -> Result<Symbols, ObjectError> {
    // Symbols. A well-known symbol is an ordinary symbol the realm keeps a
    // handle to, so a program can only reach it through the name it is given.
    let symbol_prototype = object::create(heap, Value::object(object_prototype))?;
    let iterator_symbol = crate::string::create_symbol_ascii(heap, b"Symbol.iterator")?;
    let async_iterator_symbol = crate::string::create_symbol_ascii(heap, b"Symbol.asyncIterator")?;
    let dispose_symbol = crate::string::create_symbol_ascii(heap, b"Symbol.dispose")?;
    let async_dispose_symbol = crate::string::create_symbol_ascii(heap, b"Symbol.asyncDispose")?;
    let to_primitive_symbol = crate::string::create_symbol_ascii(heap, b"Symbol.toPrimitive")?;
    let to_string_tag_symbol = crate::string::create_symbol_ascii(heap, b"Symbol.toStringTag")?;
    let species_symbol = crate::string::create_symbol_ascii(heap, b"Symbol.species")?;
    let unscopables_symbol = crate::string::create_symbol_ascii(heap, b"Symbol.unscopables")?;
    let hint_default = crate::string::create_ascii(heap, b"default")?;
    let hint_number = crate::string::create_ascii(heap, b"number")?;
    let hint_string = crate::string::create_ascii(heap, b"string")?;
    let has_instance_symbol = crate::string::create_symbol_ascii(heap, b"Symbol.hasInstance")?;
    let symbol_constructor = object::create_native(
        heap,
        Value::object(function_prototype),
        native::SYMBOL,
        object::function_flag::CONSTRUCTOR,
    )?;
    method(
        heap,
        atoms,
        symbol_prototype,
        b"toString",
        native::SYMBOL_TO_STRING,
        function_prototype,
    )?;
    method(
        heap,
        atoms,
        symbol_prototype,
        b"valueOf",
        native::SYMBOL_VALUE_OF,
        function_prototype,
    )?;
    accessor(
        heap,
        atoms,
        symbol_prototype,
        b"description",
        native::SYMBOL_DESCRIPTION,
        function_prototype,
    )?;
    define(
        heap,
        atoms,
        symbol_constructor,
        b"prototype",
        Value::object(symbol_prototype),
        0,
    )?;
    define(
        heap,
        atoms,
        symbol_constructor,
        b"iterator",
        Value::symbol(iterator_symbol),
        0,
    )?;
    define(
        heap,
        atoms,
        symbol_constructor,
        b"asyncIterator",
        Value::symbol(async_iterator_symbol),
        0,
    )?;
    define(
        heap,
        atoms,
        symbol_constructor,
        b"dispose",
        Value::symbol(dispose_symbol),
        0,
    )?;
    define(
        heap,
        atoms,
        symbol_constructor,
        b"asyncDispose",
        Value::symbol(async_dispose_symbol),
        0,
    )?;
    define(
        heap,
        atoms,
        symbol_constructor,
        b"toPrimitive",
        Value::symbol(to_primitive_symbol),
        0,
    )?;
    define(
        heap,
        atoms,
        symbol_constructor,
        b"toStringTag",
        Value::symbol(to_string_tag_symbol),
        0,
    )?;
    define(
        heap,
        atoms,
        symbol_constructor,
        b"species",
        Value::symbol(species_symbol),
        0,
    )?;
    define(
        heap,
        atoms,
        symbol_constructor,
        b"unscopables",
        Value::symbol(unscopables_symbol),
        0,
    )?;
    define(
        heap,
        atoms,
        symbol_constructor,
        b"hasInstance",
        Value::symbol(has_instance_symbol),
        0,
    )?;
    define(
        heap,
        atoms,
        symbol_prototype,
        b"constructor",
        Value::object(symbol_constructor),
        method_attributes,
    )?;
    define(
        heap,
        atoms,
        global,
        b"Symbol",
        Value::object(symbol_constructor),
        method_attributes,
    )?;
    Ok(Symbols {
        symbol_prototype,
        iterator: iterator_symbol,
        async_iterator: async_iterator_symbol,
        dispose: dispose_symbol,
        async_dispose: async_dispose_symbol,
        to_primitive: to_primitive_symbol,
        to_string_tag: to_string_tag_symbol,
        species: species_symbol,
        unscopables: unscopables_symbol,
        hint_default,
        hint_number,
        hint_string,
        has_instance: has_instance_symbol,
    })
}
