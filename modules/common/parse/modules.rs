//! `import` and `export` declarations and their attributes.

use super::*;

impl<'s, 't, 'a, 'k> Parser<'s, 't, 'a, 'k> {
    /// `import ... from 'specifier';`
    pub(super) fn parse_import(&mut self) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        let mark = self.mark();
        let token = self.peek(Goal::RegExp)?;

        // `import 'specifier'` runs a module for what it does, and binds
        // nothing.
        if matches!(token.kind, TokenKind::String) {
            let specifier = self.parse_string_literal()?;
            let attributes = self.parse_import_attributes()?;
            self.semicolon()?;
            let (list, length) = self.close_list(mark)?;
            return self.push(
                Node::new(NodeKind::Import, keyword.start, self.previous_end)
                    .with_payload(list, length, specifier)
                    .with_flags(attributes),
            );
        }

        // `import defer * as name` binds the namespace with the module's
        // evaluation put off until the namespace is meaningfully used —
        // and only a `*` right after says `defer` is not a binding name.
        let mut deferred = false;
        let mut source_phase = false;
        if matches!(token.kind, TokenKind::Identifier) && self.is_contextual(&token, b"defer") {
            let after = self.peek_after(&token)?;
            if after.kind == TokenKind::Punctuator(Punctuator::Star) {
                self.bump(&token);
                deferred = true;
            }
        }
        // `import source name from ...` asks for a source-phase record —
        // while `import source from '...'` binds a default named `source`,
        // told apart by what follows the would-be binding.
        if matches!(token.kind, TokenKind::Identifier) && self.is_contextual(&token, b"source") {
            let after = self.peek_after(&token)?;
            if matches!(after.kind, TokenKind::Identifier) {
                let phase = if self.is_contextual(&after, b"from") {
                    let third = self.peek_after(&after)?;
                    // The chained look leaves the scanner at the second
                    // token; the first is the one still to be consumed.
                    self.pending = None;
                    self.lexer.seek(token.start);
                    matches!(third.kind, TokenKind::Identifier)
                } else {
                    true
                };
                if phase {
                    let again = self.peek(Goal::RegExp)?;
                    self.bump(&again);
                    source_phase = true;
                }
            }
        }
        let token = self.peek(Goal::RegExp)?;
        // `import name` binds the default export — or the source phase's
        // record, which no host here serves.
        if !deferred && matches!(token.kind, TokenKind::Identifier) {
            let local = self.parse_binding_identifier()?;
            let clause = self.push(
                Node::new(NodeKind::ImportClause, token.start, self.previous_end)
                    .with_payload(local, crate::arena::NONE, 0)
                    .with_flags(if source_phase { flag::SOURCE } else { 0 }),
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
                let flags = if deferred {
                    flag::NAMESPACE | flag::DEFER
                } else {
                    flag::NAMESPACE
                };
                let clause = self.push(
                    Node::new(NodeKind::ImportClause, token.start, self.previous_end)
                        .with_payload(local, crate::arena::NONE, 0)
                        .with_flags(flags),
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
                    // The imported name is any IdentifierName or string; one
                    // that is no plain identifier must be renamed with `as`.
                    let plain = matches!(next.kind, TokenKind::Identifier);
                    let imported = if matches!(next.kind, TokenKind::String) {
                        self.parse_string_literal()?
                    } else {
                        self.parse_any_name()?
                    };
                    let mut local = imported;
                    let as_token = self.peek(Goal::RegExp)?;
                    if self.is_contextual(&as_token, b"as") {
                        self.bump(&as_token);
                        local = self.parse_binding_identifier()?;
                    } else if !plain {
                        return Err(self.unexpected(&as_token));
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
        let attributes = self.parse_import_attributes()?;
        self.semicolon()?;
        let (list, length) = self.close_list(mark)?;
        self.push(
            Node::new(NodeKind::Import, keyword.start, self.previous_end)
                .with_payload(list, length, specifier)
                .with_flags(attributes),
        )
    }

    /// `export ...`
    pub(super) fn parse_export(&mut self) -> Result<u32, Diagnostic> {
        let keyword = self.peek(Goal::RegExp)?;
        self.bump(&keyword);
        let token = self.peek(Goal::RegExp)?;
        let mark = self.mark();
        match token.kind {
            // `export { a, b as c };` — with `from`, the names are another
            // module's, and any IdentifierName or string can carry them.
            TokenKind::Punctuator(Punctuator::OpenBrace) => {
                self.bump(&token);
                let mut needs_from = false;
                loop {
                    let next = self.peek(Goal::RegExp)?;
                    if next.kind == TokenKind::Punctuator(Punctuator::CloseBrace) {
                        self.bump(&next);
                        break;
                    }
                    let local = if matches!(next.kind, TokenKind::String) {
                        needs_from = true;
                        self.parse_string_literal()?
                    } else {
                        if !matches!(next.kind, TokenKind::Identifier) {
                            needs_from = true;
                        }
                        self.parse_any_name()?
                    };
                    let mut exported = local;
                    let as_token = self.peek(Goal::RegExp)?;
                    if self.is_contextual(&as_token, b"as") {
                        self.bump(&as_token);
                        exported = self.parse_export_name()?;
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
                let from = self.peek(Goal::RegExp)?;
                if self.is_contextual(&from, b"from") {
                    self.bump(&from);
                    let specifier = self.parse_string_literal()?;
                    let attributes = self.parse_import_attributes()?;
                    self.semicolon()?;
                    let (list, length) = self.close_list(mark)?;
                    return self.push(
                        Node::new(NodeKind::Export, keyword.start, self.previous_end)
                            .with_payload(specifier, list, length)
                            .with_flags(flag::OF | attributes),
                    );
                }
                if needs_from {
                    return Err(self.unexpected(&from));
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
            // `export * from ...;`, `export * as name from ...;`
            TokenKind::Punctuator(Punctuator::Star) => {
                self.bump(&token);
                let as_token = self.peek(Goal::RegExp)?;
                let name = if self.is_contextual(&as_token, b"as") {
                    self.bump(&as_token);
                    self.parse_export_name()?
                } else {
                    crate::arena::NONE
                };
                let clause = self.push(
                    Node::new(NodeKind::ExportClause, token.start, self.previous_end)
                        .with_payload(name, name, 0)
                        .with_flags(flag::NAMESPACE),
                )?;
                self.push_child(clause)?;
                let from = self.peek(Goal::RegExp)?;
                if !self.is_contextual(&from, b"from") {
                    return Err(self.unexpected(&from));
                }
                self.bump(&from);
                let specifier = self.parse_string_literal()?;
                let attributes = self.parse_import_attributes()?;
                self.semicolon()?;
                let (list, length) = self.close_list(mark)?;
                self.push(
                    Node::new(NodeKind::Export, keyword.start, self.previous_end)
                        .with_payload(specifier, list, length)
                        .with_flags(flag::OF | attributes),
                )
            }
            // `export default expression;`
            TokenKind::Keyword(Keyword::Default) => {
                self.bump(&token);
                // A default-exported function is a declaration: its name —
                // when it has one — is the module's own mutable binding.
                let next = self.peek(Goal::RegExp)?;
                let value = if next.kind == TokenKind::Keyword(Keyword::Function) {
                    self.anonymous_declaration = true;
                    let function = self.parse_function(true);
                    self.anonymous_declaration = false;
                    function?
                } else if self.is_async_function(&next)? {
                    self.bump(&next);
                    self.anonymous_declaration = true;
                    let function = self.parse_function_of(true, true);
                    self.anonymous_declaration = false;
                    function?
                } else {
                    self.parse_assignment()?
                };
                // A class or function body closes the export by itself.
                let declaration_form = self.arena.node(value).is_some_and(|node| {
                    matches!(node.kind, NodeKind::Class)
                        || (matches!(node.kind, NodeKind::Function) && !node.has(flag::ARROW))
                });
                if !declaration_form {
                    self.semicolon()?;
                }
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

    /// Whether `import` starts an expression — `import(...)` or
    /// `import.meta` — rather than a declaration.
    pub(super) fn import_is_expression(&mut self, token: &Token) -> Result<bool, Diagnostic> {
        let after = self.peek_after(token)?;
        Ok(matches!(
            after.kind,
            TokenKind::Punctuator(Punctuator::OpenParen | Punctuator::Dot)
        ))
    }

    /// An IdentifierName: any identifier or keyword, as a module's export
    /// and import names may be.
    /// `with { key: 'value', ... }` after a module specifier. Answers how
    /// the loader reads the module: 1 json, 2 text, 3 bytes, 0 for no
    /// attributes, and 4 for an attribute no loader here supports — which
    /// links to nothing, exactly as the specification's host would refuse.
    pub(super) fn parse_import_attributes(&mut self) -> Result<u8, Diagnostic> {
        let token = self.peek(Goal::Div)?;
        if token.kind != TokenKind::Keyword(Keyword::With) {
            return Ok(0);
        }
        self.bump(&token);
        let open = self.peek(Goal::RegExp)?;
        if open.kind != TokenKind::Punctuator(Punctuator::OpenBrace) {
            return Err(self.unexpected(&open));
        }
        self.bump(&open);
        let mut marker = 0u8;
        let mut unknown = false;
        let mut keys = [(0u32, 0u32); 16];
        let mut count = 0usize;
        loop {
            let next = self.peek(Goal::RegExp)?;
            if next.kind == TokenKind::Punctuator(Punctuator::CloseBrace) {
                self.bump(&next);
                break;
            }
            let key = match next.kind {
                TokenKind::Identifier | TokenKind::Keyword(_) | TokenKind::String => {
                    self.bump(&next);
                    (next.inner_start, next.inner_end)
                }
                _ => return Err(self.unexpected(&next)),
            };
            // The same key twice is the early error the grammar names.
            let (duplicate, is_type) = {
                let source = self.lexer.source();
                let text = source.get(key.0 as usize..key.1 as usize).unwrap_or(&[]);
                let mut duplicate = false;
                let mut held = 0usize;
                while held < count {
                    let (start, end) = keys[held];
                    if source.get(start as usize..end as usize).unwrap_or(&[]) == text {
                        duplicate = true;
                        break;
                    }
                    held += 1;
                }
                (duplicate, text == b"type")
            };
            if duplicate {
                return Err(Diagnostic::new(
                    code::DUPLICATE_BINDING,
                    Severity::Error,
                    next.start,
                    next.end.saturating_sub(next.start),
                ));
            }
            if count < keys.len() {
                keys[count] = key;
                count += 1;
            }
            let colon = self.peek(Goal::RegExp)?;
            if colon.kind != TokenKind::Punctuator(Punctuator::Colon) {
                return Err(self.unexpected(&colon));
            }
            self.bump(&colon);
            let value = self.peek(Goal::RegExp)?;
            if value.kind != TokenKind::String {
                return Err(self.unexpected(&value));
            }
            self.bump(&value);
            if is_type {
                marker = match self
                    .lexer
                    .source()
                    .get(value.inner_start as usize..value.inner_end as usize)
                    .unwrap_or(&[])
                {
                    b"json" => 1,
                    b"text" => 2,
                    b"bytes" => 3,
                    _ => 4,
                };
            } else {
                unknown = true;
            }
            let separator = self.peek(Goal::RegExp)?;
            if separator.kind == TokenKind::Punctuator(Punctuator::Comma) {
                self.bump(&separator);
            }
        }
        Ok(if unknown { 4 } else { marker })
    }

    /// An exported name: any IdentifierName, or a string literal.
    pub(super) fn parse_export_name(&mut self) -> Result<u32, Diagnostic> {
        let token = self.peek(Goal::RegExp)?;
        if matches!(token.kind, TokenKind::String) {
            return self.parse_string_literal();
        }
        self.parse_any_name()
    }

    pub(super) fn parse_any_name(&mut self) -> Result<u32, Diagnostic> {
        let token = self.peek(Goal::RegExp)?;
        if !matches!(token.kind, TokenKind::Identifier | TokenKind::Keyword(_)) {
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
}
