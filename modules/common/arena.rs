//! The immutable syntax arena.
//!
//! Nodes are fixed-width records in caller-provided storage, addressed by
//! index. The arena is append-only and a node is pushed only once all of its
//! children exist, so nothing is ever back-patched and a committed node never
//! changes. Child sequences of unknown length are copied in from the parser's
//! scratch stack, which keeps them contiguous even when list construction
//! nests.

/// What a node is. The payload words are interpreted per kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum NodeKind {
    /// An identifier reference. `first`/`second` are the name span.
    Identifier,
    /// A private name, `#x`. `first`/`second` are the name span without `#`.
    PrivateName,
    This,
    Null,
    True,
    False,
    /// A numeric literal. `first` indexes the number arena.
    Number,
    /// A BigInt literal. `first`/`second` are the digit span, `third` the radix.
    BigInt,
    /// A string literal. `first`/`second` are the contents span, `third` the
    /// cooked length in UTF-16 code units.
    String,
    /// A template literal. `first`/`second` are a list of alternating elements
    /// and substitutions, starting and ending with an element.
    Template,
    /// One template element. `first`/`second` are the raw span, `third` the
    /// cooked length, and the `COOKED_INVALID` flag records an escape that has
    /// no cooked value.
    TemplateElement,
    /// `import(specifier)`: first the specifier expression.
    ImportCall,
    /// A tagged template. `first` is the tag, `second` the template.
    TaggedTemplate,
    /// A regular-expression literal. `first`/`second` are the pattern span and
    /// `third` the flag bits.
    RegExp,
    /// An array literal. `first`/`second` are its element list.
    Array,
    /// An elided array element.
    Elision,
    /// `...expression`. `first` is the argument.
    Spread,
    /// An object literal. `first`/`second` are its property list.
    Object,
    /// `key: value`. `first` is the key, `second` the value.
    Property,
    /// `key` used as both name and value. `first` is the identifier.
    ShorthandProperty,
    /// `[expression]` as a property key. `first` is the key expression.
    ComputedKey,
    /// A property name written as an identifier, string, or number key.
    /// `first`/`second` are its span and `third` is its `property_key` kind,
    /// which says whether the span needs cooking or numeric conversion.
    PropertyName,
    /// `[a, b = 1, ...r]` as a binding target. `first`/`second` are its
    /// element list: binding elements, elisions, and at most one rest, last.
    ArrayPattern,
    /// `{a, b: c = 1, ...r}` as a binding target. `first`/`second` are its
    /// property list: pattern properties and at most one rest, last.
    ObjectPattern,
    /// One target with an optional default. `first` is the target — a name
    /// or a nested pattern — and `second` the default expression, or `NONE`.
    BindingElement,
    /// `key: target` in an object pattern. `first` is the key, `second` the
    /// binding element it binds.
    PatternProperty,
    /// `...target` in a pattern. `first` is the target.
    RestElement,
    /// A class: first the name or `NONE`, second and third the member list,
    /// whose first entry is the heritage expression or `NONE`.
    Class,
    /// A class carrying decorators: first and second the decorator
    /// expression list, third the class node. The decorators evaluate in
    /// source order before the class does; what they answer is not applied,
    /// which the proposal reads as keeping the value decorated.
    Decorated,
    /// One class member: first the key, second the function, third the kind.
    ClassMember,
    /// `super(...)`: first and second the argument list.
    SuperCall,
    /// `super.name`: first and second the name's span.
    SuperMember,
    /// `super[expression]`. `first` is the key expression.
    SuperIndex,
    /// `new.target`.
    NewTarget,
    /// `with (object) statement`: first the object, second the body.
    With,
    /// `object.property`. `first` is the object, `second` the property name
    /// node, and the `OPTIONAL` flag marks `?.`.
    Member,
    /// `object[expression]`. `first` is the object, `second` the index, and the
    /// `OPTIONAL` flag marks `?.[`.
    Index,
    /// `callee(arguments)`. `first` is the callee, `second`/`third` the
    /// argument list, and the `OPTIONAL` flag marks `?.(`.
    Call,
    /// `new callee(arguments)`. `first` is the callee, `second`/`third` the
    /// argument list.
    New,
    /// A prefix operator. `first` is the operand, `third` the operator.
    Unary,
    /// `++`/`--`. `first` is the operand, `third` the operator, and the
    /// `PREFIX` flag distinguishes prefix from postfix.
    Update,
    /// A binary operator. `first` is the left operand, `second` the right, and
    /// `third` the operator.
    Binary,
    /// `&&`, `||`, or `??`. Fields match `Binary`.
    Logical,
    /// `test ? consequent : alternate`. `first` is the test and `second` a
    /// two-entry list holding the consequent and the alternate.
    Conditional,
    /// An assignment. `first` is the target, `second` the value, `third` the
    /// operator.
    Assign,
    /// A comma expression. `first`/`second` are its operand list.
    Sequence,
    /// A function. `first` is the name, or `NONE` for an anonymous one;
    /// `second`/`third` are a list whose first entry is the body and whose
    /// remaining entries are the parameters. The `ARROW` flag marks an arrow
    /// function, and `CONCISE_BODY` marks a body that is one expression.
    Function,
    /// A parameter. `first` is its name.
    Parameter,

    // Statements and declarations. A script is one of these.
    /// The unit's top level. `first`/`second` are its statement list.
    Script,
    /// `{ ... }`. `first`/`second` are its statement list.
    Block,
    /// A `var`, `let`, or `const` declaration. `first`/`second` are its
    /// declarator list and `third` is a `declaration` kind.
    Declaration,
    /// One declarator. `first` is the name and `second` the initialiser, or
    /// `NONE` where there is none.
    Declarator,
    /// `;`.
    Empty,
    /// An expression used as a statement. `first` is the expression.
    ExpressionStatement,
    /// `if`. `first` is the test, `second` the consequent, `third` the
    /// alternate or `NONE`.
    If,
    /// `do body while (test)`. `first` is the body, `second` the test.
    DoWhile,
    /// `while (test) body`. `first` is the test, `second` the body.
    While,
    /// `for (init; test; update) body`. `first` is the initialiser or `NONE`,
    /// and `second`/`third` are a three-entry list holding the test, the
    /// update, and the body, each of which may be `NONE`.
    For,
    /// `for (left in right) body` and `for (left of right) body`. `first` is
    /// the left side, a declaration or an assignment target; `second` the
    /// right side; `third` the body. The `OF` flag distinguishes the two.
    ForInOf,
    /// `continue label;`. `first` is the label or `NONE`.
    Continue,
    /// `break label;`. `first` is the label or `NONE`.
    Break,
    /// `return value;`. `first` is the value or `NONE`.
    Return,
    /// `throw value;`. `first` is the value.
    Throw,
    /// `try`. `first` is the block, `second` the catch clause or `NONE`,
    /// `third` the finally block or `NONE`.
    Try,
    /// `catch (parameter) block`. `first` is the parameter or `NONE`, `second`
    /// the block.
    Catch,
    /// `switch (discriminant) { cases }`. `first` is the discriminant and
    /// `second`/`third` the case list.
    Switch,
    /// One `case` or `default`. `first` is the test or `NONE`, and
    /// `second`/`third` are its statement list.
    SwitchCase,
    /// `label: statement`. `first` is the label name, `second` the statement.
    Labelled,
    /// `debugger;`.
    Debugger,

    // Modules.
    /// `import ... from 'specifier'`. `first`/`second` are the clause list and
    /// `third` the specifier, a `String` node. A list with no entries is an
    /// import for its side effects alone.
    Import,
    /// One imported name. `first` is the local name, `second` the name in the
    /// exporting module, or `NONE` for a default import. The `NAMESPACE` flag
    /// marks `* as name`.
    ImportClause,
    /// `export ...`. `first` is the declaration, or `NONE` where the export is
    /// a list; `second`/`third` are the clause list.
    Export,
    /// One exported name. `first` is the local name and `second` the name other
    /// modules use.
    ExportClause,
}

/// The absence of a child, where a node may have one and does not.
///
/// Zero is a real node index, so absence needs its own value rather than a
/// falsy one.
pub const NONE: u32 = u32::MAX;

/// How a variable declaration binds.
pub mod declaration {
    pub const VAR: u32 = 0;
    pub const LET: u32 = 1;
    pub const CONST: u32 = 2;
    /// `using x = resource`: a `const` whose value is disposed when the
    /// block that declared it is left.
    pub const USING: u32 = 3;
    /// `await using x = resource`: a `using` whose disposal is awaited,
    /// through `@@asyncDispose` where the resource has one.
    pub const AWAIT_USING: u32 = 4;
}

/// How a property name was written, which decides how its span becomes a key.
pub mod property_key {
    pub const IDENTIFIER: u32 = 0;
    pub const STRING: u32 = 1;
    pub const NUMBER: u32 = 2;
}

/// What a `ClassMember` node's third payload word says the member is: one of
/// the kinds below, with the STATIC bit set for members of the constructor.
pub mod class_member {
    pub const METHOD: u32 = 0;
    pub const GETTER: u32 = 1;
    pub const SETTER: u32 = 2;
    pub const CONSTRUCTOR: u32 = 3;
    pub const FIELD: u32 = 4;
    /// `static { ... }`: a block run once, with `this` the constructor.
    pub const STATIC_BLOCK: u32 = 5;
    /// `accessor name`: a field behind a getter and setter of its name.
    pub const ACCESSOR_FIELD: u32 = 6;
    /// A decorator on the member that follows: `second` is its expression.
    pub const DECORATOR: u32 = 7;
    pub const STATIC: u32 = 1 << 3;
}

/// What a `Parameter` node's third payload word says the parameter is.
pub mod parameter_kind {
    pub const PLAIN: u32 = 0;
    pub const REST: u32 = 1;
}

/// What a `Property` node's third payload word says the property is.
pub mod property_kind {
    pub const DATA: u32 = 0;
    pub const GETTER: u32 = 1;
    pub const SETTER: u32 = 2;
    /// A shorthand method: a data property whose function was written as a
    /// MethodDefinition, so it carries a home object and admits `super`.
    pub const METHOD: u32 = 3;
}

/// Flag bits carried by a node.
pub mod flag {
    /// The construct was written with `?.`.
    pub const OPTIONAL: u8 = 1 << 0;
    /// An update expression is prefix rather than postfix.
    pub const PREFIX: u8 = 1 << 1;
    /// The expression was parenthesised in the source.
    pub const PARENTHESISED: u8 = 1 << 2;
    /// A template element has no cooked value.
    pub const COOKED_INVALID: u8 = 1 << 3;
    /// A call is the start of an optional chain rather than a link in one.
    pub const CHAIN_ROOT: u8 = 1 << 4;
    /// A Number or String literal strict code refuses: a legacy octal
    /// integer, a non-octal decimal integer, or a legacy escape.
    pub const LEGACY_OCTAL: u8 = CHAIN_ROOT;
    /// The function was written as an arrow.
    pub const ARROW: u8 = 1 << 5;
    /// An arrow's body is one expression rather than a block.
    pub const CONCISE_BODY: u8 = 1 << 6;
    /// A `for` loop iterates with `of` rather than `in`, and an import clause
    /// names the module itself rather than one of its exports.
    pub const OF: u8 = 1 << 7;
    /// The same bit reads as `NAMESPACE` on an import clause, where no `for`
    /// loop can be.
    pub const NAMESPACE: u8 = OF;
    /// An import clause reads as DEFER: `import defer * as name` waits to
    /// evaluate the module until its namespace is meaningfully used.
    pub const DEFER: u8 = CHAIN_ROOT;
    /// An import clause reads as SOURCE: `import source name` asks for a
    /// phase no host here serves.
    pub const SOURCE: u8 = COOKED_INVALID;
    /// The same bit reads as `ASYNC` on a function, where no `?.` can be.
    pub const ASYNC: u8 = OPTIONAL;
    /// The same bit reads as `DECLARATION` on a function, where no template
    /// element can be: a declared function's name is the enclosing scope's
    /// mutable binding, never a self-name of its own.
    pub const DECLARATION: u8 = COOKED_INVALID;
    /// The same bit reads as `GENERATOR` on a function, where no update
    /// expression can be.
    pub const GENERATOR: u8 = PREFIX;
    /// The same bit reads as `FOR_AWAIT` on a `for` head.
    pub const FOR_AWAIT: u8 = PREFIX;
}

/// Unary and update operators.
pub mod unary_operator {
    pub const DELETE: u32 = 0;
    pub const VOID: u32 = 1;
    pub const TYPEOF: u32 = 2;
    pub const PLUS: u32 = 3;
    pub const MINUS: u32 = 4;
    pub const BITWISE_NOT: u32 = 5;
    pub const LOGICAL_NOT: u32 = 6;
    pub const INCREMENT: u32 = 7;
    pub const DECREMENT: u32 = 8;
    pub const AWAIT: u32 = 9;
    pub const YIELD: u32 = 10;
    pub const YIELD_DELEGATE: u32 = 11;
}

/// Binary, logical, and assignment operators.
pub mod binary_operator {
    pub const ADD: u32 = 0;
    pub const SUBTRACT: u32 = 1;
    pub const MULTIPLY: u32 = 2;
    pub const DIVIDE: u32 = 3;
    pub const REMAINDER: u32 = 4;
    pub const EXPONENT: u32 = 5;
    pub const SHIFT_LEFT: u32 = 6;
    pub const SHIFT_RIGHT: u32 = 7;
    pub const UNSIGNED_SHIFT_RIGHT: u32 = 8;
    pub const LESS: u32 = 9;
    pub const GREATER: u32 = 10;
    pub const LESS_EQUAL: u32 = 11;
    pub const GREATER_EQUAL: u32 = 12;
    pub const INSTANCEOF: u32 = 13;
    pub const IN: u32 = 14;
    pub const EQUAL: u32 = 15;
    pub const NOT_EQUAL: u32 = 16;
    pub const STRICT_EQUAL: u32 = 17;
    pub const STRICT_NOT_EQUAL: u32 = 18;
    pub const BITWISE_AND: u32 = 19;
    pub const BITWISE_XOR: u32 = 20;
    pub const BITWISE_OR: u32 = 21;
    pub const LOGICAL_AND: u32 = 22;
    pub const LOGICAL_OR: u32 = 23;
    pub const NULLISH: u32 = 24;
    /// Plain `=`.
    pub const ASSIGN: u32 = 25;
}

/// One syntax node. Every field is a scalar, so a node holds no pointer and an
/// arena can be moved or mapped without fixing anything up.
#[derive(Clone, Copy, Debug)]
pub struct Node {
    pub kind: NodeKind,
    pub flags: u8,
    /// Byte offset of the construct's first byte.
    pub start: u32,
    /// Byte offset one past the construct's last byte.
    pub end: u32,
    pub first: u32,
    pub second: u32,
    pub third: u32,
}

impl Node {
    /// A node of `kind` spanning `[start, end)` with no payload.
    pub const fn new(kind: NodeKind, start: u32, end: u32) -> Self {
        Self {
            kind,
            flags: 0,
            start,
            end,
            first: 0,
            second: 0,
            third: 0,
        }
    }

    #[must_use]
    pub const fn with_payload(mut self, first: u32, second: u32, third: u32) -> Self {
        self.first = first;
        self.second = second;
        self.third = third;
        self
    }

    #[must_use]
    pub const fn with_flags(mut self, flags: u8) -> Self {
        self.flags = flags;
        self
    }

    pub const fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

/// The storage an arena is built in ran out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Full {
    Nodes,
    Lists,
    Numbers,
}

/// An append-only syntax arena over caller-provided storage.
pub struct Arena<'a> {
    nodes: &'a mut [Node],
    node_count: u32,
    lists: &'a mut [u32],
    list_count: u32,
    numbers: &'a mut [f64],
    number_count: u32,
}

impl<'a> Arena<'a> {
    pub fn new(nodes: &'a mut [Node], lists: &'a mut [u32], numbers: &'a mut [f64]) -> Self {
        Self {
            nodes,
            node_count: 0,
            lists,
            list_count: 0,
            numbers,
            number_count: 0,
        }
    }

    /// Append a node and return its index.
    pub fn push(&mut self, node: Node) -> Result<u32, Full> {
        let slot = self
            .nodes
            .get_mut(self.node_count as usize)
            .ok_or(Full::Nodes)?;
        *slot = node;
        let index = self.node_count;
        self.node_count += 1;
        Ok(index)
    }

    /// Copy `items` into the list storage and return its start and length.
    pub fn push_list(&mut self, items: &[u32]) -> Result<(u32, u32), Full> {
        let start = self.list_count as usize;
        let end = start.checked_add(items.len()).ok_or(Full::Lists)?;
        let target = self.lists.get_mut(start..end).ok_or(Full::Lists)?;
        target.copy_from_slice(items);
        self.list_count = u32::try_from(end).map_err(|_| Full::Lists)?;
        Ok((
            u32::try_from(start).map_err(|_| Full::Lists)?,
            u32::try_from(items.len()).map_err(|_| Full::Lists)?,
        ))
    }

    /// Append a Number value and return its index.
    pub fn push_number(&mut self, value: f64) -> Result<u32, Full> {
        let slot = self
            .numbers
            .get_mut(self.number_count as usize)
            .ok_or(Full::Numbers)?;
        *slot = value;
        let index = self.number_count;
        self.number_count += 1;
        Ok(index)
    }

    /// Nodes committed so far.
    pub fn nodes(&self) -> &[Node] {
        match self.nodes.get(..self.node_count as usize) {
            Some(slice) => slice,
            None => &[],
        }
    }

    pub fn node(&self, index: u32) -> Option<&Node> {
        self.nodes().get(index as usize)
    }

    /// The child indices of a list at `start` with `length` entries.
    pub fn list(&self, start: u32, length: u32) -> &[u32] {
        let begin = start as usize;
        let end = begin.saturating_add(length as usize);
        match self.lists.get(begin..end) {
            Some(slice) => slice,
            None => &[],
        }
    }

    pub fn number(&self, index: u32) -> f64 {
        match self.numbers.get(index as usize) {
            Some(&value) => value,
            None => f64::NAN,
        }
    }

    pub const fn node_count(&self) -> u32 {
        self.node_count
    }

    pub const fn list_count(&self) -> u32 {
        self.list_count
    }

    pub const fn number_count(&self) -> u32 {
        self.number_count
    }
}
