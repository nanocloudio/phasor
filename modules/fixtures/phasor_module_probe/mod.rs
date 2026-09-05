//! On-graph conformance probe for module identity, resolution, and linking.

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

#[path = "../../common/digest.rs"]
mod digest;
#[path = "../../common/link.rs"]
mod link;
#[path = "../../common/module.rs"]
mod module;
#[path = "../../common/probe.rs"]
mod probe;
#[path = "../../common/text.rs"]
mod text;
#[path = "../../common/wire.rs"]
mod wire;

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
        b"phasor-module-probe",
        CASE_COUNT,
        |case| run_case(&mut state.storage, case),
    )
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
