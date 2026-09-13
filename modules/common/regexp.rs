//! Regular expressions: the pattern grammar, and the matcher that runs one.
//!
//! A pattern is compiled once into a small program of bytes, and the matcher
//! executes that program against UTF-16 code units. Backtracking uses an
//! explicit stack in caller-provided storage rather than the host's, so a
//! pathological pattern costs fuel and storage instead of the machine's stack.
//!
//! Semantics are over code units. The `u` and `v` flags are not admitted,
//! because their semantics are over code points and the difference is
//! observable; a pattern that asks for one is a syntax error rather than a
//! pattern that quietly means something else.

/// What a pattern may not do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatternError {
    /// The pattern ended in the middle of something.
    Unterminated,
    /// A construct this build does not admit.
    NotAdmitted,
    /// A quantifier with no target, or one whose bounds are the wrong way
    /// round.
    InvalidQuantifier,
    /// A character class that does not close, or whose range is reversed.
    InvalidClass,
    /// An escape that means nothing.
    InvalidEscape,
    /// A group name that is not an identifier, repeats another, or is
    /// referred to without existing.
    InvalidGroupName,
    /// A group that never closes, or a `)` with no group.
    UnmatchedParenthesis,
    /// The program, the group count, or the nesting is beyond what a build
    /// admits.
    TooLarge,
}

/// Flags a pattern was written with.
pub mod flag {
    pub const GLOBAL: u8 = 1 << 0;
    pub const IGNORE_CASE: u8 = 1 << 1;
    pub const MULTILINE: u8 = 1 << 2;
    pub const DOT_ALL: u8 = 1 << 3;
    pub const STICKY: u8 = 1 << 4;
    /// The pattern works in code points: a surrogate pair is one atom, and
    /// `\u{...}` escapes are admitted.
    pub const UNICODE: u8 = 1 << 5;
}

/// Instructions of the compiled program.
mod op {
    /// One code unit, matched exactly.
    pub const CHAR: u8 = 0x01;
    /// One code unit, matched with case folded.
    pub const CHAR_FOLDED: u8 = 0x02;
    /// Any code unit, or any but a line terminator when `dotAll` is off.
    pub const ANY: u8 = 0x03;
    /// A set of ranges, possibly negated.
    pub const CLASS: u8 = 0x04;
    /// Try the first branch, and the second if it fails.
    pub const SPLIT: u8 = 0x05;
    /// Continue elsewhere.
    pub const JUMP: u8 = 0x06;
    /// Record the current position in a capture slot.
    pub const SAVE: u8 = 0x07;
    /// The pattern has matched.
    pub const MATCH: u8 = 0x08;
    /// The start of the input, or of a line under `multiline`.
    pub const ASSERT_START: u8 = 0x09;
    /// The end of the input, or of a line under `multiline`.
    pub const ASSERT_END: u8 = 0x0A;
    /// A word boundary, or its negation.
    pub const WORD_BOUNDARY: u8 = 0x0B;
    /// What a group matched before, matched again.
    pub const BACKREFERENCE: u8 = 0x0C;
    /// A sub-pattern that must match, or must not, without consuming.
    pub const LOOK: u8 = 0x0D;
    /// A set of code-point ranges, possibly negated: the unicode-mode class.
    pub const CLASS32: u8 = 0x0E;
}

/// Capture slots one pattern may have: two per group, plus the whole match.
pub const MAX_SLOTS: usize = 64;
/// Named groups one pattern may have, and the units one name may run to.
pub const MAX_NAMES: usize = 16;
pub const MAX_NAME_UNITS: usize = 32;

/// A named group: which capture it is, and where its name lies in the
/// pattern.
#[derive(Clone, Copy)]
struct GroupName {
    index: u8,
    start: usize,
    end: usize,
}

const NO_NAME: GroupName = GroupName {
    index: 0,
    start: 0,
    end: 0,
};

/// The named groups a pattern declares, found before compilation so a
/// `\k<name>` written ahead of its group still resolves: each capturing
/// group counts in order, and a name may not repeat.
fn prescan_names(pattern: &[u16]) -> Result<([GroupName; MAX_NAMES], usize), PatternError> {
    let mut names = [NO_NAME; MAX_NAMES];
    let mut count = 0usize;
    let mut groups = 0usize;
    let mut position = 0usize;
    let mut in_class = false;
    while position < pattern.len() {
        let unit = pattern[position];
        if unit == 0x5C {
            position += 2;
            continue;
        }
        if in_class {
            if unit == 0x5D {
                in_class = false;
            }
            position += 1;
            continue;
        }
        if unit == 0x5B {
            in_class = true;
            position += 1;
            continue;
        }
        if unit != 0x28 {
            position += 1;
            continue;
        }
        if pattern.get(position + 1).copied() != Some(0x3F) {
            groups += 1;
            position += 1;
            continue;
        }
        if pattern.get(position + 2).copied() != Some(0x3C)
            || matches!(pattern.get(position + 3).copied(), Some(0x3D) | Some(0x21))
        {
            // `(?:`, `(?=`, `(?!`, or a lookbehind: nothing to name.
            position += 2;
            continue;
        }
        groups += 1;
        if groups > MAX_GROUPS {
            return Err(PatternError::TooLarge);
        }
        let start = position + 3;
        let end = group_name_end(pattern, start)?;
        let mut duplicate = false;
        for earlier in names.iter().take(count) {
            if pattern.get(earlier.start..earlier.end) == pattern.get(start..end) {
                duplicate = true;
            }
        }
        if duplicate || count >= MAX_NAMES {
            return Err(PatternError::InvalidGroupName);
        }
        names[count] = GroupName {
            index: u8::try_from(groups).map_err(|_| PatternError::TooLarge)?,
            start,
            end,
        };
        count += 1;
        position = end + 1;
    }
    Ok((names, count))
}

/// Where a group name starting at `start` ends — the `>` that closes it —
/// once it has been checked to be an identifier of admitted length.
fn group_name_end(pattern: &[u16], start: usize) -> Result<usize, PatternError> {
    let mut end = start;
    while let Some(&unit) = pattern.get(end) {
        if unit == 0x3E {
            break;
        }
        let code_point = u32::from(unit);
        let admitted = if end == start {
            crate::unicode_id::is_id_start(code_point) || unit == 0x24 || unit == 0x5F
        } else {
            crate::unicode_id::is_id_continue(code_point)
                || unit == 0x24
                || unit == 0x5F
                || unit == 0x200C
                || unit == 0x200D
        };
        if !admitted {
            return Err(PatternError::InvalidGroupName);
        }
        end += 1;
    }
    if end == start || end - start > MAX_NAME_UNITS || pattern.get(end).copied() != Some(0x3E) {
        return Err(PatternError::InvalidGroupName);
    }
    Ok(end)
}

/// The named groups a compiled program records, read back from its tail:
/// the count, and the entries as `(index, name units)`.
pub struct Names<'a> {
    code: &'a [u8],
    at: usize,
    remaining: usize,
}

impl<'a> Names<'a> {
    /// The name table at the end of a program's code, empty when it has none.
    pub fn of(code: &'a [u8]) -> Self {
        let length = code.len();
        if length < 3 {
            return Self {
                code,
                at: 0,
                remaining: 0,
            };
        }
        let start = usize::from(code[length - 2]) | (usize::from(code[length - 1]) << 8);
        let count = usize::from(code[length - 3]);
        if start > length - 3 {
            return Self {
                code,
                at: 0,
                remaining: 0,
            };
        }
        Self {
            code,
            at: start,
            remaining: count,
        }
    }

    /// How many names the table holds.
    pub const fn count(&self) -> usize {
        self.remaining
    }

    /// The next entry: the group's index and its name, one unit per two
    /// bytes, little-endian.
    pub fn next_entry(&mut self) -> Option<(u8, &'a [u8])> {
        if self.remaining == 0 {
            return None;
        }
        let index = *self.code.get(self.at)?;
        let units = usize::from(*self.code.get(self.at + 1)?);
        let bytes = self.code.get(self.at + 2..self.at + 2 + units * 2)?;
        self.at += 2 + units * 2;
        self.remaining -= 1;
        Some((index, bytes))
    }
}
/// Groups one pattern may have.
pub const MAX_GROUPS: usize = MAX_SLOTS / 2 - 1;

/// A compiled pattern.
pub struct Program<'a> {
    pub code: &'a [u8],
    pub groups: u8,
    pub flags: u8,
}

/// Compile `pattern` into `out`, answering the program's length and how many
/// groups it captures.
pub fn compile(pattern: &[u16], flags: u8, out: &mut [u8]) -> Result<(usize, u8), PatternError> {
    let (names, name_count) = prescan_names(pattern)?;
    let mut compiler = Compiler {
        pattern,
        position: 0,
        out,
        length: 0,
        groups: 0,
        flags,
        depth: 0,
        names,
        name_count,
    };
    compiler.alternation()?;
    if compiler.position < compiler.pattern.len() {
        // Something is left, which can only be an unmatched `)`.
        return Err(PatternError::UnmatchedParenthesis);
    }
    compiler.emit(op::MATCH)?;
    // The name table follows the code, where no instruction reaches: each
    // entry is the group's index, its length in units, and the units; then
    // the count and where the table starts, read back from the end.
    let table_start = compiler.length;
    let mut entry = 0usize;
    while entry < name_count {
        let name = compiler.names[entry];
        compiler.emit(name.index)?;
        compiler.emit(u8::try_from(name.end - name.start).map_err(|_| PatternError::TooLarge)?)?;
        let mut at = name.start;
        while at < name.end {
            compiler.emit_u16(pattern[at])?;
            at += 1;
        }
        entry += 1;
    }
    compiler.emit(u8::try_from(name_count).map_err(|_| PatternError::TooLarge)?)?;
    compiler.emit_u16(u16::try_from(table_start).map_err(|_| PatternError::TooLarge)?)?;
    Ok((compiler.length, compiler.groups))
}

/// The flags a lexer's flag bits denote, or nothing when they name one this
/// build does not admit.
pub fn flags_of_token(bits: u16) -> Result<u8, PatternError> {
    // The lexer's bits, named here rather than imported: this module knows the
    // pattern grammar and nothing about how a literal was scanned.
    const HAS_INDICES: u16 = 1 << 0;
    const GLOBAL: u16 = 1 << 1;
    const IGNORE_CASE: u16 = 1 << 2;
    const MULTILINE: u16 = 1 << 3;
    const DOT_ALL: u16 = 1 << 4;
    const UNICODE: u16 = 1 << 5;
    const UNICODE_SETS: u16 = 1 << 6;
    const STICKY: u16 = 1 << 7;
    if bits & (UNICODE_SETS | HAS_INDICES) != 0 {
        return Err(PatternError::NotAdmitted);
    }
    let mut flags = 0u8;
    for (bit, mapped) in [
        (GLOBAL, flag::GLOBAL),
        (IGNORE_CASE, flag::IGNORE_CASE),
        (MULTILINE, flag::MULTILINE),
        (DOT_ALL, flag::DOT_ALL),
        (STICKY, flag::STICKY),
        (UNICODE, flag::UNICODE),
    ] {
        if bits & bit != 0 {
            flags |= mapped;
        }
    }
    Ok(flags)
}

/// The text of a flag set, as a program sees it.
pub fn flag_text(flags: u8, out: &mut [u16]) -> usize {
    let mut written = 0usize;
    for (bit, letter) in [
        (flag::GLOBAL, b'g'),
        (flag::IGNORE_CASE, b'i'),
        (flag::MULTILINE, b'm'),
        (flag::DOT_ALL, b's'),
        (flag::UNICODE, b'u'),
        (flag::STICKY, b'y'),
    ] {
        if flags & bit != 0 {
            if let Some(slot) = out.get_mut(written) {
                *slot = u16::from(letter);
                written += 1;
            }
        }
    }
    written
}

/// The flags a flag string denotes.
pub fn flags_of(text: &[u16]) -> Result<u8, PatternError> {
    let mut flags = 0u8;
    for &unit in text {
        let bit = match unit {
            0x67 => flag::GLOBAL,
            0x69 => flag::IGNORE_CASE,
            0x6D => flag::MULTILINE,
            0x73 => flag::DOT_ALL,
            0x79 => flag::STICKY,
            // `u`, `v`, and `d` change what a pattern means or what a match
            // reports, so they are refused rather than ignored.
            _ => return Err(PatternError::NotAdmitted),
        };
        if flags & bit != 0 {
            return Err(PatternError::NotAdmitted);
        }
        flags |= bit;
    }
    Ok(flags)
}

struct Compiler<'a, 'b> {
    pattern: &'a [u16],
    position: usize,
    out: &'b mut [u8],
    length: usize,
    groups: u8,
    flags: u8,
    depth: u32,
    names: [GroupName; MAX_NAMES],
    name_count: usize,
}

impl Compiler<'_, '_> {
    fn peek(&self) -> Option<u16> {
        self.pattern.get(self.position).copied()
    }

    fn next(&mut self) -> Option<u16> {
        let unit = self.peek()?;
        self.position += 1;
        Some(unit)
    }

    fn eat(&mut self, unit: u16) -> bool {
        if self.peek() == Some(unit) {
            self.position += 1;
            return true;
        }
        false
    }

    fn emit(&mut self, byte: u8) -> Result<(), PatternError> {
        match self.out.get_mut(self.length) {
            Some(slot) => {
                *slot = byte;
                self.length += 1;
                Ok(())
            }
            None => Err(PatternError::TooLarge),
        }
    }

    fn emit_u16(&mut self, value: u16) -> Result<(), PatternError> {
        let bytes = value.to_le_bytes();
        self.emit(bytes[0])?;
        self.emit(bytes[1])
    }

    fn emit_i16(&mut self, value: i16) -> Result<(), PatternError> {
        self.emit_u16(value as u16)
    }

    fn patch_i16(&mut self, at: usize, value: i16) {
        let bytes = (value as u16).to_le_bytes();
        if let Some(slot) = self.out.get_mut(at..at + 2) {
            slot[0] = bytes[0];
            slot[1] = bytes[1];
        }
    }

    /// Copy the instructions in `[from, to)` to the end of the program, which
    /// is how a counted repetition is built.
    fn copy_range(&mut self, from: usize, to: usize) -> Result<(), PatternError> {
        let mut index = from;
        while index < to {
            let byte = self.out.get(index).copied().unwrap_or(0);
            self.emit(byte)?;
            index += 1;
        }
        Ok(())
    }

    /// `Alternative ('|' Alternative)*`
    fn alternation(&mut self) -> Result<(), PatternError> {
        self.depth += 1;
        if self.depth > 64 {
            return Err(PatternError::TooLarge);
        }
        let start = self.length;
        self.sequence()?;
        while self.peek() == Some(0x7C) {
            self.position += 1;
            // The branch just compiled needs a split in front of it and a jump
            // after it, so the instructions move up by five bytes.
            let body = self.length - start;
            let mut buffer = [0u8; MAX_PROGRAM];
            let Some(source) = self.out.get(start..self.length) else {
                return Err(PatternError::TooLarge);
            };
            if body > buffer.len() {
                return Err(PatternError::TooLarge);
            }
            buffer[..body].copy_from_slice(source);
            self.length = start;
            self.emit(op::SPLIT)?;
            let split_at = self.length;
            self.emit_i16(0)?;
            self.emit_i16(0)?;
            let first = self.length;
            self.copy_range_from(&buffer, body)?;
            self.emit(op::JUMP)?;
            let jump_at = self.length;
            self.emit_i16(0)?;
            let second = self.length;
            self.patch_i16(split_at, (first as i32 - split_at as i32) as i16);
            self.patch_i16(split_at + 2, (second as i32 - split_at as i32) as i16);
            self.sequence()?;
            let after = self.length;
            self.patch_i16(jump_at, (after as i32 - jump_at as i32) as i16);
        }
        self.depth -= 1;
        Ok(())
    }

    fn copy_range_from(&mut self, buffer: &[u8], length: usize) -> Result<(), PatternError> {
        let mut index = 0usize;
        while index < length {
            self.emit(buffer[index])?;
            index += 1;
        }
        Ok(())
    }

    /// `Term*`, up to a `|` or a `)`.
    fn sequence(&mut self) -> Result<(), PatternError> {
        loop {
            match self.peek() {
                None => return Ok(()),
                Some(0x7C) | Some(0x29) => return Ok(()),
                _ => {}
            }
            self.term()?;
        }
    }

    /// One atom with its quantifier, if it has one.
    fn term(&mut self) -> Result<(), PatternError> {
        let start = self.length;
        let quantifiable = self.atom()?;
        let Some(unit) = self.peek() else {
            return Ok(());
        };
        let (min, max) = match unit {
            0x2A => (0u32, u32::MAX),
            0x2B => (1, u32::MAX),
            0x3F => (0, 1),
            0x7B => match self.counted()? {
                Some(bounds) => bounds,
                None => return Ok(()),
            },
            _ => return Ok(()),
        };
        if matches!(unit, 0x2A | 0x2B | 0x3F) {
            self.position += 1;
        }
        if !quantifiable {
            return Err(PatternError::InvalidQuantifier);
        }
        let lazy = self.eat(0x3F);
        self.quantify(start, min, max, lazy)
    }

    /// `{n}`, `{n,}`, or `{n,m}`. A `{` that is not one of those is an ordinary
    /// character, which is what the specification's `ExtendedPatternCharacter`
    /// admits.
    fn counted(&mut self) -> Result<Option<(u32, u32)>, PatternError> {
        let mark = self.position;
        self.position += 1;
        let Some(min) = self.digits() else {
            self.position = mark;
            return Ok(None);
        };
        let max = if self.eat(0x2C) {
            self.digits().unwrap_or(u32::MAX)
        } else {
            min
        };
        if !self.eat(0x7D) {
            self.position = mark;
            return Ok(None);
        }
        if max < min {
            return Err(PatternError::InvalidQuantifier);
        }
        Ok(Some((min, max)))
    }

    fn digits(&mut self) -> Option<u32> {
        let start = self.position;
        let mut value = 0u32;
        while let Some(unit) = self.peek() {
            if !(0x30..=0x39).contains(&unit) {
                break;
            }
            value = value
                .saturating_mul(10)
                .saturating_add(u32::from(unit - 0x30));
            self.position += 1;
        }
        if self.position == start {
            return None;
        }
        Some(value)
    }

    /// Wrap the instructions from `start` in a quantifier.
    fn quantify(
        &mut self,
        start: usize,
        min: u32,
        max: u32,
        lazy: bool,
    ) -> Result<(), PatternError> {
        let body = self.length - start;
        if body == 0 {
            return Err(PatternError::InvalidQuantifier);
        }
        let mut buffer = [0u8; MAX_PROGRAM];
        if body > buffer.len() {
            return Err(PatternError::TooLarge);
        }
        if let Some(source) = self.out.get(start..self.length) {
            buffer[..body].copy_from_slice(source);
        }
        self.length = start;

        if min == 0 && max == u32::MAX {
            // `x*`: a split that either enters the body or skips it, with the
            // body jumping back to the split.
            self.emit(op::SPLIT)?;
            let split_at = self.length;
            self.emit_i16(0)?;
            self.emit_i16(0)?;
            let enter = self.length;
            self.copy_range_from(&buffer, body)?;
            self.emit(op::JUMP)?;
            let jump_at = self.length;
            self.emit_i16(0)?;
            let after = self.length;
            let (first, second) = if lazy { (after, enter) } else { (enter, after) };
            self.patch_i16(split_at, (first as i32 - split_at as i32) as i16);
            self.patch_i16(split_at + 2, (second as i32 - split_at as i32) as i16);
            // The jump goes back to the split itself, whose opcode sits one
            // byte before its operands.
            self.patch_i16(jump_at, (split_at as i32 - 1 - jump_at as i32) as i16);
            return Ok(());
        }
        if min == 1 && max == u32::MAX {
            // `x+`: the body, then a split that goes back or falls through.
            let enter = self.length;
            self.copy_range_from(&buffer, body)?;
            self.emit(op::SPLIT)?;
            let split_at = self.length;
            self.emit_i16(0)?;
            self.emit_i16(0)?;
            let after = self.length;
            let (first, second) = if lazy { (after, enter) } else { (enter, after) };
            self.patch_i16(split_at, (first as i32 - split_at as i32) as i16);
            self.patch_i16(split_at + 2, (second as i32 - split_at as i32) as i16);
            return Ok(());
        }
        if min == 0 && max == 1 {
            // `x?`
            self.emit(op::SPLIT)?;
            let split_at = self.length;
            self.emit_i16(0)?;
            self.emit_i16(0)?;
            let enter = self.length;
            self.copy_range_from(&buffer, body)?;
            let after = self.length;
            let (first, second) = if lazy { (after, enter) } else { (enter, after) };
            self.patch_i16(split_at, (first as i32 - split_at as i32) as i16);
            self.patch_i16(split_at + 2, (second as i32 - split_at as i32) as i16);
            return Ok(());
        }

        // A counted repetition is written out: the required turns in full, and
        // then either a star or one optional copy per admitted turn. Each
        // optional copy is entered by a split whose other branch is the end, so
        // failing a later copy leaves the earlier ones matched.
        let optional = if max == u32::MAX { 0 } else { max - min };
        if min.saturating_add(optional) > MAX_REPEAT_COPIES {
            return Err(PatternError::TooLarge);
        }
        let mut turn = 0u32;
        while turn < min {
            self.copy_range_from(&buffer, body)?;
            turn += 1;
        }
        if max == u32::MAX {
            // The tail is `x*`, built the same way as a star.
            self.emit(op::SPLIT)?;
            let split_at = self.length;
            self.emit_i16(0)?;
            self.emit_i16(0)?;
            let enter = self.length;
            self.copy_range_from(&buffer, body)?;
            self.emit(op::JUMP)?;
            let jump_at = self.length;
            self.emit_i16(0)?;
            let after = self.length;
            let (first, second) = if lazy { (after, enter) } else { (enter, after) };
            self.patch_i16(split_at, (first as i32 - split_at as i32) as i16);
            self.patch_i16(split_at + 2, (second as i32 - split_at as i32) as i16);
            // The jump goes back to the split itself, whose opcode sits one
            // byte before its operands.
            self.patch_i16(jump_at, (split_at as i32 - 1 - jump_at as i32) as i16);
            return Ok(());
        }
        let mut splits = [0usize; MAX_REPEAT_COPIES as usize];
        let mut split_count = 0usize;
        let mut turn = 0u32;
        while turn < optional {
            self.emit(op::SPLIT)?;
            let split_at = self.length;
            self.emit_i16(0)?;
            self.emit_i16(0)?;
            let enter = self.length;
            if let Some(slot) = splits.get_mut(split_count) {
                *slot = split_at;
                split_count += 1;
            }
            self.patch_i16(
                split_at + if lazy { 2 } else { 0 },
                (enter as i32 - split_at as i32) as i16,
            );
            self.copy_range_from(&buffer, body)?;
            turn += 1;
        }
        let after = self.length;
        let mut index = 0usize;
        while index < split_count {
            let split_at = splits[index];
            self.patch_i16(
                split_at + if lazy { 0 } else { 2 },
                (after as i32 - split_at as i32) as i16,
            );
            index += 1;
        }
        Ok(())
    }

    /// One atom. Answers whether a quantifier may follow it.
    fn atom(&mut self) -> Result<bool, PatternError> {
        let Some(unit) = self.next() else {
            return Err(PatternError::Unterminated);
        };
        match unit {
            0x5E => {
                self.emit(op::ASSERT_START)?;
                Ok(false)
            }
            0x24 => {
                self.emit(op::ASSERT_END)?;
                Ok(false)
            }
            0x2E => {
                self.emit(op::ANY)?;
                Ok(true)
            }
            0x28 => self.group(),
            0x5B => {
                self.class()?;
                Ok(true)
            }
            0x5C => self.escape(),
            0x2A | 0x2B | 0x3F => Err(PatternError::InvalidQuantifier),
            0x29 => Err(PatternError::UnmatchedParenthesis),
            _ => {
                if self.flags & flag::UNICODE != 0 {
                    self.position -= 1;
                    let Some(point) = self.next_point() else {
                        return Err(PatternError::Unterminated);
                    };
                    self.emit_point(point)?;
                } else {
                    self.emit_char(unit)?;
                }
                Ok(true)
            }
        }
    }

    /// Emit one code point as an atom: a BMP unit directly, an astral point
    /// as its surrogate pair, which a quantifier still treats as one atom.
    fn emit_point(&mut self, point: u32) -> Result<(), PatternError> {
        if point > 0x10FFFF {
            return Err(PatternError::InvalidEscape);
        }
        if point > 0xFFFF {
            let bias = point - 0x10000;
            let high = 0xD800 + (bias >> 10) as u16;
            let low = 0xDC00 + (bias & 0x3FF) as u16;
            self.emit_char(high)?;
            return self.emit_char(low);
        }
        self.emit_char(point as u16)
    }

    /// The code point at the cursor, combining a surrogate pair in unicode
    /// mode; the cursor moves past what was read.
    fn next_point(&mut self) -> Option<u32> {
        let unit = self.next()?;
        if self.flags & flag::UNICODE != 0 && (0xD800..0xDC00).contains(&unit) {
            if let Some(&low) = self.pattern.get(self.position) {
                if (0xDC00..0xE000).contains(&low) {
                    self.position += 1;
                    return Some(
                        0x10000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(low) - 0xDC00),
                    );
                }
            }
        }
        Some(u32::from(unit))
    }

    /// The code point an escape denotes in unicode mode: `\u{...}`, a
    /// combined `\uD.. \uD..` pair, or the unit escapes.
    fn escape_point(&mut self, escape: u16) -> Result<u32, PatternError> {
        if escape == 0x75 && self.peek() == Some(0x7B) {
            self.position += 1;
            let mut value = 0u32;
            let mut any = false;
            loop {
                let Some(unit) = self.peek() else {
                    return Err(PatternError::InvalidEscape);
                };
                if unit == 0x7D {
                    self.position += 1;
                    break;
                }
                let digit = self.hex_digit()?;
                value = value
                    .checked_mul(16)
                    .and_then(|scaled| scaled.checked_add(u32::from(digit)))
                    .ok_or(PatternError::InvalidEscape)?;
                if value > 0x10FFFF {
                    return Err(PatternError::InvalidEscape);
                }
                any = true;
            }
            if !any {
                return Err(PatternError::InvalidEscape);
            }
            return Ok(value);
        }
        let unit = self.escape_value(escape)?;
        if (0xD800..0xDC00).contains(&unit)
            && self.pattern.get(self.position) == Some(&0x5C)
            && self.pattern.get(self.position + 1) == Some(&0x75)
        {
            // A high escape followed by a low escape reads as one point.
            let saved = self.position;
            self.position += 2;
            if let Ok(low) = self.escape_value(0x75) {
                if (0xDC00..0xE000).contains(&low) {
                    return Ok(0x10000
                        + ((u32::from(unit) - 0xD800) << 10)
                        + (u32::from(low) - 0xDC00));
                }
            }
            self.position = saved;
        }
        Ok(u32::from(unit))
    }

    fn emit_char(&mut self, unit: u16) -> Result<(), PatternError> {
        if self.flags & flag::IGNORE_CASE != 0 {
            self.emit(op::CHAR_FOLDED)?;
            let folded = if self.flags & flag::UNICODE != 0 {
                fold_unicode(unit)
            } else {
                fold(unit)
            };
            return self.emit_u16(folded);
        }
        self.emit(op::CHAR)?;
        self.emit_u16(unit)
    }

    /// `(`, having already been consumed: a capturing group, a non-capturing
    /// one, or a lookahead.
    fn group(&mut self) -> Result<bool, PatternError> {
        if self.eat(0x3F) {
            let Some(kind) = self.next() else {
                return Err(PatternError::Unterminated);
            };
            match kind {
                // `(?:`
                0x3A => {
                    self.alternation()?;
                    if !self.eat(0x29) {
                        return Err(PatternError::UnmatchedParenthesis);
                    }
                    Ok(true)
                }
                // `(?=` and `(?!`
                0x3D | 0x21 => {
                    self.emit(op::LOOK)?;
                    self.emit(u8::from(kind == 0x21))?;
                    let length_at = self.length;
                    self.emit_u16(0)?;
                    let body = self.length;
                    self.alternation()?;
                    if !self.eat(0x29) {
                        return Err(PatternError::UnmatchedParenthesis);
                    }
                    self.emit(op::MATCH)?;
                    let length = self.length - body;
                    let bytes = u16::try_from(length)
                        .map_err(|_| PatternError::TooLarge)?
                        .to_le_bytes();
                    if let Some(slot) = self.out.get_mut(length_at..length_at + 2) {
                        slot[0] = bytes[0];
                        slot[1] = bytes[1];
                    }
                    // A lookahead consumes nothing, so a quantifier on it would
                    // either do nothing or never end.
                    Ok(false)
                }
                // `(?<name>`: a capturing group with a name the prescan
                // recorded; `(?<=` and `(?<!` are lookbehind, not admitted.
                0x3C => {
                    if matches!(self.peek(), Some(0x3D) | Some(0x21)) {
                        return Err(PatternError::NotAdmitted);
                    }
                    let end = group_name_end(self.pattern, self.position)?;
                    self.position = end + 1;
                    self.capture_group()
                }
                _ => Err(PatternError::NotAdmitted),
            }
        } else {
            self.capture_group()
        }
    }

    /// The body of a capturing group, its `(` — and any name — consumed.
    fn capture_group(&mut self) -> Result<bool, PatternError> {
        if self.groups as usize >= MAX_GROUPS {
            return Err(PatternError::TooLarge);
        }
        self.groups += 1;
        let index = self.groups;
        self.emit(op::SAVE)?;
        self.emit(index * 2)?;
        self.alternation()?;
        if !self.eat(0x29) {
            return Err(PatternError::UnmatchedParenthesis);
        }
        self.emit(op::SAVE)?;
        self.emit(index * 2 + 1)?;
        Ok(true)
    }

    /// The group a `\k<name>` refers to, by the prescan's table.
    fn named_group(&mut self) -> Result<u8, PatternError> {
        if !self.eat(0x3C) {
            return Err(PatternError::InvalidGroupName);
        }
        let start = self.position;
        let end = group_name_end(self.pattern, start)?;
        self.position = end + 1;
        for name in self.names.iter().take(self.name_count) {
            if self.pattern.get(name.start..name.end) == self.pattern.get(start..end) {
                return Ok(name.index);
            }
        }
        Err(PatternError::InvalidGroupName)
    }

    /// `[...]`, having consumed the `[`.
    fn class(&mut self) -> Result<(), PatternError> {
        if self.flags & flag::UNICODE != 0 {
            return self.class32();
        }
        let negated = self.eat(0x5E);
        self.emit(op::CLASS)?;
        self.emit(u8::from(negated))?;
        let count_at = self.length;
        self.emit(0)?;
        let mut count = 0u8;
        loop {
            let Some(unit) = self.peek() else {
                return Err(PatternError::InvalidClass);
            };
            if unit == 0x5D {
                self.position += 1;
                break;
            }
            let low = self.class_atom()?;
            match low {
                ClassAtom::Set(ranges) => {
                    for &(start, end) in ranges {
                        self.emit_u16(start)?;
                        self.emit_u16(end)?;
                        count = count.checked_add(1).ok_or(PatternError::TooLarge)?;
                    }
                    continue;
                }
                ClassAtom::Unit(low) => {
                    // A `-` between two atoms makes a range.
                    if self.peek() == Some(0x2D)
                        && self.pattern.get(self.position + 1) != Some(&0x5D)
                    {
                        self.position += 1;
                        let high = match self.class_atom()? {
                            ClassAtom::Unit(high) => high,
                            ClassAtom::Set(_) => return Err(PatternError::InvalidClass),
                        };
                        if high < low {
                            return Err(PatternError::InvalidClass);
                        }
                        self.emit_u16(low)?;
                        self.emit_u16(high)?;
                    } else {
                        self.emit_u16(low)?;
                        self.emit_u16(low)?;
                    }
                    count = count.checked_add(1).ok_or(PatternError::TooLarge)?;
                }
            }
        }
        if let Some(slot) = self.out.get_mut(count_at) {
            *slot = count;
        }
        Ok(())
    }

    fn class_atom(&mut self) -> Result<ClassAtom, PatternError> {
        let Some(unit) = self.next() else {
            return Err(PatternError::InvalidClass);
        };
        if unit != 0x5C {
            return Ok(ClassAtom::Unit(unit));
        }
        let Some(escape) = self.next() else {
            return Err(PatternError::InvalidEscape);
        };
        Ok(match escape {
            0x64 => ClassAtom::Set(DIGITS),
            0x44 => ClassAtom::Set(NOT_DIGITS),
            0x77 => ClassAtom::Set(WORD),
            0x57 => ClassAtom::Set(NOT_WORD),
            0x73 => ClassAtom::Set(SPACE),
            0x53 => ClassAtom::Set(NOT_SPACE),
            0x62 => ClassAtom::Unit(0x08),
            _ => ClassAtom::Unit(self.escape_value(escape)?),
        })
    }

    /// A character class in unicode mode: code-point ranges, a surrogate
    /// pair or `\u{...}` escape one endpoint each.
    fn class32(&mut self) -> Result<(), PatternError> {
        let negated = self.eat(0x5E);
        self.emit(op::CLASS32)?;
        self.emit(u8::from(negated))?;
        let count_at = self.length;
        self.emit(0)?;
        let mut count = 0u8;
        loop {
            let Some(unit) = self.peek() else {
                return Err(PatternError::InvalidClass);
            };
            if unit == 0x5D {
                self.position += 1;
                break;
            }
            match self.class_point()? {
                ClassPoint::Set(ranges, wide) => {
                    for &(start, end) in ranges {
                        self.emit_u32(u32::from(start))?;
                        self.emit_u32(u32::from(end))?;
                        count = count.checked_add(1).ok_or(PatternError::TooLarge)?;
                    }
                    if wide {
                        self.emit_u32(0x10000)?;
                        self.emit_u32(0x10FFFF)?;
                        count = count.checked_add(1).ok_or(PatternError::TooLarge)?;
                    }
                }
                ClassPoint::Point(low) => {
                    if self.peek() == Some(0x2D)
                        && self.pattern.get(self.position + 1) != Some(&0x5D)
                    {
                        self.position += 1;
                        let high = match self.class_point()? {
                            ClassPoint::Point(high) => high,
                            ClassPoint::Set(..) => return Err(PatternError::InvalidClass),
                        };
                        if high < low {
                            return Err(PatternError::InvalidClass);
                        }
                        self.emit_u32(low)?;
                        self.emit_u32(high)?;
                    } else {
                        self.emit_u32(low)?;
                        self.emit_u32(low)?;
                    }
                    count = count.checked_add(1).ok_or(PatternError::TooLarge)?;
                }
            }
        }
        if let Some(slot) = self.out.get_mut(count_at) {
            *slot = count;
        }
        Ok(())
    }

    fn class_point(&mut self) -> Result<ClassPoint, PatternError> {
        let Some(unit) = self.peek() else {
            return Err(PatternError::InvalidClass);
        };
        if unit != 0x5C {
            let Some(point) = self.next_point() else {
                return Err(PatternError::InvalidClass);
            };
            return Ok(ClassPoint::Point(point));
        }
        self.position += 1;
        let Some(escape) = self.next() else {
            return Err(PatternError::InvalidEscape);
        };
        Ok(match escape {
            0x64 => ClassPoint::Set(DIGITS, false),
            0x44 => ClassPoint::Set(NOT_DIGITS, true),
            0x77 => ClassPoint::Set(WORD, false),
            0x57 => ClassPoint::Set(NOT_WORD, true),
            0x73 => ClassPoint::Set(SPACE, false),
            0x53 => ClassPoint::Set(NOT_SPACE, true),
            0x62 => ClassPoint::Point(0x08),
            _ => ClassPoint::Point(self.escape_point(escape)?),
        })
    }

    fn emit_u32(&mut self, value: u32) -> Result<(), PatternError> {
        for byte in value.to_le_bytes() {
            self.emit(byte)?;
        }
        Ok(())
    }

    /// An escape outside a class. Answers whether a quantifier may follow.
    fn escape(&mut self) -> Result<bool, PatternError> {
        let Some(escape) = self.next() else {
            return Err(PatternError::InvalidEscape);
        };
        match escape {
            0x64 | 0x44 | 0x77 | 0x57 | 0x73 | 0x53 => {
                if self.flags & flag::UNICODE != 0 {
                    // The positive set, negated by flag: the complement then
                    // reaches every code point, astral ones included.
                    let (ranges, negated) = match escape {
                        0x64 => (DIGITS, false),
                        0x44 => (DIGITS, true),
                        0x77 => (WORD, false),
                        0x57 => (WORD, true),
                        0x73 => (SPACE, false),
                        _ => (SPACE, true),
                    };
                    self.emit(op::CLASS32)?;
                    self.emit(u8::from(negated))?;
                    self.emit(u8::try_from(ranges.len()).unwrap_or(0))?;
                    for &(start, end) in ranges {
                        self.emit_u32(u32::from(start))?;
                        self.emit_u32(u32::from(end))?;
                    }
                    return Ok(true);
                }
                let ranges = match escape {
                    0x64 => DIGITS,
                    0x44 => NOT_DIGITS,
                    0x77 => WORD,
                    0x57 => NOT_WORD,
                    0x73 => SPACE,
                    _ => NOT_SPACE,
                };
                self.emit(op::CLASS)?;
                self.emit(0)?;
                self.emit(u8::try_from(ranges.len()).unwrap_or(0))?;
                for &(start, end) in ranges {
                    self.emit_u16(start)?;
                    self.emit_u16(end)?;
                }
                Ok(true)
            }
            0x62 | 0x42 => {
                self.emit(op::WORD_BOUNDARY)?;
                self.emit(u8::from(escape == 0x42))?;
                Ok(false)
            }
            0x31..=0x39 => {
                self.position -= 1;
                let Some(index) = self.digits() else {
                    return Err(PatternError::InvalidEscape);
                };
                if index > u32::from(u8::MAX) {
                    return Err(PatternError::TooLarge);
                }
                self.emit(op::BACKREFERENCE)?;
                self.emit(u8::try_from(index).unwrap_or(0))?;
                Ok(true)
            }
            // `\k<name>` refers to a named group; where the pattern names
            // none, sloppy patterns read `\k` as the letter, as Annex B says.
            0x6B if self.name_count > 0 || self.flags & flag::UNICODE != 0 => {
                let index = self.named_group()?;
                self.emit(op::BACKREFERENCE)?;
                self.emit(index)?;
                Ok(true)
            }
            _ => {
                if self.flags & flag::UNICODE != 0 {
                    let point = self.escape_point(escape)?;
                    self.emit_point(point)?;
                } else {
                    let unit = self.escape_value(escape)?;
                    self.emit_char(unit)?;
                }
                Ok(true)
            }
        }
    }

    /// The code unit an escape denotes.
    fn escape_value(&mut self, escape: u16) -> Result<u16, PatternError> {
        Ok(match escape {
            0x6E => 0x0A,
            0x74 => 0x09,
            0x72 => 0x0D,
            0x66 => 0x0C,
            0x76 => 0x0B,
            0x30 => 0x00,
            0x78 => {
                let high = self.hex_digit()?;
                let low = self.hex_digit()?;
                (high << 4) | low
            }
            0x75 => {
                let mut value = 0u16;
                let mut index = 0;
                while index < 4 {
                    value = (value << 4) | self.hex_digit()?;
                    index += 1;
                }
                value
            }
            0x63 => {
                // `\cX` is the control character X modulo 32.
                let Some(letter) = self.next() else {
                    return Err(PatternError::InvalidEscape);
                };
                if !(0x41..=0x5A).contains(&letter) && !(0x61..=0x7A).contains(&letter) {
                    return Err(PatternError::InvalidEscape);
                }
                letter % 32
            }
            _ => escape,
        })
    }

    fn hex_digit(&mut self) -> Result<u16, PatternError> {
        let Some(unit) = self.next() else {
            return Err(PatternError::InvalidEscape);
        };
        match unit {
            0x30..=0x39 => Ok(unit - 0x30),
            0x41..=0x46 => Ok(unit - 0x41 + 10),
            0x61..=0x66 => Ok(unit - 0x61 + 10),
            _ => Err(PatternError::InvalidEscape),
        }
    }
}

enum ClassPoint {
    Point(u32),
    /// A named set's ranges; `true` marks a complement set, which in
    /// unicode mode also covers every astral point.
    Set(&'static [(u16, u16)], bool),
}

enum ClassAtom {
    Unit(u16),
    Set(&'static [(u16, u16)]),
}

/// The longest program a pattern may compile to.
pub const MAX_PROGRAM: usize = 2048;
/// Copies a counted repetition may be written out as. A pattern that asks for
/// more is refused rather than compiled into something enormous.
const MAX_REPEAT_COPIES: u32 = 64;

const DIGITS: &[(u16, u16)] = &[(0x30, 0x39)];
const NOT_DIGITS: &[(u16, u16)] = &[(0x00, 0x2F), (0x3A, 0xFFFF)];
const WORD: &[(u16, u16)] = &[(0x30, 0x39), (0x41, 0x5A), (0x5F, 0x5F), (0x61, 0x7A)];
const NOT_WORD: &[(u16, u16)] = &[
    (0x00, 0x2F),
    (0x3A, 0x40),
    (0x5B, 0x5E),
    (0x60, 0x60),
    (0x7B, 0xFFFF),
];
const SPACE: &[(u16, u16)] = &[
    (0x09, 0x0D),
    (0x20, 0x20),
    (0xA0, 0xA0),
    (0x1680, 0x1680),
    (0x2000, 0x200A),
    (0x2028, 0x2029),
    (0x202F, 0x202F),
    (0x205F, 0x205F),
    (0x3000, 0x3000),
    (0xFEFF, 0xFEFF),
];
const NOT_SPACE: &[(u16, u16)] = &[
    (0x00, 0x08),
    (0x0E, 0x1F),
    (0x21, 0x9F),
    (0xA1, 0x167F),
    (0x1681, 0x1FFF),
    (0x200B, 0x2027),
    (0x202A, 0x202E),
    (0x2030, 0x205E),
    (0x2060, 0x2FFF),
    (0x3001, 0xFEFE),
    (0xFF00, 0xFFFF),
];

/// The case-folded form of a code unit, for `ignoreCase`.
///
/// Only the simple one-to-one mappings are folded, which is what this build
/// admits; everything else compares as itself.
pub fn fold(unit: u16) -> u16 {
    match unit {
        0x41..=0x5A => unit + 32,
        0xC0..=0xDE if unit != 0xD7 => unit + 32,
        _ => unit,
    }
}

const fn is_word_unit(unit: u16) -> bool {
    matches!(unit, 0x30..=0x39 | 0x41..=0x5A | 0x5F | 0x61..=0x7A)
}

const fn is_line_terminator(unit: u16) -> bool {
    matches!(unit, 0x0A | 0x0D | 0x2028 | 0x2029)
}

/// One choice the matcher may go back to.
#[derive(Clone, Copy, Debug)]
pub struct Choice {
    pc: usize,
    position: usize,
    undo: usize,
}

impl Choice {
    pub const EMPTY: Self = Self {
        pc: 0,
        position: 0,
        undo: 0,
    };
}

/// Storage the matcher works in, so it allocates nothing and recurses only as
/// deep as a lookahead nests.
pub struct Matcher<'a> {
    pub choices: &'a mut [Choice],
    pub undo: &'a mut [(u8, u32)],
    /// Set when the match stopped because it ran out of room or fuel rather
    /// than because the pattern did not match.
    ///
    /// A matcher answers `None` for both, and the two are not the same
    /// answer: one says the subject does not match, the other says nobody
    /// found out. Without this the second is reported as the first, so
    /// `/^B+$/.test(s)` on a long subject is `false` -- a wrong answer, not a
    /// refused one, and nothing about it looks like a limit.
    pub halted: bool,
}

/// What a match produced: the slots, in pairs, with `u32::MAX` for a group that
/// took no part.
pub type Slots = [u32; MAX_SLOTS];

/// Run `program` against `input`, starting at `start`.
///
/// Answers the slots when it matches. Only the positions are answered: what the
/// text was is the caller's to take from its own input.
pub fn run(
    program: &Program<'_>,
    input: &[u16],
    start: usize,
    matcher: &mut Matcher<'_>,
    fuel: &mut u32,
) -> Option<Slots> {
    let mut slots: Slots = [u32::MAX; MAX_SLOTS];
    slots[0] = u32::try_from(start).unwrap_or(u32::MAX);
    let mut choices = 0usize;
    let mut undo = 0usize;
    let mut pc = 0usize;
    let mut position = start;

    loop {
        if *fuel == 0 {
            matcher.halted = true;
            return None;
        }
        *fuel -= 1;
        let &opcode = program.code.get(pc)?;
        let mut backtrack = false;
        match opcode {
            op::MATCH => {
                slots[1] = u32::try_from(position).unwrap_or(u32::MAX);
                return Some(slots);
            }
            op::CHAR | op::CHAR_FOLDED => {
                let expected = read_u16(program.code, pc + 1);
                match input.get(position).copied() {
                    Some(unit) => {
                        let unit = if opcode == op::CHAR_FOLDED {
                            if program.flags & flag::UNICODE != 0 {
                                fold_unicode(unit)
                            } else {
                                fold(unit)
                            }
                        } else {
                            unit
                        };
                        if unit == expected {
                            position += 1;
                            pc += 3;
                        } else {
                            backtrack = true;
                        }
                    }
                    None => backtrack = true,
                }
            }
            op::ANY => match input.get(position).copied() {
                Some(unit) if program.flags & flag::DOT_ALL != 0 || !is_line_terminator(unit) => {
                    // In unicode mode the dot consumes a whole pair.
                    if program.flags & flag::UNICODE != 0
                        && (0xD800..0xDC00).contains(&unit)
                        && input
                            .get(position + 1)
                            .is_some_and(|&low| (0xDC00..0xE000).contains(&low))
                    {
                        position += 2;
                    } else {
                        position += 1;
                    }
                    pc += 1;
                }
                _ => backtrack = true,
            },
            op::CLASS32 => {
                let negated = program.code.get(pc + 1).copied().unwrap_or(0) != 0;
                let count = program.code.get(pc + 2).copied().unwrap_or(0) as usize;
                match input.get(position).copied() {
                    Some(unit) => {
                        // The subject reads as one code point: a surrogate
                        // pair together, anything else alone.
                        let (point, width) = if (0xD800..0xDC00).contains(&unit)
                            && input
                                .get(position + 1)
                                .is_some_and(|&low| (0xDC00..0xE000).contains(&low))
                        {
                            let low = input.get(position + 1).copied().unwrap_or(0);
                            (
                                0x10000
                                    + ((u32::from(unit) - 0xD800) << 10)
                                    + (u32::from(low) - 0xDC00),
                                2usize,
                            )
                        } else {
                            (u32::from(unit), 1usize)
                        };
                        let folded_point =
                            if program.flags & flag::IGNORE_CASE != 0 && point <= 0xFFFF {
                                u32::from(fold(point as u16))
                            } else {
                                point
                            };
                        let mut inside = false;
                        let mut index = 0usize;
                        while index < count {
                            let at = pc + 3 + index * 8;
                            let low = read_u32(program.code, at);
                            let high = read_u32(program.code, at + 4);
                            if (point >= low && point <= high)
                                || (folded_point >= low && folded_point <= high)
                            {
                                inside = true;
                                break;
                            }
                            index += 1;
                        }
                        if inside != negated {
                            position += width;
                            pc += 3 + count * 8;
                        } else {
                            backtrack = true;
                        }
                    }
                    None => backtrack = true,
                }
            }
            op::CLASS => {
                let negated = program.code.get(pc + 1).copied().unwrap_or(0) != 0;
                let count = program.code.get(pc + 2).copied().unwrap_or(0) as usize;
                let folded = program.flags & flag::IGNORE_CASE != 0;
                match input.get(position).copied() {
                    Some(unit) => {
                        let mut inside = false;
                        let mut index = 0usize;
                        while index < count {
                            let at = pc + 3 + index * 4;
                            let low = read_u16(program.code, at);
                            let high = read_u16(program.code, at + 2);
                            if (unit >= low && unit <= high)
                                || (folded && {
                                    let unit = fold(unit);
                                    unit >= fold(low) && unit <= fold(high) && low <= high
                                })
                            {
                                inside = true;
                                break;
                            }
                            index += 1;
                        }
                        if inside != negated {
                            position += 1;
                            pc += 3 + count * 4;
                        } else {
                            backtrack = true;
                        }
                    }
                    None => backtrack = true,
                }
            }
            op::SPLIT => {
                let first = read_i16(program.code, pc + 1);
                let second = read_i16(program.code, pc + 3);
                let alternative = offset(pc + 1, second);
                if choices >= matcher.choices.len() {
                    matcher.halted = true;
                    return None;
                }
                matcher.choices[choices] = Choice {
                    pc: alternative,
                    position,
                    undo,
                };
                choices += 1;
                pc = offset(pc + 1, first);
            }
            op::JUMP => {
                let target = read_i16(program.code, pc + 1);
                pc = offset(pc + 1, target);
            }
            op::SAVE => {
                let slot = program.code.get(pc + 1).copied().unwrap_or(0) as usize;
                if slot >= MAX_SLOTS || undo >= matcher.undo.len() {
                    matcher.halted = true;
                    return None;
                }
                matcher.undo[undo] = (slot as u8, slots[slot]);
                undo += 1;
                slots[slot] = u32::try_from(position).unwrap_or(u32::MAX);
                pc += 2;
            }
            op::ASSERT_START => {
                let at_start = position == 0
                    || (program.flags & flag::MULTILINE != 0
                        && position > 0
                        && is_line_terminator(input[position - 1]));
                if at_start {
                    pc += 1;
                } else {
                    backtrack = true;
                }
            }
            op::ASSERT_END => {
                let at_end = position == input.len()
                    || (program.flags & flag::MULTILINE != 0
                        && is_line_terminator(input[position]));
                if at_end {
                    pc += 1;
                } else {
                    backtrack = true;
                }
            }
            op::WORD_BOUNDARY => {
                let negated = program.code.get(pc + 1).copied().unwrap_or(0) != 0;
                let before = position > 0 && is_word_unit(input[position - 1]);
                let after = position < input.len() && is_word_unit(input[position]);
                if (before != after) != negated {
                    pc += 2;
                } else {
                    backtrack = true;
                }
            }
            op::BACKREFERENCE => {
                let group = program.code.get(pc + 1).copied().unwrap_or(0) as usize;
                let start_slot = group * 2;
                let end_slot = start_slot + 1;
                if start_slot >= MAX_SLOTS || end_slot >= MAX_SLOTS {
                    return None;
                }
                let (from, to) = (slots[start_slot], slots[end_slot]);
                if from == u32::MAX || to == u32::MAX {
                    // A group that took no part matches the empty string.
                    pc += 2;
                } else {
                    let from = from as usize;
                    let to = to as usize;
                    let length = to.saturating_sub(from);
                    let mut index = 0usize;
                    let mut same = true;
                    while index < length {
                        let left = input.get(from + index).copied();
                        let right = input.get(position + index).copied();
                        let (left, right) = if program.flags & flag::IGNORE_CASE != 0 {
                            (left.map(fold), right.map(fold))
                        } else {
                            (left, right)
                        };
                        if left.is_none() || right.is_none() || left != right {
                            same = false;
                            break;
                        }
                        index += 1;
                    }
                    if same {
                        position += length;
                        pc += 2;
                    } else {
                        backtrack = true;
                    }
                }
            }
            op::LOOK => {
                let negated = program.code.get(pc + 1).copied().unwrap_or(0) != 0;
                let length = read_u16(program.code, pc + 2) as usize;
                let body = pc + 4;
                let code = program.code.get(body..body + length)?;
                let inner = Program {
                    code,
                    groups: program.groups,
                    flags: program.flags,
                };
                // A lookahead is a match of its own, from here, that consumes
                // nothing and keeps no captures of its own.
                let (matched, ran_out) = {
                    let mut nested = Matcher {
                        choices: matcher.choices,
                        undo: matcher.undo,
                        halted: false,
                    };
                    let matched = run(&inner, input, position, &mut nested, fuel).is_some();
                    (matched, nested.halted)
                };
                // A nested match that ran out carries that out with it: the
                // assertion around it did not fail, it was never decided.
                matcher.halted |= ran_out;
                if matched != negated {
                    pc = body + length;
                } else {
                    backtrack = true;
                }
            }
            _ => return None,
        }

        if backtrack {
            // Go back to the last choice, putting back every capture that was
            // recorded after it.
            if choices == 0 {
                return None;
            }
            choices -= 1;
            let choice = matcher.choices[choices];
            while undo > choice.undo {
                undo -= 1;
                let (slot, previous) = matcher.undo[undo];
                slots[slot as usize] = previous;
            }
            pc = choice.pc;
            position = choice.position;
        }
    }
}

/// Simple case folding for unicode mode: the ASCII fold plus the pairs the
/// specification's Canonicalize with unicode adds for common letters.
fn fold_unicode(unit: u16) -> u16 {
    match unit {
        // KELVIN SIGN and ANGSTROM SIGN fold to their lowercase letters.
        0x212A => 0x6B,
        0x212B => 0xE5,
        // LATIN SMALL LETTER LONG S folds with s.
        0x17F => 0x73,
        _ => fold(unit),
    }
}

fn read_u32(code: &[u8], at: usize) -> u32 {
    let bytes = [
        code.get(at).copied().unwrap_or(0),
        code.get(at + 1).copied().unwrap_or(0),
        code.get(at + 2).copied().unwrap_or(0),
        code.get(at + 3).copied().unwrap_or(0),
    ];
    u32::from_le_bytes(bytes)
}

fn read_u16(code: &[u8], at: usize) -> u16 {
    let low = code.get(at).copied().unwrap_or(0);
    let high = code.get(at + 1).copied().unwrap_or(0);
    u16::from_le_bytes([low, high])
}

fn read_i16(code: &[u8], at: usize) -> i16 {
    read_u16(code, at) as i16
}

fn offset(base: usize, displacement: i16) -> usize {
    let target = base as i64 + i64::from(displacement);
    if target < 0 {
        0
    } else {
        target as usize
    }
}
