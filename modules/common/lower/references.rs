//! Names and references: resolving a name to a slot, an import, or the global object, and the read and write through it.

use super::*;

/// A prepared assignment target: the base register and, for an indexed
/// target, the key register — for a named one, the key constant.
pub(super) struct Reference {
    pub(super) object: u32,
    pub(super) key: u32,
}

impl Lowering<'_, '_, '_, '_> {
    /// Where a name written in the current scope resolves to.
    pub(super) fn resolve(&self, start: u32, end: u32) -> Resolved {
        let text = self.span(start, end);
        let mut scope = self.scope;
        let mut depth = 0u32;
        while scope != NONE {
            let record = self.program.scope(scope);
            let first = record.first as usize;
            let mut index = 0u32;
            while index < record.count {
                let Some(binding) = self.program.bindings.get(first + index as usize) else {
                    break;
                };
                if self.same_name(self.span(binding.start, binding.end), text) {
                    if record.parent == NONE
                        && !self.program.eval_goal
                        && !self.program.module
                        && matches!(binding.kind, binding_kind::LET | binding_kind::CONST)
                    {
                        // A script's top-level lexical lives in the global
                        // lexical environment, reached by name, so every
                        // script — and an indirect eval — sees the same one.
                        return Resolved::Global;
                    }
                    return Resolved::Slot {
                        depth,
                        slot: binding.slot,
                        kind: binding.kind,
                    };
                }
                index += 1;
            }
            if record.context {
                depth += 1;
            }
            scope = record.parent;
        }
        // A name this eval's own code `var`-declares binds fresh in the
        // caller's variable environment at run time: it must not resolve to
        // the older binding the site could see.
        let (eval_list, eval_length) = self.program.eval_var_statements;
        if eval_length != 0
            && eval_declared_name(self.arena, self.source, eval_list, eval_length, text, false)
                .is_some()
        {
            // Unless the variable environment already binds the name — then
            // the declaration re-uses that binding rather than making one.
            let existing = self.program.eval_scope.iter().any(|binding| {
                binding.name == text && binding.depth == self.program.eval_var_env_depth
            });
            if !existing {
                return Resolved::Global;
            }
        }
        // A name the eval site's record says is visible resolves into the
        // caller's environments: its depth is counted from the call site's
        // innermost context, which is exactly where this code's own chain
        // ran out.
        for binding in self.program.eval_scope {
            if binding.name == text {
                return Resolved::Slot {
                    depth: depth + binding.depth,
                    slot: binding.slot,
                    kind: u8::try_from(binding.kind).unwrap_or(binding_kind::VARIABLE),
                };
            }
        }
        Resolved::Global
    }

    /// Load a name, wherever it was declared.
    /// Strict code may not use a strict-mode reserved word as a name at
    /// all: the reference is the early SyntaxError the declaration is.
    pub(super) fn strict_reserved_guard(&mut self, node: &Node) -> bool {
        if !self.strict {
            return false;
        }
        let span = self.span(node.first, node.second);
        let reserved = matches!(
            span,
            b"implements"
                | b"interface"
                | b"let"
                | b"package"
                | b"private"
                | b"protected"
                | b"public"
                | b"static"
                | b"yield"
        );
        if reserved {
            self.fail(node, code::SYNTAX_NOT_ADMITTED);
        }
        reserved
    }

    pub(super) fn load_name(&mut self, node: &Node) {
        // A field initialiser — and any eval its site admits — may not
        // reference `arguments`: the early error the specification makes it.
        if self.deny_arguments && self.span(node.first, node.second) == b"arguments" {
            self.fail(node, code::SYNTAX_NOT_ADMITTED);
            return;
        }
        if self.strict_reserved_guard(node) {
            return;
        }
        match self.resolve(node.first, node.second) {
            Resolved::Slot {
                slot,
                kind: binding_kind::IMPORT,
                ..
            } => {
                // An import loads by its record's index in the module's
                // import table, which bare imports and re-exports share.
                let index = self.import_record_index(slot);
                self.emit(Opcode::LdaImport, &[i64::from(index)]);
            }
            Resolved::Slot { depth, slot, kind }
                if kind == binding_kind::VARIABLE
                    && depth >= 1
                    && (self.dynamic_names || self.with_depth > 0) =>
            {
                // A direct eval between here and the slot can declare a
                // nearer binding of this name at run time, which then wins.
                let constant = self.identifier_constant(node);
                self.note_depth(depth);
                self.emit(
                    Opcode::LdaShadowable,
                    &[i64::from(constant), i64::from(slot), i64::from(depth)],
                );
            }
            Resolved::Slot { depth, slot, .. } => {
                self.note_depth(depth);
                self.emit(Opcode::LdaContextSlot, &[i64::from(slot), i64::from(depth)]);
            }
            Resolved::Global => {
                let constant = self.identifier_constant(node);
                let opcode = if self.dynamic_names || self.with_depth > 0 {
                    Opcode::LdaDynamic
                } else {
                    Opcode::LdaGlobal
                };
                self.emit(opcode, &[i64::from(constant)]);
            }
        }
    }

    /// The position of an import binding among the module's imports: the
    /// import table is in declaration order, as the scope's bindings are.
    pub(super) fn import_index_of(&mut self, slot: u32) -> u32 {
        // Imports live only in the module's own top scope; a read from a
        // closure deep inside still loads by that scope's ordering.
        let mut scope = self.function_scope;
        loop {
            let parent = self.program.scope(scope).parent;
            if parent == NONE {
                break;
            }
            scope = parent;
        }
        let record = self.program.scope(scope);
        let first = record.first as usize;
        let mut index = 0u32;
        let mut counted = 0u32;
        while index < record.count {
            if let Some(binding) = self.program.bindings.get(first + index as usize) {
                if binding.kind == binding_kind::IMPORT {
                    if binding.slot == slot {
                        return counted;
                    }
                    counted += 1;
                }
            }
            index += 1;
        }
        counted
    }

    /// The index of an import binding's record in the module's import table.
    /// The table also holds records no binding names — bare imports and
    /// re-exports, whose slot stays unwritten — so the binding's position
    /// among its kind is mapped over the records that do carry one.
    pub(super) fn import_record_index(&mut self, slot: u32) -> u32 {
        let ordinal = self.import_index_of(slot);
        let mut seen = 0u32;
        let mut index = 0usize;
        while index < self.program.import_count {
            if let Some(record) = self.program.imports.get(index) {
                if record.slot != u32::MAX {
                    if seen == ordinal {
                        return u32::try_from(index).unwrap_or(ordinal);
                    }
                    seen += 1;
                }
            }
            index += 1;
        }
        ordinal
    }

    /// Store the accumulator into a name. Assigning to a `const` is refused
    /// here, because a program that does it can never be right.
    pub(super) fn store_name(&mut self, node: &Node) {
        // Strict code refuses to assign the names `eval` and `arguments`,
        // whatever they resolve to.
        if self.strict && matches!(self.span(node.first, node.second), b"eval" | b"arguments") {
            self.fail(node, code::STRICT_ASSIGNMENT_TO_RESTRICTED_NAME);
            return;
        }
        if self.strict_reserved_guard(node) {
            return;
        }
        match self.resolve(node.first, node.second) {
            Resolved::Slot { depth, slot, kind } => {
                if kind == binding_kind::IMPORT {
                    // An imported name belongs to the module that exports
                    // it: the write parses, evaluates its value, and throws
                    // the TypeError at run time.
                    self.emit(Opcode::ThrowSelfAssignment, &[]);
                    return;
                }
                if kind == binding_kind::CONST {
                    // Assigning a constant evaluates its value and then
                    // throws the TypeError, at run time.
                    self.emit(Opcode::ThrowSelfAssignment, &[]);
                    return;
                }
                if kind == binding_kind::SELF {
                    // A named function expression's own name is immutable:
                    // strict code refuses the write, sloppy discards it.
                    if self.strict {
                        self.emit(Opcode::ThrowSelfAssignment, &[]);
                    }
                    return;
                }
                self.note_depth(depth);
                if kind == binding_kind::VARIABLE
                    && depth >= 1
                    && (self.dynamic_names || self.with_depth > 0)
                {
                    let constant = self.identifier_constant(node);
                    self.emit(
                        Opcode::StaShadowable,
                        &[i64::from(constant), i64::from(slot), i64::from(depth)],
                    );
                } else {
                    self.emit(Opcode::StaContextSlot, &[i64::from(slot), i64::from(depth)]);
                }
            }
            Resolved::Global => {
                let constant = self.identifier_constant(node);
                // Strict code assigns only what exists; sloppy code creates.
                let opcode = if self.strict {
                    Opcode::StaGlobalStrict
                } else if self.dynamic_names || self.with_depth > 0 {
                    Opcode::StaDynamic
                } else {
                    Opcode::StaGlobal
                };
                self.emit(opcode, &[i64::from(constant)]);
            }
        }
    }

    /// Give a declared name its first value.
    pub(super) fn initialise_name(&mut self, node: &Node) {
        match self.resolve(node.first, node.second) {
            Resolved::Slot { depth, slot, .. } => {
                self.note_depth(depth);
                self.emit(
                    Opcode::InitContextSlot,
                    &[i64::from(slot), i64::from(depth)],
                );
            }
            Resolved::Global => {
                let constant = self.identifier_constant(node);
                if !self.program.eval_goal && !self.program.module {
                    // A script's top-level lexical: its global binding takes
                    // the value and leaves its dead zone.
                    self.emit(Opcode::InitGlobalLexical, &[i64::from(constant)]);
                    return;
                }
                let opcode = if self.dynamic_names || self.with_depth > 0 {
                    Opcode::StaDynamic
                } else {
                    Opcode::StaGlobal
                };
                self.emit(opcode, &[i64::from(constant)]);
            }
        }
    }

    /// Evaluate a target's base and key once, into registers a read and a
    /// write both use. A plain name needs no registers at all.
    /// Whether a name resolves to a slot a run-time eval binding could
    /// shadow, and the pieces the prepared-reference instructions need.
    pub(super) fn shadowable_slot(&mut self, node: &Node) -> Option<(u32, u32, u32)> {
        if !self.dynamic_names && self.with_depth == 0 {
            return None;
        }
        match self.resolve(node.first, node.second) {
            Resolved::Slot { depth, slot, kind }
                if kind == binding_kind::VARIABLE && depth >= 1 =>
            {
                let constant = self.identifier_constant(node);
                self.note_depth(depth);
                Some((constant, slot, depth))
            }
            // A free name: its reference may capture a `with` object or the
            // global object, marked by the depth no chain reaches.
            Resolved::Global => {
                let constant = self.identifier_constant(node);
                Some((constant, 0, u32::MAX))
            }
            _ => None,
        }
    }

    pub(super) fn prepare_reference(&mut self, target: &Node) -> Reference {
        match target.kind {
            NodeKind::Member => {
                let object = self.allocate();
                self.expression(target.first);
                self.emit(Opcode::Star, &[i64::from(object)]);
                Reference {
                    object,
                    key: {
                        self.private_member_guard(target.second);
                        self.key_constant(target.second)
                    },
                }
            }
            NodeKind::Index => {
                let object = self.allocate();
                self.expression(target.first);
                self.emit(Opcode::Star, &[i64::from(object)]);
                let key = self.allocate();
                self.expression(target.second);
                // The key stays the value the expression produced: making a
                // property key of it happens at the access, after the right
                // side has run, as the specification orders an assignment.
                self.emit(Opcode::Star, &[i64::from(key)]);
                Reference { object, key }
            }
            NodeKind::SuperMember | NodeKind::SuperIndex => {
                // A super reference: the base — the home object's prototype
                // — is fetched as the reference forms, before any key
                // expression or right side runs.
                if !self.allow_super_property {
                    self.fail(target, code::SYNTAX_NOT_ADMITTED);
                    return Reference { object: 0, key: 0 };
                }
                let object = self.allocate();
                self.emit(Opcode::GetSuperBase, &[]);
                self.emit(Opcode::Star, &[i64::from(object)]);
                if matches!(target.kind, NodeKind::SuperMember) {
                    let key = self.identifier_constant(target);
                    Reference { object, key }
                } else {
                    let key = self.allocate();
                    self.expression(target.first);
                    self.emit(Opcode::Star, &[i64::from(key)]);
                    Reference { object, key }
                }
            }
            NodeKind::Identifier => {
                // A slot an eval could shadow resolves when the reference
                // forms: the environment is taken now, used at the write.
                if let Some((constant, slot, depth)) = self.shadowable_slot(target) {
                    let object = self.allocate();
                    self.emit(
                        Opcode::PrepareShadowable,
                        &[i64::from(constant), i64::from(slot), i64::from(depth)],
                    );
                    self.emit(Opcode::Star, &[i64::from(object)]);
                    return Reference {
                        object,
                        key: constant,
                    };
                }
                if self.strict_global_target(target) {
                    // Strict code resolves the reference before the value
                    // is made: an unresolvable one throws at the write,
                    // whatever the value's evaluation added to the global.
                    let constant = self.identifier_constant(target);
                    let object = self.allocate();
                    self.emit(Opcode::HasGlobal, &[i64::from(constant)]);
                    self.emit(Opcode::Star, &[i64::from(object)]);
                    return Reference {
                        object,
                        key: constant,
                    };
                }
                Reference { object: 0, key: 0 }
            }
            _ => Reference { object: 0, key: 0 },
        }
    }

    /// Whether an identifier target is a strict write to a name that
    /// resolves nowhere the program can see, through no dynamic scope.
    pub(super) fn strict_global_target(&mut self, target: &Node) -> bool {
        self.strict
            && !self.dynamic_names
            && self.with_depth == 0
            && matches!(self.resolve(target.first, target.second), Resolved::Global)
    }

    pub(super) fn read_reference(&mut self, target: &Node, reference: &Reference) {
        match target.kind {
            NodeKind::Member => self.emit(
                Opcode::GetNamedProperty,
                &[i64::from(reference.object), i64::from(reference.key)],
            ),
            NodeKind::SuperMember => {
                self.emit(Opcode::LdaConstant, &[i64::from(reference.key)]);
                self.emit(Opcode::LdaSuperKeyed, &[i64::from(reference.object)]);
            }
            NodeKind::SuperIndex => {
                self.emit(Opcode::Ldar, &[i64::from(reference.key)]);
                self.emit(Opcode::LdaSuperKeyed, &[i64::from(reference.object)]);
            }
            NodeKind::Index => {
                self.emit(Opcode::Ldar, &[i64::from(reference.key)]);
                self.emit(Opcode::GetKeyedProperty, &[i64::from(reference.object)]);
            }
            NodeKind::Identifier => {
                if let Some((_, slot, _)) = self.shadowable_slot(target) {
                    self.emit(
                        Opcode::LdaPrepared,
                        &[
                            i64::from(reference.object),
                            i64::from(reference.key),
                            i64::from(slot),
                        ],
                    );
                    return;
                }
                self.expression_target_read(target);
            }
            _ => self.expression_target_read(target),
        }
    }

    pub(super) fn expression_target_read(&mut self, target: &Node) {
        match target.kind {
            NodeKind::Identifier => self.load_name(target),
            _ => self.fail(target, code::LOWERING_NOT_ADMITTED),
        }
    }

    pub(super) fn write_reference(&mut self, target: &Node, reference: &Reference) {
        match target.kind {
            NodeKind::Member => self.emit(
                Opcode::SetNamedProperty,
                &[i64::from(reference.object), i64::from(reference.key)],
            ),
            NodeKind::SuperMember => self.emit(
                Opcode::StaSuperNamed,
                &[i64::from(reference.object), i64::from(reference.key)],
            ),
            NodeKind::SuperIndex => self.emit(
                Opcode::StaSuperKeyed,
                &[i64::from(reference.object), i64::from(reference.key)],
            ),
            NodeKind::Index => self.emit(
                Opcode::SetKeyedProperty,
                &[i64::from(reference.object), i64::from(reference.key)],
            ),
            NodeKind::Identifier => {
                // Strict code refuses the restricted and reserved names on
                // every store path, the prepared one included.
                if self.strict {
                    if matches!(
                        self.span(target.first, target.second),
                        b"eval" | b"arguments"
                    ) {
                        self.fail(target, code::STRICT_ASSIGNMENT_TO_RESTRICTED_NAME);
                        return;
                    }
                    if self.strict_reserved_guard(target) {
                        return;
                    }
                }
                if let Some((_, slot, _)) = self.shadowable_slot(target) {
                    self.emit(
                        Opcode::StaPrepared,
                        &[
                            i64::from(reference.object),
                            i64::from(reference.key),
                            i64::from(slot),
                        ],
                    );
                    return;
                }
                if self.strict_global_target(target) {
                    if matches!(
                        self.span(target.first, target.second),
                        b"eval" | b"arguments"
                    ) {
                        self.fail(target, code::STRICT_ASSIGNMENT_TO_RESTRICTED_NAME);
                        return;
                    }
                    if self.strict_reserved_guard(target) {
                        return;
                    }
                    self.emit(
                        Opcode::StaGlobalResolved,
                        &[i64::from(reference.key), i64::from(reference.object)],
                    );
                    return;
                }
                self.store_name(target);
            }
            _ => self.store_name(target),
        }
    }
}
