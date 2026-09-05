//! On-graph conformance probe for the bytecode format, encoder, and verifier.
//!
//! Each bounded step assembles one small unit image into module-owned storage
//! and checks that the verifier admits or rejects it for the stated reason.

#![cfg_attr(not(feature = "host-test"), no_std)]
#![allow(
    dead_code,
    unused_imports,
    unreachable_patterns,
    reason = "the Fluxor ABI source is mounted as one surface and this fixture consumes a subset"
)]

#[path = "../../../target/fluxor/fluxor-abi/sdk/abi.rs"]
mod abi;
use abi::SyscallTable;

include!("../../../target/fluxor/fluxor-abi/sdk/runtime.rs");

#[macro_use]
#[path = "../../common/entry.rs"]
mod entry;

#[path = "../../common/bytecode.rs"]
mod bytecode;
#[path = "../../common/diagnostic.rs"]
mod diagnostic;
#[path = "../../common/digest.rs"]
mod digest;
#[path = "../../common/emit.rs"]
mod emit;
#[path = "../../common/feature.rs"]
mod feature;
#[path = "../../common/probe.rs"]
mod probe;
#[path = "../../common/text.rs"]
mod text;
#[path = "../../common/verify.rs"]
mod verify;
#[path = "../../common/wire.rs"]
mod wire;

use bytecode::{
    decode, Constant, ConstantKind, ExceptionRegion, Function, Opcode, Unit, Width, PREFIX_WIDE,
};
use diagnostic::code;
use emit::{CodeBuilder, Patch, UnitWriter};

const CASE_COUNT: u16 = 29;
const CODE_CAPACITY: usize = 256;
const IMAGE_CAPACITY: usize = 1024;
const SAFE_POINT_CAPACITY: usize = 16;
const PATCH_CAPACITY: usize = 16;
const LABEL_CAPACITY: usize = 16;
const STATE_CAPACITY: usize = 256;

/// Module-owned assembly storage, so no large buffer is built on the stack.
struct Storage {
    code: [u8; CODE_CAPACITY],
    image: [u8; IMAGE_CAPACITY],
    safe_points: [u32; SAFE_POINT_CAPACITY],
    patches: [Patch; PATCH_CAPACITY],
    labels: [u32; LABEL_CAPACITY],
    state: [i32; STATE_CAPACITY],
}

impl Storage {
    const fn new() -> Self {
        Self {
            code: [0; CODE_CAPACITY],
            image: [0; IMAGE_CAPACITY],
            safe_points: [0; SAFE_POINT_CAPACITY],
            patches: [Patch::EMPTY; PATCH_CAPACITY],
            labels: [0; LABEL_CAPACITY],
            state: [0; STATE_CAPACITY],
        }
    }
}

/// How one case's function is declared.
#[derive(Clone, Copy)]
struct Shape {
    registers: u32,
    context_depth: u32,
    constants: u32,
}

impl Shape {
    const DEFAULT: Self = Self {
        registers: 4,
        context_depth: 2,
        constants: 2,
    };
}

/// Assemble a program, wrap it in a unit, and return the verifier's verdict as
/// a diagnostic code, with zero meaning admitted.
fn admit(storage: &mut Storage, shape: Shape, assemble: impl FnOnce(&mut CodeBuilder)) -> u16 {
    let mut builder = CodeBuilder::new(
        &mut storage.code,
        &mut storage.safe_points,
        &mut storage.patches,
        &mut storage.labels,
    );
    assemble(&mut builder);
    let points = builder.safe_points().len();
    let Ok(length) = builder.finish() else {
        return u16::MAX;
    };

    let function = Function {
        code_offset: 0,
        code_length: length,
        register_count: shape.registers,
        argument_count: 0,
        frame_extent: shape.registers,
        exception_offset: 0,
        exception_count: 0,
        safe_point_offset: 0,
        safe_point_count: u32::try_from(points).unwrap_or(0),
        context_depth: shape.context_depth,
        context_slots: 0,
        flags: 0,
    };
    let constants = [Constant::number(1.5), Constant::number(2.5)];
    let Some(constants) = constants.get(..shape.constants as usize) else {
        return u16::MAX;
    };
    let Some(code) = storage.code.get(..length as usize) else {
        return u16::MAX;
    };
    let Some(points) = storage.safe_points.get(..points) else {
        return u16::MAX;
    };

    // The image is assembled from copies so the borrow of the code buffer ends
    // before the verifier reads the image.
    let mut code_copy = [0u8; CODE_CAPACITY];
    let Some(target) = code_copy.get_mut(..code.len()) else {
        return u16::MAX;
    };
    target.copy_from_slice(code);
    let mut point_copy = [0u32; SAFE_POINT_CAPACITY];
    let Some(target) = point_copy.get_mut(..points.len()) else {
        return u16::MAX;
    };
    target.copy_from_slice(points);
    let point_count = points.len();
    let code_length = code.len();

    let written = UnitWriter::new(&mut storage.image).write(
        &[function],
        constants,
        &[],
        code_copy.get(..code_length).unwrap_or(&[]),
        &[],
        point_copy.get(..point_count).unwrap_or(&[]),
        0,
    );
    let Ok(written) = written else {
        return u16::MAX;
    };
    verdict(&storage.image, written, &mut storage.state)
}

/// Assemble a unit whose constant table holds a BigInt, which no source this
/// build compiles can produce, and return the verifier's verdict.
fn admit_bigint(storage: &mut Storage) -> u16 {
    let mut builder = CodeBuilder::new(
        &mut storage.code,
        &mut storage.safe_points,
        &mut storage.patches,
        &mut storage.labels,
    );
    builder.emit(Opcode::LdaConstant, &[0]);
    builder.emit(Opcode::Return, &[]);
    let Ok(length) = builder.finish() else {
        return u16::MAX;
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
        safe_point_count: 0,
        context_depth: 0,
        context_slots: 0,
        flags: 0,
    };
    let mut code_copy = [0u8; CODE_CAPACITY];
    let Some(code) = storage.code.get(..length as usize) else {
        return u16::MAX;
    };
    let Some(target) = code_copy.get_mut(..code.len()) else {
        return u16::MAX;
    };
    target.copy_from_slice(code);
    let code_length = code.len();
    let constants = [Constant {
        kind: ConstantKind::BigInt,
        first: 0,
        second: 2,
    }];
    let written = UnitWriter::new(&mut storage.image).write(
        &[function],
        &constants,
        b"12",
        code_copy.get(..code_length).unwrap_or(&[]),
        &[],
        &[],
        0,
    );
    let Ok(written) = written else {
        return u16::MAX;
    };
    verdict(&storage.image, written, &mut storage.state)
}

fn verdict(image: &[u8], written: usize, state: &mut [i32]) -> u16 {
    let Some(bytes) = image.get(..written) else {
        return u16::MAX;
    };
    match verify::admit(bytes, state) {
        Ok(_) => 0,
        Err(diagnostic) => diagnostic.code(),
    }
}

#[allow(
    clippy::match_same_arms,
    reason = "each case is an independent assertion and merging arms would hide which one failed"
)]
fn run_case(storage: &mut Storage, case: u16) -> bool {
    match case {
        // The format digest is derived from the encoding and is stable.
        0 => bytecode::format_digest() == bytecode::format_digest(),
        1 => bytecode::format_digest().0 != [0u8; 32],

        // Encoding and decoding.
        2 => {
            let mut builder = CodeBuilder::new(
                &mut storage.code,
                &mut storage.safe_points,
                &mut storage.patches,
                &mut storage.labels,
            );
            builder.emit(Opcode::LdaSmi, &[7]);
            builder.emit(Opcode::Return, &[]);
            let Ok(length) = builder.finish() else {
                return false;
            };
            let Ok(instruction) = decode(&storage.code, 0) else {
                return false;
            };
            length == 3
                && matches!(instruction.opcode, Opcode::LdaSmi)
                && matches!(instruction.width, Width::Narrow)
                && instruction.signed[0] == 7
        }
        3 => {
            let mut builder = CodeBuilder::new(
                &mut storage.code,
                &mut storage.safe_points,
                &mut storage.patches,
                &mut storage.labels,
            );
            builder.emit(Opcode::LdaSmi, &[30_000]);
            builder.emit(Opcode::Return, &[]);
            let Ok(_) = builder.finish() else {
                return false;
            };
            let Ok(instruction) = decode(&storage.code, 0) else {
                return false;
            };
            matches!(instruction.width, Width::Wide) && instruction.signed[0] == 30_000
        }
        4 => {
            let mut builder = CodeBuilder::new(
                &mut storage.code,
                &mut storage.safe_points,
                &mut storage.patches,
                &mut storage.labels,
            );
            builder.emit(Opcode::LdaSmi, &[-100_000]);
            builder.emit(Opcode::Return, &[]);
            let Ok(_) = builder.finish() else {
                return false;
            };
            let Ok(instruction) = decode(&storage.code, 0) else {
                return false;
            };
            matches!(instruction.width, Width::ExtraWide) && instruction.signed[0] == -100_000
        }

        // Programs the verifier admits.
        5 => {
            admit(storage, Shape::DEFAULT, |builder| {
                builder.emit(Opcode::LdaConstant, &[0]);
                builder.emit(Opcode::Star, &[0]);
                builder.emit(Opcode::LdaSmi, &[2]);
                builder.emit(Opcode::Add, &[0]);
                builder.emit(Opcode::Return, &[]);
            }) == 0
        }
        6 => {
            admit(storage, Shape::DEFAULT, |builder| {
                let top = builder.label();
                builder.bind(top);
                builder.safe_point();
                builder.emit(Opcode::LdaFalse, &[]);
                let done = builder.label();
                builder.jump(Opcode::JumpIfFalse, done);
                builder.jump(Opcode::Jump, top);
                builder.bind(done);
                builder.emit(Opcode::Return, &[]);
            }) == 0
        }
        7 => {
            admit(storage, Shape::DEFAULT, |builder| {
                builder.emit(Opcode::PushContext, &[2]);
                builder.emit(Opcode::LdaContextSlot, &[0, 0]);
                builder.emit(Opcode::PopContext, &[]);
                builder.emit(Opcode::Return, &[]);
            }) == 0
        }
        8 => {
            admit(storage, Shape::DEFAULT, |builder| {
                builder.emit(Opcode::CreateEmptyArray, &[]);
                builder.emit(Opcode::Star, &[0]);
                builder.emit(Opcode::LdaSmi, &[1]);
                builder.emit(Opcode::AppendArrayElement, &[0]);
                builder.emit(Opcode::Ldar, &[0]);
                builder.emit(Opcode::Return, &[]);
            }) == 0
        }
        9 => {
            admit(storage, Shape::DEFAULT, |builder| {
                builder.emit(Opcode::LdaUndefined, &[]);
                builder.emit(Opcode::Star, &[1]);
                builder.emit(Opcode::Call, &[0, 1, 2]);
                builder.emit(Opcode::Return, &[]);
            }) == 0
        }

        // Programs the verifier rejects.
        10 => {
            admit(storage, Shape::DEFAULT, |builder| {
                let top = builder.label();
                builder.bind(top);
                builder.emit(Opcode::LdaTrue, &[]);
                builder.jump(Opcode::Jump, top);
            }) == code::BACKWARD_JUMP_WITHOUT_SAFE_POINT
        }
        11 => {
            admit(storage, Shape::DEFAULT, |builder| {
                builder.emit(Opcode::PushContext, &[1]);
                builder.emit(Opcode::Return, &[]);
            }) == code::CONTEXT_DEPTH_MISMATCH
        }
        12 => {
            admit(storage, Shape::DEFAULT, |builder| {
                builder.emit(Opcode::PopContext, &[]);
                builder.emit(Opcode::Return, &[]);
            }) == code::CONTEXT_DEPTH_OUT_OF_RANGE
        }
        13 => {
            admit(storage, Shape::DEFAULT, |builder| {
                builder.emit(Opcode::Ldar, &[9]);
                builder.emit(Opcode::Return, &[]);
            }) == code::REGISTER_OUT_OF_RANGE
        }
        14 => {
            admit(storage, Shape::DEFAULT, |builder| {
                builder.emit(Opcode::LdaConstant, &[7]);
                builder.emit(Opcode::Return, &[]);
            }) == code::CONSTANT_OUT_OF_RANGE
        }
        15 => {
            admit(storage, Shape::DEFAULT, |builder| {
                builder.emit(Opcode::Call, &[0, 1, 8]);
                builder.emit(Opcode::Return, &[]);
            }) == code::REGISTER_OUT_OF_RANGE
        }
        16 => {
            admit(storage, Shape::DEFAULT, |builder| {
                builder.emit(Opcode::LdaTrue, &[]);
            }) == code::FALLS_OFF_END
        }
        17 => {
            let shape = Shape {
                context_depth: 0,
                ..Shape::DEFAULT
            };
            admit(storage, shape, |builder| {
                builder.emit(Opcode::LdaContextSlot, &[0, 1]);
                builder.emit(Opcode::Return, &[]);
            }) == code::CONTEXT_DEPTH_OUT_OF_RANGE
        }

        // Malformed images, checked by hand-written bytes.
        18 => raw_verdict(storage, &[0xEE, Opcode::Return.byte()]) == code::UNKNOWN_OPCODE,
        19 => {
            raw_verdict(storage, &[PREFIX_WIDE, PREFIX_WIDE, Opcode::Return.byte()])
                == code::MISPLACED_PREFIX
        }
        20 => raw_verdict(storage, &[Opcode::LdaSmi.byte()]) == code::TRUNCATED_OPERAND,
        21 => {
            // A jump whose displacement lands inside the previous instruction.
            let code_bytes = [
                Opcode::LdaSmi.byte(),
                9,
                Opcode::Jump.byte(),
                (-1i8) as u8,
                Opcode::Return.byte(),
            ];
            raw_verdict(storage, &code_bytes) == code::INVALID_JUMP_TARGET
        }
        22 => {
            // Corrupting the format digest field must fail admission.
            let written = raw_image(storage, &[Opcode::Return.byte()]);
            if let Some(byte) = storage.image.get_mut(10) {
                *byte ^= 0xFF;
            }
            verdict(&storage.image, written, &mut storage.state) == code::BYTECODE_FORMAT_MISMATCH
        }
        23 => {
            // Corrupting the magic must fail admission.
            let written = raw_image(storage, &[Opcode::Return.byte()]);
            if let Some(byte) = storage.image.get_mut(0) {
                *byte = b'X';
            }
            verdict(&storage.image, written, &mut storage.state) == code::MALFORMED_IMAGE
        }
        // A BigInt constant is an ordinary constant now that the runtime has
        // the values to go with it.
        24 => admit_bigint(storage) == 0,
        25 => {
            // An array literal with a spread compiles to an iteration, so the
            // opcodes an image may carry are the ordinary ones.
            admit(storage, Shape::DEFAULT, |builder| {
                builder.emit(Opcode::CreateEmptyArray, &[]);
                builder.emit(Opcode::Star, &[0]);
                builder.emit(Opcode::AppendArrayElement, &[0]);
                builder.emit(Opcode::Return, &[]);
            }) == 0
        }

        // The feature list binds an image as much as the encoding does.
        26 => feature::digest() == feature::digest() && feature::digest().0 != [0u8; 32],
        27 => {
            // An image compiled against a different admitted language is
            // refused, even though its encoding is this build's.
            let written = raw_image(storage, &[Opcode::Return.byte()]);
            if let Some(byte) = storage.image.get_mut(36) {
                *byte ^= 0xFF;
            }
            verdict(&storage.image, written, &mut storage.state) == code::FEATURE_LIST_MISMATCH
        }
        28 => {
            // Every feature has a name and a version, and no two share a name.
            let mut index = 0usize;
            let mut distinct = true;
            while index < feature::FEATURES.len() {
                let entry = feature::FEATURES[index];
                if entry.length == 0 || entry.version == 0 {
                    distinct = false;
                }
                let mut other = index + 1;
                while other < feature::FEATURES.len() {
                    if feature::FEATURES[other].name() == entry.name() {
                        distinct = false;
                    }
                    other += 1;
                }
                index += 1;
            }
            distinct && !feature::FEATURES.is_empty()
        }

        _ => true,
    }
}

/// Wrap hand-written code bytes in a unit image and return its length.
fn raw_image(storage: &mut Storage, code_bytes: &[u8]) -> usize {
    let function = Function {
        code_offset: 0,
        code_length: u32::try_from(code_bytes.len()).unwrap_or(0),
        register_count: 4,
        argument_count: 0,
        frame_extent: 4,
        exception_offset: 0,
        exception_count: 0,
        safe_point_offset: 0,
        safe_point_count: 0,
        context_depth: 0,
        context_slots: 0,
        flags: 0,
    };
    UnitWriter::new(&mut storage.image)
        .write(&[function], &[], &[], code_bytes, &[], &[], 0)
        .unwrap_or(0)
}

fn raw_verdict(storage: &mut Storage, code_bytes: &[u8]) -> u16 {
    let written = raw_image(storage, code_bytes);
    verdict(&storage.image, written, &mut storage.state)
}

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    report_out: i32,
    exit_out: i32,
    storage: Storage,
    progress: probe::Progress,
}

entry! {
    State;
    primary { report_out }
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
    if state.syscalls.is_null() {
        return -2;
    }
    // SAFETY: the table pointer was stored by `module_new` and checked
    // non-null above; the loader keeps it live for the module's lifetime.
    let syscalls = unsafe { &*state.syscalls };
    probe::step(
        &mut state.progress,
        syscalls,
        state.report_out,
        state.exit_out,
        b"phasor-bytecode-probe",
        CASE_COUNT,
        |case| run_case(&mut state.storage, case),
    )
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
