//! Class declarations and expressions: members, accessors, and static blocks.

use super::*;

impl<'s, 't, 'a, 'k> Parser<'s, 't, 'a, 'k> {
    pub(super) fn parse_class(&mut self, declaration: bool) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        self.enter()?;
        let token = self.peek(Goal::RegExp)?;
        let mut name = crate::arena::NONE;
        // `await` names a class outside async code and modules; `yield`
        // never does, since class code is strict.
        let contextual_name = token.kind == TokenKind::Keyword(Keyword::Await)
            && self.async_depth == 0
            && !self.module;
        if matches!(token.kind, TokenKind::Identifier) || contextual_name {
            name = self.parse_binding_identifier()?;
        } else if declaration {
            return Err(self.unexpected(&token));
        }
        let mark = self.mark();
        // The heritage is the member list's first entry, or `NONE`.
        self.push_child(crate::arena::NONE)?;
        let token = self.peek(Goal::RegExp)?;
        if token.kind == TokenKind::Keyword(Keyword::Extends) {
            self.bump(&token);
            let heritage = self.parse_left_hand_side()?;
            if let Some(slot) = self.scratch.get_mut(mark) {
                *slot = heritage;
            }
        }
        self.expect(Punctuator::OpenBrace, code::UNEXPECTED_TOKEN)?;
        loop {
            let token = self.peek(Goal::RegExp)?;
            match token.kind {
                TokenKind::Punctuator(Punctuator::CloseBrace) => {
                    self.bump(&token);
                    break;
                }
                TokenKind::Punctuator(Punctuator::Semicolon) => {
                    self.bump(&token);
                    continue;
                }
                _ => {}
            }
            if token.kind == TokenKind::Punctuator(Punctuator::At) {
                // A decorator on the member that follows: its expression is
                // carried as a member of its own, evaluated in place.
                self.bump(&token);
                let start = token.start;
                let expression = self.parse_decorator()?;
                let member = self.push(
                    Node::new(NodeKind::ClassMember, start, self.previous_end).with_payload(
                        expression,
                        expression,
                        class_member::DECORATOR,
                    ),
                )?;
                self.push_child(member)?;
                continue;
            }
            let member = self.parse_class_member()?;
            self.push_child(member)?;
        }
        let (list, length) = self.close_list(mark)?;
        self.leave();
        self.push(
            Node::new(NodeKind::Class, keyword.start, self.previous_end)
                .with_payload(name, list, length),
        )
    }

    /// One class member: a method, an accessor, or the constructor, static
    /// or not. Fields and private names are outside the admitted grammar,
    /// refused by name.
    pub(super) fn parse_class_member(&mut self) -> Result<u32, Diagnostic> {
        let mut token = self.peek(Goal::RegExp)?;
        let start = token.start;
        let mut member_flags = 0u32;
        // `static` prefixes a member unless it names one: `static() {}`.
        if matches!(token.kind, TokenKind::Identifier)
            && !token.escaped
            && self.token_text(&token) == b"static"
        {
            let after = self.peek_after(&token)?;
            // `static;`, `static = 1`, `static() {}`, and `static }` name a
            // member `static`; anything else is the prefix.
            if !matches!(
                after.kind,
                TokenKind::Punctuator(
                    Punctuator::OpenParen
                        | Punctuator::Assign
                        | Punctuator::Semicolon
                        | Punctuator::CloseBrace
                )
            ) {
                self.bump(&token);
                member_flags |= class_member::STATIC;
                token = self.peek(Goal::RegExp)?;
                if token.kind == TokenKind::Punctuator(Punctuator::OpenBrace) {
                    // A static block: a body of its own, run once when the
                    // class is defined, carried as an anonymous function.
                    let function = self.parse_static_block(start)?;
                    return self.push(
                        Node::new(NodeKind::ClassMember, start, self.previous_end).with_payload(
                            crate::arena::NONE,
                            function,
                            class_member::STATIC_BLOCK | class_member::STATIC,
                        ),
                    );
                }
            }
        }
        let mut generator = false;
        if token.kind == TokenKind::Punctuator(Punctuator::Star) {
            self.bump(&token);
            generator = true;
            token = self.peek(Goal::RegExp)?;
        }

        // `accessor name` on the same line is an auto-accessor field; on a
        // line of its own, `accessor` is a field named that.
        let mut accessor = false;
        if matches!(token.kind, TokenKind::Identifier)
            && !token.escaped
            && self.token_text(&token) == b"accessor"
        {
            let after = self.peek_after(&token)?;
            let names_field = !after.line_break_before
                && matches!(
                    after.kind,
                    TokenKind::Identifier
                        | TokenKind::Keyword(_)
                        | TokenKind::String
                        | TokenKind::Number
                        | TokenKind::PrivateName
                        | TokenKind::Punctuator(Punctuator::OpenBracket)
                );
            if names_field {
                self.bump(&token);
                accessor = true;
                token = self.peek(Goal::RegExp)?;
            }
        }

        // `get name(...)` and `set name(...)` define accessors; `get` and
        // `set` followed by a parenthesis are ordinary method names.
        let mut kind = class_member::METHOD;
        if matches!(token.kind, TokenKind::Identifier)
            && !token.escaped
            && matches!(self.token_text(&token), b"get" | b"set")
        {
            let after = self.peek_after(&token)?;
            let names_member = matches!(
                after.kind,
                TokenKind::Identifier
                    | TokenKind::Keyword(_)
                    | TokenKind::String
                    | TokenKind::Number
                    | TokenKind::PrivateName
                    | TokenKind::Punctuator(Punctuator::OpenBracket)
            );
            if names_member {
                kind = if self.token_text(&token) == b"get" {
                    class_member::GETTER
                } else {
                    class_member::SETTER
                };
                self.bump(&token);
                token = self.peek(Goal::RegExp)?;
            }
        }
        let asynchronous = if matches!(token.kind, TokenKind::Identifier)
            && !token.escaped
            && self.token_text(&token) == b"async"
            && kind == class_member::METHOD
        {
            let after = self.peek_after(&token)?;
            let names_member = !after.line_break_before
                && matches!(
                    after.kind,
                    TokenKind::Identifier
                        | TokenKind::Keyword(_)
                        | TokenKind::String
                        | TokenKind::Number
                        | TokenKind::PrivateName
                        | TokenKind::Punctuator(Punctuator::OpenBracket | Punctuator::Star)
                );
            if names_member {
                self.bump(&token);
                token = self.peek(Goal::RegExp)?;
                if token.kind == TokenKind::Punctuator(Punctuator::Star) {
                    self.bump(&token);
                    generator = true;
                    token = self.peek(Goal::RegExp)?;
                }
                true
            } else {
                false
            }
        } else {
            false
        };
        let key = if token.kind == TokenKind::PrivateName {
            // A private name keys its member under its own spelling, `#`
            // included, which no ordinary property access can write.
            self.bump(&token);
            self.push(
                Node::new(NodeKind::PropertyName, token.start, token.end).with_payload(
                    token.start,
                    token.end,
                    property_key::IDENTIFIER,
                ),
            )?
        } else {
            self.parse_property_key(&token)?
        };
        // A field: a key followed by anything but a parenthesis — an
        // initialiser, a semicolon, a line end, or the closing brace.
        if kind == class_member::METHOD && !asynchronous {
            if let Some(field) = self.parse_class_field(start, key, accessor, member_flags)? {
                return Ok(field);
            }
        }
        // A member named `constructor` is the constructor, and only a plain
        // method may carry the name.
        if kind == class_member::METHOD
            && member_flags & class_member::STATIC == 0
            && !asynchronous
            && self
                .arena
                .node(key)
                .is_some_and(|node| matches!(node.kind, NodeKind::PropertyName))
        {
            let node = self
                .arena
                .node(key)
                .copied()
                .unwrap_or(Node::new(NodeKind::Null, 0, 0));
            // Compared inside the option rather than against `Some(b"...")`:
            // the latter needs a `&[u8]` built into a constant, which is a
            // pointer in a static, which is a relocation a loaded module
            // never gets. The optimizer folds it away at some levels and not
            // at others, and a segfault that depends on the optimizer is not
            // a thing to leave in the source.
            if node.third == property_key::IDENTIFIER
                && self
                    .lexer
                    .source()
                    .get(node.first as usize..node.second as usize)
                    .is_some_and(|name| name == b"constructor")
            {
                kind = class_member::CONSTRUCTOR;
            }
        }
        if generator && kind != class_member::METHOD {
            let token = self.peek(Goal::RegExp)?;
            return Err(self.unsupported(&token, syntax_feature::YIELD));
        }
        let function = self.parse_method_function_of(start, asynchronous, generator)?;
        self.push(
            Node::new(NodeKind::ClassMember, start, self.previous_end).with_payload(
                key,
                function,
                kind | member_flags,
            ),
        )
    }

    /// A class static block's body, as an anonymous function with no
    /// parameters. Inside it `await` is reserved — claimed as an operator
    /// the body may not use — and `yield` is a name.
    pub(super) fn parse_static_block(&mut self, start: u32) -> Result<u32, Diagnostic> {
        let mark = self.mark();
        self.push_child(0)?;
        let saved = self.async_depth;
        let saved_yield = self.yield_depth;
        self.async_depth = 1;
        self.yield_depth = 0;
        let body = self.parse_block();
        self.async_depth = saved;
        self.yield_depth = saved_yield;
        let body = body?;
        if let Some(slot) = self.scratch.get_mut(mark) {
            *slot = body;
        }
        let (list, length) = self.close_list(mark)?;
        self.push(
            Node::new(NodeKind::Function, start, self.previous_end).with_payload(
                crate::arena::NONE,
                list,
                length,
            ),
        )
    }

    /// One accessor property: the key, an empty or one-name parameter list,
    /// and a body, carried as an anonymous function the property points at.
    pub(super) fn parse_accessor(&mut self, getter: bool) -> Result<u32, Diagnostic> {
        let token = self.peek(Goal::RegExp)?;
        let key = self.parse_property_key(&token)?;

        let mark = self.mark();
        self.push_child(0)?;
        // A function boundary: neither `await` nor `yield` is an operator in
        // an accessor's parameter or body.
        let saved = self.async_depth;
        let saved_yield = self.yield_depth;
        self.async_depth = 0;
        self.yield_depth = 0;
        let parsed = self.parse_accessor_tail(getter);
        self.async_depth = saved;
        self.yield_depth = saved_yield;
        let body = parsed?;
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

    /// An accessor's parameter list and body, answering the body.
    pub(super) fn parse_accessor_tail(&mut self, getter: bool) -> Result<u32, Diagnostic> {
        self.expect(Punctuator::OpenParen, code::UNEXPECTED_TOKEN)?;
        if !getter {
            let parameter = self.parse_parameter()?;
            self.push_child(parameter)?;
        }
        self.expect(Punctuator::CloseParen, code::UNEXPECTED_TOKEN)?;
        self.parse_block()
    }

    /// Whether an escaped identifier spells `await` or `yield` where that
    /// word is a name rather than an operator.
    pub(super) fn escaped_contextual(&self, token: &Token) -> bool {
        if !token.escaped || !token.spells_reserved {
            return false;
        }
        let mut units = [0u16; 16];
        let Some(written) = crate::lex::cook(self.lexer.source(), token, &mut units) else {
            return false;
        };
        let mut text = [0u8; 16];
        let mut index = 0usize;
        while index < written {
            text[index] = u8::try_from(units[index]).unwrap_or(0);
            index += 1;
        }
        match crate::lex::keyword_of(&text[..written]) {
            Some(Keyword::Await) => self.async_depth == 0 && !self.module,
            Some(Keyword::Yield) => self.yield_depth == 0,
            _ => false,
        }
    }

    /// A class field, when the member turns out to be one: a key followed by
    /// anything but a parenthesis — an initialiser, a semicolon, a line end,
    /// or the closing brace. Answers `None` for a method.
    pub(super) fn parse_class_field(
        &mut self,
        start: u32,
        key: u32,
        accessor: bool,
        member_flags: u32,
    ) -> Result<Option<u32>, Diagnostic> {
        let next = self.peek(Goal::Div)?;
        if accessor && next.kind == TokenKind::Punctuator(Punctuator::OpenParen) {
            return Err(self.unexpected(&next));
        }
        if next.kind != TokenKind::Punctuator(Punctuator::OpenParen) {
            let mut initialiser = crate::arena::NONE;
            if next.kind == TokenKind::Punctuator(Punctuator::Assign) {
                self.bump(&next);
                // The initialiser runs per construction with `this`
                // bound, so it is carried as a body of its own.
                let mark = self.mark();
                self.push_child(0)?;
                let saved = self.async_depth;
                let saved_yield = self.yield_depth;
                self.async_depth = 0;
                self.yield_depth = 0;
                let body = self.parse_assignment();
                self.async_depth = saved;
                self.yield_depth = saved_yield;
                let body = body?;
                if let Some(slot) = self.scratch.get_mut(mark) {
                    *slot = body;
                }
                let (list, length) = self.close_list(mark)?;
                initialiser = self.push(
                    Node::new(NodeKind::Function, next.start, self.previous_end)
                        .with_payload(crate::arena::NONE, list, length)
                        .with_flags(flag::CONCISE_BODY),
                )?;
            }
            let after = self.peek(Goal::Div)?;
            if after.kind == TokenKind::Punctuator(Punctuator::Semicolon) {
                self.bump(&after);
            } else if !after.line_break_before
                && after.kind != TokenKind::Punctuator(Punctuator::CloseBrace)
            {
                return Err(self.unexpected(&after));
            }
            let field_kind = if accessor {
                class_member::ACCESSOR_FIELD
            } else {
                class_member::FIELD
            };
            return self
                .push(
                    Node::new(NodeKind::ClassMember, start, self.previous_end).with_payload(
                        key,
                        initialiser,
                        field_kind | member_flags,
                    ),
                )
                .map(Some);
        }
        Ok(None)
    }
}
