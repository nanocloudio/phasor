//! A WebSocket, as a capability rather than a protocol.
//!
//! The protocol is not this project's. A deployment that wants a program to
//! hold a WebSocket wires this adapter behind the router and a provider that
//! speaks RFC 6455 in front of it; what crosses here is which link, which
//! message, and what became of it. The upgrade, the accept it verifies, the
//! masking and the frame codec all live in the provider, where one
//! implementation serves every consumer on the platform -- rather than in
//! JavaScript in the realm, where it would be the program's own fuel paying
//! for the SHA-1 of every handshake.
//!
//! The endpoint is the deployment's, exactly as it is for a stream
//! connection: `open` takes the resource and not the host, because where a
//! link goes is the graph's to decide. A deployment that wants two endpoints
//! wires two providers, and each is granted separately.
//!
//! Messages cross as `WsFrame`, whose envelope Fluxor owns -- so the link a
//! message arrived on and whether it was text or binary are read from the
//! contract rather than from anything agreed privately between the two ends.

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
#[path = "../../common/text.rs"]
mod text;
#[path = "../../common/value.rs"]
mod value;
#[path = "../../common/wire.rs"]
mod wire;

use abi::contracts::net::ws_frame as wsf;
use binding::{
    Answer, CallRecord, Cause, CompletionRecord, Disposition, CALL_FRAME, COMPLETION_FRAME,
};

/// Links this adapter may hold at once. The provider carries the same number,
/// and a mismatch either way is a link one end can name and the other cannot.
const LINKS: usize = 4;
/// Bytes one call or completion may carry.
const PAYLOAD_BYTES: usize = 8 * 1024;
/// The largest message either direction. The provider's own ceiling: a longer
/// one ends the link there, and is refused here rather than staged.
const MESSAGE_BYTES: usize = 2048;
/// The longest resource an `open` may name.
const PATH_BYTES: usize = 128;
/// Completions staged for the reply port.
const STAGE_BYTES: usize = 2 * (COMPLETION_FRAME + PAYLOAD_BYTES);
/// Opens staged for the provider.
const OPEN_BYTES: usize = LINKS * (1 + PATH_BYTES);
/// Frames staged for the provider.
const FRAME_BYTES: usize = 2 * (wsf::FRAME_HDR + MESSAGE_BYTES);
/// Calls held until the provider answers.
const PENDING: usize = 8;

/// The methods the interface offers.
const METHOD_OPEN: u32 = 0;
const METHOD_SEND: u32 = 1;
const METHOD_RECEIVE: u32 = 2;
const METHOD_CLOSE: u32 = 3;
const METHOD_ORIGIN: u32 = 4;

/// The longest authority the graph may name the origin by.
const AUTHORITY_BYTES: usize = 64;

/// What the provider says became of a link: `[conn][event][code: u16 LE]`.
/// The provider's record, restated here because a consumer of it must read
/// it and the two projects share no source -- the one place this seam is not
/// carried by a contract Fluxor owns.
const EVENT_LEN: usize = 4;
const EVENT_OPEN: u8 = 1;
const EVENT_CLOSED: u8 = 2;
const EVENT_FAILED: u8 = 3;

/// The RFC 6455 opcodes that reach a program. A close arrives as one, because
/// a reader waiting for a message has no other way to learn there will not be
/// another.
const OPCODE_TEXT: u8 = 1;
const OPCODE_BINARY: u8 = 2;
const OPCODE_CLOSE: u8 = 8;

/// One link the provider holds for this adapter.
#[derive(Clone, Copy)]
struct Link {
    /// Whether this adapter has claimed the slot.
    live: bool,
    /// Whether the provider has said it opened.
    open: bool,
    /// Whether the far end has gone. A closed link still answers the reads
    /// its program has already asked for, and then answers closed.
    closed: bool,
    /// One message, held until a `receive` takes it. The provider holds the
    /// next one meanwhile, which is what keeps a slow reader from losing the
    /// middle of a stream.
    message: [u8; MESSAGE_BYTES],
    message_len: usize,
    message_opcode: u8,
    message_ready: bool,
}

impl Link {
    const EMPTY: Self = Self {
        live: false,
        open: false,
        closed: false,
        message: [0; MESSAGE_BYTES],
        message_len: 0,
        message_opcode: 0,
        message_ready: false,
    };
}

/// One call held until the provider answers it.
#[derive(Clone, Copy)]
struct Held {
    request: u64,
    trace: u64,
    method: u32,
    slot: usize,
    live: bool,
}

impl Held {
    const EMPTY: Self = Self {
        request: 0,
        trace: 0,
        method: 0,
        slot: 0,
        live: false,
    };
}

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    request_in: i32,
    reply_out: i32,
    event_in: i32,
    frames_in: i32,
    open_out: i32,
    frames_out: i32,

    request: [u8; CALL_FRAME],
    filled: usize,
    payload: [u8; PAYLOAD_BYTES],
    payload_filled: usize,
    frame_ready: bool,
    payload_length: usize,

    replies: [u8; STAGE_BYTES],
    staged: usize,
    written: usize,

    opens: [u8; OPEN_BYTES],
    open_staged: usize,
    open_written: usize,

    frames: [u8; FRAME_BYTES],
    frame_staged: usize,
    frame_written: usize,

    event: [u8; EVENT_LEN],
    event_filled: usize,

    /// One envelope read from `frames_in` and not yet given to its link.
    ///
    /// A mailbox channel hands over a whole envelope or nothing, and there is
    /// no peeking at one -- so which link a message is for is known only once
    /// it has been taken. Taking one for a link whose message is still unread
    /// and dropping it would be a hole in that link's stream; leaving it in
    /// the channel is not on offer once it has been read. So it is held here,
    /// and nothing else is read until it has been placed.
    pending: [u8; wsf::FRAME_HDR + MESSAGE_BYTES],
    pending_len: usize,

    /// What the endpoint behind this adapter answers to. A program naming a
    /// URL has only the name, so a surface needs this to tell whether the
    /// host it was given is the one this capability reaches -- the same check
    /// `fetch` makes, for the same reason: a program must not be handed one
    /// origin's stream while it believes it reached another.
    authority: [u8; AUTHORITY_BYTES],
    authority_length: usize,

    links: [Link; LINKS],
    held: [Held; PENDING],
    answered: u64,
    phase: u8,
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

/// Stage one completion, with any bytes it answers with behind it.
fn reply(state: &mut State, record: CompletionRecord, bytes: &[u8]) {
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

fn refuse(state: &mut State, request: u64, trace: u64, cause: Cause) {
    reply(
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

fn fulfil(state: &mut State, request: u64, trace: u64, answer: Answer, bytes: &[u8]) {
    reply(
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

/// Hold a call until the provider answers it.
fn hold(state: &mut State, record: &CallRecord, slot: usize) -> bool {
    let Some(index) = state.held.iter().position(|held| !held.live) else {
        return false;
    };
    state.held[index] = Held {
        request: record.request,
        trace: record.trace,
        method: record.binding,
        slot,
        live: true,
    };
    true
}

/// Stage `[conn][path]` for the provider.
fn stage_open(state: &mut State, slot: usize, path: &[u8]) -> bool {
    let at = state.open_staged;
    let total = 1 + path.len();
    let Some(room) = state.opens.get_mut(at..at + total) else {
        return false;
    };
    room[0] = slot as u8;
    if !copy_into(room.get_mut(1..).unwrap_or(&mut []), path) {
        return false;
    }
    state.open_staged = at + total;
    true
}

/// Stage one message for the provider.
fn stage_frame(state: &mut State, slot: usize, opcode: u8, payload: &[u8]) -> bool {
    let at = state.frame_staged;
    let total = wsf::FRAME_HDR + payload.len();
    let Some(room) = state.frames.get_mut(at..at + total) else {
        return false;
    };
    let length = u16::try_from(payload.len()).unwrap_or(0);
    wsf::put_header(room, slot as u32, opcode, 1, length);
    if !copy_into(room.get_mut(wsf::FRAME_HDR..).unwrap_or(&mut []), payload) {
        return false;
    }
    state.frame_staged = at + total;
    true
}

/// Answer a held `receive` from what its link has, or from its ending.
fn serve_receives(state: &mut State) {
    let mut index = 0usize;
    while index < PENDING {
        let held = state.held[index];
        if !held.live || held.method != METHOD_RECEIVE {
            index += 1;
            continue;
        }
        let Some(link) = state.links.get(held.slot).copied() else {
            state.held[index] = Held::EMPTY;
            refuse(state, held.request, held.trace, Cause::Malformed);
            index += 1;
            continue;
        };
        if link.message_ready {
            // The opcode leads, so a reader learns what it was given without
            // a second call: text and binary are different readings of the
            // same bytes, and a close is neither.
            let mut answer = [0u8; 1 + MESSAGE_BYTES];
            answer[0] = link.message_opcode;
            let length = link.message_len.min(MESSAGE_BYTES);
            if copy_into(
                answer.get_mut(1..1 + length).unwrap_or(&mut []),
                link.message.get(..length).unwrap_or(&[]),
            ) {
                state.held[index] = Held::EMPTY;
                if let Some(entry) = state.links.get_mut(held.slot) {
                    entry.message_ready = false;
                    entry.message_len = 0;
                    entry.message_opcode = 0;
                }
                let request = held.request;
                let trace = held.trace;
                fulfil(
                    state,
                    request,
                    trace,
                    Answer::Payload(u32::try_from(1 + length).unwrap_or(0)),
                    answer.get(..1 + length).unwrap_or(&[]),
                );
            }
        } else if link.closed {
            // Nothing more is coming. A close answers the read rather than
            // leaving it outstanding forever, and it says so in the one way
            // the caller already has to understand.
            state.held[index] = Held::EMPTY;
            let request = held.request;
            let trace = held.trace;
            fulfil(state, request, trace, Answer::Payload(1), &[OPCODE_CLOSE]);
        }
        index += 1;
    }
}

/// Answer every call held against a link that has just been decided.
fn settle(state: &mut State, slot: usize, event: u8) {
    let mut index = 0usize;
    while index < PENDING {
        let held = state.held[index];
        if !held.live || held.slot != slot {
            index += 1;
            continue;
        }
        match (held.method, event) {
            (METHOD_OPEN, EVENT_OPEN) => {
                state.held[index] = Held::EMPTY;
                let (request, trace) = (held.request, held.trace);
                fulfil(state, request, trace, Answer::Resource(slot as u64), &[]);
            }
            (METHOD_OPEN, _) => {
                state.held[index] = Held::EMPTY;
                let (request, trace) = (held.request, held.trace);
                refuse(state, request, trace, Cause::Unavailable);
                if let Some(link) = state.links.get_mut(slot) {
                    *link = Link::EMPTY;
                }
            }
            _ => {}
        }
        index += 1;
    }
    // A read outstanding on a link that has ended is answered by the ending.
    serve_receives(state);
}

fn apply_event(state: &mut State, record: &[u8]) {
    let Some(&slot) = record.first() else {
        return;
    };
    let slot = slot as usize;
    if slot >= LINKS {
        return;
    }
    let event = record.get(1).copied().unwrap_or(0);
    match event {
        EVENT_OPEN => {
            if let Some(link) = state.links.get_mut(slot) {
                link.open = true;
            }
        }
        EVENT_CLOSED | EVENT_FAILED => {
            if let Some(link) = state.links.get_mut(slot) {
                link.closed = true;
                link.open = false;
            }
        }
        _ => return,
    }
    settle(state, slot, event);
}

fn apply_frame(state: &mut State, frame: &[u8]) {
    if frame.len() < wsf::FRAME_HDR {
        return;
    }
    let slot = wsf::conn_id(frame) as usize;
    if slot >= LINKS {
        return;
    }
    let length = (wsf::payload_len(frame) as usize).min(MESSAGE_BYTES);
    let opcode = wsf::opcode(frame);
    let body = frame.get(wsf::FRAME_HDR..wsf::FRAME_HDR + length).unwrap_or(&[]);
    if let Some(link) = state.links.get_mut(slot) {
        if link.message_ready {
            // Admitted only when the link had room, so this cannot happen
            // without the room check above having been wrong. Dropping the
            // message would be a hole in the stream nothing could see.
            return;
        }
        if copy_into(link.message.get_mut(..length).unwrap_or(&mut []), body) {
            link.message_len = length;
            link.message_opcode = opcode;
            link.message_ready = true;
        }
    }
    serve_receives(state);
}

fn dispatch(state: &mut State, record: &CallRecord, payload: &[u8]) {
    let mut parts: [&[u8]; 4] = [&[]; 4];
    let taken = wire::fields(payload, &mut parts);
    let field = |index: usize| -> &[u8] {
        if index >= taken {
            return &[];
        }
        parts[index]
    };
    match record.binding {
        METHOD_OPEN => {
            let path = field(0);
            if path.len() > PATH_BYTES || (!path.is_empty() && path[0] != b'/') {
                refuse(state, record.request, record.trace, Cause::Malformed);
                return;
            }
            let Some(slot) = state.links.iter().position(|link| !link.live) else {
                refuse(state, record.request, record.trace, Cause::Busy);
                return;
            };
            let root = [b'/'];
            let path = if path.is_empty() { &root[..] } else { path };
            state.links[slot] = Link {
                live: true,
                ..Link::EMPTY
            };
            if !stage_open(state, slot, path) || !hold(state, record, slot) {
                state.links[slot] = Link::EMPTY;
                refuse(state, record.request, record.trace, Cause::Busy);
            }
        }
        METHOD_SEND => {
            let Some(slot) = digits(field(0)).filter(|slot| *slot < LINKS) else {
                refuse(state, record.request, record.trace, Cause::Malformed);
                return;
            };
            let Some(link) = state.links.get(slot).filter(|link| link.live && link.open) else {
                refuse(state, record.request, record.trace, Cause::Malformed);
                return;
            };
            let _ = link;
            let opcode = match digits(field(1)) {
                Some(1) => OPCODE_TEXT,
                Some(2) => OPCODE_BINARY,
                _ => {
                    refuse(state, record.request, record.trace, Cause::Malformed);
                    return;
                }
            };
            let body = field(2);
            if body.len() > MESSAGE_BYTES {
                refuse(state, record.request, record.trace, Cause::Malformed);
                return;
            }
            if stage_frame(state, slot, opcode, body) {
                fulfil(state, record.request, record.trace, Answer::None, &[]);
            } else {
                refuse(state, record.request, record.trace, Cause::Busy);
            }
        }
        METHOD_RECEIVE => {
            let Some(slot) = digits(field(0)).filter(|slot| *slot < LINKS) else {
                refuse(state, record.request, record.trace, Cause::Malformed);
                return;
            };
            let Some(link) = state.links.get(slot).filter(|link| link.live) else {
                refuse(state, record.request, record.trace, Cause::Malformed);
                return;
            };
            let _ = link;
            if !hold(state, record, slot) {
                refuse(state, record.request, record.trace, Cause::Busy);
                return;
            }
            serve_receives(state);
        }
        METHOD_CLOSE => {
            let Some(slot) = digits(field(0)).filter(|slot| *slot < LINKS) else {
                refuse(state, record.request, record.trace, Cause::Malformed);
                return;
            };
            let Some(link) = state.links.get(slot).filter(|link| link.live) else {
                refuse(state, record.request, record.trace, Cause::Malformed);
                return;
            };
            if link.open && !link.closed {
                // The provider is asked to close; the link stays claimed
                // until it says it has, so the slot is not handed out from
                // under a close still in flight.
                stage_frame(state, slot, OPCODE_CLOSE, &[]);
            }
            fulfil(state, record.request, record.trace, Answer::None, &[]);
        }
        METHOD_ORIGIN => {
            // What the graph called the endpoint, so a surface can tell
            // whether a host it was given is the one this capability reaches.
            let length = state.authority_length.min(AUTHORITY_BYTES);
            let mut text = [0u8; AUTHORITY_BYTES];
            copy_into(
                text.get_mut(..length).unwrap_or(&mut []),
                state.authority.get(..length).unwrap_or(&[]),
            );
            fulfil(
                state,
                record.request,
                record.trace,
                Answer::Payload(u32::try_from(length).unwrap_or(0)),
                text.get(..length).unwrap_or(&[]),
            );
        }
        _ => refuse(state, record.request, record.trace, Cause::Malformed),
    }
}

define_params! {
    State;

    1, authority, str, 0
        => |s, d, len| {
            let taken = if len > AUTHORITY_BYTES { AUTHORITY_BYTES } else { len };
            s.authority_length = taken;
            if taken > 0 {
                // SAFETY: the params reader hands a pointer valid for `len`
                // bytes, and `taken` is no larger than it or the field.
                unsafe {
                    core::ptr::copy_nonoverlapping(d, s.authority.as_mut_ptr(), taken);
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

entry! {
    State;
    primary { request_in, reply_out }
    inputs { event_in = 1, frames_in = 2 }
    outputs { open_out = 1, frames_out = 2 }
    params apply_params
}

#[cfg_attr(not(feature = "host-test"), unsafe(no_mangle))]
#[link_section = ".text.module_step"]
pub extern "C" fn module_step(state: *mut u8) -> i32 {
    if state.is_null() {
        return -1;
    }
    // SAFETY: the loader owns this arena for the module's lifetime and hands
    // the same pointer to every step.
    let state = unsafe { &mut *(state as *mut State) };
    if state.syscalls.is_null() || state.request_in < 0 || state.reply_out < 0 {
        return -2;
    }
    // SAFETY: stored by `module_new` and checked non-null above.
    let syscalls = unsafe { &*state.syscalls };
    if state.phase == 1 {
        return 1;
    }

    // What the provider says became of a link, one whole record at a time.
    if state.event_in >= 0 {
        while wire::take_frame(
            syscalls,
            state.event_in,
            &mut state.event,
            &mut state.event_filled,
        ) {
            let record = state.event;
            state.event_filled = 0;
            apply_event(state, &record);
        }
    }

    // Messages, one at a time, each placed on the link it names before
    // another is taken.
    if state.frames_in >= 0 {
        let mut budget = 0;
        while budget < 8 {
            budget += 1;
            if state.pending_len == 0 {
                if !wire::has_input(syscalls, state.frames_in) {
                    break;
                }
                // SAFETY: `pending` is valid for its own length.
                let read = unsafe {
                    (syscalls.channel_read)(
                        state.frames_in,
                        state.pending.as_mut_ptr(),
                        wsf::FRAME_HDR + MESSAGE_BYTES,
                    )
                };
                if read < wsf::FRAME_HDR as i32 {
                    break;
                }
                state.pending_len = read as usize;
            }
            let slot = wsf::conn_id(&state.pending) as usize;
            let free = state
                .links
                .get(slot)
                .is_some_and(|link| link.live && !link.message_ready);
            if slot < LINKS && !free {
                // The program has not taken the last one. Hold this and stop
                // reading: the provider keeps the next behind it.
                let gone = state.links.get(slot).is_some_and(|link| !link.live);
                if gone {
                    state.pending_len = 0;
                    continue;
                }
                break;
            }
            let held = state.pending_len;
            state.pending_len = 0;
            let mut frame = [0u8; wsf::FRAME_HDR + MESSAGE_BYTES];
            if copy_into(
                frame.get_mut(..held).unwrap_or(&mut []),
                state.pending.get(..held).unwrap_or(&[]),
            ) {
                apply_frame(state, frame.get(..held).unwrap_or(&[]));
            }
        }
    }

    // One call is taken only when there is room for its answer.
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
                let length = state.payload_length.min(PAYLOAD_BYTES);
                let mut payload = [0u8; PAYLOAD_BYTES];
                if copy_into(
                    payload.get_mut(..length).unwrap_or(&mut []),
                    state.payload.get(..length).unwrap_or(&[]),
                ) {
                    dispatch(state, &record, payload.get(..length).unwrap_or(&[]));
                } else {
                    refuse(state, record.request, record.trace, Cause::Internal);
                }
            }
            state.payload_filled = 0;
            state.payload_length = 0;
        }
    }

    serve_receives(state);

    wire::push_staged(
        syscalls,
        state.open_out,
        &state.opens,
        &mut state.open_staged,
        &mut state.open_written,
    );
    wire::push_staged(
        syscalls,
        state.frames_out,
        &state.frames,
        &mut state.frame_staged,
        &mut state.frame_written,
    );
    wire::push_staged(
        syscalls,
        state.reply_out,
        &state.replies,
        &mut state.staged,
        &mut state.written,
    );

    if wire::hung_up(syscalls, state.request_in)
        && state.staged == 0
        && !state.held.iter().any(|held| held.live)
    {
        state.phase = 1;
        return 1;
    }
    0
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
