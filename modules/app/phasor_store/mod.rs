//! A bounded object store, as a capability rather than an ambient filesystem.
//!
//! An isolate has no storage. A deployment that wants a program to keep bytes
//! wires this adapter behind the router, and the program reaches it only
//! through the binding it was granted. What it gets is a namespace of its
//! own: keys and values it put there, and nothing else. There are no paths,
//! no directories, and no way to name anything outside the store, because
//! there is nothing outside it.
//!
//! It is the first adapter whose answers are bytes rather than a number, and
//! the first that issues handles. A `write` carries its payload behind the
//! call frame; a `read` answers with the payload behind the completion. An
//! `open` answers with a handle: an index and a generation the binding table
//! checks, so a handle kept past its entry's life names nothing.

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

/// Entries the store may hold at once.
const ENTRY_COUNT: usize = 32;
/// Bytes one key may take.
const KEY_BYTES: usize = 128;
/// Bytes one value may take.
const VALUE_BYTES: usize = 4096;
/// Bytes of payload one call or completion may carry.
const PAYLOAD_BYTES: usize = VALUE_BYTES + KEY_BYTES + 2;
/// Completions staged for the reply port, each with its payload behind it.
const STAGE_BYTES: usize = 4 * (COMPLETION_FRAME + PAYLOAD_BYTES);

/// Which operation a call asks for. The method is the binding the deployment
/// granted, so the adapter learns it from the binding index the call names
/// rather than from anything in the payload.
const METHOD_READ: u32 = 0;
const METHOD_WRITE: u32 = 1;
const METHOD_LIST: u32 = 2;
const METHOD_DELETE: u32 = 3;
/// Answers with a resource: the entry the key names, held open.
const METHOD_OPEN: u32 = 4;
/// Reads through a resource rather than by key.
const METHOD_READ_AT: u32 = 5;

/// One entry: a key, its value, and whether the slot is in use.
#[derive(Clone, Copy)]
struct Entry {
    key: [u8; KEY_BYTES],
    key_length: usize,
    value: [u8; VALUE_BYTES],
    value_length: usize,
    live: bool,
}

impl Entry {
    const EMPTY: Self = Self {
        key: [0; KEY_BYTES],
        key_length: 0,
        value: [0; VALUE_BYTES],
        value_length: 0,
        live: false,
    };
}

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    request_in: i32,
    reply_out: i32,
    /// The call frame being taken, and its payload behind it.
    request: [u8; CALL_FRAME],
    filled: usize,
    payload: [u8; PAYLOAD_BYTES],
    payload_filled: usize,
    /// Whether the frame is whole and the payload is what remains.
    frame_ready: bool,
    payload_length: usize,
    /// Completions staged for the port.
    replies: [u8; STAGE_BYTES],
    staged: usize,
    written: usize,
    answered: u64,
    /// Calls this adapter will answer before it refuses. Zero admits every
    /// call; a deployment that wants a bounded number of them says so here.
    quota: u32,
    /// Bytes the whole store may hold, which is what bounds a program's
    /// appetite rather than the value limit alone.
    capacity: u32,
    entries: [Entry; ENTRY_COUNT],
    phase: u8,
}

define_params! {
    State;

    1, quota, u32, 0
        => |s, d, len| { s.quota = p_u32(d, len, 0, 0); };
    2, capacity, u32, 65_536
        => |s, d, len| { s.capacity = p_u32(d, len, 0, 65_536); };
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

/// Bytes the store holds now, which a write is checked against.
fn used(state: &State) -> usize {
    let mut total = 0usize;
    for entry in &state.entries {
        if entry.live {
            total += entry.key_length + entry.value_length;
        }
    }
    total
}

/// The entry a key names, if the store holds one.
fn find(state: &State, key: &[u8]) -> Option<usize> {
    for (index, entry) in state.entries.iter().enumerate() {
        if entry.live && entry.key.get(..entry.key_length) == Some(key) {
            return Some(index);
        }
    }
    None
}

/// A payload's first field, up to the separator: the key every method names.
fn key_of(payload: &[u8]) -> (&[u8], &[u8]) {
    match payload.iter().position(|&byte| byte == 0) {
        Some(at) => (
            payload.get(..at).unwrap_or(&[]),
            payload.get(at + 1..).unwrap_or(&[]),
        ),
        None => (payload, &[]),
    }
}

/// A refusal, with the cause that says why.
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

/// Answer one call, staging any bytes it answers with into `out`. Answers the
/// completion record and how many bytes of `out` it filled.
fn answer(state: &mut State, record: &CallRecord, out: &mut [u8]) -> (CompletionRecord, usize) {
    if state.quota != 0 && state.answered >= u64::from(state.quota) {
        return refuse(record, Cause::Denied);
    }
    let payload = state
        .payload
        .get(..state.payload_length)
        .unwrap_or(&[])
        .to_owned_bounded();
    let (key, value) = key_of(payload.as_slice());
    // A read through a resource names no key: the handle the engine resolved
    // is the provider's own identifier for the entry, and arrives as digits.
    if record.binding == METHOD_READ_AT {
        let mut token = 0usize;
        for &byte in key {
            if !byte.is_ascii_digit() {
                return refuse(record, Cause::Malformed);
            }
            token = token * 10 + usize::from(byte - b'0');
        }
        let Some(entry) = state.entries.get(token).filter(|entry| entry.live) else {
            return refuse(record, Cause::Malformed);
        };
        let length = entry.value_length;
        let copied = copy_into(
            out.get_mut(..length).unwrap_or(&mut []),
            entry.value.get(..length).unwrap_or(&[]),
        );
        if !copied {
            return refuse(record, Cause::Internal);
        }
        return (
            CompletionRecord {
                request: record.request,
                disposition: Disposition::Fulfilled,
                cause: Cause::None,
                trace: record.trace,
                answer: Answer::Payload(u32::try_from(length).unwrap_or(0)),
            },
            length,
        );
    }
    if key.is_empty() || key.len() > KEY_BYTES {
        return refuse(record, Cause::Malformed);
    }
    match record.binding {
        METHOD_OPEN => match find(state, key) {
            // The entry's own index is what the provider calls the resource.
            // What the program gets back is a handle over it, checked by the
            // binding that issued it.
            Some(index) => (
                CompletionRecord {
                    request: record.request,
                    disposition: Disposition::Fulfilled,
                    cause: Cause::None,
                    trace: record.trace,
                    answer: Answer::Resource(index as u64),
                },
                0,
            ),
            None => refuse(record, Cause::Malformed),
        },
        METHOD_READ => match find(state, key) {
            Some(index) => {
                let entry = &state.entries[index];
                let length = entry.value_length;
                let copied = copy_into(
                    out.get_mut(..length).unwrap_or(&mut []),
                    entry.value.get(..length).unwrap_or(&[]),
                );
                match copied {
                    true => (
                        CompletionRecord {
                            request: record.request,
                            disposition: Disposition::Fulfilled,
                            cause: Cause::None,
                            trace: record.trace,
                            answer: Answer::Payload(u32::try_from(length).unwrap_or(0)),
                        },
                        length,
                    ),
                    false => refuse(record, Cause::Internal),
                }
            }
            // A key the store does not hold is not a failure of the provider:
            // the call was answered, and the answer is that there is nothing.
            None => (
                CompletionRecord {
                    request: record.request,
                    disposition: Disposition::Fulfilled,
                    cause: Cause::None,
                    trace: record.trace,
                    answer: Answer::None,
                },
                0,
            ),
        },
        METHOD_WRITE => {
            if value.len() > VALUE_BYTES {
                return refuse(record, Cause::Malformed);
            }
            let existing = find(state, key);
            let freed = existing.map_or(0, |index| {
                state.entries[index].key_length + state.entries[index].value_length
            });
            let after = used(state) - freed + key.len() + value.len();
            if after > state.capacity as usize {
                return refuse(record, Cause::Denied);
            }
            let index = match existing {
                Some(index) => index,
                None => match state.entries.iter().position(|entry| !entry.live) {
                    Some(index) => index,
                    None => return refuse(record, Cause::Denied),
                },
            };
            let entry = &mut state.entries[index];
            *entry = Entry::EMPTY;
            copy_into(entry.key.get_mut(..key.len()).unwrap_or(&mut []), key);
            entry.key_length = key.len();
            copy_into(entry.value.get_mut(..value.len()).unwrap_or(&mut []), value);
            entry.value_length = value.len();
            entry.live = true;
            (
                CompletionRecord {
                    request: record.request,
                    disposition: Disposition::Fulfilled,
                    cause: Cause::None,
                    trace: record.trace,
                    answer: Answer::Number(value.len() as f64),
                },
                0,
            )
        }
        METHOD_DELETE => {
            let found = find(state, key);
            if let Some(index) = found {
                state.entries[index] = Entry::EMPTY;
            }
            (
                CompletionRecord {
                    request: record.request,
                    disposition: Disposition::Fulfilled,
                    cause: Cause::None,
                    trace: record.trace,
                    answer: Answer::Number(f64::from(u8::from(found.is_some()))),
                },
                0,
            )
        }
        METHOD_LIST => {
            // Every key the store holds whose text begins with the one asked
            // for, separated by NUL, as one payload.
            let mut length = 0usize;
            for entry in &state.entries {
                if !entry.live {
                    continue;
                }
                let held = entry.key.get(..entry.key_length).unwrap_or(&[]);
                if key != b"*" && !held.starts_with(key) {
                    continue;
                }
                if length > 0 {
                    match out.get_mut(length) {
                        Some(slot) => *slot = 0,
                        None => break,
                    }
                    length += 1;
                }
                if !copy_into(
                    out.get_mut(length..length + held.len()).unwrap_or(&mut []),
                    held,
                ) {
                    break;
                }
                length += held.len();
            }
            (
                CompletionRecord {
                    request: record.request,
                    disposition: Disposition::Fulfilled,
                    cause: Cause::None,
                    trace: record.trace,
                    answer: Answer::Payload(u32::try_from(length).unwrap_or(0)),
                },
                length,
            )
        }
        _ => refuse(record, Cause::Malformed),
    }
}

/// A bounded copy of the staged payload, so the answer may borrow the store
/// mutably while it reads what the call carried.
struct Payload {
    bytes: [u8; PAYLOAD_BYTES],
    length: usize,
}

impl Payload {
    fn as_slice(&self) -> &[u8] {
        self.bytes.get(..self.length).unwrap_or(&[])
    }
}

trait BoundedCopy {
    fn to_owned_bounded(&self) -> Payload;
}

impl BoundedCopy for [u8] {
    fn to_owned_bounded(&self) -> Payload {
        let mut bytes = [0u8; PAYLOAD_BYTES];
        let length = self.len().min(PAYLOAD_BYTES);
        copy_into(
            bytes.get_mut(..length).unwrap_or(&mut []),
            self.get(..length).unwrap_or(&[]),
        );
        Payload { bytes, length }
    }
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

    // One request is taken only when there is room for its answer and the
    // bytes it may answer with.
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
        // The payload follows its frame: until every byte is here, the call
        // has not arrived.
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
                let (reply, length) = answer(state, &record, &mut bytes);
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
