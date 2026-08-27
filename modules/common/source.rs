//! Source admission, decoding, and position mapping for the Phasor front end.
//!
//! Transport is UTF-8; language semantics are UTF-16. Positions are byte
//! offsets held as 32-bit values, which the source ceiling keeps in range. Line
//! and column pairs are derived from a line table on demand rather than carried
//! on every token.

/// Hard ceilings the front end enforces. A graph may admit smaller values and
/// cannot admit larger ones.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub source_bytes: u32,
    pub code_units: u32,
    pub lines: u32,
    pub line_bytes: u32,
    pub tokens: u32,
    pub identifier_units: u32,
    pub literal_units: u32,
    pub numeric_bytes: u32,
    pub regexp_bytes: u32,
    pub template_depth: u32,
    /// Parser recursion entries, not source nesting levels: one nested
    /// parenthesis costs several. A deployment admits a depth its stack can
    /// hold, because the parser descends recursively.
    pub expression_depth: u32,
    pub syntax_nodes: u32,
}

impl Limits {
    /// The compiled-in ceiling. Every other limit set is this one clamped down.
    pub const CEILING: Self = Self {
        source_bytes: 1_048_576,
        code_units: 1_048_576,
        lines: 262_144,
        line_bytes: 65_536,
        tokens: 262_144,
        identifier_units: 1024,
        literal_units: 65_536,
        numeric_bytes: 4_096,
        regexp_bytes: 4_096,
        template_depth: 16,
        expression_depth: 512,
        syntax_nodes: 262_144,
    };

    /// Clamp every field to the ceiling, so an admitted set can never widen it.
    #[must_use]
    pub const fn clamped(self) -> Self {
        Self {
            source_bytes: min(self.source_bytes, Self::CEILING.source_bytes),
            code_units: min(self.code_units, Self::CEILING.code_units),
            lines: min(self.lines, Self::CEILING.lines),
            line_bytes: min(self.line_bytes, Self::CEILING.line_bytes),
            tokens: min(self.tokens, Self::CEILING.tokens),
            identifier_units: min(self.identifier_units, Self::CEILING.identifier_units),
            literal_units: min(self.literal_units, Self::CEILING.literal_units),
            numeric_bytes: min(self.numeric_bytes, Self::CEILING.numeric_bytes),
            regexp_bytes: min(self.regexp_bytes, Self::CEILING.regexp_bytes),
            template_depth: min(self.template_depth, Self::CEILING.template_depth),
            expression_depth: min(self.expression_depth, Self::CEILING.expression_depth),
            syntax_nodes: min(self.syntax_nodes, Self::CEILING.syntax_nodes),
        }
    }
}

const fn min(left: u32, right: u32) -> u32 {
    if left < right {
        left
    } else {
        right
    }
}

/// A decoded scalar value and the number of bytes it occupied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Decoded {
    pub code_point: u32,
    pub length: u32,
}

impl Decoded {
    /// UTF-16 code units this scalar value occupies in language semantics.
    pub const fn code_units(self) -> u32 {
        if self.code_point > 0xFFFF {
            2
        } else {
            1
        }
    }
}

/// Decode one well-formed UTF-8 sequence at `offset`.
///
/// Returns `None` for a truncated sequence, an unexpected continuation byte, an
/// overlong encoding, or a value above U+10FFFF. There is no
/// replacement-character substitution: substitution would change the program a
/// digest identifies. A three-byte sequence for a surrogate is admitted: source
/// staged from a string carries the string's lone surrogates that way, and a
/// program's source is a sequence of code units, not of scalar values.
pub fn decode(source: &[u8], offset: usize) -> Option<Decoded> {
    let first = *source.get(offset)?;
    let (length, mut code_point) = match first {
        0x00..=0x7F => {
            return Some(Decoded {
                code_point: u32::from(first),
                length: 1,
            });
        }
        0xC2..=0xDF => (2u32, u32::from(first & 0x1F)),
        0xE0..=0xEF => (3u32, u32::from(first & 0x0F)),
        0xF0..=0xF4 => (4u32, u32::from(first & 0x07)),
        _ => return None,
    };

    let mut index = 1usize;
    while index < length as usize {
        let byte = *source.get(offset + index)?;
        if !(0x80..=0xBF).contains(&byte) {
            return None;
        }
        code_point = (code_point << 6) | u32::from(byte & 0x3F);
        index += 1;
    }

    let minimum = match length {
        2 => 0x80,
        3 => 0x800,
        _ => 0x1_0000,
    };
    if code_point < minimum || code_point > 0x10_FFFF {
        return None;
    }
    Some(Decoded { code_point, length })
}

/// One recorded line start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LineStart {
    /// Byte offset of the first byte of the line.
    pub byte: u32,
    /// UTF-16 code units in the source before this line.
    pub units_before: u32,
}

/// A line and column pair, both one-based, with the column counted in UTF-16
/// code units because that is what language semantics expose.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Position {
    pub line: u32,
    pub column: u32,
}

/// Line starts recorded during scanning, in caller-provided storage.
///
/// The table counts every line even when its storage is full, so the line
/// ceiling is enforced whether or not positions can be mapped.
pub struct LineTable<'a> {
    starts: &'a mut [LineStart],
    recorded: u32,
    lines: u32,
}

impl<'a> LineTable<'a> {
    /// A table over `starts`, which may be empty when position mapping is not
    /// required. Line one always begins at offset zero.
    pub fn new(starts: &'a mut [LineStart]) -> Self {
        let mut table = Self {
            starts,
            recorded: 0,
            lines: 1,
        };
        table.record(LineStart {
            byte: 0,
            units_before: 0,
        });
        table
    }

    fn record(&mut self, start: LineStart) {
        if let Some(slot) = self.starts.get_mut(self.recorded as usize) {
            *slot = start;
            self.recorded += 1;
        }
    }

    /// Note that a new line begins at `byte`, `units_before` code units into
    /// the source. Returns the number of lines seen so far.
    pub fn begin_line(&mut self, byte: u32, units_before: u32) -> u32 {
        self.lines = self.lines.saturating_add(1);
        self.record(LineStart { byte, units_before });
        self.lines
    }

    /// Lines seen so far, whether or not they were recorded.
    pub const fn lines(&self) -> u32 {
        self.lines
    }

    /// The recorded line starts.
    pub fn starts(&self) -> &[LineStart] {
        match self.starts.get(..self.recorded as usize) {
            Some(slice) => slice,
            None => &[],
        }
    }

    /// The line containing `offset`, or `None` when the line was not recorded.
    fn line_index(&self, offset: u32) -> Option<usize> {
        let starts = self.starts();
        if starts.is_empty() || starts[0].byte > offset {
            return None;
        }
        let mut low = 0usize;
        let mut high = starts.len() - 1;
        while low < high {
            let middle = (low + high).div_ceil(2);
            if starts[middle].byte <= offset {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        Some(low)
    }

    /// Map a byte offset to a line and column, scanning the line to count
    /// UTF-16 code units. Returns `None` when the offset is past the recorded
    /// lines or does not fall on a scalar-value boundary.
    pub fn position(&self, source: &[u8], offset: u32) -> Option<Position> {
        let index = self.line_index(offset)?;
        let start = self.starts()[index];
        let mut cursor = start.byte;
        let mut column = 1u32;
        while cursor < offset {
            let decoded = decode(source, cursor as usize)?;
            column = column.saturating_add(decoded.code_units());
            cursor = cursor.saturating_add(decoded.length);
        }
        if cursor != offset {
            return None;
        }
        Some(Position {
            line: u32::try_from(index).unwrap_or(u32::MAX).saturating_add(1),
            column,
        })
    }

    /// The UTF-16 offset of `offset`, which is what language semantics count.
    pub fn code_unit_offset(&self, source: &[u8], offset: u32) -> Option<u32> {
        let index = self.line_index(offset)?;
        let start = self.starts()[index];
        let position = self.position(source, offset)?;
        Some(
            start
                .units_before
                .saturating_add(position.column.saturating_sub(1)),
        )
    }
}
