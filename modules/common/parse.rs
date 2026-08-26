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
use crate::arena::{declaration, property_key, property_kind, Arena, Full, Node, NodeKind};
use crate::diagnostic::{arena as arena_argument, code, syntax_feature, Diagnostic, Severity};
use crate::lex::{Goal, Keyword, Lexer, Punctuator, Token, TokenKind};
use crate::source::Limits;

/// Binding powers, from the loosest binary operator to the tightest.
const PRECEDENCE_NULLISH: u8 = 1;
const PRECEDENCE_LOGICAL_OR: u8 = 2;
const PRECEDENCE_LOGICAL_AND: u8 = 3;
const PRECEDENCE_BITWISE_OR: u8 = 4;
const PRECEDENCE_BITWISE_XOR: u8 = 5;
const PRECEDENCE_BITWISE_AND: u8 = 6;
const PRECEDENCE_EQUALITY: u8 = 7;
const PRECEDENCE_RELATIONAL: u8 = 8;
const PRECEDENCE_SHIFT: u8 = 9;
const PRECEDENCE_ADDITIVE: u8 = 10;
const PRECEDENCE_MULTIPLICATIVE: u8 = 11;
const PRECEDENCE_EXPONENT: u8 = 12;

/// The expression parser over one source.
pub struct Parser<'s, 't, 'a, 'k> {
    lexer: Lexer<'s, 't>,
    arena: Arena<'a>,
    scratch: &'k mut [u32],
    scratch_length: usize,
    limits: Limits,
    pending: Option<(Token, Goal)>,
    previous_end: u32,
    depth: u32,
    start_of_unit: bool,
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
            limits: limits.clamped(),
            pending: None,
            previous_end: 0,
            depth: 0,
            start_of_unit: true,
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
        self.module = true;
        self.parse_script()
    }

    /// The arena the parse was built in.
    pub const fn arena(&self) -> &Arena<'a> {
        &self.arena
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

    // Grammar.

    /// `Expression : AssignmentExpression (`,` AssignmentExpression)*`
    pub fn parse_expression(&mut self) -> Result<u32, Diagnostic> {
        let first = self.parse_assignment()?;
        let token = self.peek(Goal::Div)?;
        if token.kind != TokenKind::Punctuator(Punctuator::Comma) {
            return Ok(first);
        }

        self.enter()?;
        let mark = self.mark();
        self.push_child(first)?;
        let start = self.node_start(first);
        loop {
            let token = self.peek(Goal::Div)?;
            if token.kind != TokenKind::Punctuator(Punctuator::Comma) {
                break;
            }
            self.bump(&token);
            let next = self.parse_assignment()?;
            self.push_child(next)?;
        }
        let (list_start, length) = self.close_list(mark)?;
        self.leave();
        let end = self.previous_end;
        self.push(Node::new(NodeKind::Sequence, start, end).with_payload(list_start, length, 0))
    }

    fn parse_assignment(&mut self) -> Result<u32, Diagnostic> {
        self.enter()?;
        let left = self.parse_conditional()?;
        let token = self.peek(Goal::Div)?;

        if token.kind == TokenKind::Punctuator(Punctuator::Arrow) && !token.line_break_before {
            // The head was parsed as an expression; `=>` is what says it was a
            // parameter list all along. A line terminator before it cannot be
            // an arrow, because the arrow is a restricted production.
            let start = self.node_start(left);
            if !self.is_arrow_head(left) {
                return Err(self.parameter_failure(left));
            }
            self.leave();
            return self.arrow_from(left, start);
        }

        let Some(operator) = assignment_operator(&token) else {
            self.leave();
            return Ok(left);
        };
        self.check_assignment_target(left, &token)?;
        self.bump(&token);
        let right = self.parse_assignment()?;
        self.leave();
        let start = self.node_start(left);
        let end = self.previous_end;
        self.push(Node::new(NodeKind::Assign, start, end).with_payload(left, right, operator))
    }

    fn check_assignment_target(&self, index: u32, token: &Token) -> Result<(), Diagnostic> {
        let Some(node) = self.arena.node(index) else {
            return Err(self.unexpected(token));
        };
        match node.kind {
            NodeKind::Identifier => Ok(()),
            NodeKind::Member | NodeKind::Index => {
                if node.has(flag::OPTIONAL) || node.has(flag::CHAIN_ROOT) {
                    return Err(Diagnostic::new(
                        code::OPTIONAL_CHAIN_ASSIGNMENT,
                        Severity::Error,
                        node.start,
                        node.end.saturating_sub(node.start),
                    ));
                }
                Ok(())
            }
            NodeKind::Array | NodeKind::Object => Err(Diagnostic::new(
                code::SYNTAX_NOT_ADMITTED,
                Severity::Error,
                node.start,
                node.end.saturating_sub(node.start),
            )
            .with(syntax_feature::DESTRUCTURING)),
            _ => Err(Diagnostic::new(
                code::INVALID_ASSIGNMENT_TARGET,
                Severity::Error,
                node.start,
                node.end.saturating_sub(node.start),
            )),
        }
    }

    fn parse_conditional(&mut self) -> Result<u32, Diagnostic> {
        let test = self.parse_binary(1)?;
        let token = self.peek(Goal::Div)?;
        if token.kind != TokenKind::Punctuator(Punctuator::Question) {
            return Ok(test);
        }
        self.enter()?;
        self.bump(&token);
        let consequent = self.parse_assignment()?;
        let colon = self.peek(Goal::Div)?;
        if colon.kind != TokenKind::Punctuator(Punctuator::Colon) {
            return Err(Diagnostic::new(
                code::EXPECTED_COLON,
                Severity::Error,
                colon.start,
                colon.end.saturating_sub(colon.start),
            ));
        }
        self.bump(&colon);
        let alternate = self.parse_assignment()?;
        let mark = self.mark();
        self.push_child(consequent)?;
        self.push_child(alternate)?;
        let (branches, _) = self.close_list(mark)?;
        self.leave();
        let start = self.node_start(test);
        let end = self.previous_end;
        self.push(Node::new(NodeKind::Conditional, start, end).with_payload(test, branches, 0))
    }

    /// Precedence climbing over the binary, relational, and logical operators.
    fn parse_binary(&mut self, minimum: u8) -> Result<u32, Diagnostic> {
        self.enter()?;
        let mut left = self.parse_unary()?;
        let mut left_was_unary = self.is_unary_node(left);

        loop {
            let token = self.peek(Goal::Div)?;
            let Some((operator, precedence, logical)) = binary_operator(&token) else {
                break;
            };
            if precedence < minimum {
                break;
            }
            if operator == binop::EXPONENT && left_was_unary && !self.is_parenthesised(left) {
                return Err(Diagnostic::new(
                    code::EXPONENT_OF_UNARY,
                    Severity::Error,
                    token.start,
                    token.end.saturating_sub(token.start),
                ));
            }
            if operator == binop::NULLISH && self.is_unparenthesised_logical(left) {
                return Err(self.unexpected(&token));
            }
            self.bump(&token);

            // `**` is right-associative; every other operator is left-associative.
            let next_minimum = if operator == binop::EXPONENT {
                precedence
            } else {
                precedence + 1
            };
            let right = self.parse_binary(next_minimum)?;
            if operator == binop::NULLISH && self.is_unparenthesised_logical(right) {
                return Err(self.unexpected(&token));
            }
            let kind = if logical {
                NodeKind::Logical
            } else {
                NodeKind::Binary
            };
            let start = self.node_start(left);
            let end = self.previous_end;
            left = self.push(Node::new(kind, start, end).with_payload(left, right, operator))?;
            left_was_unary = false;
        }
        self.leave();
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<u32, Diagnostic> {
        self.enter()?;
        let token = self.peek(Goal::RegExp)?;
        let operator = match token.kind {
            TokenKind::Punctuator(Punctuator::Plus) => Some(unop::PLUS),
            TokenKind::Punctuator(Punctuator::Minus) => Some(unop::MINUS),
            TokenKind::Punctuator(Punctuator::Tilde) => Some(unop::BITWISE_NOT),
            TokenKind::Punctuator(Punctuator::Bang) => Some(unop::LOGICAL_NOT),
            TokenKind::Keyword(Keyword::Delete) => Some(unop::DELETE),
            TokenKind::Keyword(Keyword::Void) => Some(unop::VOID),
            TokenKind::Keyword(Keyword::Typeof) => Some(unop::TYPEOF),
            _ => None,
        };
        if let Some(operator) = operator {
            self.bump(&token);
            let operand = self.parse_unary()?;
            self.leave();
            let end = self.previous_end;
            return self.push(
                Node::new(NodeKind::Unary, token.start, end).with_payload(operand, 0, operator),
            );
        }

        let update = match token.kind {
            TokenKind::Punctuator(Punctuator::PlusPlus) => Some(unop::INCREMENT),
            TokenKind::Punctuator(Punctuator::MinusMinus) => Some(unop::DECREMENT),
            _ => None,
        };
        if let Some(operator) = update {
            self.bump(&token);
            let operand = self.parse_unary()?;
            self.check_update_target(operand, &token)?;
            self.leave();
            let end = self.previous_end;
            return self.push(
                Node::new(NodeKind::Update, token.start, end)
                    .with_payload(operand, 0, operator)
                    .with_flags(flag::PREFIX),
            );
        }

        let operand = self.parse_left_hand_side()?;
        let token = self.peek(Goal::Div)?;
        let postfix = match token.kind {
            TokenKind::Punctuator(Punctuator::PlusPlus) => Some(unop::INCREMENT),
            TokenKind::Punctuator(Punctuator::MinusMinus) => Some(unop::DECREMENT),
            _ => None,
        };
        // A line terminator before `++` or `--` ends the expression instead.
        if let Some(operator) = postfix {
            if !token.line_break_before {
                self.check_update_target(operand, &token)?;
                self.bump(&token);
                self.leave();
                let start = self.node_start(operand);
                return self.push(
                    Node::new(NodeKind::Update, start, token.end)
                        .with_payload(operand, 0, operator),
                );
            }
        }
        self.leave();
        Ok(operand)
    }

    fn check_update_target(&self, index: u32, token: &Token) -> Result<(), Diagnostic> {
        let Some(node) = self.arena.node(index) else {
            return Err(self.unexpected(token));
        };
        match node.kind {
            NodeKind::Identifier => Ok(()),
            NodeKind::Member | NodeKind::Index if !node.has(flag::CHAIN_ROOT) => Ok(()),
            _ => Err(Diagnostic::new(
                code::INVALID_ASSIGNMENT_TARGET,
                Severity::Error,
                node.start,
                node.end.saturating_sub(node.start),
            )),
        }
    }

    /// Member accesses, calls, `new`, optional chains, and tagged templates.
    fn parse_left_hand_side(&mut self) -> Result<u32, Diagnostic> {
        self.enter()?;
        let token = self.peek(Goal::RegExp)?;
        let mut expression = if token.kind == TokenKind::Keyword(Keyword::New) {
            self.parse_new(&token)?
        } else {
            self.parse_primary()?
        };
        let mut optional_seen = false;

        loop {
            let token = self.peek(Goal::Div)?;
            let start = self.node_start(expression);
            match token.kind {
                TokenKind::Punctuator(Punctuator::Dot) => {
                    self.bump(&token);
                    let name = self.parse_property_name_after_dot()?;
                    let end = self.previous_end;
                    expression = self.push(
                        Node::new(NodeKind::Member, start, end).with_payload(expression, name, 0),
                    )?;
                }
                TokenKind::Punctuator(Punctuator::OptionalChain) => {
                    self.bump(&token);
                    optional_seen = true;
                    expression = self.parse_optional_link(expression, start)?;
                }
                TokenKind::Punctuator(Punctuator::OpenBracket) => {
                    self.bump(&token);
                    let index = self.parse_expression()?;
                    let _ = self.expect(Punctuator::CloseBracket, code::EXPECTED_CLOSE_BRACKET)?;
                    let end = self.previous_end;
                    expression = self.push(
                        Node::new(NodeKind::Index, start, end).with_payload(expression, index, 0),
                    )?;
                }
                TokenKind::Punctuator(Punctuator::OpenParen) => {
                    let (list, length) = self.parse_arguments()?;
                    let end = self.previous_end;
                    expression = self.push(
                        Node::new(NodeKind::Call, start, end)
                            .with_payload(expression, list, length),
                    )?;
                }
                TokenKind::NoSubstitutionTemplate | TokenKind::TemplateHead => {
                    let template = self.parse_template()?;
                    let end = self.previous_end;
                    expression = self.push(
                        Node::new(NodeKind::TaggedTemplate, start, end)
                            .with_payload(expression, template, 0),
                    )?;
                }
                _ => break,
            }
        }

        if optional_seen {
            if let Some(node) = self.arena.node(expression) {
                let marked = *node;
                expression = self.push(marked.with_flags(marked.flags | flag::CHAIN_ROOT))?;
            }
        }
        self.leave();
        Ok(expression)
    }

    fn parse_optional_link(&mut self, object: u32, start: u32) -> Result<u32, Diagnostic> {
        let token = self.peek(Goal::Div)?;
        match token.kind {
            TokenKind::Punctuator(Punctuator::OpenParen) => {
                let (list, length) = self.parse_arguments()?;
                let end = self.previous_end;
                self.push(
                    Node::new(NodeKind::Call, start, end)
                        .with_payload(object, list, length)
                        .with_flags(flag::OPTIONAL),
                )
            }
            TokenKind::Punctuator(Punctuator::OpenBracket) => {
                self.bump(&token);
                let index = self.parse_expression()?;
                let _ = self.expect(Punctuator::CloseBracket, code::EXPECTED_CLOSE_BRACKET)?;
                let end = self.previous_end;
                self.push(
                    Node::new(NodeKind::Index, start, end)
                        .with_payload(object, index, 0)
                        .with_flags(flag::OPTIONAL),
                )
            }
            _ => {
                let name = self.parse_property_name_after_dot()?;
                let end = self.previous_end;
                self.push(
                    Node::new(NodeKind::Member, start, end)
                        .with_payload(object, name, 0)
                        .with_flags(flag::OPTIONAL),
                )
            }
        }
    }

    /// The name after `.` or `?.`, which may be any identifier or reserved word.
    fn parse_property_name_after_dot(&mut self) -> Result<u32, Diagnostic> {
        let token = self.peek(Goal::Div)?;
        match token.kind {
            TokenKind::Identifier | TokenKind::Keyword(_) => {
                self.bump(&token);
                self.push(
                    Node::new(NodeKind::PropertyName, token.start, token.end).with_payload(
                        token.inner_start,
                        token.inner_end,
                        property_key::IDENTIFIER,
                    ),
                )
            }
            TokenKind::PrivateName => Err(Diagnostic::new(
                code::PRIVATE_NAME_OUT_OF_CONTEXT,
                Severity::Error,
                token.start,
                token.end.saturating_sub(token.start),
            )),
            _ => Err(Diagnostic::new(
                code::EXPECTED_PROPERTY_NAME,
                Severity::Error,
                token.start,
                token.end.saturating_sub(token.start),
            )),
        }
    }

    fn parse_arguments(&mut self) -> Result<(u32, u32), Diagnostic> {
        let open = self.peek(Goal::Div)?;
        self.bump(&open);
        let mark = self.mark();
        loop {
            let token = self.peek(Goal::RegExp)?;
            if token.kind == TokenKind::Punctuator(Punctuator::CloseParen) {
                self.bump(&token);
                break;
            }
            let argument = if token.kind == TokenKind::Punctuator(Punctuator::Ellipsis) {
                self.bump(&token);
                let value = self.parse_assignment()?;
                let end = self.previous_end;
                self.push(Node::new(NodeKind::Spread, token.start, end).with_payload(value, 0, 0))?
            } else {
                self.parse_assignment()?
            };
            self.push_child(argument)?;

            let separator = self.peek(Goal::Div)?;
            match separator.kind {
                TokenKind::Punctuator(Punctuator::Comma) => self.bump(&separator),
                TokenKind::Punctuator(Punctuator::CloseParen) => {
                    self.bump(&separator);
                    break;
                }
                _ => {
                    self.scratch_length = mark;
                    return Err(Diagnostic::new(
                        code::EXPECTED_CLOSE_PAREN,
                        Severity::Error,
                        separator.start,
                        separator.end.saturating_sub(separator.start),
                    ));
                }
            }
        }
        self.close_list(mark)
    }

    fn parse_new(&mut self, token: &Token) -> Result<u32, Diagnostic> {
        self.bump(token);
        let dot = self.peek(Goal::Div)?;
        if dot.kind == TokenKind::Punctuator(Punctuator::Dot) {
            return Err(self.unsupported(&dot, syntax_feature::NEW_TARGET));
        }

        self.enter()?;
        let inner = self.peek(Goal::RegExp)?;
        let mut callee = if inner.kind == TokenKind::Keyword(Keyword::New) {
            self.parse_new(&inner)?
        } else {
            self.parse_primary()?
        };

        // Member accesses bind to the callee; the first argument list belongs
        // to this `new`.
        loop {
            let next = self.peek(Goal::Div)?;
            let start = self.node_start(callee);
            match next.kind {
                TokenKind::Punctuator(Punctuator::Dot) => {
                    self.bump(&next);
                    let name = self.parse_property_name_after_dot()?;
                    let end = self.previous_end;
                    callee = self.push(
                        Node::new(NodeKind::Member, start, end).with_payload(callee, name, 0),
                    )?;
                }
                TokenKind::Punctuator(Punctuator::OpenBracket) => {
                    self.bump(&next);
                    let index = self.parse_expression()?;
                    let _ = self.expect(Punctuator::CloseBracket, code::EXPECTED_CLOSE_BRACKET)?;
                    let end = self.previous_end;
                    callee = self.push(
                        Node::new(NodeKind::Index, start, end).with_payload(callee, index, 0),
                    )?;
                }
                _ => break,
            }
        }

        let next = self.peek(Goal::Div)?;
        let (list, length) = if next.kind == TokenKind::Punctuator(Punctuator::OpenParen) {
            self.parse_arguments()?
        } else {
            (0, 0)
        };
        self.leave();
        let end = self.previous_end;
        self.push(Node::new(NodeKind::New, token.start, end).with_payload(callee, list, length))
    }

    fn parse_primary(&mut self) -> Result<u32, Diagnostic> {
        self.enter()?;
        let token = self.peek(Goal::RegExp)?;
        let node = match token.kind {
            TokenKind::Identifier => {
                if token.spells_reserved {
                    return Err(Diagnostic::new(
                        code::ESCAPED_RESERVED_WORD,
                        Severity::Error,
                        token.start,
                        token.end.saturating_sub(token.start),
                    ));
                }
                self.bump(&token);
                self.push(
                    Node::new(NodeKind::Identifier, token.start, token.end).with_payload(
                        token.inner_start,
                        token.inner_end,
                        0,
                    ),
                )?
            }
            TokenKind::Number => {
                self.bump(&token);
                let value = self.push_number(token.number)?;
                self.push(
                    Node::new(NodeKind::Number, token.start, token.end).with_payload(value, 0, 0),
                )?
            }
            TokenKind::BigInt => {
                self.bump(&token);
                self.push(
                    Node::new(NodeKind::BigInt, token.start, token.end).with_payload(
                        token.inner_start,
                        token.inner_end,
                        u32::from(token.radix),
                    ),
                )?
            }
            TokenKind::String => {
                self.bump(&token);
                self.push(
                    Node::new(NodeKind::String, token.start, token.end).with_payload(
                        token.inner_start,
                        token.inner_end,
                        token.code_units,
                    ),
                )?
            }
            TokenKind::NoSubstitutionTemplate | TokenKind::TemplateHead => self.parse_template()?,
            TokenKind::Keyword(Keyword::This) => {
                self.bump(&token);
                self.push(Node::new(NodeKind::This, token.start, token.end))?
            }
            TokenKind::Keyword(Keyword::Null) => {
                self.bump(&token);
                self.push(Node::new(NodeKind::Null, token.start, token.end))?
            }
            TokenKind::Keyword(Keyword::True) => {
                self.bump(&token);
                self.push(Node::new(NodeKind::True, token.start, token.end))?
            }
            TokenKind::Keyword(Keyword::False) => {
                self.bump(&token);
                self.push(Node::new(NodeKind::False, token.start, token.end))?
            }
            TokenKind::Punctuator(Punctuator::OpenBracket) => self.parse_array()?,
            TokenKind::Punctuator(Punctuator::OpenBrace) => self.parse_object()?,
            TokenKind::Punctuator(Punctuator::OpenParen) => {
                self.bump(&token);
                let empty = self.peek(Goal::RegExp)?;
                if empty.kind == TokenKind::Punctuator(Punctuator::CloseParen) {
                    // `()` is not an expression: the only thing it can be is an
                    // arrow's empty parameter list.
                    self.bump(&empty);
                    let arrow = self.peek(Goal::Div)?;
                    if arrow.kind != TokenKind::Punctuator(Punctuator::Arrow) {
                        return Err(self.unexpected(&arrow));
                    }
                    self.leave();
                    return self.arrow_from(crate::arena::NONE, token.start);
                }
                let inner = self.parse_expression()?;
                let _ = self.expect(Punctuator::CloseParen, code::EXPECTED_CLOSE_PAREN)?;
                if let Some(node) = self.arena.node(inner) {
                    let marked = *node;
                    self.push(marked.with_flags(marked.flags | flag::PARENTHESISED))?
                } else {
                    inner
                }
            }
            TokenKind::RegExp => {
                self.bump(&token);
                self.push(
                    Node::new(NodeKind::RegExp, token.start, token.end).with_payload(
                        token.inner_start,
                        token.inner_end,
                        u32::from(token.flags),
                    ),
                )?
            }
            TokenKind::PrivateName => {
                return Err(Diagnostic::new(
                    code::PRIVATE_NAME_OUT_OF_CONTEXT,
                    Severity::Error,
                    token.start,
                    token.end.saturating_sub(token.start),
                ));
            }
            TokenKind::Keyword(Keyword::Function) => self.parse_function(false)?,
            TokenKind::Keyword(Keyword::Class) => {
                return Err(self.unsupported(&token, syntax_feature::CLASS_EXPRESSION));
            }
            TokenKind::Keyword(Keyword::Await) => {
                return Err(self.unsupported(&token, syntax_feature::ASYNC));
            }
            TokenKind::Keyword(Keyword::Yield) => {
                return Err(self.unsupported(&token, syntax_feature::YIELD));
            }
            TokenKind::Keyword(Keyword::Super) => {
                return Err(self.unsupported(&token, syntax_feature::SUPER));
            }
            TokenKind::Keyword(Keyword::Import) => {
                return Err(self.unsupported(&token, syntax_feature::IMPORT));
            }
            TokenKind::Keyword(
                Keyword::Var
                | Keyword::Const
                | Keyword::If
                | Keyword::For
                | Keyword::While
                | Keyword::Do
                | Keyword::Return
                | Keyword::Switch
                | Keyword::Try
                | Keyword::Throw
                | Keyword::Debugger
                | Keyword::With
                | Keyword::Break
                | Keyword::Continue,
            ) => return Err(self.unsupported(&token, syntax_feature::STATEMENT)),
            TokenKind::EndOfSource => {
                return Err(Diagnostic::at(
                    code::UNEXPECTED_END_OF_SOURCE,
                    Severity::Error,
                    token.start,
                ));
            }
            _ => {
                return Err(Diagnostic::new(
                    code::EXPECTED_EXPRESSION,
                    Severity::Error,
                    token.start,
                    token.end.saturating_sub(token.start),
                ));
            }
        };
        self.leave();
        Ok(node)
    }

    fn parse_array(&mut self) -> Result<u32, Diagnostic> {
        let open = self.peek(Goal::RegExp)?;
        self.bump(&open);
        let mark = self.mark();
        loop {
            let token = self.peek(Goal::RegExp)?;
            match token.kind {
                TokenKind::Punctuator(Punctuator::CloseBracket) => {
                    self.bump(&token);
                    break;
                }
                TokenKind::Punctuator(Punctuator::Comma) => {
                    self.bump(&token);
                    let elision =
                        self.push(Node::new(NodeKind::Elision, token.start, token.end))?;
                    self.push_child(elision)?;
                    continue;
                }
                TokenKind::Punctuator(Punctuator::Ellipsis) => {
                    self.bump(&token);
                    let value = self.parse_assignment()?;
                    let end = self.previous_end;
                    let spread = self.push(
                        Node::new(NodeKind::Spread, token.start, end).with_payload(value, 0, 0),
                    )?;
                    self.push_child(spread)?;
                }
                _ => {
                    let element = self.parse_assignment()?;
                    self.push_child(element)?;
                }
            }

            let separator = self.peek(Goal::Div)?;
            match separator.kind {
                TokenKind::Punctuator(Punctuator::Comma) => self.bump(&separator),
                TokenKind::Punctuator(Punctuator::CloseBracket) => {
                    self.bump(&separator);
                    break;
                }
                _ => {
                    self.scratch_length = mark;
                    return Err(Diagnostic::new(
                        code::EXPECTED_CLOSE_BRACKET,
                        Severity::Error,
                        separator.start,
                        separator.end.saturating_sub(separator.start),
                    ));
                }
            }
        }
        let (list, length) = self.close_list(mark)?;
        let end = self.previous_end;
        self.push(Node::new(NodeKind::Array, open.start, end).with_payload(list, length, 0))
    }

    fn parse_object(&mut self) -> Result<u32, Diagnostic> {
        let open = self.peek(Goal::RegExp)?;
        self.bump(&open);
        let mark = self.mark();
        loop {
            let token = self.peek(Goal::RegExp)?;
            if token.kind == TokenKind::Punctuator(Punctuator::CloseBrace) {
                self.bump(&token);
                break;
            }
            let property = self.parse_property()?;
            self.push_child(property)?;

            let separator = self.peek(Goal::Div)?;
            match separator.kind {
                TokenKind::Punctuator(Punctuator::Comma) => self.bump(&separator),
                TokenKind::Punctuator(Punctuator::CloseBrace) => {
                    self.bump(&separator);
                    break;
                }
                _ => {
                    self.scratch_length = mark;
                    return Err(Diagnostic::new(
                        code::EXPECTED_CLOSE_BRACE,
                        Severity::Error,
                        separator.start,
                        separator.end.saturating_sub(separator.start),
                    ));
                }
            }
        }
        let (list, length) = self.close_list(mark)?;
        let end = self.previous_end;
        self.push(Node::new(NodeKind::Object, open.start, end).with_payload(list, length, 0))
    }

    fn parse_property(&mut self) -> Result<u32, Diagnostic> {
        self.enter()?;
        let token = self.peek(Goal::RegExp)?;
        if token.kind == TokenKind::Punctuator(Punctuator::Ellipsis) {
            self.bump(&token);
            let value = self.parse_assignment()?;
            self.leave();
            let end = self.previous_end;
            return self
                .push(Node::new(NodeKind::Spread, token.start, end).with_payload(value, 0, 0));
        }

        // `get name(...)` and `set name(...)` define accessors; `get` and
        // `set` followed by anything else are ordinary property names.
        if matches!(token.kind, TokenKind::Identifier)
            && !token.escaped
            && matches!(self.token_text(&token), b"get" | b"set")
        {
            let after = self.peek_after(&token)?;
            let names_property = matches!(
                after.kind,
                TokenKind::Identifier
                    | TokenKind::Keyword(_)
                    | TokenKind::String
                    | TokenKind::Number
                    | TokenKind::Punctuator(Punctuator::OpenBracket)
            );
            if names_property {
                let getter = self.token_text(&token) == b"get";
                self.bump(&token);
                return self.parse_accessor(getter);
            }
        }

        let key = self.parse_property_key(&token)?;

        let next = self.peek(Goal::Div)?;
        match next.kind {
            TokenKind::Punctuator(Punctuator::Colon) => {
                self.bump(&next);
                let value = self.parse_assignment()?;
                self.leave();
                let start = self.node_start(key);
                let end = self.previous_end;
                self.push(Node::new(NodeKind::Property, start, end).with_payload(key, value, 0))
            }
            TokenKind::Punctuator(Punctuator::OpenParen) => {
                Err(self.unsupported(&next, syntax_feature::METHOD_DEFINITION))
            }
            TokenKind::Punctuator(Punctuator::Assign) => {
                Err(self.unsupported(&next, syntax_feature::DESTRUCTURING))
            }
            _ => {
                // Shorthand. Only a plain identifier may stand for both.
                let Some(node) = self.arena.node(key) else {
                    return Err(self.unexpected(&next));
                };
                if !matches!(node.kind, NodeKind::PropertyName) || !token_is_identifier(&token) {
                    return Err(Diagnostic::new(
                        code::EXPECTED_PROPERTY_NAME,
                        Severity::Error,
                        next.start,
                        next.end.saturating_sub(next.start),
                    ));
                }
                let (start, end) = (node.start, node.end);
                let name = self.push(Node::new(NodeKind::Identifier, start, end).with_payload(
                    node.first,
                    node.second,
                    0,
                ))?;
                self.leave();
                self.push(
                    Node::new(NodeKind::ShorthandProperty, start, end).with_payload(name, 0, 0),
                )
            }
        }
    }

    /// One accessor property: the key, an empty or one-name parameter list,
    /// and a body, carried as an anonymous function the property points at.
    fn parse_accessor(&mut self, getter: bool) -> Result<u32, Diagnostic> {
        let token = self.peek(Goal::RegExp)?;
        let key = self.parse_property_key(&token)?;

        let mark = self.mark();
        self.push_child(0)?;
        self.expect(Punctuator::OpenParen, code::UNEXPECTED_TOKEN)?;
        if !getter {
            let parameter = self.parse_parameter()?;
            self.push_child(parameter)?;
        }
        self.expect(Punctuator::CloseParen, code::UNEXPECTED_TOKEN)?;
        let body = self.parse_block()?;
        if let Some(slot) = self.scratch.get_mut(mark) {
            *slot = body;
        }
        let (list, length) = self.close_list(mark)?;
        let start = self.node_start(key);
        let function = self.push(
            Node::new(NodeKind::Function, start, self.previous_end).with_payload(
                crate::arena::NONE,
                list,
                length,
            ),
        )?;
        self.leave();
        let kind = if getter {
            property_kind::GETTER
        } else {
            property_kind::SETTER
        };
        self.push(
            Node::new(NodeKind::Property, start, self.previous_end)
                .with_payload(key, function, kind),
        )
    }

    /// One property key: a computed key in brackets, or a name written as
    /// an identifier, a keyword, a string, or a number.
    fn parse_property_key(&mut self, token: &Token) -> Result<u32, Diagnostic> {
        let key = match token.kind {
            TokenKind::Punctuator(Punctuator::OpenBracket) => {
                self.bump(token);
                let expression = self.parse_assignment()?;
                let _ = self.expect(Punctuator::CloseBracket, code::EXPECTED_CLOSE_BRACKET)?;
                let end = self.previous_end;
                self.push(
                    Node::new(NodeKind::ComputedKey, token.start, end)
                        .with_payload(expression, 0, 0),
                )?
            }
            TokenKind::Identifier
            | TokenKind::Keyword(_)
            | TokenKind::String
            | TokenKind::Number => {
                self.bump(token);
                let written = match token.kind {
                    TokenKind::String => property_key::STRING,
                    TokenKind::Number => property_key::NUMBER,
                    _ => property_key::IDENTIFIER,
                };
                // A numeric key carries the value the lexer read, not the text
                // it was written as: what the property is called is that
                // number's own rendering, which `0x10` and `1e3` do not spell.
                let (first, second) = if written == property_key::NUMBER {
                    (self.push_number(token.number)?, 0)
                } else {
                    (token.inner_start, token.inner_end)
                };
                self.push(
                    Node::new(NodeKind::PropertyName, token.start, token.end)
                        .with_payload(first, second, written),
                )?
            }
            _ => {
                return Err(Diagnostic::new(
                    code::EXPECTED_PROPERTY_NAME,
                    Severity::Error,
                    token.start,
                    token.end.saturating_sub(token.start),
                ));
            }
        };
        Ok(key)
    }

    /// A template literal: alternating elements and substitutions, beginning
    /// and ending with an element.
    fn parse_template(&mut self) -> Result<u32, Diagnostic> {
        self.enter()?;
        let head = self.peek(Goal::Div)?;
        self.bump(&head);
        let mark = self.mark();
        let element = self.template_element(&head)?;
        self.push_child(element)?;

        let mut current = head;
        while matches!(
            current.kind,
            TokenKind::TemplateHead | TokenKind::TemplateMiddle
        ) {
            let substitution = self.parse_expression()?;
            self.push_child(substitution)?;

            let next = self.peek(Goal::TemplateTail)?;
            if !matches!(
                next.kind,
                TokenKind::TemplateMiddle | TokenKind::TemplateTail
            ) {
                self.scratch_length = mark;
                return Err(Diagnostic::new(
                    code::EXPECTED_CLOSE_BRACE,
                    Severity::Error,
                    next.start,
                    next.end.saturating_sub(next.start),
                ));
            }
            self.bump(&next);
            let element = self.template_element(&next)?;
            self.push_child(element)?;
            current = next;
        }

        let (list, length) = self.close_list(mark)?;
        self.leave();
        let end = self.previous_end;
        self.push(Node::new(NodeKind::Template, head.start, end).with_payload(list, length, 0))
    }

    fn template_element(&mut self, token: &Token) -> Result<u32, Diagnostic> {
        let flags = if token.cooked_valid {
            0
        } else {
            flag::COOKED_INVALID
        };
        self.push(
            Node::new(NodeKind::TemplateElement, token.start, token.end)
                .with_payload(token.inner_start, token.inner_end, token.code_units)
                .with_flags(flags),
        )
    }

    // Statements.

    /// `Script : StatementList`
    ///
    /// The whole source is one script node, and the value a script produces is
    /// the value of its last expression statement, which is what a caller sees.
    pub fn parse_script(&mut self) -> Result<u32, Diagnostic> {
        let mark = self.mark();
        loop {
            let token = self.peek(Goal::RegExp)?;
            if matches!(token.kind, TokenKind::EndOfSource) {
                break;
            }
            let statement = self.parse_statement()?;
            self.push_child(statement)?;
        }
        let (list, length) = self.close_list(mark)?;
        let end = self.previous_end;
        self.push(Node::new(NodeKind::Script, 0, end).with_payload(list, length, 0))
    }

    /// A statement, a declaration, or a labelled statement.
    fn parse_statement(&mut self) -> Result<u32, Diagnostic> {
        self.enter()?;
        let token = self.peek(Goal::RegExp)?;
        let node = match token.kind {
            TokenKind::Punctuator(Punctuator::OpenBrace) => self.parse_block()?,
            TokenKind::Punctuator(Punctuator::Semicolon) => {
                self.bump(&token);
                self.push(Node::new(NodeKind::Empty, token.start, token.end))?
            }
            TokenKind::Keyword(Keyword::Var) => {
                let node = self.parse_declaration(declaration::VAR)?;
                self.semicolon()?;
                node
            }
            TokenKind::Keyword(Keyword::Const) => {
                let node = self.parse_declaration(declaration::CONST)?;
                self.semicolon()?;
                node
            }
            TokenKind::Identifier if self.is_let_declaration(&token)? => {
                let node = self.parse_declaration(declaration::LET)?;
                self.semicolon()?;
                node
            }
            TokenKind::Keyword(Keyword::Function) => self.parse_function(true)?,
            TokenKind::Keyword(Keyword::If) => self.parse_if()?,
            TokenKind::Keyword(Keyword::While) => self.parse_while()?,
            TokenKind::Keyword(Keyword::Do) => self.parse_do_while()?,
            TokenKind::Keyword(Keyword::For) => self.parse_for()?,
            TokenKind::Keyword(Keyword::Return) => self.parse_return()?,
            TokenKind::Keyword(Keyword::Throw) => self.parse_throw()?,
            TokenKind::Keyword(Keyword::Break | Keyword::Continue) => {
                self.parse_break_continue()?
            }
            TokenKind::Keyword(Keyword::Try) => self.parse_try()?,
            TokenKind::Keyword(Keyword::Switch) => self.parse_switch()?,
            TokenKind::Keyword(Keyword::Debugger) => {
                self.bump(&token);
                self.semicolon()?;
                self.push(Node::new(
                    NodeKind::Debugger,
                    token.start,
                    self.previous_end,
                ))?
            }
            TokenKind::Keyword(Keyword::With) => {
                return Err(self.unsupported(&token, syntax_feature::STATEMENT));
            }
            TokenKind::Keyword(Keyword::Class) => {
                return Err(self.unsupported(&token, syntax_feature::CLASS_EXPRESSION));
            }
            TokenKind::Keyword(Keyword::Import) if self.module => self.parse_import()?,
            TokenKind::Keyword(Keyword::Export) if self.module => self.parse_export()?,
            TokenKind::Keyword(Keyword::Import | Keyword::Export) => {
                return Err(self.unsupported(&token, syntax_feature::IMPORT));
            }
            TokenKind::Identifier if self.is_label(&token)? => {
                let label = self.parse_identifier_name()?;
                let colon = self.peek(Goal::RegExp)?;
                self.bump(&colon);
                let body = self.parse_statement()?;
                self.push(
                    Node::new(NodeKind::Labelled, token.start, self.previous_end)
                        .with_payload(label, body, 0),
                )?
            }
            _ => {
                let expression = self.parse_expression()?;
                self.semicolon()?;
                let start = self.node_start(expression);
                self.push(
                    Node::new(NodeKind::ExpressionStatement, start, self.previous_end)
                        .with_payload(expression, 0, 0),
                )?
            }
        };
        self.leave();
        Ok(node)
    }

    /// `import ... from 'specifier';`
    fn parse_import(&mut self) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        let mark = self.mark();
        let token = self.peek(Goal::RegExp)?;

        // `import 'specifier'` runs a module for what it does, and binds
        // nothing.
        if matches!(token.kind, TokenKind::String) {
            let specifier = self.parse_string_literal()?;
            self.semicolon()?;
            let (list, length) = self.close_list(mark)?;
            return self.push(
                Node::new(NodeKind::Import, keyword.start, self.previous_end)
                    .with_payload(list, length, specifier),
            );
        }

        // `import name` binds the default export.
        if matches!(token.kind, TokenKind::Identifier) {
            let local = self.parse_binding_identifier()?;
            let clause = self.push(
                Node::new(NodeKind::ImportClause, token.start, self.previous_end).with_payload(
                    local,
                    crate::arena::NONE,
                    0,
                ),
            )?;
            self.push_child(clause)?;
            let next = self.peek(Goal::RegExp)?;
            if next.kind == TokenKind::Punctuator(Punctuator::Comma) {
                self.bump(&next);
            }
        }

        let token = self.peek(Goal::RegExp)?;
        match token.kind {
            // `import * as name`
            TokenKind::Punctuator(Punctuator::Star) => {
                self.bump(&token);
                let as_token = self.peek(Goal::RegExp)?;
                if !self.is_contextual(&as_token, b"as") {
                    return Err(self.unexpected(&as_token));
                }
                self.bump(&as_token);
                let local = self.parse_binding_identifier()?;
                let clause = self.push(
                    Node::new(NodeKind::ImportClause, token.start, self.previous_end)
                        .with_payload(local, crate::arena::NONE, 0)
                        .with_flags(flag::NAMESPACE),
                )?;
                self.push_child(clause)?;
            }
            // `import { a, b as c }`
            TokenKind::Punctuator(Punctuator::OpenBrace) => {
                self.bump(&token);
                loop {
                    let next = self.peek(Goal::RegExp)?;
                    if next.kind == TokenKind::Punctuator(Punctuator::CloseBrace) {
                        self.bump(&next);
                        break;
                    }
                    let imported = self.parse_binding_identifier()?;
                    let mut local = imported;
                    let as_token = self.peek(Goal::RegExp)?;
                    if self.is_contextual(&as_token, b"as") {
                        self.bump(&as_token);
                        local = self.parse_binding_identifier()?;
                    }
                    let clause = self.push(
                        Node::new(NodeKind::ImportClause, next.start, self.previous_end)
                            .with_payload(local, imported, 0),
                    )?;
                    self.push_child(clause)?;
                    let separator = self.peek(Goal::RegExp)?;
                    if separator.kind == TokenKind::Punctuator(Punctuator::Comma) {
                        self.bump(&separator);
                    }
                }
            }
            _ => {}
        }

        let from = self.peek(Goal::RegExp)?;
        if !self.is_contextual(&from, b"from") {
            return Err(self.unexpected(&from));
        }
        self.bump(&from);
        let specifier = self.parse_string_literal()?;
        self.semicolon()?;
        let (list, length) = self.close_list(mark)?;
        self.push(
            Node::new(NodeKind::Import, keyword.start, self.previous_end)
                .with_payload(list, length, specifier),
        )
    }

    /// `export ...`
    fn parse_export(&mut self) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        let token = self.peek(Goal::RegExp)?;
        let mark = self.mark();
        match token.kind {
            // `export { a, b as c };`
            TokenKind::Punctuator(Punctuator::OpenBrace) => {
                self.bump(&token);
                loop {
                    let next = self.peek(Goal::RegExp)?;
                    if next.kind == TokenKind::Punctuator(Punctuator::CloseBrace) {
                        self.bump(&next);
                        break;
                    }
                    let local = self.parse_binding_identifier()?;
                    let mut exported = local;
                    let as_token = self.peek(Goal::RegExp)?;
                    if self.is_contextual(&as_token, b"as") {
                        self.bump(&as_token);
                        exported = self.parse_binding_identifier()?;
                    }
                    let clause = self.push(
                        Node::new(NodeKind::ExportClause, next.start, self.previous_end)
                            .with_payload(local, exported, 0),
                    )?;
                    self.push_child(clause)?;
                    let separator = self.peek(Goal::RegExp)?;
                    if separator.kind == TokenKind::Punctuator(Punctuator::Comma) {
                        self.bump(&separator);
                    }
                }
                self.semicolon()?;
                let (list, length) = self.close_list(mark)?;
                self.push(
                    Node::new(NodeKind::Export, keyword.start, self.previous_end).with_payload(
                        crate::arena::NONE,
                        list,
                        length,
                    ),
                )
            }
            // `export default expression;`
            TokenKind::Keyword(Keyword::Default) => {
                self.bump(&token);
                let value = self.parse_assignment()?;
                self.semicolon()?;
                let (list, length) = self.close_list(mark)?;
                let _ = (list, length);
                self.push(
                    Node::new(NodeKind::Export, keyword.start, self.previous_end)
                        .with_payload(value, 0, 0)
                        .with_flags(flag::PREFIX),
                )
            }
            // `export const x = 1;`, `export function f() {}`
            _ => {
                let declaration = self.parse_statement()?;
                let (list, length) = self.close_list(mark)?;
                let _ = (list, length);
                self.push(
                    Node::new(NodeKind::Export, keyword.start, self.previous_end).with_payload(
                        declaration,
                        0,
                        0,
                    ),
                )
            }
        }
    }

    /// A string literal, as its own node.
    fn parse_string_literal(&mut self) -> Result<u32, Diagnostic> {
        let token = self.peek(Goal::RegExp)?;
        if !matches!(token.kind, TokenKind::String) {
            return Err(self.unexpected(&token));
        }
        self.bump(&token);
        self.push(
            Node::new(NodeKind::String, token.start, token.end).with_payload(
                token.inner_start,
                token.inner_end,
                token.code_units,
            ),
        )
    }

    /// Whether a token is an identifier spelling one of the words that are
    /// keywords only where they appear.
    fn is_contextual(&self, token: &Token, text: &[u8]) -> bool {
        matches!(token.kind, TokenKind::Identifier)
            && !token.escaped
            && self.token_text(token) == text
    }

    /// Whether an identifier token is the `let` that starts a declaration.
    ///
    /// `let` is not a keyword. It starts a declaration only when what follows
    /// can begin a binding, so `let x` declares and `let + 1` reads a variable.
    fn is_let_declaration(&mut self, token: &Token) -> Result<bool, Diagnostic> {
        if token.escaped || self.token_text(token) != b"let" {
            return Ok(false);
        }
        let after = self.peek_after(token)?;
        Ok(matches!(
            after.kind,
            TokenKind::Identifier | TokenKind::Keyword(Keyword::Yield | Keyword::Await)
        ))
    }

    /// Whether an identifier token starts a labelled statement.
    fn is_label(&mut self, token: &Token) -> Result<bool, Diagnostic> {
        let after = self.peek_after(token)?;
        Ok(after.kind == TokenKind::Punctuator(Punctuator::Colon))
    }

    /// The token after `token`, leaving `token` as the pending one.
    ///
    /// The lexer is driven forwards and then rewound to the token's own start,
    /// which is the only rewind target the scanner admits.
    fn peek_after(&mut self, token: &Token) -> Result<Token, Diagnostic> {
        self.pending = None;
        self.lexer.seek(token.end);
        // Both callers hand this an identifier, and after an identifier the
        // lexical goal is division: `r /= 2` continues the expression, and a
        // `/` here is never the start of a regular-expression literal.
        let next = self.lexer.next(Goal::Div)?;
        self.lexer.seek(token.start);
        self.pending = None;
        let again = self.lexer.next(Goal::RegExp)?;
        self.pending = Some((again, Goal::RegExp));
        Ok(next)
    }

    /// The bytes a token spans.
    fn token_text(&self, token: &Token) -> &[u8] {
        self.lexer
            .source()
            .get(token.start as usize..token.end as usize)
            .unwrap_or(&[])
    }

    /// Consume a statement's terminating semicolon, or insert one.
    ///
    /// A semicolon is inserted before a `}`, at the end of the source, and
    /// wherever a line terminator separates the offending token from what came
    /// before it. Nothing else is inserted, so a missing semicolon that no rule
    /// covers is still an error.
    fn semicolon(&mut self) -> Result<(), Diagnostic> {
        let token = self.peek(Goal::Div)?;
        if token.kind == TokenKind::Punctuator(Punctuator::Semicolon) {
            self.bump(&token);
            return Ok(());
        }
        if matches!(token.kind, TokenKind::EndOfSource)
            || token.kind == TokenKind::Punctuator(Punctuator::CloseBrace)
            || token.line_break_before
        {
            return Ok(());
        }
        Err(self.unexpected(&token))
    }

    fn parse_block(&mut self) -> Result<u32, Diagnostic> {
        let open = self.expect(Punctuator::OpenBrace, code::UNEXPECTED_TOKEN)?;
        let mark = self.mark();
        loop {
            let token = self.peek(Goal::RegExp)?;
            if token.kind == TokenKind::Punctuator(Punctuator::CloseBrace) {
                self.bump(&token);
                break;
            }
            if matches!(token.kind, TokenKind::EndOfSource) {
                return Err(self.unexpected(&token));
            }
            let statement = self.parse_statement()?;
            self.push_child(statement)?;
        }
        let (list, length) = self.close_list(mark)?;
        self.push(
            Node::new(NodeKind::Block, open.start, self.previous_end).with_payload(list, length, 0),
        )
    }

    /// `var`, `let`, or `const`, without its terminator.
    fn parse_declaration(&mut self, kind: u32) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        let mark = self.mark();
        loop {
            let name = self.parse_binding_identifier()?;
            let mut initialiser = crate::arena::NONE;
            let token = self.peek(Goal::Div)?;
            if token.kind == TokenKind::Punctuator(Punctuator::Assign) {
                self.bump(&token);
                initialiser = self.parse_assignment()?;
            } else if kind == declaration::CONST {
                // A `const` with no value can never be given one.
                return Err(Diagnostic::new(
                    code::MISSING_INITIALISER,
                    Severity::Error,
                    token.start,
                    token.end.saturating_sub(token.start),
                ));
            }
            let start = self.node_start(name);
            let declarator = self.push(
                Node::new(NodeKind::Declarator, start, self.previous_end).with_payload(
                    name,
                    initialiser,
                    0,
                ),
            )?;
            self.push_child(declarator)?;
            let token = self.peek(Goal::Div)?;
            if token.kind != TokenKind::Punctuator(Punctuator::Comma) {
                break;
            }
            self.bump(&token);
        }
        let (list, length) = self.close_list(mark)?;
        self.push(
            Node::new(NodeKind::Declaration, keyword.start, self.previous_end)
                .with_payload(list, length, kind),
        )
    }

    /// A name being bound, which may not be a reserved word.
    fn parse_binding_identifier(&mut self) -> Result<u32, Diagnostic> {
        let token = self.peek(Goal::RegExp)?;
        if !matches!(token.kind, TokenKind::Identifier) || token.spells_reserved {
            return Err(self.unexpected(&token));
        }
        self.bump(&token);
        self.push(
            Node::new(NodeKind::Identifier, token.start, token.end).with_payload(
                token.inner_start,
                token.inner_end,
                0,
            ),
        )
    }

    fn parse_identifier_name(&mut self) -> Result<u32, Diagnostic> {
        self.parse_binding_identifier()
    }

    fn parse_if(&mut self) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        self.expect(Punctuator::OpenParen, code::UNEXPECTED_TOKEN)?;
        let test = self.parse_expression()?;
        self.expect(Punctuator::CloseParen, code::UNEXPECTED_TOKEN)?;
        let consequent = self.parse_statement()?;
        let mut alternate = crate::arena::NONE;
        let token = self.peek(Goal::RegExp)?;
        if token.kind == TokenKind::Keyword(Keyword::Else) {
            self.bump(&token);
            alternate = self.parse_statement()?;
        }
        self.push(
            Node::new(NodeKind::If, keyword.start, self.previous_end)
                .with_payload(test, consequent, alternate),
        )
    }

    fn parse_while(&mut self) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        self.expect(Punctuator::OpenParen, code::UNEXPECTED_TOKEN)?;
        let test = self.parse_expression()?;
        self.expect(Punctuator::CloseParen, code::UNEXPECTED_TOKEN)?;
        let body = self.parse_statement()?;
        self.push(
            Node::new(NodeKind::While, keyword.start, self.previous_end)
                .with_payload(test, body, 0),
        )
    }

    fn parse_do_while(&mut self) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        let body = self.parse_statement()?;
        let while_token = self.peek(Goal::RegExp)?;
        if while_token.kind != TokenKind::Keyword(Keyword::While) {
            return Err(self.unexpected(&while_token));
        }
        self.bump(&while_token);
        self.expect(Punctuator::OpenParen, code::UNEXPECTED_TOKEN)?;
        let test = self.parse_expression()?;
        self.expect(Punctuator::CloseParen, code::UNEXPECTED_TOKEN)?;
        // A `do` statement's semicolon is optional whatever follows it.
        let token = self.peek(Goal::Div)?;
        if token.kind == TokenKind::Punctuator(Punctuator::Semicolon) {
            self.bump(&token);
        }
        self.push(
            Node::new(NodeKind::DoWhile, keyword.start, self.previous_end)
                .with_payload(body, test, 0),
        )
    }

    fn parse_for(&mut self) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        self.expect(Punctuator::OpenParen, code::UNEXPECTED_TOKEN)?;

        let mut initialiser = crate::arena::NONE;
        let token = self.peek(Goal::RegExp)?;
        if token.kind != TokenKind::Punctuator(Punctuator::Semicolon) {
            initialiser = if matches!(
                token.kind,
                TokenKind::Keyword(Keyword::Var | Keyword::Const)
            ) {
                let kind = if token.kind == TokenKind::Keyword(Keyword::Var) {
                    declaration::VAR
                } else {
                    declaration::CONST
                };
                self.parse_for_declaration(kind)?
            } else if matches!(token.kind, TokenKind::Identifier)
                && self.is_let_declaration(&token)?
            {
                self.parse_for_declaration(declaration::LET)?
            } else {
                self.parse_expression()?
            };

            // A bare-name head swallows `in` as the operator it also is:
            // `for (q in o)` parses as one binary expression. When that
            // expression sits right before the closing parenthesis, it is
            // the iteration header, split back into its two halves — unless
            // it was written in parentheses of its own, where `for ((q in o))`
            // is a head with no `;` and so no statement at all.
            let next = self.peek(Goal::Div)?;
            if next.kind == TokenKind::Punctuator(Punctuator::CloseParen)
                && !self.is_parenthesised(initialiser)
            {
                if let Some(node) = self.arena.node(initialiser).copied() {
                    if matches!(node.kind, NodeKind::Binary) && node.third == binop::IN {
                        self.bump(&next);
                        let body = self.parse_statement()?;
                        return self.push(
                            Node::new(NodeKind::ForInOf, keyword.start, self.previous_end)
                                .with_payload(node.first, node.second, body),
                        );
                    }
                }
            }
            let is_of = matches!(next.kind, TokenKind::Identifier)
                && !next.escaped
                && self.token_text(&next) == b"of";
            if next.kind == TokenKind::Keyword(Keyword::In) || is_of {
                self.bump(&next);
                let right = if is_of {
                    self.parse_assignment()?
                } else {
                    self.parse_expression()?
                };
                self.expect(Punctuator::CloseParen, code::UNEXPECTED_TOKEN)?;
                let body = self.parse_statement()?;
                let flags = if is_of { flag::OF } else { 0 };
                return self.push(
                    Node::new(NodeKind::ForInOf, keyword.start, self.previous_end)
                        .with_payload(initialiser, right, body)
                        .with_flags(flags),
                );
            }
        }
        self.expect(Punctuator::Semicolon, code::UNEXPECTED_TOKEN)?;

        let mut test = crate::arena::NONE;
        let token = self.peek(Goal::RegExp)?;
        if token.kind != TokenKind::Punctuator(Punctuator::Semicolon) {
            test = self.parse_expression()?;
        }
        self.expect(Punctuator::Semicolon, code::UNEXPECTED_TOKEN)?;

        let mut update = crate::arena::NONE;
        let token = self.peek(Goal::RegExp)?;
        if token.kind != TokenKind::Punctuator(Punctuator::CloseParen) {
            update = self.parse_expression()?;
        }
        self.expect(Punctuator::CloseParen, code::UNEXPECTED_TOKEN)?;
        let body = self.parse_statement()?;

        let mark = self.mark();
        self.push_child(test)?;
        self.push_child(update)?;
        self.push_child(body)?;
        let (list, length) = self.close_list(mark)?;
        self.push(
            Node::new(NodeKind::For, keyword.start, self.previous_end).with_payload(
                initialiser,
                list,
                length,
            ),
        )
    }

    /// A declaration in a `for` header, which has no terminator of its own.
    fn parse_for_declaration(&mut self, kind: u32) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        let mark = self.mark();
        loop {
            let name = self.parse_binding_identifier()?;
            let mut initialiser = crate::arena::NONE;
            let token = self.peek(Goal::Div)?;
            if token.kind == TokenKind::Punctuator(Punctuator::Assign) {
                self.bump(&token);
                initialiser = self.parse_assignment()?;
            }
            let start = self.node_start(name);
            let declarator = self.push(
                Node::new(NodeKind::Declarator, start, self.previous_end).with_payload(
                    name,
                    initialiser,
                    0,
                ),
            )?;
            self.push_child(declarator)?;
            let token = self.peek(Goal::Div)?;
            if token.kind != TokenKind::Punctuator(Punctuator::Comma) {
                break;
            }
            self.bump(&token);
        }
        let (list, length) = self.close_list(mark)?;
        self.push(
            Node::new(NodeKind::Declaration, keyword.start, self.previous_end)
                .with_payload(list, length, kind),
        )
    }

    fn parse_return(&mut self) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        let mut value = crate::arena::NONE;
        let token = self.peek(Goal::RegExp)?;
        // `return` is a restricted production: a line terminator after it ends
        // the statement, whatever follows on the next line.
        let ends = matches!(token.kind, TokenKind::EndOfSource)
            || token.kind == TokenKind::Punctuator(Punctuator::Semicolon)
            || token.kind == TokenKind::Punctuator(Punctuator::CloseBrace)
            || token.line_break_before;
        if !ends {
            value = self.parse_expression()?;
        }
        self.semicolon()?;
        self.push(
            Node::new(NodeKind::Return, keyword.start, self.previous_end).with_payload(value, 0, 0),
        )
    }

    fn parse_throw(&mut self) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        let token = self.peek(Goal::RegExp)?;
        if token.line_break_before {
            // `throw` with nothing on its line has nothing to throw.
            return Err(self.unexpected(&token));
        }
        let value = self.parse_expression()?;
        self.semicolon()?;
        self.push(
            Node::new(NodeKind::Throw, keyword.start, self.previous_end).with_payload(value, 0, 0),
        )
    }

    fn parse_break_continue(&mut self) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        let mut label = crate::arena::NONE;
        let token = self.peek(Goal::RegExp)?;
        if matches!(token.kind, TokenKind::Identifier)
            && !token.line_break_before
            && !token.spells_reserved
        {
            label = self.parse_identifier_name()?;
        }
        self.semicolon()?;
        let kind = if keyword.kind == TokenKind::Keyword(Keyword::Break) {
            NodeKind::Break
        } else {
            NodeKind::Continue
        };
        self.push(Node::new(kind, keyword.start, self.previous_end).with_payload(label, 0, 0))
    }

    fn parse_try(&mut self) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        let block = self.parse_block()?;
        let mut handler = crate::arena::NONE;
        let mut finaliser = crate::arena::NONE;
        let token = self.peek(Goal::RegExp)?;
        if token.kind == TokenKind::Keyword(Keyword::Catch) {
            self.bump(&token);
            let mut parameter = crate::arena::NONE;
            let next = self.peek(Goal::RegExp)?;
            if next.kind == TokenKind::Punctuator(Punctuator::OpenParen) {
                self.bump(&next);
                parameter = self.parse_binding_identifier()?;
                self.expect(Punctuator::CloseParen, code::UNEXPECTED_TOKEN)?;
            }
            let body = self.parse_block()?;
            handler = self.push(
                Node::new(NodeKind::Catch, token.start, self.previous_end)
                    .with_payload(parameter, body, 0),
            )?;
        }
        let token = self.peek(Goal::RegExp)?;
        if token.kind == TokenKind::Keyword(Keyword::Finally) {
            self.bump(&token);
            finaliser = self.parse_block()?;
        }
        if handler == crate::arena::NONE && finaliser == crate::arena::NONE {
            let token = self.peek(Goal::RegExp)?;
            return Err(self.unexpected(&token));
        }
        self.push(
            Node::new(NodeKind::Try, keyword.start, self.previous_end)
                .with_payload(block, handler, finaliser),
        )
    }

    fn parse_switch(&mut self) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        self.expect(Punctuator::OpenParen, code::UNEXPECTED_TOKEN)?;
        let discriminant = self.parse_expression()?;
        self.expect(Punctuator::CloseParen, code::UNEXPECTED_TOKEN)?;
        self.expect(Punctuator::OpenBrace, code::UNEXPECTED_TOKEN)?;

        let mark = self.mark();
        let mut seen_default = false;
        loop {
            let token = self.peek(Goal::RegExp)?;
            if token.kind == TokenKind::Punctuator(Punctuator::CloseBrace) {
                self.bump(&token);
                break;
            }
            let mut test = crate::arena::NONE;
            match token.kind {
                TokenKind::Keyword(Keyword::Case) => {
                    self.bump(&token);
                    test = self.parse_expression()?;
                }
                TokenKind::Keyword(Keyword::Default) => {
                    if seen_default {
                        // Two defaults would make the choice ambiguous.
                        return Err(self.unexpected(&token));
                    }
                    seen_default = true;
                    self.bump(&token);
                }
                _ => return Err(self.unexpected(&token)),
            }
            self.expect(Punctuator::Colon, code::UNEXPECTED_TOKEN)?;

            let body_mark = self.mark();
            loop {
                let next = self.peek(Goal::RegExp)?;
                if matches!(
                    next.kind,
                    TokenKind::Punctuator(Punctuator::CloseBrace)
                        | TokenKind::Keyword(Keyword::Case | Keyword::Default)
                ) {
                    break;
                }
                if matches!(next.kind, TokenKind::EndOfSource) {
                    return Err(self.unexpected(&next));
                }
                let statement = self.parse_statement()?;
                self.push_child(statement)?;
            }
            let (body, body_length) = self.close_list(body_mark)?;
            let case = self.push(
                Node::new(NodeKind::SwitchCase, token.start, self.previous_end).with_payload(
                    test,
                    body,
                    body_length,
                ),
            )?;
            self.push_child(case)?;
        }
        let (cases, length) = self.close_list(mark)?;
        self.push(
            Node::new(NodeKind::Switch, keyword.start, self.previous_end).with_payload(
                discriminant,
                cases,
                length,
            ),
        )
    }

    /// `function name(parameters) { body }`, as a declaration or an expression.
    fn parse_function(&mut self, declaration: bool) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        let token = self.peek(Goal::RegExp)?;
        if token.kind == TokenKind::Punctuator(Punctuator::Star) {
            return Err(self.unsupported(&token, syntax_feature::YIELD));
        }
        let mut name = crate::arena::NONE;
        if matches!(token.kind, TokenKind::Identifier) {
            name = self.parse_binding_identifier()?;
        } else if declaration {
            // A declaration must bind a name; an expression need not.
            return Err(self.unexpected(&token));
        }

        let mark = self.mark();
        // The body is the list's first entry, so a function needs no fourth
        // payload word. It is pushed once the parameters are known.
        self.push_child(0)?;
        let parameters_at = self.mark();
        self.expect(Punctuator::OpenParen, code::UNEXPECTED_TOKEN)?;
        loop {
            let token = self.peek(Goal::RegExp)?;
            if token.kind == TokenKind::Punctuator(Punctuator::CloseParen) {
                self.bump(&token);
                break;
            }
            if token.kind == TokenKind::Punctuator(Punctuator::Ellipsis) {
                return Err(self.unsupported(&token, syntax_feature::DESTRUCTURING));
            }
            let parameter = self.parse_parameter()?;
            self.push_child(parameter)?;
            let next = self.peek(Goal::RegExp)?;
            if next.kind == TokenKind::Punctuator(Punctuator::Comma) {
                self.bump(&next);
                continue;
            }
        }
        let _ = parameters_at;
        let body = self.parse_block()?;
        if let Some(slot) = self.scratch.get_mut(mark) {
            *slot = body;
        }
        let (list, length) = self.close_list(mark)?;
        let node = self.push(
            Node::new(NodeKind::Function, keyword.start, self.previous_end)
                .with_payload(name, list, length),
        )?;
        Ok(node)
    }

    fn parse_parameter(&mut self) -> Result<u32, Diagnostic> {
        let name = self.parse_binding_identifier()?;
        let token = self.peek(Goal::Div)?;
        if token.kind == TokenKind::Punctuator(Punctuator::Assign) {
            return Err(self.unsupported(&token, syntax_feature::DESTRUCTURING));
        }
        let start = self.node_start(name);
        self.push(Node::new(NodeKind::Parameter, start, self.previous_end).with_payload(name, 0, 0))
    }

    /// Build an arrow function from a head that has already been parsed as an
    /// expression, which is how the cover grammar is resolved.
    fn arrow_from(&mut self, head: u32, start: u32) -> Result<u32, Diagnostic> {
        let arrow = self.peek(Goal::Div)?;
        self.bump(&arrow);
        let mark = self.mark();
        self.push_child(0)?;
        if head != crate::arena::NONE {
            self.push_parameters_from(head)?;
        }
        let token = self.peek(Goal::RegExp)?;
        let (body, concise) = if token.kind == TokenKind::Punctuator(Punctuator::OpenBrace) {
            (self.parse_block()?, false)
        } else {
            (self.parse_assignment()?, true)
        };
        if let Some(slot) = self.scratch.get_mut(mark) {
            *slot = body;
        }
        let (list, length) = self.close_list(mark)?;
        let mut flags = flag::ARROW;
        if concise {
            flags |= flag::CONCISE_BODY;
        }
        self.push(
            Node::new(NodeKind::Function, start, self.previous_end)
                .with_payload(crate::arena::NONE, list, length)
                .with_flags(flags),
        )
    }

    /// Reinterpret a parsed expression as an arrow's parameter list.
    fn push_parameters_from(&mut self, head: u32) -> Result<(), Diagnostic> {
        let Some(node) = self.arena.node(head).copied() else {
            return Err(self.parameter_failure(head));
        };
        match node.kind {
            NodeKind::Identifier => {
                let parameter = self.push(
                    Node::new(NodeKind::Parameter, node.start, node.end).with_payload(head, 0, 0),
                )?;
                self.push_child(parameter)
            }
            NodeKind::Sequence => {
                let mut index = 0u32;
                while index < node.second {
                    let Some(&child) = self.arena.list(node.first, node.second).get(index as usize)
                    else {
                        break;
                    };
                    let Some(item) = self.arena.node(child).copied() else {
                        return Err(self.parameter_failure(head));
                    };
                    if !matches!(item.kind, NodeKind::Identifier) {
                        return Err(self.parameter_failure(child));
                    }
                    let parameter = self.push(
                        Node::new(NodeKind::Parameter, item.start, item.end)
                            .with_payload(child, 0, 0),
                    )?;
                    self.push_child(parameter)?;
                    index += 1;
                }
                Ok(())
            }
            _ => Err(self.parameter_failure(head)),
        }
    }

    /// Whether an already-parsed expression can be an arrow's head.
    fn is_arrow_head(&self, index: u32) -> bool {
        match self.arena.node(index) {
            Some(node) => match node.kind {
                NodeKind::Identifier => true,
                NodeKind::Sequence => node.has(flag::PARENTHESISED),
                _ => false,
            },
            None => false,
        }
    }

    fn parameter_failure(&self, index: u32) -> Diagnostic {
        let (start, end) = match self.arena.node(index) {
            Some(node) => (node.start, node.end),
            None => (self.previous_end, self.previous_end),
        };
        Diagnostic::new(
            code::INVALID_ARROW_PARAMETERS,
            Severity::Error,
            start,
            end.saturating_sub(start),
        )
    }

    fn node_start(&self, index: u32) -> u32 {
        match self.arena.node(index) {
            Some(node) => node.start,
            None => self.previous_end,
        }
    }

    fn is_unary_node(&self, index: u32) -> bool {
        match self.arena.node(index) {
            Some(node) => {
                matches!(node.kind, NodeKind::Unary)
                    || (matches!(node.kind, NodeKind::Update) && node.has(flag::PREFIX))
            }
            None => false,
        }
    }

    fn is_parenthesised(&self, index: u32) -> bool {
        self.arena
            .node(index)
            .is_some_and(|node| node.has(flag::PARENTHESISED))
    }

    fn is_unparenthesised_logical(&self, index: u32) -> bool {
        self.arena.node(index).is_some_and(|node| {
            matches!(node.kind, NodeKind::Logical)
                && node.third != binop::NULLISH
                && !node.has(flag::PARENTHESISED)
        })
    }
}

fn token_is_identifier(token: &Token) -> bool {
    matches!(token.kind, TokenKind::Identifier)
}

/// The operator, binding power, and whether the result is a logical node.
fn binary_operator(token: &Token) -> Option<(u32, u8, bool)> {
    let TokenKind::Punctuator(punctuator) = token.kind else {
        return match token.kind {
            TokenKind::Keyword(Keyword::Instanceof) => {
                Some((binop::INSTANCEOF, PRECEDENCE_RELATIONAL, false))
            }
            TokenKind::Keyword(Keyword::In) => Some((binop::IN, PRECEDENCE_RELATIONAL, false)),
            _ => None,
        };
    };
    let entry = match punctuator {
        Punctuator::QuestionQuestion => (binop::NULLISH, PRECEDENCE_NULLISH, true),
        Punctuator::PipePipe => (binop::LOGICAL_OR, PRECEDENCE_LOGICAL_OR, true),
        Punctuator::AmpersandAmpersand => (binop::LOGICAL_AND, PRECEDENCE_LOGICAL_AND, true),
        Punctuator::Pipe => (binop::BITWISE_OR, PRECEDENCE_BITWISE_OR, false),
        Punctuator::Caret => (binop::BITWISE_XOR, PRECEDENCE_BITWISE_XOR, false),
        Punctuator::Ampersand => (binop::BITWISE_AND, PRECEDENCE_BITWISE_AND, false),
        Punctuator::Equal => (binop::EQUAL, PRECEDENCE_EQUALITY, false),
        Punctuator::NotEqual => (binop::NOT_EQUAL, PRECEDENCE_EQUALITY, false),
        Punctuator::StrictEqual => (binop::STRICT_EQUAL, PRECEDENCE_EQUALITY, false),
        Punctuator::StrictNotEqual => (binop::STRICT_NOT_EQUAL, PRECEDENCE_EQUALITY, false),
        Punctuator::Less => (binop::LESS, PRECEDENCE_RELATIONAL, false),
        Punctuator::Greater => (binop::GREATER, PRECEDENCE_RELATIONAL, false),
        Punctuator::LessEqual => (binop::LESS_EQUAL, PRECEDENCE_RELATIONAL, false),
        Punctuator::GreaterEqual => (binop::GREATER_EQUAL, PRECEDENCE_RELATIONAL, false),
        Punctuator::ShiftLeft => (binop::SHIFT_LEFT, PRECEDENCE_SHIFT, false),
        Punctuator::ShiftRight => (binop::SHIFT_RIGHT, PRECEDENCE_SHIFT, false),
        Punctuator::UnsignedShiftRight => (binop::UNSIGNED_SHIFT_RIGHT, PRECEDENCE_SHIFT, false),
        Punctuator::Plus => (binop::ADD, PRECEDENCE_ADDITIVE, false),
        Punctuator::Minus => (binop::SUBTRACT, PRECEDENCE_ADDITIVE, false),
        Punctuator::Star => (binop::MULTIPLY, PRECEDENCE_MULTIPLICATIVE, false),
        Punctuator::Slash => (binop::DIVIDE, PRECEDENCE_MULTIPLICATIVE, false),
        Punctuator::Percent => (binop::REMAINDER, PRECEDENCE_MULTIPLICATIVE, false),
        Punctuator::StarStar => (binop::EXPONENT, PRECEDENCE_EXPONENT, false),
        _ => return None,
    };
    Some(entry)
}

/// The operator an assignment token denotes, if it is one.
fn assignment_operator(token: &Token) -> Option<u32> {
    let TokenKind::Punctuator(punctuator) = token.kind else {
        return None;
    };
    let operator = match punctuator {
        Punctuator::Assign => binop::ASSIGN,
        Punctuator::PlusAssign => binop::ADD,
        Punctuator::MinusAssign => binop::SUBTRACT,
        Punctuator::StarAssign => binop::MULTIPLY,
        Punctuator::SlashAssign => binop::DIVIDE,
        Punctuator::PercentAssign => binop::REMAINDER,
        Punctuator::StarStarAssign => binop::EXPONENT,
        Punctuator::ShiftLeftAssign => binop::SHIFT_LEFT,
        Punctuator::ShiftRightAssign => binop::SHIFT_RIGHT,
        Punctuator::UnsignedShiftRightAssign => binop::UNSIGNED_SHIFT_RIGHT,
        Punctuator::AmpersandAssign => binop::BITWISE_AND,
        Punctuator::PipeAssign => binop::BITWISE_OR,
        Punctuator::CaretAssign => binop::BITWISE_XOR,
        Punctuator::AmpersandAmpersandAssign => binop::LOGICAL_AND,
        Punctuator::PipePipeAssign => binop::LOGICAL_OR,
        Punctuator::QuestionQuestionAssign => binop::NULLISH,
        _ => return None,
    };
    Some(operator)
}
