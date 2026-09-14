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
/// The provider's "not yet". The `fs` contract makes this a consumer MUST:
/// treat it as ask-again, never as capability absent. A provider whose
/// backing volume has not finished attaching has no answer to give, and one
/// that latched a refusal here would run degraded for the life of the process
/// and look exactly like a volume that genuinely could not do the thing.
const EAGAIN: i32 = -11;

const METHOD_READ: u32 = 0;
const METHOD_WRITE: u32 = 1;
const METHOD_LIST: u32 = 2;
const METHOD_DELETE: u32 = 3;
const METHOD_OPEN: u32 = 4;
const METHOD_READ_AT: u32 = 5;
const METHOD_CLOSE: u32 = 6;
const METHOD_SIZE: u32 = 7;

/// Stage a completion the step itself produced, rather than one `answer`
/// handed back: a held call is answered later than the call that made it.
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

/// What a read of the provider produced. Three answers rather than an
/// `Option`, because "not yet" and "failed" are acted on differently and
/// collapsing them is the mistake the contract names.
enum Read {
    Took(usize),
    NotYet,
    Failed,
}

/// What one whole-file operation did.
///
/// A provider's "not yet" is its own answer and not a failure, so it is
/// carried as one: an operation that says it is held and asked again, where
/// one that says it failed is refused. Reading the two as one thing is what
/// the contract forbids, and it is the whole reason this is not an `Option`.
enum Done {
    /// An answer, and how many bytes of the output buffer it filled.
    Answered(Answer, usize),
    /// The provider has not got what the call needs yet.
    NotYet,
    Failed,
}

/// A call waiting on a provider that answered "not yet".
#[derive(Clone, Copy)]
struct Waiting {
    request: u64,
    trace: u64,
    binding: u32,
    /// The path the call named, re-opened on the retry: a provider that could
    /// not attach its volume issued no descriptor to keep.
    path: [u8; PATH_BYTES],
    path_length: usize,
    create: bool,
    /// The slot a `readAt` reads through, or `usize::MAX`.
    slot: usize,
    live: bool,
}

impl Waiting {
    const EMPTY: Self = Self {
        request: 0,
        trace: 0,
        binding: 0,
        path: [0; PATH_BYTES],
        path_length: 0,
        create: false,
        slot: usize::MAX,
        live: false,
    };
}

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
    announced: bool,
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
    /// A call the provider answered "not yet" to, waiting to be asked again.
    /// One at a time: the buffer it reads into is the module's only one.
    waiting: Waiting,
    /// Set when the call just taken was held rather than answered, so the
    /// step stages nothing for it.
    deferred: bool,
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
    let mut parts: [&[u8]; 2] = [&[], &[]];
    let taken = wire::fields(held.get(..length).unwrap_or(&[]), &mut parts);
    // How many fields each method is: the shape of a call, checked rather
    // than assumed. A call that does not have the fields its method takes is
    // refused here instead of being read as though it did.
    let shape = match record.binding {
        METHOD_LIST => (0, 0),
        METHOD_READ | METHOD_DELETE | METHOD_OPEN | METHOD_CLOSE | METHOD_SIZE => (1, 1),
        // The length a read may answer with is the caller's to leave out.
        METHOD_READ_AT => (1, 2),
        METHOD_WRITE => (2, 2),
        _ => (0, usize::MAX),
    };
    if taken < shape.0 || taken > shape.1 {
        return refuse(record, Cause::Malformed);
    }
    let name = if taken > 0 { parts[0] } else { &[][..] };
    let rest = if taken > 1 { parts[1] } else { &[][..] };
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
            if descriptor == EAGAIN {
                // The volume has not finished attaching. Ask again next step:
                // the contract forbids reading this as "cannot".
                state.waiting = Waiting {
                    request: record.request,
                    trace: record.trace,
                    binding: record.binding,
                    path,
                    path_length,
                    create,
                    slot: usize::MAX,
                    live: true,
                };
                state.deferred = true;
                return refuse(record, Cause::None);
            }
            if descriptor < 0 {
                return refuse(record, Cause::Unavailable);
            }
            if record.binding == METHOD_OPEN {
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
            let outcome = whole_file(
                syscalls,
                record.binding,
                descriptor,
                state.chunk as usize,
                rest,
                out,
            );
            close(syscalls, descriptor);
            match outcome {
                Done::Answered(answer, length) => fulfilled(record, answer, length),
                // The provider opened the file and has not got what the call
                // needs yet. The retry re-opens, because the descriptor this
                // one held is closed above -- and the call is held rather
                // than refused, because "not yet" is not "cannot".
                Done::NotYet => {
                    state.waiting = Waiting {
                        request: record.request,
                        trace: record.trace,
                        binding: record.binding,
                        path,
                        path_length,
                        create,
                        slot: usize::MAX,
                        live: true,
                    };
                    state.deferred = true;
                    refuse(record, Cause::None)
                }
                Done::Failed => refuse(record, Cause::Internal),
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
                Read::Took(read) => {
                    state.files[slot].offset = file.offset.saturating_add(read as u64);
                    fulfilled(record, Answer::Payload(read as u32), read)
                }
                Read::NotYet => {
                    // The descriptor is the program's and stays open; only the
                    // answer waits.
                    state.waiting = Waiting {
                        request: record.request,
                        trace: record.trace,
                        binding: record.binding,
                        path: [0; PATH_BYTES],
                        path_length: 0,
                        create: false,
                        slot,
                        live: true,
                    };
                    state.deferred = true;
                    refuse(record, Cause::None)
                }
                Read::Failed => refuse(record, Cause::Internal),
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
) -> Read {
    let capacity = wanted.min(PAYLOAD_BYTES);
    if capacity == 0 {
        return Read::Took(0);
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
    if read == EAGAIN {
        return Read::NotYet;
    }
    if read < 0 {
        return Read::Failed;
    }
    match usize::try_from(read) {
        Ok(n) => Read::Took(n),
        Err(_) => Read::Failed,
    }
}

/// The whole-file operation a binding names, run against a descriptor the
/// provider has just issued.
///
/// The first attempt and the retry go through here, so a call that was held
/// is asked again as the member it was rather than as whichever member this
/// happened to be written for.
fn whole_file(
    syscalls: &SyscallTable,
    binding: u32,
    descriptor: i32,
    chunk: usize,
    rest: &[u8],
    out: &mut [u8; PAYLOAD_BYTES],
) -> Done {
    match binding {
        METHOD_READ => read_all(syscalls, descriptor, chunk, out),
        METHOD_SIZE => size_of(syscalls, descriptor),
        METHOD_WRITE => write_all(syscalls, descriptor, rest),
        _ => Done::Failed,
    }
}

/// Read a whole file, up to the chunk the deployment admits.
fn read_all(
    syscalls: &SyscallTable,
    descriptor: i32,
    chunk: usize,
    out: &mut [u8; PAYLOAD_BYTES],
) -> Done {
    match read_chunk(syscalls, descriptor, chunk, out) {
        Read::Took(read) => match u32::try_from(read) {
            Ok(length) => Done::Answered(Answer::Payload(length), read),
            Err(_) => Done::Failed,
        },
        // A whole-file read that cannot start yet is retried from the top,
        // which re-opens: this call owns no descriptor a retry could reuse.
        Read::NotYet => Done::NotYet,
        Read::Failed => Done::Failed,
    }
}

fn write_all(syscalls: &SyscallTable, descriptor: i32, bytes: &[u8]) -> Done {
    if bytes.is_empty() {
        return Done::Answered(Answer::Number(0.0), 0);
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
    if written == EAGAIN {
        return Done::NotYet;
    }
    if written < 0 {
        return Done::Failed;
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
    Done::Answered(Answer::Number(f64::from(written)), 0)
}

fn size_of(syscalls: &SyscallTable, descriptor: i32) -> Done {
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
    if written == EAGAIN {
        return Done::NotYet;
    }
    if written < 8 {
        return Done::Failed;
    }
    let Some(head) = stat.get(..8) else {
        return Done::Failed;
    };
    let Ok(bytes) = <[u8; 8]>::try_from(head) else {
        return Done::Failed;
    };
    Done::Answered(Answer::Number(u64::from_le_bytes(bytes) as f64), 0)
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

    // A call the provider answered "not yet" to. Asked again before any new
    // one, because the program is waiting on this and the buffer it reads
    // into is the module's only one.
    if state.waiting.live {
        let held = state.waiting;
        let mut bytes = [0u8; PAYLOAD_BYTES];
        let outcome = if held.slot == usize::MAX {
            // A whole-file call: the retry re-opens, because whatever
            // descriptor the first attempt had is closed. The write's own
            // bytes are still the ones this module staged, because no new
            // call is taken while one is held.
            let mut path = held.path;
            // SAFETY: the path is live for its length and the table is the
            // loader's.
            let descriptor =
                unsafe { provider_open(syscalls, &mut path, held.path_length, held.create) };
            if descriptor == EAGAIN {
                Done::NotYet
            } else if descriptor < 0 {
                Done::Failed
            } else {
                let payload = state.payload.get(..state.payload_length).unwrap_or(&[]);
                let mut parts: [&[u8]; 2] = [&[], &[]];
                let taken = wire::fields(payload, &mut parts);
                let rest = if taken > 1 { parts[1] } else { &[][..] };
                let done = whole_file(
                    syscalls,
                    held.binding,
                    descriptor,
                    state.chunk as usize,
                    rest,
                    &mut bytes,
                );
                close(syscalls, descriptor);
                done
            }
        } else {
            match state.files.get(held.slot).copied().filter(|f| f.live) {
                None => Done::Failed,
                Some(file) => {
                    match read_chunk(syscalls, file.descriptor, state.chunk as usize, &mut bytes) {
                        Read::NotYet => Done::NotYet,
                        Read::Failed => Done::Failed,
                        Read::Took(read) => {
                            if let Some(slot) = state.files.get_mut(held.slot) {
                                slot.offset = slot.offset.saturating_add(read as u64);
                            }
                            Done::Answered(Answer::Payload(read as u32), read)
                        }
                    }
                }
            }
        };
        if !matches!(outcome, Done::NotYet) {
            state.waiting = Waiting::EMPTY;
            state.payload_length = 0;
            match outcome {
                Done::Answered(answer, length) => stage_completion(
                    state,
                    CompletionRecord {
                        request: held.request,
                        disposition: Disposition::Fulfilled,
                        cause: Cause::None,
                        trace: held.trace,
                        answer,
                    },
                    bytes.get(..length).unwrap_or(&[]),
                ),
                _ => stage_completion(
                    state,
                    CompletionRecord {
                        request: held.request,
                        disposition: Disposition::Rejected,
                        cause: Cause::Unavailable,
                        trace: held.trace,
                        answer: Answer::None,
                    },
                    &[],
                ),
            }
        }
    }

    if state.phase == 1 {
        return 1;
    }

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
                // Held: the provider cannot answer yet, so nothing is staged
                // and the step answers once it can.
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
            // A held call's payload stays staged: its retry reads the bytes
            // from here, and no new call can arrive to overwrite them.
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
