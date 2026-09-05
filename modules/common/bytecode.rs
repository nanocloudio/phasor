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
pub const FORMAT_TAG: [u8; 8] = *b"PHBC0002";

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
    /// Require the object in the operand register to be coercible, then turn
    /// the accumulator into a property key. This is the reference order: a
    /// read through nothing is a type error before the key's own `toString`
    /// runs, and the key it produces is coerced exactly once.
    ToPropertyKeyChecked = 0x39,

    // The tail of this range carries a binding operation and four property
    // operations: their own ranges are full, and an opcode's value says
    // nothing about what it is about.
    /// Store the accumulator into an existing global property, refusing —
    /// with a reference error — a name the global object does not have.
    /// This is what assignment means in strict code: it never creates.
    StaGlobalStrict = 0x3A,
    /// Define an accessor property: the closure in the accumulator becomes
    /// the getter or the setter for the named key on the object in the
    /// operand register, merging with an accessor already there.
    DefineNamedGetter = 0x3B,
    DefineNamedSetter = 0x3C,
    /// The keyed forms take the key from a register.
    DefineKeyedGetter = 0x3D,
    DefineKeyedSetter = 0x3E,
    /// Branch when the accumulator is not `undefined`: what a default value
    /// checks, which fires on `undefined` alone, never on `null`.
    JumpIfNotUndefined = 0x99,
    /// Build an array of the running frame's actual arguments from the
    /// operand index onward: a rest parameter's value.
    CreateRestArguments = 0x9A,
    /// Read a name through the environment chain by its text — a binding a
    /// direct eval created — falling back to the global object.
    LdaDynamic = 0x9B,
    /// Write the accumulator through the environment chain by name, creating
    /// a global property when nothing binds it.
    StaDynamic = 0x9C,
    /// Read a name as `typeof` does: an unresolvable name is `undefined`.
    TypeofDynamic = 0x9D,
    /// Declare a `var` in the nearest variable environment — a function or
    /// arrow environment, or the global object — unless it already binds the
    /// name.
    DeclareEvalVar = 0x9E,
    /// Delete a name: a binding a direct eval created is removed, a global
    /// property is deleted, and a static binding was already answered false.
    DeleteDynamic = 0x9F,
    /// Suspend the running async function until the accumulator's value
    /// settles, answering its promise to the caller.
    Await = 0xA0,
    /// Read a context slot — unless a nearer named binding, one a direct
    /// eval created, shadows it.
    LdaShadowable = 0xA1,
    /// Write a context slot — unless a nearer named binding shadows it.
    StaShadowable = 0xA2,
    /// Resolve an assignment target's environment before its value is made:
    /// the shadowing named binding's environment when one is nearer than the
    /// static slot, the slot's own environment otherwise.
    PrepareShadowable = 0xA3,
    /// Read through a prepared environment: the named binding when the
    /// record holds one, the slot otherwise.
    LdaPrepared = 0xA4,
    /// Write through a prepared environment.
    StaPrepared = 0xA5,
    /// A class with no written constructor gets the specification's default:
    /// base does nothing, derived forwards its arguments to `super`.
    CreateDefaultConstructor = 0xA6,
    /// Shape the accumulator's function into a class constructor over the
    /// prototype object in the register: only-via-new, home object, and the
    /// `prototype`/`constructor` pair.
    MakeClassConstructor = 0xA7,
    /// Define the accumulator as a named method: non-enumerable, with the
    /// register's object as its home.
    DefineMethod = 0xA8,
    /// Define the accumulator as a method under the computed key in the
    /// second register.
    DefineMethodKeyed = 0xA9,
    /// Define the accumulator as a class accessor: non-enumerable getter or
    /// setter, by the immediate kind.
    DefineClassAccessor = 0xAA,
    /// The keyed form of the class accessor definition.
    DefineClassAccessorKeyed = 0xAB,
    /// Read a property through the running method's home object's prototype.
    LdaSuperProperty = 0xAC,
    /// Call the parent constructor with this frame's `this`.
    CallSuper = 0xAD,
    /// `new.target`: the constructed callee, or undefined in a plain call.
    LdaNewTarget = 0xAE,
    /// The prototype object a heritage supplies: the register's value must
    /// be a constructor whose `prototype` is an object or null, or the
    /// heritage is the TypeError the specification makes it.
    GetHeritagePrototype = 0xAF,
    /// Suspend the running generator, handing the accumulator to whoever
    /// called `next`.
    Yield = 0xB0,
    /// Suspend a generator at its start, once its parameters are bound: the
    /// call's answer is the generator object.
    InitialYield = 0xB1,
    /// Push an object environment over the accumulator's object: the `with`
    /// statement's scope.
    PushObjectContext = 0xB2,
    /// Run the class fields the running constructor's function carries,
    /// defining each on `this` in order.
    InitFields = 0xB3,
    /// Close the iterator in the first register unless the done flag in the
    /// second says it finished on its own.
    IteratorClose = 0xB4,
    /// Close the iterator, swallowing anything the close itself throws: what
    /// an abrupt destructuring does before rethrowing its own reason.
    IteratorCloseQuiet = 0xB5,
    /// The accumulator must be an object — an iterator result — or the
    /// TypeError the protocol demands.
    RequireObject = 0xB6,
    /// `super()` returned: `this` leaves its dead zone.
    BindThis = 0xB7,
    /// `new` with a spread: the callee in the first register, the gathered
    /// arguments in the second.
    ConstructWithArray = 0xB8,
    /// `super(...)` with a spread: the gathered arguments in the register.
    CallSuperWithArray = 0xB9,
    /// `import.source()`: a promise rejected with a SyntaxError, because a
    /// source-phase import has no host record to answer it.
    ImportReject = 0xBA,
    /// The iterator `for await` walks: the async protocol's, or the sync
    /// protocol's whose results the loop awaits.
    GetAsyncIterator = 0xBB,
    /// Suspend the running generator at a `yield*` step: a later `return`
    /// or `throw` resumes here with its kind readable, so the delegation
    /// forwards it to the inner iterator instead of acting itself.
    YieldStar = 0xBC,
    /// How the suspended generator was resumed: 0 for `next`, 1 for
    /// `throw`, 2 for `return`, as a number in the accumulator.
    ResumeKind = 0xBD,
    /// Declare a global function binding by name: the property must be
    /// definable — absent on an extensible global, configurable, or a
    /// writable enumerable data property — or the TypeError the
    /// specification makes it.
    DeclareGlobalFunction = 0xBE,
    /// Define one instance field on `this`: the register holds the field's
    /// evaluated name, the accumulator its value, and a private name makes a
    /// hidden property.
    DefineField = 0xBF,
    /// Give the function in the accumulator the register's object as its
    /// home, which is what a field initialiser's `super` resolves through.
    SetHome = 0xC0,
    /// Stamp `this` with the running constructor's prototype: the private
    /// brand, which private member access checks against.
    Brand = 0xC1,
    /// `#x in o`: whether the accumulator's object carries the site's
    /// private member named by the constant.
    TestPrivateIn = 0xC2,
    /// Read the accumulator's key through the running method's home
    /// object's prototype: `super[key]`.
    LdaSuperKeyed = 0xC3,
    /// Turn the private name in the accumulator into the storage key of the
    /// register's class object: same-named privates of other classes stay
    /// apart, and no reflective surface sees the result.
    PrivateKey = 0xC4,
    /// The running method's super base — its home object's prototype — into
    /// the accumulator, after `this` is bound.
    GetSuperBase = 0xC5,
    /// Throw a ReferenceError: what deleting a super reference does.
    ThrowReference = 0xC6,
    /// The template object for one tagged-template site: the first
    /// evaluation's array is kept, and every later one answers it.
    CacheTemplate = 0xC7,
    /// Throw a TypeError: what strict code assigning a named function
    /// expression's own name does.
    ThrowSelfAssignment = 0xC8,
    /// Suspend at a sync `yield*` step whose accumulator already holds the
    /// inner iterator's result object: the resumer receives it untouched.
    YieldDelegate = 0xC9,
    /// Unary plus: the accumulator to a Number, refusing a BigInt.
    ToNumber = 0xCA,
    /// Whether the accumulator names a property the register's object still
    /// has — or true for a primitive, whose keys cannot be deleted.
    ForInHas = 0xCB,
    /// The object a `with` environment supplies the named binding from, as
    /// a call's receiver — or undefined when no such environment binds it.
    LdaWithReceiver = 0xCC,
    /// Write the accumulator to `super.name`: through the base in the
    /// register, onto this frame's `this`.
    StaSuperNamed = 0xCD,
    /// Write the accumulator to `super[key]`: the base and the key are the
    /// registers, the receiver is this frame's `this`.
    StaSuperKeyed = 0xCE,
    /// A fresh stack of resources to dispose, in the accumulator.
    CreateDisposeStack = 0xCF,
    /// Push the accumulator onto the stack in the register: nothing for
    /// null or undefined, a TypeError for anything without `@@dispose`.
    AddDisposable = 0xD0,
    /// Dispose the stack in the register, last resource first; a disposer's
    /// throw suppresses whatever was thrown before it.
    DisposeStack = 0xD1,
    /// Dispose the stack in the first register while the exception in the
    /// second propagates, then throw what remains.
    DisposeStackThrow = 0xD2,
    /// Push the accumulator onto the stack in the register for awaited
    /// disposal: by `@@asyncDispose`, else by `@@dispose` with an await of
    /// undefined after it; null and undefined join as an await alone.
    AddDisposableAsync = 0xD3,
    /// Dispose the stack in the first register up to the next result that
    /// must be awaited, leaving that result in the accumulator — or the
    /// stack itself once nothing remains. A disposer's throw joins the
    /// second register, which holds the stack while nothing is pending.
    DisposeStackNext = 0xD4,
    /// Fold the exception in the second register into the first, which holds
    /// a pending exception — or a dispose stack while none is pending — so
    /// the new one suppresses the earlier.
    SuppressError = 0xD5,
    /// A script's top-level lexical name may be declared: no global lexical
    /// or script `var` already has it, and no non-configurable global
    /// property does — else the SyntaxError declaration instantiation throws.
    CheckGlobalLexical = 0xD6,
    /// A script's `var` or function name may be declared: no global lexical
    /// has it.
    CheckGlobalVar = 0xD7,
    /// Bind a script's top-level lexical name in the global lexical
    /// environment, uninitialised; the immediate says whether it is a const.
    DeclareGlobalLexical = 0xD8,
    /// Initialise the global lexical binding of the name with the accumulator.
    InitGlobalLexical = 0xD9,
    /// Whether the name resolves at all — a global lexical, even in its dead
    /// zone, or a property of the global object — into the accumulator: a
    /// strict assignment resolves its reference before the value is made.
    HasGlobal = 0xDA,
    /// Store the accumulator to the global name whose resolution the
    /// register holds: unresolvable is the strict ReferenceError.
    StaGlobalResolved = 0xDB,
    /// `CopyDataProperties` with an exclusion list: the accumulator's own
    /// enumerable properties cross to the first register's object, except
    /// the keys the second register's object holds — an object rest pattern
    /// never looks at the properties it named, not even to skip them.
    CopyDataPropertiesExcluding = 0xDC,
    /// Load a free name for a call under `with`: one resolution supplies
    /// both the callee, into the accumulator, and the receiver the call
    /// takes — the `with` object that bound the name, or undefined — into
    /// the register.
    LdaDynamicCallee = 0xDD,
    /// `NameClosure` from a computed key: the register holds the property
    /// key the closure in the accumulator is defined under.
    NameClosureKeyed = 0xDE,
    /// Define an auto-accessor property: a getter and setter of the key in
    /// the second register, on the first register's object, reading and
    /// writing a hidden field of the receiver.
    DefineAutoAccessor = 0xE0,
    /// Import the module the accumulator's string names, answering a promise
    /// of its namespace — the deferred one when the operand says so. The
    /// module must be one of the closure's; anything else rejects.
    DynamicImport = 0xE1,
    /// The end of a module's instantiation: its bindings exist and its
    /// function declarations hold their closures. An instantiation pass
    /// stops here; an evaluation walks straight through.
    InstantiationEnd = 0xE2,
    /// Give the closure in the accumulator its `name`, from a key constant:
    /// what the specification calls named evaluation. A closure that already
    /// carries a name keeps it.
    NameClosure = 0x3F,

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
    /// `Call` in tail position of a strict function: the running frame is
    /// given up before the callee's is made, so a chain of such calls
    /// holds one frame however long it runs. A callee that is not
    /// bytecode, or a frame that must outlive the call, calls as `Call`
    /// does and the `Return` that follows delivers the value.
    TailCall = 0xDF,
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
            0x39 => Self::ToPropertyKeyChecked,
            0x3A => Self::StaGlobalStrict,
            0x3B => Self::DefineNamedGetter,
            0x3C => Self::DefineNamedSetter,
            0x3D => Self::DefineKeyedGetter,
            0x3E => Self::DefineKeyedSetter,
            0x3F => Self::NameClosure,
            0x99 => Self::JumpIfNotUndefined,
            0x9A => Self::CreateRestArguments,
            0x9B => Self::LdaDynamic,
            0x9C => Self::StaDynamic,
            0x9D => Self::TypeofDynamic,
            0x9E => Self::DeclareEvalVar,
            0x9F => Self::DeleteDynamic,
            0xA0 => Self::Await,
            0xA1 => Self::LdaShadowable,
            0xA2 => Self::StaShadowable,
            0xA3 => Self::PrepareShadowable,
            0xA4 => Self::LdaPrepared,
            0xA5 => Self::StaPrepared,
            0xA6 => Self::CreateDefaultConstructor,
            0xA7 => Self::MakeClassConstructor,
            0xA8 => Self::DefineMethod,
            0xA9 => Self::DefineMethodKeyed,
            0xAA => Self::DefineClassAccessor,
            0xAB => Self::DefineClassAccessorKeyed,
            0xAC => Self::LdaSuperProperty,
            0xAD => Self::CallSuper,
            0xAE => Self::LdaNewTarget,
            0xAF => Self::GetHeritagePrototype,
            0xB0 => Self::Yield,
            0xB1 => Self::InitialYield,
            0xB2 => Self::PushObjectContext,
            0xB3 => Self::InitFields,
            0xB4 => Self::IteratorClose,
            0xB5 => Self::IteratorCloseQuiet,
            0xB6 => Self::RequireObject,
            0xB7 => Self::BindThis,
            0xB8 => Self::ConstructWithArray,
            0xB9 => Self::CallSuperWithArray,
            0xBA => Self::ImportReject,
            0xBB => Self::GetAsyncIterator,
            0xBC => Self::YieldStar,
            0xBD => Self::ResumeKind,
            0xBE => Self::DeclareGlobalFunction,
            0xBF => Self::DefineField,
            0xC0 => Self::SetHome,
            0xC1 => Self::Brand,
            0xC2 => Self::TestPrivateIn,
            0xC3 => Self::LdaSuperKeyed,
            0xC4 => Self::PrivateKey,
            0xC5 => Self::GetSuperBase,
            0xC6 => Self::ThrowReference,
            0xC7 => Self::CacheTemplate,
            0xC8 => Self::ThrowSelfAssignment,
            0xC9 => Self::YieldDelegate,
            0xCA => Self::ToNumber,
            0xCB => Self::ForInHas,
            0xCC => Self::LdaWithReceiver,
            0xCD => Self::StaSuperNamed,
            0xCE => Self::StaSuperKeyed,
            0xCF => Self::CreateDisposeStack,
            0xD0 => Self::AddDisposable,
            0xD1 => Self::DisposeStack,
            0xD2 => Self::DisposeStackThrow,
            0xD3 => Self::AddDisposableAsync,
            0xD4 => Self::DisposeStackNext,
            0xD5 => Self::SuppressError,
            0xD6 => Self::CheckGlobalLexical,
            0xD7 => Self::CheckGlobalVar,
            0xD8 => Self::DeclareGlobalLexical,
            0xD9 => Self::InitGlobalLexical,
            0xDA => Self::HasGlobal,
            0xDB => Self::StaGlobalResolved,
            0xDC => Self::CopyDataPropertiesExcluding,
            0xDD => Self::LdaDynamicCallee,
            0xDE => Self::NameClosureKeyed,
            0xE0 => Self::DefineAutoAccessor,
            0xE1 => Self::DynamicImport,
            0xE2 => Self::InstantiationEnd,
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
            0xDF => Self::TailCall,
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
            | Self::Return
            | Self::Throw
            | Self::Await
            | Self::Yield
            | Self::InitialYield
            | Self::PushObjectContext
            | Self::InitFields
            | Self::RequireObject
            | Self::BindThis
            | Self::ImportReject
            | Self::GetAsyncIterator
            | Self::YieldStar
            | Self::ResumeKind
            | Self::Brand
            | Self::GetSuperBase
            | Self::ThrowReference
            | Self::ThrowSelfAssignment
            | Self::YieldDelegate
            | Self::ToNumber
            | Self::InstantiationEnd => Signature::new(0, [NONE, NONE, NONE]),
            Self::ForInHas => Signature::new(1, [Register, NONE, NONE]),

            Self::LdaSmi | Self::CacheTemplate => Signature::new(1, [Immediate, NONE, NONE]),
            Self::DynamicImport => Signature::new(2, [Immediate, Register, NONE]),
            Self::LdaConstant
            | Self::LdaGlobal
            | Self::LdaGlobalOrUndefined
            | Self::StaGlobal
            | Self::StaGlobalStrict
            | Self::DeclareGlobal
            | Self::NameClosure
            | Self::DeleteNamedProperty
            | Self::LdaDynamic
            | Self::LdaWithReceiver
            | Self::StaDynamic
            | Self::TypeofDynamic
            | Self::DeclareEvalVar
            | Self::DeleteDynamic
            | Self::TestPrivateIn => Signature::new(1, [Constant, NONE, NONE]),
            Self::DeclareGlobalFunction | Self::DeclareGlobalLexical => {
                Signature::new(2, [Constant, Immediate, NONE])
            }
            Self::CheckGlobalLexical
            | Self::CheckGlobalVar
            | Self::InitGlobalLexical
            | Self::HasGlobal => Signature::new(1, [Constant, NONE, NONE]),
            Self::StaGlobalResolved | Self::LdaDynamicCallee => {
                Signature::new(2, [Constant, Register, NONE])
            }
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
            | Self::NameClosureKeyed
            | Self::DeleteKeyedProperty => Signature::new(1, [Register, NONE, NONE]),
            Self::PushContext => Signature::new(1, [Count, NONE, NONE]),
            Self::IteratorNext
            | Self::IteratorClose
            | Self::IteratorCloseQuiet
            | Self::ConstructWithArray => Signature::new(2, [Register, Register, NONE]),
            Self::CallSuperWithArray => Signature::new(1, [Register, NONE, NONE]),
            Self::DefineField => Signature::new(1, [Register, NONE, NONE]),
            Self::SetHome => Signature::new(1, [Register, NONE, NONE]),
            Self::PrivateKey => Signature::new(2, [Register, Immediate, NONE]),
            Self::LdaSuperKeyed => Signature::new(1, [Register, NONE, NONE]),
            Self::Jump
            | Self::JumpIfTrue
            | Self::JumpIfFalse
            | Self::JumpIfToBooleanTrue
            | Self::JumpIfToBooleanFalse
            | Self::JumpIfNullish
            | Self::JumpIfNotNullish
            | Self::JumpIfNotUndefined => Signature::new(1, [Jump, NONE, NONE]),

            Self::Mov => Signature::new(2, [Register, Register, NONE]),
            Self::StaSuperNamed => Signature::new(2, [Register, Constant, NONE]),
            Self::StaSuperKeyed | Self::DisposeStackThrow | Self::DisposeStackNext => {
                Signature::new(2, [Register, Register, NONE])
            }
            Self::SuppressError => Signature::new(3, [Register, Register, Register]),
            Self::CreateDisposeStack => Signature::new(0, [NONE, NONE, NONE]),
            Self::AddDisposable | Self::AddDisposableAsync | Self::DisposeStack => {
                Signature::new(1, [Register, NONE, NONE])
            }
            Self::GetNamedProperty
            | Self::SetNamedProperty
            | Self::DefineNamedProperty
            | Self::DefineNamedGetter
            | Self::DefineNamedSetter => Signature::new(2, [Register, Constant, NONE]),
            Self::SetKeyedProperty
            | Self::DefineKeyedProperty
            | Self::DefineKeyedGetter
            | Self::DefineKeyedSetter
            | Self::CopyDataPropertiesExcluding
            | Self::DefineAutoAccessor => Signature::new(2, [Register, Register, NONE]),
            Self::LdaContextSlot | Self::StaContextSlot | Self::InitContextSlot => {
                Signature::new(2, [Count, Depth, NONE])
            }
            Self::LdaShadowable | Self::StaShadowable => {
                Signature::new(3, [Constant, Count, Depth])
            }
            // The third operand is a depth or the free-name sentinel, so it
            // is not held to the declared context depth.
            Self::PrepareShadowable => Signature::new(3, [Constant, Count, Count]),
            Self::LdaPrepared | Self::StaPrepared => Signature::new(3, [Register, Constant, Count]),
            Self::CreateDefaultConstructor => Signature::new(1, [Immediate, NONE, NONE]),
            Self::MakeClassConstructor => Signature::new(2, [Register, Immediate, NONE]),
            Self::DefineMethod => Signature::new(2, [Register, Constant, NONE]),
            Self::DefineMethodKeyed => Signature::new(2, [Register, Register, NONE]),
            Self::DefineClassAccessor => Signature::new(3, [Register, Constant, Immediate]),
            Self::DefineClassAccessorKeyed => Signature::new(3, [Register, Register, Immediate]),
            Self::LdaSuperProperty => Signature::new(1, [Constant, NONE, NONE]),
            Self::CallSuper => Signature::new(2, [Register, Count, NONE]),
            Self::LdaNewTarget => Signature::new(0, [NONE, NONE, NONE]),
            Self::GetHeritagePrototype => Signature::new(1, [Register, NONE, NONE]),
            Self::CreateRestArguments | Self::CreateArguments => {
                Signature::new(1, [Count, NONE, NONE])
            }
            Self::SetPrototype | Self::ToPropertyKeyChecked => {
                Signature::new(1, [Register, NONE, NONE])
            }
            Self::CreateClosure => Signature::new(1, [Immediate, NONE, NONE]),
            Self::CreateRegExp => Signature::new(1, [Constant, NONE, NONE]),
            Self::LdaImport => Signature::new(1, [Immediate, NONE, NONE]),

            Self::Call | Self::Construct => Signature::new(3, [Register, Register, Count]),
            Self::CallWithArray => Signature::new(3, [Register, Register, Register]),
            Self::CallProperty | Self::TailCall => Signature::new(3, [Register, Register, Count]),
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
                | Self::JumpIfNotUndefined
        )
    }

    /// The highest assigned opcode byte, which bounds the format descriptor.
    pub const MAX_BYTE: u8 = 0xE0;
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
    /// The function's code may direct-eval: its environment carries spare
    /// capacity for the `var` bindings sloppy eval code declares at run time.
    pub const DYNAMIC: u32 = 1 << 2;
    /// The function is async: a call answers a promise, and `await` suspends
    /// its frame until the awaited value settles.
    pub const ASYNC: u32 = 1 << 3;
    /// The function is a generator: a call answers a generator object, and
    /// `yield` suspends its frame until the next `next`.
    pub const GENERATOR: u32 = 1 << 4;
    /// A derived class constructor: `this` stays in its dead zone until
    /// `super()` binds it.
    pub const DERIVED_CONSTRUCTOR: u32 = 1 << 5;
    /// A method, getter, or setter: callable, never a constructor.
    pub const METHOD: u32 = 1 << 6;
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
    /// Bytes of eval-site records: for each direct `eval` call, the bindings
    /// visible at that point, so a host compiling the eval source can resolve
    /// them into the caller's environments.
    pub eval_site_length: u32,
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

/// An export whose slot carries this mark names the exporting module's own
/// import of that index instead: `export { imported }` is an indirection
/// the linker follows, not a slot of this module's environment.
pub const EXPORT_IMPORT_MARK: u32 = 1 << 31;

/// An import record whose name is this asks for the module's namespace with
/// its evaluation deferred: `import defer * as name`.
pub const DEFER_IMPORT_NAME: u32 = u32::MAX - 1;

/// A resolved import row holding this could not be linked — a name that
/// resolves nowhere or ambiguously. Reading it is the SyntaxError linking
/// would have raised.
pub const POISON_IMPORT: u32 = u32::MAX - 2;

/// An import record whose name is this asks for a source-phase record no
/// host here provides; its row refuses with the host's TypeError.
pub const SOURCE_IMPORT_NAME: u32 = u32::MAX - 3;

/// A resolved row the host refused: a source phase it does not serve.
pub const HOST_POISON_IMPORT: u32 = u32::MAX - 3;

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
    eval_sites_at: usize,
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
            eval_site_length: 0,
        },
        functions_at: 0,
        constants_at: 0,
        constant_data_at: 0,
        code_at: 0,
        exceptions_at: 0,
        safe_points_at: 0,
        imports_at: 0,
        exports_at: 0,
        eval_sites_at: 0,
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
            eval_site_length: read_u32(bytes, 108)?,
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
        let eval_sites_at = advance(exports_at, header.export_count as usize, EXPORT_RECORD_SIZE)?;
        let end = advance(eval_sites_at, header.eval_site_length as usize, 1)?;
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
            eval_sites_at,
        })
    }

    pub const fn header(&self) -> &Header {
        &self.header
    }

    /// The eval-site records, as the blob the compiler wrote. The machine
    /// never reads these; they are for the host that compiles an eval source
    /// against the scope the call site could see.
    pub fn eval_sites(&self) -> &'a [u8] {
        self.bytes
            .get(self.eval_sites_at..self.eval_sites_at + self.header.eval_site_length as usize)
            .unwrap_or(&[])
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
            if record.name == u32::MAX {
                // `export * from`: no name of its own; a resolver follows it.
                index += 1;
                continue;
            }
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
        if record.name != u32::MAX
            && record.name != DEFER_IMPORT_NAME
            && record.name != SOURCE_IMPORT_NAME
        {
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

    /// A string or key constant's code units as the image lays them out:
    /// little-endian, two bytes each.
    pub fn constant_unit_bytes(&self, constant: &Constant) -> Option<&'a [u8]> {
        if !matches!(constant.kind, ConstantKind::String | ConstantKind::Key) {
            return None;
        }
        let at = self.constant_data_at + constant.first as usize;
        self.bytes.get(at..at + constant.second as usize * 2)
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
