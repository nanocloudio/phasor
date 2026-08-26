//! The bytecode encoder.
//!
//! A `CodeBuilder` writes instructions into caller-provided storage and records
//! the safe points and forward-jump patches a function needs. A `UnitWriter`
//! then lays the sections out in canonical order, so the same input always
//! produces the same bytes.
//!
//! Operand width is chosen by a fixed rule rather than by search: a backward
//! jump takes the narrowest width its known displacement fits, and a forward
//! jump is always `Wide`, because its displacement is not known when it is
//! written. Everything else takes the narrowest width its operands fit.

use crate::bytecode::{
    format_digest, width_for, Constant, ExceptionRegion, ExportRecord, Function, ImportRecord,
    Opcode, OperandKind, Width, CONSTANT_RECORD_SIZE, EXCEPTION_RECORD_SIZE, FUNCTION_RECORD_SIZE,
    HEADER_SIZE, PREFIX_EXTRA_WIDE, PREFIX_WIDE, UNIT_MAGIC,
};

/// Why encoding stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildError {
    /// The code, patch, safe-point, or image storage is full.
    Full,
    /// A forward jump is further than a `Wide` displacement reaches.
    JumpTooFar,
    /// A label was jumped to but never bound, or bound twice.
    Label,
}

/// A branch target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Label(pub u32);

/// One recorded forward jump waiting for its label.
#[derive(Clone, Copy, Debug, Default)]
pub struct Patch {
    label: u32,
    /// Byte offset of the instruction that jumps.
    instruction: u32,
    /// Byte offset of its displacement operand.
    operand: u32,
}

impl Patch {
    /// An unused slot, for building patch storage without a default value.
    pub const EMPTY: Self = Self {
        label: 0,
        instruction: 0,
        operand: 0,
    };
}

/// An instruction encoder over one function's code.
pub struct CodeBuilder<'a> {
    code: &'a mut [u8],
    length: u32,
    safe_points: &'a mut [u32],
    safe_point_count: u32,
    patches: &'a mut [Patch],
    patch_count: u32,
    labels: &'a mut [u32],
    label_count: u32,
    error: Option<BuildError>,
    /// Whether the last instruction ended the flow.
    terminated: bool,
}

/// The position stored for a label that is not bound yet.
const UNBOUND: u32 = u32::MAX;

impl<'a> CodeBuilder<'a> {
    pub fn new(
        code: &'a mut [u8],
        safe_points: &'a mut [u32],
        patches: &'a mut [Patch],
        labels: &'a mut [u32],
    ) -> Self {
        Self {
            code,
            length: 0,
            safe_points,
            safe_point_count: 0,
            patches,
            patch_count: 0,
            labels,
            label_count: 0,
            error: None,
            terminated: false,
        }
    }

    /// Bytes written so far.
    pub const fn length(&self) -> u32 {
        self.length
    }

    /// The first error, if the builder stopped.
    pub const fn error(&self) -> Option<BuildError> {
        self.error
    }

    fn fail(&mut self, error: BuildError) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }

    fn write_byte(&mut self, byte: u8) {
        match self.code.get_mut(self.length as usize) {
            Some(slot) => {
                *slot = byte;
                self.length += 1;
            }
            None => self.fail(BuildError::Full),
        }
    }

    fn write_operand(&mut self, value: i64, width: Width) {
        let bytes = value as u32;
        let mut index = 0u32;
        while index < width.bytes() {
            self.write_byte(u8::try_from((bytes >> (8 * index)) & 0xFF).unwrap_or(0));
            index += 1;
        }
    }

    /// Encode an instruction with the operands its signature declares.
    pub fn emit(&mut self, opcode: Opcode, operands: &[i64]) {
        if self.error.is_some() {
            return;
        }
        self.terminated = opcode.is_terminator();
        let signature = opcode.signature();
        if operands.len() != signature.count as usize {
            self.fail(BuildError::Label);
            return;
        }
        let width = width_for(signature, operands);
        self.write_prefix(width);
        self.write_byte(opcode.byte());
        for &operand in operands {
            self.write_operand(operand, width);
        }
    }

    fn write_prefix(&mut self, width: Width) {
        match width {
            Width::Narrow => {}
            Width::Wide => self.write_byte(PREFIX_WIDE),
            Width::ExtraWide => self.write_byte(PREFIX_EXTRA_WIDE),
        }
    }

    /// Allocate a label.
    pub fn label(&mut self) -> Label {
        let index = self.label_count;
        match self.labels.get_mut(index as usize) {
            Some(slot) => {
                *slot = UNBOUND;
                self.label_count += 1;
                Label(index)
            }
            None => {
                self.fail(BuildError::Full);
                Label(UNBOUND)
            }
        }
    }

    /// Bind a label to the current position.
    /// Whether the last instruction ends the flow, so anything written next
    /// would be unreachable. A caller that would emit an implicit return or a
    /// statement after one asks first.
    pub const fn terminated(&self) -> bool {
        self.terminated
    }

    pub fn bind(&mut self, label: Label) {
        // Something jumps here, so what follows is reachable again.
        self.terminated = false;
        match self.labels.get_mut(label.0 as usize) {
            Some(slot) if *slot == UNBOUND => *slot = self.length,
            _ => self.fail(BuildError::Label),
        }
    }

    /// Record that the current position is a safe point, which is where a
    /// deadline, cancellation, or collection slice may be observed.
    pub fn safe_point(&mut self) {
        let position = self.length;
        // One position is one safe point. A loop that begins a function marks
        // the position the function's own entry point already marked, and a
        // repeated position is not ascending, which is what the verifier
        // checks for.
        if self.safe_point_count > 0
            && self
                .safe_points
                .get(self.safe_point_count as usize - 1)
                .copied()
                == Some(position)
        {
            return;
        }
        match self.safe_points.get_mut(self.safe_point_count as usize) {
            Some(slot) => {
                *slot = position;
                self.safe_point_count += 1;
            }
            None => self.fail(BuildError::Full),
        }
    }

    /// Emit a branch to `label`, patching it later when the label is ahead.
    pub fn jump(&mut self, opcode: Opcode, label: Label) {
        if self.error.is_some() {
            return;
        }
        if !opcode.is_branch() {
            self.fail(BuildError::Label);
            return;
        }
        // An unconditional jump ends the flow here just as a return does.
        self.terminated = opcode.is_terminator();
        let instruction = self.length;
        let bound = match self.labels.get(label.0 as usize) {
            Some(&position) => position,
            None => {
                self.fail(BuildError::Label);
                return;
            }
        };

        if bound != UNBOUND {
            // Backward: the displacement is known, so take the narrowest width.
            let displacement = i64::from(bound) - i64::from(instruction);
            let width = width_for(opcode.signature(), &[displacement]);
            self.write_prefix(width);
            self.write_byte(opcode.byte());
            self.write_operand(displacement, width);
            return;
        }

        // Forward: reserve a `Wide` displacement and record the patch.
        self.write_prefix(Width::Wide);
        self.write_byte(opcode.byte());
        let operand = self.length;
        self.write_operand(0, Width::Wide);
        let patch = Patch {
            label: label.0,
            instruction,
            operand,
        };
        match self.patches.get_mut(self.patch_count as usize) {
            Some(slot) => {
                *slot = patch;
                self.patch_count += 1;
            }
            None => self.fail(BuildError::Full),
        }
    }

    /// Resolve every forward jump and return the code length and safe points.
    pub fn finish(self) -> Result<u32, BuildError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let mut index = 0usize;
        while index < self.patch_count as usize {
            let patch = self.patches[index];
            let target = match self.labels.get(patch.label as usize) {
                Some(&position) if position != UNBOUND => position,
                _ => return Err(BuildError::Label),
            };
            let displacement = i64::from(target) - i64::from(patch.instruction);
            if !(-32768..=32767).contains(&displacement) {
                return Err(BuildError::JumpTooFar);
            }
            let encoded = (displacement as i32) as u32;
            let at = patch.operand as usize;
            match self.code.get_mut(at..at + 2) {
                Some(slot) => {
                    slot[0] = u8::try_from(encoded & 0xFF).unwrap_or(0);
                    slot[1] = u8::try_from((encoded >> 8) & 0xFF).unwrap_or(0);
                }
                None => return Err(BuildError::Full),
            }
            index += 1;
        }
        Ok(self.length)
    }

    /// The safe points recorded so far, in ascending order.
    pub fn safe_points(&self) -> &[u32] {
        match self.safe_points.get(..self.safe_point_count as usize) {
            Some(slice) => slice,
            None => &[],
        }
    }
}

/// Assembles unit sections into one canonical image.
pub struct UnitWriter<'a> {
    bytes: &'a mut [u8],
    cursor: usize,
    failed: bool,
}

impl<'a> UnitWriter<'a> {
    pub fn new(bytes: &'a mut [u8]) -> Self {
        Self {
            bytes,
            cursor: 0,
            failed: false,
        }
    }

    fn put(&mut self, bytes: &[u8]) {
        match self.bytes.get_mut(self.cursor..self.cursor + bytes.len()) {
            Some(slot) => {
                slot.copy_from_slice(bytes);
                self.cursor += bytes.len();
            }
            None => self.failed = true,
        }
    }

    fn put_u32(&mut self, value: u32) {
        self.put(&value.to_le_bytes());
    }

    /// Write the whole image. Sections appear in the order the reader expects:
    /// header, functions, constants, constant data, code, exception regions,
    /// safe points.
    #[allow(
        clippy::too_many_arguments,
        reason = "the sections are the image's canonical order and grouping them would hide it"
    )]
    pub fn write(
        self,
        functions: &[Function],
        constants: &[Constant],
        constant_data: &[u8],
        code: &[u8],
        exceptions: &[ExceptionRegion],
        safe_points: &[u32],
        entry_function: u32,
    ) -> Result<usize, BuildError> {
        self.write_module(
            functions,
            constants,
            constant_data,
            code,
            exceptions,
            safe_points,
            entry_function,
            &[],
            &[],
            0,
        )
    }

    /// Write a unit that is a module: it also carries what it imports and what
    /// it exports.
    #[allow(
        clippy::too_many_arguments,
        reason = "the sections are the image's canonical order and grouping them would hide which is which"
    )]
    pub fn write_module(
        mut self,
        functions: &[Function],
        constants: &[Constant],
        constant_data: &[u8],
        code: &[u8],
        exceptions: &[ExceptionRegion],
        safe_points: &[u32],
        entry_function: u32,
        imports: &[ImportRecord],
        exports: &[ExportRecord],
        flags: u32,
    ) -> Result<usize, BuildError> {
        self.put(&UNIT_MAGIC);
        self.put(&format_digest().0);
        self.put(&crate::feature::digest().0);
        self.put_u32(u32::try_from(functions.len()).unwrap_or(u32::MAX));
        self.put_u32(u32::try_from(constants.len()).unwrap_or(u32::MAX));
        self.put_u32(u32::try_from(constant_data.len()).unwrap_or(u32::MAX));
        self.put_u32(u32::try_from(code.len()).unwrap_or(u32::MAX));
        self.put_u32(u32::try_from(exceptions.len()).unwrap_or(u32::MAX));
        self.put_u32(u32::try_from(safe_points.len()).unwrap_or(u32::MAX));
        self.put_u32(entry_function);
        self.put_u32(flags);
        self.put_u32(u32::try_from(imports.len()).unwrap_or(u32::MAX));
        self.put_u32(u32::try_from(exports.len()).unwrap_or(u32::MAX));
        self.put_u32(0);
        debug_assert_eq!(self.cursor, HEADER_SIZE);

        for function in functions {
            let at = self.cursor;
            self.put_u32(function.code_offset);
            self.put_u32(function.code_length);
            self.put_u32(function.register_count);
            self.put_u32(function.argument_count);
            self.put_u32(function.frame_extent);
            self.put_u32(function.exception_offset);
            self.put_u32(function.exception_count);
            self.put_u32(function.safe_point_offset);
            self.put_u32(function.safe_point_count);
            self.put_u32(function.context_depth);
            self.put_u32(function.context_slots);
            self.put_u32(function.flags);
            debug_assert_eq!(self.cursor - at, FUNCTION_RECORD_SIZE);
        }

        for constant in constants {
            let at = self.cursor;
            self.put(&[constant.kind as u8, 0, 0, 0]);
            self.put_u32(constant.first);
            self.put_u32(constant.second);
            self.put_u32(0);
            debug_assert_eq!(self.cursor - at, CONSTANT_RECORD_SIZE);
        }

        self.put(constant_data);
        self.put(code);

        for region in exceptions {
            let at = self.cursor;
            self.put_u32(region.start);
            self.put_u32(region.end);
            self.put_u32(region.handler);
            self.put_u32(region.register);
            self.put_u32(region.context_depth);
            debug_assert_eq!(self.cursor - at, EXCEPTION_RECORD_SIZE);
        }

        for &point in safe_points {
            self.put_u32(point);
        }

        for record in imports {
            self.put_u32(record.specifier);
            self.put_u32(record.name);
            self.put_u32(record.slot);
        }

        for record in exports {
            self.put_u32(record.name);
            self.put_u32(record.slot);
        }

        if self.failed {
            return Err(BuildError::Full);
        }
        Ok(self.cursor)
    }
}

/// The operand kinds of an opcode, for callers assembling operand lists.
pub fn operand_kinds(opcode: Opcode) -> ([OperandKind; 3], u8) {
    let signature = opcode.signature();
    (signature.kinds, signature.count)
}
