//! One log line from a module, for the target where the log is the only channel.
//!
//! On bare metal a module cannot be observed from outside. The scheduler's
//! `MON_HIST` histogram is the usual instrument, and it counts only the steps
//! that returned `StepOutcome::Continue` — the kernel records a step's time in
//! the `Continue` arm alone — so a module that returns `Ready`, `Burst`,
//! `Done` or an error is invisible in it, and a module that stops appearing
//! cannot be told apart from a module that stopped returning `Continue`. The
//! board's serial console is dead and its only log channel is UDP, which does
//! not exist until the IP stack has a lease. So a module that wants to say
//! where it got to says it in a log line, and says it in a fixed number of
//! bytes with no allocation and no panic path.
//!
//! The line is built in a stack buffer and written once, through the kernel's
//! `LOG_WRITE` primitive at info level, which is the level the rig's capture
//! carries. Writes past the buffer's end are dropped rather than failing: a
//! diagnostic must never be the thing that breaks the run it is diagnosing.
//!
//! ```ignore
//! let mut line = trace::Line::<64>::new();
//! line.text(b"[iso] phase=").number(state.phase.into());
//! line.text(b" fuel=").number(fuel);
//! trace::write(syscalls, line.bytes());
//! ```

/// A bounded ASCII line, built left to right.
pub struct Line<const N: usize> {
    buffer: [u8; N],
    length: usize,
}

impl<const N: usize> Line<N> {
    /// An empty line.
    pub fn new() -> Self {
        Self {
            buffer: [0; N],
            length: 0,
        }
    }

    /// Append `text`, dropping whatever does not fit.
    pub fn text(&mut self, text: &[u8]) -> &mut Self {
        if let Some(rest) = self.buffer.get_mut(self.length..) {
            self.length += crate::text::put_ascii(rest, text);
        }
        self
    }

    /// Append `value` in decimal, dropping whatever does not fit.
    pub fn number(&mut self, value: u32) -> &mut Self {
        if let Some(rest) = self.buffer.get_mut(self.length..) {
            self.length += crate::text::put_u32(rest, value);
        }
        self
    }

    /// What has been built.
    pub fn bytes(&self) -> &[u8] {
        self.buffer.get(..self.length).unwrap_or(&[])
    }
}

impl<const N: usize> Default for Line<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Write one built line to the kernel log at info level.
///
/// Info is the level the rig's UDP capture carries; `debug` and `trace` are
/// filtered out before they reach the wire, so a diagnostic written below
/// info is a diagnostic that does not arrive.
pub fn write(syscalls: &crate::abi::SyscallTable, line: &[u8]) {
    if line.is_empty() {
        return;
    }
    // SAFETY: the table is the loader's, live for the module's lifetime, and
    // the pointer and length describe a slice this function was handed.
    unsafe {
        crate::dev_log(syscalls, 3, line.as_ptr(), line.len());
    }
}
