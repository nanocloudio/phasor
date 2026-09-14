//! A stream connection, as a capability rather than a network.
//!
//! An isolate has no network. A deployment that wants a program to open a
//! connection wires this adapter behind the router, and the program reaches
//! the network only through the binding it was granted. `connect` names an
//! authority, `host[:port]`, or names nothing and takes the one the graph's
//! `authority` parameter holds. The name is one fact: it goes down to the
//! stack in the connect record itself, the stack resolves it, and whatever
//! sits between — `tls`, say — reads the same bytes for what it verifies.
//!
//! The confinement is `origins`: the deployment's comma-separated allow-list
//! of authorities a program may name. One outside it is refused here, and
//! nothing is dialled. Empty is no policy, which is what an applet granted
//! `net` means. The graph's own `authority` is admitted by being the
//! graph's; the list is for what the program writes.
//!
//! There is no resolver here, and no address arithmetic: an authority is
//! parsed by the stream contract's own reader, so what this module admits is
//! exactly what a provider can dial. Bytes cross as payloads, and a
//! connection crosses as a handle the engine checks. What this module holds
//! — the connection identifier the stack gave it, the buffered bytes, the
//! pending calls — never crosses at all.

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
/// The longest authority a connection may be opened to: a name the stream
/// contract carries, with its port.
const AUTHORITY_BYTES: usize = proto::MAX_NAME_LEN + 8;
/// Bytes the comma-separated `origins` allow-list may hold.
const ORIGINS_BYTES: usize = 512;

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
    announced: bool,
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

    /// The authority a `connect` that names none goes to, `host[:port]`.
    /// Empty means the graph named none, and a program must.
    authority: [u8; AUTHORITY_BYTES],
    authority_len: usize,
    /// The authorities a program may name, comma-separated. Empty is no
    /// policy.
    origins: [u8; ORIGINS_BYTES],
    origins_len: usize,
    /// The authority the last connection was opened to, which is what
    /// `endpoint` answers once one has been. Before that it answers the
    /// graph's.
    endpoint: [u8; AUTHORITY_BYTES],
    endpoint_len: usize,
    /// The authority of the dial in flight, taken into `endpoint` when the
    /// stack answers it.
    dialled: [u8; AUTHORITY_BYTES],
    dialled_len: usize,
    /// Connections this adapter will open before it refuses.
    quota: u32,
    phase: u8,
}

define_params! {
    State;

    // 1, 2: retired.
    3, quota, u32, 0
        => |s, d, len| { s.quota = p_u32(d, len, 0, 0); };
    4, authority, str, 0
        => |s, d, len| {
            // A prefix of an authority is a different host, so one that does
            // not fit is dropped and every dial that would have used it is
            // refused as malformed.
            let taken = if len > AUTHORITY_BYTES { 0 } else { len };
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
    5, origins, str, 0
        => |s, d, len| {
            // Held whole or not at all, and never as nothing: an empty list
            // is no policy, so a list too long to store becomes one empty
            // entry, which no authority equals.
            if len > ORIGINS_BYTES {
                s.origins[0] = b',';
                s.origins_len = 1;
                return;
            }
            let taken = len;
            s.origins_len = taken;
            if taken > 0 {
                // SAFETY: as above.
                unsafe {
                    core::ptr::copy_nonoverlapping(d, s.origins.as_mut_ptr(), taken);
                }
            }
        };
}

/// Whether two authorities are the same one. A host is case-insensitive
/// and a port is digits, so an ASCII fold is the whole comparison.
fn same_authority(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
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
fn serve(state: &mut State, sys: &SyscallTable, record: &CallRecord) {
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
        METHOD_ENDPOINT => (0, 0),
        // The authority is the caller's to leave out, in which case the
        // graph's stands.
        METHOD_CONNECT => (0, 1),
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
            // Where the last connection went, by the name it was opened to;
            // before any has been, where a `connect` naming nothing would
            // go. A façade reads it to say what a program reached.
            let (source, at) = if state.endpoint_len > 0 {
                (&state.endpoint, state.endpoint_len.min(AUTHORITY_BYTES))
            } else {
                (&state.authority, state.authority_len.min(AUTHORITY_BYTES))
            };
            let mut text = [0u8; AUTHORITY_BYTES];
            copy_into(
                text.get_mut(..at).unwrap_or(&mut []),
                source.get(..at).unwrap_or(&[]),
            );
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
            // The authority: the program's when it named one, checked
            // against the allow-list; the graph's otherwise, admitted by
            // being the graph's.
            let named = taken > 0 && !first.is_empty();
            let mut authority = [0u8; AUTHORITY_BYTES];
            let authority_len = if named {
                first.len()
            } else {
                state.authority_len.min(AUTHORITY_BYTES)
            };
            if authority_len == 0
                || authority_len > AUTHORITY_BYTES
                || !copy_into(
                    authority.get_mut(..authority_len).unwrap_or(&mut []),
                    if named {
                        first
                    } else {
                        state.authority.get(..authority_len).unwrap_or(&[])
                    },
                )
            {
                refuse(state, record.request, record.trace, Cause::Malformed);
                return;
            }
            let authority = authority.get(..authority_len).unwrap_or(&[]);
            // A raw connection has no protocol, so no default port: the
            // authority names one or the call is malformed.
            let Some((target, Some(port))) = proto::Target::parse(authority) else {
                refuse(state, record.request, record.trace, Cause::Malformed);
                return;
            };
            let origins_len = state.origins_len.min(ORIGINS_BYTES);
            if named && !granted(state.origins.get(..origins_len).unwrap_or(&[]), authority) {
                refuse(state, record.request, record.trace, Cause::Denied);
                return;
            }
            // The dial carries this adapter's tag, so its answer can be
            // told from every other consumer's on a shared lane.
            //
            // SAFETY: `sys` is the table the loader handed this module,
            // live for its lifetime.
            let tag = unsafe { dev_requester_tag(sys) };
            let mut payload = [0u8; proto::CONNECT_TO_MAX];
            let length = proto::write_connect_to(
                &mut payload,
                proto::SOCK_TYPE_STREAM,
                port,
                &target,
                Some(tag),
            );
            if length == 0 {
                refuse(state, record.request, record.trace, Cause::Malformed);
                return;
            }
            if !command(
                state,
                proto::CMD_CONNECT_TO,
                payload.get(..length).unwrap_or(&[]),
            ) || !hold(state, record, usize::MAX, 0)
            {
                refuse(state, record.request, record.trace, Cause::Busy);
                return;
            }
            copy_into(
                state.dialled.get_mut(..authority_len).unwrap_or(&mut []),
                authority,
            );
            state.dialled_len = authority_len;
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
/// Whether the connection a staged frame is for can hold what it carries.
///
/// Only data frames are held back: everything else is bookkeeping that must
/// arrive whether or not the program is reading.
fn room_for(state: &State, frame: &[u8]) -> bool {
    let Some(&kind) = frame.first() else {
        return true;
    };
    if kind != proto::MSG_DATA {
        return true;
    }
    let payload = frame.get(proto::FRAME_HDR..).unwrap_or(&[]);
    if payload.len() < proto::CONN_ID_LEN {
        return true;
    }
    let id = proto::conn_id(payload);
    let carried = payload.len() - proto::CONN_ID_LEN;
    match state
        .connections
        .iter()
        .find(|held| held.live && held.id == id)
    {
        // A frame for a connection this adapter does not hold is dropped by
        // `apply` either way; holding it back would stall the stream.
        None => true,
        Some(connection) => BUFFER_BYTES - connection.filled >= carried,
    }
}

fn apply(state: &mut State, sys: &SyscallTable, kind: u8, payload: &[u8]) {
    // The stack's outbound lane is shared: every adapter wired to it sees
    // every connection opened on it, including ones opened by another module
    // entirely. An answer to a dial carries the dialler's tag, and one that
    // carries another module's is not this adapter's to act on. An untagged
    // answer is taken as addressed to whoever is waiting.
    //
    // SAFETY: `sys` is the table the loader handed this module, live for
    // its lifetime.
    let mine = unsafe { dev_requester_tag(sys) };
    match kind {
        proto::MSG_CONNECTED => {
            if payload.len() < proto::CONN_ID_LEN {
                return;
            }
            let (_, tag) = proto::connected_parts(payload);
            if tag != proto::REQUESTER_TAG_NONE && tag != mine {
                return;
            }
            // A connection this adapter did not ask for is not its own, and
            // adopting it is worse than untidy — nothing here will ever read
            // that stream, so its buffer fills, and a full buffer stops the
            // whole lane for the module the connection does belong to.
            let mut index = 0usize;
            let waiting = loop {
                if index >= PENDING {
                    return;
                }
                let held = state.held[index];
                if held.live && held.method == METHOD_CONNECT {
                    break index;
                }
                index += 1;
            };
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
            // over this slot; the stack's own identifier stays here, and the
            // authority it was opened to becomes what `endpoint` answers.
            let dialled_len = state.dialled_len.min(AUTHORITY_BYTES);
            let mut dialled = [0u8; AUTHORITY_BYTES];
            copy_into(
                dialled.get_mut(..dialled_len).unwrap_or(&mut []),
                state.dialled.get(..dialled_len).unwrap_or(&[]),
            );
            copy_into(
                state.endpoint.get_mut(..dialled_len).unwrap_or(&mut []),
                dialled.get(..dialled_len).unwrap_or(&[]),
            );
            state.endpoint_len = dialled_len;
            let held = state.held[waiting];
            state.held[waiting] = Held::EMPTY;
            fulfil(
                state,
                held.request,
                held.trace,
                Answer::Resource(slot as u64),
                &[],
            );
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
                // The frame was admitted only because the room was there, so
                // this copies all of it or none: a short copy here would be
                // the silent loss the admission exists to prevent.
                let at = connection.filled;
                if copy_into(
                    connection
                        .buffer
                        .get_mut(at..at + data.len())
                        .unwrap_or(&mut []),
                    data,
                ) {
                    connection.filled = at + data.len();
                }
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
            if payload.len() < proto::CONN_ID_LEN + 1 {
                return;
            }
            let (_, _, tag) = proto::error_parts(payload);
            if tag != proto::REQUESTER_TAG_NONE && tag != mine {
                return;
            }
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
    announce_ready!(state);
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
            // A connection with no room for this frame is not a connection
            // that should lose it. The frame stays where it is and is taken
            // again once the program has read what is already buffered, which
            // holds the stream still rather than dropping the middle of it.
            if !room_for(state, state.net_frame.get(..total).unwrap_or(&[])) {
                break;
            }
            // One copy, because `apply` takes the whole adapter while the
            // payload it is given lives inside it.
            copy_into(
                payload.get_mut(..length).unwrap_or(&mut []),
                state.net_frame.get(proto::FRAME_HDR..total).unwrap_or(&[]),
            );
            apply(state, syscalls, kind, payload.get(..length).unwrap_or(&[]));
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
                serve(state, syscalls, &record);
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
