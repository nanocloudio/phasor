//! Module bodies: the import and export tables, and the module prologue.

use super::*;

impl Lowering<'_, '_, '_, '_> {
    /// A module's own names get their values before its first statement runs:
    /// its `var`s are undefined, its function declarations are closures, and
    /// its imports and exports are recorded in the image.
    pub(super) fn module_prologue(&mut self, list: u32, length: u32) {
        let scope = self.scope;
        let record = self.program.scope(scope);
        let first = record.first as usize;
        let mut slot = 0u32;
        while slot < record.count {
            let Some(binding) = self.program.bindings.get(first + slot as usize).copied() else {
                break;
            };
            if binding.kind == binding_kind::VARIABLE {
                self.emit(Opcode::LdaUndefined, &[]);
                self.emit(Opcode::InitContextSlot, &[i64::from(binding.slot), 0]);
            }
            slot += 1;
        }
        self.declare_functions(list, length);
        self.emit(Opcode::InstantiationEnd, &[]);
        self.record_module_tables(list, length);
    }

    /// Fill in what the module imports and exports, now that its names have
    /// slots and its specifiers can be interned.
    pub(super) fn record_module_tables(&mut self, list: u32, length: u32) {
        let items = self.arena.list(list, length);
        let mut index = 0usize;
        let mut import = 0usize;
        while index < items.len() {
            let node = self.node(items[index]);
            match node.kind {
                NodeKind::Import => {
                    let specifier_node = self.node(node.third);
                    let specifier = self.specifier_constant(&specifier_node, node.flags & 0x07);
                    let clauses = self.arena.list(node.first, node.second);
                    if clauses.is_empty() {
                        // A bare import: the edge alone, nothing loaded.
                        if let Some(record) = self.program.imports.get_mut(import) {
                            *record = ImportRecord {
                                specifier,
                                name: u32::MAX,
                                slot: u32::MAX,
                            };
                        }
                        import += 1;
                    }
                    let mut clause_index = 0usize;
                    while clause_index < clauses.len() {
                        let clause = self.node(clauses[clause_index]);
                        let local = self.node(clause.first);
                        let name = if clause.has(flag::NAMESPACE) {
                            // A namespace import names the module itself; a
                            // deferred one asks for it unevaluated.
                            if clause.has(flag::DEFER) {
                                crate::bytecode::DEFER_IMPORT_NAME
                            } else {
                                u32::MAX
                            }
                        } else if clause.has(flag::SOURCE) && clause.second == NONE {
                            // A source-phase record no host here serves.
                            crate::bytecode::SOURCE_IMPORT_NAME
                        } else if clause.second == NONE {
                            self.default_key_constant()
                        } else {
                            self.key_constant(clause.second)
                        };
                        let slot = match self.resolve(local.first, local.second) {
                            Resolved::Slot { slot, .. } => slot,
                            Resolved::Global => 0,
                        };
                        if let Some(record) = self.program.imports.get_mut(import) {
                            *record = ImportRecord {
                                specifier,
                                name,
                                slot,
                            };
                        }
                        import += 1;
                        clause_index += 1;
                    }
                }
                NodeKind::Export if node.has(flag::OF) => {
                    // A re-export is an import this module never binds, and
                    // an export that names it for the linker to follow.
                    let specifier_node = self.node(node.first);
                    let specifier = self.specifier_constant(&specifier_node, node.flags & 0x07);
                    let clauses = self.arena.list(node.second, node.third);
                    if clauses.is_empty() {
                        // `export {} from` still loads the module it names.
                        if let Some(held) = self.program.imports.get_mut(import) {
                            *held = ImportRecord {
                                specifier,
                                name: u32::MAX,
                                slot: u32::MAX,
                            };
                        }
                        import += 1;
                    }
                    let mut clause_index = 0usize;
                    while clause_index < clauses.len() {
                        let record = self.node(clauses[clause_index]);
                        let index = u32::try_from(import).unwrap_or(0);
                        if record.has(flag::NAMESPACE) {
                            if record.first == NONE {
                                // `export * from` merges another module's
                                // exports into this one: the record's name
                                // is no constant at all, and the resolver
                                // follows the import to whatever it asks.
                                if let Some(held) = self.program.imports.get_mut(import) {
                                    *held = ImportRecord {
                                        specifier,
                                        name: u32::MAX,
                                        slot: u32::MAX,
                                    };
                                }
                                import += 1;
                                self.push_export(
                                    u32::MAX,
                                    crate::bytecode::EXPORT_IMPORT_MARK | index,
                                );
                                clause_index += 1;
                                continue;
                            }
                            // `export * as name from` re-exports the module
                            // itself, as a namespace.
                            if let Some(held) = self.program.imports.get_mut(import) {
                                *held = ImportRecord {
                                    specifier,
                                    name: u32::MAX,
                                    slot: u32::MAX,
                                };
                            }
                            import += 1;
                            let name = self.key_constant(record.first);
                            self.push_export(name, crate::bytecode::EXPORT_IMPORT_MARK | index);
                        } else {
                            let external = self.key_constant(record.first);
                            let exported = self.key_constant(record.second);
                            if let Some(held) = self.program.imports.get_mut(import) {
                                *held = ImportRecord {
                                    specifier,
                                    name: external,
                                    slot: u32::MAX,
                                };
                            }
                            import += 1;
                            self.push_export(exported, crate::bytecode::EXPORT_IMPORT_MARK | index);
                        }
                        clause_index += 1;
                    }
                }
                NodeKind::Export => self.record_export(&node),
                _ => {}
            }
            index += 1;
        }
    }

    /// Export every name a binding target binds: a plain name, or each
    /// name inside an array or object pattern.
    pub(super) fn export_target(&mut self, target: u32) {
        if target == NONE {
            return;
        }
        let node = self.node(target);
        match node.kind {
            NodeKind::Identifier => {
                let name = self.key_constant(target);
                if let Resolved::Slot { slot, .. } = self.resolve(node.first, node.second) {
                    self.push_export(name, slot);
                }
            }
            NodeKind::ArrayPattern | NodeKind::ObjectPattern => {
                for offset in 0..node.second {
                    let Some(&child) = self
                        .arena
                        .list(node.first, node.second)
                        .get(offset as usize)
                    else {
                        break;
                    };
                    let record = self.node(child);
                    match record.kind {
                        NodeKind::Elision => {}
                        NodeKind::RestElement => self.export_target(record.first),
                        NodeKind::PatternProperty => {
                            let element = self.node(record.second);
                            self.export_target(element.first);
                        }
                        _ => self.export_target(record.first),
                    }
                }
            }
            _ => {}
        }
    }

    /// Record what one `export` makes available.
    pub(super) fn record_export(&mut self, node: &Node) {
        if node.has(flag::PREFIX) {
            // `export default expression;` binds the value to the name
            // `default`, which is a name no identifier can be — except a
            // named function or class declaration, whose default IS its own
            // binding: reassigning the name is visible through the import.
            let name = self.default_key_constant();
            let inner = self.node(node.first);
            let declared = (matches!(inner.kind, NodeKind::Class)
                && !inner.has(flag::PARENTHESISED))
                || (matches!(inner.kind, NodeKind::Function) && inner.has(flag::DECLARATION));
            if declared && inner.first != NONE {
                let named = self.node(inner.first);
                if let Resolved::Slot { slot, .. } = self.resolve(named.first, named.second) {
                    self.push_export(name, slot);
                    return;
                }
            }
            let slot = self.default_slot();
            self.push_export(name, slot);
            return;
        }
        if node.first != NONE {
            let inner = self.node(node.first);
            match inner.kind {
                NodeKind::Declaration => {
                    for offset in 0..inner.second {
                        let Some(&declarator) = self
                            .arena
                            .list(inner.first, inner.second)
                            .get(offset as usize)
                        else {
                            break;
                        };
                        let record = self.node(declarator);
                        self.export_target(record.first);
                    }
                }
                NodeKind::Function | NodeKind::Class if inner.first != NONE => {
                    let name_node = self.node(inner.first);
                    let name = self.key_constant(inner.first);
                    if let Resolved::Slot { slot, .. } =
                        self.resolve(name_node.first, name_node.second)
                    {
                        self.push_export(name, slot);
                    }
                }
                _ => {}
            }
            return;
        }
        for offset in 0..node.third {
            let Some(&clause) = self
                .arena
                .list(node.second, node.third)
                .get(offset as usize)
            else {
                break;
            };
            let record = self.node(clause);
            let local = self.node(record.first);
            let name = self.key_constant(record.second);
            if let Resolved::Slot { slot, kind, .. } = self.resolve(local.first, local.second) {
                if kind == binding_kind::IMPORT {
                    // Exporting an import is an indirection: the record
                    // names this module's own import, for the linker to
                    // follow to wherever it leads.
                    let index = self.import_record_index(slot);
                    self.push_export(name, crate::bytecode::EXPORT_IMPORT_MARK | index);
                } else {
                    self.push_export(name, slot);
                }
            }
        }
    }

    pub(super) fn push_export(&mut self, name: u32, slot: u32) {
        let at = self.program.export_count;
        match self.program.exports.get_mut(at) {
            Some(record) => {
                *record = ExportRecord { name, slot };
                self.program.export_count += 1;
            }
            None => {
                let node = Node::new(NodeKind::Null, 0, 0);
                self.fail(&node, code::CODE_TOO_LARGE);
            }
        }
    }
}
