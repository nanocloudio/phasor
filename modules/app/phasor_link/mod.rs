//! The linker: compiled modules in, one linked closure out.
//!
//! Each module arrives as its specifier and its unit image. The linker checks
//! that every image is admissible, that every import names a module the stream
//! carried, and that every imported name is one that module exports — and then
//! writes the closure in an order where a module comes after everything it
//! imports.
//!
//! Nothing here fetches anything. What a specifier resolves to is what the
//! stream said it was, which is what makes a closure a closed thing.

#![no_std]
#![allow(
    dead_code,
    unused_imports,
    unreachable_patterns,
    reason = "the Fluxor ABI source is mounted as one surface and this module consumes a subset"
)]

use core::ffi::c_void;

#[path = "../../../target/fluxor/fluxor-abi/sdk/abi.rs"]
mod abi;
use abi::SyscallTable;

include!("../../../target/fluxor/fluxor-abi/sdk/runtime.rs");

#[path = "../../common/bytecode.rs"]
mod bytecode;
#[path = "../../common/closure.rs"]
mod closure;
#[path = "../../common/diagnostic.rs"]
mod diagnostic;
#[path = "../../common/digest.rs"]
mod digest;
#[path = "../../common/feature.rs"]
mod feature;
#[path = "../../common/verify.rs"]
mod verify;

use bytecode::Unit;

const POLL_INPUT: u32 = 0x01;
const POLL_OUTPUT: u32 = 0x02;

/// Modules one closure may hold.
const MAX_MODULES: usize = 16;
/// Bytes of specifiers and images one stream may carry.
const STREAM_CAPACITY: usize = 32 * 1024;
/// Bytes the closure it produces may take.
const CLOSURE_CAPACITY: usize = 40 * 1024;
const VERIFIER_CAPACITY: usize = 4096;
/// Bytes in one record's header: the two lengths.
const RECORD_HEADER: usize = 8;

/// One module the stream carried.
#[derive(Clone, Copy)]
struct Record {
    specifier_at: u32,
    specifier_length: u32,
    image_at: u32,
    image_length: u32,
    /// Where the module sits in the order it will evaluate in.
    order: u32,
}

impl Record {
    const EMPTY: Self = Self {
        specifier_at: 0,
        specifier_length: 0,
        image_at: 0,
        image_length: 0,
        order: u32::MAX,
    };
}

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    unit_in: i32,
    closure_out: i32,
    exit_out: i32,
    stream: [u8; STREAM_CAPACITY],
    closure: [u8; CLOSURE_CAPACITY],
    verifier_state: [i32; VERIFIER_CAPACITY],
    records: [Record; MAX_MODULES],
    order: [u32; MAX_MODULES],
    visiting: [u8; MAX_MODULES],
    stream_length: usize,
    closure_length: usize,
    written: usize,
    record_count: usize,
    overflowed: bool,
    failed: bool,
    phase: u8,
}

/// Take the records apart, check them, order them, and write the closure.
fn link(state: &mut State) -> bool {
    if state.overflowed {
        return false;
    }
    // The stream is a sequence of records, each with its two lengths in front.
    let mut at = 0usize;
    state.record_count = 0;
    while at < state.stream_length {
        let Some(header) = state.stream.get(at..at + RECORD_HEADER) else {
            return false;
        };
        let specifier_length =
            u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let image_length =
            u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        let specifier_at = at + RECORD_HEADER;
        let image_at = specifier_at + specifier_length;
        let end = image_at + image_length;
        if end > state.stream_length || state.record_count >= MAX_MODULES {
            return false;
        }
        state.records[state.record_count] = Record {
            specifier_at: u32::try_from(specifier_at).unwrap_or(0),
            specifier_length: u32::try_from(specifier_length).unwrap_or(0),
            image_at: u32::try_from(image_at).unwrap_or(0),
            image_length: u32::try_from(image_length).unwrap_or(0),
            order: u32::MAX,
        };
        state.record_count += 1;
        at = end;
    }
    if state.record_count == 0 {
        return false;
    }

    // Every image must be admissible before anything is linked: a closure of
    // images one of which does not verify is not a closure.
    let stream: &[u8] =
        unsafe { core::slice::from_raw_parts(state.stream.as_ptr(), state.stream_length) };
    let mut units = [Unit::EMPTY; MAX_MODULES];
    let mut index = 0usize;
    while index < state.record_count {
        let record = state.records[index];
        let Some(image) =
            stream.get(record.image_at as usize..(record.image_at + record.image_length) as usize)
        else {
            return false;
        };
        let Ok(unit) = verify::admit(image, &mut state.verifier_state) else {
            return false;
        };
        units[index] = unit;
        index += 1;
    }

    // Order the closure: a module comes after everything it imports. A cycle is
    // admitted — the specification allows one — and its members keep the order
    // they were reached in, which is what makes a read of a binding that has
    // not been initialised the error it should be.
    let mut ordered = 0u32;
    let mut index = 0usize;
    while index < state.record_count {
        state.visiting[index] = 0;
        index += 1;
    }
    let mut index = 0usize;
    while index < state.record_count {
        if !order_module(state, &units, index, &mut ordered) {
            return false;
        }
        index += 1;
    }

    // The entry is the module nothing else imports, which is the last one the
    // ordering reached.
    let entry = ordered.saturating_sub(1);
    let mut modules = [(&[] as &[u8], &[] as &[u8]); MAX_MODULES];
    let mut index = 0usize;
    while index < state.record_count {
        let record = state.records[index];
        let Some(specifier) = stream.get(
            record.specifier_at as usize..(record.specifier_at + record.specifier_length) as usize,
        ) else {
            return false;
        };
        let Some(image) =
            stream.get(record.image_at as usize..(record.image_at + record.image_length) as usize)
        else {
            return false;
        };
        let position = record.order as usize;
        if position >= state.record_count {
            return false;
        }
        modules[position] = (specifier, image);
        index += 1;
    }

    let Ok(length) = closure::write(
        &mut state.closure,
        modules.get(..state.record_count).unwrap_or(&[]),
        entry,
    ) else {
        return false;
    };
    state.closure_length = length;
    true
}

/// Give a module its place in the order, after everything it imports.
fn order_module(
    state: &mut State,
    units: &[Unit<'_>; MAX_MODULES],
    index: usize,
    next: &mut u32,
) -> bool {
    if state.records[index].order != u32::MAX {
        return true;
    }
    if state.visiting[index] == 1 {
        // A cycle: the module is already on the way to being ordered, and the
        // one that reached it keeps its own place.
        return true;
    }
    state.visiting[index] = 1;
    let imports = units[index].header().import_count;
    let mut import = 0u32;
    while import < imports {
        let mut specifier = [0u16; 64];
        let mut name = [0u16; 64];
        let Some((specifier_length, name_length, _)) =
            units[index].import_at(import, &mut specifier, &mut name)
        else {
            return false;
        };
        let Some(source) = find_module(state, &specifier, specifier_length) else {
            return false;
        };
        // The name must be one that module exports, or the closure is not
        // linked however it is ordered.
        if name_length != 0
            && units[source]
                .export_slot(name.get(..name_length).unwrap_or(&[]))
                .is_none()
        {
            return false;
        }
        if !order_module(state, units, source, next) {
            return false;
        }
        import += 1;
    }
    state.visiting[index] = 2;
    if state.records[index].order == u32::MAX {
        state.records[index].order = *next;
        *next += 1;
    }
    true
}

/// The module a specifier names, compared as the bytes the stream carried.
fn find_module(state: &State, specifier: &[u16], length: usize) -> Option<usize> {
    let mut index = 0usize;
    while index < state.record_count {
        let record = state.records[index];
        if record.specifier_length as usize == length {
            let mut at = 0usize;
            let mut same = true;
            while at < length {
                let byte = state
                    .stream
                    .get(record.specifier_at as usize + at)
                    .copied()
                    .unwrap_or(0);
                if u16::from(byte) != specifier[at] {
                    same = false;
                    break;
                }
                at += 1;
            }
            if same {
                return Some(index);
            }
        }
        index += 1;
    }
    None
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
    in_chan: i32,
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
        core::ptr::addr_of_mut!((*state).unit_in).write(in_chan);
        core::ptr::addr_of_mut!((*state).closure_out).write(out_chan);
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
    if state.syscalls.is_null() || state.unit_in < 0 || state.closure_out < 0 {
        return -2;
    }
    let syscalls = unsafe { &*state.syscalls };

    if state.phase == 3 {
        return 1;
    }

    // The whole stream is staged before anything is linked: a closure is every
    // module of it, and a partial stream is not one.
    if state.phase == 0 {
        let poll = unsafe { (syscalls.channel_poll)(state.unit_in, POLL_INPUT | POLL_HUP) };
        if poll <= 0 {
            return 0;
        }
        if (poll as u32) & POLL_INPUT != 0 {
            let offset = state.stream_length;
            let remaining = STREAM_CAPACITY.saturating_sub(offset);
            if remaining == 0 {
                state.overflowed = true;
                let mut discard = [0u8; 64];
                let _ = unsafe {
                    (syscalls.channel_read)(state.unit_in, discard.as_mut_ptr(), discard.len())
                };
                return 0;
            }
            let read = unsafe {
                (syscalls.channel_read)(
                    state.unit_in,
                    state.stream.as_mut_ptr().add(offset),
                    remaining,
                )
            };
            if read > 0 {
                state.stream_length += usize::try_from(read).unwrap_or(0).min(remaining);
            }
            return 0;
        }
        if (poll as u32) & POLL_HUP == 0 {
            return 0;
        }
        state.failed = !link(state);
        state.phase = 1;
        return 0;
    }

    if state.phase == 1 {
        if state.failed {
            state.phase = 2;
            return 0;
        }
        let poll = unsafe { (syscalls.channel_poll)(state.closure_out, POLL_OUTPUT) };
        if poll <= 0 || (poll as u32) & POLL_OUTPUT == 0 {
            return 0;
        }
        let offset = state.written;
        let remaining = state.closure_length.saturating_sub(offset);
        if remaining == 0 {
            state.phase = 2;
            return 0;
        }
        let written = unsafe {
            (syscalls.channel_write)(
                state.closure_out,
                state.closure.as_ptr().add(offset),
                remaining,
            )
        };
        if written > 0 {
            state.written += usize::try_from(written).unwrap_or(0).min(remaining);
        }
        return 0;
    }

    if state.exit_out >= 0 {
        let code = i32::from(state.failed).to_le_bytes();
        let written =
            unsafe { (syscalls.channel_write)(state.exit_out, code.as_ptr(), code.len()) };
        if written != i32::try_from(code.len()).unwrap_or(i32::MAX) {
            return 0;
        }
    }
    state.phase = 3;
    // A stream that does not compile is an outcome, not a fault: the exit status
    // says what happened and the diagnostic says why.
    1
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
