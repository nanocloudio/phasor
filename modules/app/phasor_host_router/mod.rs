//! The router between an isolate's calls and an adapter that serves them.
//!
//! It admits a call by its binding index alone, correlates the answer with the
//! call by request identifier and trace, and bounds how many calls may be
//! outstanding. It never drops or reorders a completion: a call that cannot be
//! forwarded is answered here, with the reason, rather than left unanswered.
//!
//! A call's arguments follow its frame as bytes, and an answer's bytes follow
//! its completion the same way. The router carries both behind the frame they
//! belong to and reads neither: what they mean is between the program and the
//! adapter, and a frame is not forwarded until every byte behind it is here.
//!
//! A binding is an index the isolate admitted: `host` first, then one per
//! grant in the order the deployment stated them. An adapter knows its
//! operations by method number instead, which is the register's. The router
//! is where the one becomes the other: it is told the same grants and which
//! interface the adapter behind it serves, forwards a call on a member of
//! that interface under the member's method, and refuses a call on any other
//! interface as denied, because nothing behind it could answer. `host` is
//! forwarded as method 0, the raw call a graph with no grants makes.
//!
//! It holds no address and no credential. Which adapter serves the binding is
//! the graph's wiring, not this module's knowledge.
//!
//! A router has legs, one per interface it was told it serves, in the order
//! the graph listed them: leg one is `request_out`/`reply_in`, leg two
//! `request_out_b`/`reply_in_b`. A program granted a clock and an entropy
//! source needs both behind one isolate, and the isolate has one call port.
//! A graph that lists one interface wires only the first leg, as before.

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
#[path = "../../common/register.rs"]
mod register;
#[path = "../../common/softfloat.rs"]
mod softfloat;
#[path = "../../common/value.rs"]
mod value;
#[path = "../../common/wire.rs"]
mod wire;

use binding::{
    Answer, CallRecord, Cause, CompletionRecord, Disposition, CALL_FRAME, COMPLETION_FRAME,
};

/// The binding every isolate admits first, forwarded as the first method.
const HOST_BINDING: u32 = 0;
/// Bindings an isolate admits: `host` and one per grant. An isolate stops
/// admitting at its own table's end, so a call above this names no binding
/// there and is refused here rather than counted past it.
const MAX_BINDINGS: u32 = 17;
/// Bytes of grants and of the interface list a graph may state, the same
/// bounds the isolate holds them under.
use register::GRANTS_BYTES;
const INTERFACES_BYTES: usize = register::MAX_INTERFACE * LEGS;
/// Adapters one router can sit in front of.
const LEGS: usize = 2;

// The application-class profile carries whole payloads; the embedded one is
// what a part with a 240 KiB state arena can spare, sized for capabilities
// that answer in a few bytes -- a clock, an entropy source. A payload over
// the profile's bound is refused as busy rather than cut.
#[cfg(not(fluxor_abi = "thumbv8m"))]
mod profile {
    /// Calls that may be outstanding at once. One more is refused as busy,
    /// which the caller may retry.
    pub const IN_FLIGHT: usize = 4;
    /// Bytes of payload one call or one completion may carry behind its frame.
    pub const PAYLOAD_BYTES: usize = 8 * 1024;
    /// Frames that may be staged for each output at once, each with its payload.
    pub const STAGE: usize = 4;
}
#[cfg(fluxor_abi = "thumbv8m")]
mod profile {
    pub const IN_FLIGHT: usize = 4;
    pub const PAYLOAD_BYTES: usize = 256;
    pub const STAGE: usize = 2;
}
use profile::{IN_FLIGHT, PAYLOAD_BYTES, STAGE};

/// One call this router forwarded and is waiting on.
#[derive(Clone, Copy)]
struct Outstanding {
    request: u64,
    trace: u64,
    live: bool,
}

/// One adapter behind the router: the calls staged for it and the answer
/// being taken from it.
struct Leg {
    request_out: i32,
    reply_in: i32,
    reply: [u8; COMPLETION_FRAME],
    reply_filled: usize,
    reply_payload: [u8; PAYLOAD_BYTES],
    reply_payload_filled: usize,
    reply_payload_length: usize,
    reply_ready: bool,
    requests: [u8; (CALL_FRAME + PAYLOAD_BYTES) * STAGE],
    requests_staged: usize,
    requests_written: usize,
}

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    announced: bool,
    call_in: i32,
    completion_out: i32,
    reply_in: i32,
    request_out: i32,
    reply_in_b: i32,
    request_out_b: i32,
    table: [Outstanding; IN_FLIGHT],
    /// The call frame being taken, and its payload behind it.
    call: [u8; CALL_FRAME],
    call_filled: usize,
    call_payload: [u8; PAYLOAD_BYTES],
    call_payload_filled: usize,
    call_payload_length: usize,
    call_ready: bool,
    legs: [Leg; LEGS],
    completions: [u8; (COMPLETION_FRAME + PAYLOAD_BYTES) * STAGE],
    completions_staged: usize,
    completions_written: usize,
    /// Calls admitted, refused, and answered, which is what this module reports
    /// about itself.
    admitted: u64,
    refused: u64,
    answered: u64,
    dropped: u64,
    hung_up: bool,
    phase: u8,
    /// The interfaces the adapters behind this router serve, one per leg,
    /// separated by commas.
    interfaces: [u8; INTERFACES_BYTES],
    interfaces_length: usize,
    /// The grants the isolate was given, in the order that numbers them.
    grants: [u8; GRANTS_BYTES],
    grants_length: usize,
}

define_params! {
    State;

    1, interface, str, 0
        => |s, d, len| {
            let taken = if len > INTERFACES_BYTES { INTERFACES_BYTES } else { len };
            s.interfaces_length = taken;
            if taken > 0 {
                // SAFETY: the params reader hands a pointer valid for `len`
                // bytes, and `taken` is no larger than it or the field.
                unsafe {
                    core::ptr::copy_nonoverlapping(d, s.interfaces.as_mut_ptr(), taken);
                }
            }
        };
    2, grants, str, 0
        => |s, d, len| {
            let taken = if len > GRANTS_BYTES { GRANTS_BYTES } else { len };
            s.grants_length = taken;
            if taken > 0 {
                // SAFETY: as for `interface`.
                unsafe {
                    core::ptr::copy_nonoverlapping(d, s.grants.as_mut_ptr(), taken);
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

/// `entry` without the spaces and tabs around it.
fn trimmed(entry: &[u8]) -> &[u8] {
    let mut start = 0usize;
    let mut end = entry.len();
    while start < end && matches!(entry.get(start), Some(b' ' | b'\t')) {
        start += 1;
    }
    while end > start && matches!(entry.get(end - 1), Some(b' ' | b'\t')) {
        end -= 1;
    }
    entry.get(start..end).unwrap_or(&[])
}

/// The leg that serves `interface`, by its place in the graph's list.
fn leg_of(state: &State, interface: &[u8]) -> Option<usize> {
    let list = state
        .interfaces
        .get(..state.interfaces_length)
        .unwrap_or(&[]);
    list.split(|&byte| byte == b',')
        .map(trimmed)
        .position(|named| !named.is_empty() && named == interface)
        .filter(|&leg| leg < LEGS)
}

/// The leg and the method a call on `binding` goes to, or nothing when no
/// adapter behind this router can serve it.
///
/// `host` is method 0 on the first leg. Any other binding is the grant at
/// that position, counted from one; it is served by the leg whose interface
/// it names, under the member's method in the register.
fn method_of(state: &State, binding: u32) -> Option<(usize, u32)> {
    if binding == HOST_BINDING {
        return Some((0, 0));
    }
    if binding >= MAX_BINDINGS {
        return None;
    }
    let names = state.grants.get(..state.grants_length).unwrap_or(&[]);
    let mut index = HOST_BINDING;
    let mut found = None;
    register::grants(names, |granted, member| {
        index += 1;
        if index == binding {
            found = leg_of(state, granted).and_then(|leg| {
                register::member_of(granted, member).map(|member| (leg, member.method))
            });
        }
    });
    found
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

/// Append a frame and the payload behind it to a staging buffer, answering
/// whether both fit.
fn stage(buffer: &mut [u8], staged: &mut usize, frame: &[u8], payload: &[u8]) -> bool {
    let at = *staged;
    let middle = at + frame.len();
    let end = middle + payload.len();
    if end > buffer.len() {
        return false;
    }
    if !copy_into(buffer.get_mut(at..middle).unwrap_or(&mut []), frame)
        || !copy_into(buffer.get_mut(middle..end).unwrap_or(&mut []), payload)
    {
        return false;
    }
    *staged = end;
    true
}

/// Stage a completion for the isolate, with the bytes it answers with behind
/// it. A staging buffer that is full is not a reason to drop an answer: the
/// router stops taking calls instead.
fn stage_completion(state: &mut State, record: &CompletionRecord, payload: &[u8]) -> bool {
    stage(
        &mut state.completions,
        &mut state.completions_staged,
        &record.encode(),
        payload,
    )
}

/// Refuse a call, telling the caller why.
fn refuse(state: &mut State, record: &CallRecord, cause: Cause) {
    state.refused = state.refused.saturating_add(1);
    let _ = stage_completion(
        state,
        &CompletionRecord {
            request: record.request,
            disposition: Disposition::Rejected,
            cause,
            trace: record.trace,
            answer: Answer::None,
        },
        &[],
    );
}

/// Admit, refuse, or forward one call, with the bytes behind it.
fn route(state: &mut State, record: &CallRecord, payload: &[u8]) {
    // A binding nothing behind this router serves is refused here, and the
    // caller is told so rather than left waiting. So is one whose leg the
    // graph did not wire.
    let Some((leg, method)) = method_of(state, record.binding) else {
        return refuse(state, record, Cause::Denied);
    };
    if state.legs.get(leg).is_none_or(|leg| leg.request_out < 0) {
        return refuse(state, record, Cause::Denied);
    }
    let Some(index) = state.table.iter().position(|slot| !slot.live) else {
        return refuse(state, record, Cause::Busy);
    };
    let mut forwarded = *record;
    forwarded.binding = method;
    let staged = state.legs.get_mut(leg).is_some_and(|leg| {
        stage(
            &mut leg.requests,
            &mut leg.requests_staged,
            &forwarded.encode(),
            payload,
        )
    });
    if !staged {
        return refuse(state, record, Cause::Busy);
    }
    if let Some(slot) = state.table.get_mut(index) {
        *slot = Outstanding {
            request: record.request,
            trace: record.trace,
            live: true,
        };
    }
    state.admitted = state.admitted.saturating_add(1);
}

/// Match an adapter's answer to the call it answers, and carry its bytes on.
fn correlate(state: &mut State, record: &CompletionRecord, payload: &[u8]) {
    let found = state
        .table
        .iter()
        .position(|slot| slot.live && slot.request == record.request);
    let Some(index) = found else {
        // An answer to something nobody asked is not routed anywhere.
        state.dropped = state.dropped.saturating_add(1);
        return;
    };
    let trace = state.table.get(index).map_or(0, |slot| slot.trace);
    if let Some(slot) = state.table.get_mut(index) {
        slot.live = false;
    }
    if trace != record.trace {
        // The correlation must hold on both fields; a mismatch is a malformed
        // answer, and the caller hears that rather than the answer.
        state.dropped = state.dropped.saturating_add(1);
        let _ = stage_completion(
            state,
            &CompletionRecord {
                request: record.request,
                disposition: Disposition::Rejected,
                cause: Cause::Malformed,
                trace,
                answer: Answer::None,
            },
            &[],
        );
        return;
    }
    state.answered = state.answered.saturating_add(1);
    let _ = stage_completion(state, record, payload);
}

/// Take what one leg's adapter answered, and pass it to the isolate once it
/// is whole.
fn take_reply(state: &mut State, syscalls: &SyscallTable, leg: usize) {
    let Some(this) = state.legs.get_mut(leg) else {
        return;
    };
    if this.reply_in < 0 {
        return;
    }
    if !this.reply_ready
        && wire::take_frame(
            syscalls,
            this.reply_in,
            &mut this.reply,
            &mut this.reply_filled,
        )
    {
        this.reply_filled = 0;
        this.reply_ready = true;
        this.reply_payload_filled = 0;
        this.reply_payload_length = CompletionRecord::decode(&this.reply)
            .map_or(0, |record| record.answer.payload_length() as usize)
            .min(PAYLOAD_BYTES);
    }
    if !(this.reply_ready
        && wire::take_payload(
            syscalls,
            this.reply_in,
            &mut this.reply_payload,
            &mut this.reply_payload_filled,
            this.reply_payload_length,
        ))
    {
        return;
    }
    this.reply_ready = false;
    let length = this.reply_payload_length;
    let mut payload = [0u8; PAYLOAD_BYTES];
    copy_into(
        payload.get_mut(..length).unwrap_or(&mut []),
        this.reply_payload.get(..length).unwrap_or(&[]),
    );
    let record = CompletionRecord::decode(&this.reply);
    this.reply_payload_filled = 0;
    this.reply_payload_length = 0;
    match record {
        Some(record) => correlate(state, &record, payload.get(..length).unwrap_or(&[])),
        None => state.dropped = state.dropped.saturating_add(1),
    }
}

entry! {
    State;
    primary { call_in, completion_out }
    inputs { reply_in = 1, reply_in_b = 2 }
    outputs { request_out = 1, request_out_b = 2 }
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
    if state.syscalls.is_null() || state.call_in < 0 || state.completion_out < 0 {
        return -2;
    }
    // SAFETY: the table pointer was stored by `module_new` and checked
    // non-null above; the loader keeps it live for the module's lifetime.
    let syscalls = unsafe { &*state.syscalls };
    announce_ready!(state);
    if state.phase == 1 {
        return 1;
    }
    // The ports the loader handed over, onto the legs they belong to.
    state.legs[0].request_out = state.request_out;
    state.legs[0].reply_in = state.reply_in;
    state.legs[1].request_out = state.request_out_b;
    state.legs[1].reply_in = state.reply_in_b;

    // A call is taken only when there is room to answer it and room to
    // forward it on any leg, which is what stops an answer from ever being
    // dropped for want of space.
    let room_to_forward = state
        .legs
        .iter()
        .all(|leg| leg.requests_staged + CALL_FRAME + PAYLOAD_BYTES <= leg.requests.len());
    if state.completions_staged + COMPLETION_FRAME <= state.completions.len() && room_to_forward {
        if !state.call_ready
            && wire::take_frame(
                syscalls,
                state.call_in,
                &mut state.call,
                &mut state.call_filled,
            )
        {
            state.call_filled = 0;
            state.call_ready = true;
            state.call_payload_filled = 0;
            state.call_payload_length = CallRecord::decode(&state.call)
                .map_or(0, |record| record.payload_length as usize)
                .min(PAYLOAD_BYTES);
        }
        // The arguments follow their frame: until every byte is here, the
        // call has not arrived.
        if state.call_ready
            && wire::take_payload(
                syscalls,
                state.call_in,
                &mut state.call_payload,
                &mut state.call_payload_filled,
                state.call_payload_length,
            )
        {
            state.call_ready = false;
            let length = state.call_payload_length;
            let mut payload = [0u8; PAYLOAD_BYTES];
            copy_into(
                payload.get_mut(..length).unwrap_or(&mut []),
                state.call_payload.get(..length).unwrap_or(&[]),
            );
            if let Some(record) = CallRecord::decode(&state.call) {
                route(state, &record, payload.get(..length).unwrap_or(&[]));
            } else {
                state.dropped = state.dropped.saturating_add(1);
            }
            state.call_payload_filled = 0;
            state.call_payload_length = 0;
        }
    }

    let mut leg = 0usize;
    while leg < LEGS {
        if state.completions_staged + COMPLETION_FRAME + PAYLOAD_BYTES <= state.completions.len() {
            take_reply(state, syscalls, leg);
        }
        if let Some(this) = state.legs.get_mut(leg) {
            if this.request_out >= 0 {
                wire::push_staged(
                    syscalls,
                    this.request_out,
                    &this.requests,
                    &mut this.requests_staged,
                    &mut this.requests_written,
                );
            }
        }
        leg += 1;
    }
    wire::push_staged(
        syscalls,
        state.completion_out,
        &state.completions,
        &mut state.completions_staged,
        &mut state.completions_written,
    );

    // The router is finished when the isolate has hung up and everything
    // staged has left. A call still outstanding at that point has nobody left
    // to answer to; holding the graph open for it would help no one.
    if !state.hung_up && wire::hung_up(syscalls, state.call_in) {
        state.hung_up = true;
    }
    if state.hung_up
        && state.legs.iter().all(|leg| leg.requests_staged == 0)
        && state.completions_staged == 0
        && state.call_filled == 0
        && !state.call_ready
    {
        state.phase = 1;
        return 1;
    }
    0
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
