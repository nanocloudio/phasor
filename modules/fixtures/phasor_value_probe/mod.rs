//! On-graph conformance probe for tagged values and numeric coercion.
//!
//! The cases here run binary64 arithmetic, which on a target without
//! double-precision instructions is served by the engine's own routines. That
//! makes this fixture the evidence that the same arithmetic is available, and
//! gives the same answers, on every declared target.

#![cfg_attr(not(feature = "host-test"), no_std)]
#![allow(
    dead_code,
    unused_imports,
    unreachable_patterns,
    reason = "the Fluxor ABI source is mounted as one surface and this fixture consumes a subset"
)]

#[path = "../../../target/fluxor/fluxor-abi/sdk/abi.rs"]
mod abi;
use abi::SyscallTable;

include!("../../../target/fluxor/fluxor-abi/sdk/runtime.rs");

#[macro_use]
#[path = "../../common/entry.rs"]
mod entry;

#[path = "../../common/numeric.rs"]
mod numeric;
#[path = "../../common/probe.rs"]
mod probe;
#[path = "../../common/softfloat.rs"]
mod softfloat;
#[path = "../../common/text.rs"]
mod text;
#[path = "../../common/value.rs"]
mod value;
#[path = "../../common/wire.rs"]
mod wire;

use value::{
    bitwise_and, bitwise_not, bitwise_or, bitwise_xor, compare_numbers, is_integral, number_equals,
    same_value_number, same_value_zero_number, shift_left, shift_right, strict_equals,
    string_to_number, to_boolean_primitive, to_int32, to_integer_or_infinity, to_length,
    to_number_primitive, to_uint32, truncate, type_of, unary_minus, unsigned_shift_right,
    Comparison, Handle, TypeOf, Value,
};

const CASE_COUNT: u16 = 32;

fn same(left: f64, right: f64) -> bool {
    if left.is_nan() && right.is_nan() {
        return true;
    }
    left.to_bits() == right.to_bits()
}

/// The Number a UTF-16 string denotes, written from an ASCII literal.
fn number_of(text: &[u8]) -> f64 {
    let mut units = [0u16; 32];
    let mut length = 0usize;
    for &byte in text {
        if length < units.len() {
            units[length] = u16::from(byte);
            length += 1;
        }
    }
    string_to_number(units.get(..length).unwrap_or(&[]))
}

#[allow(
    clippy::match_same_arms,
    reason = "each case is an independent assertion and merging arms would hide which one failed"
)]
fn run_case(case: u16) -> bool {
    match case {
        // Values.
        0 => Value::number(1.5).as_number() == 1.5 && Value::number(1.5).is_number(),
        1 => Value::UNDEFINED.is_nullish() && Value::NULL.is_nullish(),
        2 => {
            let handle = Handle::new(7, 3);
            Value::object(handle).as_handle() == handle && Value::object(handle).is_object()
        }
        3 => Value::number(f64::NAN).as_number().is_nan(),
        4 => matches!(type_of(&Value::NULL, false), TypeOf::Object),
        5 => matches!(
            type_of(&Value::object(Handle::new(0, 1)), true),
            TypeOf::Function
        ),

        // Coercion of primitives.
        6 => to_boolean_primitive(&Value::number(0.0)) == Some(false),
        7 => to_boolean_primitive(&Value::number(unary_minus(0.0))) == Some(false),
        8 => to_boolean_primitive(&Value::number(f64::NAN)) == Some(false),
        9 => to_number_primitive(&Value::NULL) == Some(0.0),
        10 => to_number_primitive(&Value::UNDEFINED).is_some_and(f64::is_nan),

        // Arithmetic, which exercises the engine's own binary64 routines on a
        // target without double-precision instructions.
        11 => same(softfloat::add(0.1, 0.2), 0.30000000000000004),
        12 => same(softfloat::mul(1.0e300, 1.0e300), f64::INFINITY),
        13 => same(softfloat::div(1.0, 3.0), 0.3333333333333333),
        14 => same(softfloat::sub(1.0, 1.0e-308), 1.0),
        15 => same(softfloat::rem(5.5, 2.0), 1.5),
        16 => same(softfloat::div(0.0, 0.0), f64::NAN) && softfloat::div(0.0, 0.0).is_nan(),
        17 => same(
            softfloat::from_i64(9_007_199_254_740_993),
            9_007_199_254_740_992.0,
        ),
        18 => softfloat::to_i64(1.9e18) == 1_900_000_000_000_000_000,

        // Integer conversions.
        19 => to_uint32(4_294_967_297.0) == 1 && to_int32(4_294_967_297.0) == 1,
        20 => to_int32(-2_147_483_649.0) == 2_147_483_647,
        21 => to_uint32(f64::NAN) == 0 && to_uint32(f64::INFINITY) == 0,
        22 => to_int32(1.9) == 1 && to_int32(-1.9) == -1,
        23 => same(truncate(-0.5), unary_minus(0.0)) && same(truncate(1.9), 1.0),
        24 => to_integer_or_infinity(f64::NAN) == 0.0 && to_length(-5.0) == 0.0,
        25 => is_integral(3.0) && !is_integral(3.5),

        // Operators and comparisons.
        26 => bitwise_not(5.0) == -6.0 && bitwise_and(12.0, 10.0) == 8.0,
        27 => {
            bitwise_or(12.0, 10.0) == 14.0
                && bitwise_xor(12.0, 10.0) == 6.0
                && shift_left(1.0, 31.0) == -2_147_483_648.0
        }
        28 => unsigned_shift_right(-1.0, 0.0) == 4_294_967_295.0 && shift_right(-16.0, 2.0) == -4.0,
        29 => {
            !number_equals(f64::NAN, f64::NAN)
                && number_equals(0.0, unary_minus(0.0))
                && same_value_number(f64::NAN, f64::NAN)
                && !same_value_number(0.0, unary_minus(0.0))
                && same_value_zero_number(0.0, unary_minus(0.0))
        }
        30 => {
            matches!(compare_numbers(1.0, 2.0), Comparison::Less)
                && matches!(compare_numbers(f64::NAN, 1.0), Comparison::Undefined)
                && strict_equals(&Value::number(1.0), &Value::number(1.0)) == Some(true)
        }

        // Strings to numbers.
        31 => {
            same(number_of(b"  12  "), 12.0)
                && same(number_of(b"0x1f"), 31.0)
                && same(number_of(b"-1.5e2"), -150.0)
                && number_of(b"1x").is_nan()
                && same(number_of(b""), 0.0)
                && same(number_of(b"Infinity"), f64::INFINITY)
        }
        _ => true,
    }
}

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    report_out: i32,
    exit_out: i32,
    progress: probe::Progress,
}

entry! {
    State;
    primary { report_out }
    inputs {}
    outputs { exit_out = 1 }
}

#[cfg_attr(not(feature = "host-test"), unsafe(no_mangle))]
#[link_section = ".text.module_step"]
pub extern "C" fn module_step(state: *mut u8) -> i32 {
    if state.is_null() {
        return -1;
    }
    // SAFETY: `state` is the block `module_new` laid out, non-null as
    // checked above, and the loader hands it to one step at a time.
    let state = unsafe { &mut *state.cast::<State>() };
    if state.syscalls.is_null() {
        return -2;
    }
    // SAFETY: the table pointer was stored by `module_new` and checked
    // non-null above; the loader keeps it live for the module's lifetime.
    let syscalls = unsafe { &*state.syscalls };
    probe::step(
        &mut state.progress,
        syscalls,
        state.report_out,
        state.exit_out,
        b"phasor-value-probe",
        CASE_COUNT,
        run_case,
    )
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
