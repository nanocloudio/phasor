//! On-graph conformance probe for slices, cancellation, and deadlines.
//!
//! The task under test is an endless loop with a safe point on its backward
//! edge, which is the case a resource policy has to be able to stop.

#![no_std]
#![allow(
    dead_code,
    unused_imports,
    unreachable_patterns,
    reason = "the Fluxor ABI source is mounted as one surface and this fixture consumes a subset"
)]

use core::ffi::c_void;

#[path = "../../../target/fluxor/fluxor-abi/sdk/abi.rs"]
mod abi;
use abi::SyscallTable;

include!("../../../target/fluxor/fluxor-abi/sdk/runtime.rs");

#[path = "../../common/bigint.rs"]
mod bigint;
#[path = "../../common/binding.rs"]
mod binding;
#[path = "../../common/bytecode.rs"]
mod bytecode;
#[path = "../../common/diagnostic.rs"]
mod diagnostic;
#[path = "../../common/digest.rs"]
mod digest;
#[path = "../../common/dtoa.rs"]
mod dtoa;
#[path = "../../common/emit.rs"]
mod emit;
#[path = "../../common/env.rs"]
mod env;
#[path = "../../common/feature.rs"]
mod feature;
#[path = "../../common/gc.rs"]
mod gc;
#[path = "../../common/heap.rs"]
mod heap;
#[path = "../../common/job.rs"]
mod job;
#[path = "../../common/numeric.rs"]
mod numeric;
#[path = "../../common/object.rs"]
mod object;
#[path = "../../common/policy.rs"]
mod policy;
#[path = "../../common/promise.rs"]
mod promise;
#[path = "../../common/realm.rs"]
mod realm;
#[path = "../../common/regexp.rs"]
mod regexp;
#[path = "../../common/softfloat.rs"]
mod softfloat;
#[path = "../../common/string.rs"]
mod string;
#[path = "../../common/unicode_id.rs"]
mod unicode_id;
#[path = "../../common/value.rs"]
mod value;
#[path = "../../common/verify.rs"]
mod verify;
#[path = "../../common/vm.rs"]
mod vm;

use bytecode::{Function, Opcode};
use emit::{CodeBuilder, Patch, UnitWriter};
use heap::{Heap, Slot};
use regexp::Choice;
use string::Atoms;
use value::{Handle, Value};
use vm::{Completion, Frame, Progress, Termination, Vm};

const CASE_COUNT: u16 = 10;
const ARENA_BYTES: usize = 64 * 1024;
const SLOT_COUNT: usize = 1536;
const WORKLIST: usize = 512;
const IMAGE_CAPACITY: usize = 1024;
const CODE_CAPACITY: usize = 64;
const FRAME_COUNT: usize = 8;
const REGISTER_COUNT: usize = 32;
const STATE_CAPACITY: usize = 256;

/// Where a match backtracks, what it must put back, and the units it runs over.
const CHOICE_COUNT: usize = 256;
const UNDO_COUNT: usize = 256;
const SUBJECT_UNITS: usize = 1024;
struct Storage {
    arena: [u8; ARENA_BYTES],
    choices: [Choice; CHOICE_COUNT],
    undo: [(u8, u32); UNDO_COUNT],
    subject: [u16; SUBJECT_UNITS],
    slots: [Slot; SLOT_COUNT],
    worklist: [u32; WORKLIST],
    code: [u8; CODE_CAPACITY],
    image: [u8; IMAGE_CAPACITY],
    safe_points: [u32; 8],
    patches: [Patch; 8],
    labels: [u32; 8],
    verifier_state: [i32; STATE_CAPACITY],
    entries: [u32; 512],
    handles: [Handle; 384],
    frames: [Frame; FRAME_COUNT],
    registers: [Value; REGISTER_COUNT],
}

/// Assemble a unit whose entry loops for ever with a safe point on the
/// backward edge.
fn build_loop(storage: &mut Storage) -> Option<(usize, u32)> {
    let (length, points) = {
        let mut builder = CodeBuilder::new(
            &mut storage.code,
            &mut storage.safe_points,
            &mut storage.patches,
            &mut storage.labels,
        );
        let top = builder.label();
        builder.bind(top);
        builder.safe_point();
        builder.emit(Opcode::LdaZero, &[]);
        builder.jump(Opcode::Jump, top);
        let points = builder.safe_points().len();
        (builder.finish().ok()?, points)
    };
    let function = Function {
        code_offset: 0,
        code_length: length,
        register_count: 1,
        argument_count: 0,
        frame_extent: 1,
        exception_offset: 0,
        exception_count: 0,
        safe_point_offset: 0,
        safe_point_count: u32::try_from(points).unwrap_or(0),
        context_depth: 0,
        context_slots: 0,
        flags: 0,
    };
    let mut code = [0u8; CODE_CAPACITY];
    code.get_mut(..length as usize)?
        .copy_from_slice(storage.code.get(..length as usize)?);
    let mut points_copy = [0u32; 8];
    points_copy
        .get_mut(..points)?
        .copy_from_slice(storage.safe_points.get(..points)?);
    let written = UnitWriter::new(&mut storage.image)
        .write(
            &[function],
            &[],
            &[],
            code.get(..length as usize)?,
            &[],
            points_copy.get(..points)?,
            0,
        )
        .ok()?;
    Some((written, length))
}

/// How the loop ended, given a control setup, a slice size, and a slice count.
fn drive(
    storage: &mut Storage,
    fuel: u64,
    slice: u64,
    slices: u32,
    setup: impl Fn(&mut Vm<'_, '_, '_, '_>),
) -> Option<Completion> {
    let (length, _) = build_loop(storage)?;
    let bytes = storage.image.get(..length)?;
    let unit = verify::admit(bytes, &mut storage.verifier_state).ok()?;

    let mut heap = Heap::with_worklist(
        &mut storage.arena,
        &mut storage.slots,
        &mut storage.worklist,
    );
    let mut atoms = Atoms::new(&mut storage.entries, &mut storage.handles);
    let realm = realm::create(&mut heap, &mut atoms).ok()?;
    let mut machine = Vm::new(
        &unit,
        &mut heap,
        &mut atoms,
        &mut storage.frames,
        &mut storage.registers,
        realm,
        fuel,
    );
    machine.attach_regexp(
        &mut storage.choices,
        &mut storage.undo,
        &mut storage.subject,
    );
    machine.start().ok()?;
    setup(&mut machine);
    let mut index = 0u32;
    while index < slices {
        match machine.resume(slice) {
            Progress::Running => {}
            Progress::Finished(completion) => return Some(completion),
        }
        index += 1;
    }
    None
}

#[allow(
    clippy::match_same_arms,
    reason = "each case is an independent assertion and merging arms would hide which one failed"
)]
fn run_case(storage: &mut Storage, case: u16) -> bool {
    match case {
        // A task that does not finish keeps its state across slices.
        0 => drive(storage, u64::MAX, 100, 20, |_| {}).is_none(),

        // Cancellation stops it at the next safe point.
        1 => matches!(
            drive(storage, u64::MAX, 100, 20, |machine| machine
                .control()
                .cancel()),
            Some(Completion::Terminated(Termination::Cancelled))
        ),

        // A deadline that has passed stops it as well.
        2 => matches!(
            drive(storage, u64::MAX, 100, 20, |machine| {
                machine.control().set_deadline(100);
                machine.control().observe(101);
            }),
            Some(Completion::Terminated(Termination::DeadlineReached))
        ),

        // A deadline in the future does not.
        3 => drive(storage, u64::MAX, 100, 20, |machine| {
            machine.control().set_deadline(1_000_000);
            machine.control().observe(1);
        })
        .is_none(),

        // No deadline at all means no deadline can pass, whatever the clock
        // says.
        4 => drive(storage, u64::MAX, 100, 20, |machine| {
            machine.control().observe(u64::MAX);
        })
        .is_none(),

        // The instruction budget is separate from the slice: it ends the task.
        5 => matches!(
            drive(storage, 500, 50, 100, |_| {}),
            Some(Completion::Terminated(Termination::FuelExhausted))
        ),

        // A one-instruction slice still makes progress.
        6 => drive(storage, u64::MAX, 1, 50, |_| {}).is_none(),

        // Cancellation wins over a deadline that has not passed.
        7 => matches!(
            drive(storage, u64::MAX, 100, 20, |machine| {
                machine.control().cancel();
                machine.control().set_deadline(1_000_000);
                machine.control().observe(1);
            }),
            Some(Completion::Terminated(Termination::Cancelled))
        ),

        // The control block reports what it was told.
        8 => {
            let mut control = vm::Control::default();
            let before = control.cancelled();
            control.cancel();
            !before && control.cancelled() && !control.expired()
        }
        9 => {
            let mut control = vm::Control::default();
            control.set_deadline(10);
            control.observe(9);
            let early = control.expired();
            control.observe(10);
            !early && control.expired()
        }
        _ => true,
    }
}

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    report_out: i32,
    exit_out: i32,
    storage: Storage,
    case: u16,
    failures: u16,
    /// The first case that failed, which is what a report names.
    first_failure: u16,
    phase: u8,
}

#[no_mangle]
#[link_section = ".text.module_state_size"]
pub extern "C" fn module_state_size() -> u32 {
    u32::try_from(core::mem::size_of::<State>()).unwrap_or(u32::MAX)
}

#[no_mangle]
#[link_section = ".text.module_init"]
pub extern "C" fn module_init(_syscalls: *const c_void) {}

#[no_mangle]
#[link_section = ".text.module_new"]
pub extern "C" fn module_new(
    _in_chan: i32,
    out_chan: i32,
    _ctrl_chan: i32,
    _params: *const u8,
    _params_len: usize,
    state: *mut u8,
    state_size: usize,
    syscalls: *const c_void,
) -> i32 {
    if state.is_null() || syscalls.is_null() {
        return -1;
    }
    if state_size < core::mem::size_of::<State>() {
        return -2;
    }
    unsafe {
        let table = syscalls.cast::<SyscallTable>();
        let state = state.cast::<State>();
        core::ptr::write_bytes(state.cast::<u8>(), 0, core::mem::size_of::<State>());
        core::ptr::addr_of_mut!((*state).syscalls).write(table);
        core::ptr::addr_of_mut!((*state).report_out).write(out_chan);
        core::ptr::addr_of_mut!((*state).exit_out).write(dev_channel_port(&*table, 1, 1));
    }
    0
}

#[no_mangle]
#[link_section = ".text.module_step"]
pub extern "C" fn module_step(state: *mut u8) -> i32 {
    if state.is_null() {
        return -1;
    }
    let state = unsafe { &mut *state.cast::<State>() };
    if state.case == 0 && state.failures == 0 && state.first_failure == 0 {
        state.first_failure = u16::MAX;
    }
    if state.syscalls.is_null() {
        return -2;
    }
    let syscalls = unsafe { &*state.syscalls };

    if state.phase == 2 {
        return 1;
    }
    if state.case < CASE_COUNT {
        let case = state.case;
        if !run_case(&mut state.storage, case) {
            state.failures = state.failures.saturating_add(1);
            if state.first_failure == u16::MAX {
                state.first_failure = case;
            }
        }
        state.case = state.case.saturating_add(1);
        return 0;
    }

    if state.phase == 0 {
        // A failure names the first case that failed, so a report is enough
        // to find it without instrumenting the module again.
        let mut buffer = [0u8; 64];
        let report: &[u8] = if state.failures == 0 {
            b"phasor-control-probe: 10 passed\n"
        } else {
            let prefix = b"phasor-control-probe: failed at ";
            let mut length = 0usize;
            while length < prefix.len() {
                buffer[length] = prefix[length];
                length += 1;
            }
            let mut digits = [0u8; 5];
            let mut count = 0usize;
            let mut value = state.first_failure;
            loop {
                digits[count] = b'0' + u8::try_from(value % 10).unwrap_or(0);
                count += 1;
                value /= 10;
                if value == 0 {
                    break;
                }
            }
            while count > 0 {
                count -= 1;
                buffer[length] = digits[count];
                length += 1;
            }
            buffer[length] = b'\n';
            length += 1;
            buffer.get(..length).unwrap_or(&[])
        };
        // A graph that gives the probe no report port still runs it; the
        // outcome then shows in the module's own completion status.
        if state.report_out >= 0 {
            let written = unsafe {
                (syscalls.channel_write)(state.report_out, report.as_ptr(), report.len())
            };
            if written != i32::try_from(report.len()).unwrap_or(i32::MAX) {
                return 0;
            }
        }
        state.phase = 1;
    }

    if state.exit_out >= 0 {
        let code = i32::from(state.failures != 0).to_le_bytes();
        let written =
            unsafe { (syscalls.channel_write)(state.exit_out, code.as_ptr(), code.len()) };
        if written != i32::try_from(code.len()).unwrap_or(i32::MAX) {
            return 0;
        }
    }
    state.phase = 2;
    // Completing is the pass signal for a graph with no port to report on; a
    // failure is a module error, which the kernel reports either way.
    if state.failures == 0 {
        1
    } else {
        -3
    }
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
