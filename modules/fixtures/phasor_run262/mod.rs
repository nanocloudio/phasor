//! On-graph Test262 execution oracle.
//!
//! The front-end oracle (`phasor_test262`) says what tokenizes and parses;
//! this one says what *behaves*. The driver stages the Test262 harness once,
//! then streams selected cases; each case is compiled behind the harness and
//! run, and one verdict byte leaves per case, in order, so the driver can name
//! the file behind every failure.
//!
//! A record is a six-byte header followed by the record's bytes:
//!
//! ```text
//! length: u32 little-endian | expectation: u8 | area: u8 | bytes
//! ```
//!
//! Expectations: `0` is a positive case, which must run to completion; `1` is
//! a negative runtime case, which must throw; `2` is the harness prelude,
//! which is stored rather than judged. The verdicts are:
//!
//! ```text
//! P passed          F ran but threw       X was to throw and did not
//! T stopped on a bound (fuel, heap, stack, quota)
//! R refused by the front end (out of the admitted grammar)
//! S skipped (larger than the staging buffers)
//! ```
//!
//! `R` and `S` are scope, not verdicts on behaviour: the front-end lane is
//! where refusals are measured. `F`, `X`, and `T` are the hunt list.

#![cfg_attr(not(feature = "host-test"), no_std)]
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

#[macro_use]
#[path = "../../common/entry.rs"]
mod entry;

#[path = "../../common/arena.rs"]
mod arena;
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
#[path = "../../common/evalsite.rs"]
mod evalsite;
#[path = "../../common/feature.rs"]
mod feature;
#[path = "../../common/gc.rs"]
mod gc;
#[path = "../../common/heap.rs"]
mod heap;
#[path = "../../common/job.rs"]
mod job;
#[path = "../../common/lex.rs"]
mod lex;
#[path = "../../common/link.rs"]
mod link;
#[path = "../../common/lower.rs"]
#[macro_use]
mod lower;
#[path = "../../common/module.rs"]
mod module;
#[path = "../../common/numeric.rs"]
mod numeric;
#[path = "../../common/object.rs"]
mod object;
#[path = "../../common/parse.rs"]
mod parse;
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
#[path = "../../common/source.rs"]
mod source;
#[path = "../../common/string.rs"]
mod string;
#[path = "../../common/text.rs"]
mod text;
#[path = "../../common/unicode_id.rs"]
mod unicode_id;
#[path = "../../common/value.rs"]
mod value;
#[path = "../../common/verify.rs"]
mod verify;
#[path = "../../common/vm.rs"]
mod vm;
#[path = "../../common/wire.rs"]
mod wire;

use arena::{Arena, Node};
use bytecode::{Constant, ExceptionRegion, ExportRecord, Function, ImportRecord, Unit};
use emit::Patch;
use evalsite::EvalBinding;
use heap::{Heap, Slot};
use job::{Job, Queue};
use lex::Lexer;
use lower::{
    lower_eval, lower_module, lower_script, lower_script_strict, Binding as LexicalBinding,
    Pending as PendingFunction, Scope, Storage as LowerStorage,
};
use parse::Parser;
use regexp::Choice;
use source::{Limits, LineStart, LineTable};
use string::Atoms;
use value::{Handle, Value};
use vm::{Compiled, Completion, EvalRequest, Frame, ModuleInstance, Progress, Termination, Vm};

/// Largest case this module stages; the harness prelude has the same bound.
const CASE_CAPACITY: usize = 128 * 1024;
/// The harness core with every helper a case may include is under 64K.
const PRELUDE_CAPACITY: usize = 64 * 1024;
/// A compiled source is the prelude, a separating newline, and the case.
const SOURCE_CAPACITY: usize = PRELUDE_CAPACITY + 1 + CASE_CAPACITY;
const HEADER: usize = 6;

/// Instructions one case may run, and lexer fuel for one compile. A handful
/// of sweeping cases exhaust this deliberately: five walk a whole plane of
/// code units through a one-shot `eval` apiece, and two declare thousands of
/// identifiers in one source. They stop on the bound and the baseline records
/// the stop, which is truer of this engine than a number chosen to hide it.
const STEPS: u64 = 400_000_000;
const FUEL: u32 = 40_000_000;
/// Jobs drained per round after the body finishes, and the rounds admitted.
const JOB_SLICE: u32 = 64;
const JOB_ROUNDS: u32 = 4096;

/// Front-end storage, sized for a case behind the whole harness.
const LINE_CAPACITY: usize = 16384;
const NODE_CAPACITY: usize = 131072;
const LIST_CAPACITY: usize = 131072;
const NUMBER_CAPACITY: usize = 8192;
const SCRATCH_CAPACITY: usize = 65536;
const CODE_CAPACITY: usize = 512 * 1024;
const IMAGE_CAPACITY: usize = 1024 * 1024;
const CONSTANT_CAPACITY: usize = 16384;
const DATA_CAPACITY: usize = 512 * 1024;
const POINT_CAPACITY: usize = 32768;
const PATCH_CAPACITY: usize = 8192;
const LABEL_CAPACITY: usize = 8192;
const VERIFIER_CAPACITY: usize = 512 * 1024;
const UNIT_CODE_CAPACITY: usize = 768 * 1024;
const UNIT_POINT_CAPACITY: usize = 32768;
const FUNCTION_CAPACITY: usize = 2048;
const EXCEPTION_CAPACITY: usize = 2048;
const SCOPE_CAPACITY: usize = 4096;
const LEXICAL_CAPACITY: usize = 32768;
const PENDING_CAPACITY: usize = 2048;
const IMPORT_CAPACITY: usize = 64;
const EXPORT_CAPACITY: usize = 64;
const EVAL_SITE_CAPACITY: usize = 2048;

/// Machine storage, larger than the isolate's: a conformance case is allowed
/// to be greedier than a deployed program.
const ARENA_BYTES: usize = 4096 * 1024;
const SLOT_COUNT: usize = 65536;
const WORKLIST: usize = 2048;
const ATOM_ENTRIES: usize = 65536;
const ATOM_HANDLES: usize = 49152;
const FRAME_COUNT: usize = 128;
const REGISTER_COUNT: usize = 3072;
const ROOT_COUNT: usize = 24576;
const JOB_COUNT: usize = 64;
const CHOICE_COUNT: usize = 256;
const UNDO_COUNT: usize = 256;
const SUBJECT_UNITS: usize = 4096;
const COLLECTION_SLICE: u32 = 512;
const COLLECTION_HEADROOM: u32 = (ARENA_BYTES / 4) as u32;

/// Units an `eval` may compile in one case, and the bytes one may occupy.
/// A repeated source reuses its unit, so a loop over one eval costs one slot.
const EVAL_SLOTS: usize = 24;
/// Units the machine's own compiler makes for evals it cannot pause on —
/// inside a promise job or a native's callback — and their images, cut
/// from one region for the case. What does not fit falls back to the
/// pausing protocol above, or fails where the machine cannot pause.
const NESTED_UNITS: usize = 256;
const NESTED_REGION: usize = 512 * 1024;
/// Module companions one case may stage beside it, their sources, and the
/// region their compiled units are cut from.
const MODULE_SLOTS: usize = 12;
const FIXTURE_SOURCE: usize = 8 * 1024;
const MODULE_REGION: usize = 256 * 1024;
const MAX_MODULE_IMPORTS: usize = 192;
/// Where a staged closure's units sit in the machine's table: after the
/// static slots and the in-place compiler's own.
const MODULE_BASE: usize = EVAL_SLOTS + 1 + NESTED_UNITS;
/// Where the module prelude's compiled script sits among the units.
const PRELUDE_UNIT: usize = MODULE_BASE + MODULE_SLOTS;
/// Room for that compiled script.
const PRELUDE_IMAGE: usize = 48 * 1024;
const EVAL_IMAGE: usize = 8 * 1024;
const EVAL_SOURCE: usize = 8 * 1024;

struct Storage {
    starts: [LineStart; LINE_CAPACITY],
    nodes: [Node; NODE_CAPACITY],
    lists: [u32; LIST_CAPACITY],
    numbers: [f64; NUMBER_CAPACITY],
    scratch: [u32; SCRATCH_CAPACITY],
    code: [u8; CODE_CAPACITY],
    image: [u8; IMAGE_CAPACITY],
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
    arena: [u8; ARENA_BYTES],
    slots: [Slot; SLOT_COUNT],
    worklist: [u32; WORKLIST],
    entries: [u32; ATOM_ENTRIES],
    handles: [Handle; ATOM_HANDLES],
    frames: [Frame; FRAME_COUNT],
    registers: [Value; REGISTER_COUNT],
    roots: [Handle; ROOT_COUNT],
    jobs: [Job; JOB_COUNT],
    choices: [Choice; CHOICE_COUNT],
    undo: [(u8, u32); UNDO_COUNT],
    subject: [u16; SUBJECT_UNITS],
    eval_images: [[u8; EVAL_IMAGE]; EVAL_SLOTS],
    eval_lengths: [usize; EVAL_SLOTS],
    eval_digests: [u64; EVAL_SLOTS],
    eval_source: [u8; EVAL_SOURCE],
    nested_region: [u8; NESTED_REGION],
    nested_digests: [u64; NESTED_UNITS],
    nested_source: [u8; EVAL_SOURCE],
    entry_name: [u8; 128],
    entry_name_length: usize,
    fixture_names: [[u8; 128]; MODULE_SLOTS],
    fixture_name_lengths: [usize; MODULE_SLOTS],
    fixture_data: [[u8; FIXTURE_SOURCE]; MODULE_SLOTS],
    fixture_lengths: [usize; MODULE_SLOTS],
    fixture_count: usize,
    module_region: [u8; MODULE_REGION],
    module_offsets: [(usize, usize); MODULE_SLOTS],
    module_count: usize,
    module_imports: [(u32, u32); MAX_MODULE_IMPORTS],
    module_import_count: usize,
    module_entry_base: u32,
    module_bases: [u32; MODULE_SLOTS],
    /// Which modules each module reaches through its imports, as a mask of
    /// registry indices: a module starts only after those it reaches settle.
    module_depends: [u32; MODULE_SLOTS + 1],
    /// Which modules evaluate up front, as a mask of registry indices: what
    /// only `import defer` reaches waits for a meaningful use — unless it
    /// awaits, which the specification runs eagerly, dependencies included.
    module_eager: u32,
    /// The module prelude, compiled as a script of its own.
    prelude_image: [u8; PRELUDE_IMAGE],
    prelude_unit_length: usize,
    /// Companions that did not compile, by registry index: touching one is
    /// the SyntaxError its compilation earned.
    module_poisoned: u32,
    /// Modules in an evaluation cycle, as unit pairs: an error in one is
    /// the evaluation error of the other.
    module_cycles: [(u32, u32); 32],
    module_cycle_count: usize,
    /// The staged names a dynamic import resolves against.
    module_names: [([u8; 128], usize, u32); MODULE_SLOTS + 1],
    module_name_count: usize,
    instances: [ModuleInstance; EVAL_SLOTS + 1 + NESTED_UNITS + MODULE_SLOTS],
    /// Every unit compiled for the current case, folded into one number:
    /// the case, its prelude, each eval and `Function` body, each companion.
    /// A case that asks for it (expectation bit `0x10`) folds this into the
    /// batch digest the summary line reports.
    image_digest: u64,
}

/// Fold one compiled image into the case's digest.
fn fold_image(digest: &mut u64, image: &[u8]) {
    *digest = digest.rotate_left(13) ^ digest_of(image);
}

/// How a case ended, before the expectation is applied.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Ran {
    /// Ran to completion; the flag is whether a host `print` reported the
    /// async test protocol's completion line.
    Finished(bool),
    Threw,
    Stopped,
    Refused,
}

/// A cheap content digest, to reuse the unit an identical source compiled.
fn digest_of(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash ^ (bytes.len() as u64)
}

/// Compile one source into `image`, answering the image length.
#[allow(
    clippy::too_many_arguments,
    reason = "the eval goal carries the site's whole picture, mirroring lower_eval"
)]
fn compile_into(
    storage: &mut FrontEnd<'_>,
    source: &[u8],
    image: &mut [u8],
    direct: bool,
    scope: &[EvalBinding<'_>],
    strict: bool,
    function_site: bool,
    var_env_depth: u32,
    super_property: bool,
    super_call: bool,
    new_target: bool,
    deny_arguments: bool,
    privates: bool,
    parameter_site: bool,
    script: bool,
) -> Option<usize> {
    let table = LineTable::new(storage.starts);
    let lexer = Lexer::new(source, Limits::CEILING, table, FUEL).ok()?;
    let syntax = Arena::new(storage.nodes, storage.lists, storage.numbers);
    let mut parser = Parser::new(lexer, syntax, storage.scratch, Limits::CEILING);
    let root = parser.parse_unit().ok()?;
    let mut lowering = LowerStorage {
        code: storage.code,
        image,
        constants: storage.constants,
        constant_data: storage.constant_data,
        safe_points: storage.safe_points,
        patches: storage.patches,
        labels: storage.labels,
        verifier_state: storage.verifier_state,
        unit_code: storage.unit_code,
        unit_safe_points: storage.unit_safe_points,
        functions: storage.functions,
        exceptions: storage.exceptions,
        scopes: storage.scopes,
        bindings: storage.lexical,
        pending: storage.pending,
        imports: storage.imports,
        exports: storage.exports,
        eval_sites: storage.eval_sites,
    };
    let compiled = if script {
        lower_script(source, parser.arena(), root, &mut lowering)
    } else if direct {
        lower_eval(
            source,
            parser.arena(),
            root,
            &mut lowering,
            scope,
            strict,
            function_site,
            var_env_depth,
            super_property,
            super_call,
            new_target,
            deny_arguments,
            privates,
            parameter_site,
        )
    } else {
        lower_eval(
            source,
            parser.arena(),
            root,
            &mut lowering,
            &[],
            false,
            false,
            u32::MAX,
            false,
            false,
            false,
            false,
            false,
            false,
        )
    }
    .ok()?;
    fold_image(
        storage.image_digest,
        image.get(..compiled.length).unwrap_or(&[]),
    );
    Some(compiled.length)
}

/// The front end's storage, borrowed apart from the machine's.
struct FrontEnd<'a> {
    starts: &'a mut [LineStart; LINE_CAPACITY],
    nodes: &'a mut [Node; NODE_CAPACITY],
    lists: &'a mut [u32; LIST_CAPACITY],
    numbers: &'a mut [f64; NUMBER_CAPACITY],
    scratch: &'a mut [u32; SCRATCH_CAPACITY],
    code: &'a mut [u8; CODE_CAPACITY],
    constants: &'a mut [Constant; CONSTANT_CAPACITY],
    constant_data: &'a mut [u8; DATA_CAPACITY],
    safe_points: &'a mut [u32; POINT_CAPACITY],
    patches: &'a mut [Patch; PATCH_CAPACITY],
    labels: &'a mut [u32; LABEL_CAPACITY],
    verifier_state: &'a mut [i32; VERIFIER_CAPACITY],
    unit_code: &'a mut [u8; UNIT_CODE_CAPACITY],
    unit_safe_points: &'a mut [u32; UNIT_POINT_CAPACITY],
    functions: &'a mut [Function; FUNCTION_CAPACITY],
    exceptions: &'a mut [ExceptionRegion; EXCEPTION_CAPACITY],
    scopes: &'a mut [Scope; SCOPE_CAPACITY],
    lexical: &'a mut [LexicalBinding; LEXICAL_CAPACITY],
    pending: &'a mut [PendingFunction; PENDING_CAPACITY],
    imports: &'a mut [ImportRecord; IMPORT_CAPACITY],
    exports: &'a mut [ExportRecord; EXPORT_CAPACITY],
    eval_sites: &'a mut [u8; EVAL_SITE_CAPACITY],
    image_digest: &'a mut u64,
}

/// The machine's in-place compiler: the front end over its storage, a
/// region the unit images are cut from, and the digests of what it made so
/// a repeated source is one unit.
struct NestedCompiler<'r, 'u, 's> {
    front: FrontEnd<'s>,
    region: &'r mut &'u mut [u8],
    digests: &'r mut [u64; NESTED_UNITS],
    offsets: &'r mut [(usize, usize); NESTED_UNITS],
    count: &'r mut usize,
    used: &'r mut usize,
    source: &'r mut [u8; EVAL_SOURCE],
}

/// The machine's way into the compiler: the state pointer is the
/// `NestedCompiler` the machine was attached with, alive as long as it is.
fn nested_compile<'u>(
    state: *mut c_void,
    heap: &Heap<'_>,
    request: &EvalRequest<'u>,
    units: &mut [Unit<'u>],
) -> Compiled {
    // SAFETY: the machine holds the pointer only while the compiler it was
    // attached with lives, in the same block, and calls it from one thread.
    let compiler = unsafe { &mut *state.cast::<NestedCompiler<'_, 'u, '_>>() };
    compiler.compile(heap, request, units)
}

impl<'u> NestedCompiler<'_, 'u, '_> {
    fn compile(
        &mut self,
        heap: &Heap<'_>,
        request: &EvalRequest<'u>,
        units: &mut [Unit<'u>],
    ) -> Compiled {
        let Some(length) = stage_source(heap, request.source, self.source) else {
            return Compiled::Exhausted;
        };
        let mut digest = digest_of(self.source.get(..length).unwrap_or(&[]));
        if request.script {
            digest = digest.rotate_left(7) ^ 0x5C71;
        }
        digest ^= u64::from(request.realm).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        if let Some((module, function, pc)) = request.site {
            digest ^= digest_of(&module.to_le_bytes())
                .rotate_left(1)
                .wrapping_add(u64::from(function) << 32 | u64::from(pc));
        }
        let mut slot = 0usize;
        while slot < *self.count {
            if self.digests.get(slot).copied() == Some(digest) {
                return Compiled::Unit(slot);
            }
            slot += 1;
        }
        if *self.count >= NESTED_UNITS.min(units.len()) {
            return Compiled::Exhausted;
        }
        let mut scope = [EvalBinding {
            name: &[],
            slot: 0,
            depth: 0,
            kind: 0,
        }; 64];
        let mut scope_count = 0usize;
        let mut strict = false;
        let mut function_site = false;
        let mut var_env_depth = u32::MAX;
        let mut super_property = false;
        let mut super_call = false;
        let mut new_target = false;
        let mut deny_arguments = false;
        let mut privates = false;
        let mut parameter_site = false;
        let mut direct = false;
        if let (Some((_, function, pc)), Some(unit)) = (request.site, request.site_unit) {
            if let Some(site) = evalsite::find(unit.eval_sites(), function, pc) {
                direct = true;
                strict = site.flags & evalsite::FLAG_STRICT != 0;
                function_site = site.flags & evalsite::FLAG_FUNCTION != 0;
                super_property = site.flags & evalsite::FLAG_SUPER_PROPERTY != 0;
                super_call = site.flags & evalsite::FLAG_SUPER_CALL != 0;
                new_target = site.flags & evalsite::FLAG_NEW_TARGET != 0;
                deny_arguments = site.flags & evalsite::FLAG_NO_ARGUMENTS != 0;
                privates = site.flags & evalsite::FLAG_PRIVATES != 0;
                parameter_site = site.flags & evalsite::FLAG_PARAMETERS != 0;
                var_env_depth = site.var_depth;
                if site.flags & evalsite::FLAG_TRUNCATED == 0 {
                    scope_count = site.bindings(&mut scope);
                }
            }
        }
        let source = self.source.get(..length).unwrap_or(&[]);
        let compiled = compile_into(
            &mut self.front,
            source,
            self.region,
            direct,
            scope.get(..scope_count).unwrap_or(&[]),
            strict,
            function_site,
            var_env_depth,
            super_property,
            super_call,
            new_target,
            deny_arguments,
            privates,
            parameter_site,
            request.script,
        );
        let Some(compiled) = compiled else {
            // The front end refuses a source that does not fit as it does
            // one that does not parse; a refusal with room left is a
            // syntax error, and one without is the host's to retry.
            return if self.region.len() < EVAL_IMAGE {
                Compiled::Exhausted
            } else {
                Compiled::Refused
            };
        };
        let taken = core::mem::take(self.region);
        if compiled > taken.len() {
            *self.region = taken;
            return Compiled::Exhausted;
        }
        let (image, rest) = taken.split_at_mut(compiled);
        *self.region = rest;
        let image: &'u [u8] = image;
        let Ok(unit) = Unit::parse(image) else {
            return Compiled::Refused;
        };
        let slot = *self.count;
        if let Some(out) = units.get_mut(slot) {
            *out = unit;
        }
        if let Some(out) = self.digests.get_mut(slot) {
            *out = digest;
        }
        if let Some(out) = self.offsets.get_mut(slot) {
            *out = (*self.used, compiled);
        }
        *self.used += compiled;
        *self.count += 1;
        Compiled::Unit(slot)
    }
}

/// Stage a heap string as UTF-8 for the compiler: a surrogate pair is one
/// code point, a lone surrogate stays a code unit in the three-byte form
/// the decoder admits. Answers the byte length, or nothing if it does not
/// fit.
fn stage_source(heap: &Heap<'_>, handle: Handle, out: &mut [u8; EVAL_SOURCE]) -> Option<usize> {
    let count = string::length(heap, handle).unwrap_or(0) as usize;
    let mut units16 = [0u16; 2048];
    if count > units16.len() {
        return None;
    }
    let copied = string::copy_units(heap, handle, units16.get_mut(..count).unwrap_or(&mut []))
        .unwrap_or(0)
        .min(units16.len());
    let mut at = 0usize;
    let mut index = 0usize;
    while index < copied {
        let unit = units16.get(index).copied().unwrap_or(0);
        let low = units16.get(index + 1).copied().unwrap_or(0);
        let paired = (0xD800..=0xDBFF).contains(&unit) && (0xDC00..=0xDFFF).contains(&low);
        let code_point = if paired {
            0x1_0000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(low) - 0xDC00)
        } else {
            u32::from(unit)
        };
        let needed = if code_point < 0x80 {
            1
        } else if code_point < 0x800 {
            2
        } else if code_point < 0x1_0000 {
            3
        } else {
            4
        };
        if at + needed > EVAL_SOURCE {
            return None;
        }
        match needed {
            1 => out[at] = code_point as u8,
            2 => {
                out[at] = 0xC0 | (code_point >> 6) as u8;
                out[at + 1] = 0x80 | (code_point & 0x3F) as u8;
            }
            3 => {
                out[at] = 0xE0 | (code_point >> 12) as u8;
                out[at + 1] = 0x80 | ((code_point >> 6) & 0x3F) as u8;
                out[at + 2] = 0x80 | (code_point & 0x3F) as u8;
            }
            _ => {
                out[at] = 0xF0 | (code_point >> 18) as u8;
                out[at + 1] = 0x80 | ((code_point >> 12) & 0x3F) as u8;
                out[at + 2] = 0x80 | ((code_point >> 6) & 0x3F) as u8;
                out[at + 3] = 0x80 | (code_point & 0x3F) as u8;
            }
        }
        at += needed;
        index += if paired { 2 } else { 1 };
    }
    Some(at)
}

/// Stage one module companion: a name length, the name, and the source.
fn stage_fixture(storage: &mut Storage, header: usize, filled: usize, buffer: &[u8]) {
    let Some(payload) = buffer.get(header..filled) else {
        storage.fixture_count = MODULE_SLOTS + 1;
        return;
    };
    if payload.len() < 2 {
        storage.fixture_count = MODULE_SLOTS + 1;
        return;
    }
    let name_length = usize::from(u16::from_le_bytes([payload[0], payload[1]]));
    let rest = payload.get(2..).unwrap_or(&[]);
    let slot = storage.fixture_count;
    if name_length > 128
        || name_length > rest.len()
        || rest.len() - name_length > FIXTURE_SOURCE
        || slot >= MODULE_SLOTS
    {
        storage.fixture_count = MODULE_SLOTS + 1;
        return;
    }
    storage.fixture_names[slot][..name_length].copy_from_slice(&rest[..name_length]);
    storage.fixture_name_lengths[slot] = name_length;
    let source = rest.get(name_length..).unwrap_or(&[]);
    storage.fixture_data[slot][..source.len()].copy_from_slice(source);
    storage.fixture_lengths[slot] = source.len();
    storage.fixture_count = slot + 1;
}

/// A registry key from a module's staged name.
fn module_key(name: &[u8]) -> module::Key {
    module::Key(digest::digest(name))
}

/// The key a `./name.js` specifier names, or nothing for a shape the
/// staging does not carry.
fn specifier_key(units: &[u16]) -> Option<module::Key> {
    if units.len() < 3 || units[0] != u16::from(b'.') || units[1] != u16::from(b'/') {
        return None;
    }
    let mut bytes = [0u8; 128];
    let mut at = 0usize;
    for &unit in units.get(2..).unwrap_or(&[]) {
        if unit > 127 || at >= bytes.len() {
            return None;
        }
        bytes[at] = unit as u8;
        at += 1;
    }
    Some(module_key(bytes.get(..at).unwrap_or(&[])))
}

/// Where a resolver finds its modules: the compiled entry, the region the
/// companions were cut from, and the registry their specifiers name.
struct ResolverContext<'a, 'b> {
    image: &'a [u8],
    entry_length: usize,
    region: &'a [u8],
    offsets: &'a [(usize, usize); MODULE_SLOTS],
    registry: &'a module::Registry<'b>,
}

/// Resolve a name in a module: its own export, an `export { x } from`
/// indirection followed to wherever it leads, or a name one of its
/// `export * from` records merges in. Answers the module's registry index
/// and the environment slot — the namespace mark for a namespace — or
/// nothing where the name resolves nowhere. The visited list holds the
/// (module, name) pairs already under resolution, as digests: meeting one
/// again is a cycle answering nothing, while the same module under another
/// name is just the walk coming back — a cyclic `export { y as x }` does.
fn resolve_export(
    context: &ResolverContext<'_, '_>,
    from: usize,
    name: &[u16],
    visited: ([u64; 32], usize),
) -> Option<(usize, u32)> {
    if name.is_empty() {
        return None;
    }
    let mut digest = 0xcbf2_9ce4_8422_2325u64 ^ (from as u64);
    digest = digest.wrapping_mul(0x0000_0100_0000_01B3);
    for &unit in name {
        digest ^= u64::from(unit);
        digest = digest.wrapping_mul(0x0000_0100_0000_01B3);
    }
    let (mut trail, depth) = visited;
    if depth >= trail.len() {
        return None;
    }
    let mut at = 0usize;
    while at < depth {
        if trail[at] == digest {
            return None;
        }
        at += 1;
    }
    trail[depth] = digest;
    let visited = (trail, depth + 1);
    let unit = if from == 0 {
        Unit::parse(context.image.get(..context.entry_length)?).ok()?
    } else {
        let (offset, held) = context.offsets.get(from - 1).copied()?;
        Unit::parse(context.region.get(offset..offset + held)?).ok()?
    };
    if let Some(found) = unit.export_slot(name) {
        if found & bytecode::EXPORT_IMPORT_MARK == 0 {
            return Some((from, found));
        }
        let indirection = found & !bytecode::EXPORT_IMPORT_MARK;
        let mut spec16 = [0u16; 128];
        let mut next = [0u16; 128];
        let (spec_length, next_length, _) = unit.import_at(indirection, &mut spec16, &mut next)?;
        let through = context
            .registry
            .index_of(specifier_key(spec16.get(..spec_length)?)?)?;
        if next_length == 0 {
            // The re-export of a namespace names the module itself — still
            // deferred, when the import it follows was.
            if unit.import(indirection)?.name == bytecode::DEFER_IMPORT_NAME {
                return Some((through, bytecode::DEFER_IMPORT_NAME));
            }
            return Some((through, u32::MAX));
        }
        return resolve_export(context, through, next.get(..next_length)?, visited);
    }
    // `default` never comes through a star; every other name may live in
    // any of the starred modules.
    let mut default16 = [0u16; 7];
    for (at, byte) in b"default".iter().enumerate() {
        default16[at] = u16::from(*byte);
    }
    if name == default16 {
        return None;
    }
    let count = unit.header().export_count;
    let mut index = 0u32;
    let mut star_found: Option<(usize, u32)> = None;
    while index < count {
        let record = unit.export(index)?;
        if record.name == u32::MAX {
            let indirection = record.slot & !bytecode::EXPORT_IMPORT_MARK;
            let mut spec16 = [0u16; 128];
            let mut next = [0u16; 128];
            let (spec_length, _, _) = unit.import_at(indirection, &mut spec16, &mut next)?;
            let through = context
                .registry
                .index_of(specifier_key(spec16.get(..spec_length)?)?)?;
            if let Some(found) = resolve_export(context, through, name, visited) {
                match star_found {
                    // Two stars giving one name different bindings make it
                    // ambiguous, which resolves to nothing.
                    Some(existing) if existing != found => return None,
                    _ => star_found = Some(found),
                }
            }
        }
        index += 1;
    }
    star_found
}

/// Build the module closure a staged case needs: compile every companion,
/// register and link the graph, order it, and lay out the import table the
/// machine resolves loads through. Answers how many modules the evaluation
/// order holds, with the entry last; nothing is an honest refusal.
fn prepare_module_closure(
    storage: &mut Storage,
    entry_length: usize,
    entry_is_module: bool,
    order_out: &mut [u32; MODULE_SLOTS + 1],
) -> Option<usize> {
    storage.module_count = 0;
    storage.module_import_count = 0;
    storage.module_entry_base = 0;
    storage.module_poisoned = 0;
    let fixture_count = storage.fixture_count;
    if fixture_count > MODULE_SLOTS {
        return None;
    }
    let entry_unit = Unit::parse(storage.image.get(..entry_length)?).ok()?;
    if entry_unit.header().import_count == 0 && fixture_count == 0 {
        order_out[0] = 0;
        // The entry's own name still resolves — a module may import itself.
        if entry_is_module {
            let mut name = [0u8; 128];
            let held = storage.entry_name_length.min(128);
            name[..held].copy_from_slice(storage.entry_name.get(..held).unwrap_or(&[]));
            storage.module_names[0] = (name, held, 0);
            storage.module_name_count = 1;
        }
        return Some(1);
    }
    // Compile every companion as a module of its own, cut from one region.
    // One that does not compile is poisoned, not fatal: only touching it —
    // statically from something that runs up front, or through a dynamic
    // import — surfaces the SyntaxError it earned.
    let mut poisoned = 0u32;
    let mut used = 0usize;
    let mut slot = 0usize;
    while slot < fixture_count {
        let source_length = storage.fixture_lengths[slot];
        let compiled = 'compile: {
            let table = LineTable::new(&mut storage.starts);
            let Ok(lexer) = Lexer::new(
                storage.fixture_data[slot].get(..source_length)?,
                Limits::CEILING,
                table,
                FUEL,
            ) else {
                break 'compile None;
            };
            let syntax = Arena::new(&mut storage.nodes, &mut storage.lists, &mut storage.numbers);
            let mut parser = Parser::new(lexer, syntax, &mut storage.scratch, Limits::CEILING);
            let Ok(root) = parser.parse_module() else {
                break 'compile None;
            };
            let mut lowering = LowerStorage {
                code: &mut storage.code,
                image: storage.module_region.get_mut(used..)?,
                constants: &mut storage.constants,
                constant_data: &mut storage.constant_data,
                safe_points: &mut storage.safe_points,
                patches: &mut storage.patches,
                labels: &mut storage.labels,
                verifier_state: &mut storage.verifier_state,
                unit_code: &mut storage.unit_code,
                unit_safe_points: &mut storage.unit_safe_points,
                functions: &mut storage.functions,
                exceptions: &mut storage.exceptions,
                scopes: &mut storage.scopes,
                bindings: &mut storage.lexical,
                pending: &mut storage.pending,
                imports: &mut storage.imports,
                exports: &mut storage.exports,
                eval_sites: &mut storage.eval_sites,
            };
            match lower::lower_module(
                storage.fixture_data[slot].get(..source_length)?,
                parser.arena(),
                root,
                &mut lowering,
            ) {
                Ok(lowered) => Some(lowered.length),
                Err(_) => None,
            }
        };
        let Some(compiled) = compiled else {
            poisoned |= 1u32 << (slot + 1);
            storage.module_offsets[slot] = (used, 0);
            slot += 1;
            continue;
        };
        storage.module_offsets[slot] = (used, compiled);
        fold_image(
            &mut storage.image_digest,
            storage
                .module_region
                .get(used..used + compiled)
                .unwrap_or(&[]),
        );
        used = used.checked_add(compiled)?;
        if used > MODULE_REGION {
            return None;
        }
        slot += 1;
    }
    storage.module_count = fixture_count;

    // Register the graph: the entry first, each module's imports right
    // after it, exactly as the registry expects.
    let blank_record = module::Record {
        key: module_key(b""),
        form: module::Form::Source,
        status: module::Status::New,
        imports_at: 0,
        import_count: 0,
        order: 0,
    };
    let mut records = [blank_record; MODULE_SLOTS + 1];
    let blank_import = module::Import {
        specifier_start: 0,
        specifier_end: 0,
        resolved: None,
    };
    let mut import_records = [blank_import; MAX_MODULE_IMPORTS];
    let mut registry = module::Registry::new(&mut records, &mut import_records);
    let mut import_keys = [module_key(b""); MAX_MODULE_IMPORTS];
    let mut own_keys = [module_key(b""); MODULE_SLOTS + 1];
    let mut flat = 0usize;
    let mut registry_index = 0usize;
    while registry_index <= fixture_count {
        let unit = if registry_index == 0 {
            entry_unit
        } else if poisoned & (1u32 << registry_index) != 0 {
            Unit::EMPTY
        } else {
            let (offset, held) = storage.module_offsets[registry_index - 1];
            Unit::parse(storage.module_region.get(offset..offset + held)?).ok()?
        };
        let key = if registry_index == 0 {
            if entry_is_module {
                module_key(
                    storage
                        .entry_name
                        .get(..storage.entry_name_length)
                        .unwrap_or(&[]),
                )
            } else {
                // A script is not importable: its record answers to a name
                // no specifier can spell.
                module_key(b"\x00script")
            }
        } else {
            let held = storage.fixture_name_lengths[registry_index - 1];
            module_key(storage.fixture_names[registry_index - 1].get(..held)?)
        };
        own_keys[registry_index.min(MODULE_SLOTS)] = key;
        let registered = registry.register(key, module::Form::Source).ok()?;
        let unit_index = if registry_index == 0 {
            0u32
        } else {
            u32::try_from(MODULE_BASE + registry_index - 1).unwrap_or(0)
        };
        {
            let mut name = [0u8; 128];
            let held = if registry_index == 0 {
                if entry_is_module {
                    let length = storage.entry_name_length.min(128);
                    name[..length].copy_from_slice(storage.entry_name.get(..length).unwrap_or(&[]));
                    length
                } else {
                    // A script entry spells no importable name.
                    0
                }
            } else {
                let length = storage.fixture_name_lengths[registry_index - 1].min(128);
                name[..length].copy_from_slice(
                    storage.fixture_names[registry_index - 1]
                        .get(..length)
                        .unwrap_or(&[]),
                );
                length
            };
            storage.module_names[registry_index.min(MODULE_SLOTS)] = (name, held, unit_index);
            storage.module_name_count = registry_index + 1;
        }
        let count = unit.header().import_count;
        let mut import = 0u32;
        while import < count {
            let mut spec16 = [0u16; 128];
            let mut name16 = [0u16; 128];
            let (spec_length, _, _) = unit.import_at(import, &mut spec16, &mut name16)?;
            // A specifier no staging can spell — a bare name, an absolute
            // path — resolves to nothing; the edge is kept for alignment
            // and its row poisoned rather than the whole case refused.
            let target = specifier_key(spec16.get(..spec_length)?)
                .unwrap_or_else(|| module_key(b"\x00missing"));
            registry.add_import(registered, 0, 0).ok()?;
            if flat >= MAX_MODULE_IMPORTS {
                return None;
            }
            import_keys[flat] = target;
            flat += 1;
            import += 1;
        }
        registry_index += 1;
    }
    let mut flat_at = 0usize;
    let mut registry_index = 0usize;
    while registry_index <= fixture_count {
        let count = registry.imports(registry_index).len();
        let mut import = 0usize;
        while import < count {
            // An edge to a module the staging does not hold resolves to
            // its own module, so linking still walks; its row is poisoned.
            let key = import_keys[flat_at];
            let resolved = if registry.index_of(key).is_some() {
                key
            } else {
                own_keys[registry_index.min(MODULE_SLOTS)]
            };
            registry.resolve(registry_index, import, resolved).ok()?;
            flat_at += 1;
            import += 1;
        }
        registry_index += 1;
    }
    let mut stack = [(0u32, 0u32); MODULE_SLOTS + 2];
    let mut walk = [0u8; MODULE_SLOTS + 1];
    let closure = link::link(&mut registry, 0, &mut stack, &mut walk).ok()?;
    if (closure.count as usize) > MODULE_SLOTS + 1 {
        return None;
    }

    // The import table: for each module, in registry order, every import
    // resolved to its source unit and export slot — or the namespace mark.
    let mut depends = [0u32; MODULE_SLOTS + 1];
    let mut eager_edges = [0u32; MODULE_SLOTS + 1];
    let mut defer_targets = [0u32; MODULE_SLOTS + 1];
    let mut edge_lists = [[0u8; 64]; MODULE_SLOTS + 1];
    let mut edge_counts = [0usize; MODULE_SLOTS + 1];
    let mut flat_at = 0usize;
    let mut registry_index = 0usize;
    while registry_index <= fixture_count {
        let base = u32::try_from(storage.module_import_count).unwrap_or(0);
        if registry_index == 0 {
            storage.module_entry_base = base;
        } else {
            storage.module_bases[registry_index - 1] = base;
        }
        let unit = if registry_index == 0 {
            entry_unit
        } else if poisoned & (1u32 << registry_index) != 0 {
            Unit::EMPTY
        } else {
            let (offset, held) = storage.module_offsets[registry_index - 1];
            Unit::parse(storage.module_region.get(offset..offset + held)?).ok()?
        };
        let count = unit.header().import_count;
        let mut import = 0u32;
        while import < count {
            let mut spec16 = [0u16; 128];
            let mut name16 = [0u16; 128];
            let (_, name_length, _) = unit.import_at(import, &mut spec16, &mut name16)?;
            let Some(source_registry) = registry.index_of(import_keys[flat_at]) else {
                // The module this edge names was never staged: the row is
                // the SyntaxError touching it earns.
                let at = storage.module_import_count;
                if at >= MAX_MODULE_IMPORTS {
                    return None;
                }
                let held = if registry_index == 0 {
                    0u32
                } else {
                    u32::try_from(MODULE_BASE + registry_index - 1).unwrap_or(0)
                };
                // A module nothing staged: the host's own refusal, which is
                // no SyntaxError — the graph was well-formed, just absent.
                storage.module_imports[at] = (held, bytecode::HOST_POISON_IMPORT);
                storage.module_import_count = at + 1;
                flat_at += 1;
                import += 1;
                continue;
            };
            let raw = unit.import(import)?;
            if source_registry != registry_index {
                depends[registry_index] |= 1u32 << source_registry;
                if raw.name == bytecode::DEFER_IMPORT_NAME {
                    defer_targets[registry_index] |= 1u32 << source_registry;
                } else {
                    eager_edges[registry_index] |= 1u32 << source_registry;
                }
                let held = edge_counts[registry_index];
                if held < 64 {
                    let mark = if raw.name == bytecode::DEFER_IMPORT_NAME {
                        0x80u8
                    } else {
                        0u8
                    };
                    edge_lists[registry_index][held] = source_registry as u8 | mark;
                    edge_counts[registry_index] = held + 1;
                }
            }
            let (through, slot) = if raw.name == bytecode::SOURCE_IMPORT_NAME {
                (source_registry, bytecode::HOST_POISON_IMPORT)
            } else if poisoned & (1u32 << source_registry) != 0 {
                (source_registry, bytecode::POISON_IMPORT)
            } else if raw.name == bytecode::DEFER_IMPORT_NAME {
                (source_registry, bytecode::DEFER_IMPORT_NAME)
            } else if name_length == 0 {
                (source_registry, u32::MAX)
            } else {
                match resolve_export(
                    &ResolverContext {
                        image: storage.image.get(..entry_length)?,
                        entry_length,
                        region: &storage.module_region,
                        offsets: &storage.module_offsets,
                        registry: &registry,
                    },
                    source_registry,
                    name16.get(..name_length)?,
                    ([0u64; 32], 0),
                ) {
                    Some(found) => found,
                    // A name that resolves nowhere — or ambiguously — is
                    // the SyntaxError whoever touches it earns.
                    None => (source_registry, bytecode::POISON_IMPORT),
                }
            };
            let source_unit = if through == 0 {
                0u32
            } else {
                u32::try_from(MODULE_BASE + through - 1).unwrap_or(0)
            };
            let at = storage.module_import_count;
            if at >= MAX_MODULE_IMPORTS {
                return None;
            }
            storage.module_imports[at] = (source_unit, slot);
            storage.module_import_count = at + 1;
            flat_at += 1;
            import += 1;
        }
        registry_index += 1;
    }
    // What a module reaches, it reaches through what its imports reach.
    let mut changed = true;
    while changed {
        changed = false;
        let mut index = 0usize;
        while index <= fixture_count {
            let mut mask = depends[index];
            let mut source = 0usize;
            while source <= fixture_count {
                if mask & (1u32 << source) != 0 {
                    mask |= depends[source];
                }
                source += 1;
            }
            mask &= !(1u32 << index);
            if mask != depends[index] {
                depends[index] = mask;
                changed = true;
            }
            index += 1;
        }
    }
    // What runs up front: everything a non-deferred edge reaches from the
    // entry — and any module that awaits, wherever it sits, along with all
    // it depends on, exactly as the specification pre-evaluates it.
    let mut eager = 1u32;
    let mut changed = true;
    while changed {
        changed = false;
        let mut index = 0usize;
        while index <= fixture_count {
            if eager & (1u32 << index) != 0 {
                let widened = eager | eager_edges[index];
                if widened != eager {
                    eager = widened;
                    changed = true;
                }
            }
            index += 1;
        }
        let mut index = 0usize;
        while index <= fixture_count {
            // Only what the entry actually reaches — a companion only a
            // dynamic import names waits for that import, awaiting or not.
            if eager & (1u32 << index) == 0
                && depends[0] & (1u32 << index) != 0
                && poisoned & (1u32 << index) == 0
            {
                let unit = if index == 0 {
                    entry_unit
                } else {
                    let (offset, held) = storage.module_offsets[index - 1];
                    Unit::parse(storage.module_region.get(offset..offset + held)?).ok()?
                };
                let entry_function = unit.header().entry_function;
                let flags = unit.function(entry_function).map_or(0, |held| held.flags);
                if flags & bytecode::function_flag::ASYNC != 0 {
                    eager |= 1u32 << index;
                    changed = true;
                }
            }
            index += 1;
        }
    }
    if eager & poisoned != 0 {
        // A companion the static graph runs up front does not compile: the
        // case is refused whole, exactly as a broken static import is.
        return None;
    }
    storage.module_poisoned = poisoned;
    storage.module_eager = eager;
    // Which of the eager await: only they justify walking through a
    // deferred edge to pre-evaluate.
    let mut pulled = 0u32;
    let mut index = 0usize;
    while index <= fixture_count {
        if eager & (1u32 << index) != 0 && poisoned & (1u32 << index) == 0 {
            let unit = if index == 0 {
                entry_unit
            } else {
                let (offset, held) = storage.module_offsets[index - 1];
                Unit::parse(storage.module_region.get(offset..offset + held)?).ok()?
            };
            let entry_function = unit.header().entry_function;
            let flags = unit.function(entry_function).map_or(0, |held| held.flags);
            if flags & bytecode::function_flag::ASYNC != 0 {
                pulled |= 1u32 << index;
            }
        }
        index += 1;
    }
    // What a module waits on before its body runs: everything a plain
    // edge reaches, and — through a deferred edge — only the async modules
    // pre-evaluated on its behalf, which settle mid-flight. A mutual wait
    // is a cycle, whose members run without waiting for each other.
    let mut waits = eager_edges;
    let mut index = 0usize;
    while index <= fixture_count {
        let mut targets = defer_targets[index];
        let mut target = 0usize;
        while target <= fixture_count {
            if targets & 1 != 0 {
                waits[index] |= ((1u32 << target) | depends[target]) & pulled;
            }
            targets >>= 1;
            target += 1;
        }
        index += 1;
    }
    let mut changed = true;
    while changed {
        changed = false;
        let mut index = 0usize;
        while index <= fixture_count {
            let mut mask = waits[index];
            let mut source = 0usize;
            while source <= fixture_count {
                if mask & (1u32 << source) != 0 {
                    mask |= waits[source];
                }
                source += 1;
            }
            mask &= !(1u32 << index);
            if mask != waits[index] {
                waits[index] = mask;
                changed = true;
            }
            index += 1;
        }
    }
    // The evaluation order walks the non-deferred edges depth first in the
    // order they are written, a module after everything it eagerly imports,
    // the entry last among what runs. What only `import defer` reaches
    // follows at the end, placed for its environment alone.
    let mut ordered = 0usize;
    let mut emitted = 0u32;
    let mut state = [0u8; MODULE_SLOTS + 1];
    let mut visit = [(0u8, 0u8); MODULE_SLOTS + 2];
    // One walk from the entry, following every edge that leads to a module
    // that runs up front — through a deferred module, when what waits
    // behind it awaits: the pre-evaluation happens right where the
    // deferring import is written, and only the eager are placed to run.
    let mut depth = 1usize;
    visit[0] = (0, 0);
    state[0] = 1;
    while depth > 0 {
        let (at, cursor) = visit[depth - 1];
        let registry_index = at as usize;
        if (cursor as usize) < edge_counts[registry_index] {
            visit[depth - 1].1 = cursor + 1;
            let held = edge_lists[registry_index][cursor as usize];
            let next = (held & 0x7F) as usize;
            // An eager edge is followed into the eager set — or through a
            // deferred module towards something that awaits; a deferred
            // edge only towards what awaits, for its pre-evaluation.
            let pulled_reach = (pulled & (1u32 << next) != 0) || depends[next] & pulled != 0;
            let leads = if held & 0x80 != 0 {
                pulled_reach
            } else {
                eager & (1u32 << next) != 0 || pulled_reach
            };
            if state[next] == 0 && leads {
                state[next] = 1;
                if depth > MODULE_SLOTS + 1 {
                    return None;
                }
                visit[depth] = (next as u8, 0);
                depth += 1;
            }
            continue;
        }
        depth -= 1;
        state[registry_index] = 2;
        if eager & (1u32 << registry_index) != 0 {
            let unit_index = if registry_index == 0 {
                0u32
            } else {
                u32::try_from(MODULE_BASE + registry_index - 1).unwrap_or(0)
            };
            order_out[ordered.min(MODULE_SLOTS)] = unit_index;
            emitted |= 1u32 << registry_index;
            ordered += 1;
        }
    }
    let mut registry_index = 0usize;
    while registry_index <= fixture_count {
        if emitted & (1u32 << registry_index) == 0 {
            let unit_index = if registry_index == 0 {
                0u32
            } else {
                u32::try_from(MODULE_BASE + registry_index - 1).unwrap_or(0)
            };
            if ordered <= MODULE_SLOTS {
                order_out[ordered] = unit_index;
            }
            ordered += 1;
        }
        registry_index += 1;
    }
    // A mutual wait is a cycle: the member placed earlier runs without
    // waiting for the later one, which still waits its turn.
    let mut place = [u32::MAX; MODULE_SLOTS + 1];
    let mut position = 0usize;
    while position < ordered.min(MODULE_SLOTS + 1) {
        let unit = order_out[position];
        let registry_index = if unit == 0 {
            0usize
        } else {
            (unit as usize).saturating_sub(MODULE_BASE) + 1
        };
        if registry_index <= MODULE_SLOTS {
            place[registry_index] = position as u32;
        }
        position += 1;
    }
    storage.module_cycle_count = 0;
    let mut index = 0usize;
    while index <= fixture_count {
        let mut source = 0usize;
        while source <= fixture_count {
            if waits[index] & (1u32 << source) != 0 && waits[source] & (1u32 << index) != 0 {
                if place[index] < place[source] {
                    waits[index] &= !(1u32 << source);
                }
                // A mutual wait is a cycle: its members share whatever
                // evaluation error any one of them earns.
                let held = storage.module_cycle_count;
                if index < source && held < 32 {
                    let one = if index == 0 {
                        0u32
                    } else {
                        u32::try_from(MODULE_BASE + index - 1).unwrap_or(0)
                    };
                    let two = if source == 0 {
                        0u32
                    } else {
                        u32::try_from(MODULE_BASE + source - 1).unwrap_or(0)
                    };
                    storage.module_cycles[held] = (one, two);
                    storage.module_cycle_count = held + 1;
                }
            }
            source += 1;
        }
        index += 1;
    }
    storage.module_depends = waits;
    Some(ordered.min(MODULE_SLOTS + 1))
}

/// The next position in evaluation order that runs up front: `import
/// defer` leaves what only it reaches to a meaningful use.
fn next_eager_position(
    order: &[u32; MODULE_SLOTS + 1],
    count: usize,
    eager: u32,
    mut position: usize,
) -> usize {
    while position < count {
        let unit = order[position];
        let registry = if unit == 0 {
            0usize
        } else {
            (unit as usize).saturating_sub(MODULE_BASE) + 1
        };
        if eager & (1u32 << registry) != 0 {
            break;
        }
        position += 1;
    }
    position
}

/// Compile and run one assembled source.
fn execute(
    storage: &mut Storage,
    source: &[u8],
    prelude: &[u8],
    strict: bool,
    module: bool,
) -> Ran {
    let length = {
        let table = LineTable::new(&mut storage.starts);
        let Ok(lexer) = Lexer::new(source, Limits::CEILING, table, FUEL) else {
            return Ran::Refused;
        };
        let syntax = Arena::new(&mut storage.nodes, &mut storage.lists, &mut storage.numbers);
        let mut parser = Parser::new(lexer, syntax, &mut storage.scratch, Limits::CEILING);
        let root = if module {
            match parser.parse_module() {
                Ok(root) => root,
                Err(_) => return Ran::Refused,
            }
        } else {
            match parser.parse_unit() {
                Ok(root) => root,
                Err(_) => return Ran::Refused,
            }
        };
        let mut lowering = lower_storage!(storage);
        let compiled = if module {
            lower_module(source, parser.arena(), root, &mut lowering)
        } else if strict {
            lower_script_strict(source, parser.arena(), root, &mut lowering)
        } else {
            lower_script(source, parser.arena(), root, &mut lowering)
        };
        match compiled {
            Ok(compiled) => compiled.length,
            Err(_) => return Ran::Refused,
        }
    };
    if storage.image.get(..length).is_none() {
        return Ran::Refused;
    }
    fold_image(
        &mut storage.image_digest,
        storage.image.get(..length).unwrap_or(&[]),
    );
    // A module's imports resolve against the companions staged beside it:
    // each compiled as a module of its own, registered, linked, and put in
    // evaluation order before anything runs. A closure that cannot be built
    // — a missing companion, an unresolvable name — is refused.
    let mut module_order = [0u32; MODULE_SLOTS + 1];
    let mut module_order_count = 1usize;
    storage.module_name_count = 0;
    if module || storage.fixture_count > 0 {
        // A script stages companions too: a dynamic import resolves into
        // the same closure a module's static imports would.
        match prepare_module_closure(storage, length, module, &mut module_order) {
            Some(count) => module_order_count = count,
            None => return Ran::Refused,
        }
    }
    let mut module_position = 0usize;
    // Which modules have begun, as a mask of registry indices: a module
    // starts when everything it reaches has settled, whatever the order
    // writes — an awaiting module holds back only its dependants.
    let mut started_mask = 0u32;

    // The machine pauses when the program calls `eval`: the source is
    // compiled into a unit slot here, the machine is rebuilt over the same
    // storage with the unit attached, and the eval is entered as a frame. A
    // repeated source reuses its slot, so a loop over one eval is bounded.
    let mut eval_count = 0usize;
    // When every slot is taken, the oldest is replaced round-robin. A unit a
    // program still holds a closure into would then be wrong, not unsafe —
    // the corpus's sweeping cases eval thousands of one-shot sources and hold
    // nothing, which is what the rotation is for.
    let mut evict_next = 0usize;
    // A module's harness prelude runs as a script of its own first: its
    // declarations land on the global object, where the whole closure sees
    // them — a script case keeps the prelude spliced in front instead.
    let mut prelude_pending = false;
    storage.prelude_unit_length = 0;
    if module && !prelude.is_empty() {
        let compiled = {
            let table = LineTable::new(&mut storage.starts);
            let Ok(lexer) = Lexer::new(prelude, Limits::CEILING, table, FUEL) else {
                return Ran::Refused;
            };
            let syntax = Arena::new(&mut storage.nodes, &mut storage.lists, &mut storage.numbers);
            let mut parser = Parser::new(lexer, syntax, &mut storage.scratch, Limits::CEILING);
            let root = match parser.parse_unit() {
                Ok(root) => root,
                Err(_) => return Ran::Refused,
            };
            let mut lowering = LowerStorage {
                code: &mut storage.code,
                image: &mut storage.prelude_image[..],
                constants: &mut storage.constants,
                constant_data: &mut storage.constant_data,
                safe_points: &mut storage.safe_points,
                patches: &mut storage.patches,
                labels: &mut storage.labels,
                verifier_state: &mut storage.verifier_state,
                unit_code: &mut storage.unit_code,
                unit_safe_points: &mut storage.unit_safe_points,
                functions: &mut storage.functions,
                exceptions: &mut storage.exceptions,
                scopes: &mut storage.scopes,
                bindings: &mut storage.lexical,
                pending: &mut storage.pending,
                imports: &mut storage.imports,
                exports: &mut storage.exports,
                eval_sites: &mut storage.eval_sites,
            };
            match lower_script(prelude, parser.arena(), root, &mut lowering) {
                Ok(compiled) => compiled.length,
                Err(_) => return Ran::Refused,
            }
        };
        fold_image(
            &mut storage.image_digest,
            storage.prelude_image.get(..compiled).unwrap_or(&[]),
        );
        storage.prelude_unit_length = compiled;
        prelude_pending = true;
    }
    let mut saves: Option<vm::Saves> = None;
    let mut kept_realm: Option<realm::Realm> = None;
    let mut enter: Option<u32> = None;
    let mut fail_pending = false;
    // The machine's own compiler, for evals it cannot pause on: its units
    // outlive every rebuild of the machine, so they sit outside the loop.
    let mut nested_offsets = [(0usize, 0usize); NESTED_UNITS];
    let mut nested_count = 0usize;
    let mut nested_used = 0usize;

    loop {
        // What to do next, decided while the machine exists and acted on
        // after its borrows end.
        let mut compile_request: Option<usize> = None;
        let mut compile_site: Option<(u32, u32, u32)> = None;
        let mut compile_script = false;
        let mut compile_realm = 0u8;
        {
            let Ok(unit) = Unit::parse(storage.image.get(..length).unwrap_or(&[])) else {
                return Ran::Refused;
            };
            let mut units = [Unit::EMPTY; EVAL_SLOTS + 1];
            units[0] = unit;
            let mut slot = 0usize;
            while slot < eval_count {
                let held = storage.eval_lengths.get(slot).copied().unwrap_or(0);
                let image = storage
                    .eval_images
                    .get(slot)
                    .and_then(|image| image.get(..held))
                    .unwrap_or(&[]);
                let Ok(parsed) = Unit::parse(image) else {
                    return Ran::Stopped;
                };
                units[slot + 1] = parsed;
                slot += 1;
            }
            // The instances persist across the machine's rebuilds: the
            // environments they hold live on the adopted heap, so a module
            // evaluated before a pause keeps its bindings after it.
            let mut index = if saves.is_none() { 0usize } else { usize::MAX };
            while index < EVAL_SLOTS + 1 + NESTED_UNITS + MODULE_SLOTS {
                storage.instances[index] = ModuleInstance {
                    environment: Value::UNDEFINED,
                    import_base: 0,
                    namespace: Value::UNDEFINED,
                    deferred_namespace: Value::UNDEFINED,
                    completion: Value::UNDEFINED,
                    body_pc: 0,
                    evaluated: 0,
                };
                index += 1;
            }

            let mut heap = match &saves {
                Some(kept) => Heap::adopt(
                    &mut storage.arena,
                    &mut storage.slots,
                    &mut storage.worklist,
                    &kept.heap,
                ),
                None => Heap::with_worklist(
                    &mut storage.arena,
                    &mut storage.slots,
                    &mut storage.worklist,
                ),
            };
            let mut atoms = match &saves {
                Some(kept) => Atoms::adopt(&mut storage.entries, &mut storage.handles, &kept.atoms),
                None => Atoms::new(&mut storage.entries, &mut storage.handles),
            };
            let realm = match kept_realm {
                Some(realm) => realm,
                None => match realm::create(&mut heap, &mut atoms) {
                    Ok(realm) => {
                        // The async test protocol reports through `print`,
                        // and the corpus reaches its host through `$262`.
                        if realm::install_print(&mut heap, &mut atoms, &realm).is_err()
                            || realm::install_262(&mut heap, &mut atoms, &realm).is_err()
                            || realm::install_random(&mut heap, &mut atoms, &realm).is_err()
                        {
                            return Ran::Stopped;
                        }
                        realm
                    }
                    Err(_) => return Ran::Stopped,
                },
            };
            // The compiler's units are views over the region's used prefix,
            // parsed afresh for each machine; new ones are cut from the rest.
            let (nested_done, mut nested_free) = storage
                .nested_region
                .split_at_mut(nested_used.min(NESTED_REGION));
            let nested_done: &[u8] = nested_done;
            let mut nested_units = [Unit::EMPTY; NESTED_UNITS + MODULE_SLOTS + 1];
            let mut slot = 0usize;
            while slot < nested_count.min(NESTED_UNITS) {
                let (offset, held) = nested_offsets.get(slot).copied().unwrap_or((0, 0));
                let Ok(parsed) = Unit::parse(nested_done.get(offset..offset + held).unwrap_or(&[]))
                else {
                    return Ran::Stopped;
                };
                if let Some(out) = nested_units.get_mut(slot) {
                    *out = parsed;
                }
                slot += 1;
            }
            let mut slot = 0usize;
            while slot < storage.module_count.min(MODULE_SLOTS) {
                let (offset, held) = storage.module_offsets.get(slot).copied().unwrap_or((0, 0));
                if held == 0 {
                    // A poisoned companion has no unit; touching it throws.
                    slot += 1;
                    continue;
                }
                let Ok(parsed) = Unit::parse(
                    storage
                        .module_region
                        .get(offset..offset + held)
                        .unwrap_or(&[]),
                ) else {
                    return Ran::Stopped;
                };
                if let Some(out) = nested_units.get_mut(NESTED_UNITS + slot) {
                    *out = parsed;
                }
                slot += 1;
            }
            if storage.prelude_unit_length > 0 {
                let held = storage.prelude_unit_length;
                let Ok(parsed) = Unit::parse(storage.prelude_image.get(..held).unwrap_or(&[]))
                else {
                    return Ran::Stopped;
                };
                if let Some(out) = nested_units.get_mut(NESTED_UNITS + MODULE_SLOTS) {
                    *out = parsed;
                }
            }
            let mut nested = NestedCompiler {
                front: FrontEnd {
                    starts: &mut storage.starts,
                    nodes: &mut storage.nodes,
                    lists: &mut storage.lists,
                    numbers: &mut storage.numbers,
                    scratch: &mut storage.scratch,
                    code: &mut storage.code,
                    constants: &mut storage.constants,
                    constant_data: &mut storage.constant_data,
                    safe_points: &mut storage.safe_points,
                    patches: &mut storage.patches,
                    labels: &mut storage.labels,
                    verifier_state: &mut storage.verifier_state,
                    unit_code: &mut storage.unit_code,
                    unit_safe_points: &mut storage.unit_safe_points,
                    functions: &mut storage.functions,
                    exceptions: &mut storage.exceptions,
                    scopes: &mut storage.scopes,
                    lexical: &mut storage.lexical,
                    pending: &mut storage.pending,
                    imports: &mut storage.imports,
                    exports: &mut storage.exports,
                    eval_sites: &mut storage.eval_sites,
                    image_digest: &mut storage.image_digest,
                },
                region: &mut nested_free,
                digests: &mut storage.nested_digests,
                offsets: &mut nested_offsets,
                count: &mut nested_count,
                used: &mut nested_used,
                source: &mut storage.nested_source,
            };
            let mut queue = Queue::new(&mut storage.jobs);
            let mut machine = Vm::new(
                &units[0],
                &mut heap,
                &mut atoms,
                &mut storage.frames,
                &mut storage.registers,
                realm,
                STEPS,
            );
            machine.attach_regexp(
                &mut storage.choices,
                &mut storage.undo,
                &mut storage.subject,
            );
            machine.attach_jobs(&mut queue);
            machine.attach_collector(&mut storage.roots, COLLECTION_SLICE, COLLECTION_HEADROOM);
            // Every slot is attached, taken or not, so the numbering of the
            // compiler's own units after them never moves.
            if (module || storage.module_count > 0) && saves.is_none() {
                storage.instances[0].import_base = storage.module_entry_base;
                let mut slot = 0usize;
                while slot < storage.module_count.min(MODULE_SLOTS) {
                    if let Some(instance) = storage.instances.get_mut(MODULE_BASE + slot) {
                        instance.import_base = storage.module_bases[slot];
                    }
                    slot += 1;
                }
            }
            let (instances, imports) = (
                &mut storage.instances[..],
                storage
                    .module_imports
                    .get(..storage.module_import_count.min(MAX_MODULE_IMPORTS))
                    .unwrap_or(&[]),
            );
            machine.attach_modules(&units[..], instances, imports);
            machine.attach_module_names(
                storage
                    .module_names
                    .get(..storage.module_name_count.min(MODULE_SLOTS + 1))
                    .unwrap_or(&[]),
            );
            machine.attach_module_cycles(
                storage
                    .module_cycles
                    .get(..storage.module_cycle_count.min(32))
                    .unwrap_or(&[]),
            );
            machine.attach_compiler(
                (&mut nested as *mut NestedCompiler<'_, '_, '_>).cast::<c_void>(),
                nested_compile,
                &mut nested_units[..],
            );
            match &saves {
                Some(kept) => machine.restore_all(kept),
                None if module => {
                    // Every module's environment exists before any of them
                    // runs, which is what lets a cycle see across itself.
                    let mut position = 0usize;
                    while position < module_order_count {
                        let unit = module_order.get(position).copied().unwrap_or(0);
                        let registry = if unit == 0 {
                            0usize
                        } else {
                            (unit as usize).saturating_sub(MODULE_BASE) + 1
                        };
                        if storage.module_poisoned & (1u32 << registry) != 0 {
                            machine.set_module_status(unit, 6);
                            position += 1;
                            continue;
                        }
                        let Ok(environment) = machine.create_module_environment(unit) else {
                            return Ran::Stopped;
                        };
                        machine.set_module_environment(unit, environment);
                        // Instantiation gives every module its function
                        // declarations before any body runs: a cycle calls
                        // across itself through them.
                        if machine.instantiate_module(unit).is_err() {
                            return Ran::Stopped;
                        }
                        // The whole eager set is evaluating from the first
                        // body on: a deferred access into it throws.
                        if storage.module_eager & (1u32 << registry) != 0 {
                            machine.set_module_status(unit, 1);
                        }
                        position += 1;
                    }
                    if prelude_pending {
                        // The prelude script runs before the first module.
                        let unit = u32::try_from(PRELUDE_UNIT).unwrap_or(0);
                        if machine.start_script(unit).is_err() {
                            return Ran::Stopped;
                        }
                    } else {
                        module_position = next_eager_position(
                            &module_order,
                            module_order_count,
                            storage.module_eager,
                            0,
                        );
                        let first = module_order.get(module_position).copied().unwrap_or(0);
                        let registry = if first == 0 {
                            0usize
                        } else {
                            (first as usize).saturating_sub(MODULE_BASE) + 1
                        };
                        started_mask |= 1u32 << registry;
                        if machine.start_module(first).is_err() {
                            return Ran::Stopped;
                        }
                    }
                }
                None => {
                    // A script with staged companions gives each of them an
                    // environment; a dynamic import runs them on demand.
                    let mut position = 1usize;
                    while position < module_order_count {
                        let unit = module_order.get(position).copied().unwrap_or(0);
                        if unit != 0 {
                            let registry = (unit as usize).saturating_sub(MODULE_BASE) + 1;
                            if storage.module_poisoned & (1u32 << registry) != 0 {
                                machine.set_module_status(unit, 6);
                                position += 1;
                                continue;
                            }
                            let Ok(environment) = machine.create_module_environment(unit) else {
                                return Ran::Stopped;
                            };
                            machine.set_module_environment(unit, environment);
                            if machine.instantiate_module(unit).is_err() {
                                return Ran::Stopped;
                            }
                        }
                        position += 1;
                    }
                    if machine.start().is_err() {
                        return Ran::Stopped;
                    }
                }
            }
            if fail_pending {
                fail_pending = false;
                match machine.fail_eval() {
                    Some(Completion::Throw(_)) => return Ran::Threw,
                    Some(_) => return Ran::Stopped,
                    None => {}
                }
            }
            if let Some(unit) = enter.take() {
                if machine.enter_eval(unit).is_err() {
                    return Ran::Stopped;
                }
            }

            // One resume with the whole budget runs to the next pause or the
            // end; every arm decides the case except a pause, which falls
            // through to the compile step below with the machine's borrows
            // ended.
            match machine.resume(u64::MAX) {
                Progress::Running => {
                    let Some(handle) = machine.pending_eval() else {
                        return Ran::Stopped;
                    };
                    // Stage the source as UTF-8 for the compiler.
                    let count = string::length(machine.heap(), handle).unwrap_or(0) as usize;
                    let mut units16 = [0u16; 2048];
                    if count > units16.len() {
                        fail_pending = true;
                    } else {
                        let copied = string::copy_units(
                            machine.heap(),
                            handle,
                            units16.get_mut(..count).unwrap_or(&mut []),
                        )
                        .unwrap_or(0);
                        let mut at = 0usize;
                        let mut index = 0usize;
                        let mut fits = true;
                        while index < copied {
                            let unit = units16[index];
                            // A surrogate pair is one code point; a lone
                            // surrogate stays a code unit of its own, in the
                            // three-byte form the decoder admits.
                            let low = units16.get(index + 1).copied().unwrap_or(0);
                            let paired = (0xD800..=0xDBFF).contains(&unit)
                                && (0xDC00..=0xDFFF).contains(&low);
                            let code_point = if paired {
                                0x1_0000
                                    + ((u32::from(unit) - 0xD800) << 10)
                                    + (u32::from(low) - 0xDC00)
                            } else {
                                u32::from(unit)
                            };
                            let needed = if code_point < 0x80 {
                                1
                            } else if code_point < 0x800 {
                                2
                            } else if code_point < 0x1_0000 {
                                3
                            } else {
                                4
                            };
                            if at + needed > EVAL_SOURCE {
                                fits = false;
                                break;
                            }
                            match needed {
                                1 => storage.eval_source[at] = code_point as u8,
                                2 => {
                                    storage.eval_source[at] = 0xC0 | (code_point >> 6) as u8;
                                    storage.eval_source[at + 1] = 0x80 | (code_point & 0x3F) as u8;
                                }
                                3 => {
                                    storage.eval_source[at] = 0xE0 | (code_point >> 12) as u8;
                                    storage.eval_source[at + 1] =
                                        0x80 | ((code_point >> 6) & 0x3F) as u8;
                                    storage.eval_source[at + 2] = 0x80 | (code_point & 0x3F) as u8;
                                }
                                _ => {
                                    storage.eval_source[at] = 0xF0 | (code_point >> 18) as u8;
                                    storage.eval_source[at + 1] =
                                        0x80 | ((code_point >> 12) & 0x3F) as u8;
                                    storage.eval_source[at + 2] =
                                        0x80 | ((code_point >> 6) & 0x3F) as u8;
                                    storage.eval_source[at + 3] = 0x80 | (code_point & 0x3F) as u8;
                                }
                            }
                            at += needed;
                            index += if paired { 2 } else { 1 };
                        }
                        if fits {
                            compile_request = Some(at);
                            compile_site = machine.pending_eval_site();
                            compile_script = machine.pending_eval_is_script();
                            compile_realm = machine.pending_eval_realm();
                        } else {
                            fail_pending = true;
                        }
                    }
                    saves = Some(machine.save());
                    kept_realm = Some(realm);
                }
                Progress::Finished(Completion::Value(value)) => {
                    if prelude_pending {
                        // The prelude is done; its globals stand, and the
                        // closure starts on the same heap.
                        prelude_pending = false;
                        let _ = value;
                        module_position = next_eager_position(
                            &module_order,
                            module_order_count,
                            storage.module_eager,
                            0,
                        );
                        let first = module_order.get(module_position).copied().unwrap_or(0);
                        let registry = if first == 0 {
                            0usize
                        } else {
                            (first as usize).saturating_sub(MODULE_BASE) + 1
                        };
                        started_mask |= 1u32 << registry;
                        if machine.start_module(first).is_err() {
                            return Ran::Stopped;
                        }
                        saves = Some(machine.save());
                        kept_realm = Some(realm);
                        continue;
                    }
                    let finished = module_order.get(module_position).copied().unwrap_or(0);
                    if module && finished != 0 {
                        machine.set_module_completion(finished, value);
                        // A body that answered a pending promise is still
                        // evaluating; it is done when that promise settles.
                        let settled = !(value.is_object()
                            && object::promise_state(machine.heap(), value.as_handle())
                                == Ok(promise::PENDING));
                        if settled {
                            machine.set_module_status(finished, 2);
                        }
                        // Start whichever module is ready: everything it
                        // reaches settled. Nothing ready means an await is
                        // pending, and the jobs run until something settles.
                        let mut round = 0u32;
                        let chosen = loop {
                            let mut position = 0usize;
                            while position < module_order_count {
                                let unit = module_order.get(position).copied().unwrap_or(0);
                                let registry = if unit == 0 {
                                    0usize
                                } else {
                                    (unit as usize).saturating_sub(MODULE_BASE) + 1
                                };
                                if storage.module_eager & (1u32 << registry) != 0
                                    && started_mask & (1u32 << registry) != 0
                                    && machine.module_status(unit) != 2
                                {
                                    let completion = machine.module_completion(unit);
                                    if completion.is_object() {
                                        match object::promise_state(
                                            machine.heap(),
                                            completion.as_handle(),
                                        ) {
                                            Ok(promise::REJECTED) => return Ran::Threw,
                                            Ok(promise::PENDING) => {}
                                            _ => machine.set_module_status(unit, 2),
                                        }
                                    }
                                }
                                position += 1;
                            }
                            let mut pick = None;
                            let mut waiting = false;
                            let mut position = 0usize;
                            while position < module_order_count {
                                let unit = module_order.get(position).copied().unwrap_or(0);
                                let registry = if unit == 0 {
                                    0usize
                                } else {
                                    (unit as usize).saturating_sub(MODULE_BASE) + 1
                                };
                                if storage.module_eager & (1u32 << registry) == 0
                                    || started_mask & (1u32 << registry) != 0
                                {
                                    position += 1;
                                    continue;
                                }
                                let mask =
                                    storage.module_depends.get(registry).copied().unwrap_or(0)
                                        & storage.module_eager;
                                let mut ready = true;
                                let mut reached = 0usize;
                                while reached <= MODULE_SLOTS {
                                    if mask & (1u32 << reached) != 0 {
                                        let held = if reached == 0 {
                                            0u32
                                        } else {
                                            u32::try_from(MODULE_BASE + reached - 1).unwrap_or(0)
                                        };
                                        if machine.module_status(held) != 2 {
                                            ready = false;
                                            break;
                                        }
                                    }
                                    reached += 1;
                                }
                                if ready {
                                    pick = Some((position, unit, registry));
                                    break;
                                }
                                waiting = true;
                                position += 1;
                            }
                            if pick.is_some() {
                                break pick;
                            }
                            if !waiting {
                                break None;
                            }
                            if machine.pending_jobs() == 0 || round >= JOB_ROUNDS {
                                return Ran::Stopped;
                            }
                            match machine.run_jobs(JOB_SLICE) {
                                Ok(_) => {}
                                Err(Completion::Throw(_)) => return Ran::Threw,
                                Err(_) => return Ran::Stopped,
                            }
                            round += 1;
                        };
                        let Some((position, next, registry)) = chosen else {
                            return Ran::Stopped;
                        };
                        module_position = position;
                        started_mask |= 1u32 << registry;
                        if machine.start_module(next).is_err() {
                            return Ran::Stopped;
                        }
                        saves = Some(machine.save());
                        kept_realm = Some(realm);
                    } else {
                        // The body finished; whatever it queued still counts.
                        let mut round = 0u32;
                        while machine.pending_jobs() > 0 && round < JOB_ROUNDS {
                            match machine.run_jobs(JOB_SLICE) {
                                Ok(_) => {}
                                Err(Completion::Throw(_)) => return Ran::Threw,
                                Err(_) => return Ran::Stopped,
                            }
                            round += 1;
                        }
                        // An entry whose own completion promise rejected is
                        // an uncaught error, however quietly it settled.
                        if module
                            && value.is_object()
                            && object::promise_state(machine.heap(), value.as_handle())
                                == Ok(promise::REJECTED)
                        {
                            return Ran::Threw;
                        }
                        return Ran::Finished(machine.print_status() == 1);
                    }
                }
                Progress::Finished(Completion::Throw(_)) => return Ran::Threw,
                Progress::Finished(Completion::Terminated(_)) => return Ran::Stopped,
            }
        }

        // The machine's borrows have ended; compile the staged source.
        if let Some(source_length) = compile_request {
            let source = storage.eval_source.get(..source_length).unwrap_or(&[]);
            // A direct eval compiles against its site's scope, so the slot's
            // identity is the source AND the site: the same text at another
            // site is another unit.
            let mut digest = digest_of(source);
            if compile_script {
                // A script compiles differently from eval code of the same
                // text, and runs its declaration instantiation each time.
                digest = digest.rotate_left(7) ^ 0x5C71;
            }
            // The same text in another realm is another unit: its closures
            // belong to that realm.
            digest ^= u64::from(compile_realm).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            if let Some((module, function, pc)) = compile_site {
                digest ^= digest_of(&module.to_le_bytes())
                    .rotate_left(1)
                    .wrapping_add(u64::from(function) << 32 | u64::from(pc));
            }
            let mut found = None;
            let mut slot = 0usize;
            while slot < eval_count {
                if storage.eval_digests.get(slot).copied() == Some(digest) {
                    found = Some(slot);
                    break;
                }
                slot += 1;
            }
            let target_slot = if eval_count >= EVAL_SLOTS {
                let slot = evict_next;
                evict_next = (evict_next + 1) % EVAL_SLOTS;
                slot
            } else {
                eval_count
            }
            .min(EVAL_SLOTS - 1);
            match found {
                Some(slot) => enter = Some(u32::try_from(slot + 1).unwrap_or(0)),
                None => {
                    let mut front = FrontEnd {
                        starts: &mut storage.starts,
                        nodes: &mut storage.nodes,
                        lists: &mut storage.lists,
                        numbers: &mut storage.numbers,
                        scratch: &mut storage.scratch,
                        code: &mut storage.code,
                        constants: &mut storage.constants,
                        constant_data: &mut storage.constant_data,
                        safe_points: &mut storage.safe_points,
                        patches: &mut storage.patches,
                        labels: &mut storage.labels,
                        verifier_state: &mut storage.verifier_state,
                        unit_code: &mut storage.unit_code,
                        unit_safe_points: &mut storage.unit_safe_points,
                        functions: &mut storage.functions,
                        exceptions: &mut storage.exceptions,
                        scopes: &mut storage.scopes,
                        lexical: &mut storage.lexical,
                        pending: &mut storage.pending,
                        imports: &mut storage.imports,
                        exports: &mut storage.exports,
                        eval_sites: &mut storage.eval_sites,
                        image_digest: &mut storage.image_digest,
                    };
                    let source = storage.eval_source.get(..source_length).unwrap_or(&[]);
                    // The caller's recorded scope, when the pause was a
                    // recorded direct-eval site.
                    let (done, rest) = storage.eval_images.split_at_mut(target_slot);
                    let mut scope = [EvalBinding {
                        name: &[],
                        slot: 0,
                        depth: 0,
                        kind: 0,
                    }; 64];
                    let mut scope_count = 0usize;
                    let mut strict = false;
                    let mut function_site = false;
                    let mut var_env_depth = u32::MAX;
                    let mut super_property = false;
                    let mut super_call = false;
                    let mut new_target = false;
                    let mut deny_arguments = false;
                    let mut privates = false;
                    let mut parameter_site = false;
                    let mut direct = false;
                    if let Some((module, function, pc)) = compile_site {
                        let image: &[u8] = if module == 0 {
                            storage.image.get(..length).unwrap_or(&[])
                        } else if (module as usize - 1) < target_slot {
                            let held = storage
                                .eval_lengths
                                .get(module as usize - 1)
                                .copied()
                                .unwrap_or(0);
                            done.get(module as usize - 1)
                                .and_then(|slot| slot.get(..held))
                                .unwrap_or(&[])
                        } else {
                            // The pausing unit is at or past the slot being
                            // replaced; its record is out of reach here, and
                            // the eval runs as global code.
                            &[]
                        };
                        if let Ok(unit) = Unit::parse(image) {
                            if let Some(site) = evalsite::find(unit.eval_sites(), function, pc) {
                                direct = true;
                                strict = site.flags & evalsite::FLAG_STRICT != 0;
                                function_site = site.flags & evalsite::FLAG_FUNCTION != 0;
                                super_property = site.flags & evalsite::FLAG_SUPER_PROPERTY != 0;
                                super_call = site.flags & evalsite::FLAG_SUPER_CALL != 0;
                                new_target = site.flags & evalsite::FLAG_NEW_TARGET != 0;
                                deny_arguments = site.flags & evalsite::FLAG_NO_ARGUMENTS != 0;
                                privates = site.flags & evalsite::FLAG_PRIVATES != 0;
                                parameter_site = site.flags & evalsite::FLAG_PARAMETERS != 0;
                                var_env_depth = site.var_depth;
                                if site.flags & evalsite::FLAG_TRUNCATED == 0 {
                                    scope_count = site.bindings(&mut scope);
                                }
                            }
                        }
                    }
                    let out = rest.first_mut();
                    let Some(out) = out else {
                        fail_pending = true;
                        continue;
                    };
                    match compile_into(
                        &mut front,
                        source,
                        out,
                        direct,
                        scope.get(..scope_count).unwrap_or(&[]),
                        strict,
                        function_site,
                        var_env_depth,
                        super_property,
                        super_call,
                        new_target,
                        deny_arguments,
                        privates,
                        parameter_site,
                        compile_script,
                    ) {
                        Some(compiled) => {
                            if let Some(slot) = storage.eval_lengths.get_mut(target_slot) {
                                *slot = compiled;
                            }
                            if let Some(slot) = storage.eval_digests.get_mut(target_slot) {
                                *slot = digest;
                            }
                            if target_slot == eval_count {
                                eval_count += 1;
                            }
                            enter = Some(u32::try_from(target_slot + 1).unwrap_or(0));
                        }
                        None => fail_pending = true,
                    }
                }
            }
        }
    }
}

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    input: i32,
    report_out: i32,
    exit_out: i32,
    buffer: [u8; CASE_CAPACITY + HEADER],
    prelude: [u8; PRELUDE_CAPACITY],
    source: [u8; SOURCE_CAPACITY],
    storage: Storage,
    filled: usize,
    prelude_length: usize,
    /// Bytes of an oversized record still to be discarded.
    discarding: usize,
    /// The discarded record's verdict, staged once the bytes are gone.
    discard_verdict: u8,
    passed: u32,
    failed: u32,
    refused: u32,
    stopped: u32,
    skipped: u32,
    cases: u32,
    verdicts: [u8; 256],
    verdict_length: usize,
    /// The digests of every case that carried the digest flag, folded.
    batch_digest: u64,
    digest_cases: u32,
    verdict_offset: usize,
    report: [u8; 160],
    report_length: usize,
    report_offset: usize,
    phase: u8,
}

/// Judge one staged case and answer its verdict byte.
fn judge(state: &mut State, length: usize, expectation: u8) -> u8 {
    let prelude = state.prelude_length;
    // A newline keeps the prelude's last line off the case's first — unless
    // there is no prelude, when the case must start at the very first
    // byte, where a hashbang comment is admitted.
    let gap = usize::from(prelude != 0);
    let total = prelude + gap + length;
    if total > SOURCE_CAPACITY {
        return b'S';
    }
    let strict = expectation & 4 != 0;
    let module = expectation & 8 != 0;
    let wants_digest = expectation & 0x10 != 0;
    let expectation = expectation & 3;
    state.storage.image_digest = 0;
    let ran = if module {
        // A module case keeps the prelude apart: it runs as a script of its
        // own, so its declarations are globals the whole closure sees.
        execute(
            &mut state.storage,
            &state.buffer[HEADER..HEADER + length],
            &state.prelude[..prelude],
            strict,
            true,
        )
    } else {
        state.source[..prelude].copy_from_slice(&state.prelude[..prelude]);
        if gap == 1 {
            state.source[prelude] = b'\n';
        }
        state.source[prelude + gap..total].copy_from_slice(&state.buffer[HEADER..HEADER + length]);
        let source = state.source.get(..total).unwrap_or(&[]);
        execute(&mut state.storage, source, &[], strict, false)
    };
    if wants_digest {
        state.batch_digest = state.batch_digest.rotate_left(7) ^ state.storage.image_digest;
        state.digest_cases = state.digest_cases.saturating_add(1);
    }
    if expectation == 3 {
        // An async case reports through the harness's `$DONE`: the run must
        // finish and the completion line must have been printed.
        return match ran {
            Ran::Refused => b'R',
            Ran::Stopped => b'T',
            Ran::Finished(true) => b'P',
            Ran::Finished(false) | Ran::Threw => b'F',
        };
    }
    match (ran, expectation == 1) {
        (Ran::Refused, _) => b'R',
        (Ran::Stopped, _) => b'T',
        (Ran::Finished(_), false) | (Ran::Threw, true) => b'P',
        (Ran::Threw, false) => b'F',
        (Ran::Finished(_), true) => b'X',
    }
}

fn record_verdict(state: &mut State, verdict: u8) {
    match verdict {
        b'P' => state.passed += 1,
        b'F' | b'X' => state.failed += 1,
        b'R' => state.refused += 1,
        b'T' => state.stopped += 1,
        _ => state.skipped += 1,
    }
    state.cases += 1;
    if state.verdict_length < state.verdicts.len() {
        state.verdicts[state.verdict_length] = verdict;
        state.verdict_length += 1;
    }
}

/// Move `buffer[from..to]` down to the front.
///
/// `copy_within` carries a bounds check that panics, and it is instantiated
/// out of line, so a guard at the call site does not reach it: whether the
/// panic path survives to the link is the optimizer's decision rather than
/// the code's. A checked walk has no such path to begin with, which is what
/// a module that cannot panic needs from a buffer compaction.
fn shift_down(buffer: &mut [u8], from: usize, to: usize) {
    let mut index = 0usize;
    while from + index < to {
        let byte = match buffer.get(from + index) {
            Some(byte) => *byte,
            None => return,
        };
        match buffer.get_mut(index) {
            Some(slot) => *slot = byte,
            None => return,
        }
        index += 1;
    }
}

/// Consume one complete record from the staging buffer, if one is there.
///
/// One record per step: a case executes under a real instruction budget, and
/// a step that ran a whole batch would be a step the scheduler cannot bound.
fn drain_one(state: &mut State) {
    if state.discarding > 0 {
        let drop = state.discarding.min(state.filled);
        if drop == 0 {
            return;
        }
        shift_down(&mut state.buffer, drop, state.filled);
        state.filled -= drop;
        state.discarding -= drop;
        if state.discarding == 0 {
            let verdict = state.discard_verdict;
            record_verdict(state, verdict);
        }
        return;
    }
    if state.filled < HEADER {
        return;
    }
    let length = u32::from_le_bytes([
        state.buffer[0],
        state.buffer[1],
        state.buffer[2],
        state.buffer[3],
    ]) as usize;
    let expectation = state.buffer[4];

    if length > CASE_CAPACITY {
        shift_down(&mut state.buffer, HEADER, state.filled);
        state.filled -= HEADER;
        state.discarding = length;
        state.discard_verdict = b'S';
        return;
    }
    if state.filled < HEADER + length {
        return;
    }

    if expectation == 2 {
        let kept = length.min(PRELUDE_CAPACITY);
        state.prelude[..kept].copy_from_slice(&state.buffer[HEADER..HEADER + kept]);
        state.prelude_length = kept;
    } else if expectation == 13 {
        // The staged module case's own name: a fresh closure begins here.
        let kept = length.min(128);
        state.storage.entry_name[..kept].copy_from_slice(&state.buffer[HEADER..HEADER + kept]);
        state.storage.entry_name_length = kept;
        state.storage.fixture_count = 0;
    } else if expectation == 12 {
        let held = state.filled.min(HEADER + length);
        stage_fixture(&mut state.storage, HEADER, held, &state.buffer);
    } else {
        let verdict = judge(state, length, expectation);
        record_verdict(state, verdict);
        state.storage.fixture_count = 0;
        state.storage.entry_name_length = 0;
    }
    shift_down(&mut state.buffer, HEADER + length, state.filled);
    state.filled -= HEADER + length;
}

fn compose(state: &mut State) -> usize {
    let mut out = [0u8; 160];
    let mut at = 0usize;
    out[at] = b'\n';
    at += 1;
    let prefix = b"phasor-run262: ";
    out[at..at + prefix.len()].copy_from_slice(prefix);
    at += prefix.len();
    for (value, label) in [
        (state.passed, &b" passed "[..]),
        (state.failed, &b" failed "[..]),
        (state.stopped, &b" stopped "[..]),
        (state.refused, &b" refused "[..]),
        (state.skipped, &b" skipped of "[..]),
    ] {
        at += text::put_u32(&mut out[at..], value);
        out[at..at + label.len()].copy_from_slice(label);
        at += label.len();
    }
    at += text::put_u32(&mut out[at..], state.cases);
    let tail = b" cases";
    out[at..at + tail.len()].copy_from_slice(tail);
    at += tail.len();
    if state.digest_cases > 0 {
        let label = b" digest ";
        out[at..at + label.len()].copy_from_slice(label);
        at += label.len();
        let mut shift = 64;
        while shift > 0 {
            shift -= 4;
            let nibble = ((state.batch_digest >> shift) & 0xF) as u8;
            out[at] = if nibble < 10 {
                b'0' + nibble
            } else {
                b'a' + nibble - 10
            };
            at += 1;
        }
    }
    out[at] = b'\n';
    at += 1;
    state.report[..at].copy_from_slice(&out[..at]);
    at
}

entry! {
    State;
    primary { input, report_out }
    inputs {}
    outputs { exit_out = 1 }
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
    if state.syscalls.is_null() || state.input < 0 || state.report_out < 0 {
        return -2;
    }
    // SAFETY: the table pointer was stored by `module_new` and checked
    // non-null above; the loader keeps it live for the module's lifetime.
    let syscalls = unsafe { &*state.syscalls };

    if state.phase == 3 {
        return 1;
    }

    // Verdicts leave before anything else happens, so the stream stays in
    // case order and the staging buffer stays small.
    if state.verdict_length > state.verdict_offset {
        let verdicts = state.verdicts.get(..state.verdict_length).unwrap_or(&[]);
        if !wire::push_progress(
            syscalls,
            state.report_out,
            verdicts,
            &mut state.verdict_offset,
        ) {
            return 0;
        }
        state.verdict_length = 0;
        state.verdict_offset = 0;
    }

    if state.phase == 1 {
        if state.report_length == 0 {
            state.report_length = compose(state);
            state.report_offset = 0;
        }
        let report = state.report.get(..state.report_length).unwrap_or(&[]);
        if !wire::push_progress(syscalls, state.report_out, report, &mut state.report_offset) {
            return 0;
        }
        state.phase = 2;
        return 0;
    }

    if state.phase == 2 {
        if !wire::push_exit(syscalls, state.exit_out, state.failed != 0) {
            return 0;
        }
        state.phase = 3;
        return 1;
    }

    if wire::has_input(syscalls, state.input) {
        let offset = state.filled;
        let room = state.buffer.get_mut(offset..).unwrap_or(&mut []);
        state.filled += wire::read_available(syscalls, state.input, room);
        drain_one(state);
        return 0;
    }

    if !wire::hung_up(syscalls, state.input) {
        return 0;
    }

    // The stream has ended; whole records may still be staged.
    if state.filled >= HEADER || state.discarding > 0 {
        drain_one(state);
        return 0;
    }
    state.phase = 1;
    0
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
