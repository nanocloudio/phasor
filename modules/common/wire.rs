//! Bytes between a module and its ports.
//!
//! Every fmod moves bytes the same three ways: it stages an input stream until
//! the writer hangs up, it reads fixed-width frames in whatever pieces they
//! arrive, and it pushes staged bytes out as the port takes them. This file is
//! those three, once. It holds no state of its own and knows nothing about
//! what a frame means: every buffer and every count is the caller's.
//!
//! It is one of the few common files that names the Fluxor ABI, and the only
//! one whose job is to hold the `unsafe` a syscall takes. A module mounts it
//! beside the SDK, so `crate::abi` and the poll constants resolve here as
//! they do in the module's own entry file.

use crate::abi::SyscallTable;

/// What staging an input stream found this step.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Staged {
    /// Nothing arrived and the writer is still there.
    Idle,
    /// Bytes arrived, or were discarded past the buffer's end.
    Partial,
    /// The writer hung up: the stream is whole, as far as it will ever be.
    Complete,
}

/// Poll a port for `events`, answering the events that are ready.
fn ready(sys: &SyscallTable, port: i32, events: u32) -> u32 {
    // SAFETY: the table is the one the loader handed this module, live for
    // the module's lifetime, and `port` is a handle it resolved.
    let poll = unsafe { (sys.channel_poll)(port, events) };
    if poll <= 0 {
        return 0;
    }
    (poll as u32) & events
}

/// Read up to `into.len()` bytes from a port, answering how many arrived.
fn read(sys: &SyscallTable, port: i32, into: &mut [u8]) -> usize {
    // SAFETY: `into` is a live, writable slice of exactly the length passed,
    // and the port handle belongs to this module.
    let read = unsafe { (sys.channel_read)(port, into.as_mut_ptr(), into.len()) };
    if read <= 0 {
        return 0;
    }
    usize::try_from(read).unwrap_or(0).min(into.len())
}

/// Write up to `from.len()` bytes to a port, answering how many it took.
fn write(sys: &SyscallTable, port: i32, from: &[u8]) -> usize {
    // SAFETY: `from` is a live slice of exactly the length passed, and the
    // port handle belongs to this module.
    let written = unsafe { (sys.channel_write)(port, from.as_ptr(), from.len()) };
    if written <= 0 {
        return 0;
    }
    usize::try_from(written).unwrap_or(0).min(from.len())
}

/// Stage an input stream until the writer hangs up.
///
/// `filled` is how much of `buffer` holds the stream so far. A stream larger
/// than the buffer sets `overflowed` and is drained, never truncated: the
/// caller refuses it whole. The answer is `Complete` once, on the hang-up.
pub fn stage_stream(
    sys: &SyscallTable,
    port: i32,
    buffer: &mut [u8],
    filled: &mut usize,
    overflowed: &mut bool,
) -> Staged {
    let events = ready(sys, port, crate::POLL_IN | crate::POLL_HUP);
    if events == 0 {
        return Staged::Idle;
    }
    if events & crate::POLL_IN != 0 {
        let offset = (*filled).min(buffer.len());
        let Some(room) = buffer.get_mut(offset..) else {
            return Staged::Idle;
        };
        if room.is_empty() {
            *overflowed = true;
            let mut discard = [0u8; 64];
            let _ = read(sys, port, &mut discard);
            return Staged::Partial;
        }
        *filled = offset + read(sys, port, room);
        return Staged::Partial;
    }
    if events & crate::POLL_HUP != 0 {
        Staged::Complete
    } else {
        Staged::Idle
    }
}

/// Read whatever is waiting on a port into `into`, answering how many bytes
/// arrived. A rolling record stream is staged this way: the caller drains
/// whole records and keeps the rest.
pub fn read_available(sys: &SyscallTable, port: i32, into: &mut [u8]) -> usize {
    if port < 0 || into.is_empty() || ready(sys, port, crate::POLL_IN) == 0 {
        return 0;
    }
    read(sys, port, into)
}

/// Read one fixed-width frame in pieces. `filled` is how much of `frame` has
/// arrived; the answer is `true` once the frame is whole.
pub fn take_frame(sys: &SyscallTable, port: i32, frame: &mut [u8], filled: &mut usize) -> bool {
    if port < 0 {
        return false;
    }
    if ready(sys, port, crate::POLL_IN) == 0 {
        return false;
    }
    let offset = (*filled).min(frame.len());
    let Some(room) = frame.get_mut(offset..) else {
        return false;
    };
    if room.is_empty() {
        return true;
    }
    *filled = offset + read(sys, port, room);
    *filled == frame.len()
}

/// Push `bytes[written..staged]` out, and forget them once they are all gone:
/// both counts return to zero when the port has taken everything.
pub fn push_staged(
    sys: &SyscallTable,
    port: i32,
    bytes: &[u8],
    staged: &mut usize,
    written: &mut usize,
) {
    if port < 0 || *written >= *staged {
        return;
    }
    if ready(sys, port, crate::POLL_OUT) == 0 {
        return;
    }
    let end = (*staged).min(bytes.len());
    let offset = (*written).min(end);
    *written = offset + write(sys, port, bytes.get(offset..end).unwrap_or(&[]));
    if *written >= *staged {
        *written = 0;
        *staged = 0;
    }
}

/// Push `bytes[written..]` out, advancing `written` by what the port took.
/// The answer is `true` once everything has left; nothing is reset.
pub fn push_progress(sys: &SyscallTable, port: i32, bytes: &[u8], written: &mut usize) -> bool {
    if *written >= bytes.len() {
        return true;
    }
    if port < 0 {
        return false;
    }
    if ready(sys, port, crate::POLL_OUT) == 0 {
        return false;
    }
    let offset = (*written).min(bytes.len());
    *written = offset + write(sys, port, bytes.get(offset..).unwrap_or(&[]));
    *written >= bytes.len()
}

/// Write a whole slice in one call, or report that the port did not take it
/// all. A record that must not be split goes through here.
pub fn push_whole(sys: &SyscallTable, port: i32, bytes: &[u8]) -> bool {
    write(sys, port, bytes) == bytes.len()
}

/// The exit code every fmod ends with: one little-endian `i32`, zero for
/// success. A port that is not wired counts as written.
pub fn push_exit(sys: &SyscallTable, port: i32, failed: bool) -> bool {
    if port < 0 {
        return true;
    }
    push_whole(sys, port, &i32::from(failed).to_le_bytes())
}

/// Whether an input port has hung up.
pub fn hung_up(sys: &SyscallTable, port: i32) -> bool {
    port >= 0 && ready(sys, port, crate::POLL_HUP) != 0
}

/// Whether an input port has bytes waiting.
pub fn has_input(sys: &SyscallTable, port: i32) -> bool {
    port >= 0 && ready(sys, port, crate::POLL_IN) != 0
}

/// Whether a parameter block is TLV-framed under `magic` and `version`.
///
/// # Safety
/// `params` must be valid for reads of `len` bytes, or null.
pub unsafe fn params_are_tlv(params: *const u8, len: usize, magic: u8, version: u8) -> bool {
    !params.is_null() && len >= 4 && *params == magic && *params.add(1) == version
}
