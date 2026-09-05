//! Statements and declarations, with the contextual keywords that decide between them.

use super::*;

impl<'s, 't, 'a, 'k> Parser<'s, 't, 'a, 'k> {
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
    /// A statement in a position that admits only a Statement, not a
    /// declaration: the body of `if`, a loop, `with`, or a label. There
    /// `let` is a name, so `if (x) let // ASI` reads a variable.
    pub(super) fn parse_embedded_statement(&mut self) -> Result<u32, Diagnostic> {
        self.embedded = true;
        let result = self.parse_statement();
        self.embedded = false;
        result
    }

    pub(super) fn parse_statement(&mut self) -> Result<u32, Diagnostic> {
        self.enter()?;
        let embedded = core::mem::replace(&mut self.embedded, false);
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
            TokenKind::Identifier if !embedded && self.is_let_declaration(&token)? => {
                let node = self.parse_declaration(declaration::LET)?;
                self.semicolon()?;
                node
            }
            TokenKind::Identifier if !embedded && self.is_using_declaration(&token)? => {
                // A `using` declaration stands in a block or a module's top
                // level, never a script's, an eval's, or a `case` clause's.
                if (self.block_depth == 0 && !self.module) || self.case_clause {
                    return Err(self.unexpected(&token));
                }
                let node = self.parse_declaration(declaration::USING)?;
                self.semicolon()?;
                node
            }
            TokenKind::Keyword(Keyword::Await) if !embedded && self.is_await_using(&token)? => {
                if (self.block_depth == 0 && !self.module) || self.case_clause {
                    return Err(self.unexpected(&token));
                }
                self.bump(&token);
                let node = self.parse_declaration(declaration::AWAIT_USING)?;
                self.semicolon()?;
                node
            }
            TokenKind::Identifier if self.is_async_function(&token)? => {
                self.bump(&token);
                self.parse_function_of(true, true)?
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
                self.bump(&token);
                self.expect(Punctuator::OpenParen, code::UNEXPECTED_TOKEN)?;
                let object = self.parse_expression()?;
                self.expect(Punctuator::CloseParen, code::EXPECTED_CLOSE_PAREN)?;
                let body = self.parse_embedded_statement()?;
                self.push(
                    Node::new(NodeKind::With, token.start, self.previous_end)
                        .with_payload(object, body, 0),
                )?
            }
            TokenKind::Keyword(Keyword::Class) => self.parse_class(true)?,
            TokenKind::Punctuator(Punctuator::At) => self.parse_decorated(true)?,
            TokenKind::Keyword(Keyword::Import) if self.import_is_expression(&token)? => {
                let expression = self.parse_expression()?;
                self.semicolon()?;
                let start = self.node_start(expression);
                self.push(
                    Node::new(NodeKind::ExpressionStatement, start, self.previous_end)
                        .with_payload(expression, 0, 0),
                )?
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
                let body = self.parse_embedded_statement()?;
                self.push(
                    Node::new(NodeKind::Labelled, token.start, self.previous_end)
                        .with_payload(label, body, 0),
                )?
            }
            TokenKind::Keyword(Keyword::Await)
                if !self.module && self.async_depth == 0 && self.is_label(&token)? =>
            {
                let label = self.parse_identifier_name()?;
                let colon = self.peek(Goal::RegExp)?;
                self.bump(&colon);
                let body = self.parse_embedded_statement()?;
                self.push(
                    Node::new(NodeKind::Labelled, token.start, self.previous_end)
                        .with_payload(label, body, 0),
                )?
            }
            TokenKind::Keyword(Keyword::Yield)
                if self.yield_depth == 0 && self.is_label(&token)? =>
            {
                let label = self.parse_identifier_name()?;
                let colon = self.peek(Goal::RegExp)?;
                self.bump(&colon);
                let body = self.parse_embedded_statement()?;
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

    /// Whether a token is an identifier spelling one of the words that are
    /// keywords only where they appear.
    pub(super) fn is_contextual(&self, token: &Token, text: &[u8]) -> bool {
        matches!(token.kind, TokenKind::Identifier)
            && !token.escaped
            && self.token_text(token) == text
    }

    /// Whether an identifier token is the `let` that starts a declaration.
    ///
    /// `let` is not a keyword. It starts a declaration only when what follows
    /// can begin a binding, so `let x` declares and `let + 1` reads a variable.
    pub(super) fn is_let_declaration(&mut self, token: &Token) -> Result<bool, Diagnostic> {
        if token.escaped || self.token_text(token) != b"let" {
            return Ok(false);
        }
        let after = self.peek_after(token)?;
        Ok(matches!(
            after.kind,
            TokenKind::Identifier
                | TokenKind::Keyword(Keyword::Yield | Keyword::Await)
                | TokenKind::Punctuator(Punctuator::OpenBracket | Punctuator::OpenBrace)
        ))
    }

    /// Whether an identifier token is the `using` that starts a declaration:
    /// `using` followed on the same line by a name, so `using[x]` and
    /// `using` alone before a line end are the variable they read.
    pub(super) fn is_using_declaration(&mut self, token: &Token) -> Result<bool, Diagnostic> {
        if token.escaped || self.token_text(token) != b"using" {
            return Ok(false);
        }
        let after = self.peek_after(token)?;
        if after.line_break_before {
            return Ok(false);
        }
        Ok(matches!(after.kind, TokenKind::Identifier)
            || (after.kind == TokenKind::Keyword(Keyword::Yield) && self.yield_depth == 0)
            || (after.kind == TokenKind::Keyword(Keyword::Await)
                && self.async_depth == 0
                && !self.module))
    }

    /// Whether an `await` token opens an `await using` declaration: async
    /// code, `using` on the same line, and a name on the line after that.
    pub(super) fn is_await_using(&mut self, token: &Token) -> Result<bool, Diagnostic> {
        if self.async_depth == 0 || token.escaped {
            return Ok(false);
        }
        let (using, name) = self.peek_two_after(token)?;
        if using.line_break_before
            || using.escaped
            || using.kind != TokenKind::Identifier
            || self.token_text(&using) != b"using"
        {
            return Ok(false);
        }
        Ok(!name.line_break_before
            && (matches!(name.kind, TokenKind::Identifier)
                || (name.kind == TokenKind::Keyword(Keyword::Yield) && self.yield_depth == 0)))
    }

    /// Whether `using` at the head of a `for` starts a declaration. `for
    /// (using of x)` reads the variable `using`: only `using of` followed by
    /// `=`, `;`, or `,` declares a resource named `of`.
    pub(super) fn using_heads_declaration(&mut self, token: &Token) -> Result<bool, Diagnostic> {
        let (after, second) = self.peek_two_after(token)?;
        if !self.is_contextual(&after, b"of") {
            return Ok(true);
        }
        Ok(matches!(
            second.kind,
            TokenKind::Punctuator(Punctuator::Assign | Punctuator::Semicolon | Punctuator::Comma)
        ))
    }

    /// The two tokens after `token`, leaving the stream where it was.
    pub(super) fn peek_two_after(&mut self, token: &Token) -> Result<(Token, Token), Diagnostic> {
        self.pending = None;
        self.lexer.seek(token.end);
        let first = self.lexer.next(Goal::Div)?;
        let second = self.lexer.next(Goal::Div)?;
        self.lexer.seek(token.start);
        self.pending = None;
        let again = self.lexer.next(Goal::RegExp)?;
        self.pending = Some((again, Goal::RegExp));
        Ok((first, second))
    }

    /// Whether an identifier token is the `async` that prefixes a function.
    /// A line terminator after `async` ends the restriction: what follows is
    /// then an ordinary statement or expression.
    pub(super) fn is_async_function(&mut self, token: &Token) -> Result<bool, Diagnostic> {
        if token.escaped || self.token_text(token) != b"async" {
            return Ok(false);
        }
        let after = self.peek_after(token)?;
        Ok(after.kind == TokenKind::Keyword(Keyword::Function) && !after.line_break_before)
    }

    /// Whether an identifier token starts a labelled statement.
    pub(super) fn is_label(&mut self, token: &Token) -> Result<bool, Diagnostic> {
        let after = self.peek_after(token)?;
        Ok(after.kind == TokenKind::Punctuator(Punctuator::Colon))
    }

    /// The token after `token`, leaving `token` as the pending one.
    ///
    /// The lexer is driven forwards and then rewound to the token's own start,
    /// which is the only rewind target the scanner admits.
    pub(super) fn peek_after(&mut self, token: &Token) -> Result<Token, Diagnostic> {
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
    pub(super) fn token_text(&self, token: &Token) -> &[u8] {
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
    pub(super) fn semicolon(&mut self) -> Result<(), Diagnostic> {
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

    pub(super) fn parse_block(&mut self) -> Result<u32, Diagnostic> {
        let open = self.expect(Punctuator::OpenBrace, code::UNEXPECTED_TOKEN)?;
        let in_case = self.case_clause;
        self.case_clause = false;
        self.block_depth += 1;
        let parsed = self.parse_block_body(open);
        self.block_depth -= 1;
        self.case_clause = in_case;
        parsed
    }

    pub(super) fn parse_block_body(&mut self, open: Token) -> Result<u32, Diagnostic> {
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
    pub(super) fn parse_declaration(&mut self, kind: u32) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        let mark = self.mark();
        loop {
            let name = self.parse_binding_target()?;
            let pattern = !matches!(
                self.arena.node(name).map(|node| node.kind),
                Some(NodeKind::Identifier)
            );
            let using = matches!(kind, declaration::USING | declaration::AWAIT_USING);
            if using && pattern {
                // A resource binds a name of its own, never a pattern.
                let token = self.peek(Goal::Div)?;
                return Err(self.unexpected(&token));
            }
            let mut initialiser = crate::arena::NONE;
            let token = self.peek(Goal::Div)?;
            if token.kind == TokenKind::Punctuator(Punctuator::Assign) {
                self.bump(&token);
                initialiser = self.parse_assignment()?;
            } else if kind == declaration::CONST || using || pattern {
                // A `const` with no value can never be given one, and a
                // pattern with no value has nothing to take apart.
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

    pub(super) fn parse_if(&mut self) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        self.expect(Punctuator::OpenParen, code::UNEXPECTED_TOKEN)?;
        let test = self.parse_expression()?;
        self.expect(Punctuator::CloseParen, code::UNEXPECTED_TOKEN)?;
        let consequent = self.parse_embedded_statement()?;
        let mut alternate = crate::arena::NONE;
        let token = self.peek(Goal::RegExp)?;
        if token.kind == TokenKind::Keyword(Keyword::Else) {
            self.bump(&token);
            alternate = self.parse_embedded_statement()?;
        }
        self.push(
            Node::new(NodeKind::If, keyword.start, self.previous_end)
                .with_payload(test, consequent, alternate),
        )
    }

    pub(super) fn parse_while(&mut self) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        self.expect(Punctuator::OpenParen, code::UNEXPECTED_TOKEN)?;
        let test = self.parse_expression()?;
        self.expect(Punctuator::CloseParen, code::UNEXPECTED_TOKEN)?;
        let body = self.parse_embedded_statement()?;
        self.push(
            Node::new(NodeKind::While, keyword.start, self.previous_end)
                .with_payload(test, body, 0),
        )
    }

    pub(super) fn parse_do_while(&mut self) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        let body = self.parse_embedded_statement()?;
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

    pub(super) fn parse_for(&mut self) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        // `for await` walks an async iterable, and only async code has it.
        let mut for_await = false;
        let next = self.peek(Goal::RegExp)?;
        if next.kind == TokenKind::Keyword(Keyword::Await) && self.async_depth > 0 {
            self.bump(&next);
            for_await = true;
        }
        self.expect(Punctuator::OpenParen, code::UNEXPECTED_TOKEN)?;

        let mut initialiser = crate::arena::NONE;
        let token = self.peek(Goal::RegExp)?;
        if token.kind != TokenKind::Punctuator(Punctuator::Semicolon) {
            let async_of = for_await
                && matches!(token.kind, TokenKind::Identifier)
                && !token.escaped
                && self.token_text(&token) == b"async"
                && {
                    let after = self.peek_after(&token)?;
                    self.is_contextual(&after, b"of")
                };
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
            } else if matches!(token.kind, TokenKind::Identifier)
                && self.is_using_declaration(&token)?
                && self.using_heads_declaration(&token)?
            {
                self.parse_for_declaration(declaration::USING)?
            } else if token.kind == TokenKind::Keyword(Keyword::Await)
                && self.is_await_using(&token)?
            {
                self.bump(&token);
                self.parse_for_declaration(declaration::AWAIT_USING)?
            } else if async_of {
                // `for await (async of …)`: the name, not an arrow's head.
                self.bump(&token);
                self.push(
                    Node::new(NodeKind::Identifier, token.start, token.end).with_payload(
                        token.inner_start,
                        token.inner_end,
                        0,
                    ),
                )?
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
                        let body = self.parse_embedded_statement()?;
                        return self.push(
                            Node::new(NodeKind::ForInOf, keyword.start, self.previous_end)
                                .with_payload(node.first, node.second, body),
                        );
                    }
                    // `for (x in a, b)`: the `in` binds inside the first entry
                    // of a sequence, and the rest of the sequence is the
                    // object expression's tail.
                    if matches!(node.kind, NodeKind::Sequence) && !node.has(flag::PARENTHESISED) {
                        let head = self
                            .arena
                            .list(node.first, node.second)
                            .first()
                            .copied()
                            .and_then(|entry| self.arena.node(entry).copied().map(|n| (entry, n)));
                        if let Some((_, first)) = head {
                            if matches!(first.kind, NodeKind::Binary)
                                && first.third == binop::IN
                                && !first.has(flag::PARENTHESISED)
                            {
                                let mark = self.mark();
                                self.push_child(first.second)?;
                                let mut index = 1u32;
                                while index < node.second {
                                    let Some(&entry) = self
                                        .arena
                                        .list(node.first, node.second)
                                        .get(index as usize)
                                    else {
                                        break;
                                    };
                                    self.push_child(entry)?;
                                    index += 1;
                                }
                                let (list_start, length) = self.close_list(mark)?;
                                let right = self.push(
                                    Node::new(NodeKind::Sequence, node.start, node.end)
                                        .with_payload(list_start, length, 0),
                                )?;
                                self.bump(&next);
                                let body = self.parse_embedded_statement()?;
                                return self.push(
                                    Node::new(NodeKind::ForInOf, keyword.start, self.previous_end)
                                        .with_payload(first.first, right, body),
                                );
                            }
                        }
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
                let body = self.parse_embedded_statement()?;
                let mut flags = if is_of { flag::OF } else { 0 };
                if for_await && is_of {
                    flags |= flag::FOR_AWAIT;
                }
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
        let body = self.parse_embedded_statement()?;

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
    pub(super) fn parse_for_declaration(&mut self, kind: u32) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        let mark = self.mark();
        loop {
            let name = self.parse_binding_target()?;
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

    pub(super) fn parse_return(&mut self) -> Result<u32, Diagnostic> {
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

    pub(super) fn parse_throw(&mut self) -> Result<u32, Diagnostic> {
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

    pub(super) fn parse_break_continue(&mut self) -> Result<u32, Diagnostic> {
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

    pub(super) fn parse_try(&mut self) -> Result<u32, Diagnostic> {
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
                parameter = self.parse_binding_target()?;
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

    pub(super) fn parse_switch(&mut self) -> Result<u32, Diagnostic> {
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
                self.case_clause = true;
                let statement = self.parse_statement();
                self.case_clause = false;
                self.push_child(statement?)?;
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
}
