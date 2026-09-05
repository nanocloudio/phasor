//! Binding patterns and identifiers.

use super::*;

impl<'s, 't, 'a, 'k> Parser<'s, 't, 'a, 'k> {
    /// A binding target: a name, an array pattern, or an object pattern.
    pub(super) fn parse_binding_target(&mut self) -> Result<u32, Diagnostic> {
        let token = self.peek(Goal::RegExp)?;
        match token.kind {
            TokenKind::Punctuator(Punctuator::OpenBracket) => self.parse_array_pattern(),
            TokenKind::Punctuator(Punctuator::OpenBrace) => self.parse_object_pattern(),
            _ => self.parse_binding_identifier(),
        }
    }

    /// One target with its optional default.
    pub(super) fn parse_binding_element(&mut self) -> Result<u32, Diagnostic> {
        let target = self.parse_binding_target()?;
        let mut initialiser = crate::arena::NONE;
        let token = self.peek(Goal::Div)?;
        if token.kind == TokenKind::Punctuator(Punctuator::Assign) {
            self.bump(&token);
            initialiser = self.parse_assignment()?;
        }
        let start = self.node_start(target);
        self.push(
            Node::new(NodeKind::BindingElement, start, self.previous_end).with_payload(
                target,
                initialiser,
                0,
            ),
        )
    }

    /// `[a, , b = 1, ...rest]` as a target.
    pub(super) fn parse_array_pattern(&mut self) -> Result<u32, Diagnostic> {
        let open = self.peek(Goal::RegExp)?;
        self.bump(&open);
        self.enter()?;
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
                    let hole = self.push(Node::new(NodeKind::Elision, token.start, token.end))?;
                    self.push_child(hole)?;
                    continue;
                }
                TokenKind::Punctuator(Punctuator::Ellipsis) => {
                    self.bump(&token);
                    let target = self.parse_binding_target()?;
                    let rest = self.push(
                        Node::new(NodeKind::RestElement, token.start, self.previous_end)
                            .with_payload(target, 0, 0),
                    )?;
                    self.push_child(rest)?;
                    let close = self.peek(Goal::Div)?;
                    if close.kind != TokenKind::Punctuator(Punctuator::CloseBracket) {
                        self.scratch_length = mark;
                        return Err(self.unexpected(&close));
                    }
                    self.bump(&close);
                    break;
                }
                _ => {}
            }
            let element = self.parse_binding_element()?;
            self.push_child(element)?;
            let separator = self.peek(Goal::Div)?;
            match separator.kind {
                TokenKind::Punctuator(Punctuator::Comma) => self.bump(&separator),
                TokenKind::Punctuator(Punctuator::CloseBracket) => {
                    self.bump(&separator);
                    break;
                }
                _ => {
                    self.scratch_length = mark;
                    return Err(self.unexpected(&separator));
                }
            }
        }
        let (list, length) = self.close_list(mark)?;
        self.leave();
        self.push(
            Node::new(NodeKind::ArrayPattern, open.start, self.previous_end)
                .with_payload(list, length, 0),
        )
    }

    /// `{a, b: c = 1, [k]: d, ...rest}` as a target.
    pub(super) fn parse_object_pattern(&mut self) -> Result<u32, Diagnostic> {
        let open = self.peek(Goal::RegExp)?;
        self.bump(&open);
        self.enter()?;
        let mark = self.mark();
        loop {
            let token = self.peek(Goal::RegExp)?;
            if token.kind == TokenKind::Punctuator(Punctuator::CloseBrace) {
                self.bump(&token);
                break;
            }
            if token.kind == TokenKind::Punctuator(Punctuator::Ellipsis) {
                self.bump(&token);
                let target = self.parse_binding_identifier()?;
                let rest = self.push(
                    Node::new(NodeKind::RestElement, token.start, self.previous_end)
                        .with_payload(target, 0, 0),
                )?;
                self.push_child(rest)?;
            } else {
                let key = self.parse_property_key(&token)?;
                let next = self.peek(Goal::Div)?;
                let element = if next.kind == TokenKind::Punctuator(Punctuator::Colon) {
                    self.bump(&next);
                    self.parse_binding_element()?
                } else {
                    // Shorthand: the key is the name being bound, with an
                    // optional default.
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
                        self.scratch_length = mark;
                        return Err(self.unexpected(&next));
                    }
                    let target = self.push(
                        Node::new(NodeKind::Identifier, node.start, node.end).with_payload(
                            node.first,
                            node.second,
                            0,
                        ),
                    )?;
                    let mut initialiser = crate::arena::NONE;
                    if next.kind == TokenKind::Punctuator(Punctuator::Assign) {
                        self.bump(&next);
                        initialiser = self.parse_assignment()?;
                    }
                    self.push(
                        Node::new(NodeKind::BindingElement, node.start, self.previous_end)
                            .with_payload(target, initialiser, 0),
                    )?
                };
                let start = self.node_start(key);
                let property = self.push(
                    Node::new(NodeKind::PatternProperty, start, self.previous_end)
                        .with_payload(key, element, 0),
                )?;
                self.push_child(property)?;
            }
            let separator = self.peek(Goal::Div)?;
            match separator.kind {
                TokenKind::Punctuator(Punctuator::Comma) => self.bump(&separator),
                TokenKind::Punctuator(Punctuator::CloseBrace) => {
                    self.bump(&separator);
                    break;
                }
                _ => {
                    self.scratch_length = mark;
                    return Err(self.unexpected(&separator));
                }
            }
        }
        let (list, length) = self.close_list(mark)?;
        self.leave();
        self.push(
            Node::new(NodeKind::ObjectPattern, open.start, self.previous_end)
                .with_payload(list, length, 0),
        )
    }

    /// A name being bound, which may not be a reserved word.
    pub(super) fn parse_binding_identifier(&mut self) -> Result<u32, Diagnostic> {
        let token = self.peek(Goal::RegExp)?;
        // `yield` and `await` are names wherever no generator or async
        // context claims them as operators.
        let contextual = (token.kind == TokenKind::Keyword(Keyword::Yield)
            && self.yield_depth == 0)
            || (token.kind == TokenKind::Keyword(Keyword::Await) && self.async_depth == 0);
        if !(matches!(token.kind, TokenKind::Identifier) || contextual)
            || (token.spells_reserved && !self.escaped_contextual(&token))
        {
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

    pub(super) fn parse_identifier_name(&mut self) -> Result<u32, Diagnostic> {
        self.parse_binding_identifier()
    }
}
