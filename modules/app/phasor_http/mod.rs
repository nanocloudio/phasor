//! An HTTP request, as a capability rather than a protocol.
//!
//! An isolate has no network and, with this adapter, no HTTP either: it has a
//! binding that performs one request against an authority the program names
//! and the deployment admits, and answers with a handle over the response.
//! What the protocol is, which version it negotiated, how the body was framed
//! on the wire — none of that reaches the program, and none of it is this
//! module's work. The protocol belongs to the provider the graph put behind
//! it.
//!
//! # The authority is one fact
//!
//! `send` names where the request goes, as `host[:port]`, and that name is
//! written into the record itself. The provider behind either output dials
//! it, the transport under that provider verifies it, and this module polices
//! it: `origins` is the deployment's allow-list, and an authority outside it
//! is refused here, before any record is composed. Empty means no policy at
//! this boundary: an applet granted `http` may reach any authority it names.
//!
//! The scheme picks the output. `https` goes to `publish_out`, which a graph
//! wires through its TLS leg; `http` goes to `plain_out`, wired straight to
//! the socket. A scheme whose leg the graph did not wire is refused rather
//! than sent down the other one: a deployment that wired no cleartext leg
//! has said so.
//!
//! That is what this adapter is for. HTTP/1.1 framing, chunked decoding and
//! a connection's lifetime are the provider's, where one implementation
//! serves every consumer on the platform; in the surface's JavaScript they
//! would be a second implementation that nothing else can share or replace.
//! Here the request crosses one seam as a record and the response comes back
//! as a head and a stream, so a deployment that wants HTTP/2 or keep-alive
//! wires a provider that has them.
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

// The records this adapter composes and reads are fluxor's `http_exchange`
// contract, already carried by the `abi` mount above. Every connector that
// performs the request reads the same definition, so an offset written here
// could not disagree with one written there -- because neither end writes
// one.
use abi::contracts::net::http_exchange as wire_http;
// The authority a request names is parsed by the stream contract's own
// reader, so what this module admits is exactly what a provider can dial:
// a name, a dotted quad, or a bracketed v6 literal, with a port or without.
use abi::contracts::net::net_proto as proto;
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

const METHOD_SEND: u32 = 0;
const METHOD_STATUS: u32 = 1;
const METHOD_HEADERS: u32 = 2;
const METHOD_READ: u32 = 3;
const METHOD_CLOSE: u32 = 4;
const METHOD_ORIGINS: u32 = 5;
/// Narrow the allow-list to the intersection with the one supplied.
///
/// The one direction it may move. A deployment's `origins` is the ceiling;
/// this only ever lowers it, so a caller that can reach this method — the
/// shell, carrying what an operator typed after `--grant http=` — cannot
/// widen what the graph admitted. An empty supplied list is a list of none
/// and refuses everything, which is what an operator asking for no origins
/// has asked for.
const METHOD_NARROW: u32 = 6;

/// Bytes the comma-separated `origins` allow-list may hold.
const ORIGINS_BYTES: usize = 512;

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
    announced: bool,
    request_in: i32,
    reply_in: i32,
    body_in: i32,
    reply_out: i32,
    publish_out: i32,
    plain_out: i32,
    /// The output the staged record goes to: whichever leg the request's
    /// scheme picked.
    publish_port: i32,

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
    /// The authorities a program may name, comma-separated, `host[:port]`
    /// each. Empty is no policy: every authority is admitted, and what the
    /// program wrote is what is dialled.
    origins: [u8; ORIGINS_BYTES],
    origins_length: usize,
    /// Requests this adapter will perform before it refuses.
    quota: u32,
    phase: u8,
}

define_params! {
    State;

    1, quota, u32, 0
        => |s, d, len| { s.quota = p_u32(d, len, 0, 0); };
    // 2: retired.
    3, origins, str, 0
        => |s, d, len| {
            // A policy too long to hold is not held in part: the first
            // `ORIGINS_BYTES` of a longer list is a different policy, and an
            // empty one would be no policy at all — every origin admitted.
            // It becomes instead a list of one empty entry, which no
            // authority equals, so every request is refused until the
            // deployment writes a list that fits.
            if len > ORIGINS_BYTES {
                s.origins[0] = b',';
                s.origins_length = 1;
                return;
            }
            let taken = len;
            s.origins_length = taken;
            if taken > 0 {
                // SAFETY: the params reader hands a pointer valid for `len`
                // bytes, and `taken` is no larger than it or the field.
                unsafe {
                    core::ptr::copy_nonoverlapping(d, s.origins.as_mut_ptr(), taken);
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
/// carry a panic path, and a module image carries none. The copy itself is
/// a raw one, because at `opt_level = "z"` the slice copy is not inlined
/// and its own length check, though never reached, is a panic path too.
fn copy_into(dst: &mut [u8], src: &[u8]) -> bool {
    if dst.len() != src.len() {
        return false;
    }
    // SAFETY: the lengths are equal, and two distinct slices never overlap.
    unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr(), src.len()) };
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

/// The port an authority names, if it names one. A bracketed IPv6 literal
/// carries colons of its own, so the port is the text after the LAST colon
/// and only when that colon follows the closing bracket.
fn port_of(authority: &[u8]) -> Option<&[u8]> {
    let close = authority.iter().rposition(|&c| c == b']');
    let colon = authority.iter().rposition(|&c| c == b':')?;
    match close {
        Some(at) if colon < at => None,
        _ => authority.get(colon + 1..).filter(|p| !p.is_empty()),
    }
}

/// Whether two authorities are the same one. A host is case-insensitive
/// and a port is digits, so an ASCII fold is the whole comparison.
fn same_authority(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// One entry with its surrounding spaces removed, which a hand-written list
/// may carry after a comma.
fn trim_spaces(mut entry: &[u8]) -> &[u8] {
    while let Some((&first, rest)) = entry.split_first() {
        if first != b' ' {
            break;
        }
        entry = rest;
    }
    while let Some((&last, rest)) = entry.split_last() {
        if last != b' ' {
            break;
        }
        entry = rest;
    }
    entry
}

/// Whether the allow-list admits `authority`. An empty list is no policy;
/// otherwise the authority must be one of its comma-separated entries,
/// with the space a hand-written list may put after a comma ignored.
fn granted(list: &[u8], authority: &[u8]) -> bool {
    if list.is_empty() {
        return true;
    }
    list.split(|&c| c == b',').any(|entry| {
        let mut entry = entry;
        while let Some((&first, rest)) = entry.split_first() {
            if first != b' ' {
                break;
            }
            entry = rest;
        }
        while let Some((&last, rest)) = entry.split_last() {
            if last != b' ' {
                break;
            }
            entry = rest;
        }
        !entry.is_empty() && same_authority(entry, authority)
    })
}

/// The verb code a method name asks for, or none for a word that is not one.
/// The table is the contract's; only the shape of the answer is this
/// adapter's, which wants absence rather than a sentinel.
fn verb_of(name: &[u8]) -> Option<u8> {
    match wire_http::method_from_token(name) {
        wire_http::METHOD_NONE => None,
        verb => Some(verb),
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
                let taken = state.response.filled.min(held.wanted).min(PAYLOAD_BYTES);
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

// Both legs answer on the one `reply_in`/`body_in` pair. That is safe because
// one request is in flight at a time and only the leg it was sent down has
// anything to say: a reply carries the correlation this module chose, and a
// body chunk can only follow a reply. A second pair per leg would be a
// second copy of the same reader, told apart by nothing.
entry! {
    State;
    primary { request_in, reply_out }
    inputs { reply_in = 1, body_in = 2 }
    outputs { publish_out = 1, plain_out = 2 }
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
    let mut parts: [&[u8]; 6] = [&[], &[], &[], &[], &[], &[]];
    let taken = wire::fields(held_payload.get(..length).unwrap_or(&[]), &mut parts);
    // How many fields each method is: the shape of a call, checked rather
    // than assumed. `send` is authority, scheme, method, path, and then
    // the header block and the body, either of which may be left off.
    let shape = match record.binding {
        METHOD_ORIGINS => (0, 0),
        METHOD_STATUS | METHOD_HEADERS | METHOD_CLOSE => (1, 1),
        METHOD_READ => (1, 2),
        METHOD_SEND => (4, 6),
        _ => (0, usize::MAX),
    };
    if taken < shape.0 || taken > shape.1 {
        refuse(state, record.request, record.trace, Cause::Malformed);
        return;
    }

    if record.binding == METHOD_ORIGINS {
        // The allow-list as the graph wrote it, so a surface can say why a
        // request was refused, and can pick an origin for a program that
        // wrote a bare path. Empty when there is no policy.
        let length = state.origins_length.min(ORIGINS_BYTES);
        let mut text = [0u8; ORIGINS_BYTES];
        copy_into(
            text.get_mut(..length).unwrap_or(&mut []),
            state.origins.get(..length).unwrap_or(&[]),
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

    if record.binding == METHOD_NARROW {
        let supplied = state
            .payload
            .get(..state.payload_length.min(ORIGINS_BYTES))
            .unwrap_or(&[]);
        let existing_len = state.origins_length.min(ORIGINS_BYTES);
        let mut narrowed = [0u8; ORIGINS_BYTES];
        let mut at = 0usize;
        // The intersection, written out as the same comma-separated text the
        // parameter carries. An existing empty list is no policy, so the
        // supplied list becomes the policy entire.
        for entry in supplied.split(|&c| c == b',') {
            let entry = trim_spaces(entry);
            if entry.is_empty() {
                continue;
            }
            let admitted = existing_len == 0
                || granted(state.origins.get(..existing_len).unwrap_or(&[]), entry);
            if !admitted {
                continue;
            }
            if at > 0 && at < narrowed.len() {
                narrowed[at] = b',';
                at += 1;
            }
            if at + entry.len() > narrowed.len() {
                refuse(state, record.request, record.trace, Cause::Malformed);
                return;
            }
            if !copy_into(
                narrowed.get_mut(at..at + entry.len()).unwrap_or(&mut []),
                entry,
            ) {
                refuse(state, record.request, record.trace, Cause::Malformed);
                return;
            }
            at += entry.len();
        }
        // A narrowing that admits nothing is still a narrowing: it is
        // recorded as one empty entry, which no authority equals, so every
        // request is refused rather than every request being allowed.
        if at == 0 && !supplied.is_empty() {
            narrowed[0] = b' ';
            at = 1;
        }
        copy_into(
            state.origins.get_mut(..at).unwrap_or(&mut []),
            narrowed.get(..at).unwrap_or(&[]),
        );
        state.origins_length = at;
        fulfil(state, record.request, record.trace, Answer::Payload(0), &[]);
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
        let authority = parts[0];
        let scheme = parts[1];
        let Some(verb) = verb_of(parts[2]) else {
            refuse(state, record.request, record.trace, Cause::Malformed);
            return;
        };
        let path = parts[3];
        let headers = if taken > 4 { parts[4] } else { &[] };
        let body = if taken > 5 { parts[5] } else { &[] };
        // An authority is one a provider can dial and the record can carry,
        // or the call is malformed: nothing here guesses at a name.
        if path.is_empty()
            || !wire_http::authority_ok(authority)
            || proto::Target::parse(authority).is_none()
        {
            refuse(state, record.request, record.trace, Cause::Malformed);
            return;
        }
        // The scheme picks the leg. An empty scheme is a program that wrote
        // a bare path and left the choice to the deployment: the TLS leg
        // when the graph wired one, the cleartext leg otherwise.
        let port = match scheme {
            b"https" => state.publish_out,
            b"http" => state.plain_out,
            b"" if state.publish_out >= 0 => state.publish_out,
            b"" => state.plain_out,
            _ => {
                refuse(state, record.request, record.trace, Cause::Malformed);
                return;
            }
        };
        // The policy, and the only place it lives: an authority the
        // deployment did not admit goes nowhere.
        let origins_length = state.origins_length.min(ORIGINS_BYTES);
        if !granted(
            state.origins.get(..origins_length).unwrap_or(&[]),
            authority,
        ) {
            refuse(state, record.request, record.trace, Cause::Denied);
            return;
        }
        // No leg for this scheme is wired: the deployment provided no way
        // to carry it, which is a refusal and not a fallback to the other.
        if port < 0 {
            refuse(state, record.request, record.trace, Cause::Denied);
            return;
        }
        // The record the provider takes, composed by the core that owns it,
        // naming the authority so the open provider behind either leg dials
        // it. The extended form asks to be answered with the whole response
        // rather than its body alone, which is what lets a program read a
        // status and a header block at all.
        //
        // An authority that names no port is completed here, because the
        // port a scheme implies is known here and nowhere below: the
        // provider's own default is its protocol's, which is the cleartext
        // one. The leg the scheme picked is what says which port that is.
        let mut dialled = [0u8; wire_http::AUTHORITY_MAX + 4];
        let authority = match (port == state.publish_out, port_of(authority)) {
            (true, None) => {
                let n = authority.len();
                if n + 4 > dialled.len() {
                    refuse(state, record.request, record.trace, Cause::Malformed);
                    return;
                }
                if !copy_into(dialled.get_mut(..n).unwrap_or(&mut []), authority)
                    || !copy_into(dialled.get_mut(n..n + 4).unwrap_or(&mut []), b":443")
                {
                    refuse(state, record.request, record.trace, Cause::Malformed);
                    return;
                }
                dialled.get(..n + 4).unwrap_or(&[])
            }
            _ => authority,
        };
        let mut payload = [0u8; exchange::PAYLOAD_MAX];
        let Some(at) =
            wire_http::write_request_to(verb, true, path, headers, body, authority, &mut payload)
        else {
            refuse(state, record.request, record.trace, Cause::Malformed);
            return;
        };
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
        state.publish_port = port;
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
    // Read by the core that owns the layout, so the status and the block are
    // taken from where the provider actually put them.
    let Some((status, headers)) = wire_http::parse_reply_head(reply.payload) else {
        return;
    };
    state.response.status = u32::from(status);
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
    let Some((bytes, _)) = wire_http::parse_chunk(frame) else {
        // Still arriving: nothing to make room for yet.
        return true;
    };
    bytes.is_empty() || BODY_BYTES - state.response.filled >= bytes.len()
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
        while state.body_filled >= wire_http::CHUNK_HEAD {
            let staged = state.body_frame.get(..state.body_filled).unwrap_or(&[]);
            // Read by the core: `None` while a chunk is still arriving, which
            // is the one case a reader must not guess at.
            let Some((bytes, total)) = wire_http::parse_chunk(staged) else {
                if state.body_filled >= state.body_frame.len() {
                    // Longer than this adapter can stage, so it will never
                    // complete. Dropping what is staged is the only way on.
                    state.body_filled = 0;
                }
                break;
            };
            let carried = bytes.len();
            // A chunk the body cannot hold is left where it is and taken
            // again once the program has read what is already here, so the
            // stream stops rather than losing the middle of itself.
            if !room_for_body(state, staged) {
                break;
            }
            let mut chunk = [0u8; FRAME_BYTES];
            copy_into(chunk.get_mut(..carried).unwrap_or(&mut []), bytes);
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
        state.publish_port,
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
