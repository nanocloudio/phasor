//! An HTTP request, as a capability rather than a protocol.
//!
//! An isolate has no network and, with this adapter, no HTTP either: it has a
//! binding that performs one request against the one origin a deployment
//! wired, and answers with a handle over the response. What the protocol is,
//! which version it negotiated, how the body was framed on the wire — none of
//! that reaches the program, and none of it is this module's work. The
//! protocol belongs to the provider the graph put behind it.
//!
//! That is what this adapter is for. Speaking HTTP in the surface's
//! JavaScript meant owning HTTP/1.1 framing, chunked decoding and the
//! connection's lifetime in a place where none of it can be shared or
//! replaced. Here the request crosses one seam as a record and the response
//! comes back as a head and a stream, so a deployment that wants HTTP/2 or
//! keep-alive wires a provider that has them rather than waiting for the
//! surface to grow them.
//!
//! # The response is a resource
//!
//! `send` answers a handle, not a body. The status and the headers are read
//! from it, and the body is read through it in pieces, exactly as a file is.
//! That is what makes a response unbounded: nothing here has to hold all of
//! one, and a program that reads slowly is a program the bytes wait for.
//!
//! One request is in flight at a time, because one is what the surface behind
//! this carries. A second is refused as busy rather than queued: a queue here
//! would be a second implementation of ordering, in the module that has the
//! least idea what the requests mean.

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

use abi::contracts::exchange;
use binding::{
    Answer, CallRecord, Cause, CompletionRecord, Disposition, CALL_FRAME, COMPLETION_FRAME,
};

/// Bytes one call or completion may carry.
const PAYLOAD_BYTES: usize = 8 * 1024;
/// Bytes of a response body held before the provider is made to wait.
const BODY_BYTES: usize = 32 * 1024;
/// The response head: a status and the block of fields that came with it.
const HEAD_BYTES: usize = 4 * 1024;
/// Completions staged for the reply port.
const STAGE_BYTES: usize = 2 * (COMPLETION_FRAME + PAYLOAD_BYTES);
/// One publish frame, staged for the provider.
const PUBLISH_BYTES: usize = 3 + exchange::PUBLISH_FRAME_MAX;
/// The largest frame this adapter reads in one piece from either input.
const FRAME_BYTES: usize = 16 * 1024;
/// Calls this adapter may hold while it waits.
const PENDING: usize = 4;
/// What ends a body: a chunk that carries nothing.
const CHUNK_HEADER: usize = 2;

const METHOD_SEND: u32 = 0;
const METHOD_STATUS: u32 = 1;
const METHOD_HEADERS: u32 = 2;
const METHOD_READ: u32 = 3;
const METHOD_CLOSE: u32 = 4;
const METHOD_ORIGIN: u32 = 5;

/// The longest authority the graph may name the origin by.
const AUTHORITY_BYTES: usize = 64;

/// The verb codes the provider's record uses, which are `wire::method`'s.
const VERB_GET: u8 = 1;
const VERB_CONNECT: u8 = 2;
const VERB_POST: u8 = 3;
const VERB_HEAD: u8 = 4;
const VERB_PUT: u8 = 5;
const VERB_PATCH: u8 = 6;
const VERB_DELETE: u8 = 7;
const VERB_OPTIONS: u8 = 8;
/// Set on the verb, this asks for the whole response and says the record
/// carries a header block of its own.
const VERB_EXTENDED: u8 = 0x80;

/// The response in flight, and what has arrived of it.
struct Response {
    live: bool,
    /// Whether the head has come. Until it has, the status and the fields are
    /// not yet anything, and a call asking for them waits.
    head: bool,
    status: u32,
    headers: [u8; HEAD_BYTES],
    headers_length: usize,
    body: [u8; BODY_BYTES],
    filled: usize,
    /// Whether the body's last chunk has come.
    complete: bool,
}

impl Response {
    const EMPTY: Self = Self {
        live: false,
        head: false,
        status: 0,
        headers: [0; HEAD_BYTES],
        headers_length: 0,
        body: [0; BODY_BYTES],
        filled: 0,
        complete: false,
    };
}

/// One call held until the response can answer it.
#[derive(Clone, Copy)]
struct Held {
    request: u64,
    trace: u64,
    method: u32,
    wanted: usize,
    live: bool,
}

impl Held {
    const EMPTY: Self = Self {
        request: 0,
        trace: 0,
        method: 0,
        wanted: 0,
        live: false,
    };
}

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    request_in: i32,
    reply_in: i32,
    body_in: i32,
    reply_out: i32,
    publish_out: i32,

    call: [u8; CALL_FRAME],
    call_filled: usize,
    payload: [u8; PAYLOAD_BYTES],
    payload_length: usize,

    replies: [u8; STAGE_BYTES],
    staged: usize,
    written: usize,

    publish: [u8; PUBLISH_BYTES],
    publish_staged: usize,
    publish_written: usize,

    frame: [u8; FRAME_BYTES],
    frame_filled: usize,
    body_frame: [u8; FRAME_BYTES],
    body_filled: usize,

    response: Response,
    held: [Held; PENDING],
    /// The correlation the next request carries. Never zero, which is what
    /// the surface requires and what lets a reply be told from silence.
    corr: u64,
    answered: u64,
    /// What the origin this adapter reaches is called.
    ///
    /// The graph says, because the provider behind this was given the address
    /// and the program is given neither. A surface that lets a program write
    /// a URL needs the name to compare it against, or a request naming
    /// somewhere else is answered by somewhere else without anybody saying so.
    authority: [u8; AUTHORITY_BYTES],
    authority_length: usize,
    /// Requests this adapter will perform before it refuses.
    quota: u32,
    phase: u8,
}

define_params! {
    State;

    1, quota, u32, 0
        => |s, d, len| { s.quota = p_u32(d, len, 0, 0); };
    2, authority, str, 0
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
        value = value.checked_mul(10)?.checked_add(usize::from(byte - b'0'))?;
    }
    Some(value)
}

/// The verb code a method name asks for, or none for a word that is not one.
fn verb_of(name: &[u8]) -> Option<u8> {
    match name {
        b"GET" => Some(VERB_GET),
        b"HEAD" => Some(VERB_HEAD),
        b"POST" => Some(VERB_POST),
        b"PUT" => Some(VERB_PUT),
        b"PATCH" => Some(VERB_PATCH),
        b"DELETE" => Some(VERB_DELETE),
        b"OPTIONS" => Some(VERB_OPTIONS),
        b"CONNECT" => Some(VERB_CONNECT),
        _ => None,
    }
}

/// Stage one completion, with the bytes it answers with behind it.
fn reply(state: &mut State, record: CompletionRecord, bytes: &[u8]) {
    let at = state.staged;
    let total = COMPLETION_FRAME + bytes.len();
    let Some(slot) = state.replies.get_mut(at..at + total) else {
        return;
    };
    let frame = record.encode();
    if !copy_into(slot.get_mut(..COMPLETION_FRAME).unwrap_or(&mut []), &frame) {
        return;
    }
    if !copy_into(slot.get_mut(COMPLETION_FRAME..).unwrap_or(&mut []), bytes) {
        return;
    }
    state.staged = at + total;
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

/// Hold a call until the response can answer it.
fn hold(state: &mut State, record: &CallRecord, wanted: usize) -> bool {
    let Some(index) = state.held.iter().position(|held| !held.live) else {
        return false;
    };
    state.held[index] = Held {
        request: record.request,
        trace: record.trace,
        method: record.binding,
        wanted,
        live: true,
    };
    true
}

/// Answer every held call the response can now answer.
fn serve_held(state: &mut State) {
    let mut index = 0usize;
    while index < PENDING {
        let held = state.held[index];
        if !held.live {
            index += 1;
            continue;
        }
        if state.staged + COMPLETION_FRAME + PAYLOAD_BYTES > state.replies.len() {
            break;
        }
        match held.method {
            METHOD_STATUS | METHOD_HEADERS if state.response.head => {
                state.held[index] = Held::EMPTY;
                if held.method == METHOD_STATUS {
                    let status = softfloat::from_u64(u64::from(state.response.status));
                    fulfil(state, held.request, held.trace, Answer::Number(status), &[]);
                } else {
                    let length = state.response.headers_length.min(PAYLOAD_BYTES);
                    let mut bytes = [0u8; PAYLOAD_BYTES];
                    copy_into(
                        bytes.get_mut(..length).unwrap_or(&mut []),
                        state.response.headers.get(..length).unwrap_or(&[]),
                    );
                    fulfil(
                        state,
                        held.request,
                        held.trace,
                        Answer::Payload(u32::try_from(length).unwrap_or(0)),
                        bytes.get(..length).unwrap_or(&[]),
                    );
                }
                continue;
            }
            METHOD_READ => {
                if state.response.filled == 0 && !state.response.complete {
                    index += 1;
                    continue;
                }
                // A body that is whole and drained answers with nothing,
                // which is how a reader learns it has all of it.
                let taken = state
                    .response
                    .filled
                    .min(held.wanted)
                    .min(PAYLOAD_BYTES);
                let mut bytes = [0u8; PAYLOAD_BYTES];
                copy_into(
                    bytes.get_mut(..taken).unwrap_or(&mut []),
                    state.response.body.get(..taken).unwrap_or(&[]),
                );
                state.held[index] = Held::EMPTY;
                let rest = state.response.filled - taken;
                let mut at = 0usize;
                while at < rest {
                    state.response.body[at] = state.response.body[taken + at];
                    at += 1;
                }
                state.response.filled = rest;
                fulfil(
                    state,
                    held.request,
                    held.trace,
                    Answer::Payload(u32::try_from(taken).unwrap_or(0)),
                    bytes.get(..taken).unwrap_or(&[]),
                );
                continue;
            }
            _ => {}
        }
        index += 1;
    }
}

entry! {
    State;
    primary { request_in, reply_out }
    inputs { reply_in = 1, body_in = 2 }
    outputs { publish_out = 1 }
    params apply_params
}

/// Perform one call, or hold it until the response can.
fn serve(state: &mut State, record: &CallRecord) {
    let length = state.payload_length.min(state.payload.len());
    let mut held_payload = [0u8; PAYLOAD_BYTES];
    copy_into(
        held_payload.get_mut(..length).unwrap_or(&mut []),
        state.payload.get(..length).unwrap_or(&[]),
    );
    let mut parts: [&[u8]; 4] = [&[], &[], &[], &[]];
    let taken = wire::fields(held_payload.get(..length).unwrap_or(&[]), &mut parts);
    // How many fields each method is: the shape of a call, checked rather
    // than assumed.
    let shape = match record.binding {
        METHOD_ORIGIN => (0, 0),
        METHOD_STATUS | METHOD_HEADERS | METHOD_CLOSE => (1, 1),
        METHOD_READ => (1, 2),
        METHOD_SEND => (2, 4),
        _ => (0, usize::MAX),
    };
    if taken < shape.0 || taken > shape.1 {
        refuse(state, record.request, record.trace, Cause::Malformed);
        return;
    }

    if record.binding == METHOD_ORIGIN {
        // What the graph called the origin, so a surface can tell whether a
        // name it was given is the one this capability reaches.
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
        return;
    }

    if record.binding == METHOD_SEND {
        if state.quota != 0 && state.answered >= u64::from(state.quota) {
            refuse(state, record.request, record.trace, Cause::Denied);
            return;
        }
        // One request is in flight at a time, because one is what the surface
        // behind this carries.
        if state.response.live || state.publish_staged != 0 {
            refuse(state, record.request, record.trace, Cause::Busy);
            return;
        }
        let Some(verb) = verb_of(parts[0]) else {
            refuse(state, record.request, record.trace, Cause::Malformed);
            return;
        };
        let path = parts[1];
        let headers = if taken > 2 { parts[2] } else { &[] };
        let body = if taken > 3 { parts[3] } else { &[] };
        if path.is_empty() || path.len() + headers.len() + body.len() + 7 > exchange::PAYLOAD_MAX {
            refuse(state, record.request, record.trace, Cause::Malformed);
            return;
        }
        // The record the provider takes: a verb whose high bit asks for the
        // whole response, the three lengths, then the three fields.
        let mut payload = [0u8; exchange::PAYLOAD_MAX];
        payload[0] = verb | VERB_EXTENDED;
        let path_len = u16::try_from(path.len()).unwrap_or(0).to_le_bytes();
        let body_len = u16::try_from(body.len()).unwrap_or(0).to_le_bytes();
        let head_len = u16::try_from(headers.len()).unwrap_or(0).to_le_bytes();
        payload[1] = path_len[0];
        payload[2] = path_len[1];
        payload[3] = body_len[0];
        payload[4] = body_len[1];
        payload[5] = head_len[0];
        payload[6] = head_len[1];
        let mut at = 7usize;
        for field in [path, headers, body] {
            if !copy_into(
                payload.get_mut(at..at + field.len()).unwrap_or(&mut []),
                field,
            ) {
                refuse(state, record.request, record.trace, Cause::Internal);
                return;
            }
            at += field.len();
        }
        state.corr = state.corr.wrapping_add(1).max(1);
        let publish = exchange::Publish {
            corr: state.corr,
            flags: 0,
            msg_key: &[],
            payload: payload.get(..at).unwrap_or(&[]),
        };
        let Some(encoded) = publish.encode(state.publish.get_mut(3..).unwrap_or(&mut [])) else {
            refuse(state, record.request, record.trace, Cause::Internal);
            return;
        };
        state.publish[0] = exchange::MSG_PUBLISH;
        let framed = u16::try_from(encoded).unwrap_or(0).to_le_bytes();
        state.publish[1] = framed[0];
        state.publish[2] = framed[1];
        state.publish_staged = 3 + encoded;
        state.publish_written = 0;
        state.response = Response {
            live: true,
            ..Response::EMPTY
        };
        state.answered = state.answered.saturating_add(1);
        // The handle is answered now, not when the head comes: a program
        // reads the status through it, and waiting to hand it over would
        // mean waiting for the thing the handle is how you ask about.
        fulfil(
            state,
            record.request,
            record.trace,
            Answer::Resource(0),
            &[],
        );
        return;
    }

    // Every other method names the response it is about.
    let Some(slot) = digits(parts[0]) else {
        refuse(state, record.request, record.trace, Cause::Malformed);
        return;
    };
    if slot != 0 || !state.response.live {
        refuse(state, record.request, record.trace, Cause::Malformed);
        return;
    }
    match record.binding {
        METHOD_CLOSE => {
            state.response = Response::EMPTY;
            fulfil(state, record.request, record.trace, Answer::None, &[]);
        }
        METHOD_STATUS | METHOD_HEADERS | METHOD_READ => {
            let wanted = if record.binding == METHOD_READ {
                digits(if taken > 1 { parts[1] } else { &[] }).unwrap_or(PAYLOAD_BYTES)
            } else {
                0
            };
            if !hold(state, record, wanted.min(PAYLOAD_BYTES)) {
                refuse(state, record.request, record.trace, Cause::Busy);
                return;
            }
            serve_held(state);
        }
        _ => refuse(state, record.request, record.trace, Cause::Malformed),
    }
}

/// Take the head of a response from one `Reply` frame.
fn apply_reply(state: &mut State, payload: &[u8]) {
    let Some(reply) = exchange::Reply::decode(payload) else {
        return;
    };
    if reply.corr != state.corr || !state.response.live {
        return;
    }
    if reply.status != exchange::STATUS_OK {
        // The provider refused the exchange. Every call waiting on a response
        // that will not come is told so.
        let mut index = 0usize;
        while index < PENDING {
            let held = state.held[index];
            if held.live {
                state.held[index] = Held::EMPTY;
                refuse(state, held.request, held.trace, Cause::Unavailable);
            }
            index += 1;
        }
        state.response.complete = true;
        state.response.head = true;
        return;
    }
    let body = reply.payload;
    if body.len() < 4 {
        return;
    }
    state.response.status =
        u32::from(u16::from_le_bytes([body[0], body[1]]));
    let headers_length = usize::from(u16::from_le_bytes([body[2], body[3]]));
    let headers = body.get(4..4 + headers_length).unwrap_or(&[]);
    let kept = headers.len().min(HEAD_BYTES);
    copy_into(
        state.response.headers.get_mut(..kept).unwrap_or(&mut []),
        headers.get(..kept).unwrap_or(&[]),
    );
    state.response.headers_length = kept;
    state.response.head = true;
}

/// Take one length-framed chunk of the body, or its ending.
fn apply_body(state: &mut State, chunk: &[u8]) {
    if !state.response.live {
        return;
    }
    if chunk.is_empty() {
        // The chunk that carries nothing is the one that says there are no
        // more: without it a reader could not tell a pause from an ending.
        state.response.complete = true;
        return;
    }
    let at = state.response.filled;
    if copy_into(
        state
            .response
            .body
            .get_mut(at..at + chunk.len())
            .unwrap_or(&mut []),
        chunk,
    ) {
        state.response.filled = at + chunk.len();
    }
}

/// Whether the body buffer can hold the chunk a staged frame carries.
fn room_for_body(state: &State, frame: &[u8]) -> bool {
    let Some(head) = frame.get(..CHUNK_HEADER) else {
        return true;
    };
    let carried = usize::from(u16::from_le_bytes([head[0], head[1]]));
    carried == 0 || BODY_BYTES - state.response.filled >= carried
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

    // The head of a response, which comes before the body it describes.
    if state.reply_in >= 0 {
        let read = wire::read_available(
            syscalls,
            state.reply_in,
            state.frame.get_mut(state.frame_filled..).unwrap_or(&mut []),
        );
        state.frame_filled += read;
        while state.frame_filled >= 3 {
            let kind = state.frame[0];
            let length = usize::from(u16::from_le_bytes([state.frame[1], state.frame[2]]));
            let total = 3 + length;
            if total > state.frame.len() {
                state.frame_filled = 0;
                break;
            }
            if state.frame_filled < total {
                break;
            }
            let mut payload = [0u8; FRAME_BYTES];
            copy_into(
                payload.get_mut(..length).unwrap_or(&mut []),
                state.frame.get(3..total).unwrap_or(&[]),
            );
            if kind == exchange::MSG_REPLY {
                apply_reply(state, payload.get(..length).unwrap_or(&[]));
            }
            let rest = state.frame_filled - total;
            let mut at = 0usize;
            while at < rest {
                state.frame[at] = state.frame[total + at];
                at += 1;
            }
            state.frame_filled = rest;
        }
    }

    // The body, in the chunks it was framed as.
    if state.body_in >= 0 {
        // Drain what is there, not one read of it. A provider that fills the
        // channel faster than this takes from it is one whose writes start
        // failing, and a stream that stops for that reason stops for good --
        // the reader has to keep up, or take everything each time it looks.
        loop {
            let read = wire::read_available(
                syscalls,
                state.body_in,
                state
                    .body_frame
                    .get_mut(state.body_filled..)
                    .unwrap_or(&mut []),
            );
            if read == 0 {
                break;
            }
            state.body_filled += read;
        }
        while state.body_filled >= CHUNK_HEADER {
            let carried = usize::from(u16::from_le_bytes([
                state.body_frame[0],
                state.body_frame[1],
            ]));
            let total = CHUNK_HEADER + carried;
            if total > state.body_frame.len() {
                state.body_filled = 0;
                break;
            }
            if state.body_filled < total {
                break;
            }
            // A chunk the body cannot hold is left where it is and taken
            // again once the program has read what is already here, so the
            // stream stops rather than losing the middle of itself.
            if !room_for_body(state, state.body_frame.get(..total).unwrap_or(&[])) {
                break;
            }
            let mut chunk = [0u8; FRAME_BYTES];
            copy_into(
                chunk.get_mut(..carried).unwrap_or(&mut []),
                state.body_frame.get(CHUNK_HEADER..total).unwrap_or(&[]),
            );
            apply_body(state, chunk.get(..carried).unwrap_or(&[]));
            let rest = state.body_filled - total;
            let mut at = 0usize;
            while at < rest {
                state.body_frame[at] = state.body_frame[total + at];
                at += 1;
            }
            state.body_filled = rest;
        }
    }

    serve_held(state);

    // One call is taken only when there is room for what it may answer with.
    if state.staged + COMPLETION_FRAME + PAYLOAD_BYTES <= state.replies.len()
        && wire::take_frame(
            syscalls,
            state.request_in,
            &mut state.call,
            &mut state.call_filled,
        )
    {
        if let Some(record) = CallRecord::decode(&state.call) {
            let wanted = record.payload_length as usize;
            if wanted == 0
                || wire::take_payload(
                    syscalls,
                    state.request_in,
                    &mut state.payload,
                    &mut state.payload_length,
                    wanted,
                )
            {
                state.call_filled = 0;
                state.payload_length = wanted.min(state.payload.len());
                serve(state, &record);
                state.payload_length = 0;
            }
        } else {
            state.call_filled = 0;
        }
    }

    wire::push_staged(
        syscalls,
        state.publish_out,
        &state.publish,
        &mut state.publish_staged,
        &mut state.publish_written,
    );
    wire::push_staged(
        syscalls,
        state.reply_out,
        &state.replies,
        &mut state.staged,
        &mut state.written,
    );
    0
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
