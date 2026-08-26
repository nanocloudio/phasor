//! On-graph conformance probe for the bounded heap and its collection slices.

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
#[path = "../../common/gc.rs"]
mod gc;
#[path = "../../common/heap.rs"]
mod heap;
#[path = "../../common/numeric.rs"]
mod numeric;
#[path = "../../common/object.rs"]
mod object;
#[path = "../../common/policy.rs"]
mod policy;
#[path = "../../common/softfloat.rs"]
mod softfloat;
#[path = "../../common/string.rs"]
mod string;
#[path = "../../common/value.rs"]
mod value;

use heap::{Heap, HeapError, Phase, Slot};
use object::{attribute, Descriptor, Lookup};
use policy::{Outcome, Policy, State};
use string::Key;
use value::{Handle, Value};

const CASE_COUNT: u16 = 16;
const ARENA_BYTES: usize = 16 * 1024;
const SLOT_COUNT: usize = 256;
const WORKLIST: usize = 128;
const ROOT_COUNT: usize = 64;

struct Storage {
    arena: [u8; ARENA_BYTES],
    slots: [Slot; SLOT_COUNT],
    worklist: [u32; WORKLIST],
    roots: [Handle; ROOT_COUNT],
}

#[allow(
    clippy::match_same_arms,
    reason = "each case is an independent assertion and merging arms would hide which one failed"
)]
fn run_case(storage: &mut Storage, case: u16) -> bool {
    let mut heap = Heap::with_worklist(
        &mut storage.arena,
        &mut storage.slots,
        &mut storage.worklist,
    );

    match case {
        // The policy contract.
        0 => Policy::MODEST.clamped() == Policy::MODEST,
        1 => {
            // A policy asking for more than the ceiling is clamped down.
            let greedy = Policy {
                heap_bytes: u32::MAX,
                fuel: u64::MAX,
                ..Policy::CEILING
            };
            let clamped = greedy.clamped();
            clamped.heap_bytes == Policy::CEILING.heap_bytes && clamped.fuel == Policy::CEILING.fuel
        }
        2 => {
            let policy = Policy::MODEST;
            policy.admits(
                policy.heap_bytes,
                policy.heap_cells,
                policy.frames,
                policy.registers,
            ) && !policy.admits(0, 0, 0, 0)
        }
        3 => State::Empty.admits(State::Ready) && !State::Empty.admits(State::Running),
        4 => State::Running.admits(State::Suspended) && State::Suspended.admits(State::Running),
        5 => !State::Stopped.admits(State::Ready),
        6 => {
            Outcome::Returned.catchable()
                && Outcome::Threw.catchable()
                && !Outcome::Cancelled.catchable()
                && !Outcome::FuelExhausted.catchable()
        }
        7 => {
            Outcome::FuelExhausted.resumable()
                && Outcome::DeadlineReached.resumable()
                && !Outcome::HeapExhausted.resumable()
                && !Outcome::Cancelled.resumable()
        }

        // Collection.
        8 => {
            // A reachable object and everything it names survive.
            let Ok(keep) = string::create_ascii(&mut heap, b"survivor") else {
                return false;
            };
            let Ok(object) = object::create(&mut heap, Value::NULL) else {
                return false;
            };
            if object::define_own_property(
                &mut heap,
                object,
                Key::Name(keep),
                Descriptor::data(Value::string(keep), attribute::DEFAULT),
            )
            .is_err()
            {
                return false;
            }
            let mut index = 0u32;
            while index < 50 {
                if string::create_ascii(&mut heap, b"garbage").is_err() {
                    return false;
                }
                index += 1;
            }
            let before = heap.used();
            if gc::collect(&mut heap, &[object], 64).is_err() {
                return false;
            }
            heap.used() < before
                && string::length(&heap, keep) == Ok(8)
                && matches!(
                    object::get(&heap, object, Key::Name(keep)),
                    Ok(Lookup::Value(value)) if value.as_handle() == keep
                )
        }
        9 => {
            // Unreachable cells are reclaimed and their handles go stale.
            let Ok(doomed) = string::create_ascii(&mut heap, b"unreachable") else {
                return false;
            };
            let Ok(root) = object::create(&mut heap, Value::NULL) else {
                return false;
            };
            if gc::collect(&mut heap, &[root], 64).is_err() {
                return false;
            }
            let (cells, bytes) = heap.reclaimed();
            cells >= 1 && bytes > 0 && string::length(&heap, doomed) == Err(HeapError::StaleHandle)
        }
        10 => {
            // A collection runs in slices, and nothing may be allocated while
            // one is in progress.
            let Ok(root) = object::create(&mut heap, Value::NULL) else {
                return false;
            };
            let mut index = 0u32;
            while index < 20 {
                if string::create_ascii(&mut heap, b"junk").is_err() {
                    return false;
                }
                index += 1;
            }
            if heap.begin_collection(&[root]).is_err() {
                return false;
            }
            let refused = string::create_ascii(&mut heap, b"during") == Err(HeapError::Collecting);
            let mut slices = 0u32;
            while heap.phase() != Phase::Idle && slices < 10_000 {
                gc::collect_slice(&mut heap, 1);
                slices += 1;
            }
            refused && slices > 2 && heap.phase() == Phase::Idle
        }
        11 => {
            // A chain of objects is preserved through a collection.
            let Ok(mut previous) = object::create(&mut heap, Value::NULL) else {
                return false;
            };
            let bottom = previous;
            let mut index = 0u32;
            while index < 20 {
                let Ok(child) = object::create(&mut heap, Value::object(previous)) else {
                    return false;
                };
                previous = child;
                index += 1;
            }
            if gc::collect(&mut heap, &[previous], 8).is_err() {
                return false;
            }
            let mut walker = previous;
            let mut depth = 0u32;
            loop {
                match object::prototype(&heap, walker) {
                    Ok(value) if value.is_object() => {
                        walker = value.as_handle();
                        depth += 1;
                    }
                    Ok(_) => break,
                    Err(_) => return false,
                }
            }
            depth == 20 && walker == bottom
        }
        12 => {
            // Allocation resumes after a collection.
            let Ok(root) = object::create(&mut heap, Value::NULL) else {
                return false;
            };
            if gc::collect(&mut heap, &[root], 64).is_err() {
                return false;
            }
            string::create_ascii(&mut heap, b"after").is_ok()
        }
        13 => {
            // A small heap sustains far more allocation than it can hold, by
            // collecting when it fills.
            let Ok(live) = object::create(&mut heap, Value::NULL) else {
                return false;
            };
            let mut allocated = 0u32;
            let mut collections = 0u32;
            while allocated < 2000 {
                match string::create_ascii(&mut heap, b"transient allocation") {
                    Ok(_) => allocated += 1,
                    Err(HeapError::ArenaFull | HeapError::SlotsFull) => {
                        if gc::collect(&mut heap, &[live], 64).is_err() {
                            return false;
                        }
                        collections += 1;
                        if collections > 500 {
                            return false;
                        }
                    }
                    Err(_) => return false,
                }
            }
            collections > 0 && heap.used() <= ARENA_BYTES as u32
        }
        14 => {
            // Exhaustion without a collection is still an ordinary result.
            let mut allocated = 0u32;
            loop {
                match heap.allocate(heap::CellKind::String, 256) {
                    Ok(_) => allocated += 1,
                    Err(HeapError::ArenaFull | HeapError::SlotsFull) => break,
                    Err(_) => return false,
                }
                if allocated > 10_000 {
                    return false;
                }
            }
            allocated > 0
        }
        15 => {
            // An object's property table is reached through the object, so
            // properties written before a collection are there after it.
            let Ok(object) = object::create(&mut heap, Value::NULL) else {
                return false;
            };
            let mut index = 0u32;
            while index < 16 {
                if object::set(
                    &mut heap,
                    object,
                    Key::Index(index),
                    Value::number(f64::from(index)),
                )
                .is_err()
                {
                    return false;
                }
                index += 1;
            }
            if gc::collect(&mut heap, &[object], 16).is_err() {
                return false;
            }
            let mut index = 0u32;
            while index < 16 {
                match object::get(&heap, object, Key::Index(index)) {
                    Ok(Lookup::Value(value)) if value.as_number() == f64::from(index) => {}
                    _ => return false,
                }
                index += 1;
            }
            true
        }
        _ => true,
    }
}

#[repr(C)]
struct State_ {
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
    u32::try_from(core::mem::size_of::<State_>()).unwrap_or(u32::MAX)
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
    if state_size < core::mem::size_of::<State_>() {
        return -2;
    }
    unsafe {
        let table = syscalls.cast::<SyscallTable>();
        let state = state.cast::<State_>();
        core::ptr::write_bytes(state.cast::<u8>(), 0, core::mem::size_of::<State_>());
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
    let state = unsafe { &mut *state.cast::<State_>() };
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
            b"phasor-gc-probe: 16 passed\n"
        } else {
            let prefix = b"phasor-gc-probe: failed at ";
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
