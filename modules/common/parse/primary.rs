//! Primary expressions: literals, arrays, objects and their properties, templates, and decorators.

use super::*;

impl<'s, 't, 'a, 'k> Parser<'s, 't, 'a, 'k> {
    pub(super) fn parse_primary(&mut self) -> Result<u32, Diagnostic> {
        self.enter()?;
        let token = self.peek(Goal::RegExp)?;
        let node = match token.kind {
            TokenKind::Identifier if self.is_async_function(&token)? => {
                self.bump(&token);
                self.leave();
                return self.parse_function_of(false, true);
            }
            TokenKind::Identifier
                if !token.escaped
                    && self.token_text(&token) == b"async"
                    && matches!(self.peek_after(&token)?.kind, TokenKind::Identifier)
                    && !self.peek_after(&token)?.line_break_before =>
            {
                // `async x => …`: the only thing `async` followed by a name
                // can be is an async arrow's head.
                self.bump(&token);
                let head = self.parse_binding_identifier()?;
                let arrow = self.peek(Goal::Div)?;
                if arrow.kind != TokenKind::Punctuator(Punctuator::Arrow) {
                    return Err(self.unexpected(&arrow));
                }
                self.leave();
                return self.arrow_from_of(head, token.start, true);
            }
            TokenKind::Identifier => {
                if token.spells_reserved && !self.escaped_contextual(&token) {
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
                let flags = if token.flags & crate::lex::token_flag::LEGACY_OCTAL != 0 {
                    flag::LEGACY_OCTAL
                } else {
                    0
                };
                self.push(
                    Node::new(NodeKind::Number, token.start, token.end)
                        .with_payload(value, 0, 0)
                        .with_flags(flags),
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
                let flags = if token.flags & crate::lex::token_flag::LEGACY_OCTAL != 0 {
                    flag::LEGACY_OCTAL
                } else {
                    0
                };
                self.push(
                    Node::new(NodeKind::String, token.start, token.end)
                        .with_payload(token.inner_start, token.inner_end, token.code_units)
                        .with_flags(flags),
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
            TokenKind::Punctuator(Punctuator::OpenParen) => self.parse_parenthesised(&token)?,
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
                // `#x in o` is the brand check: whether the object carries
                // this class's private member.
                self.bump(&token);
                self.push(
                    Node::new(NodeKind::PrivateName, token.start, token.end).with_payload(
                        token.start,
                        token.end,
                        0,
                    ),
                )?
            }
            TokenKind::Keyword(Keyword::Function) => self.parse_function(false)?,
            TokenKind::Keyword(Keyword::Class) => {
                self.leave();
                return self.parse_class(false);
            }
            TokenKind::Punctuator(Punctuator::At) => {
                self.leave();
                return self.parse_decorated(false);
            }
            TokenKind::Keyword(Keyword::Await) => {
                if self.async_depth > 0 {
                    return Err(self.unsupported(&token, syntax_feature::ASYNC));
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
            TokenKind::Keyword(Keyword::Yield) => {
                if self.yield_depth > 0 {
                    return Err(self.unsupported(&token, syntax_feature::YIELD));
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
            TokenKind::Keyword(Keyword::Super) => self.parse_super(&token)?,
            TokenKind::Keyword(Keyword::Import) => self.parse_import_expression(&token)?,
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

    pub(super) fn parse_array(&mut self) -> Result<u32, Diagnostic> {
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

    pub(super) fn parse_object(&mut self) -> Result<u32, Diagnostic> {
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

    pub(super) fn parse_property(&mut self) -> Result<u32, Diagnostic> {
        self.enter()?;
        let token = self.peek(Goal::RegExp)?;
        if token.kind == TokenKind::Punctuator(Punctuator::Star) {
            // A generator method: the key, then the starred function.
            self.bump(&token);
            let key_token = self.peek(Goal::RegExp)?;
            let key = self.parse_property_key(&key_token)?;
            let start = token.start;
            let function = self.parse_method_function_of(start, false, true)?;
            self.leave();
            return self.push(
                Node::new(NodeKind::Property, start, self.previous_end).with_payload(
                    key,
                    function,
                    property_kind::METHOD,
                ),
            );
        }
        if token.kind == TokenKind::Punctuator(Punctuator::Ellipsis) {
            self.bump(&token);
            let value = self.parse_assignment()?;
            self.leave();
            let end = self.previous_end;
            return self
                .push(Node::new(NodeKind::Spread, token.start, end).with_payload(value, 0, 0));
        }

        // `async name(...)` and `async *name(...)` define async methods.
        if matches!(token.kind, TokenKind::Identifier)
            && !token.escaped
            && self.token_text(&token) == b"async"
        {
            let after = self.peek_after(&token)?;
            let names_method = !after.line_break_before
                && matches!(
                    after.kind,
                    TokenKind::Identifier
                        | TokenKind::Keyword(_)
                        | TokenKind::String
                        | TokenKind::Number
                        | TokenKind::Punctuator(Punctuator::OpenBracket | Punctuator::Star)
                );
            if names_method {
                self.bump(&token);
                let mut generator = false;
                let mut key_token = self.peek(Goal::RegExp)?;
                if key_token.kind == TokenKind::Punctuator(Punctuator::Star) {
                    self.bump(&key_token);
                    generator = true;
                    key_token = self.peek(Goal::RegExp)?;
                }
                let key = self.parse_property_key(&key_token)?;
                let start = token.start;
                let function = self.parse_method_function_of(start, true, generator)?;
                self.leave();
                return self.push(
                    Node::new(NodeKind::Property, start, self.previous_end).with_payload(
                        key,
                        function,
                        property_kind::METHOD,
                    ),
                );
            }
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
                // A method: the property's value is an anonymous function.
                let start = self.node_start(key);
                let function = self.parse_method_function(start, false)?;
                self.leave();
                self.push(
                    Node::new(NodeKind::Property, start, self.previous_end).with_payload(
                        key,
                        function,
                        property_kind::METHOD,
                    ),
                )
            }
            TokenKind::Punctuator(Punctuator::Assign) => {
                // `{ a = 1 }` is only ever a pattern: the shorthand keeps the
                // default, and an object literal that reaches the lowering
                // with one is refused there.
                let Some(node) = self.arena.node(key).copied() else {
                    return Err(self.unexpected(&next));
                };
                let contextual = (token.kind == TokenKind::Keyword(Keyword::Yield)
                    && self.yield_depth == 0)
                    || (token.kind == TokenKind::Keyword(Keyword::Await)
                        && self.async_depth == 0
                        && !self.module);
                if !matches!(node.kind, NodeKind::PropertyName)
                    || !(token_is_identifier(&token) || contextual)
                {
                    return Err(self.unexpected(&next));
                }
                let name = self.push(
                    Node::new(NodeKind::Identifier, node.start, node.end).with_payload(
                        node.first,
                        node.second,
                        0,
                    ),
                )?;
                self.bump(&next);
                let default = self.parse_assignment()?;
                self.leave();
                self.push(
                    Node::new(NodeKind::ShorthandProperty, node.start, self.previous_end)
                        .with_payload(name, default, 0),
                )
            }
            _ => {
                // Shorthand. Only a plain identifier may stand for both.
                let Some(node) = self.arena.node(key) else {
                    return Err(self.unexpected(&next));
                };
                let contextual = (token.kind == TokenKind::Keyword(Keyword::Yield)
                    && self.yield_depth == 0)
                    || (token.kind == TokenKind::Keyword(Keyword::Await)
                        && self.async_depth == 0
                        && !self.module);
                if !matches!(node.kind, NodeKind::PropertyName)
                    || !(token_is_identifier(&token) || contextual)
                {
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
                    Node::new(NodeKind::ShorthandProperty, start, end).with_payload(
                        name,
                        crate::arena::NONE,
                        0,
                    ),
                )
            }
        }
    }

    /// A class: its name, its heritage, and its members. The parser's only
    /// strictness duty is delegating: class code is strict, which the
    /// lowering enforces where strictness is decided.
    /// A decorator list and the class it decorates, in statement or
    /// expression position.
    pub(super) fn parse_decorated(&mut self, declaration: bool) -> Result<u32, Diagnostic> {
        let first = self.peek(Goal::RegExp)?;
        let mark = self.mark();
        let mut token = first;
        while token.kind == TokenKind::Punctuator(Punctuator::At) {
            self.bump(&token);
            let decorator = self.parse_decorator()?;
            self.push_child(decorator)?;
            token = self.peek(Goal::RegExp)?;
        }
        if token.kind != TokenKind::Keyword(Keyword::Class) {
            return Err(self.unexpected(&token));
        }
        let class = self.parse_class(declaration)?;
        let (list, length) = self.close_list(mark)?;
        self.push(
            Node::new(NodeKind::Decorated, first.start, self.previous_end)
                .with_payload(list, length, class),
        )
    }

    /// One decorator after its `@`: a parenthesised expression, or an
    /// identifier reference followed by `.name` or `.#name` links, with at
    /// most one call at the end.
    pub(super) fn parse_decorator(&mut self) -> Result<u32, Diagnostic> {
        let token = self.peek(Goal::RegExp)?;
        if token.kind == TokenKind::Punctuator(Punctuator::OpenParen) {
            self.bump(&token);
            let expression = self.parse_expression()?;
            self.expect(Punctuator::CloseParen, code::EXPECTED_CLOSE_PAREN)?;
            return Ok(expression);
        }
        // `yield` and `await` are names here wherever no generator or async
        // context claims them.
        let contextual = (token.kind == TokenKind::Keyword(Keyword::Yield)
            && self.yield_depth == 0)
            || (token.kind == TokenKind::Keyword(Keyword::Await) && self.async_depth == 0);
        if !(matches!(token.kind, TokenKind::Identifier) || contextual) {
            return Err(self.unexpected(&token));
        }
        self.bump(&token);
        let start = token.start;
        let mut expression = self.push(
            Node::new(NodeKind::Identifier, token.start, token.end).with_payload(
                token.inner_start,
                token.inner_end,
                0,
            ),
        )?;
        loop {
            let next = self.peek(Goal::Div)?;
            match next.kind {
                TokenKind::Punctuator(Punctuator::Dot) => {
                    self.bump(&next);
                    let name = self.parse_property_name_after_dot()?;
                    expression = self.push(
                        Node::new(NodeKind::Member, start, self.previous_end)
                            .with_payload(expression, name, 0),
                    )?;
                }
                TokenKind::Punctuator(Punctuator::OpenParen) => {
                    let (list, length) = self.parse_arguments()?;
                    return self.push(
                        Node::new(NodeKind::Call, start, self.previous_end)
                            .with_payload(expression, list, length),
                    );
                }
                _ => return Ok(expression),
            }
        }
    }

    /// One property key: a computed key in brackets, or a name written as
    /// an identifier, a keyword, a string, or a number.
    pub(super) fn parse_property_key(&mut self, token: &Token) -> Result<u32, Diagnostic> {
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
            TokenKind::BigInt => {
                // `{ 1n: x }`: the property is named by the digits, which
                // only a decimal literal spells as written.
                if token.radix != 10 {
                    return Err(self.unexpected(token));
                }
                self.bump(token);
                self.push(
                    Node::new(NodeKind::PropertyName, token.start, token.end).with_payload(
                        token.inner_start,
                        token.inner_end,
                        property_key::STRING,
                    ),
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
    pub(super) fn parse_template(&mut self) -> Result<u32, Diagnostic> {
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

    pub(super) fn template_element(&mut self, token: &Token) -> Result<u32, Diagnostic> {
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

    /// A string literal, as its own node.
    pub(super) fn parse_string_literal(&mut self) -> Result<u32, Diagnostic> {
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

    /// A parenthesised expression, or the parameter list of an arrow function, which is only known once the arrow is seen.
    pub(super) fn parse_parenthesised(&mut self, token: &Token) -> Result<u32, Diagnostic> {
        self.bump(token);
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
        // The parenthesis covers two grammars at once: an expression
        // — possibly a comma sequence — and an arrow's parameter
        // list, which alone admits a trailing comma and a rest.
        self.enter()?;
        let mark = self.mark();
        let mut count = 0u32;
        let mut arrow_only = false;
        loop {
            let next = self.peek(Goal::RegExp)?;
            if next.kind == TokenKind::Punctuator(Punctuator::CloseParen) {
                self.bump(&next);
                break;
            }
            if next.kind == TokenKind::Punctuator(Punctuator::Ellipsis) {
                self.bump(&next);
                let target = self.parse_binding_target()?;
                let rest = self.push(
                    Node::new(NodeKind::Spread, next.start, self.previous_end)
                        .with_payload(target, 0, 0),
                )?;
                self.push_child(rest)?;
                count += 1;
                arrow_only = true;
                let close = self.peek(Goal::Div)?;
                if close.kind != TokenKind::Punctuator(Punctuator::CloseParen) {
                    return Err(self.unexpected(&close));
                }
                self.bump(&close);
                break;
            }
            let item = self.parse_assignment()?;
            self.push_child(item)?;
            count += 1;
            let separator = self.peek(Goal::Div)?;
            match separator.kind {
                TokenKind::Punctuator(Punctuator::Comma) => {
                    self.bump(&separator);
                    let after = self.peek(Goal::RegExp)?;
                    if after.kind == TokenKind::Punctuator(Punctuator::CloseParen) {
                        self.bump(&after);
                        arrow_only = true;
                        break;
                    }
                }
                TokenKind::Punctuator(Punctuator::CloseParen) => {
                    self.bump(&separator);
                    break;
                }
                _ => {
                    return Err(Diagnostic::new(
                        code::EXPECTED_CLOSE_PAREN,
                        Severity::Error,
                        separator.start,
                        separator.end.saturating_sub(separator.start),
                    ));
                }
            }
        }
        let result = if count == 1 && !arrow_only {
            let Some(&only) = self.scratch.get(mark) else {
                return Err(self.unexpected(token));
            };
            self.scratch_length = mark;
            if let Some(node) = self.arena.node(only) {
                let marked = *node;
                self.push(marked.with_flags(marked.flags | flag::PARENTHESISED))?
            } else {
                only
            }
        } else {
            let (list, length) = self.close_list(mark)?;
            let sequence = self.push(
                Node::new(NodeKind::Sequence, token.start, self.previous_end)
                    .with_payload(list, length, 0)
                    .with_flags(flag::PARENTHESISED),
            )?;
            if arrow_only {
                let arrow = self.peek(Goal::Div)?;
                if arrow.kind != TokenKind::Punctuator(Punctuator::Arrow) {
                    return Err(self.unexpected(&arrow));
                }
            }
            sequence
        };
        self.leave();
        Ok(result)
    }

    /// `import(...)`, `import.defer(...)`, and `import.meta` in expression position.
    pub(super) fn parse_import_expression(&mut self, token: &Token) -> Result<u32, Diagnostic> {
        self.bump(token);
        let mut next = self.peek(Goal::Div)?;
        // `import.source(...)` and `import.defer(...)` are phase
        // imports: calls like `import(...)`, answered the same way.
        let mut phase_kind = 0u32;
        if next.kind == TokenKind::Punctuator(Punctuator::Dot) {
            self.bump(&next);
            let phase = self.peek(Goal::Div)?;
            if !matches!(phase.kind, TokenKind::Identifier)
                || phase.escaped
                || !matches!(self.token_text(&phase), b"source" | b"defer")
            {
                return Err(self.unsupported(&phase, syntax_feature::IMPORT));
            }
            phase_kind = if self.token_text(&phase) == b"defer" {
                1
            } else {
                2
            };
            self.bump(&phase);
            next = self.peek(Goal::Div)?;
        }
        if next.kind != TokenKind::Punctuator(Punctuator::OpenParen) {
            return Err(self.unsupported(&next, syntax_feature::IMPORT));
        }
        let (list, length) = self.parse_arguments()?;
        if length == 0 {
            return Err(self.unexpected(&next));
        }
        self.push(
            Node::new(NodeKind::ImportCall, token.start, self.previous_end)
                .with_payload(list, length, phase_kind),
        )
    }

    /// `super.name`, `super[key]`, and `super(...)`.
    pub(super) fn parse_super(&mut self, token: &Token) -> Result<u32, Diagnostic> {
        self.bump(token);
        let next = self.peek(Goal::Div)?;
        match next.kind {
            TokenKind::Punctuator(Punctuator::Dot) => {
                self.bump(&next);
                let name = self.peek(Goal::Div)?;
                if !matches!(name.kind, TokenKind::Identifier | TokenKind::Keyword(_)) {
                    return Err(self.unexpected(&name));
                }
                self.bump(&name);
                self.leave();
                self.push(
                    Node::new(NodeKind::SuperMember, token.start, name.end).with_payload(
                        name.inner_start,
                        name.inner_end,
                        0,
                    ),
                )
            }
            TokenKind::Punctuator(Punctuator::OpenParen) => {
                let (list, length) = self.parse_arguments()?;
                self.leave();
                self.push(
                    Node::new(NodeKind::SuperCall, token.start, self.previous_end)
                        .with_payload(list, length, 0),
                )
            }
            TokenKind::Punctuator(Punctuator::OpenBracket) => {
                self.bump(&next);
                let key = self.parse_expression()?;
                let close = self.peek(Goal::Div)?;
                if close.kind != TokenKind::Punctuator(Punctuator::CloseBracket) {
                    return Err(self.unexpected(&close));
                }
                self.bump(&close);
                self.leave();
                self.push(
                    Node::new(NodeKind::SuperIndex, token.start, self.previous_end)
                        .with_payload(key, 0, 0),
                )
            }
            _ => Err(self.unsupported(&next, syntax_feature::SUPER)),
        }
    }
}
