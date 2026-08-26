//! The bytecode verifier.
//!
//! Nothing executes before this passes. The verifier decodes every instruction
//! of every function, checks each operand against the shape the function
//! declares, proves that control transfers land on instruction boundaries
//! inside the function, requires a safe point at the target of every backward
//! edge, and checks that context pushes and pops balance on every path that
//! reaches an instruction.
//!
//! Dynamic call depth is deliberately not claimed to be provable here: the
//! isolate checks the admitted call-stack limit before every call.

use crate::bytecode::{decode, DecodeError, ImageError, Opcode, OperandKind, Unit};
use crate::diagnostic::{code, image as image_argument, image_feature, Diagnostic, Severity};

/// Per-byte verification state for one function's code.
///
/// A slot is `NOT_START` until an instruction is seen to begin there, then
/// `UNVISITED` until a context depth is known, then that depth.
/// Exception regions one function may have open at one point.
const MAX_REGION_NESTING: usize = 32;

const NOT_START: i32 = -1;
const UNVISITED: i32 = -2;

/// Verify every function of a unit image.
///
/// `state` must hold at least one entry per byte of the unit's code section.
pub fn verify(unit: &Unit<'_>, state: &mut [i32]) -> Result<(), Diagnostic> {
    let header = unit.header();
    for index in 0..header.function_count {
        let Some(function) = unit.function(index) else {
            return Err(Diagnostic::at(
                code::INCONSISTENT_DECLARED_BOUNDS,
                Severity::Error,
                index,
            ));
        };
        let Some(code_bytes) = unit.code(&function) else {
            return Err(Diagnostic::at(
                code::INCONSISTENT_DECLARED_BOUNDS,
                Severity::Error,
                index,
            ));
        };
        if function.argument_count > function.register_count
            || function.frame_extent < function.register_count
        {
            return Err(Diagnostic::at(
                code::INCONSISTENT_DECLARED_BOUNDS,
                Severity::Error,
                index,
            ));
        }
        let Some(slots) = state.get_mut(..code_bytes.len()) else {
            return Err(Diagnostic::at(
                code::VERIFIER_STORAGE_TOO_SMALL,
                Severity::Fatal,
                index,
            ));
        };
        verify_function(unit, index, &function, code_bytes, slots)?;
    }
    Ok(())
}

/// Validate an image and then verify it, which is the whole admission path.
pub fn admit<'a>(bytes: &'a [u8], state: &mut [i32]) -> Result<Unit<'a>, Diagnostic> {
    let unit = Unit::parse(bytes).map_err(|error| {
        let (failure, argument) = match error {
            ImageError::Magic => (code::MALFORMED_IMAGE, image_argument::MAGIC),
            ImageError::FormatDigest => (
                code::BYTECODE_FORMAT_MISMATCH,
                image_argument::FORMAT_DIGEST,
            ),
            ImageError::FeatureDigest => {
                (code::FEATURE_LIST_MISMATCH, image_argument::FEATURE_DIGEST)
            }
            ImageError::Truncated => (code::MALFORMED_IMAGE, image_argument::TRUNCATED),
            ImageError::Overflow => (code::MALFORMED_IMAGE, image_argument::OVERFLOW),
        };
        Diagnostic::at(failure, Severity::Error, 0).with(argument)
    })?;
    verify(&unit, state)?;
    Ok(unit)
}

fn verify_function(
    unit: &Unit<'_>,
    index: u32,
    function: &crate::bytecode::Function,
    code_bytes: &[u8],
    state: &mut [i32],
) -> Result<(), Diagnostic> {
    let constant_count = unit.header().constant_count;
    let length = u32::try_from(code_bytes.len()).unwrap_or(u32::MAX);
    for slot in state.iter_mut() {
        *slot = NOT_START;
    }

    // Pass one: instruction boundaries and operand ranges.
    let mut offset = 0u32;
    let mut last_opcode = None;
    while offset < length {
        let instruction = decode(code_bytes, offset).map_err(|error| {
            let failure = match error {
                DecodeError::UnknownOpcode => code::UNKNOWN_OPCODE,
                DecodeError::TruncatedOperand => code::TRUNCATED_OPERAND,
                DecodeError::MisplacedPrefix => code::MISPLACED_PREFIX,
            };
            Diagnostic::at(failure, Severity::Error, offset).with(index)
        })?;
        if let Some(slot) = state.get_mut(offset as usize) {
            *slot = UNVISITED;
        }

        let signature = instruction.opcode.signature();
        let mut operand = 0usize;
        while operand < signature.count as usize {
            let value = instruction.operands[operand];
            match signature.kinds[operand] {
                OperandKind::Register => {
                    if value >= function.register_count {
                        return Err(Diagnostic::at(
                            code::REGISTER_OUT_OF_RANGE,
                            Severity::Error,
                            offset,
                        )
                        .with(value)
                        .with(function.register_count));
                    }
                }
                OperandKind::Constant => {
                    if value >= constant_count {
                        return Err(Diagnostic::at(
                            code::CONSTANT_OUT_OF_RANGE,
                            Severity::Error,
                            offset,
                        )
                        .with(value)
                        .with(constant_count));
                    }
                }
                OperandKind::Count => {
                    // A count of consecutive registers must stay inside the
                    // frame, counted from the register operand before it.
                    if matches!(
                        instruction.opcode,
                        Opcode::Call | Opcode::CallProperty | Opcode::Construct
                    ) {
                        let first = instruction.operands[1];
                        if first.saturating_add(value) > function.register_count {
                            return Err(Diagnostic::at(
                                code::REGISTER_OUT_OF_RANGE,
                                Severity::Error,
                                offset,
                            )
                            .with(first.saturating_add(value))
                            .with(function.register_count));
                        }
                    }
                }
                OperandKind::Depth => {
                    if value > function.context_depth {
                        return Err(Diagnostic::at(
                            code::CONTEXT_DEPTH_OUT_OF_RANGE,
                            Severity::Error,
                            offset,
                        )
                        .with(value)
                        .with(function.context_depth));
                    }
                }
                OperandKind::Immediate | OperandKind::Jump => {}
            }
            operand += 1;
        }

        last_opcode = Some(instruction.opcode);
        offset = offset.saturating_add(instruction.length);
    }
    if offset != length {
        return Err(Diagnostic::at(code::TRUNCATED_OPERAND, Severity::Error, length).with(index));
    }
    match last_opcode {
        Some(opcode) if opcode.is_terminator() => {}
        _ => return Err(Diagnostic::at(code::FALLS_OFF_END, Severity::Error, length).with(index)),
    }

    // Safe points must be ascending instruction boundaries inside this function.
    let mut previous = None;
    for slot in 0..function.safe_point_count {
        let Some(point) = unit.safe_point(function.safe_point_offset + slot) else {
            return Err(
                Diagnostic::at(code::INVALID_SAFE_POINT, Severity::Error, slot).with(index),
            );
        };
        if point >= length || state.get(point as usize).copied() == Some(NOT_START) {
            return Err(
                Diagnostic::at(code::INVALID_SAFE_POINT, Severity::Error, point).with(index),
            );
        }
        if previous.is_some_and(|previous| point <= previous) {
            return Err(
                Diagnostic::at(code::INVALID_SAFE_POINT, Severity::Error, point).with(index),
            );
        }
        previous = Some(point);
    }

    // Exception regions must be ordered, properly nested, and on instruction
    // boundaries. Nesting is admitted because a `try` inside a `try` is
    // ordinary; a partial overlap is not, because the innermost handler for a
    // throw would be ambiguous.
    let mut open: [u32; MAX_REGION_NESTING] = [0; MAX_REGION_NESTING];
    let mut open_count = 0usize;
    let mut previous_start = 0u32;
    for slot in 0..function.exception_count {
        let Some(region) = unit.exception_region(function.exception_offset + slot) else {
            return Err(
                Diagnostic::at(code::INVALID_EXCEPTION_REGION, Severity::Error, slot).with(index),
            );
        };
        if region.start >= region.end
            || region.end > length
            || region.register >= function.register_count
            || state.get(region.start as usize).copied() == Some(NOT_START)
            || state.get(region.handler as usize).copied() == Some(NOT_START)
        {
            return Err(Diagnostic::at(
                code::INVALID_EXCEPTION_REGION,
                Severity::Error,
                region.start,
            )
            .with(index));
        }
        if region.start < previous_start {
            return Err(Diagnostic::at(
                code::OVERLAPPING_EXCEPTION_REGIONS,
                Severity::Error,
                region.start,
            )
            .with(index));
        }
        previous_start = region.start;
        while open_count > 0 && open[open_count - 1] <= region.start {
            open_count -= 1;
        }
        if open_count > 0 && region.end > open[open_count - 1] {
            return Err(Diagnostic::at(
                code::OVERLAPPING_EXCEPTION_REGIONS,
                Severity::Error,
                region.start,
            )
            .with(index));
        }
        if open_count >= open.len() {
            return Err(Diagnostic::at(
                code::INVALID_EXCEPTION_REGION,
                Severity::Error,
                region.start,
            )
            .with(index));
        }
        open[open_count] = region.end;
        open_count += 1;
    }

    // Pass two: control flow, backward-edge safe points, and context balance.
    let mut offset = 0u32;
    let mut depth: Option<i32> = Some(0);
    while offset < length {
        let instruction = decode(code_bytes, offset).map_err(|_| {
            Diagnostic::at(code::TRUNCATED_OPERAND, Severity::Error, offset).with(index)
        })?;

        let recorded = state.get(offset as usize).copied().unwrap_or(NOT_START);
        let current = match (depth, recorded) {
            (Some(reached), UNVISITED) => reached,
            (Some(reached), stored) if stored >= 0 => {
                if stored != reached {
                    return Err(Diagnostic::at(
                        code::CONTEXT_DEPTH_MISMATCH,
                        Severity::Error,
                        offset,
                    )
                    .with(u32::try_from(reached).unwrap_or(0))
                    .with(u32::try_from(stored).unwrap_or(0)));
                }
                reached
            }
            (None, stored) if stored >= 0 => stored,
            _ => {
                return Err(
                    Diagnostic::at(code::UNREACHABLE_CODE, Severity::Error, offset).with(index),
                );
            }
        };
        if let Some(slot) = state.get_mut(offset as usize) {
            *slot = current;
        }

        // A handler is reached by the exception edge from its region, so it
        // inherits the depth its region started at rather than being
        // unreachable.
        let mut region_index = 0u32;
        while region_index < function.exception_count {
            let Some(region) = unit.exception_region(function.exception_offset + region_index)
            else {
                return Err(Diagnostic::at(
                    code::INVALID_EXCEPTION_REGION,
                    Severity::Error,
                    offset,
                )
                .with(index));
            };
            if region.start == offset {
                // The region says what the handler will run at, and unwinding
                // makes that true; it must be what the code at the region's
                // start is at.
                if region.context_depth != u32::try_from(current).unwrap_or(u32::MAX) {
                    return Err(Diagnostic::at(
                        code::CONTEXT_DEPTH_MISMATCH,
                        Severity::Error,
                        region.start,
                    )
                    .with(region.context_depth)
                    .with(u32::try_from(current).unwrap_or(0)));
                }
                let stored = state
                    .get(region.handler as usize)
                    .copied()
                    .unwrap_or(NOT_START);
                if stored >= 0 {
                    if stored != current {
                        return Err(Diagnostic::at(
                            code::CONTEXT_DEPTH_MISMATCH,
                            Severity::Error,
                            region.handler,
                        )
                        .with(u32::try_from(current).unwrap_or(0))
                        .with(u32::try_from(stored).unwrap_or(0)));
                    }
                } else if let Some(slot) = state.get_mut(region.handler as usize) {
                    *slot = current;
                }
            }
            region_index += 1;
        }

        let mut next = match instruction.opcode {
            Opcode::PushContext => current + 1,
            Opcode::PopContext => current - 1,
            _ => current,
        };
        if next < 0 || next > i32::try_from(function.context_depth).unwrap_or(i32::MAX) {
            return Err(
                Diagnostic::at(code::CONTEXT_DEPTH_OUT_OF_RANGE, Severity::Error, offset)
                    .with(u32::try_from(next.max(0)).unwrap_or(0))
                    .with(function.context_depth),
            );
        }
        if matches!(instruction.opcode, Opcode::Return) && next != 0 {
            return Err(
                Diagnostic::at(code::CONTEXT_DEPTH_MISMATCH, Severity::Error, offset)
                    .with(u32::try_from(next).unwrap_or(0))
                    .with(0),
            );
        }

        if instruction.opcode.is_branch() {
            let target = i64::from(offset) + i64::from(instruction.signed[0]);
            if target < 0 || target >= i64::from(length) {
                return Err(
                    Diagnostic::at(code::INVALID_JUMP_TARGET, Severity::Error, offset)
                        .with(u32::try_from(target.max(0)).unwrap_or(u32::MAX)),
                );
            }
            let target = u32::try_from(target).unwrap_or(0);
            let stored = state.get(target as usize).copied().unwrap_or(NOT_START);
            if stored == NOT_START {
                return Err(
                    Diagnostic::at(code::INVALID_JUMP_TARGET, Severity::Error, offset).with(target),
                );
            }
            if target <= offset && !is_safe_point(unit, function, target) {
                return Err(Diagnostic::at(
                    code::BACKWARD_JUMP_WITHOUT_SAFE_POINT,
                    Severity::Error,
                    offset,
                )
                .with(target));
            }
            if stored >= 0 {
                if stored != next {
                    return Err(Diagnostic::at(
                        code::CONTEXT_DEPTH_MISMATCH,
                        Severity::Error,
                        offset,
                    )
                    .with(u32::try_from(next).unwrap_or(0))
                    .with(u32::try_from(stored).unwrap_or(0)));
                }
            } else if let Some(slot) = state.get_mut(target as usize) {
                *slot = next;
            }
        }

        if instruction.opcode.is_terminator() {
            next = -1;
        }
        depth = if next < 0 { None } else { Some(next) };
        offset = offset.saturating_add(instruction.length);
    }
    Ok(())
}

fn is_safe_point(unit: &Unit<'_>, function: &crate::bytecode::Function, target: u32) -> bool {
    let mut slot = 0u32;
    while slot < function.safe_point_count {
        if unit.safe_point(function.safe_point_offset + slot) == Some(target) {
            return true;
        }
        slot += 1;
    }
    false
}
