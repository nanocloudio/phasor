//! The compiler: source in, verified unit image out.
//!
//! One input stream ending in a hang-up is one source. The module compiles it,
//! verifies the result, and writes the unit image to its output. Nothing else
//! happens here: the image is content-addressed, so whatever carries it next —
//! a pipe, a link, another device — cannot change it without changing its
//! digest.

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

#[path = "../../common/arena.rs"]
mod arena;
#[path = "../../common/bytecode.rs"]
mod bytecode;
#[path = "../../common/diagnostic.rs"]
mod diagnostic;
#[path = "../../common/digest.rs"]
mod digest;
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
#[path = "../../common/frontend.rs"]
#[macro_use]
mod frontend;
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
#[path = "../../common/wire.rs"]
mod wire;

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
    if wire::params_are_tlv(params, params_len, TLV_MAGIC, TLV_VERSION) {
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
        let Some(source) = state.source.get(..source_length) else {
            return false;
        };
        // SAFETY: the source is read once and never written, so the borrow the
        // front end takes of it does not conflict with the storage it writes into.
        let source: &[u8] = unsafe { core::slice::from_raw_parts(source.as_ptr(), source.len()) };
        let goal = if state.goal == GOAL_MODULE {
            frontend::Goal::Module
        } else {
            frontend::Goal::Script
        };
        let mut storage = frontend_storage!(state);
        match frontend::compile(source, goal, Limits::CEILING, FUEL, &mut storage) {
            Ok(compiled) => compiled.length,
            Err(report) => return state.report(report),
        }
    };
    state.image_length = length;
    true
}

entry! {
    State;
    primary { source_in, image_out }
    inputs {}
    outputs { diagnostic_out = 1, exit_out = 2 }
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
    if state.syscalls.is_null() || state.source_in < 0 || state.image_out < 0 {
        return -2;
    }
    // SAFETY: the table pointer was stored by `module_new` and checked
    // non-null above; the loader keeps it live for the module's lifetime.
    let syscalls = unsafe { &*state.syscalls };

    if state.phase == 3 {
        return 1;
    }

    // Stage the whole source before compiling it: a partial read is not a
    // program.
    if state.phase == 0 {
        let staged = wire::stage_stream(
            syscalls,
            state.source_in,
            &mut state.source,
            &mut state.source_length,
            &mut state.overflowed,
        );
        if staged != wire::Staged::Complete {
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
            if state.has_diagnostic
                && state.diagnostic_out >= 0
                && !wire::push_progress(
                    syscalls,
                    state.diagnostic_out,
                    &state.diagnostic,
                    &mut state.diagnostic_written,
                )
            {
                return 0;
            }
            state.phase = 2;
            return 0;
        }
        let image = state.image.get(..state.image_length).unwrap_or(&[]);
        if wire::push_progress(syscalls, state.image_out, image, &mut state.written) {
            state.phase = 2;
        }
        return 0;
    }

    if !wire::push_exit(syscalls, state.exit_out, state.failed) {
        return 0;
    }
    state.phase = 3;
    // A source that does not compile is an outcome, not a fault: the exit status
    // says what happened and the diagnostic says why.
    1
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
