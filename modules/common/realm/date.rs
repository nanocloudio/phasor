//! `Date`.

use super::*;

/// `Date`.
pub(super) fn build_date(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    function_prototype: Handle,
    global: Handle,
    object_prototype: Handle,
    to_primitive_symbol: Handle,
    to_string_tag_symbol: Handle,
) -> Result<Handle, ObjectError> {
    // `Date`: time values in UTC. Without a clock capability "now" is the
    // epoch, so a program that asks for the time gets the same answer every
    // run rather than an authority it was not given.
    let date_prototype = object::create(heap, Value::object(object_prototype))?;
    object::reserve(heap, date_prototype, 52)?;
    let date_constructor = constructor(
        heap,
        atoms,
        global,
        b"Date",
        native::DATE,
        date_prototype,
        function_prototype,
    )?;
    let entries = [
        Entry::Method(b"now", native::DATE_NOW),
        Entry::Method(b"UTC", native::DATE_UTC),
        Entry::Method(b"parse", native::DATE_PARSE),
    ];
    install(heap, atoms, date_constructor, function_prototype, &entries)?;
    let entries = [
        Entry::Method(b"getTime", native::DATE_GET_TIME),
        Entry::Method(b"valueOf", native::DATE_GET_TIME),
        Entry::Method(b"getFullYear", native::DATE_GET_FULL_YEAR),
        Entry::Method(b"getUTCFullYear", native::DATE_GET_FULL_YEAR),
        Entry::Method(b"getMonth", native::DATE_GET_MONTH),
        Entry::Method(b"getUTCMonth", native::DATE_GET_MONTH),
        Entry::Method(b"getDate", native::DATE_GET_DATE),
        Entry::Method(b"getUTCDate", native::DATE_GET_DATE),
        Entry::Method(b"getDay", native::DATE_GET_DAY),
        Entry::Method(b"getUTCDay", native::DATE_GET_DAY),
        Entry::Method(b"getHours", native::DATE_GET_HOURS),
        Entry::Method(b"getUTCHours", native::DATE_GET_HOURS),
        Entry::Method(b"getMinutes", native::DATE_GET_MINUTES),
        Entry::Method(b"getUTCMinutes", native::DATE_GET_MINUTES),
        Entry::Method(b"getSeconds", native::DATE_GET_SECONDS),
        Entry::Method(b"getUTCSeconds", native::DATE_GET_SECONDS),
        Entry::Method(b"getMilliseconds", native::DATE_GET_MILLISECONDS),
        Entry::Method(b"getUTCMilliseconds", native::DATE_GET_MILLISECONDS),
        Entry::Method(b"getTimezoneOffset", native::DATE_GET_TIMEZONE_OFFSET),
        Entry::Method(b"toString", native::DATE_TO_STRING),
        Entry::Method(b"toISOString", native::DATE_TO_ISO_STRING),
        Entry::Method(b"toUTCString", native::DATE_TO_UTC_STRING),
        Entry::Method(b"toDateString", native::DATE_TO_DATE_STRING),
        Entry::Method(b"toTimeString", native::DATE_TO_TIME_STRING),
        Entry::Method(b"toJSON", native::DATE_TO_JSON),
        Entry::Method(b"setTime", native::DATE_SET_TIME),
        Entry::Method(b"toLocaleString", native::DATE_TO_STRING),
        Entry::Method(b"toLocaleDateString", native::DATE_TO_DATE_STRING),
        Entry::Method(b"toLocaleTimeString", native::DATE_TO_TIME_STRING),
        Entry::Method(b"setFullYear", native::DATE_SET_FULL_YEAR),
        Entry::Method(b"setUTCFullYear", native::DATE_SET_FULL_YEAR),
        Entry::Method(b"setMonth", native::DATE_SET_MONTH),
        Entry::Method(b"setUTCMonth", native::DATE_SET_MONTH),
        Entry::Method(b"setDate", native::DATE_SET_DATE),
        Entry::Method(b"setUTCDate", native::DATE_SET_DATE),
        Entry::Method(b"setHours", native::DATE_SET_HOURS),
        Entry::Method(b"setUTCHours", native::DATE_SET_HOURS),
        Entry::Method(b"setMinutes", native::DATE_SET_MINUTES),
        Entry::Method(b"setUTCMinutes", native::DATE_SET_MINUTES),
        Entry::Method(b"setSeconds", native::DATE_SET_SECONDS),
        Entry::Method(b"setUTCSeconds", native::DATE_SET_SECONDS),
        Entry::Method(b"setMilliseconds", native::DATE_SET_MILLISECONDS),
        Entry::Method(b"setUTCMilliseconds", native::DATE_SET_MILLISECONDS),
    ];
    install(heap, atoms, date_prototype, function_prototype, &entries)?;
    let to_primitive = object::create_native(
        heap,
        Value::object(function_prototype),
        native::DATE_TO_PRIMITIVE,
        0,
    )?;
    object::define_own_property(
        heap,
        date_prototype,
        Key::Symbol(to_primitive_symbol),
        Descriptor::data(Value::object(to_primitive), attribute::CONFIGURABLE),
    )?;
    let date_tag = crate::string::create_ascii(heap, b"Date")?;
    object::define_own_property(
        heap,
        date_prototype,
        Key::Symbol(to_string_tag_symbol),
        Descriptor::data(Value::string(date_tag), attribute::CONFIGURABLE),
    )?;
    Ok(date_prototype)
}
