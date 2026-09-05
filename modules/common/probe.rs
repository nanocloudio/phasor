//! The skeleton every fixture probe runs on.
//!
//! A probe is a numbered list of assertions. One case runs per module step, so
//! a case executes under a real instruction budget; the first failure is
//! remembered by number, so a report is enough to find it; and the report
//! leaves as one line, `<name>: <count> passed` or `<name>: failed at <case>`,
//! followed by an exit code. Nothing here knows what a case asserts.
//!
//! This is one of the few common files that names the Fluxor ABI: the report
//! and the exit code cross a port, and `wire.rs` is what carries them.

use crate::abi::SyscallTable;
use crate::wire;

/// Where a probe is in its run. Lives inside the fixture's `State`, zeroed
/// by `module_new` like everything else there.
#[repr(C)]
pub struct Progress {
    pub case: u16,
    pub failures: u16,
    /// The first case that failed, which is what a report names. Zero state
    /// means "none yet"; the first step marks it so.
    pub first_failure: u16,
    pub phase: u8,
}

/// Render the report line into `out`, answering its length.
pub fn report(out: &mut [u8; 64], name: &[u8], count: u16, failures: u16, first: u16) -> usize {
    let mut at = crate::text::put_ascii(out, name);
    if failures == 0 {
        at += crate::text::put_ascii(&mut out[at..], b": ");
        at += crate::text::put_u32(&mut out[at..], u32::from(count));
        at += crate::text::put_ascii(&mut out[at..], b" passed\n");
    } else {
        at += crate::text::put_ascii(&mut out[at..], b": failed at ");
        at += crate::text::put_u32(&mut out[at..], u32::from(first));
        at += crate::text::put_ascii(&mut out[at..], b"\n");
    }
    at
}

/// One module step of a probe: run the next case, or, once every case has
/// run, push the report and the exit code and complete.
///
/// Completing is the pass signal for a graph with no port to report on; a
/// failure is a module error (`-3`), which the kernel reports either way.
pub fn step(
    progress: &mut Progress,
    sys: &SyscallTable,
    report_out: i32,
    exit_out: i32,
    name: &[u8],
    count: u16,
    run: impl FnOnce(u16) -> bool,
) -> i32 {
    if progress.case == 0 && progress.failures == 0 && progress.first_failure == 0 {
        progress.first_failure = u16::MAX;
    }
    if progress.phase == 2 {
        return 1;
    }
    if progress.case < count {
        let case = progress.case;
        if !run(case) {
            progress.failures = progress.failures.saturating_add(1);
            if progress.first_failure == u16::MAX {
                progress.first_failure = case;
            }
        }
        progress.case = progress.case.saturating_add(1);
        return 0;
    }

    if progress.phase == 0 {
        // A graph that gives the probe no report port still runs it; the
        // outcome then shows in the module's own completion status.
        if report_out >= 0 {
            let mut line = [0u8; 64];
            let length = report(
                &mut line,
                name,
                count,
                progress.failures,
                progress.first_failure,
            );
            if !wire::push_whole(sys, report_out, line.get(..length).unwrap_or(&[])) {
                return 0;
            }
        }
        progress.phase = 1;
    }

    if !wire::push_exit(sys, exit_out, progress.failures != 0) {
        return 0;
    }
    progress.phase = 2;
    if progress.failures == 0 {
        1
    } else {
        -3
    }
}
