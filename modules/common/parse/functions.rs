//! Functions, methods, parameters, and arrows recovered from what was first parsed as an expression.

use super::*;

impl<'s, 't, 'a, 'k> Parser<'s, 't, 'a, 'k> {
    /// `function name(parameters) { body }`, as a declaration or an expression.
    pub(super) fn parse_function(&mut self, declaration: bool) -> Result<u32, Diagnostic> {
        self.parse_function_of(declaration, false)
    }

    pub(super) fn parse_function_of(
        &mut self,
        declaration: bool,
        asynchronous: bool,
    ) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        // A function boundary decides `await` and `yield` afresh: operators
        // inside async functions and generators, names anywhere else.
        let saved = self.async_depth;
        let saved_yield = self.yield_depth;
        self.async_depth = u32::from(asynchronous);
        let result =
            self.parse_function_inner(declaration, asynchronous, keyword, (saved, saved_yield));
        self.async_depth = saved;
        self.yield_depth = saved_yield;
        result
    }

    pub(super) fn parse_function_inner(
        &mut self,
        declaration: bool,
        asynchronous: bool,
        keyword: Token,
        outer: (u32, u32),
    ) -> Result<u32, Diagnostic> {
        let mut token = self.peek(Goal::RegExp)?;
        let mut generator = false;
        if token.kind == TokenKind::Punctuator(Punctuator::Star) {
            self.bump(&token);
            generator = true;
            token = self.peek(Goal::RegExp)?;
        }
        self.yield_depth = u32::from(generator);
        let mut name = crate::arena::NONE;
        // A declaration's name belongs to the enclosing context — `async
        // function await() {}` in a script — while an expression's name is
        // read in the function's own: `yield` names a function that is not
        // a generator, `await` one that is not async.
        let (name_async, name_yield) = if declaration {
            outer
        } else {
            (self.async_depth, self.yield_depth)
        };
        let contextual_name = (token.kind == TokenKind::Keyword(Keyword::Yield) && name_yield == 0)
            || (token.kind == TokenKind::Keyword(Keyword::Await)
                && name_async == 0
                && !self.module);
        if matches!(token.kind, TokenKind::Identifier) || contextual_name {
            let (inner_async, inner_yield) = (self.async_depth, self.yield_depth);
            self.async_depth = name_async;
            self.yield_depth = name_yield;
            let parsed = self.parse_binding_identifier();
            self.async_depth = inner_async;
            self.yield_depth = inner_yield;
            name = parsed?;
        } else if declaration && !self.anonymous_declaration {
            // A declaration must bind a name; an expression need not —
            // nor a default export, whose name is `default` itself.
            return Err(self.unexpected(&token));
        }
        self.parse_function_tail_of(name, keyword.start, asynchronous, generator, declaration)
    }

    /// A function's parameter list and body, from the opening parenthesis:
    /// what a `function` keyword's tail and a method definition share.
    pub(super) fn parse_function_tail(
        &mut self,
        name: u32,
        start: u32,
        asynchronous: bool,
    ) -> Result<u32, Diagnostic> {
        self.parse_function_tail_of(name, start, asynchronous, false, false)
    }

    pub(super) fn parse_function_tail_of(
        &mut self,
        name: u32,
        start: u32,
        asynchronous: bool,
        generator: bool,
        declaration: bool,
    ) -> Result<u32, Diagnostic> {
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
                self.bump(&token);
                let target = self.parse_binding_target()?;
                let rest = self.push(
                    Node::new(NodeKind::RestElement, token.start, self.previous_end)
                        .with_payload(target, 0, 0),
                )?;
                let parameter =
                    self.push(
                        Node::new(NodeKind::Parameter, token.start, self.previous_end)
                            .with_payload(rest, crate::arena::NONE, parameter_kind::REST),
                    )?;
                self.push_child(parameter)?;
                let close = self.peek(Goal::Div)?;
                if close.kind != TokenKind::Punctuator(Punctuator::CloseParen) {
                    return Err(self.unexpected(&close));
                }
                self.bump(&close);
                break;
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
        let mut flags = if asynchronous { flag::ASYNC } else { 0 };
        if generator {
            flags |= flag::GENERATOR;
        }
        if declaration {
            flags |= flag::DECLARATION;
        }
        let node = self.push(
            Node::new(NodeKind::Function, start, self.previous_end)
                .with_payload(name, list, length)
                .with_flags(flags),
        )?;
        Ok(node)
    }

    /// A method definition's function: an anonymous function whose parameter
    /// list starts at the parenthesis already peeked.
    pub(super) fn parse_method_function(
        &mut self,
        start: u32,
        asynchronous: bool,
    ) -> Result<u32, Diagnostic> {
        self.parse_method_function_of(start, asynchronous, false)
    }

    pub(super) fn parse_method_function_of(
        &mut self,
        start: u32,
        asynchronous: bool,
        generator: bool,
    ) -> Result<u32, Diagnostic> {
        let saved = self.async_depth;
        let saved_yield = self.yield_depth;
        self.async_depth = u32::from(asynchronous);
        self.yield_depth = u32::from(generator);
        let result =
            self.parse_function_tail_of(crate::arena::NONE, start, asynchronous, generator, false);
        self.async_depth = saved;
        self.yield_depth = saved_yield;
        result
    }

    pub(super) fn parse_parameter(&mut self) -> Result<u32, Diagnostic> {
        let name = self.parse_binding_target()?;
        let mut initialiser = crate::arena::NONE;
        let token = self.peek(Goal::Div)?;
        if token.kind == TokenKind::Punctuator(Punctuator::Assign) {
            self.bump(&token);
            initialiser = self.parse_assignment()?;
        }
        let start = self.node_start(name);
        self.push(
            Node::new(NodeKind::Parameter, start, self.previous_end).with_payload(
                name,
                initialiser,
                parameter_kind::PLAIN,
            ),
        )
    }

    /// Build an arrow function from a head that has already been parsed as an
    /// expression, which is how the cover grammar is resolved.
    pub(super) fn arrow_from(&mut self, head: u32, start: u32) -> Result<u32, Diagnostic> {
        self.arrow_from_of(head, start, false)
    }

    pub(super) fn arrow_from_of(
        &mut self,
        head: u32,
        start: u32,
        asynchronous: bool,
    ) -> Result<u32, Diagnostic> {
        let arrow = self.peek(Goal::Div)?;
        self.bump(&arrow);
        let mark = self.mark();
        self.push_child(0)?;
        if head != crate::arena::NONE {
            self.push_parameters_from(head)?;
        }
        let saved = self.async_depth;
        let saved_yield = self.yield_depth;
        self.async_depth = u32::from(asynchronous);
        self.yield_depth = 0;
        let token = self.peek(Goal::RegExp)?;
        let outcome = if token.kind == TokenKind::Punctuator(Punctuator::OpenBrace) {
            self.parse_block().map(|body| (body, false))
        } else {
            self.parse_assignment().map(|body| (body, true))
        };
        self.async_depth = saved;
        self.yield_depth = saved_yield;
        let (body, concise) = outcome?;
        if let Some(slot) = self.scratch.get_mut(mark) {
            *slot = body;
        }
        let (list, length) = self.close_list(mark)?;
        let mut flags = flag::ARROW;
        if concise {
            flags |= flag::CONCISE_BODY;
        }
        if asynchronous {
            flags |= flag::ASYNC;
        }
        self.push(
            Node::new(NodeKind::Function, start, self.previous_end)
                .with_payload(crate::arena::NONE, list, length)
                .with_flags(flags),
        )
    }

    /// Reinterpret a parsed expression as an arrow's parameter list.
    pub(super) fn push_parameters_from(&mut self, head: u32) -> Result<(), Diagnostic> {
        let Some(node) = self.arena.node(head).copied() else {
            return Err(self.parameter_failure(head));
        };
        match node.kind {
            NodeKind::Sequence if node.has(flag::PARENTHESISED) => {
                let mut index = 0u32;
                while index < node.second {
                    let Some(&child) = self.arena.list(node.first, node.second).get(index as usize)
                    else {
                        break;
                    };
                    let parameter = self.parameter_from(child)?;
                    self.push_child(parameter)?;
                    index += 1;
                }
                Ok(())
            }
            _ => {
                let parameter = self.parameter_from(head)?;
                self.push_child(parameter)
            }
        }
    }

    /// One arrow parameter reinterpreted from an expression: a name or a
    /// pattern, with a default when a plain assignment wrapped it.
    pub(super) fn parameter_from(&mut self, index: u32) -> Result<u32, Diagnostic> {
        let Some(item) = self.arena.node(index).copied() else {
            return Err(self.parameter_failure(index));
        };
        match item.kind {
            NodeKind::Identifier => self.push(
                Node::new(NodeKind::Parameter, item.start, item.end).with_payload(
                    index,
                    crate::arena::NONE,
                    parameter_kind::PLAIN,
                ),
            ),
            NodeKind::Array | NodeKind::Object => {
                let target = self.pattern_from_expression(index)?;
                self.push(
                    Node::new(NodeKind::Parameter, item.start, item.end).with_payload(
                        target,
                        crate::arena::NONE,
                        parameter_kind::PLAIN,
                    ),
                )
            }
            NodeKind::Assign if item.third == binop::ASSIGN => {
                let target = self.pattern_from_expression(item.first)?;
                self.push(
                    Node::new(NodeKind::Parameter, item.start, item.end).with_payload(
                        target,
                        item.second,
                        parameter_kind::PLAIN,
                    ),
                )
            }
            NodeKind::Spread => {
                let target = self.pattern_from_expression(item.first)?;
                let rest = self.push(
                    Node::new(NodeKind::RestElement, item.start, item.end)
                        .with_payload(target, 0, 0),
                )?;
                self.push(
                    Node::new(NodeKind::Parameter, item.start, item.end).with_payload(
                        rest,
                        crate::arena::NONE,
                        parameter_kind::REST,
                    ),
                )
            }
            _ => Err(self.parameter_failure(index)),
        }
    }

    /// An expression re-read as a binding target: the cover grammar's array
    /// and object literals become patterns, element by element.
    pub(super) fn pattern_from_expression(&mut self, index: u32) -> Result<u32, Diagnostic> {
        let Some(item) = self.arena.node(index).copied() else {
            return Err(self.parameter_failure(index));
        };
        match item.kind {
            NodeKind::Identifier
            | NodeKind::ArrayPattern
            | NodeKind::ObjectPattern
            | NodeKind::Member
            | NodeKind::Index => Ok(index),
            NodeKind::Array => {
                let elements: [u32; 0] = [];
                let _ = elements;
                let list = item.first;
                let length = item.second;
                let mark = self.mark();
                let mut offset = 0u32;
                while offset < length {
                    let Some(&child) = self.arena.list(list, length).get(offset as usize) else {
                        break;
                    };
                    offset += 1;
                    let Some(node) = self.arena.node(child).copied() else {
                        return Err(self.parameter_failure(index));
                    };
                    let element = match node.kind {
                        NodeKind::Elision => child,
                        NodeKind::Spread => {
                            let target = self.pattern_from_expression(node.first)?;
                            self.push(
                                Node::new(NodeKind::RestElement, node.start, node.end)
                                    .with_payload(target, 0, 0),
                            )?
                        }
                        NodeKind::Assign if node.third == binop::ASSIGN => {
                            let target = self.pattern_from_expression(node.first)?;
                            self.push(
                                Node::new(NodeKind::BindingElement, node.start, node.end)
                                    .with_payload(target, node.second, 0),
                            )?
                        }
                        _ => {
                            let target = self.pattern_from_expression(child)?;
                            self.push(
                                Node::new(NodeKind::BindingElement, node.start, node.end)
                                    .with_payload(target, crate::arena::NONE, 0),
                            )?
                        }
                    };
                    self.push_child(element)?;
                }
                let (new_list, new_length) = self.close_list(mark)?;
                self.push(
                    Node::new(NodeKind::ArrayPattern, item.start, item.end)
                        .with_payload(new_list, new_length, 0),
                )
            }
            NodeKind::Object => {
                let list = item.first;
                let length = item.second;
                let mark = self.mark();
                let mut offset = 0u32;
                while offset < length {
                    let Some(&child) = self.arena.list(list, length).get(offset as usize) else {
                        break;
                    };
                    offset += 1;
                    let Some(node) = self.arena.node(child).copied() else {
                        return Err(self.parameter_failure(index));
                    };
                    let member = match node.kind {
                        NodeKind::Spread => {
                            let target = self.pattern_from_expression(node.first)?;
                            self.push(
                                Node::new(NodeKind::RestElement, node.start, node.end)
                                    .with_payload(target, 0, 0),
                            )?
                        }
                        NodeKind::ShorthandProperty => {
                            let element = self.push(
                                Node::new(NodeKind::BindingElement, node.start, node.end)
                                    .with_payload(node.first, node.second, 0),
                            )?;
                            let key = self
                                .arena
                                .node(node.first)
                                .copied()
                                .ok_or_else(|| self.parameter_failure(index))?;
                            let name = self.push(
                                Node::new(NodeKind::PropertyName, node.start, node.end)
                                    .with_payload(key.first, key.second, property_key::IDENTIFIER),
                            )?;
                            self.push(
                                Node::new(NodeKind::PatternProperty, node.start, node.end)
                                    .with_payload(name, element, 0),
                            )?
                        }
                        NodeKind::Property if node.third == property_kind::DATA => {
                            let (target, default) = match self.arena.node(node.second).copied() {
                                Some(value)
                                    if matches!(value.kind, NodeKind::Assign)
                                        && value.third == binop::ASSIGN =>
                                {
                                    (self.pattern_from_expression(value.first)?, value.second)
                                }
                                _ => (
                                    self.pattern_from_expression(node.second)?,
                                    crate::arena::NONE,
                                ),
                            };
                            let element = self.push(
                                Node::new(NodeKind::BindingElement, node.start, node.end)
                                    .with_payload(target, default, 0),
                            )?;
                            self.push(
                                Node::new(NodeKind::PatternProperty, node.start, node.end)
                                    .with_payload(node.first, element, 0),
                            )?
                        }
                        _ => return Err(self.parameter_failure(child)),
                    };
                    self.push_child(member)?;
                }
                let (new_list, new_length) = self.close_list(mark)?;
                self.push(
                    Node::new(NodeKind::ObjectPattern, item.start, item.end)
                        .with_payload(new_list, new_length, 0),
                )
            }
            _ => Err(self.parameter_failure(index)),
        }
    }

    /// Build an async arrow from `async (…)` parsed as a call.
    pub(super) fn async_arrow_from_call(
        &mut self,
        call: u32,
        start: u32,
    ) -> Result<u32, Diagnostic> {
        let Some(node) = self.arena.node(call).copied() else {
            return Err(self.parameter_failure(call));
        };
        let arrow = self.peek(Goal::Div)?;
        self.bump(&arrow);
        let mark = self.mark();
        self.push_child(0)?;
        let mut offset = 0u32;
        while offset < node.third {
            let Some(&argument) = self
                .arena
                .list(node.second, node.third)
                .get(offset as usize)
            else {
                break;
            };
            offset += 1;
            let parameter = self.parameter_from(argument)?;
            self.push_child(parameter)?;
        }
        let saved = self.async_depth;
        self.async_depth = 1;
        let token = self.peek(Goal::RegExp)?;
        let outcome = if token.kind == TokenKind::Punctuator(Punctuator::OpenBrace) {
            self.parse_block().map(|body| (body, false))
        } else {
            self.parse_assignment().map(|body| (body, true))
        };
        self.async_depth = saved;
        let (body, concise) = outcome?;
        if let Some(slot) = self.scratch.get_mut(mark) {
            *slot = body;
        }
        let (list, length) = self.close_list(mark)?;
        let mut flags = flag::ARROW | flag::ASYNC;
        if concise {
            flags |= flag::CONCISE_BODY;
        }
        self.push(
            Node::new(NodeKind::Function, start, self.previous_end)
                .with_payload(crate::arena::NONE, list, length)
                .with_flags(flags),
        )
    }

    /// Whether an already-parsed expression can be an arrow's head.
    pub(super) fn is_arrow_head(&self, index: u32) -> bool {
        match self.arena.node(index) {
            Some(node) => match node.kind {
                NodeKind::Identifier => true,
                NodeKind::Sequence | NodeKind::Array | NodeKind::Object => {
                    node.has(flag::PARENTHESISED)
                }
                NodeKind::Assign => node.has(flag::PARENTHESISED) && node.third == binop::ASSIGN,
                // `async (…)` parsed as a call is an async arrow's head when
                // `=>` follows.
                NodeKind::Call => self.call_is_async_head(*node),
                _ => false,
            },
            None => false,
        }
    }

    /// Whether a parsed call is `async (…)` — the cover an async arrow wears.
    pub(super) fn call_is_async_head(&self, node: Node) -> bool {
        match self.arena.node(node.first) {
            Some(callee) => {
                matches!(callee.kind, NodeKind::Identifier)
                    && self
                        .lexer
                        .source()
                        .get(callee.first as usize..callee.second as usize)
                        == Some(b"async")
            }
            None => false,
        }
    }

    pub(super) fn parameter_failure(&self, index: u32) -> Diagnostic {
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
}
