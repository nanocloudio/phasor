//! `Number`, `Boolean`, and the global conversions.

use super::*;

/// `Number`, `Boolean`, and the conversions that live on the global object.
pub(super) fn build_number_intrinsics(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    global: Handle,
    number_prototype: Handle,
    boolean_prototype: Handle,
    function_prototype: Handle,
) -> Result<(), ObjectError> {
    object::reserve(heap, number_prototype, 6)?;
    let handle = constructor(
        heap,
        atoms,
        global,
        b"Number",
        native::NUMBER,
        number_prototype,
        function_prototype,
    )?;
    let entries = [
        Entry::Method(b"isInteger", native::NUMBER_IS_INTEGER),
        Entry::Method(b"isFinite", native::NUMBER_IS_FINITE),
        Entry::Method(b"isNaN", native::NUMBER_IS_NAN),
        Entry::Method(b"isSafeInteger", native::NUMBER_IS_SAFE_INTEGER),
    ];
    install(heap, atoms, handle, function_prototype, &entries)?;
    let entries = [
        Entry::Method(b"parseInt", native::PARSE_INT),
        Entry::Method(b"parseFloat", native::PARSE_FLOAT),
    ];
    install(heap, atoms, handle, function_prototype, &entries)?;
    for (name, value) in [
        (&b"MAX_SAFE_INTEGER"[..], 9_007_199_254_740_991.0f64),
        (&b"MIN_SAFE_INTEGER"[..], -9_007_199_254_740_991.0f64),
        (&b"EPSILON"[..], f64::EPSILON),
        (&b"POSITIVE_INFINITY"[..], f64::INFINITY),
    ] {
        define(heap, atoms, handle, name, Value::number(value), 0)?;
    }
    for (name, value) in [
        (&b"NEGATIVE_INFINITY"[..], f64::NEG_INFINITY),
        (&b"NaN"[..], f64::NAN),
        (&b"MAX_VALUE"[..], f64::MAX),
        (&b"MIN_VALUE"[..], 5e-324),
    ] {
        define(heap, atoms, handle, name, Value::number(value), 0)?;
    }
    let entries = [
        Entry::Method(b"toString", native::NUMBER_TO_STRING),
        Entry::Method(b"toFixed", native::NUMBER_TO_FIXED),
        Entry::Method(b"toExponential", native::NUMBER_TO_EXPONENTIAL),
        Entry::Method(b"toPrecision", native::NUMBER_TO_PRECISION),
        Entry::Method(b"valueOf", native::NUMBER_VALUE_OF),
    ];
    install(heap, atoms, number_prototype, function_prototype, &entries)?;

    constructor(
        heap,
        atoms,
        global,
        b"Boolean",
        native::BOOLEAN,
        boolean_prototype,
        function_prototype,
    )?;
    // `Function` exists so a function's constructor, `Function.prototype`,
    // and `f instanceof Function` all answer; only building one from source
    // is refused, because the compiler lives outside the machine.
    constructor(
        heap,
        atoms,
        global,
        b"Function",
        native::FUNCTION,
        function_prototype,
        function_prototype,
    )?;
    let entries = [
        Entry::Method(b"toString", native::BOOLEAN_TO_STRING),
        Entry::Method(b"valueOf", native::BOOLEAN_VALUE_OF),
    ];
    install(heap, atoms, boolean_prototype, function_prototype, &entries)?;

    // `eval` is a function like any other; only the machine knows that a call
    // to it pauses for the compiler.
    {
        let function =
            object::create_native(heap, Value::object(function_prototype), native::EVAL, 0)?;
        define(
            heap,
            atoms,
            global,
            b"eval",
            Value::object(function),
            attribute::WRITABLE | attribute::CONFIGURABLE,
        )?;
    }
    let entries = [
        Entry::Method(b"parseInt", native::PARSE_INT),
        Entry::Method(b"parseFloat", native::PARSE_FLOAT),
        Entry::Method(b"isNaN", native::IS_NAN),
        Entry::Method(b"isFinite", native::IS_FINITE),
        Entry::Method(b"encodeURI", native::ENCODE_URI),
        Entry::Method(b"encodeURIComponent", native::ENCODE_URI_COMPONENT),
        Entry::Method(b"decodeURI", native::DECODE_URI),
        Entry::Method(b"decodeURIComponent", native::DECODE_URI_COMPONENT),
    ];
    install(heap, atoms, global, function_prototype, &entries)?;
    Ok(())
}
