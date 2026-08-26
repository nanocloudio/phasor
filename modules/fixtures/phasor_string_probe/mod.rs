//! On-graph conformance probe for the heap, strings, property keys, and
//! `Number::toString`.

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
#[path = "../../common/softfloat.rs"]
mod softfloat;
#[path = "../../common/string.rs"]
mod string;
#[path = "../../common/value.rs"]
mod value;

use heap::{CellKind, Heap, HeapError, Slot};
use string::{array_index, number_to_string, Atoms, Key};
use value::Handle;

const CASE_COUNT: u16 = 28;
const ARENA_BYTES: usize = 16 * 1024;
const SLOT_COUNT: usize = 128;
const ATOM_ENTRIES: usize = 64;
const ATOM_HANDLES: usize = 32;

/// Heap storage owned by the module.
struct Storage {
    arena: [u8; ARENA_BYTES],
    slots: [Slot; SLOT_COUNT],
    entries: [u32; ATOM_ENTRIES],
    handles: [Handle; ATOM_HANDLES],
}

/// Turn ASCII into code units for a case.
fn units(text: &[u8], out: &mut [u16; 64]) -> usize {
    let mut length = 0usize;
    for &byte in text {
        if length < out.len() {
            out[length] = u16::from(byte);
            length += 1;
        }
    }
    length
}

/// Whether a string cell holds exactly this ASCII text.
fn holds(heap: &Heap<'_>, handle: Handle, text: &[u8]) -> bool {
    let Ok(length) = string::length(heap, handle) else {
        return false;
    };
    if length as usize != text.len() {
        return false;
    }
    let mut index = 0u32;
    while (index as usize) < text.len() {
        match string::unit_at(heap, handle, index) {
            Ok(Some(unit)) if unit == u16::from(text[index as usize]) => {}
            _ => return false,
        }
        index += 1;
    }
    true
}

/// Whether `Number::toString` writes exactly this ASCII text.
fn prints(value: f64, text: &[u8]) -> bool {
    let mut out = [0u16; 64];
    let written = number_to_string(value, &mut out);
    if written != text.len() {
        return false;
    }
    let mut index = 0usize;
    while index < text.len() {
        if out[index] != u16::from(text[index]) {
            return false;
        }
        index += 1;
    }
    true
}

#[allow(
    clippy::match_same_arms,
    reason = "each case is an independent assertion and merging arms would hide which one failed"
)]
fn run_case(storage: &mut Storage, case: u16) -> bool {
    let mut heap = Heap::new(&mut storage.arena, &mut storage.slots);
    let mut buffer = [0u16; 64];

    match case {
        // Heap and string cells.
        0 => {
            let length = units(b"hello", &mut buffer);
            let handle = match string::create(&mut heap, buffer.get(..length).unwrap_or(&[])) {
                Ok(handle) => handle,
                Err(_) => return false,
            };
            holds(&heap, handle, b"hello") && string::length(&heap, handle) == Ok(5)
        }
        1 => {
            let handle = match string::create_ascii(&mut heap, b"abc") {
                Ok(handle) => handle,
                Err(_) => return false,
            };
            holds(&heap, handle, b"abc")
        }
        2 => {
            // A supplementary code point occupies two units.
            let text = [0xD83Du16, 0xDE00];
            let handle = match string::create(&mut heap, &text) {
                Ok(handle) => handle,
                Err(_) => return false,
            };
            string::length(&heap, handle) == Ok(2)
                && string::unit_at(&heap, handle, 0) == Ok(Some(0xD83D))
                && string::unit_at(&heap, handle, 1) == Ok(Some(0xDE00))
                && string::unit_at(&heap, handle, 2) == Ok(None)
        }
        3 => {
            let first = string::create_ascii(&mut heap, b"ab").unwrap_or(Handle::new(0, 0));
            let second = string::create_ascii(&mut heap, b"ab").unwrap_or(Handle::new(0, 0));
            first != second && string::equals(&heap, first, second) == Ok(true)
        }
        4 => {
            let first = string::create_ascii(&mut heap, b"ab").unwrap_or(Handle::new(0, 0));
            let second = string::create_ascii(&mut heap, b"abc").unwrap_or(Handle::new(0, 0));
            string::compare(&heap, first, second) == Ok(core::cmp::Ordering::Less)
                && string::compare(&heap, second, first) == Ok(core::cmp::Ordering::Greater)
                && string::compare(&heap, first, first) == Ok(core::cmp::Ordering::Equal)
        }
        5 => {
            let first = string::create_ascii(&mut heap, b"foo").unwrap_or(Handle::new(0, 0));
            let second = string::create_ascii(&mut heap, b"bar").unwrap_or(Handle::new(0, 0));
            match string::concat(&mut heap, first, second) {
                Ok(joined) => holds(&heap, joined, b"foobar"),
                Err(_) => false,
            }
        }
        6 => {
            // Joining a one-byte string with a two-byte one widens the result.
            let first = string::create_ascii(&mut heap, b"a").unwrap_or(Handle::new(0, 0));
            let wide = [0x4E2Du16];
            let second = match string::create(&mut heap, &wide) {
                Ok(handle) => handle,
                Err(_) => return false,
            };
            match string::concat(&mut heap, first, second) {
                Ok(joined) => {
                    string::length(&heap, joined) == Ok(2)
                        && string::unit_at(&heap, joined, 1) == Ok(Some(0x4E2D))
                }
                Err(_) => false,
            }
        }
        7 => {
            // A retired handle is stale for good.
            let handle = string::create_ascii(&mut heap, b"x").unwrap_or(Handle::new(0, 0));
            heap.retire(handle).is_ok()
                && string::length(&heap, handle) == Err(HeapError::StaleHandle)
        }
        8 => {
            // A wrong generation is stale even when the slot is live.
            let handle = string::create_ascii(&mut heap, b"x").unwrap_or(Handle::new(0, 0));
            let forged = Handle::new(handle.index, handle.generation.wrapping_add(1));
            string::length(&heap, forged) == Err(HeapError::StaleHandle)
        }
        9 => {
            // Exhaustion is an ordinary result.
            let mut allocated = 0u32;
            loop {
                match heap.allocate(CellKind::String, 512) {
                    Ok(_) => allocated += 1,
                    Err(HeapError::ArenaFull | HeapError::SlotsFull) => break,
                    Err(_) => return false,
                }
                if allocated > 1000 {
                    return false;
                }
            }
            allocated > 0 && heap.free() < 512
        }

        // Property keys.
        10 => array_index(&[u16::from(b'0')]) == Some(0),
        11 => {
            let length = units(b"42", &mut buffer);
            array_index(buffer.get(..length).unwrap_or(&[])) == Some(42)
        }
        12 => {
            let length = units(b"01", &mut buffer);
            array_index(buffer.get(..length).unwrap_or(&[])).is_none()
        }
        13 => {
            let length = units(b"4294967294", &mut buffer);
            let max = array_index(buffer.get(..length).unwrap_or(&[]));
            let length = units(b"4294967295", &mut buffer);
            max == Some(string::MAX_ARRAY_INDEX)
                && array_index(buffer.get(..length).unwrap_or(&[])).is_none()
        }
        14 => {
            let length = units(b"1.5", &mut buffer);
            array_index(buffer.get(..length).unwrap_or(&[])).is_none() && array_index(&[]).is_none()
        }
        15 => {
            let mut atoms = Atoms::new(&mut storage.entries, &mut storage.handles);
            let length = units(b"key", &mut buffer);
            let key = buffer.get(..length).unwrap_or(&[]);
            let Ok(first) = atoms.intern(&mut heap, key) else {
                return false;
            };
            let Ok(second) = atoms.intern(&mut heap, key) else {
                return false;
            };
            first == second && atoms.count() == 1
        }
        16 => {
            let mut atoms = Atoms::new(&mut storage.entries, &mut storage.handles);
            let length = units(b"one", &mut buffer);
            let Ok(first) = atoms.intern(&mut heap, buffer.get(..length).unwrap_or(&[])) else {
                return false;
            };
            let length = units(b"two", &mut buffer);
            let Ok(second) = atoms.intern(&mut heap, buffer.get(..length).unwrap_or(&[])) else {
                return false;
            };
            first != second && atoms.count() == 2
        }
        17 => {
            let mut atoms = Atoms::new(&mut storage.entries, &mut storage.handles);
            let length = units(b"here", &mut buffer);
            let Ok(handle) = atoms.intern(&mut heap, buffer.get(..length).unwrap_or(&[])) else {
                return false;
            };
            let found = atoms.lookup(&heap, buffer.get(..length).unwrap_or(&[]));
            let length = units(b"absent", &mut buffer);
            let missing = atoms.lookup(&heap, buffer.get(..length).unwrap_or(&[]));
            found == Ok(Some(handle)) && missing == Ok(None)
        }
        18 => {
            let handle = string::create_ascii(&mut heap, b"k").unwrap_or(Handle::new(0, 0));
            matches!(Key::Name(handle), Key::Name(_)) && Key::Index(3) == Key::Index(3)
        }

        // Number::toString, which must agree with the language exactly.
        19 => prints(0.0, b"0") && prints(value::unary_minus(0.0), b"0"),
        20 => prints(1.0, b"1") && prints(-1.0, b"-1") && prints(42.0, b"42"),
        21 => prints(0.1, b"0.1") && prints(0.5, b"0.5") && prints(123.456, b"123.456"),
        22 => prints(1e20, b"100000000000000000000") && prints(1e21, b"1e+21"),
        23 => prints(1e-6, b"0.000001") && prints(1e-7, b"1e-7"),
        24 => prints(f64::NAN, b"NaN") && prints(f64::INFINITY, b"Infinity"),
        25 => prints(f64::NEG_INFINITY, b"-Infinity") && prints(-0.5, b"-0.5"),
        26 => prints(core::f64::consts::PI, b"3.141592653589793") && prints(255.0, b"255"),
        27 => {
            prints(f64::from_bits(1), b"5e-324")
                && prints(1.797_693_134_862_315_7e308, b"1.7976931348623157e+308")
                && prints(9_007_199_254_740_992.0, b"9007199254740992")
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
            b"phasor-string-probe: 28 passed\n"
        } else {
            let prefix = b"phasor-string-probe: failed at ";
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
