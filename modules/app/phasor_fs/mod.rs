//! Files, as a capability rather than an ambient filesystem.
//!
//! An isolate has no filesystem. A deployment that wants a program to read or
//! write files wires this adapter behind the router and gives it a root; the
//! program reaches it only through the binding it was granted, and only under
//! that root. There are no absolute paths and no way up: a name that escapes
//! the root is refused before anything is opened, so the root is the whole of
//! what the capability grants.
//!
//! The adapter holds the real descriptors and the real paths. What crosses
//! the boundary is a name relative to the root, the bytes themselves, and
//! handles the engine checks — never a descriptor, never a path the provider
//! resolved, and never anything about the filesystem around the root.

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
include!("../../../target/fluxor/fluxor-abi/sdk/runtime/params.rs");

#[macro_use]
#[path = "../../common/entry.rs"]
mod entry;

#[path = "../../common/binding.rs"]
mod binding;
#[path = "../../common/digest.rs"]
mod digest;
#[path = "../../common/numeric.rs"]
mod numeric;
#[path = "../../common/softfloat.rs"]
mod softfloat;
#[path = "../../common/value.rs"]
mod value;
#[path = "../../common/wire.rs"]
mod wire;

use binding::{
    Answer, CallRecord, Cause, CompletionRecord, Disposition, CALL_FRAME, COMPLETION_FRAME,
};

/// Bytes one name may take, root included. The provider's own ceiling is
/// smaller than this, and it refuses anything past it.
const PATH_BYTES: usize = 224;
/// Bytes one call or completion may carry.
const PAYLOAD_BYTES: usize = 16 * 1024;
/// Completions staged for the reply port, each with its payload behind it.
const STAGE_BYTES: usize = 2 * (COMPLETION_FRAME + PAYLOAD_BYTES);
/// Files this adapter may hold open at once.
const OPEN_FILES: usize = 8;

/// The operations the interface offers, as the method numbers the engine's
/// bindings carry. What a call may do is decided by which binding the
/// deployment granted, not by anything in the payload.
const METHOD_READ: u32 = 0;
const METHOD_WRITE: u32 = 1;
const METHOD_LIST: u32 = 2;
const METHOD_DELETE: u32 = 3;
const METHOD_OPEN: u32 = 4;
const METHOD_READ_AT: u32 = 5;
const METHOD_CLOSE: u32 = 6;
const METHOD_SIZE: u32 = 7;

/// One file this adapter holds open on a program's behalf.
#[derive(Clone, Copy)]
struct Open {
    /// The provider's own descriptor, which never crosses the boundary.
    descriptor: i32,
    /// Where the next read continues from.
    offset: u64,
    live: bool,
}

impl Open {
    const EMPTY: Self = Self {
        descriptor: -1,
        offset: 0,
        live: false,
    };
}

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    request_in: i32,
    reply_out: i32,
    request: [u8; CALL_FRAME],
    filled: usize,
    payload: [u8; PAYLOAD_BYTES],
    payload_filled: usize,
    frame_ready: bool,
    payload_length: usize,
    replies: [u8; STAGE_BYTES],
    staged: usize,
    written: usize,
    answered: u64,
    /// Calls this adapter will answer before it refuses.
    quota: u32,
    /// Bytes one read may answer with.
    chunk: u32,
    /// Whether the deployment admits writing at all. A read-only grant is
    /// the common one, and it is enforced here rather than trusted.
    writable: u8,
    files: [Open; OPEN_FILES],
    phase: u8,
}

define_params! {
    State;

    1, quota, u32, 0
        => |s, d, len| { s.quota = p_u32(d, len, 0, 0); };
    2, chunk, u32, 8192
        => |s, d, len| { s.chunk = p_u32(d, len, 0, 8192); };
    3, writable, u8, 0, enum { no=0, yes=1 }
        => |s, d, len| { s.writable = p_u8(d, len, 0, 0); };
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

/// Whether a name is one this capability admits.
///
/// Nothing absolute, nothing that climbs, nothing hidden in a component, and
/// no NUL. The check is on the name as written rather than on what it
/// resolves to, because a name that could escape must be refused before
/// anything opens it.
fn admits(name: &[u8]) -> bool {
    if name.is_empty() || name.len() > PATH_BYTES / 2 {
        return false;
    }
    if name.first() == Some(&b'/') {
        return false;
    }
    let mut index = 0usize;
    let mut component_start = 0usize;
    while index <= name.len() {
        let at_end = index == name.len();
        let byte = if at_end { b'/' } else { name[index] };
        if byte == 0 {
            return false;
        }
        if byte == b'/' {
            let component = name.get(component_start..index).unwrap_or(&[]);
            if component == b".." || component.is_empty() && !at_end {
                return false;
            }
            component_start = index + 1;
        }
        index += 1;
    }
    true
}

/// The name as the bytes the provider is given.
///
/// The root is the directory the graph runs in, which a deployment chooses by
/// where it runs it. Nothing here can leave it: a name is relative, carries no
/// `..`, and is checked before anything opens it, so the working directory is
/// the whole of what the capability grants.
fn resolve(name: &[u8], out: &mut [u8; PATH_BYTES]) -> Option<usize> {
    if !admits(name) {
        return None;
    }
    if !copy_into(out.get_mut(..name.len())?, name) {
        return None;
    }
    // The provider takes a name and its length, so nothing here is
    // NUL-terminated; the length is the whole of it.
    Some(name.len())
}

/// A payload's fields: the name, and whatever follows it.
fn split(payload: &[u8]) -> (&[u8], &[u8]) {
    match payload.iter().position(|&byte| byte == 0) {
        Some(at) => (
            payload.get(..at).unwrap_or(&[]),
            payload.get(at + 1..).unwrap_or(&[]),
        ),
        None => (payload, &[]),
    }
}

/// Read digits, which is how a handle the engine resolved arrives.
fn digits(text: &[u8]) -> Option<usize> {
    if text.is_empty() {
        return None;
    }
    let mut value = 0usize;
    for &byte in text {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value
            .checked_mul(10)?
            .checked_add(usize::from(byte - b'0'))?;
    }
    Some(value)
}

fn refuse(record: &CallRecord, cause: Cause) -> (CompletionRecord, usize) {
    (
        CompletionRecord {
            request: record.request,
            disposition: Disposition::Rejected,
            cause,
            trace: record.trace,
            answer: Answer::None,
        },
        0,
    )
}

fn fulfilled(record: &CallRecord, answer: Answer, length: usize) -> (CompletionRecord, usize) {
    (
        CompletionRecord {
            request: record.request,
            disposition: Disposition::Fulfilled,
            cause: Cause::None,
            trace: record.trace,
            answer,
        },
        length,
    )
}

/// Open a name under the root, answering the provider's descriptor.
///
/// # Safety
/// The syscall table is the loader's, live for the module's lifetime.
unsafe fn provider_open(
    syscalls: &SyscallTable,
    path: &mut [u8; PATH_BYTES],
    length: usize,
    create: bool,
) -> i32 {
    let opcode = if create {
        abi::contracts::storage::fs::OPEN_CREATE
    } else {
        abi::contracts::storage::fs::OPEN
    };
    // SAFETY: the path buffer is live for `length` bytes and the table is the
    // loader's own; the provider reads the name and opens it or refuses.
    unsafe { (syscalls.provider_call)(-1, opcode, path.as_mut_ptr(), length) }
}

/// Answer one call, staging any bytes it answers with into `out`.
fn answer(
    state: &mut State,
    record: &CallRecord,
    out: &mut [u8; PAYLOAD_BYTES],
    syscalls: &SyscallTable,
) -> (CompletionRecord, usize) {
    if state.quota != 0 && state.answered >= u64::from(state.quota) {
        return refuse(record, Cause::Denied);
    }
    let mut held = [0u8; PAYLOAD_BYTES];
    let length = state.payload_length.min(PAYLOAD_BYTES);
    if !copy_into(
        held.get_mut(..length).unwrap_or(&mut []),
        state.payload.get(..length).unwrap_or(&[]),
    ) {
        return refuse(record, Cause::Internal);
    }
    let (name, rest) = split(held.get(..length).unwrap_or(&[]));
    let writes = matches!(record.binding, METHOD_WRITE | METHOD_DELETE);
    if writes && state.writable == 0 {
        return refuse(record, Cause::Denied);
    }

    match record.binding {
        METHOD_OPEN | METHOD_READ | METHOD_SIZE | METHOD_WRITE => {
            let mut path = [0u8; PATH_BYTES];
            let Some(path_length) = resolve(name, &mut path) else {
                return refuse(record, Cause::Denied);
            };
            let create = record.binding == METHOD_WRITE;
            // SAFETY: the path is live for its length and the table is the
            // loader's.
            let descriptor = unsafe { provider_open(syscalls, &mut path, path_length, create) };
            if descriptor < 0 {
                return refuse(record, Cause::Unavailable);
            }
            let outcome = match record.binding {
                METHOD_OPEN => {
                    let Some(slot) = state.files.iter().position(|file| !file.live) else {
                        return close_and_refuse(syscalls, descriptor, record, Cause::Busy);
                    };
                    state.files[slot] = Open {
                        descriptor,
                        offset: 0,
                        live: true,
                    };
                    // The slot is what the provider calls the resource; what
                    // the program gets is a handle over it.
                    return fulfilled(record, Answer::Resource(slot as u64), 0);
                }
                METHOD_READ => read_all(syscalls, descriptor, state.chunk as usize, out),
                METHOD_SIZE => size_of(syscalls, descriptor)
                    .map(Answer::Number)
                    .map(|a| (a, 0)),
                _ => write_all(syscalls, descriptor, rest),
            };
            close(syscalls, descriptor);
            match outcome {
                Some((answer, length)) => fulfilled(record, answer, length),
                None => refuse(record, Cause::Internal),
            }
        }
        METHOD_READ_AT => {
            let Some(slot) = digits(name).filter(|slot| *slot < OPEN_FILES) else {
                return refuse(record, Cause::Malformed);
            };
            let file = state.files[slot];
            if !file.live {
                return refuse(record, Cause::Malformed);
            }
            let wanted = digits(rest).unwrap_or(state.chunk as usize);
            match read_chunk(
                syscalls,
                file.descriptor,
                wanted.min(state.chunk as usize),
                out,
            ) {
                Some(read) => {
                    state.files[slot].offset = file.offset.saturating_add(read as u64);
                    fulfilled(record, Answer::Payload(read as u32), read)
                }
                None => refuse(record, Cause::Internal),
            }
        }
        METHOD_CLOSE => {
            let Some(slot) = digits(name).filter(|slot| *slot < OPEN_FILES) else {
                return refuse(record, Cause::Malformed);
            };
            if !state.files[slot].live {
                return refuse(record, Cause::Malformed);
            }
            close(syscalls, state.files[slot].descriptor);
            state.files[slot] = Open::EMPTY;
            fulfilled(record, Answer::Number(1.0), 0)
        }
        _ => refuse(record, Cause::Malformed),
    }
}

fn close_and_refuse(
    syscalls: &SyscallTable,
    descriptor: i32,
    record: &CallRecord,
    cause: Cause,
) -> (CompletionRecord, usize) {
    close(syscalls, descriptor);
    refuse(record, cause)
}

fn close(syscalls: &SyscallTable, descriptor: i32) {
    let mut nothing = [0u8; 1];
    // SAFETY: the descriptor is one the provider issued, and the table is the
    // loader's own.
    unsafe {
        (syscalls.provider_call)(
            descriptor,
            abi::contracts::storage::fs::CLOSE,
            nothing.as_mut_ptr(),
            0,
        );
    }
}

/// Read up to `wanted` bytes into `out`.
fn read_chunk(
    syscalls: &SyscallTable,
    descriptor: i32,
    wanted: usize,
    out: &mut [u8; PAYLOAD_BYTES],
) -> Option<usize> {
    let capacity = wanted.min(PAYLOAD_BYTES);
    if capacity == 0 {
        return Some(0);
    }
    // SAFETY: `out` is writable for `capacity` bytes and the descriptor is
    // one the provider issued.
    let read = unsafe {
        (syscalls.provider_call)(
            descriptor,
            abi::contracts::storage::fs::READ,
            out.as_mut_ptr(),
            capacity,
        )
    };
    if read < 0 {
        return None;
    }
    usize::try_from(read).ok()
}

/// Read a whole file, up to the chunk the deployment admits.
fn read_all(
    syscalls: &SyscallTable,
    descriptor: i32,
    chunk: usize,
    out: &mut [u8; PAYLOAD_BYTES],
) -> Option<(Answer, usize)> {
    let read = read_chunk(syscalls, descriptor, chunk, out)?;
    Some((Answer::Payload(u32::try_from(read).ok()?), read))
}

fn write_all(syscalls: &SyscallTable, descriptor: i32, bytes: &[u8]) -> Option<(Answer, usize)> {
    if bytes.is_empty() {
        return Some((Answer::Number(0.0), 0));
    }
    let mut buffer = [0u8; PAYLOAD_BYTES];
    let length = bytes.len().min(PAYLOAD_BYTES);
    copy_into(
        buffer.get_mut(..length).unwrap_or(&mut []),
        bytes.get(..length).unwrap_or(&[]),
    );
    // SAFETY: the buffer is live for `length` bytes and the descriptor is one
    // the provider issued.
    let written = unsafe {
        (syscalls.provider_call)(
            descriptor,
            abi::contracts::storage::fs::WRITE,
            buffer.as_mut_ptr(),
            length,
        )
    };
    if written < 0 {
        return None;
    }
    // The bytes are volatile until a fence, which is what the contract says:
    // a program that needs them durable asks for it.
    let mut nothing = [0u8; 1];
    // SAFETY: as above; the fence takes no argument.
    unsafe {
        (syscalls.provider_call)(
            descriptor,
            abi::contracts::storage::fs::FSYNC,
            nothing.as_mut_ptr(),
            0,
        );
    }
    Some((Answer::Number(f64::from(written)), 0))
}

fn size_of(syscalls: &SyscallTable, descriptor: i32) -> Option<f64> {
    let mut stat = [0u8; 16];
    // SAFETY: the buffer is writable for its length and the descriptor is one
    // the provider issued; the contract selects the shape by width.
    let written = unsafe {
        (syscalls.provider_call)(
            descriptor,
            abi::contracts::storage::fs::STAT,
            stat.as_mut_ptr(),
            stat.len(),
        )
    };
    if written < 8 {
        return None;
    }
    let bytes = <[u8; 8]>::try_from(stat.get(..8)?).ok()?;
    Some(u64::from_le_bytes(bytes) as f64)
}

entry! {
    State;
    primary { request_in, reply_out }
    inputs {}
    outputs {}
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
    if state.syscalls.is_null() || state.request_in < 0 || state.reply_out < 0 {
        return -2;
    }
    // SAFETY: the table pointer was stored by `module_new` and checked
    // non-null above; the loader keeps it live for the module's lifetime.
    let syscalls = unsafe { &*state.syscalls };
    if state.phase == 1 {
        return 1;
    }

    if state.staged + COMPLETION_FRAME + PAYLOAD_BYTES <= state.replies.len() {
        if !state.frame_ready
            && wire::take_frame(
                syscalls,
                state.request_in,
                &mut state.request,
                &mut state.filled,
            )
        {
            state.filled = 0;
            state.frame_ready = true;
            state.payload_filled = 0;
            state.payload_length = CallRecord::decode(&state.request)
                .map_or(0, |record| record.payload_length as usize)
                .min(PAYLOAD_BYTES);
        }
        if state.frame_ready
            && wire::take_payload(
                syscalls,
                state.request_in,
                &mut state.payload,
                &mut state.payload_filled,
                state.payload_length,
            )
        {
            state.frame_ready = false;
            if let Some(record) = CallRecord::decode(&state.request) {
                let mut bytes = [0u8; PAYLOAD_BYTES];
                let (reply, length) = answer(state, &record, &mut bytes, syscalls);
                let at = state.staged;
                let frame = reply.encode();
                copy_into(
                    state
                        .replies
                        .get_mut(at..at + COMPLETION_FRAME)
                        .unwrap_or(&mut []),
                    &frame,
                );
                copy_into(
                    state
                        .replies
                        .get_mut(at + COMPLETION_FRAME..at + COMPLETION_FRAME + length)
                        .unwrap_or(&mut []),
                    bytes.get(..length).unwrap_or(&[]),
                );
                state.staged = at + COMPLETION_FRAME + length;
                state.answered = state.answered.saturating_add(1);
            }
            state.payload_filled = 0;
            state.payload_length = 0;
        }
    }

    wire::push_staged(
        syscalls,
        state.reply_out,
        &state.replies,
        &mut state.staged,
        &mut state.written,
    );

    if wire::hung_up(syscalls, state.request_in) && state.staged == 0 {
        state.phase = 1;
        return 1;
    }
    0
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
