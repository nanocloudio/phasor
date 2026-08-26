//! On-graph conformance probe for ordinary objects: properties, descriptors,
//! attributes, accessors, extensibility, and the prototype chain.

#![no_std]
#![allow(
    dead_code,
    unused_imports,
    unreachable_patterns,
    reason = "the Fluxor ABI source is mounted as one surface and this fixture consumes a subset"
)]

use core::ffi::c_void;

#[path = "../../../target/fluxor/fluxor-abi/sdk/abi.rs"]
mod abi;
use abi::SyscallTable;

include!("../../../target/fluxor/fluxor-abi/sdk/runtime.rs");

#[path = "../../common/dtoa.rs"]
mod dtoa;
#[path = "../../common/heap.rs"]
mod heap;
#[path = "../../common/numeric.rs"]
mod numeric;
#[path = "../../common/object.rs"]
mod object;
#[path = "../../common/softfloat.rs"]
mod softfloat;
#[path = "../../common/string.rs"]
mod string;
#[path = "../../common/value.rs"]
mod value;

use heap::{Heap, Slot};
use object::{
    attribute, create, define_own_property, delete, get, get_own_property, has_property,
    is_extensible, own_keys, own_property_count, prevent_extensions, prototype, set, set_prototype,
    Assignment, Descriptor, Lookup, ObjectError, MAX_PROTOTYPE_DEPTH,
};
use string::Key;
use value::{Handle, Value};

const CASE_COUNT: u16 = 26;
const ARENA_BYTES: usize = 48 * 1024;
const SLOT_COUNT: usize = 512;

struct Storage {
    arena: [u8; ARENA_BYTES],
    slots: [Slot; SLOT_COUNT],
}

/// A name key from ASCII text.
fn name(heap: &mut Heap<'_>, text: &[u8]) -> Key {
    match string::create_ascii(heap, text) {
        Ok(handle) => Key::Name(handle),
        Err(_) => Key::Index(u32::MAX),
    }
}

fn value_of(lookup: Lookup) -> Option<f64> {
    match lookup {
        Lookup::Value(value) => Some(value.as_number()),
        _ => None,
    }
}

#[allow(
    clippy::match_same_arms,
    reason = "each case is an independent assertion and merging arms would hide which one failed"
)]
fn run_case(storage: &mut Storage, case: u16) -> bool {
    let mut heap = Heap::new(&mut storage.arena, &mut storage.slots);
    let key_a = name(&mut heap, b"a");
    let key_b = name(&mut heap, b"b");

    match case {
        0 => {
            let object = match create(&mut heap, Value::NULL) {
                Ok(object) => object,
                Err(_) => return false,
            };
            prototype(&heap, object).is_ok_and(|value| value.is_null())
                && is_extensible(&heap, object) == Ok(true)
                && own_property_count(&heap, object) == Ok(0)
        }
        1 => {
            let proto = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let object = create(&mut heap, Value::object(proto)).unwrap_or(Handle::new(0, 0));
            prototype(&heap, object).is_ok_and(|value| value.as_handle() == proto)
        }
        2 => {
            let object = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            define_own_property(
                &mut heap,
                object,
                key_a,
                Descriptor::data(Value::number(1.0), attribute::DEFAULT),
            ) == Ok(true)
                && get(&heap, object, key_a).ok().and_then(value_of) == Some(1.0)
        }
        3 => {
            let object = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            matches!(get(&heap, object, key_a), Ok(Lookup::Absent))
                && has_property(&heap, object, key_a) == Ok(false)
        }
        4 => {
            // A property found on the prototype is not an own property.
            let proto = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let object = create(&mut heap, Value::object(proto)).unwrap_or(Handle::new(0, 0));
            let _ = define_own_property(
                &mut heap,
                proto,
                key_b,
                Descriptor::data(Value::number(2.0), attribute::DEFAULT),
            );
            get(&heap, object, key_b).ok().and_then(value_of) == Some(2.0)
                && get_own_property(&heap, object, key_b).is_ok_and(|found| found.is_none())
                && has_property(&heap, object, key_b) == Ok(true)
        }
        5 => {
            let object = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            set(&mut heap, object, key_a, Value::number(5.0)) == Ok(Assignment::Done)
                && get(&heap, object, key_a).ok().and_then(value_of) == Some(5.0)
        }
        6 => {
            // Assigning over an inherited value creates an own property and
            // leaves the prototype alone.
            let proto = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let object = create(&mut heap, Value::object(proto)).unwrap_or(Handle::new(0, 0));
            let _ = set(&mut heap, proto, key_b, Value::number(2.0));
            let _ = set(&mut heap, object, key_b, Value::number(9.0));
            get(&heap, object, key_b).ok().and_then(value_of) == Some(9.0)
                && get(&heap, proto, key_b).ok().and_then(value_of) == Some(2.0)
                && own_property_count(&heap, object) == Ok(1)
        }
        7 => {
            let object = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let _ = define_own_property(
                &mut heap,
                object,
                key_a,
                Descriptor::data(Value::number(1.0), attribute::ENUMERABLE),
            );
            set(&mut heap, object, key_a, Value::number(2.0)) == Ok(Assignment::Refused)
                && get(&heap, object, key_a).ok().and_then(value_of) == Some(1.0)
        }
        8 => {
            // A non-writable inherited property blocks assignment on the child.
            let proto = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let object = create(&mut heap, Value::object(proto)).unwrap_or(Handle::new(0, 0));
            let _ = define_own_property(
                &mut heap,
                proto,
                key_a,
                Descriptor::data(Value::number(1.0), attribute::ENUMERABLE),
            );
            set(&mut heap, object, key_a, Value::number(2.0)) == Ok(Assignment::Refused)
                && own_property_count(&heap, object) == Ok(0)
        }
        9 => {
            let object = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let _ = define_own_property(
                &mut heap,
                object,
                key_a,
                Descriptor::data(Value::number(1.0), attribute::ENUMERABLE),
            );
            delete(&mut heap, object, key_a) == Ok(false)
        }
        10 => {
            let object = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let _ = set(&mut heap, object, key_a, Value::number(1.0));
            delete(&mut heap, object, key_a) == Ok(true)
                && matches!(get(&heap, object, key_a), Ok(Lookup::Absent))
                && delete(&mut heap, object, key_b) == Ok(true)
        }
        11 => {
            // A non-configurable, non-writable property may be redefined only
            // with the same value.
            let object = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let _ = define_own_property(
                &mut heap,
                object,
                key_a,
                Descriptor::data(Value::number(1.0), attribute::ENUMERABLE),
            );
            define_own_property(
                &mut heap,
                object,
                key_a,
                Descriptor::data(Value::number(3.0), attribute::ENUMERABLE),
            ) == Ok(false)
                && define_own_property(
                    &mut heap,
                    object,
                    key_a,
                    Descriptor::data(Value::number(1.0), attribute::ENUMERABLE),
                ) == Ok(true)
        }
        12 => {
            // A configurable property may be redefined freely.
            let object = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let _ = define_own_property(
                &mut heap,
                object,
                key_a,
                Descriptor::data(Value::number(1.0), attribute::CONFIGURABLE),
            );
            define_own_property(
                &mut heap,
                object,
                key_a,
                Descriptor::data(Value::TRUE, attribute::DEFAULT),
            ) == Ok(true)
        }
        13 => {
            let object = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let getter = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let setter = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let _ = define_own_property(
                &mut heap,
                object,
                key_a,
                Descriptor::accessor(
                    Value::object(getter),
                    Value::object(setter),
                    attribute::CONFIGURABLE,
                ),
            );
            matches!(get(&heap, object, key_a), Ok(Lookup::Accessor(value)) if value.as_handle() == getter)
        }
        14 => {
            let object = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let setter = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let _ = define_own_property(
                &mut heap,
                object,
                key_a,
                Descriptor::accessor(Value::UNDEFINED, Value::object(setter), attribute::DEFAULT),
            );
            set(&mut heap, object, key_a, Value::number(1.0)) == Ok(Assignment::Setter(setter))
        }
        15 => {
            // An accessor without a setter refuses the write.
            let object = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let getter = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let _ = define_own_property(
                &mut heap,
                object,
                key_a,
                Descriptor::accessor(Value::object(getter), Value::UNDEFINED, attribute::DEFAULT),
            );
            set(&mut heap, object, key_a, Value::number(1.0)) == Ok(Assignment::Refused)
        }
        16 => {
            // An inherited accessor is found through the chain.
            let proto = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let object = create(&mut heap, Value::object(proto)).unwrap_or(Handle::new(0, 0));
            let getter = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let _ = define_own_property(
                &mut heap,
                proto,
                key_a,
                Descriptor::accessor(Value::object(getter), Value::UNDEFINED, attribute::DEFAULT),
            );
            matches!(get(&heap, object, key_a), Ok(Lookup::Accessor(_)))
        }
        17 => {
            let object = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let _ = prevent_extensions(&mut heap, object);
            is_extensible(&heap, object) == Ok(false)
                && set(&mut heap, object, key_a, Value::TRUE) == Ok(Assignment::Refused)
                && define_own_property(
                    &mut heap,
                    object,
                    key_a,
                    Descriptor::data(Value::TRUE, attribute::DEFAULT),
                ) == Ok(false)
        }
        18 => {
            // An existing property is still writable on a sealed object.
            let object = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let _ = set(&mut heap, object, key_a, Value::number(1.0));
            let _ = prevent_extensions(&mut heap, object);
            set(&mut heap, object, key_a, Value::number(2.0)) == Ok(Assignment::Done)
        }
        19 => {
            let first = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let second = create(&mut heap, Value::object(first)).unwrap_or(Handle::new(0, 0));
            set_prototype(&mut heap, first, Value::object(second)) == Ok(false)
        }
        20 => {
            let object = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let other = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            set_prototype(&mut heap, object, Value::object(other)) == Ok(true)
                && prototype(&heap, object).is_ok_and(|value| value.as_handle() == other)
        }
        21 => {
            let object = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let _ = prevent_extensions(&mut heap, object);
            let other = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            set_prototype(&mut heap, object, Value::object(other)) == Ok(false)
        }
        22 => {
            // Keys come out as integer indices ascending, then names, then
            // symbols, each in the order they were added.
            let object = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let name_z = name(&mut heap, b"z");
            let name_y = name(&mut heap, b"y");
            let symbol = Key::Symbol(Handle::new(4242, 1));
            for key in [
                Key::Index(2),
                name_z,
                Key::Index(0),
                symbol,
                name_y,
                Key::Index(1),
            ] {
                let _ = set(&mut heap, object, key, Value::TRUE);
            }
            let mut keys = [Key::Index(u32::MAX); 8];
            let written = own_keys(&heap, object, &mut keys).unwrap_or(0);
            written == 6
                && keys[0] == Key::Index(0)
                && keys[1] == Key::Index(1)
                && keys[2] == Key::Index(2)
                && keys[3] == name_z
                && keys[4] == name_y
                && keys[5] == symbol
        }
        23 => {
            // The property table grows without changing the object's identity.
            let object = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let mut index = 0u32;
            while index < 40 {
                if set(
                    &mut heap,
                    object,
                    Key::Index(index),
                    Value::number(f64::from(index)),
                ) != Ok(Assignment::Done)
                {
                    return false;
                }
                index += 1;
            }
            let mut index = 0u32;
            while index < 40 {
                if get(&heap, object, Key::Index(index))
                    .ok()
                    .and_then(value_of)
                    != Some(f64::from(index))
                {
                    return false;
                }
                index += 1;
            }
            own_property_count(&heap, object) == Ok(40)
        }
        24 => {
            // Deleting keeps the order of what remains.
            let object = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let mut index = 0u32;
            while index < 8 {
                let _ = set(
                    &mut heap,
                    object,
                    Key::Index(index),
                    Value::number(f64::from(index)),
                );
                index += 1;
            }
            let _ = delete(&mut heap, object, Key::Index(3));
            own_property_count(&heap, object) == Ok(7)
                && get(&heap, object, Key::Index(4)).ok().and_then(value_of) == Some(4.0)
                && matches!(get(&heap, object, Key::Index(3)), Ok(Lookup::Absent))
        }
        25 => {
            // A chain longer than the admitted depth is a bounded failure.
            let mut current = create(&mut heap, Value::NULL).unwrap_or(Handle::new(0, 0));
            let mut depth = 0u32;
            while depth < MAX_PROTOTYPE_DEPTH + 4 {
                current = match create(&mut heap, Value::object(current)) {
                    Ok(object) => object,
                    Err(_) => return false,
                };
                depth += 1;
            }
            matches!(
                get(&heap, current, key_a),
                Err(ObjectError::PrototypeChainTooDeep)
            )
        }
        _ => true,
    }
}

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    report_out: i32,
    exit_out: i32,
    storage: Storage,
    case: u16,
    failures: u16,
    /// The first case that failed, which is what a report names.
    first_failure: u16,
    phase: u8,
}

#[no_mangle]
#[link_section = ".text.module_state_size"]
pub extern "C" fn module_state_size() -> u32 {
    u32::try_from(core::mem::size_of::<State>()).unwrap_or(u32::MAX)
}

#[no_mangle]
#[link_section = ".text.module_init"]
pub extern "C" fn module_init(_syscalls: *const c_void) {}

#[no_mangle]
#[link_section = ".text.module_new"]
pub extern "C" fn module_new(
    _in_chan: i32,
    out_chan: i32,
    _ctrl_chan: i32,
    _params: *const u8,
    _params_len: usize,
    state: *mut u8,
    state_size: usize,
    syscalls: *const c_void,
) -> i32 {
    if state.is_null() || syscalls.is_null() {
        return -1;
    }
    if state_size < core::mem::size_of::<State>() {
        return -2;
    }
    unsafe {
        let table = syscalls.cast::<SyscallTable>();
        let state = state.cast::<State>();
        core::ptr::write_bytes(state.cast::<u8>(), 0, core::mem::size_of::<State>());
        core::ptr::addr_of_mut!((*state).syscalls).write(table);
        core::ptr::addr_of_mut!((*state).report_out).write(out_chan);
        core::ptr::addr_of_mut!((*state).exit_out).write(dev_channel_port(&*table, 1, 1));
    }
    0
}

#[no_mangle]
#[link_section = ".text.module_step"]
pub extern "C" fn module_step(state: *mut u8) -> i32 {
    if state.is_null() {
        return -1;
    }
    let state = unsafe { &mut *state.cast::<State>() };
    if state.case == 0 && state.failures == 0 && state.first_failure == 0 {
        state.first_failure = u16::MAX;
    }
    if state.syscalls.is_null() {
        return -2;
    }
    let syscalls = unsafe { &*state.syscalls };

    if state.phase == 2 {
        return 1;
    }
    if state.case < CASE_COUNT {
        let case = state.case;
        if !run_case(&mut state.storage, case) {
            state.failures = state.failures.saturating_add(1);
            if state.first_failure == u16::MAX {
                state.first_failure = case;
            }
        }
        state.case = state.case.saturating_add(1);
        return 0;
    }

    if state.phase == 0 {
        // A failure names the first case that failed, so a report is enough
        // to find it without instrumenting the module again.
        let mut buffer = [0u8; 64];
        let report: &[u8] = if state.failures == 0 {
            b"phasor-object-probe: 26 passed\n"
        } else {
            let prefix = b"phasor-object-probe: failed at ";
            let mut length = 0usize;
            while length < prefix.len() {
                buffer[length] = prefix[length];
                length += 1;
            }
            let mut digits = [0u8; 5];
            let mut count = 0usize;
            let mut value = state.first_failure;
            loop {
                digits[count] = b'0' + u8::try_from(value % 10).unwrap_or(0);
                count += 1;
                value /= 10;
                if value == 0 {
                    break;
                }
            }
            while count > 0 {
                count -= 1;
                buffer[length] = digits[count];
                length += 1;
            }
            buffer[length] = b'\n';
            length += 1;
            buffer.get(..length).unwrap_or(&[])
        };
        // A graph that gives the probe no report port still runs it; the
        // outcome then shows in the module's own completion status.
        if state.report_out >= 0 {
            let written = unsafe {
                (syscalls.channel_write)(state.report_out, report.as_ptr(), report.len())
            };
            if written != i32::try_from(report.len()).unwrap_or(i32::MAX) {
                return 0;
            }
        }
        state.phase = 1;
    }

    if state.exit_out >= 0 {
        let code = i32::from(state.failures != 0).to_le_bytes();
        let written =
            unsafe { (syscalls.channel_write)(state.exit_out, code.as_ptr(), code.len()) };
        if written != i32::try_from(code.len()).unwrap_or(i32::MAX) {
            return 0;
        }
    }
    state.phase = 2;
    // Completing is the pass signal for a graph with no port to report on; a
    // failure is a module error, which the kernel reports either way.
    if state.failures == 0 {
        1
    } else {
        -3
    }
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
