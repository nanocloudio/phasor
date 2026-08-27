//! The compiler: source in, verified unit image out.
//!
//! One input stream ending in a hang-up is one source. The module compiles it,
//! verifies the result, and writes the unit image to its output. Nothing else
//! happens here: the image is content-addressed, so whatever carries it next —
//! a pipe, a link, another device — cannot change it without changing its
//! digest.

#![no_std]
#![allow(
    dead_code,
    unused_imports,
    unreachable_patterns,
    reason = "the Fluxor ABI source is mounted as one surface and this module consumes a subset"
)]

use core::ffi::c_void;

#[path = "../../../target/fluxor/fluxor-abi/sdk/abi.rs"]
mod abi;
use abi::SyscallTable;

include!("../../../target/fluxor/fluxor-abi/sdk/runtime.rs");
include!("../../../target/fluxor/fluxor-abi/sdk/runtime/params.rs");

#[path = "../../common/arena.rs"]
mod arena;
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
#[path = "../../common/evalsite.rs"]
mod evalsite;
#[path = "../../common/feature.rs"]
mod feature;
#[path = "../../common/lex.rs"]
mod lex;
#[path = "../../common/lower.rs"]
#[macro_use]
mod lower;
#[path = "../../common/numeric.rs"]
mod numeric;
#[path = "../../common/parse.rs"]
mod parse;
#[path = "../../common/softfloat.rs"]
mod softfloat;
#[path = "../../common/source.rs"]
mod source;
#[path = "../../common/unicode_id.rs"]
mod unicode_id;
#[path = "../../common/value.rs"]
mod value;
#[path = "../../common/verify.rs"]
mod verify;

use arena::{Arena, Node, NodeKind};
use bytecode::{Constant, ExceptionRegion, ExportRecord, Function, ImportRecord};
use diagnostic::{code, Diagnostic, Severity};
use emit::Patch;
use lex::Lexer;
use lower::{
    lower_expression, lower_module, Binding as LexicalBinding, Pending as PendingFunction, Scope,
    Storage as LowerStorage,
};
use parse::Parser;
use source::{Limits, LineStart, LineTable};

const POLL_INPUT: u32 = 0x01;
const POLL_OUTPUT: u32 = 0x02;

// Sized for a real program rather than a snippet: a conformance case carries
// its harness, and this fmod runs on the application targets, where the state
// arena is memory the deployment declared for it.
const SOURCE_CAPACITY: usize = 32 * 1024;
const IMAGE_CAPACITY: usize = 64 * 1024;
const CODE_CAPACITY: usize = 16 * 1024;
const NODE_CAPACITY: usize = 4096;
const LIST_CAPACITY: usize = 4096;
const NUMBER_CAPACITY: usize = 512;
const SCRATCH_CAPACITY: usize = 1024;
const LINE_CAPACITY: usize = 1024;
const CONSTANT_CAPACITY: usize = 512;
const DATA_CAPACITY: usize = 16 * 1024;
const POINT_CAPACITY: usize = 1024;
const PATCH_CAPACITY: usize = 512;
const LABEL_CAPACITY: usize = 512;
const VERIFIER_CAPACITY: usize = 16 * 1024;
const FUEL: u32 = 4_000_000;

/// What a source is taken to be.
const GOAL_SCRIPT: u8 = 0;
const GOAL_MODULE: u8 = 1;

define_params! {
    State;

    1, goal, u8, 0, enum { script=0, module=1 }
        => |s, d, len| { s.goal = p_u8(d, len, 0, 0); };
}

/// Take the graph's parameters, or the defaults where it gave none.
///
/// # Safety
/// `params` must be valid for reads of `params_len` bytes, or null.
unsafe fn apply_params(state: &mut State, params: *const u8, params_len: usize) {
    let tlv = !params.is_null()
        && params_len >= 4
        && *params == TLV_MAGIC
        && *params.add(1) == TLV_VERSION;
    if tlv {
        parse_tlv(state, params, params_len);
    } else {
        set_defaults(state);
    }
}

const UNIT_CODE_CAPACITY: usize = 48 * 1024;
const UNIT_POINT_CAPACITY: usize = 2048;
const FUNCTION_CAPACITY: usize = 256;
const EXCEPTION_CAPACITY: usize = 256;
const SCOPE_CAPACITY: usize = 512;
const LEXICAL_CAPACITY: usize = 1024;
const PENDING_CAPACITY: usize = 256;
const IMPORT_CAPACITY: usize = 64;
const EXPORT_CAPACITY: usize = 64;
const EVAL_SITE_CAPACITY: usize = 2048;
#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    source_in: i32,
    image_out: i32,
    exit_out: i32,
    source: [u8; SOURCE_CAPACITY],
    image: [u8; IMAGE_CAPACITY],
    code: [u8; CODE_CAPACITY],
    nodes: [Node; NODE_CAPACITY],
    lists: [u32; LIST_CAPACITY],
    numbers: [f64; NUMBER_CAPACITY],
    scratch: [u32; SCRATCH_CAPACITY],
    starts: [LineStart; LINE_CAPACITY],
    constants: [Constant; CONSTANT_CAPACITY],
    constant_data: [u8; DATA_CAPACITY],
    safe_points: [u32; POINT_CAPACITY],
    patches: [Patch; PATCH_CAPACITY],
    labels: [u32; LABEL_CAPACITY],
    verifier_state: [i32; VERIFIER_CAPACITY],
    unit_code: [u8; UNIT_CODE_CAPACITY],
    unit_safe_points: [u32; UNIT_POINT_CAPACITY],
    functions: [Function; FUNCTION_CAPACITY],
    exceptions: [ExceptionRegion; EXCEPTION_CAPACITY],
    scopes: [Scope; SCOPE_CAPACITY],
    lexical: [LexicalBinding; LEXICAL_CAPACITY],
    pending: [PendingFunction; PENDING_CAPACITY],
    imports: [ImportRecord; IMPORT_CAPACITY],
    exports: [ExportRecord; EXPORT_CAPACITY],
    eval_sites: [u8; EVAL_SITE_CAPACITY],
    /// Whether the source is a script or a module.
    goal: u8,
    /// Why a compile failed, ready to hand to whatever renders it.
    diagnostic: [u8; diagnostic::FRAME],
    has_diagnostic: bool,
    diagnostic_out: i32,
    diagnostic_written: usize,
    source_length: usize,
    image_length: usize,
    written: usize,
    overflowed: bool,
    failed: bool,
    phase: u8,
}

impl State {
    /// Keep a diagnostic to hand on, and say the compile failed.
    fn report(&mut self, report: Diagnostic) -> bool {
        self.diagnostic = report.encode();
        self.has_diagnostic = true;
        false
    }
}

/// Compile the staged source, leaving the image in `state.image`.
fn compile(state: &mut State) -> bool {
    if state.overflowed {
        state.diagnostic = Diagnostic::at(code::SOURCE_TOO_LARGE, Severity::Error, 0).encode();
        state.has_diagnostic = true;
        return false;
    }
    let source_length = state.source_length;
    let length = {
        let table = LineTable::new(&mut state.starts);
        let Some(source) = state.source.get(..source_length) else {
            return false;
        };
        // The source is read once and never written, so the borrow the front
        // end takes of it does not conflict with the storage it writes into.
        let source: &[u8] = unsafe { core::slice::from_raw_parts(source.as_ptr(), source.len()) };
        let lexer = match Lexer::new(source, Limits::CEILING, table, FUEL) {
            Ok(lexer) => lexer,
            Err(report) => return state.report(report),
        };
        let syntax = Arena::new(&mut state.nodes, &mut state.lists, &mut state.numbers);
        let mut parser = Parser::new(lexer, syntax, &mut state.scratch, Limits::CEILING);
        let module = state.goal == GOAL_MODULE;
        let parsed = if module {
            parser.parse_module()
        } else {
            parser.parse_unit()
        };
        let root = match parsed {
            Ok(root) => root,
            Err(report) => {
                state.diagnostic = report.encode();
                state.has_diagnostic = true;
                return false;
            }
        };
        let mut storage = lower_storage!(state);
        let compiled = if module {
            lower_module(source, parser.arena(), root, &mut storage)
        } else {
            lower_expression(source, parser.arena(), root, &mut storage)
        };
        match compiled {
            Ok(compiled) => compiled.length,
            Err(report) => {
                state.diagnostic = report.encode();
                state.has_diagnostic = true;
                return false;
            }
        }
    };
    state.image_length = length;
    true
}

#[no_mangle]
#[link_section = ".text.module_state_size"]
pub extern "C" fn module_state_size() -> u32 {
    u32::try_from(core::mem::size_of::<State>()).unwrap_or(u32::MAX)
}

#[no_mangle]
#[link_section = ".text.module_init"]
pub extern "C" fn module_init(_syscalls: *const c_void) {}

#[allow(
    clippy::not_unsafe_ptr_arg_deref,
    reason = "the ABI fixes this signature: the loader passes the parameter block as a raw pointer and length, and the module reads it once under the contract that it is valid for that length"
)]
#[no_mangle]
#[link_section = ".text.module_new"]
pub extern "C" fn module_new(
    in_chan: i32,
    out_chan: i32,
    _ctrl_chan: i32,
    params: *const u8,
    params_len: usize,
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
        core::ptr::addr_of_mut!((*state).source_in).write(in_chan);
        core::ptr::addr_of_mut!((*state).image_out).write(out_chan);
        core::ptr::addr_of_mut!((*state).diagnostic_out).write(dev_channel_port(&*table, 1, 1));
        core::ptr::addr_of_mut!((*state).exit_out).write(dev_channel_port(&*table, 1, 2));
        apply_params(&mut *state, params, params_len);
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
    if state.syscalls.is_null() || state.source_in < 0 || state.image_out < 0 {
        return -2;
    }
    let syscalls = unsafe { &*state.syscalls };

    if state.phase == 3 {
        return 1;
    }

    // Stage the whole source before compiling it: a partial read is not a
    // program.
    if state.phase == 0 {
        let poll = unsafe { (syscalls.channel_poll)(state.source_in, POLL_INPUT | POLL_HUP) };
        if poll <= 0 {
            return 0;
        }
        if (poll as u32) & POLL_INPUT != 0 {
            let offset = state.source_length;
            let remaining = SOURCE_CAPACITY.saturating_sub(offset);
            if remaining == 0 {
                state.overflowed = true;
                let mut discard = [0u8; 64];
                let _ = unsafe {
                    (syscalls.channel_read)(state.source_in, discard.as_mut_ptr(), discard.len())
                };
                return 0;
            }
            let read = unsafe {
                (syscalls.channel_read)(
                    state.source_in,
                    state.source.as_mut_ptr().add(offset),
                    remaining,
                )
            };
            if read > 0 {
                state.source_length += usize::try_from(read).unwrap_or(0).min(remaining);
            }
            return 0;
        }
        if (poll as u32) & POLL_HUP == 0 {
            return 0;
        }
        state.failed = !compile(state);
        state.phase = 1;
        return 0;
    }

    // Write the image out, one bounded write at a time.
    if state.phase == 1 {
        if state.failed {
            // A failure leaves as numbers on its own port; nothing here turns
            // it into words.
            if state.has_diagnostic && state.diagnostic_out >= 0 {
                let poll = unsafe { (syscalls.channel_poll)(state.diagnostic_out, POLL_OUTPUT) };
                if poll > 0 && (poll as u32) & POLL_OUTPUT != 0 {
                    let offset = state.diagnostic_written;
                    let remaining = diagnostic::FRAME.saturating_sub(offset);
                    if remaining > 0 {
                        let written = unsafe {
                            (syscalls.channel_write)(
                                state.diagnostic_out,
                                state.diagnostic.as_ptr().add(offset),
                                remaining,
                            )
                        };
                        if written > 0 {
                            state.diagnostic_written +=
                                usize::try_from(written).unwrap_or(0).min(remaining);
                        }
                    }
                }
                if state.diagnostic_written < diagnostic::FRAME {
                    return 0;
                }
            }
            state.phase = 2;
            return 0;
        }
        let poll = unsafe { (syscalls.channel_poll)(state.image_out, POLL_OUTPUT) };
        if poll <= 0 || (poll as u32) & POLL_OUTPUT == 0 {
            return 0;
        }
        let offset = state.written;
        let remaining = state.image_length.saturating_sub(offset);
        if remaining == 0 {
            state.phase = 2;
            return 0;
        }
        let written = unsafe {
            (syscalls.channel_write)(state.image_out, state.image.as_ptr().add(offset), remaining)
        };
        if written > 0 {
            state.written += usize::try_from(written).unwrap_or(0).min(remaining);
        }
        return 0;
    }

    if state.exit_out >= 0 {
        let code = i32::from(state.failed).to_le_bytes();
        let written =
            unsafe { (syscalls.channel_write)(state.exit_out, code.as_ptr(), code.len()) };
        if written != i32::try_from(code.len()).unwrap_or(i32::MAX) {
            return 0;
        }
    }
    state.phase = 3;
    // A source that does not compile is an outcome, not a fault: the exit status
    // says what happened and the diagnostic says why.
    1
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
