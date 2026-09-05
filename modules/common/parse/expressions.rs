//! Expressions by precedence: assignment, conditional, binary, unary, and the left-hand side with its calls and members.

use super::*;

/// Binding powers, from the loosest binary operator to the tightest.
pub(super) const PRECEDENCE_NULLISH: u8 = 1;

pub(super) const PRECEDENCE_LOGICAL_OR: u8 = 2;

pub(super) const PRECEDENCE_LOGICAL_AND: u8 = 3;

pub(super) const PRECEDENCE_BITWISE_OR: u8 = 4;

pub(super) const PRECEDENCE_BITWISE_XOR: u8 = 5;

pub(super) const PRECEDENCE_BITWISE_AND: u8 = 6;

pub(super) const PRECEDENCE_EQUALITY: u8 = 7;

pub(super) const PRECEDENCE_RELATIONAL: u8 = 8;

pub(super) const PRECEDENCE_SHIFT: u8 = 9;

pub(super) const PRECEDENCE_ADDITIVE: u8 = 10;

pub(super) const PRECEDENCE_MULTIPLICATIVE: u8 = 11;

pub(super) const PRECEDENCE_EXPONENT: u8 = 12;

/// The operator, binding power, and whether the result is a logical node.
pub(super) fn binary_operator(token: &Token) -> Option<(u32, u8, bool)> {
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
pub(super) fn assignment_operator(token: &Token) -> Option<u32> {
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

impl<'s, 't, 'a, 'k> Parser<'s, 't, 'a, 'k> {
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

    pub(super) fn parse_assignment(&mut self) -> Result<u32, Diagnostic> {
        let token = self.peek(Goal::RegExp)?;
        if token.kind == TokenKind::Keyword(Keyword::Yield) && self.yield_depth > 0 {
            self.bump(&token);
            let mut operator = unop::YIELD;
            let mut operand = crate::arena::NONE;
            let next = self.peek(Goal::RegExp)?;
            if next.kind == TokenKind::Punctuator(Punctuator::Star) && !next.line_break_before {
                self.bump(&next);
                operator = unop::YIELD_DELEGATE;
                operand = self.parse_assignment()?;
            } else {
                // `yield` alone yields undefined: anything that cannot start
                // an expression — or a line break — ends it.
                let starts = !next.line_break_before
                    && !matches!(
                        next.kind,
                        TokenKind::EndOfSource
                            | TokenKind::Punctuator(
                                Punctuator::Semicolon
                                    | Punctuator::CloseParen
                                    | Punctuator::CloseBracket
                                    | Punctuator::CloseBrace
                                    | Punctuator::Comma
                                    | Punctuator::Colon
                            )
                    );
                if starts {
                    operand = self.parse_assignment()?;
                }
            }
            let end = self.previous_end;
            return self.push(
                Node::new(NodeKind::Unary, token.start, end).with_payload(operand, 0, operator),
            );
        }
        self.enter()?;
        let left = self.parse_conditional()?;
        let token = self.peek(Goal::Div)?;

        if token.kind == TokenKind::Punctuator(Punctuator::Arrow) && !token.line_break_before {
            // The head was parsed as an expression; `=>` is what says it was a
            // parameter list all along. A line terminator before it cannot be
            // an arrow, because the arrow is a restricted production.
            let start = self.node_start(left);
            if self.arena.node(left).copied().is_some_and(|node| {
                matches!(node.kind, NodeKind::Call) && self.call_is_async_head(node)
            }) {
                // The call's arguments are the async arrow's parameters.
                self.leave();
                return self.async_arrow_from_call(left, start);
            }
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

    pub(super) fn check_assignment_target(
        &self,
        index: u32,
        token: &Token,
    ) -> Result<(), Diagnostic> {
        let Some(node) = self.arena.node(index) else {
            return Err(self.unexpected(token));
        };
        match node.kind {
            NodeKind::Identifier | NodeKind::SuperMember | NodeKind::SuperIndex => Ok(()),
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
            // The cover grammar: an array or object literal before a plain
            // `=` is a pattern, taken apart by the lowering. A compound
            // operator cannot read a pattern, so only `=` admits one.
            NodeKind::Array | NodeKind::Object
                if token.kind == TokenKind::Punctuator(Punctuator::Assign) =>
            {
                Ok(())
            }
            _ => Err(Diagnostic::new(
                code::INVALID_ASSIGNMENT_TARGET,
                Severity::Error,
                node.start,
                node.end.saturating_sub(node.start),
            )),
        }
    }

    pub(super) fn parse_conditional(&mut self) -> Result<u32, Diagnostic> {
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
    pub(super) fn parse_binary(&mut self, minimum: u8) -> Result<u32, Diagnostic> {
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

    pub(super) fn parse_unary(&mut self) -> Result<u32, Diagnostic> {
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
            TokenKind::Keyword(Keyword::Await) if self.async_depth > 0 => Some(unop::AWAIT),
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

    pub(super) fn check_update_target(&self, index: u32, token: &Token) -> Result<(), Diagnostic> {
        let Some(node) = self.arena.node(index) else {
            return Err(self.unexpected(token));
        };
        match node.kind {
            NodeKind::Identifier | NodeKind::SuperMember | NodeKind::SuperIndex => Ok(()),
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
    pub(super) fn parse_left_hand_side(&mut self) -> Result<u32, Diagnostic> {
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

    pub(super) fn parse_optional_link(
        &mut self,
        object: u32,
        start: u32,
    ) -> Result<u32, Diagnostic> {
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
    pub(super) fn parse_property_name_after_dot(&mut self) -> Result<u32, Diagnostic> {
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
            TokenKind::PrivateName => {
                self.bump(&token);
                self.push(
                    Node::new(NodeKind::PropertyName, token.start, token.end).with_payload(
                        token.start,
                        token.end,
                        property_key::IDENTIFIER,
                    ),
                )
            }
            _ => Err(Diagnostic::new(
                code::EXPECTED_PROPERTY_NAME,
                Severity::Error,
                token.start,
                token.end.saturating_sub(token.start),
            )),
        }
    }

    pub(super) fn parse_arguments(&mut self) -> Result<(u32, u32), Diagnostic> {
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

    pub(super) fn parse_new(&mut self, token: &Token) -> Result<u32, Diagnostic> {
        self.bump(token);
        let dot = self.peek(Goal::Div)?;
        if dot.kind == TokenKind::Punctuator(Punctuator::Dot) {
            self.bump(&dot);
            let target = self.peek(Goal::Div)?;
            if !matches!(target.kind, TokenKind::Identifier)
                || self.token_text(&target) != b"target"
                || target.escaped
            {
                return Err(self.unexpected(&target));
            }
            self.bump(&target);
            return self.push(Node::new(NodeKind::NewTarget, token.start, target.end));
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
                // A tagged template is a member expression too: `new tag\`x\``
                // constructs what the tag answers.
                TokenKind::NoSubstitutionTemplate | TokenKind::TemplateHead => {
                    let template = self.parse_template()?;
                    let end = self.previous_end;
                    callee = self.push(
                        Node::new(NodeKind::TaggedTemplate, start, end)
                            .with_payload(callee, template, 0),
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

    pub(super) fn node_start(&self, index: u32) -> u32 {
        match self.arena.node(index) {
            Some(node) => node.start,
            None => self.previous_end,
        }
    }

    pub(super) fn is_unary_node(&self, index: u32) -> bool {
        match self.arena.node(index) {
            Some(node) => matches!(node.kind, NodeKind::Unary),
            None => false,
        }
    }

    pub(super) fn is_parenthesised(&self, index: u32) -> bool {
        self.arena
            .node(index)
            .is_some_and(|node| node.has(flag::PARENTHESISED))
    }

    pub(super) fn is_unparenthesised_logical(&self, index: u32) -> bool {
        self.arena.node(index).is_some_and(|node| {
            matches!(node.kind, NodeKind::Logical)
                && node.third != binop::NULLISH
                && !node.has(flag::PARENTHESISED)
        })
    }
}
