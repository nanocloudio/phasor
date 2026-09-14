//! An object store, as a capability rather than an ambient filesystem.
//!
//! An isolate has no storage. A deployment that wants a program to keep bytes
//! wires this adapter behind the router, and the program reaches it only
//! through the binding it was granted. What it gets is a namespace of its
//! own: keys and values it put there, and nothing else. There are no paths,
//! no directories, and no way to name anything outside the store, because
//! there is nothing outside it.
//!
//! The bytes are not here. This adapter turns a granted call into one
//! operation on Fluxor's `storage.object` and `storage.namespace` contracts
//! and answers with what came back, so which provider holds them is the
//! graph's to wire and a program cannot tell one from another. A `write`
//! carries its payload behind the call frame; a `read` answers with the
//! payload behind the completion. An `open` answers with a handle: an index
//! and a generation the binding table checks, so a handle kept past its
//! entry's life names nothing.

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

/// Entries held open at once, as handles a program may read through.
const ENTRY_COUNT: usize = 32;
/// Bytes one key may take.
const KEY_BYTES: usize = 128;
/// Bytes one value may take. Not a limit this adapter imposes on the store —
/// the provider has its own — but the most one call or answer may carry
/// across the seam in a single piece.
const VALUE_BYTES: usize = 4096;
/// The encoded durability fence a mutating call answers with. Nothing here
/// reads it, but the contract requires somewhere to put it.
const FENCE_BYTES: usize = 64;
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

/// The provider's "not yet": the fetch this call needs has not landed. It is
/// not a failure and must not be answered as one -- the call waits and is
/// tried again, which is the whole difference between a store that streams
/// from somewhere and one that only ever reads local memory.
const EAGAIN: i32 = -11;

/// A read waiting on a provider that answered "not yet".
#[derive(Clone, Copy)]
struct Waiting {
    request: u64,
    trace: u64,
    /// The member that was held, so the retry asks the same question. A
    /// retry that ran whichever member it was written for would answer a
    /// `delete` with a listing.
    binding: u32,
    /// The entry to read through, for a `readAt`; `usize::MAX` for a `read`,
    /// which holds its own descriptor because it opened one for this call.
    slot: usize,
    /// The descriptor the held read owns, or `-1` for a call that owns none
    /// and is asked again from its payload.
    descriptor: i32,
    offset: u64,
    /// The digest the call carried, so the retry rebuilds the record it was
    /// asked as rather than one that merely resembles it.
    payload: crate::digest::Digest,
    live: bool,
}

impl Waiting {
    const EMPTY: Self = Self {
        request: 0,
        trace: 0,
        binding: 0,
        slot: usize::MAX,
        descriptor: -1,
        offset: 0,
        payload: crate::digest::Digest([0u8; 32]),
        live: false,
    };
}

/// One object held open: the descriptor the provider issued, and how far
/// through it a program has read.
///
/// The descriptor never crosses the boundary. What a program gets is this
/// slot's index, wrapped in a handle the binding checks, so a handle kept
/// past its entry's life names nothing.
#[derive(Clone, Copy)]
struct Entry {
    descriptor: i32,
    offset: u64,
    live: bool,
}

impl Entry {
    const EMPTY: Self = Self {
        descriptor: -1,
        offset: 0,
        live: false,
    };
}

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    announced: bool,
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
    /// The prefix every key this adapter touches sits under, so one store
    /// serves several programs without either seeing the other's keys. The
    /// graph names it; a program cannot, and cannot escape it.
    namespace: [u8; KEY_BYTES],
    namespace_length: usize,
    entries: [Entry; ENTRY_COUNT],
    /// Reads the provider has not answered yet. One at a time: a store that
    /// is streaming has a call in flight, and a second would race it for the
    /// same buffer.
    waiting: Waiting,
    /// Set when the call just taken was held rather than answered, so the
    /// step knows to stage nothing for it.
    deferred: bool,
    phase: u8,
}

define_params! {
    State;

    1, quota, u32, 0
        => |s, d, len| { s.quota = p_u32(d, len, 0, 0); };
    2, namespace, str, 0
        => |s, d, len| {
            let taken = if len > KEY_BYTES { KEY_BYTES } else { len };
            s.namespace_length = taken;
            if taken > 0 {
                // SAFETY: the params reader hands a pointer valid for `len`
                // bytes, and `taken` is no larger than it or the field.
                unsafe {
                    core::ptr::copy_nonoverlapping(d, s.namespace.as_mut_ptr(), taken);
                }
            }
        };
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

// ── the object surface ────────────────────────────────────────────────
//
// `phasor:store/keyvalue` is served by whatever the deployment wired behind
// Fluxor's `storage.object` and `storage.namespace` contracts: the local
// versioned store, the browser's OPFS, or anything else that conforms. This
// adapter holds no keys and no bytes of its own -- it translates one granted
// call into one provider op and answers with what came back. What a store IS
// is not this project's business, and a graph swaps one for another without
// a program noticing.

use abi::contracts::storage::namespace as ns;
use abi::contracts::storage::object as obj;

/// Write `value` little-endian at `at`, answering where the next field goes.
fn put_u16(buf: &mut [u8], at: usize, value: u16) -> usize {
    copy_into(
        buf.get_mut(at..at + 2).unwrap_or(&mut []),
        &value.to_le_bytes(),
    );
    at + 2
}

fn put_u32(buf: &mut [u8], at: usize, value: u32) -> usize {
    copy_into(
        buf.get_mut(at..at + 4).unwrap_or(&mut []),
        &value.to_le_bytes(),
    );
    at + 4
}

fn put_u64(buf: &mut [u8], at: usize, value: u64) -> usize {
    copy_into(
        buf.get_mut(at..at + 8).unwrap_or(&mut []),
        &value.to_le_bytes(),
    );
    at + 8
}

/// The key a program named, under the namespace the graph gave this adapter.
///
/// A program's keys are its own, and the prefix is what keeps them so: two
/// programs granted stores in the same graph reach the same provider and must
/// not reach each other's keys. The prefix is joined here and stripped from
/// anything listed, so a program never sees it and cannot write outside it.
fn qualify(state: &State, key: &[u8], out: &mut [u8]) -> Option<usize> {
    let prefix = state.namespace.get(..state.namespace_length)?;
    let total = prefix.len() + key.len();
    if total > out.len() {
        return None;
    }
    copy_into(out.get_mut(..prefix.len()).unwrap_or(&mut []), prefix);
    copy_into(out.get_mut(prefix.len()..total).unwrap_or(&mut []), key);
    Some(total)
}

/// Open the object a key names, answering the provider's descriptor.
fn object_open(state: &State, syscalls: &SyscallTable, key: &[u8]) -> i32 {
    let mut name = [0u8; KEY_BYTES * 2];
    let Some(length) = qualify(state, key, &mut name) else {
        return -1;
    };
    // SAFETY: `name` is live for `length` bytes and the table is the loader's
    // own; the provider reads the key and opens it or refuses.
    unsafe { (syscalls.provider_call)(-1, obj::GET, name.as_mut_ptr(), length) }
}

/// Read through an open descriptor from `offset`, answering how much came.
fn object_range(syscalls: &SyscallTable, descriptor: i32, offset: u64, out: &mut [u8]) -> i32 {
    let mut arg = [0u8; 20];
    let at = put_u64(&mut arg, 0, offset);
    let at = put_u32(&mut arg, at, u32::try_from(out.len()).unwrap_or(0));
    let _ = put_u64(&mut arg, at, out.as_mut_ptr() as u64);
    // SAFETY: `out` is writable for its own length, the descriptor is one the
    // provider issued, and `arg` is live for the call.
    unsafe { (syscalls.provider_call)(descriptor, obj::RANGE_GET, arg.as_mut_ptr(), 20) }
}

/// Release a descriptor the provider issued.
fn object_close(syscalls: &SyscallTable, descriptor: i32) {
    let mut nothing = [0u8; 1];
    // SAFETY: the descriptor is one the provider issued.
    unsafe {
        (syscalls.provider_call)(descriptor, obj::CLOSE, nothing.as_mut_ptr(), 0);
    }
}

/// Turn one page of `storage.namespace` entries into the NUL-separated key
/// list this capability answers with, dropping the namespace prefix.
///
/// An entry is `[name_len: u8][kind: u8][name…]`, and the page ends with a
/// `0xFF` cursor record. The cursor's own length field is a `u8` where the
/// request's was a `u16` -- the contract says so out loud, because a consumer
/// that guessed the other width would corrupt every page but the last.
fn list_into(page: &[u8], strip: usize, out: &mut [u8]) -> usize {
    let mut at = 0usize;
    let mut length = 0usize;
    while at < page.len() {
        let name_len = usize::from(page[at]);
        if page[at] == 0xFF {
            break;
        }
        let Some(name) = page.get(at + 2..at + 2 + name_len) else {
            break;
        };
        at += 2 + name_len;
        let Some(shown) = name.get(strip..) else {
            continue;
        };
        if shown.is_empty() {
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
            out.get_mut(length..length + shown.len()).unwrap_or(&mut []),
            shown,
        ) {
            break;
        }
        length += shown.len();
    }
    length
}

/// A payload's fields: the key every method names, and whatever follows it.
fn key_of<'a>(payload: &'a [u8], parts: &mut [&'a [u8]; 2]) -> (&'a [u8], &'a [u8]) {
    let taken = wire::fields(payload, parts);
    (
        if taken > 0 { parts[0] } else { &[] },
        if taken > 1 { parts[1] } else { &[] },
    )
}

/// Stage a completion the step itself produced, rather than one `answer`
/// handed back: a held read is answered later than the call that made it.
fn stage_completion(state: &mut State, record: CompletionRecord, bytes: &[u8]) {
    let at = state.staged;
    let frame = record.encode();
    if !copy_into(
        state
            .replies
            .get_mut(at..at + COMPLETION_FRAME)
            .unwrap_or(&mut []),
        &frame,
    ) {
        return;
    }
    if !copy_into(
        state
            .replies
            .get_mut(at + COMPLETION_FRAME..at + COMPLETION_FRAME + bytes.len())
            .unwrap_or(&mut []),
        bytes,
    ) {
        return;
    }
    state.staged = at + COMPLETION_FRAME + bytes.len();
    state.answered = state.answered.saturating_add(1);
}

fn refuse_into(state: &mut State, request: u64, trace: u64, cause: Cause) {
    stage_completion(
        state,
        CompletionRecord {
            request,
            disposition: Disposition::Rejected,
            cause,
            trace,
            answer: Answer::None,
        },
        &[],
    );
}

fn fulfil_into(state: &mut State, request: u64, trace: u64, answer: Answer, bytes: &[u8]) {
    stage_completion(
        state,
        CompletionRecord {
            request,
            disposition: Disposition::Fulfilled,
            cause: Cause::None,
            trace,
            answer,
        },
        bytes,
    );
}

/// Hold a call the provider said "not yet" to, for a member that owns no
/// descriptor: the retry asks the whole question again from the payload,
/// which stays staged because no new call is taken while one is held.
fn hold_from_payload(state: &mut State, record: &CallRecord) -> (CompletionRecord, usize) {
    state.waiting = Waiting {
        request: record.request,
        trace: record.trace,
        binding: record.binding,
        slot: usize::MAX,
        descriptor: -1,
        offset: 0,
        payload: record.payload,
        live: true,
    };
    held(state, record)
}

/// A call taken but not answered. Nothing is staged for it: the step answers
/// it once the provider has the bytes. The record is a placeholder the caller
/// discards, because `deferred` is what the step actually reads.
fn held(state: &mut State, record: &CallRecord) -> (CompletionRecord, usize) {
    state.deferred = true;
    (
        CompletionRecord {
            request: record.request,
            disposition: Disposition::Rejected,
            cause: Cause::None,
            trace: record.trace,
            answer: Answer::None,
        },
        0,
    )
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
fn answer(
    state: &mut State,
    record: &CallRecord,
    out: &mut [u8],
    syscalls: &SyscallTable,
) -> (CompletionRecord, usize) {
    if state.quota != 0 && state.answered >= u64::from(state.quota) {
        return refuse(record, Cause::Denied);
    }
    let payload = state
        .payload
        .get(..state.payload_length)
        .unwrap_or(&[])
        .to_owned_bounded();
    let mut parts: [&[u8]; 2] = [&[], &[]];
    let taken = wire::fields(payload.as_slice(), &mut parts);
    // How many fields each method is: the shape of a call, checked rather
    // than assumed. A call that does not have the fields its method takes is
    // refused here instead of being read as though it did.
    let shape = match record.binding {
        METHOD_LIST => (0, 0),
        METHOD_READ | METHOD_DELETE | METHOD_OPEN | METHOD_READ_AT => (1, 1),
        METHOD_WRITE => (2, 2),
        _ => (0, usize::MAX),
    };
    if taken < shape.0 || taken > shape.1 {
        return refuse(record, Cause::Malformed);
    }
    let (key, value) = key_of(payload.as_slice(), &mut parts);
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
        let Some(entry) = state.entries.get(token).copied().filter(|entry| entry.live) else {
            return refuse(record, Cause::Malformed);
        };
        let read = object_range(syscalls, entry.descriptor, entry.offset, out);
        if read == EAGAIN {
            // Not yet. Hold the call and try again next step, rather than
            // telling a program its store failed when it has not.
            state.waiting = Waiting {
                request: record.request,
                trace: record.trace,
                binding: record.binding,
                slot: token,
                descriptor: entry.descriptor,
                offset: entry.offset,
                payload: record.payload,
                live: true,
            };
            return held(state, record);
        }
        if read < 0 {
            return refuse(record, Cause::Unavailable);
        }
        let length = read as usize;
        // Where the next read continues from. A reader that has reached the
        // end is answered with nothing, which is how it learns there is no
        // more -- the same way a file's is.
        if let Some(slot) = state.entries.get_mut(token) {
            slot.offset = slot.offset.saturating_add(length as u64);
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
    // Every method but `list` names a key, and an empty one names nothing. A
    // list names a PREFIX, and the empty prefix is the whole of what this
    // program has -- which is the listing a caller with no argument asked for.
    let names_a_key = record.binding != METHOD_LIST;
    if (names_a_key && key.is_empty()) || key.len() > KEY_BYTES {
        return refuse(record, Cause::Malformed);
    }
    // A key is a flat name. The separator belongs to the graph, which used it
    // to say where this program's keys live, and a program that could write
    // one could write `..` either side of it -- which for a provider that
    // reads names hierarchically is a key outside the namespace. This
    // capability promises there is nothing outside it, so the character that
    // would express "outside" is not one a key may contain.
    #[allow(
        clippy::manual_contains,
        reason = "`contains` on a slice reaches for `memchr`, which a loaded module does not link; the walk is the same answer with no symbol behind it"
    )]
    if key.iter().any(|&byte| byte == b'/') {
        return refuse(record, Cause::Malformed);
    }
    match record.binding {
        METHOD_OPEN => {
            let Some(index) = state.entries.iter().position(|entry| !entry.live) else {
                return refuse(record, Cause::Busy);
            };
            let descriptor = object_open(state, syscalls, key);
            if descriptor < 0 {
                return refuse(record, Cause::Malformed);
            }
            state.entries[index] = Entry {
                descriptor,
                offset: 0,
                live: true,
            };
            (
                CompletionRecord {
                    request: record.request,
                    disposition: Disposition::Fulfilled,
                    cause: Cause::None,
                    trace: record.trace,
                    answer: Answer::Resource(index as u64),
                },
                0,
            )
        }
        METHOD_READ => {
            let descriptor = object_open(state, syscalls, key);
            if descriptor < 0 {
                // A key the store does not hold is not a failure of the
                // provider: the call was answered, and the answer is that
                // there is nothing.
                return (
                    CompletionRecord {
                        request: record.request,
                        disposition: Disposition::Fulfilled,
                        cause: Cause::None,
                        trace: record.trace,
                        answer: Answer::None,
                    },
                    0,
                );
            }
            let read = object_range(syscalls, descriptor, 0, out);
            if read == EAGAIN {
                // Held with its descriptor: closing it here would throw away
                // the fetch that is already in flight, and the retry would
                // start a new one.
                state.waiting = Waiting {
                    request: record.request,
                    trace: record.trace,
                    binding: record.binding,
                    slot: usize::MAX,
                    descriptor,
                    offset: 0,
                    payload: record.payload,
                    live: true,
                };
                return held(state, record);
            }
            object_close(syscalls, descriptor);
            if read < 0 {
                return refuse(record, Cause::Unavailable);
            }
            let length = read as usize;
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
        METHOD_WRITE => {
            if value.len() > VALUE_BYTES {
                return refuse(record, Cause::Malformed);
            }
            let mut name = [0u8; KEY_BYTES * 2];
            let Some(length) = qualify(state, key, &mut name) else {
                return refuse(record, Cause::Malformed);
            };
            let mut fence = [0u8; FENCE_BYTES];
            let mut body = [0u8; VALUE_BYTES];
            if !copy_into(body.get_mut(..value.len()).unwrap_or(&mut []), value) {
                return refuse(record, Cause::Internal);
            }
            let mut arg = [0u8; KEY_BYTES * 2 + 32];
            let at = put_u16(&mut arg, 0, u16::try_from(length).unwrap_or(0));
            if !copy_into(
                arg.get_mut(at..at + length).unwrap_or(&mut []),
                name.get(..length).unwrap_or(&[]),
            ) {
                return refuse(record, Cause::Internal);
            }
            let mut at = at + length;
            // No content type: bytes under a key are bytes, and a store that
            // wanted a MIME tag would be describing something this capability
            // does not promise.
            arg[at] = 0;
            at += 1;
            let at = put_u64(&mut arg, at, body.as_mut_ptr() as u64);
            let at = put_u64(&mut arg, at, value.len() as u64);
            arg[at] = obj::precondition::ANY;
            arg[at + 1] = 0;
            let at = put_u64(&mut arg, at + 2, fence.as_mut_ptr() as u64);
            let at = put_u16(&mut arg, at, FENCE_BYTES as u16);
            // SAFETY: every pointer in `arg` names a buffer live for this
            // call, and the table is the loader's own.
            let rc = unsafe { (syscalls.provider_call)(-1, obj::PUT, arg.as_mut_ptr(), at) };
            if rc == EAGAIN {
                // The store has not taken the bytes yet. That is not a store
                // that failed, and answering as though it were would tell a
                // program its write was lost while it is still in flight.
                return hold_from_payload(state, record);
            }
            if rc < 0 {
                // No store is wired, or it refused. Saying so is the whole of
                // what this adapter can honestly do: it holds nothing itself,
                // so there is no second place the bytes could have gone.
                return refuse(record, Cause::Unavailable);
            }
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
            let mut name = [0u8; KEY_BYTES * 2];
            let Some(length) = qualify(state, key, &mut name) else {
                return refuse(record, Cause::Malformed);
            };
            let mut fence = [0u8; FENCE_BYTES];
            let mut arg = [0u8; KEY_BYTES * 2 + 16];
            let at = put_u16(&mut arg, 0, u16::try_from(length).unwrap_or(0));
            if !copy_into(
                arg.get_mut(at..at + length).unwrap_or(&mut []),
                name.get(..length).unwrap_or(&[]),
            ) {
                return refuse(record, Cause::Internal);
            }
            let mut at = at + length;
            arg[at] = obj::precondition::ANY;
            arg[at + 1] = 0;
            at += 2;
            let at = put_u64(&mut arg, at, fence.as_mut_ptr() as u64);
            let at = put_u16(&mut arg, at, FENCE_BYTES as u16);
            // SAFETY: as for the write above.
            let rc = unsafe { (syscalls.provider_call)(-1, obj::DELETE, arg.as_mut_ptr(), at) };
            // Worse here than anywhere else: this member answers with whether
            // the key went, so reading "not yet" as a number would tell a
            // program the key is still there when it is merely still going.
            if rc == EAGAIN {
                return hold_from_payload(state, record);
            }
            (
                CompletionRecord {
                    request: record.request,
                    disposition: Disposition::Fulfilled,
                    cause: Cause::None,
                    trace: record.trace,
                    answer: Answer::Number(f64::from(u8::from(rc >= 0))),
                },
                0,
            )
        }
        METHOD_LIST => {
            // Every key under this adapter's namespace whose text begins with
            // the one asked for, separated by NUL, as one payload. The
            // namespace prefix is stripped: a program never sees where its
            // keys actually live.
            let asked = if key == b"*" { &[][..] } else { key };
            let mut prefix = [0u8; KEY_BYTES * 2];
            let Some(plen) = qualify(state, asked, &mut prefix) else {
                return refuse(record, Cause::Malformed);
            };
            let mut page = [0u8; VALUE_BYTES];
            let mut fence = [0u8; FENCE_BYTES];
            let mut arg = [0u8; KEY_BYTES * 2 + 32];
            let at = put_u16(&mut arg, 0, u16::try_from(plen).unwrap_or(0));
            if !copy_into(
                arg.get_mut(at..at + plen).unwrap_or(&mut []),
                prefix.get(..plen).unwrap_or(&[]),
            ) {
                return refuse(record, Cause::Internal);
            }
            let at = at + plen;
            // First page only. A listing longer than one answer is a cursor
            // this capability does not yet carry, and stopping at a page
            // boundary is visible; guessing past it would not be.
            let at = put_u16(&mut arg, at, 0);
            let at = put_u64(&mut arg, at, page.as_mut_ptr() as u64);
            let at = put_u32(&mut arg, at, VALUE_BYTES as u32);
            let at = put_u64(&mut arg, at, fence.as_mut_ptr() as u64);
            let at = put_u16(&mut arg, at, FENCE_BYTES as u16);
            // SAFETY: as for the write above.
            let written = unsafe { (syscalls.provider_call)(-1, ns::LIST, arg.as_mut_ptr(), at) };
            if written == EAGAIN {
                return hold_from_payload(state, record);
            }
            if written < 0 {
                return refuse(record, Cause::Unavailable);
            }
            let length = list_into(
                page.get(..written as usize).unwrap_or(&[]),
                state.namespace_length,
                out,
            );
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
    announce_ready!(state);

    // A read the provider had not got the bytes for. Tried again before any
    // new call, because the program is waiting on this one and the buffer it
    // reads into is the module's only one.
    if state.waiting.live {
        let mut bytes = [0u8; PAYLOAD_BYTES];
        let held = state.waiting;
        // A held call that owns no descriptor is asked again whole: the same
        // record, over the same payload, through the same answer. "Ask again"
        // is not a different question, so it is not a second code path.
        if held.descriptor < 0 {
            let record = CallRecord {
                request: held.request,
                binding: held.binding,
                payload_length: u32::try_from(state.payload_length).unwrap_or(0),
                trace: held.trace,
                payload: held.payload,
            };
            state.deferred = false;
            let (reply, length) = answer(state, &record, &mut bytes, syscalls);
            if !state.deferred {
                state.waiting = Waiting::EMPTY;
                state.payload_length = 0;
                stage_completion(state, reply, bytes.get(..length).unwrap_or(&[]));
            }
        } else {
            let read = object_range(syscalls, held.descriptor, held.offset, &mut bytes);
            if read != EAGAIN {
                state.waiting = Waiting::EMPTY;
                if held.slot == usize::MAX {
                    // A `read` opened this descriptor for itself, so it closes it.
                    object_close(syscalls, held.descriptor);
                }
                if read < 0 {
                    refuse_into(state, held.request, held.trace, Cause::Unavailable);
                } else {
                    let length = read as usize;
                    if let Some(entry) = state.entries.get_mut(held.slot) {
                        entry.offset = entry.offset.saturating_add(length as u64);
                    }
                    fulfil_into(
                        state,
                        held.request,
                        held.trace,
                        Answer::Payload(u32::try_from(length).unwrap_or(0)),
                        bytes.get(..length).unwrap_or(&[]),
                    );
                }
            }
        }
    }
    if state.phase == 1 {
        return 1;
    }

    // One request is taken only when there is room for its answer and the
    // bytes it may answer with.
    // No new call while one is held: a payload-held retry reads its bytes
    // from the staging buffer, and a new call would overwrite them.
    if !state.waiting.live && state.staged + COMPLETION_FRAME + PAYLOAD_BYTES <= state.replies.len()
    {
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
                state.deferred = false;
                let (reply, length) = answer(state, &record, &mut bytes, syscalls);
                // Held: the provider has not got the bytes yet, so nothing
                // is staged and the step answers it once it has.
                if !state.deferred {
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
            }
            state.payload_filled = 0;
            if !state.deferred {
                state.payload_length = 0;
            }
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
