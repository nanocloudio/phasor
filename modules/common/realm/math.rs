//! `Math`.

use super::*;

/// `Math`, which holds no state and no authority: every function of it is a
/// pure function of its arguments. There is no `random`, because randomness is
/// a capability rather than something an engine may help itself to.
pub(super) fn build_math(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    global: Handle,
    object_prototype: Handle,
    function_prototype: Handle,
) -> Result<(), ObjectError> {
    let math = object::create(heap, Value::object(object_prototype))?;
    object::reserve(heap, math, 24)?;
    let entries = [
        Entry::Method(b"abs", native::MATH_ABS),
        Entry::Method(b"floor", native::MATH_FLOOR),
        Entry::Method(b"ceil", native::MATH_CEIL),
        Entry::Method(b"round", native::MATH_ROUND),
    ];
    install(heap, atoms, math, function_prototype, &entries)?;
    let entries = [
        Entry::Method(b"trunc", native::MATH_TRUNC),
        Entry::Method(b"sqrt", native::MATH_SQRT),
        Entry::Method(b"pow", native::MATH_POW),
        Entry::Method(b"sign", native::MATH_SIGN),
    ];
    install(heap, atoms, math, function_prototype, &entries)?;
    let entries = [
        Entry::Method(b"min", native::MATH_MIN),
        Entry::Method(b"max", native::MATH_MAX),
        Entry::Method(b"hypot", native::MATH_HYPOT),
        Entry::Method(b"sin", native::MATH_SIN),
        Entry::Method(b"cos", native::MATH_COS),
        Entry::Method(b"tan", native::MATH_TAN),
        Entry::Method(b"asin", native::MATH_ASIN),
        Entry::Method(b"acos", native::MATH_ACOS),
        Entry::Method(b"atan", native::MATH_ATAN),
        Entry::Method(b"atan2", native::MATH_ATAN2),
        Entry::Method(b"exp", native::MATH_EXP),
        Entry::Method(b"log", native::MATH_LOG),
        Entry::Method(b"log2", native::MATH_LOG2),
        Entry::Method(b"log10", native::MATH_LOG10),
        Entry::Method(b"cbrt", native::MATH_CBRT),
    ];
    install(heap, atoms, math, function_prototype, &entries)?;
    for (name, value) in [
        (&b"PI"[..], core::f64::consts::PI),
        (&b"E"[..], core::f64::consts::E),
        (&b"LN2"[..], core::f64::consts::LN_2),
        (&b"LN10"[..], core::f64::consts::LN_10),
        (&b"LOG2E"[..], core::f64::consts::LOG2_E),
        (&b"LOG10E"[..], core::f64::consts::LOG10_E),
        (&b"SQRT2"[..], core::f64::consts::SQRT_2),
        (&b"SQRT1_2"[..], core::f64::consts::FRAC_1_SQRT_2),
    ] {
        define(heap, atoms, math, name, Value::number(value), 0)?;
    }
    define(
        heap,
        atoms,
        global,
        b"Math",
        Value::object(math),
        attribute::WRITABLE | attribute::CONFIGURABLE,
    )?;
    Ok(())
}
