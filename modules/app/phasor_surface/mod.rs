//! The standard surface, as a module that hands it over.
//!
//! A façade is a program: the JavaScript a deployment puts in front of the
//! program it runs. A host that runs linked closures links it as a module and
//! needs nothing else. A host that runs scripts — a shell — has nowhere to
//! link it, and carrying the source inside the host's own image binds the
//! surface to whatever room that image has left.
//!
//! So the surface is its own module. It writes its source once and hangs up;
//! whatever reads it compiles and runs it through the same front end and
//! verifier as any other program. The surface can then grow to this module's
//! own room rather than the host's, and a deployment chooses its profile by
//! wiring a different surface rather than by building a different host.
//!
//! Nothing here is privileged. The source it hands over reaches the outside
//! only through the bindings the deployment granted the program that runs it.

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

#[path = "../../common/facade.rs"]
mod facade;
#[path = "../../common/wire.rs"]
mod wire;

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    source_out: i32,
    /// How much of the source has left.
    written: usize,
    phase: u8,
}

entry! {
    State;
    primary { source_out }
    inputs {}
    outputs {}
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
    if state.syscalls.is_null() || state.source_out < 0 {
        return -2;
    }
    // SAFETY: the table pointer was stored by `module_new` and checked
    // non-null above; the loader keeps it live for the module's lifetime.
    let syscalls = unsafe { &*state.syscalls };
    if state.phase == 1 {
        return 1;
    }

    // The whole source, then the hang-up that says it was the whole of it.
    if wire::push_progress(
        syscalls,
        state.source_out,
        facade::SOURCE,
        &mut state.written,
    ) {
        state.phase = 1;
        return 1;
    }
    0
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
