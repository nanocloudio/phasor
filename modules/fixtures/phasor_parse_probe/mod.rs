//! On-graph conformance probe for the expression parser and syntax arena.
//!
//! Each bounded step parses one source into module-owned arena storage and
//! checks the shape of the tree it produced, so the parser is exercised through
//! the module ABI and the target compilation rather than a host harness.

#![cfg_attr(not(feature = "host-test"), no_std)]
#![allow(
    dead_code,
    unused_imports,
    unreachable_patterns,
    reason = "the Fluxor ABI source is mounted as one surface and this fixture consumes a subset"
)]

#[path = "../../../target/fluxor/fluxor-abi/sdk/abi.rs"]
mod abi;
use abi::SyscallTable;

include!("../../../target/fluxor/fluxor-abi/sdk/runtime.rs");

#[macro_use]
#[path = "../../common/entry.rs"]
mod entry;

#[path = "../../common/arena.rs"]
mod arena;
#[path = "../../common/diagnostic.rs"]
mod diagnostic;
#[path = "../../common/lex.rs"]
mod lex;
#[path = "../../common/numeric.rs"]
mod numeric;
#[path = "../../common/parse.rs"]
mod parse;
#[path = "../../common/probe.rs"]
mod probe;
#[path = "../../common/softfloat.rs"]
mod softfloat;
#[path = "../../common/source.rs"]
mod source;
#[path = "../../common/text.rs"]
mod text;
#[path = "../../common/unicode_id.rs"]
mod unicode_id;
#[path = "../../common/value.rs"]
mod value;
#[path = "../../common/wire.rs"]
mod wire;

use arena::{binary_operator as binop, declaration, flag, property_key, unary_operator as unop};
use arena::{Arena, Node, NodeKind};
use diagnostic::code;
use lex::Lexer;
use parse::Parser;
use source::{Limits, LineStart, LineTable};

const CASE_COUNT: u16 = 60;
const NODE_CAPACITY: usize = 64;
const LIST_CAPACITY: usize = 64;
const NUMBER_CAPACITY: usize = 16;
const SCRATCH_CAPACITY: usize = 32;
const LINE_CAPACITY: usize = 8;
const FUEL: u32 = 200_000;

/// Arena storage for one parse, owned by the module rather than the stack of a
/// deep call chain.
struct Storage {
    nodes: [Node; NODE_CAPACITY],
    lists: [u32; LIST_CAPACITY],
    numbers: [f64; NUMBER_CAPACITY],
    scratch: [u32; SCRATCH_CAPACITY],
    starts: [LineStart; LINE_CAPACITY],
}

impl Storage {
    const fn new() -> Self {
        Self {
            nodes: [Node::new(NodeKind::Null, 0, 0); NODE_CAPACITY],
            lists: [0; LIST_CAPACITY],
            numbers: [0.0; NUMBER_CAPACITY],
            scratch: [0; SCRATCH_CAPACITY],
            starts: [LineStart {
                byte: 0,
                units_before: 0,
            }; LINE_CAPACITY],
        }
    }
}

/// Parse `source` and hand the tree to `check`, which returns whether the shape
/// is the expected one.
fn parsed(
    storage: &mut Storage,
    source: &[u8],
    limits: Limits,
    check: impl Fn(&Tree) -> bool,
) -> bool {
    let table = LineTable::new(&mut storage.starts);
    let Ok(lexer) = Lexer::new(source, limits, table, FUEL) else {
        return false;
    };
    let arena = Arena::new(&mut storage.nodes, &mut storage.lists, &mut storage.numbers);
    let mut parser = Parser::new(lexer, arena, &mut storage.scratch, limits);
    match parser.parse_unit() {
        Ok(root) => check(&Tree {
            arena: parser.arena(),
            root,
            source,
        }),
        Err(_) => false,
    }
}

/// Parse `source` and check that it is rejected with `expected`.
fn rejected(storage: &mut Storage, source: &[u8], limits: Limits, expected: u16) -> bool {
    let table = LineTable::new(&mut storage.starts);
    let Ok(lexer) = Lexer::new(source, limits, table, FUEL) else {
        return false;
    };
    let arena = Arena::new(&mut storage.nodes, &mut storage.lists, &mut storage.numbers);
    let mut parser = Parser::new(lexer, arena, &mut storage.scratch, limits);
    match parser.parse_unit() {
        Ok(_) => false,
        Err(diagnostic) => diagnostic.code() == expected,
    }
}

/// A parsed tree with the accessors the assertions need.
struct Tree<'a, 'b> {
    arena: &'a Arena<'b>,
    root: u32,
    source: &'a [u8],
}

impl Tree<'_, '_> {
    fn node(&self, index: u32) -> Node {
        match self.arena.node(index) {
            Some(node) => *node,
            None => Node::new(NodeKind::Null, 0, 0),
        }
    }

    /// The root of what was parsed. A source that is one expression is one
    /// script holding one expression statement, and an assertion about the
    /// expression should not have to say so every time.
    fn root(&self) -> Node {
        let node = self.node(self.root);
        if !matches!(node.kind, NodeKind::Script) || node.second != 1 {
            return node;
        }
        let Some(&statement) = self.arena.list(node.first, node.second).first() else {
            return node;
        };
        let statement = self.node(statement);
        match statement.kind {
            NodeKind::ExpressionStatement => self.node(statement.first),
            _ => statement,
        }
    }

    /// The script node itself, for an assertion about statements.
    fn script(&self) -> Node {
        self.node(self.root)
    }

    fn text(&self, node: Node) -> &[u8] {
        let start = node.first as usize;
        let end = node.second as usize;
        match self.source.get(start..end) {
            Some(text) => text,
            None => &[],
        }
    }

    fn number(&self, node: Node) -> f64 {
        self.arena.number(node.first)
    }

    fn children(&self, node: Node) -> &[u32] {
        self.arena.list(node.first, node.second)
    }

    fn arguments(&self, node: Node) -> &[u32] {
        self.arena.list(node.second, node.third)
    }
}

#[allow(
    clippy::match_same_arms,
    reason = "each case is an independent assertion and merging arms would hide which one failed"
)]
fn run_case(storage: &mut Storage, case: u16) -> bool {
    let limits = Limits::CEILING;
    match case {
        // Precedence and associativity.
        0 => parsed(storage, b"1 + 2 * 3", limits, |tree| {
            let root = tree.root();
            let right = tree.node(root.second);
            matches!(root.kind, NodeKind::Binary)
                && root.third == binop::ADD
                && matches!(right.kind, NodeKind::Binary)
                && right.third == binop::MULTIPLY
        }),
        1 => parsed(storage, b"1 + 2 + 3", limits, |tree| {
            let root = tree.root();
            let left = tree.node(root.first);
            matches!(root.kind, NodeKind::Binary) && matches!(left.kind, NodeKind::Binary)
        }),
        2 => parsed(storage, b"2 ** 3 ** 2", limits, |tree| {
            let root = tree.root();
            let right = tree.node(root.second);
            root.third == binop::EXPONENT && right.third == binop::EXPONENT
        }),
        3 => parsed(storage, b"(1 + 2) * 3", limits, |tree| {
            let root = tree.root();
            let left = tree.node(root.first);
            root.third == binop::MULTIPLY
                && left.third == binop::ADD
                && left.has(flag::PARENTHESISED)
        }),
        4 => parsed(storage, b"a || b && c", limits, |tree| {
            let root = tree.root();
            let right = tree.node(root.second);
            matches!(root.kind, NodeKind::Logical)
                && root.third == binop::LOGICAL_OR
                && right.third == binop::LOGICAL_AND
        }),
        5 => parsed(storage, b"a = b = c", limits, |tree| {
            let root = tree.root();
            let right = tree.node(root.second);
            matches!(root.kind, NodeKind::Assign) && matches!(right.kind, NodeKind::Assign)
        }),
        6 => parsed(storage, b"a += 1", limits, |tree| {
            let root = tree.root();
            matches!(root.kind, NodeKind::Assign) && root.third == binop::ADD
        }),
        7 => parsed(storage, b"a ? b : c ? d : e", limits, |tree| {
            let root = tree.root();
            let branches = tree.arena.list(root.second, 2);
            matches!(root.kind, NodeKind::Conditional)
                && branches.len() == 2
                && matches!(tree.node(branches[1]).kind, NodeKind::Conditional)
        }),
        8 => parsed(storage, b"a, b, c", limits, |tree| {
            let root = tree.root();
            matches!(root.kind, NodeKind::Sequence) && tree.children(root).len() == 3
        }),
        9 => parsed(storage, b"a in b instanceof c", limits, |tree| {
            let root = tree.root();
            root.third == binop::INSTANCEOF && tree.node(root.first).third == binop::IN
        }),

        // Unary and update expressions.
        10 => parsed(storage, b"typeof a", limits, |tree| {
            let root = tree.root();
            matches!(root.kind, NodeKind::Unary) && root.third == unop::TYPEOF
        }),
        11 => parsed(storage, b"x++", limits, |tree| {
            let root = tree.root();
            matches!(root.kind, NodeKind::Update) && !root.has(flag::PREFIX)
        }),
        12 => parsed(storage, b"--x", limits, |tree| {
            let root = tree.root();
            matches!(root.kind, NodeKind::Update)
                && root.has(flag::PREFIX)
                && root.third == unop::DECREMENT
        }),
        13 => parsed(storage, b"delete a.b", limits, |tree| {
            let root = tree.root();
            matches!(root.kind, NodeKind::Unary)
                && root.third == unop::DELETE
                && matches!(tree.node(root.first).kind, NodeKind::Member)
        }),

        // Member access, calls, and chains.
        14 => parsed(storage, b"a.b.c", limits, |tree| {
            let root = tree.root();
            matches!(root.kind, NodeKind::Member)
                && tree.text(tree.node(root.second)) == b"c"
                && matches!(tree.node(root.first).kind, NodeKind::Member)
        }),
        15 => parsed(storage, b"a[0](1, 2)", limits, |tree| {
            let root = tree.root();
            matches!(root.kind, NodeKind::Call)
                && tree.arguments(root).len() == 2
                && matches!(tree.node(root.first).kind, NodeKind::Index)
        }),
        16 => parsed(storage, b"new Foo(1).bar", limits, |tree| {
            let root = tree.root();
            matches!(root.kind, NodeKind::Member)
                && matches!(tree.node(root.first).kind, NodeKind::New)
        }),
        17 => parsed(storage, b"new a.b.c()", limits, |tree| {
            let root = tree.root();
            matches!(root.kind, NodeKind::New)
                && matches!(tree.node(root.first).kind, NodeKind::Member)
                && tree.arguments(root).is_empty()
        }),
        18 => parsed(storage, b"a?.b", limits, |tree| {
            let root = tree.root();
            matches!(root.kind, NodeKind::Member)
                && root.has(flag::OPTIONAL)
                && root.has(flag::CHAIN_ROOT)
        }),
        19 => parsed(storage, b"a?.(b)", limits, |tree| {
            let root = tree.root();
            matches!(root.kind, NodeKind::Call) && root.has(flag::OPTIONAL)
        }),
        20 => parsed(storage, b"f(...a, b,)", limits, |tree| {
            let root = tree.root();
            let arguments = tree.arguments(root);
            arguments.len() == 2 && matches!(tree.node(arguments[0]).kind, NodeKind::Spread)
        }),

        // Literals.
        21 => parsed(storage, b"[1, , 2, ...r]", limits, |tree| {
            let root = tree.root();
            let elements = tree.children(root);
            matches!(root.kind, NodeKind::Array)
                && elements.len() == 4
                && matches!(tree.node(elements[1]).kind, NodeKind::Elision)
                && matches!(tree.node(elements[3]).kind, NodeKind::Spread)
        }),
        22 => parsed(storage, b"({ a: 1, b, [c]: d, ...e })", limits, |tree| {
            let root = tree.root();
            let properties = tree.children(root);
            matches!(root.kind, NodeKind::Object)
                && properties.len() == 4
                && matches!(tree.node(properties[0]).kind, NodeKind::Property)
                && matches!(tree.node(properties[1]).kind, NodeKind::ShorthandProperty)
                && matches!(
                    tree.node(tree.node(properties[2]).first).kind,
                    NodeKind::ComputedKey
                )
                && matches!(tree.node(properties[3]).kind, NodeKind::Spread)
        }),
        23 => parsed(storage, b"({'k': 1, 2: 3, if: 4})", limits, |tree| {
            let properties = tree.children(tree.root());
            let first = tree.node(tree.node(properties[0]).first);
            let second = tree.node(tree.node(properties[1]).first);
            let third = tree.node(tree.node(properties[2]).first);
            first.third == property_key::STRING
                && second.third == property_key::NUMBER
                && third.third == property_key::IDENTIFIER
        }),
        24 => parsed(storage, b"`a${b}c`", limits, |tree| {
            let root = tree.root();
            let parts = tree.children(root);
            matches!(root.kind, NodeKind::Template)
                && parts.len() == 3
                && matches!(tree.node(parts[0]).kind, NodeKind::TemplateElement)
                && matches!(tree.node(parts[1]).kind, NodeKind::Identifier)
        }),
        25 => parsed(storage, b"tag`x${y}z`", limits, |tree| {
            let root = tree.root();
            matches!(root.kind, NodeKind::TaggedTemplate)
                && matches!(tree.node(root.second).kind, NodeKind::Template)
        }),
        26 => parsed(storage, b"`x${`y${z}`}w`", limits, |tree| {
            let parts = tree.children(tree.root());
            parts.len() == 3 && matches!(tree.node(parts[1]).kind, NodeKind::Template)
        }),
        27 => parsed(storage, b"1.5e2", limits, |tree| {
            let root = tree.root();
            matches!(root.kind, NodeKind::Number)
                && tree.number(root).to_bits() == 150.0f64.to_bits()
        }),
        28 => parsed(storage, b"true", limits, |tree| {
            matches!(tree.root().kind, NodeKind::True)
        }),
        29 => parsed(storage, b"this", limits, |tree| {
            matches!(tree.root().kind, NodeKind::This)
        }),

        // Rejections.
        30 => rejected(storage, b"-a ** 2", limits, code::EXPONENT_OF_UNARY),
        31 => rejected(storage, b"a || b ?? c", limits, code::UNEXPECTED_TOKEN),
        32 => rejected(storage, b"1 = 2", limits, code::INVALID_ASSIGNMENT_TARGET),
        33 => rejected(
            storage,
            b"a?.b = 1",
            limits,
            code::OPTIONAL_CHAIN_ASSIGNMENT,
        ),
        34 => parsed(storage, b"(a) => a", limits, |tree| {
            let root = tree.root();
            let entries = tree.arena.list(root.second, root.third);
            matches!(root.kind, NodeKind::Function)
                && root.has(flag::ARROW)
                && root.has(flag::CONCISE_BODY)
                && entries.len() == 2
                && matches!(tree.node(entries[1]).kind, NodeKind::Parameter)
        }),
        35 => parsed(storage, b"/re+/gi", limits, |tree| {
            let root = tree.root();
            matches!(root.kind, NodeKind::RegExp) && root.third != 0
        }),
        36 => parsed(storage, b"#x in a", limits, |tree| {
            // The ergonomic brand check: the private name reads as the key
            // it stores under, on the left of `in`.
            let root = tree.root();
            matches!(root.kind, NodeKind::Binary)
        }),
        37 => rejected(storage, b"a ? b", limits, code::EXPECTED_COLON),
        38 => {
            let mut limits = Limits::CEILING;
            limits.expression_depth = 12;
            rejected(storage, b"((((((1))))))", limits, code::EXPRESSION_TOO_DEEP)
        }
        39 => {
            let mut limits = Limits::CEILING;
            limits.syntax_nodes = 4;
            rejected(
                storage,
                b"1 + 2 + 3 + 4",
                limits,
                code::TOO_MANY_SYNTAX_NODES,
            )
        }
        // Statements, declarations, and functions.
        40 => parsed(storage, b"var x = 1;", limits, |tree| {
            let root = tree.root();
            matches!(root.kind, NodeKind::Declaration)
                && root.third == declaration::VAR
                && root.second == 1
        }),
        41 => parsed(storage, b"let a = 1, b = 2;", limits, |tree| {
            let root = tree.root();
            matches!(root.kind, NodeKind::Declaration)
                && root.third == declaration::LET
                && root.second == 2
        }),
        42 => rejected(storage, b"const c;", limits, code::MISSING_INITIALISER),
        43 => parsed(storage, b"if (a) b; else c;", limits, |tree| {
            let root = tree.root();
            matches!(root.kind, NodeKind::If) && root.third != arena::NONE
        }),
        44 => parsed(storage, b"while (a) { b; }", limits, |tree| {
            matches!(tree.root().kind, NodeKind::While)
        }),
        45 => parsed(storage, b"for (var i = 0; i < 2; i++) c;", limits, |tree| {
            let root = tree.root();
            matches!(root.kind, NodeKind::For)
                && tree.arena.list(root.second, root.third).len() == 3
        }),
        46 => parsed(storage, b"for (const v of xs) c;", limits, |tree| {
            let root = tree.root();
            matches!(root.kind, NodeKind::ForInOf) && root.has(flag::OF)
        }),
        47 => parsed(storage, b"function f(a, b) { return a; }", limits, |tree| {
            let root = tree.root();
            let entries = tree.arena.list(root.second, root.third);
            matches!(root.kind, NodeKind::Function)
                && root.first != arena::NONE
                && entries.len() == 3
                && matches!(tree.node(entries[0]).kind, NodeKind::Block)
        }),
        48 => parsed(
            storage,
            b"try { a; } catch (e) { b; } finally { c; }",
            limits,
            |tree| {
                let root = tree.root();
                matches!(root.kind, NodeKind::Try)
                    && root.second != arena::NONE
                    && root.third != arena::NONE
            },
        ),
        49 => parsed(
            storage,
            b"switch (a) { case 1: b; break; default: c; }",
            limits,
            |tree| {
                let root = tree.root();
                matches!(root.kind, NodeKind::Switch) && root.third == 2
            },
        ),
        50 => parsed(
            storage,
            b"outer: for (;;) { break outer; }",
            limits,
            |tree| matches!(tree.root().kind, NodeKind::Labelled),
        ),
        // A line terminator ends a statement where a semicolon was left out,
        // and `return` on its own line returns nothing.
        51 => parsed(storage, b"a = 1\nb = 2\n", limits, |tree| {
            let script = tree.script();
            matches!(script.kind, NodeKind::Script) && script.second == 2
        }),
        52 => parsed(storage, b"function f() { return\n1; }", limits, |tree| {
            let root = tree.root();
            let entries = tree.arena.list(root.second, root.third);
            let body = tree.node(entries[0]);
            let statements = tree.arena.list(body.first, body.second);
            statements.len() == 2 && tree.node(statements[0]).first == arena::NONE
        }),
        53 => rejected(storage, b"var 1 = 2;", limits, code::UNEXPECTED_TOKEN),
        54 => parsed(storage, b"{ let a = 1; }", limits, |tree| {
            matches!(tree.root().kind, NodeKind::Block)
        }),
        55 => parsed(storage, b"do a; while (b);", limits, |tree| {
            matches!(tree.root().kind, NodeKind::DoWhile)
        }),
        56 => parsed(storage, b"let f = function () {};", limits, |tree| {
            let root = tree.root();
            let declarator = tree.node(tree.arena.list(root.first, root.second)[0]);
            let value = tree.node(declarator.second);
            matches!(value.kind, NodeKind::Function) && value.first == arena::NONE
        }),
        57 => parsed(storage, b"() => {}", limits, |tree| {
            let root = tree.root();
            root.has(flag::ARROW) && tree.arena.list(root.second, root.third).len() == 1
        }),
        58 => parsed(storage, b"class A {}", limits, |tree| {
            matches!(tree.root().kind, NodeKind::Class)
        }),
        59 => rejected(storage, b"import 'x';", limits, code::SYNTAX_NOT_ADMITTED),

        _ => true,
    }
}

#[repr(C)]
struct State {
    syscalls: *const SyscallTable,
    announced: bool,
    report_out: i32,
    exit_out: i32,
    storage: Storage,
    progress: probe::Progress,
}

entry! {
    State;
    primary { report_out }
    inputs {}
    outputs { exit_out = 1 }
}

#[cfg_attr(not(feature = "host-test"), unsafe(no_mangle))]
#[link_section = ".text.module_step"]
pub extern "C" fn module_step(state: *mut u8) -> i32 {
    if state.is_null() {
        return -1;
    }
    // SAFETY: `state` is the block `module_new` laid out, non-null as
    // checked above, and the loader hands it to one step at a time.
    let state = unsafe { &mut *state.cast::<State>() };
    if state.syscalls.is_null() {
        return -2;
    }
    // SAFETY: the table pointer was stored by `module_new` and checked
    // non-null above; the loader keeps it live for the module's lifetime.
    let syscalls = unsafe { &*state.syscalls };
    announce_ready!(state);
    probe::step(
        &mut state.progress,
        syscalls,
        state.report_out,
        state.exit_out,
        b"phasor-parse-probe",
        CASE_COUNT,
        |case| run_case(&mut state.storage, case),
    )
}

include!("../../../target/fluxor/fluxor-abi/sdk/runtime/wasm_entry.rs");
