//! The constants a unit carries: numbers, text, keys, specifiers, and the interning that makes each one once.

use super::*;

/// The first occurrence of `needle` in `haystack`, as a source position.
pub(super) fn find_text(haystack: &[u8], needle: &[u8]) -> Option<u32> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let mut index = 0usize;
    while index + needle.len() <= haystack.len() {
        if &haystack[index..index + needle.len()] == needle {
            return u32::try_from(index).ok();
        }
        index += 1;
    }
    None
}

impl Lowering<'_, '_, '_, '_> {
    // Statements.

    /// The constant holding the name `default`, which is what a default export
    /// is bound to and a default import asks for.
    pub(super) fn default_key_constant(&mut self) -> u32 {
        let offset = self.program.constant_data_length;
        let text = [
            b'd', 0, b'e', 0, b'f', 0, b'a', 0, b'u', 0, b'l', 0, b't', 0,
        ];
        let Some(space) = self
            .program
            .constant_data
            .get_mut(offset..offset + text.len())
        else {
            let node = Node::new(NodeKind::Null, 0, 0);
            self.fail(&node, code::TOO_MANY_CONSTANTS);
            return 0;
        };
        space.copy_from_slice(&text);
        if let Some(existing) = self.find_text(ConstantKind::Key, offset, text.len()) {
            return existing;
        }
        self.program.constant_data_length += text.len();
        self.intern(Constant {
            kind: ConstantKind::Key,
            first: u32::try_from(offset).unwrap_or(0),
            second: 7,
        })
    }

    /// The slot a default export's value is held in.
    pub(super) fn default_slot(&mut self) -> u32 {
        // The default export's binding is the one with no name: no
        // identifier can spell an empty span, so it is unmistakable.
        let record = self.program.scope(self.function_scope);
        let first = record.first as usize;
        let count = record.count;
        let mut index = 0u32;
        while index < count {
            if let Some(binding) = self.program.bindings.get(first + index as usize) {
                if binding.start == binding.end && binding.kind == binding_kind::LET {
                    return binding.slot;
                }
            }
            index += 1;
        }
        self.default_export_slot
    }

    /// The constant holding the name `length`, which the loops that walk an
    /// array-like need.
    pub(super) fn length_key_constant(&mut self) -> u32 {
        self.text_key_constant(b"length")
    }

    /// A key constant for a name the lowering itself needs, staged as the
    /// UTF-16 the constant table holds.
    /// A key constant from UTF-16 units directly.
    pub(super) fn unit_text_constant(&mut self, units: &[u16]) -> u32 {
        let offset = self.program.constant_data_length;
        let length = units.len() * 2;
        let Some(space) = self.program.constant_data.get_mut(offset..offset + length) else {
            let node = Node::new(NodeKind::Null, 0, 0);
            self.fail(&node, code::TOO_MANY_CONSTANTS);
            return 0;
        };
        for (index, &unit) in units.iter().enumerate() {
            let bytes = unit.to_le_bytes();
            space[index * 2] = bytes[0];
            space[index * 2 + 1] = bytes[1];
        }
        if let Some(existing) = self.find_text(ConstantKind::Key, offset, length) {
            return existing;
        }
        self.program.constant_data_length += length;
        self.intern(Constant {
            kind: ConstantKind::Key,
            first: u32::try_from(offset).unwrap_or(0),
            second: u32::try_from(units.len()).unwrap_or(0),
        })
    }

    pub(super) fn text_key_constant(&mut self, name: &[u8]) -> u32 {
        let offset = self.program.constant_data_length;
        let length = name.len() * 2;
        let Some(space) = self.program.constant_data.get_mut(offset..offset + length) else {
            let node = Node::new(NodeKind::Null, 0, 0);
            self.fail(&node, code::TOO_MANY_CONSTANTS);
            return 0;
        };
        for (index, &byte) in name.iter().enumerate() {
            space[index * 2] = byte;
            space[index * 2 + 1] = 0;
        }
        if let Some(existing) = self.find_text(ConstantKind::Key, offset, length) {
            return existing;
        }
        self.program.constant_data_length += length;
        self.intern(Constant {
            kind: ConstantKind::Key,
            first: u32::try_from(offset).unwrap_or(0),
            second: u32::try_from(name.len()).unwrap_or(0),
        })
    }

    // Constant interning. Equal constants share a slot, so the table stays
    // small and identical sources produce identical images.

    pub(super) fn intern(&mut self, constant: Constant) -> u32 {
        let mut index = 0usize;
        while index < self.program.constant_count {
            let existing = self.program.constants[index];
            if existing.kind as u8 == constant.kind as u8
                && existing.first == constant.first
                && existing.second == constant.second
            {
                return u32::try_from(index).unwrap_or(0);
            }
            index += 1;
        }
        match self.program.constants.get_mut(self.program.constant_count) {
            Some(slot) => {
                *slot = constant;
                let index = u32::try_from(self.program.constant_count).unwrap_or(0);
                self.program.constant_count += 1;
                index
            }
            None => {
                if self.program.failure.is_none() {
                    self.program.failure = Some(failure(code::TOO_MANY_CONSTANTS));
                }
                0
            }
        }
    }

    pub(super) fn number_constant(&mut self, value: f64) -> u32 {
        self.intern(Constant::number(value))
    }

    /// Intern the cooked text of a literal or name span as UTF-16 data.
    pub(super) fn text_constant(
        &mut self,
        node: &Node,
        kind: ConstantKind,
        token_kind: TokenKind,
    ) -> u32 {
        let token = Token {
            kind: token_kind,
            start: node.start,
            end: node.end,
            inner_start: node.first,
            inner_end: node.second,
            line_break_before: false,
            escaped: false,
            spells_reserved: false,
            cooked_valid: true,
            number: 0.0,
            code_units: node.end.saturating_sub(node.start),
            radix: 10,
            flags: 0,
        };
        let offset = self.program.constant_data_length;
        let Some(space) = self.program.constant_data.get_mut(offset..) else {
            self.fail(node, code::TOO_MANY_CONSTANTS);
            return 0;
        };
        // Cook straight into the spare data area as UTF-16, two bytes per
        // code unit: a literal is as long as the data area has room for.
        let mut sink = crate::lex::LittleEndianUnits(space);
        let Some(written) = crate::lex::cook_into(self.source, &token, &mut sink) else {
            self.fail(node, code::TOO_MANY_CONSTANTS);
            return 0;
        };
        // Reuse an identical constant rather than storing its text twice, so
        // the same source always produces the same table.
        if let Some(existing) = self.find_text(kind, offset, written * 2) {
            return existing;
        }
        self.program.constant_data_length += written * 2;
        self.intern(Constant {
            kind,
            first: u32::try_from(offset).unwrap_or(0),
            second: u32::try_from(written).unwrap_or(0),
        })
    }

    /// A module specifier's constant with the marker its import attributes
    /// chose appended after a 0x01 unit: the loader stages `type: 'json'`,
    /// `'text'`, and `'bytes'` variants under exactly that spelling, and an
    /// attribute nothing supports spells a name that links to nothing.
    /// A module specifier's constant: plain, or spelled with its import
    /// attributes' marker.
    pub(super) fn specifier_constant(&mut self, node: &Node, attributes: u8) -> u32 {
        match attributes {
            0 => self.text_constant(node, ConstantKind::String, TokenKind::String),
            1 => self.attributed_specifier_constant(node, b'j'),
            2 => self.attributed_specifier_constant(node, b't'),
            3 => self.attributed_specifier_constant(node, b'b'),
            _ => self.attributed_specifier_constant(node, b'?'),
        }
    }

    pub(super) fn attributed_specifier_constant(&mut self, node: &Node, marker: u8) -> u32 {
        let token = Token {
            kind: TokenKind::String,
            start: node.start,
            end: node.end,
            inner_start: node.first,
            inner_end: node.second,
            line_break_before: false,
            escaped: false,
            spells_reserved: false,
            cooked_valid: true,
            number: 0.0,
            code_units: node.end.saturating_sub(node.start),
            radix: 10,
            flags: 0,
        };
        let offset = self.program.constant_data_length;
        let Some(space) = self.program.constant_data.get_mut(offset..) else {
            self.fail(node, code::TOO_MANY_CONSTANTS);
            return 0;
        };
        let mut sink = crate::lex::LittleEndianUnits(space);
        let Some(written) = crate::lex::cook_into(self.source, &token, &mut sink) else {
            self.fail(node, code::TOO_MANY_CONSTANTS);
            return 0;
        };
        let tail = written * 2;
        let Some(space) = self
            .program
            .constant_data
            .get_mut(offset + tail..offset + tail + 4)
        else {
            self.fail(node, code::TOO_MANY_CONSTANTS);
            return 0;
        };
        space[0] = 1;
        space[1] = 0;
        space[2] = marker;
        space[3] = 0;
        let written = written + 2;
        if let Some(existing) = self.find_text(ConstantKind::String, offset, written * 2) {
            return existing;
        }
        self.program.constant_data_length += written * 2;
        self.intern(Constant {
            kind: ConstantKind::String,
            first: u32::try_from(offset).unwrap_or(0),
            second: u32::try_from(written).unwrap_or(0),
        })
    }

    /// The hidden name an `accessor` field stores behind: NUL, `acc `, and
    /// the key's own text — the NUL keeps it off every reflective surface,
    /// and the getter and setter derive the same name from the key.
    pub(super) fn accessor_backing_constant(&mut self, index: u32) -> u32 {
        let node = self.node(index);
        let token_kind = match node.kind {
            NodeKind::PropertyName => match node.third {
                property_key::STRING => TokenKind::String,
                property_key::NUMBER => {
                    self.fail(&node, code::LOWERING_NOT_ADMITTED);
                    return 0;
                }
                _ => TokenKind::Identifier,
            },
            NodeKind::Identifier => TokenKind::Identifier,
            _ => {
                self.fail(&node, code::LOWERING_NOT_ADMITTED);
                return 0;
            }
        };
        let token = Token {
            kind: token_kind,
            start: node.start,
            end: node.end,
            inner_start: node.first,
            inner_end: node.second,
            line_break_before: false,
            escaped: false,
            spells_reserved: false,
            cooked_valid: true,
            number: 0.0,
            code_units: node.end.saturating_sub(node.start),
            radix: 10,
            flags: 0,
        };
        let offset = self.program.constant_data_length;
        let Some(space) = self.program.constant_data.get_mut(offset..) else {
            self.fail(&node, code::TOO_MANY_CONSTANTS);
            return 0;
        };
        if space.len() < 10 {
            self.fail(&node, code::TOO_MANY_CONSTANTS);
            return 0;
        }
        let prefix = [
            0u16,
            u16::from(b'a'),
            u16::from(b'c'),
            u16::from(b'c'),
            u16::from(b' '),
        ];
        let mut at = 0usize;
        for &unit in &prefix {
            space[at..at + 2].copy_from_slice(&unit.to_le_bytes());
            at += 2;
        }
        let mut sink = crate::lex::LittleEndianUnits(&mut space[10..]);
        let Some(written) = crate::lex::cook_into(self.source, &token, &mut sink) else {
            self.fail(&node, code::TOO_MANY_CONSTANTS);
            return 0;
        };
        let total = written + 5;
        if let Some(existing) = self.find_text(ConstantKind::Key, offset, total * 2) {
            return existing;
        }
        self.program.constant_data_length += total * 2;
        self.intern(Constant {
            kind: ConstantKind::Key,
            first: u32::try_from(offset).unwrap_or(0),
            second: u32::try_from(total).unwrap_or(0),
        })
    }

    /// The index of a constant whose data equals the `length` bytes just
    /// written at `offset`, if the table already holds one.
    pub(super) fn find_text(
        &self,
        kind: ConstantKind,
        offset: usize,
        length: usize,
    ) -> Option<u32> {
        let mut index = 0usize;
        while index < self.program.constant_count {
            let existing = self.program.constants[index];
            if existing.kind as u8 == kind as u8 {
                let start = existing.first as usize;
                let existing_length = match kind {
                    ConstantKind::BigInt => existing.second as usize,
                    _ => existing.second as usize * 2,
                };
                if existing_length == length {
                    let left = self.program.constant_data.get(start..start + length);
                    let right = self.program.constant_data.get(offset..offset + length);
                    if left.is_some() && left == right {
                        return u32::try_from(index).ok();
                    }
                }
            }
            index += 1;
        }
        None
    }

    pub(super) fn key_constant(&mut self, index: u32) -> u32 {
        let node = self.node(index);
        match node.kind {
            NodeKind::PropertyName => match node.third {
                property_key::STRING => {
                    self.text_constant(&node, ConstantKind::Key, TokenKind::String)
                }
                property_key::NUMBER => {
                    // A numeric key is its Number value's canonical text, which
                    // the isolate produces; the constant carries the value the
                    // lexer read, whatever form the source wrote it in.
                    let value = self.arena.number(node.first);
                    self.number_constant(value)
                }
                _ => self.text_constant(&node, ConstantKind::Key, TokenKind::Identifier),
            },
            NodeKind::Identifier => {
                self.text_constant(&node, ConstantKind::Key, TokenKind::Identifier)
            }
            // An export or import name can be any string: the key is its
            // cooked text, exactly as a quoted property name's would be.
            NodeKind::String => self.text_constant(&node, ConstantKind::Key, TokenKind::String),
            _ => {
                self.fail(&node, code::LOWERING_NOT_ADMITTED);
                0
            }
        }
    }

    /// A pattern constant: the flags in one code unit, then the pattern.
    pub(super) fn regexp_constant(&mut self, node: &Node) -> u32 {
        let offset = self.program.constant_data_length;
        let flags = u16::try_from(node.third).unwrap_or(0).to_le_bytes();
        match self.program.constant_data.get_mut(offset..offset + 2) {
            Some(slot) => {
                slot[0] = flags[0];
                slot[1] = flags[1];
            }
            None => {
                self.fail(node, code::TOO_MANY_CONSTANTS);
                return 0;
            }
        }
        // The pattern is source text, and its own escapes belong to the pattern
        // grammar rather than to the string grammar, so it is copied as it was
        // written — decoded from the transport's UTF-8 into the code units
        // JavaScript strings are made of, a pair for anything beyond one.
        let mut units = [0u16; 512];
        let mut count = 0usize;
        let text = self.span(node.first, node.second);
        let mut at = 0usize;
        while at < text.len() {
            let byte = text.get(at).copied().unwrap_or(0);
            let tail =
                |offset: usize| u32::from(text.get(at + offset).copied().unwrap_or(0) & 0x3F);
            let (scalar, width) = if byte < 0x80 {
                (u32::from(byte), 1)
            } else if byte & 0xE0 == 0xC0 && at + 1 < text.len() {
                ((u32::from(byte & 0x1F) << 6) | tail(1), 2)
            } else if byte & 0xF0 == 0xE0 && at + 2 < text.len() {
                ((u32::from(byte & 0x0F) << 12) | (tail(1) << 6) | tail(2), 3)
            } else if byte & 0xF8 == 0xF0 && at + 3 < text.len() {
                (
                    (u32::from(byte & 0x07) << 18) | (tail(1) << 12) | (tail(2) << 6) | tail(3),
                    4,
                )
            } else {
                (u32::from(byte), 1)
            };
            at += width;
            let needed = if scalar > 0xFFFF { 2 } else { 1 };
            if count + needed > units.len() {
                self.fail(node, code::TOO_MANY_CONSTANTS);
                return 0;
            }
            if scalar > 0xFFFF {
                let value = scalar - 0x1_0000;
                if let Some(slot) = units.get_mut(count) {
                    *slot = 0xD800 | u16::try_from(value >> 10).unwrap_or(0);
                }
                if let Some(slot) = units.get_mut(count + 1) {
                    *slot = 0xDC00 | u16::try_from(value & 0x3FF).unwrap_or(0);
                }
                count += 2;
            } else {
                if let Some(slot) = units.get_mut(count) {
                    *slot = u16::try_from(scalar).unwrap_or(0);
                }
                count += 1;
            }
        }
        let mut length = 2usize;
        let mut index = 0usize;
        while index < count {
            let bytes = units[index].to_le_bytes();
            match self
                .program
                .constant_data
                .get_mut(offset + length..offset + length + 2)
            {
                Some(slot) => {
                    slot[0] = bytes[0];
                    slot[1] = bytes[1];
                    length += 2;
                }
                None => {
                    self.fail(node, code::TOO_MANY_CONSTANTS);
                    return 0;
                }
            }
            index += 1;
        }
        if let Some(existing) = self.find_text(ConstantKind::RegExp, offset, length) {
            return existing;
        }
        self.program.constant_data_length += length;
        self.intern(Constant {
            kind: ConstantKind::RegExp,
            first: u32::try_from(offset).unwrap_or(0),
            second: u32::try_from(length / 2).unwrap_or(0),
        })
    }

    pub(super) fn bigint_constant(&mut self, node: &Node) -> u32 {
        // The digits are copied through a fixed buffer so the source borrow
        // ends before the constant data is written. The radix prefix is kept:
        // it is part of what the literal denotes.
        let mut digits = [0u8; 1024];
        let mut digit_count = 0usize;
        for &byte in self.span(node.first, node.second) {
            if byte == b'_' {
                continue;
            }
            match digits.get_mut(digit_count) {
                Some(slot) => {
                    *slot = byte;
                    digit_count += 1;
                }
                None => {
                    self.fail(node, code::TOO_MANY_CONSTANTS);
                    return 0;
                }
            }
        }
        // The radix goes in front of the digits: what a literal denotes is the
        // digits read in the radix it was written in, and the constant must
        // carry both.
        let offset = self.program.constant_data_length;
        match self.program.constant_data.get_mut(offset) {
            Some(slot) => *slot = u8::try_from(node.third).unwrap_or(10),
            None => {
                self.fail(node, code::TOO_MANY_CONSTANTS);
                return 0;
            }
        }
        let mut length = 1usize;
        while length <= digit_count {
            let byte = digits[length - 1];
            match self.program.constant_data.get_mut(offset + length) {
                Some(slot) => {
                    *slot = byte;
                    length += 1;
                }
                None => {
                    self.fail(node, code::TOO_MANY_CONSTANTS);
                    return 0;
                }
            }
        }
        if let Some(existing) = self.find_text(ConstantKind::BigInt, offset, length) {
            return existing;
        }
        self.program.constant_data_length += length;
        self.intern(Constant {
            kind: ConstantKind::BigInt,
            first: u32::try_from(offset).unwrap_or(0),
            second: u32::try_from(length).unwrap_or(0),
        })
    }

    pub(super) fn load_number(&mut self, value: f64) {
        // Small integers encode as an immediate; everything else is a constant.
        // The test is made on the bits, so it holds on targets with no
        // floating-point instructions, and negative zero stays a constant
        // because it is not the same value as zero.
        if value.to_bits() == 0 {
            self.emit(Opcode::LdaZero, &[]);
            return;
        }
        if let Some(integer) = crate::numeric::exact_i32(value) {
            if integer != 0 {
                self.emit(Opcode::LdaSmi, &[i64::from(integer)]);
                return;
            }
        }
        let constant = self.number_constant(value);
        self.emit(Opcode::LdaConstant, &[i64::from(constant)]);
    }

    pub(super) fn identifier_constant(&mut self, node: &Node) -> u32 {
        self.text_constant(node, ConstantKind::Key, TokenKind::Identifier)
    }
}
