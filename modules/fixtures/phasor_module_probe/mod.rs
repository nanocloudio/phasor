//! On-graph conformance probe for module identity, resolution, and linking.

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

#[path = "../../common/digest.rs"]
mod digest;
#[path = "../../common/link.rs"]
mod link;
#[path = "../../common/module.rs"]
mod module;

use module::{Form, Import, Key, Record, Registry, Rejection, Status, MAX_CLOSURE};

const CASE_COUNT: u16 = 14;
const RECORD_COUNT: usize = 80;
const IMPORT_COUNT: usize = 160;
const STACK_DEPTH: usize = 96;

struct Storage {
    records: [Record; RECORD_COUNT],
    imports: [Import; IMPORT_COUNT],
    stack: [(u32, u32); STACK_DEPTH],
    state: [u8; RECORD_COUNT],
}

/// A key from a short name.
fn key(name: &[u8]) -> Key {
    Key(digest::digest(name))
}

#[allow(
    clippy::match_same_arms,
    reason = "each case is an independent assertion and merging arms would hide which one failed"
)]
fn run_case(storage: &mut Storage, case: u16) -> bool {
    let mut registry = Registry::new(&mut storage.records, &mut storage.imports);

    match case {
        // Identity and registration.
        0 => {
            let first = registry.register(key(b"a"), Form::Source);
            let again = registry.register(key(b"a"), Form::Source);
            first == again && registry.count() == 1
        }
        1 => {
            let _ = registry.register(key(b"a"), Form::Source);
            registry.register(key(b"a"), Form::Image) == Err(Rejection::DuplicateKey)
        }
        2 => {
            let _ = registry.register(key(b"a"), Form::Source);
            let _ = registry.register(key(b"b"), Form::Image);
            registry.count() == 2 && registry.index_of(key(b"b")) == Some(1)
        }
        3 => {
            let mut count = 0usize;
            loop {
                let mut name = [b'm', 0, 0];
                name[1] = b'0' + u8::try_from(count % 10).unwrap_or(0);
                name[2] = b'0' + u8::try_from(count / 10).unwrap_or(0);
                match registry.register(key(&name), Form::Source) {
                    Ok(_) => count += 1,
                    Err(Rejection::ClosureTooLarge) => break,
                    Err(_) => return false,
                }
                if count > MAX_CLOSURE + 4 {
                    return false;
                }
            }
            count == MAX_CLOSURE
        }

        // Imports and resolution.
        4 => {
            let Ok(module) = registry.register(key(b"a"), Form::Source) else {
                return false;
            };
            let _ = registry.add_import(module, 0, 3);
            let _ = registry.register(key(b"b"), Form::Source);
            registry.imports(module).len() == 1
                && registry.imports(module)[0].resolved.is_none()
                && registry.resolve(module, 0, key(b"b")) == Ok(())
                && registry.imports(module)[0].resolved == Some(key(b"b"))
        }
        5 => {
            let Ok(module) = registry.register(key(b"a"), Form::Source) else {
                return false;
            };
            let _ = registry.add_import(module, 0, 3);
            registry.resolve(module, 0, key(b"absent")) == Err(Rejection::MissingDependency)
        }
        6 => {
            let Ok(module) = registry.register(key(b"a"), Form::Source) else {
                return false;
            };
            registry.resolve(module, 0, key(b"a")) == Err(Rejection::WrongState)
        }

        // The lifecycle.
        7 => {
            let Ok(module) = registry.register(key(b"a"), Form::Source) else {
                return false;
            };
            registry.set_status(module, Status::Linked) == Err(Rejection::WrongState)
                && registry.set_status(module, Status::Linking) == Ok(())
                && registry.set_status(module, Status::Linked) == Ok(())
        }
        8 => {
            Status::Linking.admits(Status::Failed)
                && !Status::Failed.admits(Status::Linking)
                && !Status::Evaluated.admits(Status::Evaluating)
        }

        // Linking.
        9 => {
            // a imports b and c; c imports b.
            let Ok(a) = registry.register(key(b"a"), Form::Source) else {
                return false;
            };
            let _ = registry.add_import(a, 0, 1);
            let _ = registry.add_import(a, 2, 3);
            let Ok(b) = registry.register(key(b"b"), Form::Source) else {
                return false;
            };
            let Ok(c) = registry.register(key(b"c"), Form::Source) else {
                return false;
            };
            let _ = registry.add_import(c, 4, 5);
            let _ = registry.resolve(a, 0, key(b"b"));
            let _ = registry.resolve(a, 1, key(b"c"));
            let _ = registry.resolve(c, 0, key(b"b"));
            let Ok(closure) = link::link(&mut registry, a, &mut storage.stack, &mut storage.state)
            else {
                return false;
            };
            closure.count == 3
                && registry.order(b) < registry.order(c)
                && registry.order(c) < registry.order(a)
                && link::is_linked(&registry)
        }
        10 => {
            // A cycle links.
            let Ok(a) = registry.register(key(b"a"), Form::Source) else {
                return false;
            };
            let _ = registry.add_import(a, 0, 1);
            let Ok(b) = registry.register(key(b"b"), Form::Source) else {
                return false;
            };
            let _ = registry.add_import(b, 2, 3);
            let _ = registry.resolve(a, 0, key(b"b"));
            let _ = registry.resolve(b, 0, key(b"a"));
            match link::link(&mut registry, a, &mut storage.stack, &mut storage.state) {
                Ok(closure) => closure.count == 2 && registry.order(b) < registry.order(a),
                Err(_) => false,
            }
        }
        11 => {
            // An unresolved import fails, and nothing claims to be linked.
            let Ok(a) = registry.register(key(b"a"), Form::Source) else {
                return false;
            };
            let _ = registry.add_import(a, 0, 1);
            let _ = registry.register(key(b"b"), Form::Source);
            matches!(
                link::link(&mut registry, a, &mut storage.stack, &mut storage.state),
                Err(Rejection::Unresolved)
            ) && !link::is_linked(&registry)
        }
        12 => {
            // A shared dependency is evaluated before both of its importers.
            let Ok(a) = registry.register(key(b"a"), Form::Source) else {
                return false;
            };
            let _ = registry.add_import(a, 0, 1);
            let _ = registry.add_import(a, 2, 3);
            let Ok(b) = registry.register(key(b"b"), Form::Source) else {
                return false;
            };
            let _ = registry.add_import(b, 4, 5);
            let Ok(c) = registry.register(key(b"c"), Form::Source) else {
                return false;
            };
            let _ = registry.add_import(c, 6, 7);
            let Ok(d) = registry.register(key(b"d"), Form::Source) else {
                return false;
            };
            let _ = registry.resolve(a, 0, key(b"b"));
            let _ = registry.resolve(a, 1, key(b"c"));
            let _ = registry.resolve(b, 0, key(b"d"));
            let _ = registry.resolve(c, 0, key(b"d"));
            let Ok(closure) = link::link(&mut registry, a, &mut storage.stack, &mut storage.state)
            else {
                return false;
            };
            let first = closure.digest;
            let relinked = link::link(&mut registry, a, &mut storage.stack, &mut storage.state);
            closure.count == 4
                && registry.order(d) < registry.order(b)
                && registry.order(d) < registry.order(c)
                && relinked.is_ok_and(|second| second.digest == first)
        }
        13 => {
            // A chain deeper than the walk's stack is a bounded failure.
            let Ok(mut previous) = registry.register(key(b"m0"), Form::Source) else {
                return false;
            };
            let mut index = 1u8;
            while index < 40 {
                let _ = registry.add_import(previous, 0, 1);
                let name = [b'm', b'0' + index % 10, b'0' + index / 10];
                let Ok(next) = registry.register(key(&name), Form::Source) else {
                    return false;
                };
                let _ = registry.resolve(previous, 0, key(&name));
                previous = next;
                index += 1;
            }
            let mut small = [(0u32, 0u32); 4];
            matches!(
                link::link(&mut registry, 0, &mut small, &mut storage.state),
                Err(Rejection::ClosureTooLarge)
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
            b"phasor-module-probe: 14 passed\n"
        } else {
            let prefix = b"phasor-module-probe: failed at ";
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
