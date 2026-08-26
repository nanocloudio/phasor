//! Stable diagnostic records shared by every Phasor front-end phase.
//!
//! A diagnostic is a value, not text. It carries a stable code, a severity, a
//! byte span, and up to four integer arguments. It never carries source bytes,
//! because a record that quotes the program leaks it to every holder of the
//! record.

/// Stable diagnostic codes. A code keeps its number and meaning permanently.
pub mod code {
    // Request and transfer, 0x0000-0x00FF.
    pub const TRANSFER_INCOMPLETE: u16 = 0x0001;
    pub const TRANSFER_OVERFLOW: u16 = 0x0002;
    pub const DIGEST_MISMATCH: u16 = 0x0003;
    pub const FEATURE_DIGEST_MISMATCH: u16 = 0x0004;
    pub const UNSUPPORTED_GOAL: u16 = 0x0005;
    pub const COMPILE_BUDGET_EXHAUSTED: u16 = 0x0006;
    pub const TOO_MANY_DIAGNOSTICS: u16 = 0x0007;
    pub const SOURCE_TOO_LARGE: u16 = 0x0010;
    pub const TOO_MANY_LINES: u16 = 0x0011;
    pub const LINE_TOO_LONG: u16 = 0x0012;
    pub const TOO_MANY_TOKENS: u16 = 0x0013;
    pub const IDENTIFIER_TOO_LONG: u16 = 0x0014;
    pub const LITERAL_TOO_LONG: u16 = 0x0015;
    pub const NUMERIC_LITERAL_TOO_LONG: u16 = 0x0016;
    pub const REGEXP_LITERAL_TOO_LONG: u16 = 0x0017;
    pub const TEMPLATE_NESTING_TOO_DEEP: u16 = 0x0018;
    pub const FEATURE_NOT_ADMITTED: u16 = 0x0019;

    // Lexical, 0x0100-0x01FF.
    pub const INVALID_UTF8: u16 = 0x0100;
    pub const INVALID_CHARACTER: u16 = 0x0101;
    pub const UNTERMINATED_COMMENT: u16 = 0x0102;
    pub const HASHBANG_NOT_AT_START: u16 = 0x0103;
    pub const INVALID_IDENTIFIER_ESCAPE: u16 = 0x0110;
    pub const ESCAPED_RESERVED_WORD: u16 = 0x0111;
    pub const INVALID_NUMERIC_SEPARATOR: u16 = 0x0120;
    pub const LEGACY_OCTAL_LITERAL: u16 = 0x0121;
    pub const INVALID_NUMERIC_TERMINATOR: u16 = 0x0122;
    pub const MISSING_RADIX_DIGITS: u16 = 0x0123;
    pub const INVALID_BIGINT_LITERAL: u16 = 0x0124;
    pub const UNTERMINATED_STRING: u16 = 0x0130;
    pub const INVALID_ESCAPE: u16 = 0x0131;
    pub const LEGACY_OCTAL_ESCAPE: u16 = 0x0132;
    pub const INVALID_CODE_POINT: u16 = 0x0133;
    pub const UNTERMINATED_TEMPLATE: u16 = 0x0140;
    pub const INVALID_REGEXP_LITERAL: u16 = 0x0150;
    pub const INVALID_REGEXP_FLAG: u16 = 0x0151;
    pub const DUPLICATE_REGEXP_FLAG: u16 = 0x0152;
    pub const REGEXP_PATTERN_UNSUPPORTED: u16 = 0x0153;

    // Syntactic, 0x0200-0x02FF.
    pub const UNEXPECTED_TOKEN: u16 = 0x0200;
    pub const UNEXPECTED_END_OF_SOURCE: u16 = 0x0201;
    pub const EXPECTED_EXPRESSION: u16 = 0x0202;
    pub const EXPECTED_CLOSE_PAREN: u16 = 0x0203;
    pub const EXPECTED_CLOSE_BRACKET: u16 = 0x0204;
    pub const EXPECTED_CLOSE_BRACE: u16 = 0x0205;
    pub const EXPECTED_COLON: u16 = 0x0206;
    pub const EXPECTED_PROPERTY_NAME: u16 = 0x0207;
    pub const INVALID_ASSIGNMENT_TARGET: u16 = 0x0208;
    pub const OPTIONAL_CHAIN_ASSIGNMENT: u16 = 0x0209;
    pub const EXPONENT_OF_UNARY: u16 = 0x020A;
    pub const PRIVATE_NAME_OUT_OF_CONTEXT: u16 = 0x020B;
    pub const EXPRESSION_TOO_DEEP: u16 = 0x020C;
    pub const TOO_MANY_SYNTAX_NODES: u16 = 0x020D;
    pub const SYNTAX_NOT_ADMITTED: u16 = 0x020E;
    pub const MISSING_INITIALISER: u16 = 0x020F;
    pub const INVALID_ARROW_PARAMETERS: u16 = 0x0210;
    pub const DUPLICATE_BINDING: u16 = 0x0211;
    pub const ASSIGNMENT_TO_CONSTANT: u16 = 0x0212;
    pub const UNDECLARED_LABEL: u16 = 0x0213;
    pub const ILLEGAL_BREAK_OR_CONTINUE: u16 = 0x0214;
    pub const RETURN_OUTSIDE_FUNCTION: u16 = 0x0215;
    /// Strict code refuses to assign `eval` or `arguments`.
    pub const STRICT_ASSIGNMENT_TO_RESTRICTED_NAME: u16 = 0x0216;
    /// A strict function refuses a parameter named `eval` or `arguments`,
    /// and refuses two parameters with one name.
    pub const STRICT_INVALID_PARAMETER: u16 = 0x0217;

    // Bytecode emission and verification, 0x0400-0x04FF.
    pub const UNKNOWN_OPCODE: u16 = 0x0400;
    pub const TRUNCATED_OPERAND: u16 = 0x0401;
    pub const MISPLACED_PREFIX: u16 = 0x0402;
    pub const REGISTER_OUT_OF_RANGE: u16 = 0x0403;
    pub const CONSTANT_OUT_OF_RANGE: u16 = 0x0404;
    pub const INVALID_JUMP_TARGET: u16 = 0x0405;
    pub const BACKWARD_JUMP_WITHOUT_SAFE_POINT: u16 = 0x0406;
    pub const INVALID_EXCEPTION_REGION: u16 = 0x0407;
    pub const OVERLAPPING_EXCEPTION_REGIONS: u16 = 0x0408;
    pub const CONTEXT_DEPTH_MISMATCH: u16 = 0x0409;
    pub const CONTEXT_DEPTH_OUT_OF_RANGE: u16 = 0x040A;
    pub const FALLS_OFF_END: u16 = 0x040B;
    pub const INVALID_SAFE_POINT: u16 = 0x040C;
    pub const UNREACHABLE_CODE: u16 = 0x040D;
    pub const INCONSISTENT_DECLARED_BOUNDS: u16 = 0x040E;
    pub const MALFORMED_IMAGE: u16 = 0x040F;
    pub const BYTECODE_FORMAT_MISMATCH: u16 = 0x0410;
    pub const VERIFIER_STORAGE_TOO_SMALL: u16 = 0x0411;
    pub const CODE_TOO_LARGE: u16 = 0x0412;
    pub const TOO_MANY_CONSTANTS: u16 = 0x0413;
    pub const TOO_MANY_REGISTERS: u16 = 0x0414;
    pub const JUMP_TOO_FAR: u16 = 0x0415;
    pub const LOWERING_NOT_ADMITTED: u16 = 0x0416;
    pub const IMAGE_NOT_ADMITTED: u16 = 0x0417;
    pub const FEATURE_LIST_MISMATCH: u16 = 0x0418;
}

/// Enumerated `image feature` argument values, naming what an image asked for
/// that this build does not implement. An image is refused for one of these
/// when it is admitted, rather than terminating part-way through a run.
pub mod image_feature {
    pub const BIG_INT: u32 = 0;
}

/// Why a task stopped without producing a value, `0x0500`-`0x05FF`.
///
/// A termination is not a defect in the program's text, so it has no span: it
/// is what happened when the program ran.
pub mod termination {
    pub const FUEL_EXHAUSTED: u16 = 0x0500;
    pub const QUOTA_EXCEEDED: u16 = 0x0501;
    pub const CANCELLED: u16 = 0x0502;
    pub const DEADLINE_REACHED: u16 = 0x0503;
    pub const STACK_OVERFLOW: u16 = 0x0504;
    pub const REGISTERS_EXHAUSTED: u16 = 0x0505;
    pub const HEAP_EXHAUSTED: u16 = 0x0506;
    pub const NOT_IMPLEMENTED: u16 = 0x0507;
    pub const MALFORMED_IMAGE_AT_RUN_TIME: u16 = 0x0508;
    /// The task's value was a rejected promise, and the cause it carried.
    pub const REJECTED: u16 = 0x0509;
    /// The task threw, and nothing caught it.
    pub const UNCAUGHT_THROW: u16 = 0x050A;
}

/// Enumerated `phase` argument values.
pub mod phase {
    pub const DECODE: u32 = 0;
    pub const LEX: u32 = 1;
    pub const PARSE: u32 = 2;
    pub const STATIC_SEMANTICS: u32 = 3;
    pub const LOWER: u32 = 4;
    pub const VERIFY: u32 = 5;
    pub const SERIALISE: u32 = 6;
}

/// Enumerated `arena` argument values, naming which storage was exhausted.
pub mod arena {
    pub const NODES: u32 = 0;
    pub const LISTS: u32 = 1;
    pub const NUMBERS: u32 = 2;
    pub const STACK: u32 = 3;
}

/// Enumerated `syntax feature` argument values for constructs the admitted
/// feature list does not contain.
pub mod syntax_feature {
    pub const ARROW_FUNCTION: u32 = 0;
    pub const FUNCTION_EXPRESSION: u32 = 1;
    pub const CLASS_EXPRESSION: u32 = 2;
    pub const ASYNC: u32 = 3;
    pub const YIELD: u32 = 4;
    pub const SUPER: u32 = 5;
    pub const IMPORT: u32 = 6;
    pub const NEW_TARGET: u32 = 7;
    pub const DESTRUCTURING: u32 = 8;
    pub const METHOD_DEFINITION: u32 = 9;
    pub const REGEXP_PATTERN: u32 = 10;
    pub const STATEMENT: u32 = 11;
}

/// Enumerated `image` argument values, naming why an image was rejected.
pub mod image {
    pub const MAGIC: u32 = 0;
    pub const FORMAT_DIGEST: u32 = 1;
    pub const TRUNCATED: u32 = 2;
    pub const OVERFLOW: u32 = 3;
    pub const FEATURE_DIGEST: u32 = 4;
}

/// Enumerated `escape kind` argument values.
pub mod escape_kind {
    pub const HEXADECIMAL: u32 = 0;
    pub const UNICODE: u32 = 1;
    pub const CODE_POINT: u32 = 2;
    pub const UNRECOGNISED: u32 = 3;
}

/// How far a diagnostic stops the work in progress.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Severity {
    /// The unit is not compilable; the phase stays available for the next request.
    Error,
    /// The request itself is unusable and nothing further is reported for it.
    Fatal,
}

/// Maximum number of populated argument slots.
pub const MAX_ARGUMENTS: usize = 4;

/// Bytes in an encoded diagnostic, which is what one fmod hands another.
pub const FRAME: usize = 32;

/// One rejected construct, described without quoting the source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    code: u16,
    severity: Severity,
    offset: u32,
    length: u32,
    argument_count: u8,
    arguments: [u32; MAX_ARGUMENTS],
}

impl Diagnostic {
    /// A diagnostic covering the half-open byte span `[offset, offset + length)`.
    pub const fn new(code: u16, severity: Severity, offset: u32, length: u32) -> Self {
        Self {
            code,
            severity,
            offset,
            length,
            argument_count: 0,
            arguments: [0; MAX_ARGUMENTS],
        }
    }

    /// A diagnostic at a point rather than over a construct.
    pub const fn at(code: u16, severity: Severity, offset: u32) -> Self {
        Self::new(code, severity, offset, 0)
    }

    /// Append one integer argument. Arguments beyond the fourth are dropped
    /// rather than reallocating or panicking.
    #[must_use]
    pub const fn with(mut self, argument: u32) -> Self {
        if (self.argument_count as usize) < MAX_ARGUMENTS {
            self.arguments[self.argument_count as usize] = argument;
            self.argument_count += 1;
        }
        self
    }

    pub const fn code(&self) -> u16 {
        self.code
    }

    pub const fn severity(&self) -> Severity {
        self.severity
    }

    pub const fn is_fatal(&self) -> bool {
        matches!(self.severity, Severity::Fatal)
    }

    pub const fn offset(&self) -> u32 {
        self.offset
    }

    pub const fn length(&self) -> u32 {
        self.length
    }

    /// The populated argument slots, in the order the code defines.
    /// The wire form: what a phase hands to whatever renders it.
    ///
    /// A diagnostic crosses a port as numbers — a code, a severity, a span, and
    /// its arguments — and never as text. What it reads as in a human's
    /// language is decided at the edge, by whoever knows which language that
    /// is.
    pub fn encode(&self) -> [u8; FRAME] {
        let mut frame = [0u8; FRAME];
        frame[0..2].copy_from_slice(&self.code.to_le_bytes());
        frame[2] = matches!(self.severity, Severity::Fatal) as u8;
        frame[3] = self.argument_count;
        frame[4..8].copy_from_slice(&self.offset.to_le_bytes());
        frame[8..12].copy_from_slice(&self.length.to_le_bytes());
        let mut index = 0usize;
        while index < MAX_ARGUMENTS {
            let at = 16 + index * 4;
            frame[at..at + 4].copy_from_slice(&self.arguments[index].to_le_bytes());
            index += 1;
        }
        frame
    }

    /// Read a diagnostic back from its wire form.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let frame = bytes.get(..FRAME)?;
        let mut arguments = [0u32; MAX_ARGUMENTS];
        let mut index = 0usize;
        while index < MAX_ARGUMENTS {
            let at = 16 + index * 4;
            arguments[index] =
                u32::from_le_bytes([frame[at], frame[at + 1], frame[at + 2], frame[at + 3]]);
            index += 1;
        }
        Some(Self {
            code: u16::from_le_bytes([frame[0], frame[1]]),
            severity: if frame[2] == 0 {
                Severity::Error
            } else {
                Severity::Fatal
            },
            offset: u32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]),
            length: u32::from_le_bytes([frame[8], frame[9], frame[10], frame[11]]),
            argument_count: frame[3].min(MAX_ARGUMENTS as u8),
            arguments,
        })
    }

    pub fn arguments(&self) -> &[u32] {
        let count = self.argument_count as usize;
        match self.arguments.get(..count) {
            Some(slice) => slice,
            None => &[],
        }
    }
}
