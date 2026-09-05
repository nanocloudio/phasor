//! The entry points every fmod exports, written once.
//!
//! The loader calls four functions by name: the state size, an init hook, a
//! constructor that lays the module's `State` over the arena it was given,
//! and the step. The first three are the same in every module but for the
//! `State` type, the names of the two primary port fields, the extra ports
//! `dev_channel_port` resolves, and whether a parameter block is applied.
//! `entry!` writes them from exactly that list; `module_step` stays the
//! module's own, because it is the module.
//!
//! The attribute forms are the ones the Fluxor modules standard prescribes:
//! the symbol is exported only outside a host-test build. Phasor has no such
//! build, so the gate is never taken, and costs nothing here.
//!
//! This is one of the few common files that names the Fluxor ABI. It is a
//! textual macro, consumed through `#[macro_use] mod entry;` in the entry
//! file, never `#[macro_export]`.

/// Emit `module_state_size`, `module_init`, and `module_new` for `$state`.
///
/// ```ignore
/// entry! {
///     State;
///     primary { image_in, result_out }
///     inputs { completion_in = 1, control_in = 2 }
///     outputs { call_out = 1, diagnostic_out = 2, exit_out = 3 }
///     params apply_params
/// }
/// ```
///
/// `primary` names the fields that take the loader's `in_chan` and
/// `out_chan`; `inputs` and `outputs` name the fields resolved by port
/// index; `params` names an `unsafe fn(&mut State, *const u8, usize)`, or is
/// omitted. Every field not named is zero.
#[allow(unused_macros, reason = "consumed by the fmods that include this file")]
macro_rules! entry {
    (
        $state:ty;
        primary { $in_field:ident, $out_field:ident }
        inputs { $($in_port:ident = $in_index:expr),* $(,)? }
        outputs { $($out_port:ident = $out_index:expr),* $(,)? }
        $(params $apply:ident)?
    ) => {
        #[cfg_attr(not(feature = "host-test"), unsafe(no_mangle))]
        #[link_section = ".text.module_state_size"]
        pub extern "C" fn module_state_size() -> u32 {
            u32::try_from(core::mem::size_of::<$state>()).unwrap_or(u32::MAX)
        }

        #[cfg_attr(not(feature = "host-test"), unsafe(no_mangle))]
        #[link_section = ".text.module_init"]
        pub extern "C" fn module_init(_syscalls: *const core::ffi::c_void) {}

        // Not `expect`: this macro is expanded by every module, and a module
        // whose `module_new` keeps no parameters never derefs the pointer, so
        // the lint fires in some crates and not others.
        #[allow(
            clippy::not_unsafe_ptr_arg_deref,
            reason = "the ABI fixes this signature: the loader passes the parameter block as a raw pointer and length, and the module reads it once under the contract that it is valid for that length"
        )]
        #[cfg_attr(not(feature = "host-test"), unsafe(no_mangle))]
        #[link_section = ".text.module_new"]
        pub extern "C" fn module_new(
            in_chan: i32,
            out_chan: i32,
            _ctrl_chan: i32,
            params: *const u8,
            params_len: usize,
            state: *mut u8,
            state_size: usize,
            syscalls: *const core::ffi::c_void,
        ) -> i32 {
            if state.is_null() || syscalls.is_null() {
                return -1;
            }
            if state_size < core::mem::size_of::<$state>() {
                return -2;
            }
            // SAFETY: the loader hands this module an arena of at least
            // `state_size` bytes, aligned for its state, that it owns for the
            // module's lifetime, and a syscall table that outlives it; the
            // parameter block is valid for `params_len` bytes or null.
            unsafe {
                let table = syscalls.cast::<$crate::abi::SyscallTable>();
                let state = state.cast::<$state>();
                core::ptr::write_bytes(state.cast::<u8>(), 0, core::mem::size_of::<$state>());
                core::ptr::addr_of_mut!((*state).syscalls).write(table);
                core::ptr::addr_of_mut!((*state).$in_field).write(in_chan);
                core::ptr::addr_of_mut!((*state).$out_field).write(out_chan);
                $(
                    core::ptr::addr_of_mut!((*state).$in_port)
                        .write(dev_channel_port(&*table, 0, $in_index));
                )*
                $(
                    core::ptr::addr_of_mut!((*state).$out_port)
                        .write(dev_channel_port(&*table, 1, $out_index));
                )*
                let _ = (params, params_len);
                $( $apply(&mut *state, params, params_len); )?
            }
            0
        }
    };
    (
        $state:ty;
        primary { $out_field:ident }
        inputs { $($in_port:ident = $in_index:expr),* $(,)? }
        outputs { $($out_port:ident = $out_index:expr),* $(,)? }
        $(params $apply:ident)?
    ) => {
        #[cfg_attr(not(feature = "host-test"), unsafe(no_mangle))]
        #[link_section = ".text.module_state_size"]
        pub extern "C" fn module_state_size() -> u32 {
            u32::try_from(core::mem::size_of::<$state>()).unwrap_or(u32::MAX)
        }

        #[cfg_attr(not(feature = "host-test"), unsafe(no_mangle))]
        #[link_section = ".text.module_init"]
        pub extern "C" fn module_init(_syscalls: *const core::ffi::c_void) {}

        // Not `expect`: this macro is expanded by every module, and a module
        // whose `module_new` keeps no parameters never derefs the pointer, so
        // the lint fires in some crates and not others.
        #[allow(
            clippy::not_unsafe_ptr_arg_deref,
            reason = "the ABI fixes this signature: the loader passes the parameter block as a raw pointer and length, and the module reads it once under the contract that it is valid for that length"
        )]
        #[cfg_attr(not(feature = "host-test"), unsafe(no_mangle))]
        #[link_section = ".text.module_new"]
        pub extern "C" fn module_new(
            in_chan: i32,
            out_chan: i32,
            _ctrl_chan: i32,
            params: *const u8,
            params_len: usize,
            state: *mut u8,
            state_size: usize,
            syscalls: *const core::ffi::c_void,
        ) -> i32 {
            if state.is_null() || syscalls.is_null() {
                return -1;
            }
            if state_size < core::mem::size_of::<$state>() {
                return -2;
            }
            // SAFETY: the loader hands this module an arena of at least
            // `state_size` bytes, aligned for its state, that it owns for the
            // module's lifetime, and a syscall table that outlives it; the
            // parameter block is valid for `params_len` bytes or null.
            unsafe {
                let table = syscalls.cast::<$crate::abi::SyscallTable>();
                let state = state.cast::<$state>();
                core::ptr::write_bytes(state.cast::<u8>(), 0, core::mem::size_of::<$state>());
                core::ptr::addr_of_mut!((*state).syscalls).write(table);
                let _ = in_chan;
                core::ptr::addr_of_mut!((*state).$out_field).write(out_chan);
                $(
                    core::ptr::addr_of_mut!((*state).$in_port)
                        .write(dev_channel_port(&*table, 0, $in_index));
                )*
                $(
                    core::ptr::addr_of_mut!((*state).$out_port)
                        .write(dev_channel_port(&*table, 1, $out_index));
                )*
                let _ = (params, params_len);
                $( $apply(&mut *state, params, params_len); )?
            }
            0
        }
    };
}
