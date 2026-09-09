//! A stream connection, as a capability rather than a network.
//!
//! An isolate has no network. A deployment that wants a program to reach one
//! endpoint wires this adapter behind the router and names that endpoint in
//! the graph; the program reaches it only through the binding it was granted,
//! and only that endpoint. A program cannot name an address at all: `connect`
//! takes nothing, because where it goes is the deployment's to decide and not
//! the program's to ask. A deployment that wants two endpoints wires two
//! adapters, and each is granted separately.
//!
//! That is the whole of the confinement, and it is simple on purpose. There
//! is no resolver here, so there is no name a program could steer; there is
//! no address in a payload, so there is nothing to validate; and the
//! endpoint is visible in the graph, where a deployment can read it.
//!
//! Bytes cross as payloads, and a connection crosses as a handle the engine
//! checks. What this module holds — the connection identifier the stack gave
//! it, the buffered bytes, the pending calls — never crosses at all.

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

use abi::contracts::net::net_proto as proto;
use binding::{
    Answer, CallRecord, Cause, CompletionRecord, Disposition, CALL_FRAME, COMPLETION_FRAME,
};

/// Connections this adapter may hold at once.
const CONNECTIONS: usize = 4;
/// Bytes one call or completion may carry.
const PAYLOAD_BYTES: usize = 8 * 1024;
/// Bytes buffered per connection before the stack is told to wait.
const BUFFER_BYTES: usize = 32 * 1024;
/// Completions staged for the reply port.
const STAGE_BYTES: usize = 2 * (COMPLETION_FRAME + PAYLOAD_BYTES);
/// Commands staged for the network port.
const COMMAND_BYTES: usize = 4 * (proto::FRAME_HDR + proto::MAX_CMD_DATA);
/// The largest network frame this adapter reads in one piece.
/// Room for whatever the port can deliver, rather than for the largest frame
/// the contract describes. A producer that sends a bigger one is still within
/// what the channel accepts, and an adapter that sized itself from the
/// document rather than from the channel loses framing at the first frame it
/// did not expect — which is a hang, not a diagnosis.
const NET_FRAME: usize = 16 * 1024;
/// Calls held until the stack answers.
const PENDING: usize = 8;
/// The longest authority the graph may name the endpoint by.
const AUTHORITY_BYTES: usize = 64;

/// The methods the interface offers.
const METHOD_CONNECT: u32 = 0;
const METHOD_SEND: u32 = 1;
const METHOD_RECEIVE: u32 = 2;
const METHOD_CLOSE: u32 = 3;
const METHOD_ENDPOINT: u32 = 4;

/// One connection the stack opened for this adapter.
#[derive(Clone, Copy)]
struct Connection {
    /// The stack's own identifier, which never crosses the boundary.
    id: u16,
    buffer: [u8; BUFFER_BYTES],
    filled: usize,
    /// Whether the far end has gone.
    closed: bool,
    live: bool,
}

impl Connection {
    const EMPTY: Self = Self {
        id: 0,
        buffer: [0; BUFFER_BYTES],
        filled: 0,
        closed: false,
        live: false,
    };
}

/// One call held until the stack answers it.
#[derive(Clone, Copy)]
struct Held {
    request: u64,
    trace: u64,
    method: u32,
    /// The connection the call is about, for everything but `connect`.
    slot: usize,
    /// Bytes a `receive` may answer with.
    wanted: usize,
    live: bool,
}

impl Held {
    const EMPTY: Self = Self {
        request: 0,
        trace: 0,
        method: 0,
        slot: 0,
        wanted: 0,
        live: false,
    };
}

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    request_in: i32,
    reply_out: i32,
    net_out: i32,
    net_in: i32,

    request: [u8; CALL_FRAME],
    filled: usize,
    payload: [u8; PAYLOAD_BYTES],
    payload_filled: usize,
    frame_ready: bool,
    payload_length: usize,

    replies: [u8; STAGE_BYTES],
    staged: usize,
    written: usize,

    commands: [u8; COMMAND_BYTES],
    command_staged: usize,
    command_written: usize,

    net_frame: [u8; NET_FRAME],
    net_filled: usize,

    connections: [Connection; CONNECTIONS],
    held: [Held; PENDING],
    answered: u64,

    /// The endpoint this adapter reaches, which the graph names and the
    /// program cannot. Held as the address and port the stack takes.
    address: u32,
    port: u32,
    /// What that endpoint answers to, when the deployment says. An address
    /// is where to go; a name is what the far end is called, and a program
    /// naming a URL has only the latter. Empty means the graph named none,
    /// and the endpoint is then its address and port.
    authority: [u8; AUTHORITY_BYTES],
    authority_len: usize,
    /// Connections this adapter will open before it refuses.
    quota: u32,
    phase: u8,
}

define_params! {
    State;

    1, address, u32, 2130706433
        => |s, d, len| { s.address = p_u32(d, len, 0, 2130706433); };
    2, port, u32, 80
        => |s, d, len| { s.port = p_u32(d, len, 0, 80); };
    3, quota, u32, 0
        => |s, d, len| { s.quota = p_u32(d, len, 0, 0); };
    4, authority, str, 0
        => |s, d, len| {
            let taken = if len > AUTHORITY_BYTES { AUTHORITY_BYTES } else { len };
            s.authority_len = taken;
            if taken > 0 {
                // SAFETY: the params reader hands a pointer valid for `len`
                // bytes, and `taken` is no larger than either that or the
                // destination.
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
        value = value
            .checked_mul(10)?
            .checked_add(usize::from(byte - b'0'))?;
    }
    Some(value)
}

/// Stage one network command.
fn command(state: &mut State, kind: u8, payload: &[u8]) -> bool {
    let at = state.command_staged;
    let total = proto::FRAME_HDR + payload.len();
    let Some(slot) = state.commands.get_mut(at..at + total) else {
        return false;
    };
    slot[0] = kind;
    let length = u16::try_from(payload.len()).unwrap_or(0);
    slot[1..3].copy_from_slice(&length.to_le_bytes());
    if !copy_into(slot.get_mut(proto::FRAME_HDR..).unwrap_or(&mut []), payload) {
        return false;
    }
    state.command_staged = at + total;
    true
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

/// Hold a call until the stack answers it.
fn hold(state: &mut State, record: &CallRecord, slot: usize, wanted: usize) -> bool {
    let Some(index) = state.held.iter().position(|held| !held.live) else {
        return false;
    };
    state.held[index] = Held {
        request: record.request,
        trace: record.trace,
        method: record.binding,
        slot,
        wanted,
        live: true,
    };
    true
}

/// Answer a held `receive` from what its connection has buffered.
fn serve_receives(state: &mut State) {
    let mut index = 0usize;
    while index < PENDING {
        let held = state.held[index];
        if !held.live || held.method != METHOD_RECEIVE {
            index += 1;
            continue;
        }
        let Some(connection) = state.connections.get(held.slot) else {
            index += 1;
            continue;
        };
        if !connection.live {
            index += 1;
            continue;
        }
        if connection.filled == 0 && !connection.closed {
            index += 1;
            continue;
        }
        if state.staged + COMPLETION_FRAME + PAYLOAD_BYTES > state.replies.len() {
            break;
        }
        // A closed connection with nothing buffered answers with nothing,
        // which is how a reader learns the far end has gone.
        let taken = connection.filled.min(held.wanted).min(PAYLOAD_BYTES);
        let mut bytes = [0u8; PAYLOAD_BYTES];
        copy_into(
            bytes.get_mut(..taken).unwrap_or(&mut []),
            connection.buffer.get(..taken).unwrap_or(&[]),
        );
        state.held[index] = Held::EMPTY;
        if let Some(connection) = state.connections.get_mut(held.slot) {
            // What is left moves down over what was taken, a byte at a time
            // rather than through a buffer of its own: the buffer is large
            // enough that a copy of it does not belong on the stack.
            let rest = connection.filled - taken;
            let mut at = 0usize;
            while at < rest {
                connection.buffer[at] = connection.buffer[taken + at];
                at += 1;
            }
            connection.filled = rest;
        }
        fulfil(
            state,
            held.request,
            held.trace,
            Answer::Payload(u32::try_from(taken).unwrap_or(0)),
            bytes.get(..taken).unwrap_or(&[]),
        );
        index += 1;
    }
}

/// Answer one call, or hold it until the stack can.
fn serve(state: &mut State, record: &CallRecord) {
    if state.quota != 0
        && record.binding == METHOD_CONNECT
        && state.answered >= u64::from(state.quota)
    {
        refuse(state, record.request, record.trace, Cause::Denied);
        return;
    }
    let mut held_payload = [0u8; PAYLOAD_BYTES];
    let length = state.payload_length.min(PAYLOAD_BYTES);
    copy_into(
        held_payload.get_mut(..length).unwrap_or(&mut []),
        state.payload.get(..length).unwrap_or(&[]),
    );
    let mut parts: [&[u8]; 2] = [&[], &[]];
    let taken = wire::fields(held_payload.get(..length).unwrap_or(&[]), &mut parts);
    // How many fields each method is: the shape of a call, checked rather
    // than assumed. The frame says where every field ends, so a call that
    // does not have the fields this method takes is refused here instead of
    // being read as though it did.
    let shape = match record.binding {
        METHOD_CONNECT | METHOD_ENDPOINT => (0, 0),
        METHOD_CLOSE => (1, 1),
        // The length a read may answer with is the caller's to leave out.
        METHOD_RECEIVE => (1, 2),
        METHOD_SEND => (2, 2),
        _ => (0, usize::MAX),
    };
    if taken < shape.0 || taken > shape.1 {
        refuse(state, record.request, record.trace, Cause::Malformed);
        return;
    }
    let first = if taken > 0 { parts[0] } else { &[][..] };
    let rest = if taken > 1 { parts[1] } else { &[][..] };

    match record.binding {
        METHOD_ENDPOINT => {
            // What the graph wired, so a façade can tell whether a name it
            // was given is the one this capability reaches — and so a
            // request carries an authority the far end recognises. A named
            // endpoint answers by its name; an unnamed one by its address.
            let mut text = [0u8; AUTHORITY_BYTES];
            let mut at = 0usize;
            if state.authority_len > 0 {
                at = state.authority_len.min(text.len());
                copy_into(
                    text.get_mut(..at).unwrap_or(&mut []),
                    state.authority.get(..at).unwrap_or(&[]),
                );
                fulfil(
                    state,
                    record.request,
                    record.trace,
                    Answer::Payload(u32::try_from(at).unwrap_or(0)),
                    text.get(..at).unwrap_or(&[]),
                );
                return;
            }
            let octets = state.address.to_be_bytes();
            for (index, octet) in octets.iter().enumerate() {
                if index > 0 {
                    at += text::put_ascii(text.get_mut(at..).unwrap_or(&mut []), b".");
                }
                at += text::put_u32(text.get_mut(at..).unwrap_or(&mut []), u32::from(*octet));
            }
            at += text::put_ascii(text.get_mut(at..).unwrap_or(&mut []), b":");
            at += text::put_u32(text.get_mut(at..).unwrap_or(&mut []), state.port);
            fulfil(
                state,
                record.request,
                record.trace,
                Answer::Payload(u32::try_from(at).unwrap_or(0)),
                text.get(..at).unwrap_or(&[]),
            );
        }
        METHOD_CONNECT => {
            if state.connections.iter().all(|held| held.live) {
                refuse(state, record.request, record.trace, Cause::Busy);
                return;
            }
            let mut payload = [0u8; 8];
            payload[0] = proto::SOCK_TYPE_STREAM;
            payload[1..5].copy_from_slice(&state.address.to_le_bytes());
            let port = u16::try_from(state.port).unwrap_or(80);
            payload[5..7].copy_from_slice(&port.to_le_bytes());
            if !command(state, proto::CMD_CONNECT, &payload[..7])
                || !hold(state, record, usize::MAX, 0)
            {
                refuse(state, record.request, record.trace, Cause::Busy);
            }
        }
        METHOD_SEND => {
            let Some(slot) = digits(first).filter(|slot| *slot < CONNECTIONS) else {
                refuse(state, record.request, record.trace, Cause::Malformed);
                return;
            };
            let Some(connection) = state.connections.get(slot).filter(|held| held.live) else {
                refuse(state, record.request, record.trace, Cause::Malformed);
                return;
            };
            let id = connection.id;
            if rest.len() > proto::MAX_CMD_DATA - proto::CONN_ID_LEN {
                refuse(state, record.request, record.trace, Cause::Malformed);
                return;
            }
            let mut payload = [0u8; proto::MAX_CMD_DATA];
            payload[0..2].copy_from_slice(&id.to_le_bytes());
            let total = proto::CONN_ID_LEN + rest.len();
            if !copy_into(
                payload
                    .get_mut(proto::CONN_ID_LEN..total)
                    .unwrap_or(&mut []),
                rest,
            ) || !command(state, proto::CMD_SEND, payload.get(..total).unwrap_or(&[]))
            {
                refuse(state, record.request, record.trace, Cause::Busy);
                return;
            }
            fulfil(
                state,
                record.request,
                record.trace,
                Answer::Number(rest.len() as f64),
                &[],
            );
        }
        METHOD_RECEIVE => {
            let Some(slot) = digits(first).filter(|slot| *slot < CONNECTIONS) else {
                refuse(state, record.request, record.trace, Cause::Malformed);
                return;
            };
            if state.connections.get(slot).is_none_or(|held| !held.live) {
                refuse(state, record.request, record.trace, Cause::Malformed);
                return;
            }
            let wanted = digits(rest).unwrap_or(PAYLOAD_BYTES).min(PAYLOAD_BYTES);
            if !hold(state, record, slot, wanted) {
                refuse(state, record.request, record.trace, Cause::Busy);
                return;
            }
            serve_receives(state);
        }
        METHOD_CLOSE => {
            let Some(slot) = digits(first).filter(|slot| *slot < CONNECTIONS) else {
                refuse(state, record.request, record.trace, Cause::Malformed);
                return;
            };
            let Some(connection) = state.connections.get(slot).filter(|held| held.live) else {
                refuse(state, record.request, record.trace, Cause::Malformed);
                return;
            };
            let id = connection.id;
            let mut payload = [0u8; 2];
            payload.copy_from_slice(&id.to_le_bytes());
            command(state, proto::CMD_CLOSE, &payload);
            if let Some(connection) = state.connections.get_mut(slot) {
                *connection = Connection::EMPTY;
            }
            fulfil(
                state,
                record.request,
                record.trace,
                Answer::Number(1.0),
                &[],
            );
        }
        _ => refuse(state, record.request, record.trace, Cause::Malformed),
    }
}

/// Apply one frame the network stack produced.
fn apply(state: &mut State, kind: u8, payload: &[u8]) {
    match kind {
        proto::MSG_CONNECTED => {
            if payload.len() < proto::CONN_ID_LEN {
                return;
            }
            let id = proto::conn_id(payload);
            let Some(slot) = state.connections.iter().position(|held| !held.live) else {
                return;
            };
            state.connections[slot] = Connection {
                id,
                buffer: [0; BUFFER_BYTES],
                filled: 0,
                closed: false,
                live: true,
            };
            // The call that asked for a connection is answered with a handle
            // over this slot; the stack's own identifier stays here.
            let mut index = 0usize;
            while index < PENDING {
                let held = state.held[index];
                if held.live && held.method == METHOD_CONNECT {
                    state.held[index] = Held::EMPTY;
                    fulfil(
                        state,
                        held.request,
                        held.trace,
                        Answer::Resource(slot as u64),
                        &[],
                    );
                    return;
                }
                index += 1;
            }
        }
        proto::MSG_DATA => {
            if payload.len() < proto::CONN_ID_LEN {
                return;
            }
            let id = proto::conn_id(payload);
            let data = payload.get(proto::CONN_ID_LEN..).unwrap_or(&[]);
            let Some(slot) = state
                .connections
                .iter()
                .position(|held| held.live && held.id == id)
            else {
                return;
            };
            if let Some(connection) = state.connections.get_mut(slot) {
                let at = connection.filled;
                let taken = data.len().min(BUFFER_BYTES - at);
                copy_into(
                    connection.buffer.get_mut(at..at + taken).unwrap_or(&mut []),
                    data.get(..taken).unwrap_or(&[]),
                );
                connection.filled = at + taken;
            }
        }
        proto::MSG_CLOSED => {
            if payload.len() < proto::CONN_ID_LEN {
                return;
            }
            let id = proto::conn_id(payload);
            if let Some(slot) = state
                .connections
                .iter()
                .position(|held| held.live && held.id == id)
            {
                if let Some(connection) = state.connections.get_mut(slot) {
                    connection.closed = true;
                }
            }
        }
        proto::MSG_ERROR => {
            // Whatever was waiting is told, rather than left waiting.
            let mut index = 0usize;
            while index < PENDING {
                let held = state.held[index];
                if held.live {
                    state.held[index] = Held::EMPTY;
                    refuse(state, held.request, held.trace, Cause::Unavailable);
                }
                index += 1;
            }
        }
        _ => {}
    }
}

entry! {
    State;
    primary { request_in, reply_out }
    inputs { net_in = 1 }
    outputs { net_out = 1 }
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

    // What the stack produced, one frame at a time.
    if state.net_in >= 0 {
        let mut header = [0u8; proto::FRAME_HDR];
        let mut taken = state.net_filled.min(proto::FRAME_HDR);
        copy_into(
            header.get_mut(..taken).unwrap_or(&mut []),
            state.net_frame.get(..taken).unwrap_or(&[]),
        );
        let read = wire::read_available(
            syscalls,
            state.net_in,
            state
                .net_frame
                .get_mut(state.net_filled..)
                .unwrap_or(&mut []),
        );
        state.net_filled += read;
        let mut payload = [0u8; NET_FRAME];
        while state.net_filled >= proto::FRAME_HDR {
            let kind = state.net_frame[0];
            let length = usize::from(u16::from_le_bytes([state.net_frame[1], state.net_frame[2]]));
            let total = proto::FRAME_HDR + length;
            if total > state.net_frame.len() {
                // Not a frame this adapter can hold: drop what is staged
                // rather than misreading it.
                state.net_filled = 0;
                break;
            }
            if state.net_filled < total {
                break;
            }
            // One copy, because `apply` takes the whole adapter while the
            // payload it is given lives inside it.
            copy_into(
                payload.get_mut(..length).unwrap_or(&mut []),
                state.net_frame.get(proto::FRAME_HDR..total).unwrap_or(&[]),
            );
            apply(state, kind, payload.get(..length).unwrap_or(&[]));
            // What is left moves down over the frame just taken, in place:
            // a second buffer of this size does not belong on the stack.
            let rest = state.net_filled - total;
            let mut at = 0usize;
            while at < rest {
                state.net_frame[at] = state.net_frame[total + at];
                at += 1;
            }
            state.net_filled = rest;
        }
        taken = 0;
        let _ = taken;
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
                serve(state, &record);
            }
            state.payload_filled = 0;
            state.payload_length = 0;
        }
    }

    serve_receives(state);

    if state.net_out >= 0 {
        wire::push_staged(
            syscalls,
            state.net_out,
            &state.commands,
            &mut state.command_staged,
            &mut state.command_written,
        );
    }
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
