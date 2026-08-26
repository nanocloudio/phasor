//! The Phasor tokenizer.
//!
//! The lexer is driven by the parser: before every token the caller states
//! which goal symbol it expects, so `/` as division and `/` as a regular
//! expression, and `}` as a punctuator and `}` as a template continuation, are
//! never guessed. Every token records whether a line terminator preceded it,
//! which is what automatic semicolon insertion and the restricted productions
//! need; the lexer inserts nothing itself.
//!
//! Scanning is bounded and resumable. The lexer holds its cursor, spends fuel
//! for consumed bytes and produced tokens, and stops with a budget diagnostic
//! rather than running to the end of a variable-sized input.

use crate::diagnostic::{code, escape_kind, phase, Diagnostic, Severity};
use crate::numeric::{decimal_value, radix_value, DecimalLiteral};
use crate::source::{decode, Limits, LineTable};
use crate::unicode_id::{is_id_continue, is_id_start, is_space_separator};

/// Which token the parser expects next.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Goal {
    /// An expression has completed, so `/` is division.
    Div,
    /// An operand is expected, so `/` opens a regular-expression literal.
    RegExp,
    /// A template substitution has closed, so `}` continues the template.
    TemplateTail,
    /// The start of a unit, where `#!` is a comment.
    HashbangOrDiv,
}

/// Reserved words. Contextual words are ordinary identifiers to the lexer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Keyword {
    Await,
    Break,
    Case,
    Catch,
    Class,
    Const,
    Continue,
    Debugger,
    Default,
    Delete,
    Do,
    Else,
    Enum,
    Export,
    Extends,
    False,
    Finally,
    For,
    Function,
    If,
    Import,
    In,
    Instanceof,
    New,
    Null,
    Return,
    Super,
    Switch,
    This,
    Throw,
    True,
    Try,
    Typeof,
    Var,
    Void,
    While,
    With,
    Yield,
}

fn keyword_of(text: &[u8]) -> Option<Keyword> {
    let keyword = match text {
        b"await" => Keyword::Await,
        b"break" => Keyword::Break,
        b"case" => Keyword::Case,
        b"catch" => Keyword::Catch,
        b"class" => Keyword::Class,
        b"const" => Keyword::Const,
        b"continue" => Keyword::Continue,
        b"debugger" => Keyword::Debugger,
        b"default" => Keyword::Default,
        b"delete" => Keyword::Delete,
        b"do" => Keyword::Do,
        b"else" => Keyword::Else,
        b"enum" => Keyword::Enum,
        b"export" => Keyword::Export,
        b"extends" => Keyword::Extends,
        b"false" => Keyword::False,
        b"finally" => Keyword::Finally,
        b"for" => Keyword::For,
        b"function" => Keyword::Function,
        b"if" => Keyword::If,
        b"import" => Keyword::Import,
        b"in" => Keyword::In,
        b"instanceof" => Keyword::Instanceof,
        b"new" => Keyword::New,
        b"null" => Keyword::Null,
        b"return" => Keyword::Return,
        b"super" => Keyword::Super,
        b"switch" => Keyword::Switch,
        b"this" => Keyword::This,
        b"throw" => Keyword::Throw,
        b"true" => Keyword::True,
        b"try" => Keyword::Try,
        b"typeof" => Keyword::Typeof,
        b"var" => Keyword::Var,
        b"void" => Keyword::Void,
        b"while" => Keyword::While,
        b"with" => Keyword::With,
        b"yield" => Keyword::Yield,
        _ => return None,
    };
    Some(keyword)
}

/// Punctuators, longest match first.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Punctuator {
    OpenBrace,
    CloseBrace,
    OpenParen,
    CloseParen,
    OpenBracket,
    CloseBracket,
    Semicolon,
    Comma,
    Colon,
    Tilde,
    Question,
    Dot,
    Ellipsis,
    OptionalChain,
    Arrow,
    Less,
    Greater,
    LessEqual,
    GreaterEqual,
    Equal,
    NotEqual,
    StrictEqual,
    StrictNotEqual,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    StarStar,
    PlusPlus,
    MinusMinus,
    ShiftLeft,
    ShiftRight,
    UnsignedShiftRight,
    Ampersand,
    Pipe,
    Caret,
    Bang,
    AmpersandAmpersand,
    PipePipe,
    QuestionQuestion,
    Assign,
    PlusAssign,
    MinusAssign,
    StarAssign,
    SlashAssign,
    PercentAssign,
    StarStarAssign,
    ShiftLeftAssign,
    ShiftRightAssign,
    UnsignedShiftRightAssign,
    AmpersandAssign,
    PipeAssign,
    CaretAssign,
    AmpersandAmpersandAssign,
    PipePipeAssign,
    QuestionQuestionAssign,
}

/// What the token is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenKind {
    EndOfSource,
    Identifier,
    PrivateName,
    Keyword(Keyword),
    Punctuator(Punctuator),
    Number,
    BigInt,
    String,
    NoSubstitutionTemplate,
    TemplateHead,
    TemplateMiddle,
    TemplateTail,
    RegExp,
}

/// Regular-expression flag bits, in the order the flags are declared.
pub mod regexp_flag {
    pub const HAS_INDICES: u16 = 1 << 0;
    pub const GLOBAL: u16 = 1 << 1;
    pub const IGNORE_CASE: u16 = 1 << 2;
    pub const MULTILINE: u16 = 1 << 3;
    pub const DOT_ALL: u16 = 1 << 4;
    pub const UNICODE: u16 = 1 << 5;
    pub const UNICODE_SETS: u16 = 1 << 6;
    pub const STICKY: u16 = 1 << 7;
}

/// One token, described entirely by spans and small scalars.
#[derive(Clone, Copy, Debug)]
pub struct Token {
    pub kind: TokenKind,
    /// Byte offset of the first byte of the token.
    pub start: u32,
    /// Byte offset one past the last byte of the token.
    pub end: u32,
    /// Byte offset of the payload: literal contents without delimiters.
    pub inner_start: u32,
    /// Byte offset one past the payload.
    pub inner_end: u32,
    /// Whether a line terminator appeared in the trivia before this token.
    pub line_break_before: bool,
    /// Whether an identifier or keyword contained a Unicode escape.
    pub escaped: bool,
    /// Whether an escaped identifier spells a reserved word. Such a name is a
    /// legitimate property name and an illegal identifier reference, which is a
    /// distinction only the parser can make.
    pub spells_reserved: bool,
    /// Whether a literal has a cooked value. Only a tagged template part may
    /// legitimately lack one.
    pub cooked_valid: bool,
    /// The Number value of a numeric token.
    pub number: f64,
    /// Cooked length in UTF-16 code units for literals and identifiers.
    pub code_units: u32,
    /// Radix of a numeric or BigInt token.
    pub radix: u8,
    /// Flag bits of a regular-expression token.
    pub flags: u16,
}

impl Token {
    const fn empty(kind: TokenKind, start: u32, end: u32, line_break_before: bool) -> Self {
        Self {
            kind,
            start,
            end,
            inner_start: start,
            inner_end: end,
            line_break_before,
            escaped: false,
            spells_reserved: false,
            cooked_valid: true,
            number: 0.0,
            code_units: 0,
            radix: 10,
            flags: 0,
        }
    }
}

/// The tokenizer over one committed source transfer.
pub struct Lexer<'s, 't> {
    source: &'s [u8],
    lines: LineTable<'t>,
    limits: Limits,
    cursor: u32,
    units: u32,
    fuel: u32,
    tokens: u32,
    template_depth: u32,
    /// Byte offset of the current line's first byte. It is tracked here rather
    /// than read from the line table, so a table that has run out of storage
    /// cannot change how the line-length limit is measured.
    line_start: u32,
    line_break: bool,
}

impl<'s, 't> Lexer<'s, 't> {
    /// A lexer over `source`, admitted under `limits`.
    ///
    /// The source is rejected here rather than at the first token, so an
    /// oversized transfer never starts scanning.
    pub fn new(
        source: &'s [u8],
        limits: Limits,
        lines: LineTable<'t>,
        fuel: u32,
    ) -> Result<Self, Diagnostic> {
        let limits = limits.clamped();
        let length = u32::try_from(source.len()).unwrap_or(u32::MAX);
        if length > limits.source_bytes {
            return Err(Diagnostic::at(code::SOURCE_TOO_LARGE, Severity::Error, 0)
                .with(length)
                .with(limits.source_bytes));
        }
        Ok(Self {
            source,
            lines,
            limits,
            cursor: 0,
            units: 0,
            fuel,
            tokens: 0,
            template_depth: 0,
            line_start: 0,
            line_break: false,
        })
    }

    /// The source being scanned, for a caller that must compare a token's text
    /// against a contextual keyword.
    pub const fn source(&self) -> &'s [u8] {
        self.source
    }

    /// Byte offset of the next unscanned byte.
    pub const fn cursor(&self) -> u32 {
        self.cursor
    }

    /// Remaining fuel for this step.
    pub const fn fuel(&self) -> u32 {
        self.fuel
    }

    /// Re-scan the token that begins at `offset` under a different goal.
    ///
    /// The parser needs this where one token has two readings, such as a `}`
    /// that closes a template substitution rather than a block. Only a token
    /// start is a legal target: the trivia before it has already been consumed,
    /// so no line terminator is crossed twice and the line table stays exact.
    pub fn seek(&mut self, offset: u32) {
        self.cursor = offset;
    }

    /// Grant the next step its fuel allowance.
    pub fn refuel(&mut self, fuel: u32) {
        self.fuel = fuel;
    }

    /// The line table, for mapping a span to a line and column.
    pub const fn lines(&self) -> &LineTable<'t> {
        &self.lines
    }

    fn spend(&mut self, amount: u32) -> Result<(), Diagnostic> {
        match self.fuel.checked_sub(amount) {
            Some(remaining) => {
                self.fuel = remaining;
                Ok(())
            }
            None => {
                self.fuel = 0;
                Err(
                    Diagnostic::at(code::COMPILE_BUDGET_EXHAUSTED, Severity::Fatal, self.cursor)
                        .with(phase::LEX)
                        .with(self.cursor),
                )
            }
        }
    }

    fn byte(&self, offset: u32) -> Option<u8> {
        self.source.get(offset as usize).copied()
    }

    fn peek(&self) -> Option<u8> {
        self.byte(self.cursor)
    }

    fn peek_at(&self, ahead: u32) -> Option<u8> {
        self.byte(self.cursor.saturating_add(ahead))
    }

    fn fatal(&self, code: u16) -> Diagnostic {
        Diagnostic::at(code, Severity::Fatal, self.cursor)
    }

    fn error(&self, code: u16, start: u32) -> Diagnostic {
        Diagnostic::new(
            code,
            Severity::Error,
            start,
            self.cursor.saturating_sub(start),
        )
    }

    /// Advance over one ASCII byte that has already been inspected.
    fn bump(&mut self) {
        self.cursor = self.cursor.saturating_add(1);
        self.units = self.units.saturating_add(1);
    }

    /// Decode and advance over one scalar value, rejecting ill-formed bytes.
    fn advance_scalar(&mut self) -> Result<u32, Diagnostic> {
        let Some(decoded) = decode(self.source, self.cursor as usize) else {
            return Err(
                Diagnostic::at(code::INVALID_UTF8, Severity::Fatal, self.cursor)
                    .with(self.cursor)
                    .with(u32::from(self.peek().unwrap_or(0))),
            );
        };
        self.cursor = self.cursor.saturating_add(decoded.length);
        self.units = self.units.saturating_add(decoded.code_units());
        Ok(decoded.code_point)
    }

    fn peek_scalar(&self) -> Result<Option<u32>, Diagnostic> {
        if self.cursor as usize >= self.source.len() {
            return Ok(None);
        }
        match decode(self.source, self.cursor as usize) {
            Some(decoded) => Ok(Some(decoded.code_point)),
            None => Err(
                Diagnostic::at(code::INVALID_UTF8, Severity::Fatal, self.cursor)
                    .with(self.cursor)
                    .with(u32::from(self.peek().unwrap_or(0))),
            ),
        }
    }

    fn begin_line(&mut self) -> Result<(), Diagnostic> {
        let lines = self.lines.begin_line(self.cursor, self.units);
        self.line_start = self.cursor;
        if lines > self.limits.lines {
            return Err(
                Diagnostic::at(code::TOO_MANY_LINES, Severity::Error, self.cursor)
                    .with(lines)
                    .with(self.limits.lines),
            );
        }
        self.line_break = true;
        Ok(())
    }

    fn check_line_length(&self) -> Result<(), Diagnostic> {
        let length = self.cursor.saturating_sub(self.line_start);
        if length > self.limits.line_bytes {
            return Err(
                Diagnostic::at(code::LINE_TOO_LONG, Severity::Error, self.line_start)
                    .with(length)
                    .with(self.limits.line_bytes),
            );
        }
        Ok(())
    }

    /// Skip white space, line terminators, and comments before a token.
    fn skip_trivia(&mut self, goal: Goal) -> Result<(), Diagnostic> {
        loop {
            let Some(byte) = self.peek() else {
                return Ok(());
            };
            self.spend(1)?;
            match byte {
                b'\t' | 0x0B | 0x0C | b' ' => self.bump(),
                b'\n' => {
                    self.bump();
                    self.begin_line()?;
                }
                b'\r' => {
                    self.bump();
                    if self.peek() == Some(b'\n') {
                        self.bump();
                    }
                    self.begin_line()?;
                }
                b'/' => match self.peek_at(1) {
                    Some(b'/') => self.skip_line_comment()?,
                    Some(b'*') => self.skip_block_comment()?,
                    _ => return Ok(()),
                },
                b'#' => {
                    if goal == Goal::HashbangOrDiv
                        && self.cursor == 0
                        && self.peek_at(1) == Some(b'!')
                    {
                        self.skip_line_comment()?;
                    } else {
                        return Ok(());
                    }
                }
                0x00..=0x7F => return Ok(()),
                _ => {
                    let code_point = self.peek_scalar()?.unwrap_or(0);
                    match code_point {
                        0x00A0 | 0xFEFF => {
                            let _ = self.advance_scalar()?;
                        }
                        0x2028 | 0x2029 => {
                            let _ = self.advance_scalar()?;
                            self.begin_line()?;
                        }
                        _ if is_space_separator(code_point) => {
                            let _ = self.advance_scalar()?;
                        }
                        _ => return Ok(()),
                    }
                }
            }
            self.check_line_length()?;
        }
    }

    fn skip_line_comment(&mut self) -> Result<(), Diagnostic> {
        self.bump();
        self.bump();
        loop {
            let Some(byte) = self.peek() else {
                return Ok(());
            };
            self.spend(1)?;
            match byte {
                b'\n' | b'\r' => return Ok(()),
                0x00..=0x7F => self.bump(),
                _ => {
                    let code_point = self.peek_scalar()?.unwrap_or(0);
                    if code_point == 0x2028 || code_point == 0x2029 {
                        return Ok(());
                    }
                    let _ = self.advance_scalar()?;
                }
            }
        }
    }

    fn skip_block_comment(&mut self) -> Result<(), Diagnostic> {
        let start = self.cursor;
        self.bump();
        self.bump();
        loop {
            let Some(byte) = self.peek() else {
                return Err(self.error(code::UNTERMINATED_COMMENT, start));
            };
            self.spend(1)?;
            match byte {
                b'*' => {
                    self.bump();
                    if self.peek() == Some(b'/') {
                        self.bump();
                        return Ok(());
                    }
                }
                b'\n' => {
                    self.bump();
                    self.begin_line()?;
                }
                b'\r' => {
                    self.bump();
                    if self.peek() == Some(b'\n') {
                        self.bump();
                    }
                    self.begin_line()?;
                }
                0x00..=0x7F => self.bump(),
                _ => {
                    let code_point = self.peek_scalar()?.unwrap_or(0);
                    let _ = self.advance_scalar()?;
                    if code_point == 0x2028 || code_point == 0x2029 {
                        self.begin_line()?;
                    }
                }
            }
        }
    }

    /// Scan the next token under `goal`.
    pub fn next(&mut self, goal: Goal) -> Result<Token, Diagnostic> {
        self.line_break = false;
        self.skip_trivia(goal)?;
        let line_break = self.line_break;
        let start = self.cursor;

        let Some(byte) = self.peek() else {
            return Ok(Token::empty(
                TokenKind::EndOfSource,
                start,
                start,
                line_break,
            ));
        };

        self.tokens = self.tokens.saturating_add(1);
        if self.tokens > self.limits.tokens {
            return Err(
                Diagnostic::at(code::TOO_MANY_TOKENS, Severity::Error, start)
                    .with(self.tokens)
                    .with(self.limits.tokens),
            );
        }
        self.spend(1)?;

        match byte {
            b'"' | b'\'' => self.scan_string(byte, line_break),
            b'`' => {
                self.bump();
                self.scan_template(TokenKind::TemplateHead, start, line_break)
            }
            b'0'..=b'9' => self.scan_number(line_break),
            b'.' => {
                if matches!(self.peek_at(1), Some(b'0'..=b'9')) {
                    self.scan_number(line_break)
                } else {
                    self.scan_punctuator(line_break)
                }
            }
            b'#' => {
                if self.peek_at(1) == Some(b'!') {
                    self.bump();
                    self.bump();
                    return Err(self.error(code::HASHBANG_NOT_AT_START, start));
                }
                self.bump();
                let mut token = self.scan_identifier_body(start, line_break, true)?;
                token.kind = TokenKind::PrivateName;
                Ok(token)
            }
            b'/' => match goal {
                Goal::RegExp => self.scan_regexp(line_break),
                _ => self.scan_punctuator(line_break),
            },
            b'}' => match goal {
                Goal::TemplateTail => {
                    self.bump();
                    self.scan_template(TokenKind::TemplateMiddle, start, line_break)
                }
                _ => self.scan_punctuator(line_break),
            },
            b'$' | b'_' | b'\\' | b'a'..=b'z' | b'A'..=b'Z' => {
                self.scan_identifier_body(start, line_break, false)
            }
            0x00..=0x7F => self.scan_punctuator(line_break),
            _ => {
                let code_point = self.peek_scalar()?.unwrap_or(0);
                if is_id_start(code_point) {
                    self.scan_identifier_body(start, line_break, false)
                } else {
                    let _ = self.advance_scalar()?;
                    Err(self.error(code::INVALID_CHARACTER, start).with(code_point))
                }
            }
        }
    }

    /// Scan an identifier, private name, or reserved word.
    ///
    /// `private` means the leading `#` has already been consumed, so the name
    /// must start with an identifier-start character.
    fn scan_identifier_body(
        &mut self,
        start: u32,
        line_break: bool,
        private: bool,
    ) -> Result<Token, Diagnostic> {
        let name_start = self.cursor;
        let mut units = 0u32;
        let mut escaped = false;
        let mut cooked = [0u8; 12];
        let mut cooked_length = 0usize;
        let mut cooked_complete = true;
        let mut first = true;

        loop {
            let Some(byte) = self.peek() else {
                break;
            };
            self.spend(1)?;
            let code_point = if byte == b'\\' {
                let escape_start = self.cursor;
                self.bump();
                let value = self.scan_unicode_escape(escape_start)?;
                escaped = true;
                let admitted = if first {
                    is_identifier_start(value)
                } else {
                    is_identifier_part(value)
                };
                if !admitted {
                    return Err(self
                        .error(code::INVALID_IDENTIFIER_ESCAPE, escape_start)
                        .with(value));
                }
                value
            } else if byte < 0x80 {
                let value = u32::from(byte);
                let admitted = if first {
                    is_identifier_start(value)
                } else {
                    is_identifier_part(value)
                };
                if !admitted {
                    break;
                }
                self.bump();
                value
            } else {
                let value = self.peek_scalar()?.unwrap_or(0);
                let admitted = if first {
                    is_identifier_start(value)
                } else {
                    is_identifier_part(value)
                };
                if !admitted {
                    break;
                }
                let _ = self.advance_scalar()?;
                value
            };

            units = units.saturating_add(if code_point > 0xFFFF { 2 } else { 1 });
            if units > self.limits.identifier_units {
                return Err(self
                    .error(code::IDENTIFIER_TOO_LONG, start)
                    .with(units)
                    .with(self.limits.identifier_units));
            }
            cooked_complete &= append_utf8(&mut cooked, &mut cooked_length, code_point);
            first = false;
        }

        if units == 0 {
            let code_point = self.peek_scalar()?.unwrap_or(0);
            let failure = if private {
                code::INVALID_CHARACTER
            } else {
                code::INVALID_IDENTIFIER_ESCAPE
            };
            return Err(self.error(failure, start).with(code_point));
        }

        let keyword = if cooked_complete {
            cooked.get(..cooked_length).and_then(keyword_of)
        } else {
            None
        };

        // An escaped name is never a keyword token: `\u0069f` is an
        // identifier name, legal as a property name and illegal as an
        // identifier reference. The parser decides which position it is in.
        if let Some(keyword) = keyword {
            if !escaped && !private {
                let mut token =
                    Token::empty(TokenKind::Keyword(keyword), start, self.cursor, line_break);
                token.inner_start = name_start;
                token.inner_end = self.cursor;
                token.code_units = units;
                return Ok(token);
            }
        }

        let mut token = Token::empty(TokenKind::Identifier, start, self.cursor, line_break);
        token.inner_start = name_start;
        token.inner_end = self.cursor;
        token.code_units = units;
        token.escaped = escaped;
        token.spells_reserved = keyword.is_some();
        Ok(token)
    }

    /// Scan `uHHHH` or `u{...}` after a backslash and return its scalar value.
    fn scan_unicode_escape(&mut self, escape_start: u32) -> Result<u32, Diagnostic> {
        if self.peek() != Some(b'u') {
            return Err(self
                .error(code::INVALID_ESCAPE, escape_start)
                .with(escape_kind::UNRECOGNISED));
        }
        self.bump();
        if self.peek() == Some(b'{') {
            self.bump();
            let mut value = 0u32;
            let mut digits = 0u32;
            while let Some(byte) = self.peek() {
                self.spend(1)?;
                if byte == b'}' {
                    self.bump();
                    if digits == 0 {
                        return Err(self
                            .error(code::INVALID_ESCAPE, escape_start)
                            .with(escape_kind::CODE_POINT));
                    }
                    if value > 0x10_FFFF {
                        return Err(self
                            .error(code::INVALID_CODE_POINT, escape_start)
                            .with(value));
                    }
                    return Ok(value);
                }
                let Some(digit) = hex_digit(byte) else {
                    return Err(self
                        .error(code::INVALID_ESCAPE, escape_start)
                        .with(escape_kind::CODE_POINT));
                };
                self.bump();
                value = value.saturating_mul(16).saturating_add(digit);
                if value > 0x11_0000 {
                    value = 0x11_0000;
                }
                digits = digits.saturating_add(1);
            }
            return Err(self
                .error(code::INVALID_ESCAPE, escape_start)
                .with(escape_kind::CODE_POINT));
        }

        let mut value = 0u32;
        let mut digits = 0u32;
        while digits < 4 {
            let Some(byte) = self.peek() else {
                break;
            };
            let Some(digit) = hex_digit(byte) else {
                break;
            };
            self.bump();
            value = value * 16 + digit;
            digits += 1;
        }
        if digits != 4 {
            return Err(self
                .error(code::INVALID_ESCAPE, escape_start)
                .with(escape_kind::UNICODE));
        }
        Ok(value)
    }

    /// Scan a string literal, validating its escapes and counting its cooked
    /// length in UTF-16 code units.
    fn scan_string(&mut self, quote: u8, line_break: bool) -> Result<Token, Diagnostic> {
        let start = self.cursor;
        self.bump();
        let inner_start = self.cursor;
        let mut units = 0u32;

        loop {
            let Some(byte) = self.peek() else {
                return Err(self.error(code::UNTERMINATED_STRING, start));
            };
            self.spend(1)?;
            match byte {
                _ if byte == quote => {
                    let inner_end = self.cursor;
                    self.bump();
                    let mut token = Token::empty(TokenKind::String, start, self.cursor, line_break);
                    token.inner_start = inner_start;
                    token.inner_end = inner_end;
                    token.code_units = units;
                    return Ok(token);
                }
                b'\n' | b'\r' => return Err(self.error(code::UNTERMINATED_STRING, start)),
                b'\\' => {
                    let escape_start = self.cursor;
                    self.bump();
                    match self.scan_string_escape(escape_start)? {
                        Escape::Empty => {}
                        Escape::Units(count) => units = units.saturating_add(count),
                        Escape::LineTerminator => self.begin_line()?,
                    }
                }
                0x00..=0x7F => {
                    self.bump();
                    units = units.saturating_add(1);
                }
                _ => {
                    // U+2028 and U+2029 are ordinary string characters: a
                    // string literal is a superset of a JSON string.
                    let code_point = self.peek_scalar()?.unwrap_or(0);
                    let _ = self.advance_scalar()?;
                    units = units.saturating_add(if code_point > 0xFFFF { 2 } else { 1 });
                    if code_point == 0x2028 || code_point == 0x2029 {
                        self.begin_line()?;
                    }
                }
            }
            if units > self.limits.literal_units {
                return Err(self
                    .error(code::LITERAL_TOO_LONG, start)
                    .with(units)
                    .with(self.limits.literal_units));
            }
        }
    }

    /// Scan one escape sequence after its backslash.
    fn scan_string_escape(&mut self, escape_start: u32) -> Result<Escape, Diagnostic> {
        let Some(byte) = self.peek() else {
            return Err(self
                .error(code::INVALID_ESCAPE, escape_start)
                .with(escape_kind::UNRECOGNISED));
        };
        match byte {
            b'\n' => {
                self.bump();
                Ok(Escape::LineTerminator)
            }
            b'\r' => {
                self.bump();
                if self.peek() == Some(b'\n') {
                    self.bump();
                }
                Ok(Escape::LineTerminator)
            }
            b'0'..=b'7' => {
                if byte == b'0' && !matches!(self.peek_at(1), Some(b'0'..=b'9')) {
                    self.bump();
                    Ok(Escape::Units(1))
                } else {
                    self.bump();
                    Err(self.error(code::LEGACY_OCTAL_ESCAPE, escape_start))
                }
            }
            b'8' | b'9' => {
                self.bump();
                Err(self.error(code::LEGACY_OCTAL_ESCAPE, escape_start))
            }
            b'x' => {
                self.bump();
                let mut digits = 0u32;
                while digits < 2 {
                    let Some(byte) = self.peek() else {
                        break;
                    };
                    if hex_digit(byte).is_none() {
                        break;
                    }
                    self.bump();
                    digits += 1;
                }
                if digits != 2 {
                    return Err(self
                        .error(code::INVALID_ESCAPE, escape_start)
                        .with(escape_kind::HEXADECIMAL));
                }
                Ok(Escape::Units(1))
            }
            b'u' => {
                let value = self.scan_unicode_escape(escape_start)?;
                Ok(Escape::Units(if value > 0xFFFF { 2 } else { 1 }))
            }
            0x00..=0x7F => {
                self.bump();
                Ok(Escape::Units(1))
            }
            _ => {
                let code_point = self.peek_scalar()?.unwrap_or(0);
                if code_point == 0x2028 || code_point == 0x2029 {
                    let _ = self.advance_scalar()?;
                    return Ok(Escape::LineTerminator);
                }
                let _ = self.advance_scalar()?;
                Ok(Escape::Units(if code_point > 0xFFFF { 2 } else { 1 }))
            }
        }
    }

    /// Scan a template part after its opening backtick or closing brace.
    ///
    /// An invalid escape is not reported here: a tagged template admits one and
    /// keeps its raw text, so the token records that it has no cooked value and
    /// the parser decides whether that is an error.
    fn scan_template(
        &mut self,
        opening: TokenKind,
        start: u32,
        line_break: bool,
    ) -> Result<Token, Diagnostic> {
        let inner_start = self.cursor;
        let mut units = 0u32;
        let mut cooked_valid = true;

        loop {
            let Some(byte) = self.peek() else {
                return Err(self.error(code::UNTERMINATED_TEMPLATE, start));
            };
            self.spend(1)?;
            match byte {
                b'`' => {
                    let inner_end = self.cursor;
                    self.bump();
                    let kind = if matches!(opening, TokenKind::TemplateHead) {
                        TokenKind::NoSubstitutionTemplate
                    } else {
                        self.template_depth = self.template_depth.saturating_sub(1);
                        TokenKind::TemplateTail
                    };
                    return Ok(self.template_token(
                        kind,
                        start,
                        inner_start,
                        inner_end,
                        units,
                        cooked_valid,
                        line_break,
                    ));
                }
                b'$' if self.peek_at(1) == Some(b'{') => {
                    let inner_end = self.cursor;
                    self.bump();
                    self.bump();
                    if matches!(opening, TokenKind::TemplateHead) {
                        self.template_depth = self.template_depth.saturating_add(1);
                        if self.template_depth > self.limits.template_depth {
                            return Err(self
                                .error(code::TEMPLATE_NESTING_TOO_DEEP, start)
                                .with(self.template_depth)
                                .with(self.limits.template_depth));
                        }
                    }
                    return Ok(self.template_token(
                        opening,
                        start,
                        inner_start,
                        inner_end,
                        units,
                        cooked_valid,
                        line_break,
                    ));
                }
                b'\\' => {
                    let escape_start = self.cursor;
                    self.bump();
                    match self.scan_template_escape(escape_start) {
                        Ok(Escape::Empty) => {}
                        Ok(Escape::Units(count)) => units = units.saturating_add(count),
                        Ok(Escape::LineTerminator) => self.begin_line()?,
                        Err(diagnostic) if diagnostic.is_fatal() => return Err(diagnostic),
                        Err(_) => cooked_valid = false,
                    }
                }
                b'\r' => {
                    self.bump();
                    if self.peek() == Some(b'\n') {
                        self.bump();
                    }
                    self.begin_line()?;
                    units = units.saturating_add(1);
                }
                b'\n' => {
                    self.bump();
                    self.begin_line()?;
                    units = units.saturating_add(1);
                }
                0x00..=0x7F => {
                    self.bump();
                    units = units.saturating_add(1);
                }
                _ => {
                    let code_point = self.peek_scalar()?.unwrap_or(0);
                    let _ = self.advance_scalar()?;
                    units = units.saturating_add(if code_point > 0xFFFF { 2 } else { 1 });
                    if code_point == 0x2028 || code_point == 0x2029 {
                        self.begin_line()?;
                    }
                }
            }
            if units > self.limits.literal_units {
                return Err(self
                    .error(code::LITERAL_TOO_LONG, start)
                    .with(units)
                    .with(self.limits.literal_units));
            }
        }
    }

    /// Skip one template escape, recovering past a malformed one so that a
    /// tagged template can still produce its raw text.
    fn scan_template_escape(&mut self, escape_start: u32) -> Result<Escape, Diagnostic> {
        let outcome = self.scan_string_escape(escape_start);
        if outcome.is_err() {
            // Step past the escaped character so scanning continues at the
            // next template character rather than re-reading the backslash.
            if self.cursor == escape_start.saturating_add(1) && self.peek().is_some() {
                let _ = self.advance_scalar()?;
            }
        }
        outcome
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the token fields are flat scalars and packing them into a temporary struct would not make the call clearer"
    )]
    fn template_token(
        &self,
        kind: TokenKind,
        start: u32,
        inner_start: u32,
        inner_end: u32,
        units: u32,
        cooked_valid: bool,
        line_break: bool,
    ) -> Token {
        let mut token = Token::empty(kind, start, self.cursor, line_break);
        token.inner_start = inner_start;
        token.inner_end = inner_end;
        token.code_units = units;
        token.cooked_valid = cooked_valid;
        token
    }

    /// Scan a numeric or BigInt literal.
    fn scan_number(&mut self, line_break: bool) -> Result<Token, Diagnostic> {
        let start = self.cursor;
        let mut radix = 10u8;
        let mut digits_start = start;
        let integer_end;
        let mut fraction_start = start;
        let mut fraction_end = start;
        let mut exponent = 0i32;
        let mut is_bigint = false;
        let mut fractional = false;

        if self.peek() == Some(b'0') {
            match self.peek_at(1) {
                Some(b'x' | b'X') => radix = 16,
                Some(b'o' | b'O') => radix = 8,
                Some(b'b' | b'B') => radix = 2,
                Some(b'0'..=b'9') => {
                    self.bump();
                    self.bump();
                    while matches!(self.peek(), Some(b'0'..=b'9')) {
                        self.bump();
                    }
                    return Err(self.error(code::LEGACY_OCTAL_LITERAL, start));
                }
                _ => {}
            }
        }

        if radix == 10 {
            self.scan_digits(10, start)?;
            integer_end = self.cursor;
            if self.peek() == Some(b'.') {
                fractional = true;
                self.bump();
                fraction_start = self.cursor;
                self.scan_digits(10, start)?;
                fraction_end = self.cursor;
            }
            if matches!(self.peek(), Some(b'e' | b'E')) {
                fractional = true;
                self.bump();
                if matches!(self.peek(), Some(b'+' | b'-')) {
                    self.bump();
                }
                let exponent_start = self.cursor;
                let digits = self.scan_digits(10, start)?;
                if digits == 0 {
                    return Err(self.error(code::INVALID_NUMERIC_TERMINATOR, start).with(0));
                }
                exponent = self.exponent_value(exponent_start);
            }
        } else {
            self.bump();
            self.bump();
            digits_start = self.cursor;
            let digits = self.scan_digits(u32::from(radix), start)?;
            if digits == 0 {
                return Err(self
                    .error(code::MISSING_RADIX_DIGITS, start)
                    .with(u32::from(radix)));
            }
            integer_end = self.cursor;
        }

        if self.peek() == Some(b'n') {
            if fractional {
                self.bump();
                return Err(self.error(code::INVALID_BIGINT_LITERAL, start));
            }
            if radix == 10 && self.byte(start) == Some(b'0') && integer_end > start + 1 {
                self.bump();
                return Err(self.error(code::INVALID_BIGINT_LITERAL, start));
            }
            self.bump();
            is_bigint = true;
        }

        let length = self.cursor.saturating_sub(start);
        if length > self.limits.numeric_bytes {
            return Err(self
                .error(code::NUMERIC_LITERAL_TOO_LONG, start)
                .with(length)
                .with(self.limits.numeric_bytes));
        }

        if let Some(byte) = self.peek() {
            let terminator = if byte < 0x80 {
                u32::from(byte)
            } else {
                self.peek_scalar()?.unwrap_or(0)
            };
            if terminator < 0x80 && (byte.is_ascii_digit() || is_identifier_part(terminator)) {
                return Err(self
                    .error(code::INVALID_NUMERIC_TERMINATOR, start)
                    .with(terminator));
            }
            if terminator >= 0x80 && is_identifier_part(terminator) {
                return Err(self
                    .error(code::INVALID_NUMERIC_TERMINATOR, start)
                    .with(terminator));
            }
        }

        let kind = if is_bigint {
            TokenKind::BigInt
        } else {
            TokenKind::Number
        };
        let mut token = Token::empty(kind, start, self.cursor, line_break);
        token.radix = radix;
        token.inner_start = digits_start;
        token.inner_end = integer_end;
        if !is_bigint {
            token.number = if radix == 10 {
                decimal_value(DecimalLiteral {
                    integer: self.span(start, integer_end),
                    fraction: self.span(fraction_start, fraction_end),
                    exponent,
                })
            } else {
                radix_value(self.span(digits_start, integer_end), u32::from(radix))
            };
        }
        Ok(token)
    }

    /// Scan digits of `radix`, rejecting misplaced numeric separators.
    fn scan_digits(&mut self, radix: u32, start: u32) -> Result<u32, Diagnostic> {
        let mut digits = 0u32;
        let mut previous_separator = false;
        let mut first = true;

        loop {
            let Some(byte) = self.peek() else {
                break;
            };
            if byte == b'_' {
                if first || previous_separator {
                    self.bump();
                    return Err(self.error(code::INVALID_NUMERIC_SEPARATOR, start));
                }
                self.spend(1)?;
                self.bump();
                previous_separator = true;
                continue;
            }
            let Some(value) = hex_digit(byte) else {
                break;
            };
            if value >= radix {
                break;
            }
            self.spend(1)?;
            self.bump();
            digits = digits.saturating_add(1);
            previous_separator = false;
            first = false;
        }

        if previous_separator {
            return Err(self.error(code::INVALID_NUMERIC_SEPARATOR, start));
        }
        Ok(digits)
    }

    fn exponent_value(&self, digits_start: u32) -> i32 {
        let negative = matches!(self.byte(digits_start.saturating_sub(1)), Some(b'-'));
        let mut value = 0i32;
        let mut offset = digits_start;
        while offset < self.cursor {
            if let Some(byte) = self.byte(offset) {
                if byte.is_ascii_digit() {
                    value = value
                        .saturating_mul(10)
                        .saturating_add(i32::from(byte - b'0'));
                    if value > 100_000 {
                        value = 100_000;
                    }
                }
            }
            offset += 1;
        }
        if negative {
            -value
        } else {
            value
        }
    }

    /// Scan a regular-expression literal, validating framing and flags but not
    /// the pattern grammar.
    fn scan_regexp(&mut self, line_break: bool) -> Result<Token, Diagnostic> {
        let start = self.cursor;
        self.bump();
        let inner_start = self.cursor;
        let mut in_class = false;
        let inner_end;

        loop {
            let Some(byte) = self.peek() else {
                return Err(self.error(code::INVALID_REGEXP_LITERAL, start));
            };
            self.spend(1)?;
            match byte {
                b'\n' | b'\r' => return Err(self.error(code::INVALID_REGEXP_LITERAL, start)),
                b'\\' => {
                    self.bump();
                    match self.peek() {
                        None | Some(b'\n' | b'\r') => {
                            return Err(self.error(code::INVALID_REGEXP_LITERAL, start));
                        }
                        Some(byte) if byte < 0x80 => self.bump(),
                        Some(_) => {
                            // A backslash escapes no line terminator: the
                            // paragraph and line separators end the literal
                            // even behind one.
                            let code_point = self.peek_scalar()?.unwrap_or(0);
                            if code_point == 0x2028 || code_point == 0x2029 {
                                return Err(self.error(code::INVALID_REGEXP_LITERAL, start));
                            }
                            let _ = self.advance_scalar()?;
                        }
                    }
                }
                b'[' => {
                    in_class = true;
                    self.bump();
                }
                b']' => {
                    in_class = false;
                    self.bump();
                }
                b'/' if !in_class => {
                    inner_end = self.cursor;
                    self.bump();
                    break;
                }
                0x00..=0x7F => self.bump(),
                _ => {
                    let code_point = self.peek_scalar()?.unwrap_or(0);
                    if code_point == 0x2028 || code_point == 0x2029 {
                        return Err(self.error(code::INVALID_REGEXP_LITERAL, start));
                    }
                    let _ = self.advance_scalar()?;
                }
            }
        }

        if inner_end == inner_start {
            return Err(self.error(code::INVALID_REGEXP_LITERAL, start));
        }

        let mut flags = 0u16;
        loop {
            let Some(byte) = self.peek() else {
                break;
            };
            let code_point = if byte < 0x80 {
                u32::from(byte)
            } else {
                self.peek_scalar()?.unwrap_or(0)
            };
            if !is_identifier_part(code_point) {
                break;
            }
            let flag_start = self.cursor;
            let bit = match byte {
                b'd' => regexp_flag::HAS_INDICES,
                b'g' => regexp_flag::GLOBAL,
                b'i' => regexp_flag::IGNORE_CASE,
                b'm' => regexp_flag::MULTILINE,
                b's' => regexp_flag::DOT_ALL,
                b'u' => regexp_flag::UNICODE,
                b'v' => regexp_flag::UNICODE_SETS,
                b'y' => regexp_flag::STICKY,
                _ => {
                    let _ = self.advance_scalar()?;
                    return Err(self
                        .error(code::INVALID_REGEXP_FLAG, flag_start)
                        .with(code_point));
                }
            };
            self.bump();
            if flags & bit != 0 {
                return Err(self
                    .error(code::DUPLICATE_REGEXP_FLAG, flag_start)
                    .with(code_point));
            }
            flags |= bit;
        }

        let length = self.cursor.saturating_sub(start);
        if length > self.limits.regexp_bytes {
            return Err(self
                .error(code::REGEXP_LITERAL_TOO_LONG, start)
                .with(length)
                .with(self.limits.regexp_bytes));
        }

        let mut token = Token::empty(TokenKind::RegExp, start, self.cursor, line_break);
        token.inner_start = inner_start;
        token.inner_end = inner_end;
        token.flags = flags;
        Ok(token)
    }

    /// Scan one punctuator by longest match.
    fn scan_punctuator(&mut self, line_break: bool) -> Result<Token, Diagnostic> {
        let start = self.cursor;
        let first = self.peek().unwrap_or(0);
        let second = self.peek_at(1);
        let third = self.peek_at(2);
        let fourth = self.peek_at(3);

        let (punctuator, length) = match (first, second, third, fourth) {
            (b'>', Some(b'>'), Some(b'>'), Some(b'=')) => (Punctuator::UnsignedShiftRightAssign, 4),
            (b'.', Some(b'.'), Some(b'.'), _) => (Punctuator::Ellipsis, 3),
            (b'=', Some(b'='), Some(b'='), _) => (Punctuator::StrictEqual, 3),
            (b'!', Some(b'='), Some(b'='), _) => (Punctuator::StrictNotEqual, 3),
            (b'>', Some(b'>'), Some(b'>'), _) => (Punctuator::UnsignedShiftRight, 3),
            (b'*', Some(b'*'), Some(b'='), _) => (Punctuator::StarStarAssign, 3),
            (b'<', Some(b'<'), Some(b'='), _) => (Punctuator::ShiftLeftAssign, 3),
            (b'>', Some(b'>'), Some(b'='), _) => (Punctuator::ShiftRightAssign, 3),
            (b'&', Some(b'&'), Some(b'='), _) => (Punctuator::AmpersandAmpersandAssign, 3),
            (b'|', Some(b'|'), Some(b'='), _) => (Punctuator::PipePipeAssign, 3),
            (b'?', Some(b'?'), Some(b'='), _) => (Punctuator::QuestionQuestionAssign, 3),
            (b'=', Some(b'>'), _, _) => (Punctuator::Arrow, 2),
            (b'=', Some(b'='), _, _) => (Punctuator::Equal, 2),
            (b'!', Some(b'='), _, _) => (Punctuator::NotEqual, 2),
            (b'<', Some(b'='), _, _) => (Punctuator::LessEqual, 2),
            (b'>', Some(b'='), _, _) => (Punctuator::GreaterEqual, 2),
            (b'<', Some(b'<'), _, _) => (Punctuator::ShiftLeft, 2),
            (b'>', Some(b'>'), _, _) => (Punctuator::ShiftRight, 2),
            (b'+', Some(b'+'), _, _) => (Punctuator::PlusPlus, 2),
            (b'-', Some(b'-'), _, _) => (Punctuator::MinusMinus, 2),
            (b'*', Some(b'*'), _, _) => (Punctuator::StarStar, 2),
            (b'&', Some(b'&'), _, _) => (Punctuator::AmpersandAmpersand, 2),
            (b'|', Some(b'|'), _, _) => (Punctuator::PipePipe, 2),
            (b'?', Some(b'?'), _, _) => (Punctuator::QuestionQuestion, 2),
            (b'?', Some(b'.'), digit, _) if !matches!(digit, Some(b'0'..=b'9')) => {
                (Punctuator::OptionalChain, 2)
            }
            (b'+', Some(b'='), _, _) => (Punctuator::PlusAssign, 2),
            (b'-', Some(b'='), _, _) => (Punctuator::MinusAssign, 2),
            (b'*', Some(b'='), _, _) => (Punctuator::StarAssign, 2),
            (b'/', Some(b'='), _, _) => (Punctuator::SlashAssign, 2),
            (b'%', Some(b'='), _, _) => (Punctuator::PercentAssign, 2),
            (b'&', Some(b'='), _, _) => (Punctuator::AmpersandAssign, 2),
            (b'|', Some(b'='), _, _) => (Punctuator::PipeAssign, 2),
            (b'^', Some(b'='), _, _) => (Punctuator::CaretAssign, 2),
            (b'{', _, _, _) => (Punctuator::OpenBrace, 1),
            (b'}', _, _, _) => (Punctuator::CloseBrace, 1),
            (b'(', _, _, _) => (Punctuator::OpenParen, 1),
            (b')', _, _, _) => (Punctuator::CloseParen, 1),
            (b'[', _, _, _) => (Punctuator::OpenBracket, 1),
            (b']', _, _, _) => (Punctuator::CloseBracket, 1),
            (b';', _, _, _) => (Punctuator::Semicolon, 1),
            (b',', _, _, _) => (Punctuator::Comma, 1),
            (b':', _, _, _) => (Punctuator::Colon, 1),
            (b'~', _, _, _) => (Punctuator::Tilde, 1),
            (b'?', _, _, _) => (Punctuator::Question, 1),
            (b'.', _, _, _) => (Punctuator::Dot, 1),
            (b'<', _, _, _) => (Punctuator::Less, 1),
            (b'>', _, _, _) => (Punctuator::Greater, 1),
            (b'+', _, _, _) => (Punctuator::Plus, 1),
            (b'-', _, _, _) => (Punctuator::Minus, 1),
            (b'*', _, _, _) => (Punctuator::Star, 1),
            (b'/', _, _, _) => (Punctuator::Slash, 1),
            (b'%', _, _, _) => (Punctuator::Percent, 1),
            (b'&', _, _, _) => (Punctuator::Ampersand, 1),
            (b'|', _, _, _) => (Punctuator::Pipe, 1),
            (b'^', _, _, _) => (Punctuator::Caret, 1),
            (b'!', _, _, _) => (Punctuator::Bang, 1),
            (b'=', _, _, _) => (Punctuator::Assign, 1),
            _ => {
                self.bump();
                return Err(self
                    .error(code::INVALID_CHARACTER, start)
                    .with(u32::from(first)));
            }
        };

        for _ in 0..length {
            self.bump();
        }
        Ok(Token::empty(
            TokenKind::Punctuator(punctuator),
            start,
            self.cursor,
            line_break,
        ))
    }

    fn span(&self, start: u32, end: u32) -> &'s [u8] {
        if end <= start {
            return &[];
        }
        match self.source.get(start as usize..end as usize) {
            Some(slice) => slice,
            None => &[],
        }
    }
}

/// The result of one escape sequence.
enum Escape {
    /// A line continuation, which contributes nothing.
    Empty,
    /// A value contributing this many UTF-16 code units.
    Units(u32),
    /// A line continuation that also began a new line.
    LineTerminator,
}

const fn hex_digit(byte: u8) -> Option<u32> {
    match byte {
        b'0'..=b'9' => Some((byte - b'0') as u32),
        b'a'..=b'f' => Some((byte - b'a') as u32 + 10),
        b'A'..=b'F' => Some((byte - b'A') as u32 + 10),
        _ => None,
    }
}

/// ECMAScript adds `$` and `_` to the Unicode `ID_Start` property.
pub fn is_identifier_start(code_point: u32) -> bool {
    code_point == u32::from(b'$') || code_point == u32::from(b'_') || is_id_start(code_point)
}

/// ECMAScript adds `$`, zero-width non-joiner, and zero-width joiner to the
/// Unicode `ID_Continue` property, which already contains `_`.
pub fn is_identifier_part(code_point: u32) -> bool {
    code_point == u32::from(b'$')
        || code_point == 0x200C
        || code_point == 0x200D
        || is_id_continue(code_point)
}

/// Append the UTF-8 encoding of `code_point`, reporting whether it fitted.
fn append_utf8(buffer: &mut [u8; 12], length: &mut usize, code_point: u32) -> bool {
    let mut encoded = [0u8; 4];
    let width = if code_point < 0x80 {
        encoded[0] = u8::try_from(code_point).unwrap_or(0);
        1
    } else if code_point < 0x800 {
        encoded[0] = 0xC0 | u8::try_from(code_point >> 6).unwrap_or(0);
        encoded[1] = 0x80 | u8::try_from(code_point & 0x3F).unwrap_or(0);
        2
    } else if code_point < 0x1_0000 {
        encoded[0] = 0xE0 | u8::try_from(code_point >> 12).unwrap_or(0);
        encoded[1] = 0x80 | u8::try_from((code_point >> 6) & 0x3F).unwrap_or(0);
        encoded[2] = 0x80 | u8::try_from(code_point & 0x3F).unwrap_or(0);
        3
    } else {
        encoded[0] = 0xF0 | u8::try_from(code_point >> 18).unwrap_or(0);
        encoded[1] = 0x80 | u8::try_from((code_point >> 12) & 0x3F).unwrap_or(0);
        encoded[2] = 0x80 | u8::try_from((code_point >> 6) & 0x3F).unwrap_or(0);
        encoded[3] = 0x80 | u8::try_from(code_point & 0x3F).unwrap_or(0);
        4
    };
    if *length + width > buffer.len() {
        return false;
    }
    let mut index = 0usize;
    while index < width {
        buffer[*length] = encoded[index];
        *length += 1;
        index += 1;
    }
    true
}

/// Write the cooked value of a token into `out` as UTF-16 code units.
///
/// The token has already been validated by the lexer, so the only reasons this
/// returns `None` are a token that has no cooked value, a token that is not a
/// literal or identifier, and storage smaller than `token.code_units`.
pub fn cook(source: &[u8], token: &Token, out: &mut [u16]) -> Option<usize> {
    if !token.cooked_valid {
        return None;
    }
    let escapes = match token.kind {
        TokenKind::String
        | TokenKind::NoSubstitutionTemplate
        | TokenKind::TemplateHead
        | TokenKind::TemplateMiddle
        | TokenKind::TemplateTail => Escapes::Full,
        TokenKind::Identifier | TokenKind::PrivateName | TokenKind::Keyword(_) => {
            Escapes::UnicodeOnly
        }
        _ => return None,
    };
    if out.len() < token.code_units as usize {
        return None;
    }

    let mut cursor = token.inner_start as usize;
    let end = token.inner_end as usize;
    let mut written = 0usize;

    while cursor < end {
        let byte = *source.get(cursor)?;
        if byte == b'\\' {
            cursor += 1;
            let escaped = *source.get(cursor)?;
            match (escapes, escaped) {
                (Escapes::UnicodeOnly, b'u') | (Escapes::Full, b'u') => {
                    let (value, next) = read_unicode_escape(source, cursor + 1)?;
                    cursor = next;
                    written += write_scalar(out, written, value)?;
                }
                (Escapes::UnicodeOnly, _) => return None,
                (Escapes::Full, b'x') => {
                    let high = hex_digit(*source.get(cursor + 1)?)?;
                    let low = hex_digit(*source.get(cursor + 2)?)?;
                    cursor += 3;
                    written += write_scalar(out, written, high * 16 + low)?;
                }
                (Escapes::Full, b'\n') => cursor += 1,
                (Escapes::Full, b'\r') => {
                    cursor += 1;
                    if source.get(cursor) == Some(&b'\n') {
                        cursor += 1;
                    }
                }
                (Escapes::Full, b'0') => {
                    cursor += 1;
                    written += write_scalar(out, written, 0)?;
                }
                (Escapes::Full, _) => {
                    let decoded = decode(source, cursor)?;
                    cursor += decoded.length as usize;
                    let value = match decoded.code_point {
                        0x62 => 0x08,
                        0x66 => 0x0C,
                        0x6E => 0x0A,
                        0x72 => 0x0D,
                        0x74 => 0x09,
                        0x76 => 0x0B,
                        0x2028 | 0x2029 => {
                            continue;
                        }
                        other => other,
                    };
                    written += write_scalar(out, written, value)?;
                }
            }
            continue;
        }

        // A carriage return in a template part cooks to a single line feed.
        if byte == b'\r' && matches!(escapes, Escapes::Full) {
            cursor += 1;
            if source.get(cursor) == Some(&b'\n') {
                cursor += 1;
            }
            written += write_scalar(out, written, 0x0A)?;
            continue;
        }

        let decoded = decode(source, cursor)?;
        cursor += decoded.length as usize;
        written += write_scalar(out, written, decoded.code_point)?;
    }

    Some(written)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Escapes {
    Full,
    UnicodeOnly,
}

fn read_unicode_escape(source: &[u8], offset: usize) -> Option<(u32, usize)> {
    if source.get(offset) == Some(&b'{') {
        let mut cursor = offset + 1;
        let mut value = 0u32;
        loop {
            let byte = *source.get(cursor)?;
            if byte == b'}' {
                return Some((value, cursor + 1));
            }
            value = value.checked_mul(16)?.checked_add(hex_digit(byte)?)?;
            cursor += 1;
        }
    }
    let mut value = 0u32;
    let mut cursor = offset;
    for _ in 0..4 {
        value = value * 16 + hex_digit(*source.get(cursor)?)?;
        cursor += 1;
    }
    Some((value, cursor))
}

/// Write one scalar value as UTF-16, returning the code units written.
///
/// A value in the surrogate range is written verbatim, because a lone surrogate
/// written with an escape is a legitimate ECMAScript string element.
fn write_scalar(out: &mut [u16], written: usize, value: u32) -> Option<usize> {
    if value > 0xFFFF {
        let adjusted = value.checked_sub(0x1_0000)?;
        let high = u16::try_from(0xD800 + (adjusted >> 10)).ok()?;
        let low = u16::try_from(0xDC00 + (adjusted & 0x3FF)).ok()?;
        *out.get_mut(written)? = high;
        *out.get_mut(written + 1)? = low;
        return Some(2);
    }
    *out.get_mut(written)? = u16::try_from(value).ok()?;
    Some(1)
}
