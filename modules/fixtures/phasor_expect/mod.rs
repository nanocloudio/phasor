//! A program and the answer it must produce.
//!
//! A graph that runs the isolate takes the image from standard input and
//! hands the result to standard output, and a lane holds the one to the
//! other. A WebAssembly bundle has neither: it is self-contained, and what a
//! host sees of it is whether each module completed. This fixture is the two
//! ends folded into one module, so such a bundle can state a program and
//! what it must answer, and complete only when it did.
//!
//! The source leaves as one stream ending in a hang-up, which is what
//! `phasor_compile` takes; the result and the exit status come back on the
//! two inputs once the isolate has finished. A program that is a module has
//! one more leg: its image comes back on `image_in` and leaves on
//! `record_out` as the one linker record `phasor_link` turns into the
//! closure, since an image with capability imports runs only as a closure.
//! A result that is not the one the graph named is a module error, which the
//! host reports the same way it reports a probe with a failed case.

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
include!("../../../target/fluxor/fluxor-abi/sdk/runtime/params.rs");

#[macro_use]
#[path = "../../common/entry.rs"]
mod entry;

#[path = "../../common/text.rs"]
mod text;
#[path = "../../common/trace.rs"]
mod trace;
#[path = "../../common/wire.rs"]
mod wire;

/// Copy `src` into `dst` when they are the same length, answering whether
/// they were: a copy whose lengths the compiler cannot prove equal would
/// carry a panic path, and a module image carries none.
fn copy_into(dst: &mut [u8], src: &[u8]) -> bool {
    if dst.len() != src.len() {
        return false;
    }
    dst.copy_from_slice(src);
    true
}

/// Bytes of source one graph may state.
///
/// A parameter's length is one byte on the wire, so 255 is every byte a
/// graph can pass here however much room this module makes. A longer source
/// does not arrive truncated in a way anything can see -- the length wraps
/// and what lands is not what was written -- so the bound is stated at the
/// true figure rather than at a larger one that would only mislead whoever
/// wrote the graph.
const SOURCE_BYTES: usize = 255;
/// Bytes of answer one graph may state, and the most a result may be. The
/// answer is a parameter and so bounded as `SOURCE_BYTES` is; the result is
/// read off a port and may use the whole of this.
const RESULT_BYTES: usize = 256;
/// Bytes of image one module may compile to, and of the record it leaves as.
const IMAGE_BYTES: usize = 32 * 1024;
/// Bytes of specifier a graph may name the module by.
const SPECIFIER_BYTES: usize = 32;
/// The linker record's header: the specifier's length and the image's.
const RECORD_HEADER: usize = 8;

/// How the result is held to the answer.
const SHAPE_EXACT: u8 = 0;
/// The result is one or more decimal digits and nothing else: an answer
/// that is a number no graph can name in advance, such as a seed.
const SHAPE_DIGITS: u8 = 1;

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    announced: bool,
    /// Emit one `[exp]` line every `trace` steps; 0 is silent.
    trace: u32,
    /// Steps taken, which the step histogram cannot report.
    steps: u32,
    /// Hold the source back for this many steps before pushing it.
    delay: u32,
    source_out: i32,
    result_in: i32,
    exit_in: i32,
    source: [u8; SOURCE_BYTES],
    source_length: usize,
    source_written: usize,
    expect: [u8; RESULT_BYTES],
    expect_length: usize,
    result: [u8; RESULT_BYTES],
    result_length: usize,
    result_overflowed: bool,
    exit: [u8; 4],
    exit_filled: usize,
    exit_ready: bool,
    shape: u8,
    phase: u8,
    image_in: i32,
    record_out: i32,
    specifier: [u8; SPECIFIER_BYTES],
    specifier_length: usize,
    /// The image staged from `image_in`, then the record built over it.
    image: [u8; RECORD_HEADER + SPECIFIER_BYTES + IMAGE_BYTES],
    image_length: usize,
    image_overflowed: bool,
    record_length: usize,
    record_written: usize,
    /// Whether the record has left and the port has hung up.
    record_done: bool,
}

define_params! {
    State;

    1, source, str, 0
        => |s, d, len| {
            let taken = if len > SOURCE_BYTES { SOURCE_BYTES } else { len };
            s.source_length = taken;
            if taken > 0 {
                // SAFETY: the params reader hands a pointer valid for `len`
                // bytes, and `taken` is no larger than it or the field.
                unsafe {
                    core::ptr::copy_nonoverlapping(d, s.source.as_mut_ptr(), taken);
                }
            }
        };
    2, expect, str, 0
        => |s, d, len| {
            let taken = if len > RESULT_BYTES { RESULT_BYTES } else { len };
            s.expect_length = taken;
            if taken > 0 {
                // SAFETY: as for `source`.
                unsafe {
                    core::ptr::copy_nonoverlapping(d, s.expect.as_mut_ptr(), taken);
                }
            }
        };
    3, shape, u8, 0, enum { exact=0, digits=1 }
        => |s, d, len| { s.shape = p_u8(d, len, 0, 0); };
    4, specifier, str, 0
        => |s, d, len| {
            let taken = if len > SPECIFIER_BYTES { SPECIFIER_BYTES } else { len };
            s.specifier_length = taken;
            if taken > 0 {
                // SAFETY: as for `source`.
                unsafe {
                    core::ptr::copy_nonoverlapping(d, s.specifier.as_mut_ptr(), taken);
                }
            }
        };
    5, trace, u32, 0
        => |s, d, len| { s.trace = p_u32(d, len, 0, 0); };
    6, delay, u32, 0
        => |s, d, len| { s.delay = p_u32(d, len, 0, 0); };
}

/// Say where this module got to, once per step, when `trace` is set.
///
/// The counter is in the line because the histogram cannot supply it: the
/// kernel records a step's time only in the `Continue` arm, so a step that
/// returned `Ready`, `Done` or an error never reaches `MON_HIST` and a module
/// that stops appearing there cannot be told from one that stopped returning
/// `Continue`. This line is emitted from every step regardless of what the
/// step returns, so the two are distinguishable.
fn say(state: &mut State, syscalls: &SyscallTable) {
    // Counted whether or not it is reported: `delay` is measured in steps.
    state.steps = state.steps.saturating_add(1);
    if state.trace == 0 || !state.steps.is_multiple_of(state.trace) {
        return;
    }
    let mut line = trace::Line::<80>::new();
    line.text(b"[exp] step=").number(state.steps);
    line.text(b" phase=").number(u32::from(state.phase));
    line.text(b" src=")
        .number(u32::try_from(state.source_written).unwrap_or(u32::MAX));
    line.text(b" res=")
        .number(u32::try_from(state.result_length).unwrap_or(u32::MAX));
    line.text(b" exit=").number(u32::from(state.exit_ready));
    trace::write(syscalls, line.bytes());
}

/// Carry the image from `image_in` to `record_out` as one linker record.
///
/// The image is staged whole, because the record states its length in front
/// of it; then the header and specifier are written before it in place, the
/// record leaves, and the port hangs up. Answers `false` while any of that is
/// still to happen, and `true` once the record has gone or when the graph
/// wired no such leg.
fn carry_image(state: &mut State, syscalls: &SyscallTable) -> bool {
    if state.image_in < 0 || state.record_out < 0 || state.record_done {
        return true;
    }
    if state.record_length == 0 {
        let lead = RECORD_HEADER + SPECIFIER_BYTES;
        let Some(room) = state.image.get_mut(lead..) else {
            return false;
        };
        let staged = wire::stage_stream(
            syscalls,
            state.image_in,
            room,
            &mut state.image_length,
            &mut state.image_overflowed,
        );
        if staged != wire::Staged::Complete || state.image_overflowed {
            return false;
        }
        // The record is laid out directly in front of the image: the header,
        // the specifier, then the bytes that are already there.
        let specifier_length = state.specifier_length;
        let image_length = state.image_length;
        let start = lead - RECORD_HEADER - specifier_length;
        let mut header = [0u8; RECORD_HEADER];
        header[..4].copy_from_slice(&(specifier_length as u32).to_le_bytes());
        header[4..].copy_from_slice(&(image_length as u32).to_le_bytes());
        let specifier = state.specifier;
        if !copy_into(
            state
                .image
                .get_mut(start..start + RECORD_HEADER)
                .unwrap_or(&mut []),
            &header,
        ) || !copy_into(
            state
                .image
                .get_mut(start + RECORD_HEADER..start + RECORD_HEADER + specifier_length)
                .unwrap_or(&mut []),
            specifier.get(..specifier_length).unwrap_or(&[]),
        ) {
            return false;
        }
        state.record_written = start;
        state.record_length = lead + image_length;
    }
    let record = state.image.get(..state.record_length).unwrap_or(&[]);
    if !wire::push_progress(
        syscalls,
        state.record_out,
        record,
        &mut state.record_written,
    ) {
        return false;
    }
    // SAFETY: the port is this module's own, and the command takes no
    // argument.
    let hung_up = unsafe {
        dev_channel_ioctl(
            syscalls,
            state.record_out,
            IOCTL_EOF,
            core::ptr::null_mut(),
            0,
        )
    };
    state.record_done = hung_up >= 0;
    state.record_done
}

/// Take the graph's parameters, or the defaults where it gave none.
///
/// # Safety
/// `params` must be valid for reads of `params_len` bytes, or null.
unsafe fn apply_params(state: &mut State, params: *const u8, params_len: usize) {
    if wire::params_are_tlv(params, params_len, TLV_MAGIC, TLV_VERSION) {
        parse_tlv(state, params, params_len);
    } else {
        set_defaults(state);
    }
}

/// Whether the result is the answer the graph named.
///
/// The isolate ends a rendered value with one newline, as a line on standard
/// output would end. The answer a graph states is the value, so the newline
/// is taken off before the two are compared, once, as a shell's `$(...)`
/// takes it off for the Linux lanes.
fn accepted(state: &State) -> bool {
    if state.result_overflowed {
        return false;
    }
    let mut result = state.result.get(..state.result_length).unwrap_or(&[]);
    if let Some((&b'\n', rest)) = result.split_last() {
        result = rest;
    }
    match state.shape {
        SHAPE_DIGITS => !result.is_empty() && result.iter().all(u8::is_ascii_digit),
        _ => result == state.expect.get(..state.expect_length).unwrap_or(&[]),
    }
}

entry! {
    State;
    primary { source_out }
    inputs { result_in = 0, exit_in = 1, image_in = 2 }
    outputs { record_out = 1 }
    params apply_params
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
    if state.syscalls.is_null() || state.source_out < 0 || state.result_in < 0 {
        return -2;
    }
    // SAFETY: the table pointer was stored by `module_new` and checked
    // non-null above; the loader keeps it live for the module's lifetime.
    let syscalls = unsafe { &*state.syscalls };
    say(state, syscalls);
    announce_ready!(state);

    // A fixture that must be observed cannot start before the observer
    // exists. On a board whose serial console is dead, the only log channel
    // is UDP and it does not exist until the IP stack has a DHCP lease --
    // about twenty seconds in. This chain compiles and runs `1+2` in under
    // twenty STEPS, so on bare metal it was finishing inside that window and
    // every line that said so, the scheduler's own `done` included, was
    // written to a channel that was not there yet. Nothing was wrong with
    // the engine; the evidence was being posted to an address that did not
    // exist. `delay` holds the source back until the wire is up.
    //
    // It is counted in steps rather than milliseconds because a step is what
    // this module gets and what it can count; the graph converts, knowing its
    // own tick.
    if state.steps <= state.delay {
        return 0;
    }

    if state.phase == 2 {
        return 1;
    }

    if state.phase == 0 {
        let source = state.source.get(..state.source_length).unwrap_or(&[]);
        if !wire::push_progress(
            syscalls,
            state.source_out,
            source,
            &mut state.source_written,
        ) {
            return 0;
        }
        // The stream is whole: the hang-up is what tells the compiler so, and
        // this module stays to read what comes back.
        // SAFETY: the port is this module's own, and the command takes no
        // argument.
        let hung_up = unsafe {
            dev_channel_ioctl(
                syscalls,
                state.source_out,
                IOCTL_EOF,
                core::ptr::null_mut(),
                0,
            )
        };
        if hung_up < 0 {
            return -3;
        }
        state.phase = 1;
        return 0;
    }

    // A module's image goes round through the linker before anything can
    // come back.
    if !carry_image(state, syscalls) {
        return 0;
    }

    // The result is one stream; the exit status is one little-endian word.
    // Both are whole when the isolate has finished, which is the hang-up.
    let staged = wire::stage_stream(
        syscalls,
        state.result_in,
        &mut state.result,
        &mut state.result_length,
        &mut state.result_overflowed,
    );
    if !state.exit_ready
        && wire::take_frame(
            syscalls,
            state.exit_in,
            &mut state.exit,
            &mut state.exit_filled,
        )
    {
        state.exit_ready = true;
    }
    if staged != wire::Staged::Complete {
        return 0;
    }
    state.phase = 2;
    let exit = i32::from_le_bytes(state.exit);
    if state.exit_ready && exit == 0 && accepted(state) {
        1
    } else {
        -3
    }
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
