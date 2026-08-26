//! The Phasor bytecode format.
//!
//! The virtual machine is an accumulator-and-register machine: common results
//! flow through an accumulator, and indexed frame registers hold locals,
//! temporaries, and arguments. Operands are one byte by default, with `Wide`
//! and `ExtraWide` prefixes for larger tables, which keeps ordinary code
//! compact without a second instruction set.
//!
//! A unit is a canonical byte image: little-endian fixed-width integers,
//! explicit offsets, no implicit padding, and no pointers. The same source
//! therefore produces the same bytes on every target, and the image's digest is
//! its identity.

use crate::digest::{Digest, Hasher};

/// Bumped whenever the meaning of any existing encoding changes. It is one
/// input to the format digest, which is what an image is actually admitted
/// against.
pub const FORMAT_TAG: [u8; 8] = *b"PHBC0001";

/// Magic bytes at the head of a unit image.
pub const UNIT_MAGIC: [u8; 4] = *b"PHBC";

/// Instruction operand widths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Width {
    Narrow,
    Wide,
    ExtraWide,
}

impl Width {
    pub const fn bytes(self) -> u32 {
        match self {
            Self::Narrow => 1,
            Self::Wide => 2,
            Self::ExtraWide => 4,
        }
    }
}

/// The `Wide` operand prefix.
pub const PREFIX_WIDE: u8 = 0xFE;
/// The `ExtraWide` operand prefix.
pub const PREFIX_EXTRA_WIDE: u8 = 0xFF;

/// What one operand denotes, which is what the verifier checks it against.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperandKind {
    /// A frame register index.
    Register,
    /// An index into the constant table.
    Constant,
    /// A signed immediate.
    Immediate,
    /// A count of registers, arguments, or slots.
    Count,
    /// A signed jump displacement from the start of this instruction.
    Jump,
    /// A context depth.
    Depth,
}

/// An opcode's operand list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Signature {
    pub count: u8,
    pub kinds: [OperandKind; 3],
}

impl Signature {
    const fn new(count: u8, kinds: [OperandKind; 3]) -> Self {
        Self { count, kinds }
    }
}

/// The instruction set.
///
/// Binary and comparison operations read their left operand from a register and
/// their right operand from the accumulator, and leave the result in the
/// accumulator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Opcode {
    // Accumulator loads.
    LdaUndefined = 0x00,
    LdaNull = 0x01,
    LdaTrue = 0x02,
    LdaFalse = 0x03,
    LdaZero = 0x04,
    LdaSmi = 0x05,
    LdaConstant = 0x06,

    // Register moves.
    Ldar = 0x10,
    Star = 0x11,
    Mov = 0x12,

    // Arithmetic.
    Add = 0x20,
    Sub = 0x21,
    Mul = 0x22,
    Div = 0x23,
    Mod = 0x24,
    Exp = 0x25,
    BitAnd = 0x26,
    BitOr = 0x27,
    BitXor = 0x28,
    ShiftLeft = 0x29,
    ShiftRight = 0x2A,
    ShiftRightLogical = 0x2B,

    // Unary operations on the accumulator.
    Inc = 0x30,
    Dec = 0x31,
    Negate = 0x32,
    BitNot = 0x33,
    LogicalNot = 0x34,
    TypeOf = 0x35,
    ToNumeric = 0x36,
    ToString = 0x37,
    ToPropertyKey = 0x38,

    // Comparisons.
    TestEqual = 0x40,
    TestNotEqual = 0x41,
    TestStrictEqual = 0x42,
    TestStrictNotEqual = 0x43,
    TestLess = 0x44,
    TestGreater = 0x45,
    TestLessEqual = 0x46,
    TestGreaterEqual = 0x47,
    TestInstanceOf = 0x48,
    TestIn = 0x49,

    // Properties.
    GetNamedProperty = 0x50,
    GetKeyedProperty = 0x51,
    SetNamedProperty = 0x52,
    SetKeyedProperty = 0x53,
    DeleteNamedProperty = 0x54,
    DeleteKeyedProperty = 0x55,

    // Bindings.
    LdaGlobal = 0x60,
    StaGlobal = 0x61,
    LdaContextSlot = 0x62,
    StaContextSlot = 0x63,
    PushContext = 0x64,
    PopContext = 0x65,
    /// Like `LdaGlobal`, but an unresolvable name is `undefined` rather than a
    /// reference error, which is what `typeof` needs.
    LdaGlobalOrUndefined = 0x66,
    /// Give a context slot its first value. A slot that has none is in the
    /// temporal dead zone, and reading it is a reference error.
    InitContextSlot = 0x67,
    /// Build a function object over the current environment. The operand is a
    /// function index in this unit.
    CreateClosure = 0x68,
    /// The value an import names, read through the module that exports it, so
    /// what it sees is what that module holds now.
    LdaImport = 0x79,
    /// Build a regular expression from a pattern constant. A literal makes a
    /// new one every time it is evaluated, which is what the specification
    /// says, because its `lastIndex` is state.
    CreateRegExp = 0x6F,
    /// The `this` of the nearest enclosing function, which is the receiver a
    /// call was made with. An arrow has none of its own, so it reads the one
    /// its definition was inside.
    LdaThis = 0x69,
    /// Define a global property for a `var` that has none yet, so a name that
    /// is declared but never assigned still reads as `undefined`.
    DeclareGlobal = 0x6A,
    /// The function object the running frame was called through, which is what
    /// a function's own name refers to inside it.
    LdaCallee = 0x6B,
    /// Ask the accumulator for its iterator, which is what `for (x of y)` and
    /// a spread do before anything else.
    GetIterator = 0x6C,
    /// Take one step of the iterator in the operand register. The accumulator
    /// becomes the value, and the flag register the operand names is set to
    /// whether the iteration is done.
    IteratorNext = 0x6D,
    /// The enumerable string-keyed property names of the accumulator and its
    /// prototypes, as an array, which is what `for (x in y)` walks.
    GetEnumerable = 0x6E,

    // Object and array construction.
    CreateEmptyArray = 0x70,
    CreateEmptyObject = 0x71,
    AppendArrayElement = 0x72,
    AppendArrayHole = 0x74,
    DefineNamedProperty = 0x75,
    DefineKeyedProperty = 0x76,
    CopyDataProperties = 0x77,
    /// Build the `arguments` array from the running frame's actual arguments.
    /// The values are in the frame's first registers and the count is what the
    /// call supplied, so the object says how the function was called rather
    /// than what it declared.
    CreateArguments = 0x78,
    /// Set the object in the operand register's prototype to the accumulator,
    /// when the accumulator is an object or null; anything else is ignored.
    /// This is what `__proto__:` in an object literal means.
    SetPrototype = 0x73,

    // Calls.
    Call = 0x80,
    CallProperty = 0x81,
    Construct = 0x82,
    /// Call with the arguments an array holds, which is what a spread argument
    /// needs: how many there are is not known where the call is written.
    CallWithArray = 0x83,

    // Control flow.
    Jump = 0x90,
    JumpIfTrue = 0x91,
    JumpIfFalse = 0x92,
    JumpIfToBooleanTrue = 0x93,
    JumpIfToBooleanFalse = 0x94,
    JumpIfNullish = 0x95,
    JumpIfNotNullish = 0x96,
    Return = 0x97,
    Throw = 0x98,
}

impl Opcode {
    /// The opcode a byte denotes, or `None` when the byte is not one.
    pub const fn from_byte(byte: u8) -> Option<Self> {
        let opcode = match byte {
            0x00 => Self::LdaUndefined,
            0x01 => Self::LdaNull,
            0x02 => Self::LdaTrue,
            0x03 => Self::LdaFalse,
            0x04 => Self::LdaZero,
            0x05 => Self::LdaSmi,
            0x06 => Self::LdaConstant,
            0x10 => Self::Ldar,
            0x11 => Self::Star,
            0x12 => Self::Mov,
            0x20 => Self::Add,
            0x21 => Self::Sub,
            0x22 => Self::Mul,
            0x23 => Self::Div,
            0x24 => Self::Mod,
            0x25 => Self::Exp,
            0x26 => Self::BitAnd,
            0x27 => Self::BitOr,
            0x28 => Self::BitXor,
            0x29 => Self::ShiftLeft,
            0x2A => Self::ShiftRight,
            0x2B => Self::ShiftRightLogical,
            0x30 => Self::Inc,
            0x31 => Self::Dec,
            0x32 => Self::Negate,
            0x33 => Self::BitNot,
            0x34 => Self::LogicalNot,
            0x35 => Self::TypeOf,
            0x36 => Self::ToNumeric,
            0x37 => Self::ToString,
            0x38 => Self::ToPropertyKey,
            0x40 => Self::TestEqual,
            0x41 => Self::TestNotEqual,
            0x42 => Self::TestStrictEqual,
            0x43 => Self::TestStrictNotEqual,
            0x44 => Self::TestLess,
            0x45 => Self::TestGreater,
            0x46 => Self::TestLessEqual,
            0x47 => Self::TestGreaterEqual,
            0x48 => Self::TestInstanceOf,
            0x49 => Self::TestIn,
            0x50 => Self::GetNamedProperty,
            0x51 => Self::GetKeyedProperty,
            0x52 => Self::SetNamedProperty,
            0x53 => Self::SetKeyedProperty,
            0x54 => Self::DeleteNamedProperty,
            0x55 => Self::DeleteKeyedProperty,
            0x60 => Self::LdaGlobal,
            0x61 => Self::StaGlobal,
            0x62 => Self::LdaContextSlot,
            0x63 => Self::StaContextSlot,
            0x64 => Self::PushContext,
            0x65 => Self::PopContext,
            0x66 => Self::LdaGlobalOrUndefined,
            0x67 => Self::InitContextSlot,
            0x68 => Self::CreateClosure,
            0x69 => Self::LdaThis,
            0x6A => Self::DeclareGlobal,
            0x6B => Self::LdaCallee,
            0x6C => Self::GetIterator,
            0x6D => Self::IteratorNext,
            0x6E => Self::GetEnumerable,
            0x6F => Self::CreateRegExp,
            0x79 => Self::LdaImport,
            0x70 => Self::CreateEmptyArray,
            0x71 => Self::CreateEmptyObject,
            0x72 => Self::AppendArrayElement,
            0x74 => Self::AppendArrayHole,
            0x75 => Self::DefineNamedProperty,
            0x76 => Self::DefineKeyedProperty,
            0x77 => Self::CopyDataProperties,
            0x78 => Self::CreateArguments,
            0x73 => Self::SetPrototype,
            0x80 => Self::Call,
            0x81 => Self::CallProperty,
            0x82 => Self::Construct,
            0x83 => Self::CallWithArray,
            0x90 => Self::Jump,
            0x91 => Self::JumpIfTrue,
            0x92 => Self::JumpIfFalse,
            0x93 => Self::JumpIfToBooleanTrue,
            0x94 => Self::JumpIfToBooleanFalse,
            0x95 => Self::JumpIfNullish,
            0x96 => Self::JumpIfNotNullish,
            0x97 => Self::Return,
            0x98 => Self::Throw,
            _ => return None,
        };
        Some(opcode)
    }

    pub const fn byte(self) -> u8 {
        self as u8
    }

    /// The operands this opcode takes.
    pub const fn signature(self) -> Signature {
        use OperandKind::{Constant, Count, Depth, Immediate, Jump, Register};
        const NONE: OperandKind = OperandKind::Count;
        match self {
            Self::LdaUndefined
            | Self::LdaNull
            | Self::LdaTrue
            | Self::LdaFalse
            | Self::LdaZero
            | Self::Inc
            | Self::Dec
            | Self::Negate
            | Self::BitNot
            | Self::LogicalNot
            | Self::TypeOf
            | Self::ToNumeric
            | Self::ToString
            | Self::ToPropertyKey
            | Self::CreateEmptyArray
            | Self::CreateEmptyObject
            | Self::PopContext
            | Self::LdaThis
            | Self::LdaCallee
            | Self::GetIterator
            | Self::GetEnumerable
            | Self::CreateArguments
            | Self::Return
            | Self::Throw => Signature::new(0, [NONE, NONE, NONE]),

            Self::LdaSmi => Signature::new(1, [Immediate, NONE, NONE]),
            Self::LdaConstant
            | Self::LdaGlobal
            | Self::LdaGlobalOrUndefined
            | Self::StaGlobal
            | Self::DeclareGlobal
            | Self::DeleteNamedProperty => Signature::new(1, [Constant, NONE, NONE]),
            Self::Ldar
            | Self::Star
            | Self::Add
            | Self::Sub
            | Self::Mul
            | Self::Div
            | Self::Mod
            | Self::Exp
            | Self::BitAnd
            | Self::BitOr
            | Self::BitXor
            | Self::ShiftLeft
            | Self::ShiftRight
            | Self::ShiftRightLogical
            | Self::TestEqual
            | Self::TestNotEqual
            | Self::TestStrictEqual
            | Self::TestStrictNotEqual
            | Self::TestLess
            | Self::TestGreater
            | Self::TestLessEqual
            | Self::TestGreaterEqual
            | Self::TestInstanceOf
            | Self::TestIn
            | Self::GetKeyedProperty
            | Self::AppendArrayElement
            | Self::AppendArrayHole
            | Self::CopyDataProperties
            | Self::DeleteKeyedProperty => Signature::new(1, [Register, NONE, NONE]),
            Self::PushContext => Signature::new(1, [Count, NONE, NONE]),
            Self::IteratorNext => Signature::new(2, [Register, Register, NONE]),
            Self::Jump
            | Self::JumpIfTrue
            | Self::JumpIfFalse
            | Self::JumpIfToBooleanTrue
            | Self::JumpIfToBooleanFalse
            | Self::JumpIfNullish
            | Self::JumpIfNotNullish => Signature::new(1, [Jump, NONE, NONE]),

            Self::Mov => Signature::new(2, [Register, Register, NONE]),
            Self::GetNamedProperty | Self::SetNamedProperty | Self::DefineNamedProperty => {
                Signature::new(2, [Register, Constant, NONE])
            }
            Self::SetKeyedProperty | Self::DefineKeyedProperty => {
                Signature::new(2, [Register, Register, NONE])
            }
            Self::LdaContextSlot | Self::StaContextSlot | Self::InitContextSlot => {
                Signature::new(2, [Count, Depth, NONE])
            }
            Self::SetPrototype => Signature::new(1, [Register, NONE, NONE]),
            Self::CreateClosure => Signature::new(1, [Immediate, NONE, NONE]),
            Self::CreateRegExp => Signature::new(1, [Constant, NONE, NONE]),
            Self::LdaImport => Signature::new(1, [Immediate, NONE, NONE]),

            Self::Call | Self::Construct => Signature::new(3, [Register, Register, Count]),
            Self::CallWithArray => Signature::new(3, [Register, Register, Register]),
            Self::CallProperty => Signature::new(3, [Register, Register, Count]),
        }
    }

    /// Whether control cannot continue to the next instruction.
    pub const fn is_terminator(self) -> bool {
        matches!(self, Self::Return | Self::Throw | Self::Jump)
    }

    /// Whether the instruction transfers control to a displacement.
    pub const fn is_branch(self) -> bool {
        matches!(
            self,
            Self::Jump
                | Self::JumpIfTrue
                | Self::JumpIfFalse
                | Self::JumpIfToBooleanTrue
                | Self::JumpIfToBooleanFalse
                | Self::JumpIfNullish
                | Self::JumpIfNotNullish
        )
    }

    /// The highest assigned opcode byte, which bounds the format descriptor.
    pub const MAX_BYTE: u8 = 0x98;
}

/// One decoded instruction.
#[derive(Clone, Copy, Debug)]
pub struct Instruction {
    pub opcode: Opcode,
    pub width: Width,
    /// Operand values, zero-extended, except `Jump` and `Immediate` which are
    /// sign-extended into `signed`.
    pub operands: [u32; 3],
    pub signed: [i32; 3],
    /// Total encoded length, including any prefix.
    pub length: u32,
}

/// Why a byte sequence is not an instruction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    UnknownOpcode,
    TruncatedOperand,
    MisplacedPrefix,
}

/// Decode the instruction at `offset`.
pub fn decode(code: &[u8], offset: u32) -> Result<Instruction, DecodeError> {
    let mut cursor = offset as usize;
    let first = *code.get(cursor).ok_or(DecodeError::TruncatedOperand)?;
    let width = match first {
        PREFIX_WIDE => Width::Wide,
        PREFIX_EXTRA_WIDE => Width::ExtraWide,
        _ => Width::Narrow,
    };
    if !matches!(width, Width::Narrow) {
        cursor += 1;
    }

    let byte = *code.get(cursor).ok_or(DecodeError::TruncatedOperand)?;
    if byte == PREFIX_WIDE || byte == PREFIX_EXTRA_WIDE {
        return Err(DecodeError::MisplacedPrefix);
    }
    let opcode = Opcode::from_byte(byte).ok_or(DecodeError::UnknownOpcode)?;
    cursor += 1;

    let signature = opcode.signature();
    let size = width.bytes() as usize;
    let mut operands = [0u32; 3];
    let mut signed = [0i32; 3];
    let mut index = 0usize;
    while index < signature.count as usize {
        let bytes = code
            .get(cursor..cursor + size)
            .ok_or(DecodeError::TruncatedOperand)?;
        let mut value = 0u32;
        let mut byte_index = 0usize;
        while byte_index < size {
            value |= u32::from(bytes[byte_index]) << (8 * byte_index);
            byte_index += 1;
        }
        operands[index] = value;
        signed[index] = sign_extend(value, width);
        cursor += size;
        index += 1;
    }

    Ok(Instruction {
        opcode,
        width,
        operands,
        signed,
        length: u32::try_from(cursor - offset as usize).unwrap_or(u32::MAX),
    })
}

const fn sign_extend(value: u32, width: Width) -> i32 {
    match width {
        Width::Narrow => value as u8 as i8 as i32,
        Width::Wide => value as u16 as i16 as i32,
        Width::ExtraWide => value as i32,
    }
}

/// The narrowest width that encodes every operand of an instruction.
pub fn width_for(signature: Signature, operands: &[i64]) -> Width {
    let mut width = Width::Narrow;
    let mut index = 0usize;
    while index < signature.count as usize {
        let value = match operands.get(index) {
            Some(&value) => value,
            None => 0,
        };
        let signed = matches!(
            signature.kinds[index],
            OperandKind::Jump | OperandKind::Immediate
        );
        let needed = if signed {
            if (-128..=127).contains(&value) {
                Width::Narrow
            } else if (-32768..=32767).contains(&value) {
                Width::Wide
            } else {
                Width::ExtraWide
            }
        } else if (0..=255).contains(&value) {
            Width::Narrow
        } else if (0..=65535).contains(&value) {
            Width::Wide
        } else {
            Width::ExtraWide
        };
        if needed.bytes() > width.bytes() {
            width = needed;
        }
        index += 1;
    }
    width
}

/// The digest of the instruction format itself.
///
/// It is derived from the encoding rather than written down beside it: every
/// opcode number, its operand kinds, the record sizes, and the format tag are
/// hashed, so any change to the format changes the digest and every image
/// compiled under the old one fails admission.
pub fn format_digest() -> Digest {
    let mut hasher = Hasher::new();
    hasher.update(&FORMAT_TAG);
    hasher.update(&[
        PREFIX_WIDE,
        PREFIX_EXTRA_WIDE,
        FUNCTION_RECORD_SIZE as u8,
        CONSTANT_RECORD_SIZE as u8,
        EXCEPTION_RECORD_SIZE as u8,
        HEADER_SIZE as u8,
    ]);
    let mut byte = 0u16;
    while byte <= u16::from(Opcode::MAX_BYTE) {
        let value = u8::try_from(byte).unwrap_or(0);
        if let Some(opcode) = Opcode::from_byte(value) {
            let signature = opcode.signature();
            hasher.update(&[value, signature.count]);
            let mut index = 0usize;
            while index < signature.count as usize {
                hasher.update(&[operand_kind_tag(signature.kinds[index])]);
                index += 1;
            }
        }
        byte += 1;
    }
    hasher.finish()
}

const fn operand_kind_tag(kind: OperandKind) -> u8 {
    match kind {
        OperandKind::Register => 1,
        OperandKind::Constant => 2,
        OperandKind::Immediate => 3,
        OperandKind::Count => 4,
        OperandKind::Jump => 5,
        OperandKind::Depth => 6,
    }
}

// The unit image.

/// Bytes in the unit header: the magic, the format digest, the feature digest,
/// and the section counts.
pub const HEADER_SIZE: usize = 112;
/// Bytes in one function record.
pub const FUNCTION_RECORD_SIZE: usize = 48;
/// Bytes in one constant record.
pub const CONSTANT_RECORD_SIZE: usize = 16;
/// Bytes in one exception region record.
pub const EXCEPTION_RECORD_SIZE: usize = 20;

/// What a constant holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ConstantKind {
    /// A Number value, held as the bits of its binary64 representation.
    Number = 1,
    /// A string, held as UTF-16 code units in the constant data.
    String = 2,
    /// A property key, held as UTF-16 code units in the constant data.
    Key = 3,
    /// A BigInt, held as its canonical decimal digits in the constant data.
    BigInt = 4,
    /// A regular expression, held as its flags in one code unit followed by the
    /// units of its pattern.
    RegExp = 5,
}

impl ConstantKind {
    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Number),
            2 => Some(Self::String),
            3 => Some(Self::Key),
            4 => Some(Self::BigInt),
            5 => Some(Self::RegExp),
            _ => None,
        }
    }
}

/// One constant table entry.
#[derive(Clone, Copy, Debug)]
pub struct Constant {
    pub kind: ConstantKind,
    /// Number: the low half of the binary64 bits. Otherwise a byte offset into
    /// the constant data.
    pub first: u32,
    /// Number: the high half of the binary64 bits. Otherwise a length in UTF-16
    /// code units for a string or key, and in bytes for a BigInt.
    pub second: u32,
}

impl Constant {
    pub fn number(value: f64) -> Self {
        let bits = value.to_bits();
        Self {
            kind: ConstantKind::Number,
            first: (bits & 0xFFFF_FFFF) as u32,
            second: (bits >> 32) as u32,
        }
    }

    pub fn value(&self) -> f64 {
        f64::from_bits(((self.second as u64) << 32) | self.first as u64)
    }
}

/// One protected region of a function's code.
#[derive(Clone, Copy, Debug)]
pub struct ExceptionRegion {
    /// First protected byte offset, relative to the function's code.
    pub start: u32,
    /// One past the last protected byte offset.
    pub end: u32,
    /// Where control resumes when the region throws.
    pub handler: u32,
    /// Register receiving the thrown value.
    pub register: u32,
    /// Contexts open where the region starts. Unwinding into the handler leaves
    /// every context the protected code entered, so the handler runs with the
    /// environment its own code was compiled against.
    pub context_depth: u32,
}

/// One function's declared shape.
#[derive(Clone, Copy, Debug, Default)]
pub struct Function {
    pub code_offset: u32,
    pub code_length: u32,
    pub register_count: u32,
    pub argument_count: u32,
    pub frame_extent: u32,
    pub exception_offset: u32,
    pub exception_count: u32,
    pub safe_point_offset: u32,
    pub safe_point_count: u32,
    pub context_depth: u32,
    /// Slots in the environment a call to this function creates. Its
    /// parameters and its `var` declarations live there, so a closure made
    /// inside it can reach them after it returns.
    pub context_slots: u32,
    pub flags: u32,
}

/// Flags a function record carries.
pub mod function_flag {
    /// The function was written as an arrow: it has no `this` of its own, so a
    /// call gives it a declarative environment rather than a function one and
    /// `this` reads the enclosing function's.
    pub const ARROW: u32 = 1 << 0;
    /// The function's body is strict code: a call with no receiver leaves
    /// `this` undefined rather than binding the global object.
    pub const STRICT: u32 = 1 << 1;
}

/// The section table of a unit image.
#[derive(Clone, Copy, Debug, Default)]
pub struct Header {
    pub function_count: u32,
    pub constant_count: u32,
    pub constant_data_length: u32,
    pub code_length: u32,
    pub exception_count: u32,
    pub safe_point_count: u32,
    pub entry_function: u32,
    /// What a unit is, and how it behaves: see `unit_flag`.
    pub flags: u32,
    /// Names this unit imports from other modules.
    pub import_count: u32,
    /// Names this unit makes available to other modules.
    pub export_count: u32,
}

/// Flags a unit carries.
pub mod unit_flag {
    /// The unit is a module: its top level has bindings of its own, its
    /// imports must be resolved before it runs, and its exports are what other
    /// modules may name.
    pub const MODULE: u32 = 1 << 0;
}

/// One name a module imports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImportRecord {
    /// The specifier, as a string constant.
    pub specifier: u32,
    /// The name in the exporting module, as a key constant, or `u32::MAX` for
    /// a namespace import, which names the module itself.
    pub name: u32,
    /// The slot this module's environment holds it in.
    pub slot: u32,
}

/// One name a module exports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExportRecord {
    /// The name other modules use, as a key constant.
    pub name: u32,
    /// The slot in this module's environment that holds it.
    pub slot: u32,
}

/// Bytes in one import record.
pub const IMPORT_RECORD_SIZE: usize = 12;
/// Bytes in one export record.
pub const EXPORT_RECORD_SIZE: usize = 8;

/// A validated view over a unit image.
///
/// Construction checks the magic, the format digest, and every section offset,
/// so an accessor cannot read outside the image.
#[derive(Clone, Copy)]
pub struct Unit<'a> {
    bytes: &'a [u8],
    header: Header,
    functions_at: usize,
    constants_at: usize,
    constant_data_at: usize,
    code_at: usize,
    exceptions_at: usize,
    safe_points_at: usize,
    imports_at: usize,
    exports_at: usize,
}

/// Why an image is not a usable unit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageError {
    Magic,
    FormatDigest,
    FeatureDigest,
    Truncated,
    Overflow,
}

impl Unit<'static> {
    /// A unit that holds nothing, for storage a caller fills in.
    pub const EMPTY: Self = Self {
        bytes: &[],
        header: Header {
            function_count: 0,
            constant_count: 0,
            constant_data_length: 0,
            code_length: 0,
            exception_count: 0,
            safe_point_count: 0,
            entry_function: 0,
            flags: 0,
            import_count: 0,
            export_count: 0,
        },
        functions_at: 0,
        constants_at: 0,
        constant_data_at: 0,
        code_at: 0,
        exceptions_at: 0,
        safe_points_at: 0,
        imports_at: 0,
        exports_at: 0,
    };
}

impl<'a> Unit<'a> {
    /// Validate an image against this build's format digest.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, ImageError> {
        if bytes.len() < HEADER_SIZE {
            return Err(ImageError::Truncated);
        }
        if bytes.get(..4) != Some(&UNIT_MAGIC[..]) {
            return Err(ImageError::Magic);
        }
        if bytes.get(4..36) != Some(&format_digest().0[..]) {
            return Err(ImageError::FormatDigest);
        }
        // The language the image was compiled against, not just the encoding it
        // was written in. An image from a build that admitted different
        // features is refused rather than run under these ones.
        if bytes.get(36..68) != Some(&crate::feature::digest().0[..]) {
            return Err(ImageError::FeatureDigest);
        }

        let header = Header {
            function_count: read_u32(bytes, 68)?,
            constant_count: read_u32(bytes, 72)?,
            constant_data_length: read_u32(bytes, 76)?,
            code_length: read_u32(bytes, 80)?,
            exception_count: read_u32(bytes, 84)?,
            safe_point_count: read_u32(bytes, 88)?,
            entry_function: read_u32(bytes, 92)?,
            flags: read_u32(bytes, 96)?,
            import_count: read_u32(bytes, 100)?,
            export_count: read_u32(bytes, 104)?,
        };

        let functions_at = HEADER_SIZE;
        let constants_at = advance(
            functions_at,
            header.function_count as usize,
            FUNCTION_RECORD_SIZE,
        )?;
        let constant_data_at = advance(
            constants_at,
            header.constant_count as usize,
            CONSTANT_RECORD_SIZE,
        )?;
        let code_at = advance(constant_data_at, header.constant_data_length as usize, 1)?;
        let exceptions_at = advance(code_at, header.code_length as usize, 1)?;
        let safe_points_at = advance(
            exceptions_at,
            header.exception_count as usize,
            EXCEPTION_RECORD_SIZE,
        )?;
        let imports_at = advance(safe_points_at, header.safe_point_count as usize, 4)?;
        let exports_at = advance(imports_at, header.import_count as usize, IMPORT_RECORD_SIZE)?;
        let end = advance(exports_at, header.export_count as usize, EXPORT_RECORD_SIZE)?;
        if end > bytes.len() {
            return Err(ImageError::Truncated);
        }
        if header.entry_function >= header.function_count {
            return Err(ImageError::Overflow);
        }

        Ok(Self {
            bytes,
            header,
            functions_at,
            constants_at,
            constant_data_at,
            code_at,
            exceptions_at,
            safe_points_at,
            imports_at,
            exports_at,
        })
    }

    pub const fn header(&self) -> &Header {
        &self.header
    }

    /// The logical digest of the whole image, which is its identity.
    pub fn logical_digest(&self) -> Digest {
        crate::digest::digest(self.bytes)
    }

    pub fn function(&self, index: u32) -> Option<Function> {
        if index >= self.header.function_count {
            return None;
        }
        let at = self.functions_at + index as usize * FUNCTION_RECORD_SIZE;
        Some(Function {
            code_offset: read_u32(self.bytes, at).ok()?,
            code_length: read_u32(self.bytes, at + 4).ok()?,
            register_count: read_u32(self.bytes, at + 8).ok()?,
            argument_count: read_u32(self.bytes, at + 12).ok()?,
            frame_extent: read_u32(self.bytes, at + 16).ok()?,
            exception_offset: read_u32(self.bytes, at + 20).ok()?,
            exception_count: read_u32(self.bytes, at + 24).ok()?,
            safe_point_offset: read_u32(self.bytes, at + 28).ok()?,
            safe_point_count: read_u32(self.bytes, at + 32).ok()?,
            context_depth: read_u32(self.bytes, at + 36).ok()?,
            context_slots: read_u32(self.bytes, at + 40).ok()?,
            flags: read_u32(self.bytes, at + 44).ok()?,
        })
    }

    /// One import record.
    pub fn import(&self, index: u32) -> Option<ImportRecord> {
        if index >= self.header.import_count {
            return None;
        }
        let at = self.imports_at + index as usize * IMPORT_RECORD_SIZE;
        Some(ImportRecord {
            specifier: read_u32(self.bytes, at).ok()?,
            name: read_u32(self.bytes, at + 4).ok()?,
            slot: read_u32(self.bytes, at + 8).ok()?,
        })
    }

    /// One export record.
    pub fn export(&self, index: u32) -> Option<ExportRecord> {
        if index >= self.header.export_count {
            return None;
        }
        let at = self.exports_at + index as usize * EXPORT_RECORD_SIZE;
        Some(ExportRecord {
            name: read_u32(self.bytes, at).ok()?,
            slot: read_u32(self.bytes, at + 4).ok()?,
        })
    }

    /// The slot an exported name is held in, if this unit exports it.
    pub fn export_slot(&self, name: &[u16]) -> Option<u32> {
        let mut index = 0u32;
        while index < self.header.export_count {
            let record = self.export(index)?;
            let constant = self.constant(record.name)?;
            let mut units = [0u16; 64];
            let length = self.constant_units(&constant, &mut units)?;
            if units.get(..length) == Some(name) {
                return Some(record.slot);
            }
            index += 1;
        }
        None
    }

    /// One import, read into caller-provided buffers: the specifier, the name
    /// it asks for, and the slot it fills. A name length of zero is a namespace
    /// import, which names the module itself.
    pub fn import_at(
        &self,
        index: u32,
        specifier: &mut [u16],
        name: &mut [u16],
    ) -> Option<(usize, usize, u32)> {
        let record = self.import(index)?;
        let constant = self.constant(record.specifier)?;
        let specifier_length = self.constant_units(&constant, specifier)?;
        let mut name_length = 0usize;
        if record.name != u32::MAX {
            let constant = self.constant(record.name)?;
            name_length = self.constant_units(&constant, name)?;
        }
        Some((specifier_length, name_length, record.slot))
    }

    /// Whether this unit is a module.
    pub fn is_module(&self) -> bool {
        self.header.flags & unit_flag::MODULE != 0
    }

    pub fn constant(&self, index: u32) -> Option<Constant> {
        if index >= self.header.constant_count {
            return None;
        }
        let at = self.constants_at + index as usize * CONSTANT_RECORD_SIZE;
        let kind = ConstantKind::from_byte(*self.bytes.get(at)?)?;
        Some(Constant {
            kind,
            first: read_u32(self.bytes, at + 4).ok()?,
            second: read_u32(self.bytes, at + 8).ok()?,
        })
    }

    /// The UTF-16 code units of a string, key, or pattern constant.
    pub fn constant_units(&self, constant: &Constant, out: &mut [u16]) -> Option<usize> {
        if !matches!(
            constant.kind,
            ConstantKind::String | ConstantKind::Key | ConstantKind::RegExp
        ) {
            return None;
        }
        let length = constant.second as usize;
        if out.len() < length {
            return None;
        }
        let at = self.constant_data_at + constant.first as usize;
        let bytes = self.bytes.get(at..at + length * 2)?;
        let mut index = 0usize;
        while index < length {
            out[index] = u16::from(bytes[index * 2]) | (u16::from(bytes[index * 2 + 1]) << 8);
            index += 1;
        }
        Some(length)
    }

    /// The raw bytes of a BigInt constant's digits.
    pub fn constant_bytes(&self, constant: &Constant) -> Option<&'a [u8]> {
        let at = self.constant_data_at + constant.first as usize;
        self.bytes.get(at..at + constant.second as usize)
    }

    /// A function's code.
    pub fn code(&self, function: &Function) -> Option<&'a [u8]> {
        let at = self.code_at + function.code_offset as usize;
        self.bytes.get(at..at + function.code_length as usize)
    }

    pub fn exception_region(&self, index: u32) -> Option<ExceptionRegion> {
        if index >= self.header.exception_count {
            return None;
        }
        let at = self.exceptions_at + index as usize * EXCEPTION_RECORD_SIZE;
        Some(ExceptionRegion {
            start: read_u32(self.bytes, at).ok()?,
            end: read_u32(self.bytes, at + 4).ok()?,
            handler: read_u32(self.bytes, at + 8).ok()?,
            register: read_u32(self.bytes, at + 12).ok()?,
            context_depth: read_u32(self.bytes, at + 16).ok()?,
        })
    }

    pub fn safe_point(&self, index: u32) -> Option<u32> {
        if index >= self.header.safe_point_count {
            return None;
        }
        read_u32(self.bytes, self.safe_points_at + index as usize * 4).ok()
    }

    pub const fn bytes(&self) -> &'a [u8] {
        self.bytes
    }
}

fn advance(at: usize, count: usize, size: usize) -> Result<usize, ImageError> {
    count
        .checked_mul(size)
        .and_then(|span| at.checked_add(span))
        .ok_or(ImageError::Overflow)
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32, ImageError> {
    let slice = bytes.get(at..at + 4).ok_or(ImageError::Truncated)?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}
