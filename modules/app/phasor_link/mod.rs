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

#![cfg_attr(not(feature = "host-test"), no_std)]
#![allow(
    dead_code,
    unused_imports,
    unreachable_patterns,
    reason = "the Fluxor ABI source is mounted as one surface and this module consumes a subset"
)]

#[path = "../../../target/fluxor/fluxor-abi/sdk/abi.rs"]
mod abi;
use abi::SyscallTable;

include!("../../../target/fluxor/fluxor-abi/sdk/runtime.rs");

#[macro_use]
#[path = "../../common/entry.rs"]
mod entry;

#[path = "../../common/bytecode.rs"]
mod bytecode;
#[path = "../../common/capability.rs"]
mod capability;
#[path = "../../common/closure.rs"]
mod closure;
#[path = "../../common/diagnostic.rs"]
mod diagnostic;
#[path = "../../common/digest.rs"]
mod digest;
#[path = "../../common/feature.rs"]
mod feature;
#[path = "../../common/link.rs"]
mod link;
#[path = "../../common/module.rs"]
mod module;
#[path = "../../common/verify.rs"]
mod verify;
#[path = "../../common/wire.rs"]
mod wire;

use bytecode::Unit;
use module::{Form, Import, Key, Registry};

/// Modules one closure may hold.
const MAX_MODULES: usize = 16;
/// Bytes of specifiers and images one stream may carry.
const STREAM_CAPACITY: usize = 32 * 1024;
/// Bytes the closure it produces may take.
const CLOSURE_CAPACITY: usize = 40 * 1024;
const VERIFIER_CAPACITY: usize = 4096;
/// Bytes in one record's header: the two lengths.
const RECORD_HEADER: usize = 8;

/// Imports one closure may carry, over all its modules.
const MAX_IMPORTS: usize = 128;

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    unit_in: i32,
    closure_out: i32,
    exit_out: i32,
    stream: [u8; STREAM_CAPACITY],
    closure: [u8; CLOSURE_CAPACITY],
    verifier_state: [i32; VERIFIER_CAPACITY],
    stream_length: usize,
    closure_length: usize,
    written: usize,
    overflowed: bool,
    failed: bool,
    phase: u8,
}

/// Take the records apart, check them, order them, and write the closure.
fn link(state: &mut State) -> bool {
    if state.overflowed {
        return false;
    }
    let State {
        stream,
        stream_length,
        verifier_state,
        closure,
        closure_length,
        ..
    } = state;
    let stream = stream.get(..*stream_length).unwrap_or(&[]);

    // The stream is a sequence of records, each with its two lengths in front:
    // a specifier and an image.
    let mut modules = [(&[] as &[u8], &[] as &[u8]); MAX_MODULES];
    let mut count = 0usize;
    let mut at = 0usize;
    while at < stream.len() {
        let Some(header) = stream.get(at..at + RECORD_HEADER) else {
            return false;
        };
        let specifier_length =
            u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let image_length =
            u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        let specifier_at = at + RECORD_HEADER;
        let image_at = specifier_at + specifier_length;
        let end = image_at + image_length;
        if end > stream.len() || count >= MAX_MODULES {
            return false;
        }
        let (Some(specifier), Some(image)) = (
            stream.get(specifier_at..image_at),
            stream.get(image_at..end),
        ) else {
            return false;
        };
        modules[count] = (specifier, image);
        count += 1;
        at = end;
    }
    if count == 0 {
        return false;
    }
    let modules = &modules[..count];

    // Every image must be admissible before anything is linked: a closure of
    // images one of which does not verify is not a closure.
    let mut units = [Unit::EMPTY; MAX_MODULES];
    let mut index = 0usize;
    while index < count {
        let Ok(unit) = verify::admit(modules[index].1, verifier_state) else {
            return false;
        };
        units[index] = unit;
        index += 1;
    }

    // Register the modules under their specifiers, each with its imports, then
    // resolve every import to the module its specifier names. The name must be
    // one that module exports, or the closure is not linked however it is
    // ordered.
    let mut records = [module::Record::EMPTY; MAX_MODULES];
    let mut imports = [Import::EMPTY; MAX_IMPORTS];
    let mut registry = Registry::new(&mut records, &mut imports);
    let mut index = 0usize;
    while index < count {
        if registry.register(key_of(modules[index].0), Form::Image) != Ok(index) {
            return false;
        }
        // A capability import is not a module import: the registry never
        // holds one, and the isolate answers it at admission.
        let mut import = 0u32;
        while import < units[index].header().import_count {
            let mut specifier = [0u16; 64];
            let mut name = [0u16; 64];
            let Some((specifier_length, _, _)) =
                units[index].import_at(import, &mut specifier, &mut name)
            else {
                return false;
            };
            import += 1;
            if capability::is_capability(specifier.get(..specifier_length).unwrap_or(&[])) {
                continue;
            }
            if registry.add_import(index, 0, 0).is_err() {
                return false;
            }
        }
        index += 1;
    }
    let mut index = 0usize;
    while index < count {
        // The registry holds only the module imports, in the order they were
        // added, so the position a resolution names is counted here.
        let mut registered = 0usize;
        let mut import = 0u32;
        while import < units[index].header().import_count {
            let mut specifier = [0u16; 64];
            let mut name = [0u16; 64];
            let Some((specifier_length, name_length, _)) =
                units[index].import_at(import, &mut specifier, &mut name)
            else {
                return false;
            };
            // A capability import names what the deployment grants, not a
            // module the stream carries: the linker records nothing for it
            // and the isolate answers it at admission.
            if capability::is_capability(specifier.get(..specifier_length).unwrap_or(&[])) {
                import += 1;
                continue;
            }
            let position = registered;
            registered += 1;
            let Some(source) = find_module(modules, &specifier, specifier_length) else {
                return false;
            };
            if name_length != 0
                && units[source]
                    .export_slot(name.get(..name_length).unwrap_or(&[]))
                    .is_none()
            {
                return false;
            }
            if registry
                .resolve(index, position, key_of(modules[source].0))
                .is_err()
            {
                return false;
            }
            import += 1;
        }
        index += 1;
    }

    // Order the closure: a module comes after everything it imports. A cycle is
    // admitted — the specification allows one — and its members keep the order
    // they were reached in, which is what makes a read of a binding that has
    // not been initialised the error it should be. The entry is the module
    // nothing else imports, which is the last one the ordering reached.
    let mut stack = [(0u32, 0u32); MAX_MODULES];
    let mut walk = [0u8; MAX_MODULES];
    let Ok(linked) = link::link_all(&mut registry, &mut stack, &mut walk) else {
        return false;
    };
    let mut ordered = [(&[] as &[u8], &[] as &[u8]); MAX_MODULES];
    let mut index = 0usize;
    while index < count {
        let position = registry.order(index).unwrap_or(u32::MAX) as usize;
        if position >= count {
            return false;
        }
        ordered[position] = modules[index];
        index += 1;
    }

    let Ok(length) = closure::write(closure, &ordered[..count], linked.entry_order) else {
        return false;
    };
    *closure_length = length;
    true
}

/// A module's key in the registry: the digest of the specifier the stream
/// carried it under, which is what an import names.
fn key_of(specifier: &[u8]) -> Key {
    Key(digest::digest(specifier))
}

/// The module a specifier names, compared as the bytes the stream carried.
fn find_module(modules: &[(&[u8], &[u8])], specifier: &[u16], length: usize) -> Option<usize> {
    let mut index = 0usize;
    while index < modules.len() {
        let carried = modules[index].0;
        if carried.len() == length {
            let mut at = 0usize;
            let mut same = true;
            while at < length {
                if u16::from(carried[at]) != specifier[at] {
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

entry! {
    State;
    primary { unit_in, closure_out }
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
    if state.syscalls.is_null() || state.unit_in < 0 || state.closure_out < 0 {
        return -2;
    }
    // SAFETY: the table pointer was stored by `module_new` and checked
    // non-null above; the loader keeps it live for the module's lifetime.
    let syscalls = unsafe { &*state.syscalls };

    if state.phase == 3 {
        return 1;
    }

    // The whole stream is staged before anything is linked: a closure is every
    // module of it, and a partial stream is not one.
    if state.phase == 0 {
        let staged = wire::stage_stream(
            syscalls,
            state.unit_in,
            &mut state.stream,
            &mut state.stream_length,
            &mut state.overflowed,
        );
        if staged != wire::Staged::Complete {
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
        let closure = state.closure.get(..state.closure_length).unwrap_or(&[]);
        if wire::push_progress(syscalls, state.closure_out, closure, &mut state.written) {
            state.phase = 2;
        }
        return 0;
    }

    if !wire::push_exit(syscalls, state.exit_out, state.failed) {
        return 0;
    }
    state.phase = 3;
    // A stream that does not compile is an outcome, not a fault: the exit status
    // says what happened and the diagnostic says why.
    1
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
