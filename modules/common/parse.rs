//! The Phasor expression parser.
//!
//! The parser drives the lexer: it states which goal symbol it expects before
//! every token, builds nodes bottom-up into an append-only arena, and never
//! back-patches a committed node. Recursion is bounded by an admitted
//! expression depth, and every storage limit produces an ordinary diagnostic
//! rather than a panic.
//!
//! The admitted grammar is the expression surface listed in the design
//! documentation. Constructs outside it, such as arrow functions and class
//! expressions, are rejected by name rather than mis-parsed.

use crate::arena::{binary_operator as binop, flag, unary_operator as unop};
use crate::arena::{
    class_member, declaration, parameter_kind, property_key, property_kind, Arena, Full, Node,
    NodeKind,
};
use crate::diagnostic::{arena as arena_argument, code, syntax_feature, Diagnostic, Severity};
use crate::lex::{Goal, Keyword, Lexer, Punctuator, Token, TokenKind};
use crate::source::Limits;

// One type across these files: each child adds `impl` blocks and sees the
// parent through `use super::*`; the parent sees what a child marks
// `pub(super)` through these globs.
#[path = "parse/classes.rs"]
mod classes;
#[path = "parse/expressions.rs"]
mod expressions;
#[path = "parse/functions.rs"]
mod functions;
#[path = "parse/modules.rs"]
mod modules;
#[path = "parse/patterns.rs"]
mod patterns;
#[path = "parse/primary.rs"]
mod primary;
#[path = "parse/statements.rs"]
mod statements;
use classes::*;
use expressions::*;
use functions::*;
use modules::*;
use patterns::*;
use primary::*;
use statements::*;

/// The expression parser over one source.
pub struct Parser<'s, 't, 'a, 'k> {
    lexer: Lexer<'s, 't>,
    arena: Arena<'a>,
    scratch: &'k mut [u32],
    scratch_length: usize,
    /// How many async functions enclose the position, which is what makes
    /// `await` an operator rather than a name.
    async_depth: u32,
    /// How many generators enclose the position, which is what makes
    /// `yield` an expression rather than a name.
    yield_depth: u32,
    limits: Limits,
    pending: Option<(Token, Goal)>,
    previous_end: u32,
    depth: u32,
    start_of_unit: bool,
    /// How many blocks enclose the position: a `using` declaration needs
    /// one, or a module's top level.
    block_depth: u32,
    /// Whether the position is directly inside a `case` clause, where a
    /// `using` declaration may not stand.
    case_clause: bool,
    /// Whether the next statement stands where only a Statement may: set
    /// by the construct whose body it is, cleared as the statement begins.
    embedded: bool,
    /// `export default function` may omit the name and still declare.
    anonymous_declaration: bool,
    /// Whether the source is a module, which is what admits `import` and
    /// `export` and makes the top level a scope of its own.
    module: bool,
}

impl<'s, 't, 'a, 'k> Parser<'s, 't, 'a, 'k> {
    pub fn new(
        lexer: Lexer<'s, 't>,
        arena: Arena<'a>,
        scratch: &'k mut [u32],
        limits: Limits,
    ) -> Self {
        Self {
            lexer,
            arena,
            scratch,
            scratch_length: 0,
            async_depth: 0,
            yield_depth: 0,
            limits: limits.clamped(),
            pending: None,
            previous_end: 0,
            depth: 0,
            start_of_unit: true,
            block_depth: 0,
            case_clause: false,
            embedded: false,
            anonymous_declaration: false,
            module: false,
        }
    }

    /// Parse the whole source as a script.
    pub fn parse_unit(&mut self) -> Result<u32, Diagnostic> {
        self.parse_script()
    }

    /// Parse the whole source as a module.
    ///
    /// A module is a script that may also import and export, and whose top
    /// level is a scope of its own rather than the global object.
    pub fn parse_module(&mut self) -> Result<u32, Diagnostic> {
        // A module's top level is async code: `await` is an operator there.
        self.async_depth = 1;
        self.module = true;
        self.parse_script()
    }

    /// The arena the parse was built in.
    pub const fn arena(&self) -> &Arena<'a> {
        &self.arena
    }

    /// Give up the parser for the tree it built.
    pub fn into_arena(self) -> Arena<'a> {
        self.arena
    }

    /// The lexer, for source positions of a reported span.
    pub const fn lexer(&self) -> &Lexer<'s, 't> {
        &self.lexer
    }

    // Token access. A token is scanned only once the goal symbol is known, and
    // re-scanned from its own start when a construct needs the other reading.

    fn peek(&mut self, goal: Goal) -> Result<Token, Diagnostic> {
        // Whether a line terminator preceded the token is a fact about the
        // trivia before it, which a re-scan from the token's own start cannot
        // see. It is carried across the re-scan, because automatic semicolon
        // insertion depends on it.
        let mut line_break = None;
        if let Some((token, cached)) = self.pending {
            if cached == goal {
                return Ok(token);
            }
            line_break = Some(token.line_break_before);
            self.lexer.seek(token.start);
            self.pending = None;
        }
        let goal = if self.start_of_unit && matches!(goal, Goal::RegExp) {
            Goal::HashbangOrDiv
        } else {
            goal
        };
        let mut token = self.lexer.next(goal)?;
        if let Some(line_break) = line_break {
            token.line_break_before = line_break;
        }
        self.start_of_unit = false;
        self.pending = Some((token, goal));
        Ok(token)
    }

    fn bump(&mut self, token: &Token) {
        self.previous_end = token.end;
        self.pending = None;
    }

    fn enter(&mut self) -> Result<(), Diagnostic> {
        self.depth += 1;
        if self.depth > self.limits.expression_depth {
            return Err(Diagnostic::at(
                code::EXPRESSION_TOO_DEEP,
                Severity::Error,
                self.previous_end,
            )
            .with(self.depth)
            .with(self.limits.expression_depth));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn unexpected(&self, token: &Token) -> Diagnostic {
        let code = if matches!(token.kind, TokenKind::EndOfSource) {
            code::UNEXPECTED_END_OF_SOURCE
        } else {
            code::UNEXPECTED_TOKEN
        };
        Diagnostic::new(
            code,
            Severity::Error,
            token.start,
            token.end.saturating_sub(token.start),
        )
    }

    fn unsupported(&self, token: &Token, feature: u32) -> Diagnostic {
        Diagnostic::new(
            code::SYNTAX_NOT_ADMITTED,
            Severity::Error,
            token.start,
            token.end.saturating_sub(token.start),
        )
        .with(feature)
    }

    fn full(&self, kind: Full) -> Diagnostic {
        let argument = match kind {
            Full::Nodes => arena_argument::NODES,
            Full::Lists => arena_argument::LISTS,
            Full::Numbers => arena_argument::NUMBERS,
        };
        Diagnostic::at(
            code::TOO_MANY_SYNTAX_NODES,
            Severity::Error,
            self.previous_end,
        )
        .with(argument)
        .with(self.limits.syntax_nodes)
    }

    fn push(&mut self, node: Node) -> Result<u32, Diagnostic> {
        if self.arena.node_count() >= self.limits.syntax_nodes {
            return Err(self.full(Full::Nodes));
        }
        self.arena.push(node).map_err(|kind| self.full(kind))
    }

    fn push_number(&mut self, value: f64) -> Result<u32, Diagnostic> {
        self.arena
            .push_number(value)
            .map_err(|kind| self.full(kind))
    }

    // The scratch stack keeps child sequences contiguous while list
    // construction nests.

    fn mark(&self) -> usize {
        self.scratch_length
    }

    fn push_child(&mut self, index: u32) -> Result<(), Diagnostic> {
        let slot = self.scratch.get_mut(self.scratch_length).ok_or_else(|| {
            Diagnostic::at(
                code::TOO_MANY_SYNTAX_NODES,
                Severity::Error,
                self.previous_end,
            )
            .with(arena_argument::STACK)
            .with(self.limits.syntax_nodes)
        })?;
        *slot = index;
        self.scratch_length += 1;
        Ok(())
    }

    fn close_list(&mut self, mark: usize) -> Result<(u32, u32), Diagnostic> {
        let items = self
            .scratch
            .get(mark..self.scratch_length)
            .unwrap_or_default();
        let list = self.arena.push_list(items);
        self.scratch_length = mark;
        match list {
            Ok(list) => Ok(list),
            Err(kind) => Err(self.full(kind)),
        }
    }

    fn expect(&mut self, punctuator: Punctuator, failure: u16) -> Result<Token, Diagnostic> {
        let token = self.peek(Goal::Div)?;
        if token.kind == TokenKind::Punctuator(punctuator) {
            self.bump(&token);
            return Ok(token);
        }
        Err(Diagnostic::new(
            failure,
            Severity::Error,
            token.start,
            token.end.saturating_sub(token.start),
        ))
    }
}

fn token_is_identifier(token: &Token) -> bool {
    matches!(token.kind, TokenKind::Identifier)
}
