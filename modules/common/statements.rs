// Statement lowering.
//
// This file is included by `lower.rs` and extends the lowering with the
// statement grammar: blocks, declarations, the control statements, the
// prologues, and the exception-region and finaliser machinery those need.

impl<'a, 'b, 'c, 'p> Lowering<'a, 'b, 'c, 'p> {
    fn statements(&mut self, list: u32, length: u32) {
        let mut disposing = false;
        let mut awaited = false;
        for &statement in self.arena.list(list, length) {
            let node = self.node(statement);
            if !matches!(node.kind, NodeKind::Declaration) {
                continue;
            }
            disposing |= matches!(node.third, declaration::USING | declaration::AWAIT_USING);
            awaited |= node.third == declaration::AWAIT_USING;
        }
        if disposing {
            self.disposing_statements(list, length, awaited);
            return;
        }
        self.statement_list(list, length);
    }

    fn statement_list(&mut self, list: u32, length: u32) {
        let items = self.arena.list(list, length);
        let mut index = 0usize;
        while index < items.len() {
            let statement = items[index];
            self.statement(statement);
            if self.program.failure.is_some() {
                return;
            }
            // A statement written after one that returns, throws, or jumps away
            // can never run, so nothing is emitted for it.
            if self.builder.terminated() {
                return;
            }
            index += 1;
        }
    }

    /// A statement list with `using` declarations: every resource declared
    /// at its level is disposed when the list is left — falling off its
    /// end, jumping out, or throwing — last resource first.
    fn disposing_statements(&mut self, list: u32, length: u32, awaited: bool) {
        let mark = self.registers;
        let stack = self.allocate();
        let caught = self.allocate();
        self.emit(Opcode::CreateDisposeStack, &[]);
        self.emit(Opcode::Star, &[i64::from(stack)]);
        let finaliser = Finaliser {
            node: NONE,
            context_depth: self.context_depth,
            try_depth: self.try_depth,
            scope: self.scope,
            iterator: stack,
            flag: if awaited {
                DISPOSE_STACK_ASYNC
            } else {
                DISPOSE_STACK
            },
        };
        match self.finalisers.get_mut(self.finaliser_count) {
            Some(slot) => {
                *slot = finaliser;
                self.finaliser_count += 1;
            }
            None => {
                let node = Node::new(NodeKind::Null, 0, 0);
                self.fail(&node, code::EXPRESSION_TOO_DEEP);
                return;
            }
        }
        let outer_stack = self.dispose_stack;
        self.dispose_stack = stack;
        let region_depth = self.context_depth;
        self.try_depth += 1;
        let region_start = self.builder.length();
        self.statement_list(list, length);
        let region_end = self.builder.length();
        self.try_depth = self.try_depth.saturating_sub(1);
        self.dispose_stack = outer_stack;
        self.finaliser_count = self.finaliser_count.saturating_sub(1);
        if self.program.failure.is_some() {
            return;
        }
        let after = self.builder.label();
        let mut fell_through = false;
        if !self.builder.terminated() {
            self.emit_dispose(stack, None, awaited);
            self.builder.jump(Opcode::Jump, after);
            fell_through = true;
        }
        if region_start != region_end {
            // A throw from the list disposes the stack — a disposer's own
            // throw suppressing it — and throws what remains.
            let handler = self.builder.length();
            self.emit_dispose(stack, Some(caught), awaited);
            if self.record_region(region_start, region_end, handler, caught, region_depth) {
                return;
            }
        }
        if fell_through {
            self.builder.bind(after);
        }
        self.release(mark);
    }

    /// Empty a dispose stack here: synchronously, or — where `awaited` —
    /// in a loop that awaits each disposal that calls for it. A `caught`
    /// exception propagates once the stack is empty, a disposer's own throw
    /// suppressing it; without one, only a disposer's throw propagates.
    fn emit_dispose(&mut self, stack: u32, caught: Option<u32>, awaited: bool) {
        if !awaited {
            match caught {
                Some(caught) => {
                    self.emit(
                        Opcode::DisposeStackThrow,
                        &[i64::from(stack), i64::from(caught)],
                    );
                    self.emit(Opcode::Ldar, &[i64::from(caught)]);
                    self.emit(Opcode::Throw, &[]);
                }
                None => self.emit(Opcode::DisposeStack, &[i64::from(stack)]),
            }
            return;
        }
        // The pending exception lives in a register that holds the stack
        // itself while nothing has been thrown.
        let mark = self.registers;
        let pending = self.allocate();
        let scratch = self.allocate();
        match caught {
            Some(caught) => self.emit(Opcode::Ldar, &[i64::from(caught)]),
            None => self.emit(Opcode::Ldar, &[i64::from(stack)]),
        }
        self.emit(Opcode::Star, &[i64::from(pending)]);
        let top = self.builder.label();
        let done = self.builder.label();
        let after = self.builder.label();
        self.builder.bind(top);
        self.builder.safe_point();
        self.emit(
            Opcode::DisposeStackNext,
            &[i64::from(stack), i64::from(pending)],
        );
        self.emit(Opcode::Star, &[i64::from(scratch)]);
        self.emit(Opcode::TestStrictEqual, &[i64::from(stack)]);
        self.builder.jump(Opcode::JumpIfTrue, done);
        self.emit(Opcode::Ldar, &[i64::from(scratch)]);
        // A rejection joins the pending exception and disposal goes on.
        let region_depth = self.context_depth;
        let region_start = self.builder.length();
        self.emit(Opcode::Await, &[]);
        let region_end = self.builder.length();
        self.builder.jump(Opcode::Jump, top);
        let handler = self.builder.length();
        self.emit(
            Opcode::SuppressError,
            &[i64::from(stack), i64::from(pending), i64::from(scratch)],
        );
        self.builder.jump(Opcode::Jump, top);
        if self.record_region(region_start, region_end, handler, scratch, region_depth) {
            return;
        }
        self.builder.bind(done);
        self.emit(Opcode::Ldar, &[i64::from(pending)]);
        self.emit(Opcode::TestStrictEqual, &[i64::from(stack)]);
        self.builder.jump(Opcode::JumpIfTrue, after);
        self.emit(Opcode::Ldar, &[i64::from(pending)]);
        self.emit(Opcode::Throw, &[]);
        self.builder.bind(after);
        self.release(mark);
    }

    fn statement(&mut self, index: u32) {
        if self.program.failure.is_some() || index == NONE {
            return;
        }
        let node = self.node(index);
        match node.kind {
            NodeKind::ExpressionStatement => {
                let mark = self.registers;
                self.expression(node.first);
                if !self.in_function && self.completion != u32::MAX {
                    // A script's value is its last expression statement's.
                    let completion = self.completion;
                    self.emit(Opcode::Star, &[i64::from(completion)]);
                }
                self.release(mark);
            }
            NodeKind::Declaration => self.declaration(&node),
            NodeKind::Block => self.block(&node),
            NodeKind::Empty | NodeKind::Debugger => {}
            NodeKind::Function => {
                // Declarations were hoisted; the closure was made on entry.
            }
            NodeKind::With => {
                self.reset_completion();
                self.with_statement(&node);
            }
            NodeKind::Class => {
                // A class declaration initialises its `let`-like binding
                // where it is written.
                let mark = self.registers;
                self.lower_class(&node, index);
                let name = self.node(node.first);
                self.initialise_name(&name);
                self.release(mark);
            }
            NodeKind::Decorated => {
                // The decorators evaluate first, in source order; what they
                // answer is not applied, which keeps the class they saw.
                let mark = self.registers;
                for &decorator in self.arena.list(node.first, node.second) {
                    self.expression(decorator);
                }
                self.release(mark);
                self.statement(node.third);
            }
            NodeKind::Import => {
                // An import binds nothing here: the module that exports the
                // name holds it, and a read goes through that module.
            }
            NodeKind::Export => {
                if node.has(flag::PREFIX) {
                    // `export default expression;` — and a named function
                    // declaration also takes its own module binding.
                    let mark = self.registers;
                    let inner = self.node(node.first);
                    if matches!(inner.kind, NodeKind::Function) && inner.has(flag::DECLARATION) {
                        // Hoisted on entry; the statement is spent.
                        self.release(mark);
                        return;
                    }
                    // An anonymous default-exported class is named `default`
                    // before its static initialisers run.
                    if matches!(inner.kind, NodeKind::Class) && inner.first == NONE {
                        let constant = self.default_key_constant();
                        self.pending_class_name = Some(constant);
                    }
                    self.expression(node.first);
                    self.pending_class_name = None;
                    // An anonymous function or arrow takes the name
                    // `default`, as any named evaluation would give it.
                    if matches!(inner.kind, NodeKind::Function) && inner.first == NONE {
                        let constant = self.default_key_constant();
                        self.emit(Opcode::NameClosure, &[i64::from(constant)]);
                    }
                    let slot = self.default_slot();
                    self.emit(Opcode::InitContextSlot, &[i64::from(slot), 0]);
                    self.release(mark);
                } else if node.has(flag::OF) {
                    // A re-export runs nothing here: the linker wires the
                    // names, and the specifier's edge alone loads the module.
                } else if node.first != NONE {
                    // A function declaration was already made on entry.
                    let inner = self.node(node.first);
                    if !matches!(inner.kind, NodeKind::Function) {
                        self.statement(node.first);
                    }
                }
            }
            NodeKind::If => {
                self.reset_completion();
                self.if_statement(&node);
            }
            NodeKind::While => {
                self.reset_completion();
                self.while_statement(&node, 0, 0);
            }
            NodeKind::DoWhile => {
                self.reset_completion();
                self.do_while_statement(&node, 0, 0);
            }
            NodeKind::For => {
                self.reset_completion();
                self.for_statement(&node, 0, 0);
            }
            NodeKind::ForInOf => {
                self.reset_completion();
                self.for_in_of_statement(&node, 0, 0);
            }
            NodeKind::Break | NodeKind::Continue => self.break_or_continue(&node),
            NodeKind::Return => self.return_statement(&node),
            NodeKind::Throw => {
                let mark = self.registers;
                self.expression(node.first);
                self.emit(Opcode::Throw, &[]);
                self.release(mark);
            }
            NodeKind::Try => {
                self.reset_completion();
                self.try_statement(&node);
            }
            NodeKind::Switch => {
                self.reset_completion();
                self.switch_statement(&node, 0, 0);
            }
            NodeKind::Labelled => {
                self.reset_completion();
                self.labelled_statement(&node);
            }
            _ => self.fail(&node, code::LOWERING_NOT_ADMITTED),
        }
    }

    /// A labelled statement passes its label to the statement it labels, so
    /// `break label` and `continue label` reach the right one.
    /// A script's value comes only from the statements inside a compound
    /// statement, never from before it: `1; if (true) {}` is `undefined`.
    /// A `finally` block: its completion starts at undefined, so an abrupt
    /// exit from it carries its own last value, and a normal one gives the
    /// protected block's value back.
    fn finally_block(&mut self, body: u32) {
        if self.in_function || self.completion == u32::MAX {
            self.statement(body);
            return;
        }
        let mark = self.registers;
        let saved = self.allocate();
        let completion = self.completion;
        self.emit(Opcode::Ldar, &[i64::from(completion)]);
        self.emit(Opcode::Star, &[i64::from(saved)]);
        self.emit(Opcode::LdaUndefined, &[]);
        self.emit(Opcode::Star, &[i64::from(completion)]);
        self.statement(body);
        if !self.builder.terminated() {
            self.emit(Opcode::Ldar, &[i64::from(saved)]);
            self.emit(Opcode::Star, &[i64::from(completion)]);
        }
        self.release(mark);
    }

    fn reset_completion(&mut self) {
        if self.in_function || self.completion == u32::MAX {
            return;
        }
        self.emit(Opcode::LdaUndefined, &[]);
        let completion = self.completion;
        self.emit(Opcode::Star, &[i64::from(completion)]);
    }

    fn labelled_statement(&mut self, node: &Node) {
        let label = self.node(node.first);
        let body = self.node(node.second);
        match body.kind {
            NodeKind::While => self.while_statement(&body, label.first, label.second),
            NodeKind::DoWhile => self.do_while_statement(&body, label.first, label.second),
            NodeKind::For => self.for_statement(&body, label.first, label.second),
            NodeKind::ForInOf => self.for_in_of_statement(&body, label.first, label.second),
            NodeKind::Switch => self.switch_statement(&body, label.first, label.second),
            _ => {
                // A label on anything else can still be broken out of.
                let done = self.builder.label();
                self.open_target(Target {
                    label_start: label.first,
                    label_end: label.second,
                    breakable: false,
                    continuable: false,
                    break_label: done,
                    continue_label: done,
                    context_depth: self.context_depth,
                    finaliser_depth: u32::try_from(self.finaliser_count).unwrap_or(0),
                    break_used: false,
                    continue_used: false,
                });
                self.statement(node.second);
                let (break_used, _) = self.target_used();
                self.close_target();
                if break_used || !self.builder.terminated() {
                    self.builder.bind(done);
                }
            }
        }
    }

    fn open_target(&mut self, target: Target) {
        match self.targets.get_mut(self.target_count) {
            Some(slot) => {
                *slot = target;
                self.target_count += 1;
            }
            None => {
                let node = Node::new(NodeKind::Null, 0, 0);
                self.fail(&node, code::EXPRESSION_TOO_DEEP);
            }
        }
    }

    /// Which of the innermost target's labels anything jumps to.
    fn target_used(&self) -> (bool, bool) {
        match self.targets.get(self.target_count.saturating_sub(1)) {
            Some(target) => (target.break_used, target.continue_used),
            None => (false, false),
        }
    }

    fn close_target(&mut self) {
        self.target_count = self.target_count.saturating_sub(1);
    }

    fn block(&mut self, node: &Node) {
        let outer = self.scope;
        let scope = match self.program.open_scope(outer) {
            Ok(scope) => scope,
            Err(diagnostic) => {
                if self.program.failure.is_none() {
                    self.program.failure = Some(diagnostic);
                }
                return;
            }
        };
        self.scope = scope;
        let arena = self.arena;
        if let Err(diagnostic) = declare_lexical(
            arena,
            self.source,
            node.first,
            node.second,
            scope,
            self.program,
            false,
        ) {
            if self.program.failure.is_none() {
                self.program.failure = Some(diagnostic);
            }
            self.scope = outer;
            return;
        }
        let slots = self.program.scope(scope).count;
        if slots > 0 {
            self.push_context(slots);
            self.declare_functions(node.first, node.second);
        }
        self.statements(node.first, node.second);
        if slots > 0 {
            self.pop_context();
        }
        self.scope = outer;
    }

    /// Make the closures a scope's function declarations name, before any of
    /// its statements run.
    fn declare_functions(&mut self, list: u32, length: u32) {
        let items = self.arena.list(list, length);
        let mut index = 0usize;
        while index < items.len() {
            // `export function f() {}` declares `f` exactly as a bare
            // declaration does, so the export is looked through.
            let mut statement = items[index];
            let mut node = self.node(statement);
            if matches!(node.kind, NodeKind::Export)
                && !node.has(flag::PREFIX)
                && node.first != NONE
            {
                statement = node.first;
                node = self.node(statement);
            }
            if matches!(node.kind, NodeKind::Function) && node.first != NONE {
                let function = self.queue_function(statement);
                self.emit(Opcode::CreateClosure, &[i64::from(function)]);
                let name = self.node(node.first);
                let constant = self.identifier_constant(&name);
                self.emit(Opcode::NameClosure, &[i64::from(constant)]);
                self.initialise_name(&name);
            }
            // `export default function ... () {}` hoists too: the default
            // binding — and the function's own name, when it has one —
            // hold the closure before the first statement runs.
            if matches!(node.kind, NodeKind::Export) && node.has(flag::PREFIX) && node.first != NONE
            {
                let inner_index = node.first;
                let inner = self.node(inner_index);
                if matches!(inner.kind, NodeKind::Function) && inner.has(flag::DECLARATION) {
                    let mark = self.registers;
                    let function = self.queue_function(inner_index);
                    self.emit(Opcode::CreateClosure, &[i64::from(function)]);
                    if inner.first != NONE {
                        let name = self.node(inner.first);
                        let constant = self.identifier_constant(&name);
                        self.emit(Opcode::NameClosure, &[i64::from(constant)]);
                        let held = self.allocate();
                        self.emit(Opcode::Star, &[i64::from(held)]);
                        self.initialise_name(&name);
                        self.emit(Opcode::Ldar, &[i64::from(held)]);
                    } else {
                        let constant = self.default_key_constant();
                        self.emit(Opcode::NameClosure, &[i64::from(constant)]);
                    }
                    let slot = self.default_slot();
                    self.emit(Opcode::InitContextSlot, &[i64::from(slot), 0]);
                    self.release(mark);
                }
            }
            index += 1;
        }
    }

    /// A module's own names get their values before its first statement runs:
    /// its `var`s are undefined, its function declarations are closures, and
    /// its imports and exports are recorded in the image.
    fn module_prologue(&mut self, list: u32, length: u32) {
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
    fn record_module_tables(&mut self, list: u32, length: u32) {
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
    fn export_target(&mut self, target: u32) {
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
    fn record_export(&mut self, node: &Node) {
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

    fn push_export(&mut self, name: u32, slot: u32) {
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

    /// A script's `var` names become properties of the global object, and its
    /// function declarations become properties holding closures.
    fn script_prologue(&mut self, list: u32, length: u32) {
        // Every global function declaration is checked definable before any
        // binding — var or function — is created, so a failing declaration
        // leaves nothing half-instantiated behind.
        let global_var_env = self.program.eval_var_env_depth == u32::MAX;
        let whole_script = global_var_env && !self.program.eval_goal && !self.program.module;
        if whole_script {
            // GlobalDeclarationInstantiation: every lexical name is checked
            // against the global lexicals, the script `var`s, and the
            // restricted global properties, and every `var` and function
            // name against the global lexicals, before anything is bound.
            self.global_lexical_names(list, length, Opcode::CheckGlobalLexical);
            let items = self.arena.list(list, length);
            let mut index = 0usize;
            while index < items.len() {
                self.global_var_names(items[index], Opcode::CheckGlobalVar);
                index += 1;
            }
            let items = self.arena.list(list, length);
            let mut index = 0usize;
            while index < items.len() {
                let node = self.node(items[index]);
                if matches!(node.kind, NodeKind::Function) && node.first != NONE {
                    let name = self.node(node.first);
                    let constant = self.identifier_constant(&name);
                    self.emit(Opcode::CheckGlobalVar, &[i64::from(constant)]);
                }
                index += 1;
            }
        }
        if global_var_env {
            let items = self.arena.list(list, length);
            let mut index = 0usize;
            while index < items.len() {
                let node = self.node(items[index]);
                if matches!(node.kind, NodeKind::Function) && node.first != NONE {
                    let name = self.node(node.first);
                    if matches!(self.resolve(name.first, name.second), Resolved::Global) {
                        let constant = self.identifier_constant(&name);
                        self.emit(Opcode::DeclareGlobalFunction, &[i64::from(constant), 0]);
                    }
                }
                index += 1;
            }
        }
        let items = self.arena.list(list, length);
        let mut index = 0usize;
        while index < items.len() {
            let statement = items[index];
            self.declare_global_vars(statement);
            index += 1;
        }
        let items = self.arena.list(list, length);
        let mut index = 0usize;
        while index < items.len() {
            let node = self.node(items[index]);
            if matches!(node.kind, NodeKind::Function) && node.first != NONE {
                let function = self.queue_function(items[index]);
                self.emit(Opcode::CreateClosure, &[i64::from(function)]);
                // Stored wherever the name resolves: the global object for a
                // script, the eval's own scope when strict eval hoisted it —
                // and a sloppy eval in a function first makes the binding its
                // closure then fills.
                let name = self.node(node.first);
                let constant = self.identifier_constant(&name);
                self.emit(Opcode::NameClosure, &[i64::from(constant)]);
                if matches!(self.resolve(name.first, name.second), Resolved::Global) {
                    // The binding exists before the closure is stored, which
                    // strict assignment demands. On the global itself the
                    // function may only land on a definable property; in a
                    // function's environment the eval declares as a var.
                    if global_var_env {
                        let mode = if self.program.eval_goal { 2 } else { 1 };
                        self.emit(Opcode::DeclareGlobalFunction, &[i64::from(constant), mode]);
                    } else {
                        self.emit(Opcode::DeclareEvalVar, &[i64::from(constant)]);
                    }
                }
                self.store_name(&name);
            }
            index += 1;
        }
        // The script's own lexical declarations are initialised where they are
        // written; only its functions exist before the first statement runs.
        self.declare_script_functions_in_blocks(list, length);
        if whole_script {
            // The lexical names bind now, uninitialised: reading one before
            // its declaration runs is the dead zone's ReferenceError.
            self.global_lexical_names(list, length, Opcode::DeclareGlobalLexical);
        }
    }

    fn declare_script_functions_in_blocks(&mut self, _list: u32, _length: u32) {}

    /// Emit `opcode` for each top-level lexical name a script declares —
    /// `let`, `const`, and `class` — with an immediate saying whether the
    /// binding is a const where the opcode takes one.
    fn global_lexical_names(&mut self, list: u32, length: u32, opcode: Opcode) {
        let items = self.arena.list(list, length);
        let mut index = 0usize;
        while index < items.len() {
            let node = self.node(items[index]);
            index += 1;
            match node.kind {
                NodeKind::Declaration if node.third != declaration::VAR => {
                    let constant_binding = matches!(
                        node.third,
                        declaration::CONST | declaration::USING | declaration::AWAIT_USING
                    );
                    for offset in 0..node.second {
                        let Some(&declarator) = self
                            .arena
                            .list(node.first, node.second)
                            .get(offset as usize)
                        else {
                            break;
                        };
                        let record = self.node(declarator);
                        self.global_lexical_target(record.first, opcode, constant_binding);
                    }
                }
                NodeKind::Class if node.first != NONE => {
                    let name = self.node(node.first);
                    let constant = self.identifier_constant(&name);
                    self.emit_global_lexical(opcode, constant, false);
                }
                _ => {}
            }
        }
    }

    /// `opcode` for every name a lexical declaration's target binds.
    fn global_lexical_target(&mut self, target: u32, opcode: Opcode, constant_binding: bool) {
        let node = self.node(target);
        match node.kind {
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
                        NodeKind::PatternProperty => {
                            let element = self.node(record.second);
                            self.global_lexical_target(element.first, opcode, constant_binding);
                        }
                        _ => self.global_lexical_target(record.first, opcode, constant_binding),
                    }
                }
            }
            NodeKind::Identifier => {
                let constant = self.identifier_constant(&node);
                self.emit_global_lexical(opcode, constant, constant_binding);
            }
            _ => {}
        }
    }

    fn emit_global_lexical(&mut self, opcode: Opcode, constant: u32, constant_binding: bool) {
        if matches!(opcode, Opcode::DeclareGlobalLexical) {
            self.emit(
                opcode,
                &[i64::from(constant), i64::from(u8::from(constant_binding))],
            );
        } else {
            self.emit(opcode, &[i64::from(constant)]);
        }
    }

    /// `opcode` for every `var` name a statement declares, wherever it is
    /// written — the walk `declare_global_vars` makes, emitting a check.
    fn global_var_names(&mut self, index: u32, opcode: Opcode) {
        if index == NONE {
            return;
        }
        let node = self.node(index);
        match node.kind {
            NodeKind::Declaration if node.third == declaration::VAR => {
                for offset in 0..node.second {
                    let Some(&declarator) = self
                        .arena
                        .list(node.first, node.second)
                        .get(offset as usize)
                    else {
                        break;
                    };
                    let record = self.node(declarator);
                    self.global_var_target(record.first, opcode);
                }
            }
            NodeKind::Block => {
                for offset in 0..node.second {
                    let Some(&child) = self
                        .arena
                        .list(node.first, node.second)
                        .get(offset as usize)
                    else {
                        break;
                    };
                    self.global_var_names(child, opcode);
                }
            }
            NodeKind::If => {
                self.global_var_names(node.second, opcode);
                self.global_var_names(node.third, opcode);
            }
            NodeKind::While => self.global_var_names(node.second, opcode),
            NodeKind::DoWhile => self.global_var_names(node.first, opcode),
            NodeKind::For => {
                self.global_var_names(node.first, opcode);
                if let Some(&body) = self.arena.list(node.second, node.third).get(2) {
                    self.global_var_names(body, opcode);
                }
            }
            NodeKind::ForInOf => {
                self.global_var_names(node.first, opcode);
                self.global_var_names(node.third, opcode);
            }
            NodeKind::Labelled => self.global_var_names(node.second, opcode),
            NodeKind::With => self.global_var_names(node.second, opcode),
            NodeKind::Try => {
                self.global_var_names(node.first, opcode);
                if node.second != NONE {
                    let handler = self.node(node.second);
                    self.global_var_names(handler.second, opcode);
                }
                self.global_var_names(node.third, opcode);
            }
            NodeKind::Switch => {
                for offset in 0..node.third {
                    let Some(&case) = self
                        .arena
                        .list(node.second, node.third)
                        .get(offset as usize)
                    else {
                        break;
                    };
                    let clause = self.node(case);
                    for inner in 0..clause.third {
                        let Some(&statement) = self
                            .arena
                            .list(clause.second, clause.third)
                            .get(inner as usize)
                        else {
                            break;
                        };
                        self.global_var_names(statement, opcode);
                    }
                }
            }
            _ => {}
        }
    }

    /// `opcode` for every name a `var` declaration's target binds.
    fn global_var_target(&mut self, target: u32, opcode: Opcode) {
        let node = self.node(target);
        match node.kind {
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
                        NodeKind::PatternProperty => {
                            let element = self.node(record.second);
                            self.global_var_target(element.first, opcode);
                        }
                        _ => self.global_var_target(record.first, opcode),
                    }
                }
            }
            NodeKind::Identifier => {
                let constant = self.identifier_constant(&node);
                self.emit(opcode, &[i64::from(constant)]);
            }
            _ => {}
        }
    }

    /// Define the global properties a `var` introduces, so a name that is
    /// declared but never assigned still reads as `undefined`.
    fn declare_global_vars(&mut self, index: u32) {
        if index == NONE {
            return;
        }
        let node = self.node(index);
        match node.kind {
            NodeKind::Declaration if node.third == declaration::VAR => {
                for offset in 0..node.second {
                    let Some(&declarator) = self
                        .arena
                        .list(node.first, node.second)
                        .get(offset as usize)
                    else {
                        break;
                    };
                    let record = self.node(declarator);
                    self.declare_global_target(record.first);
                }
            }
            NodeKind::Block => {
                for offset in 0..node.second {
                    let Some(&child) = self
                        .arena
                        .list(node.first, node.second)
                        .get(offset as usize)
                    else {
                        break;
                    };
                    self.declare_global_vars(child);
                }
            }
            NodeKind::If => {
                self.declare_global_vars(node.second);
                self.declare_global_vars(node.third);
            }
            NodeKind::While => self.declare_global_vars(node.second),
            NodeKind::DoWhile => self.declare_global_vars(node.first),
            NodeKind::For => {
                self.declare_global_vars(node.first);
                if let Some(&body) = self.arena.list(node.second, node.third).get(2) {
                    self.declare_global_vars(body);
                }
            }
            NodeKind::ForInOf => {
                self.declare_global_vars(node.first);
                self.declare_global_vars(node.third);
            }
            NodeKind::Labelled => self.declare_global_vars(node.second),
            NodeKind::With => self.declare_global_vars(node.second),
            NodeKind::Try => {
                self.declare_global_vars(node.first);
                if node.second != NONE {
                    let handler = self.node(node.second);
                    self.declare_global_vars(handler.second);
                }
                self.declare_global_vars(node.third);
            }
            NodeKind::Switch => {
                for offset in 0..node.third {
                    let Some(&case) = self
                        .arena
                        .list(node.second, node.third)
                        .get(offset as usize)
                    else {
                        break;
                    };
                    let record = self.node(case);
                    for inner in 0..record.third {
                        let Some(&child) = self
                            .arena
                            .list(record.second, record.third)
                            .get(inner as usize)
                        else {
                            break;
                        };
                        self.declare_global_vars(child);
                    }
                }
            }
            _ => {}
        }
    }

    /// Give every `var`-kind slot of `scope` its starting `undefined`.
    fn initialise_hoisted(&mut self, scope: u32) {
        let record = self.program.scope(scope);
        let first = record.first as usize;
        let mut index = 0u32;
        while index < record.count {
            let Some(binding) = self.program.bindings.get(first + index as usize).copied() else {
                break;
            };
            if matches!(
                binding.kind,
                binding_kind::VARIABLE | binding_kind::FUNCTION
            ) {
                self.emit(Opcode::LdaUndefined, &[]);
                self.emit(Opcode::InitContextSlot, &[i64::from(binding.slot), 0]);
            }
            index += 1;
        }
    }

    /// A function's parameters and hoisted names get their values before its
    /// first statement runs. A `let` or `const` does not: it stays
    /// uninitialised until its declaration, which is its dead zone.
    #[expect(
        clippy::too_many_arguments,
        reason = "the prologue is one seam between the scope walk and the parameter list, and a struct would only rename the arity"
    )]
    fn function_prologue(
        &mut self,
        self_name: bool,
        function: u32,
        simple: bool,
        parameters: u32,
        parameter_bindings: u32,
        list: u32,
        length: u32,
        concise: bool,
    ) {
        let record = self.program.scope(self.scope);
        let first = record.first as usize;
        let mut slot = 0u32;
        while slot < record.count {
            let Some(binding) = self.program.bindings.get(first + slot as usize).copied() else {
                break;
            };
            if matches!(binding.kind, binding_kind::LET | binding_kind::CONST) {
                slot += 1;
                continue;
            }
            // A pattern-list parameter stays uninitialised until its own
            // binding step: a default that reads a later parameter is in the
            // dead zone, as the specification has it.
            if !simple && slot < parameter_bindings && binding.kind == binding_kind::VARIABLE {
                slot += 1;
                continue;
            }
            let _ = self_name;
            match Some(slot) {
                Some(argument)
                    if simple
                        && argument < parameters
                        && binding.kind == binding_kind::VARIABLE =>
                {
                    // The arguments arrive in the first registers, in order.
                    self.emit(Opcode::Ldar, &[i64::from(argument)]);
                }
                _ if binding.kind == binding_kind::SELF => self.emit(Opcode::LdaCallee, &[]),
                // The binding a body reads as `arguments` starts as the array
                // of what the call supplied; a `var arguments` shares it, as
                // the specification says it does.
                _ if self.span(binding.start, binding.end) == b"arguments" => {
                    let mapped = if simple && !self.strict {
                        parameters
                    } else {
                        0
                    };
                    self.emit(Opcode::CreateArguments, &[i64::from(mapped)]);
                }
                _ => self.emit(Opcode::LdaUndefined, &[]),
            }
            self.emit(Opcode::InitContextSlot, &[i64::from(binding.slot), 0]);
            slot += 1;
        }
        // A list with a pattern, a default, or a rest binds each parameter
        // explicitly: the slots all hold `undefined` by now, and each entry
        // takes its argument register apart in order.
        if !simple {
            let node = self.node(function);
            let entries = self.arena.list(node.second, node.third);
            // The arguments sit in the first registers until each is bound:
            // the scratch registers the binding code takes must start above
            // them, not over them.
            let floor = self.registers;
            let has_rest = entries
                .last()
                .and_then(|&last| self.arena.node(last))
                .is_some_and(|last| last.third == parameter_kind::REST);
            let reserved = if has_rest {
                MAX_CALL_ARGUMENTS
            } else {
                u32::try_from(entries.len().saturating_sub(1)).unwrap_or(0)
            };
            if self.registers < reserved {
                self.registers = reserved;
            }
            let mut argument = 0u32;
            self.in_parameters = true;
            for &parameter in entries.get(1..).unwrap_or(&[]) {
                let record = self.node(parameter);
                let mark = self.registers;
                if record.third == parameter_kind::REST {
                    let rest = self.node(record.first);
                    self.emit(Opcode::CreateRestArguments, &[i64::from(argument)]);
                    self.bind_target(rest.first, true);
                } else {
                    self.emit(Opcode::Ldar, &[i64::from(argument)]);
                    self.bind_with_default(record.first, record.second, true);
                    argument += 1;
                }
                self.release(mark);
            }
            self.in_parameters = false;
            self.registers = floor;
        }
        if !concise && simple {
            // A non-simple parameter list defers the body's function
            // closures until the body environment is pushed by the caller.
            self.declare_functions(list, length);
        }
    }

    /// Bind the accumulator to a target, taking a default in place of
    /// `undefined` when the element declares one.
    fn bind_with_default(&mut self, target: u32, default: u32, initialise: bool) {
        if default != NONE {
            let bound = self.builder.label();
            self.builder.jump(Opcode::JumpIfNotUndefined, bound);
            let name = self.node(target);
            if matches!(name.kind, NodeKind::Identifier) {
                self.named_expression(default, &name);
            } else {
                self.expression(default);
            }
            self.builder.bind(bound);
        }
        self.bind_target(target, initialise);
    }

    /// Take the accumulator apart over a binding target: a bare name stores
    /// or initialises it whole; a pattern reads the pieces and recurses.
    fn bind_target(&mut self, target: u32, initialise: bool) {
        let node = self.node(target);
        match node.kind {
            NodeKind::ArrayPattern => self.bind_array_pattern(&node, initialise),
            NodeKind::ObjectPattern => self.bind_object_pattern(&node, initialise),
            _ if initialise => self.initialise_name(&node),
            _ => self.store_name(&node),
        }
    }

    fn bind_array_pattern(&mut self, node: &Node, initialise: bool) {
        let mark = self.registers;
        let iterator = self.allocate();
        let done = self.allocate();
        let exception = self.allocate();
        self.emit(Opcode::GetIterator, &[]);
        self.emit(Opcode::Star, &[i64::from(iterator)]);
        if !self.push_iterator_finaliser(iterator, done) {
            return;
        }
        let region_depth = self.context_depth;
        let mut segments = [(0u32, 0u32); 32];
        let mut segment_count = 0usize;
        let elements = self.arena.list(node.first, node.second);
        for &element in elements {
            let child = self.node(element);
            match child.kind {
                NodeKind::Elision => {
                    self.emit(
                        Opcode::IteratorNext,
                        &[i64::from(iterator), i64::from(done)],
                    );
                }
                NodeKind::RestElement => {
                    // Whatever the iterator still holds becomes a fresh array.
                    let inner = self.registers;
                    let array = self.allocate();
                    let value = self.allocate();
                    self.emit(Opcode::CreateEmptyArray, &[]);
                    self.emit(Opcode::Star, &[i64::from(array)]);
                    let top = self.builder.label();
                    let end = self.builder.label();
                    self.builder.safe_point();
                    self.builder.bind(top);
                    self.emit(
                        Opcode::IteratorNext,
                        &[i64::from(iterator), i64::from(done)],
                    );
                    self.emit(Opcode::Star, &[i64::from(value)]);
                    self.emit(Opcode::Ldar, &[i64::from(done)]);
                    self.builder.jump(Opcode::JumpIfTrue, end);
                    self.emit(Opcode::Ldar, &[i64::from(value)]);
                    self.emit(Opcode::AppendArrayElement, &[i64::from(array)]);
                    self.builder.jump(Opcode::Jump, top);
                    self.builder.bind(end);
                    self.emit(Opcode::Ldar, &[i64::from(array)]);
                    self.release(inner);
                    let seg_start = self.builder.length();
                    self.bind_target(child.first, initialise);
                    if segment_count < segments.len() {
                        segments[segment_count] = (seg_start, self.builder.length());
                        segment_count += 1;
                    }
                }
                // A binding element: the next value, then its default.
                _ => {
                    self.emit(
                        Opcode::IteratorNext,
                        &[i64::from(iterator), i64::from(done)],
                    );
                    let seg_start = self.builder.length();
                    self.bind_with_default(child.first, child.second, initialise);
                    if segment_count < segments.len() {
                        segments[segment_count] = (seg_start, self.builder.length());
                        segment_count += 1;
                    }
                }
            }
        }
        self.finaliser_count = self.finaliser_count.saturating_sub(1);
        self.emit(
            Opcode::IteratorClose,
            &[i64::from(iterator), i64::from(done)],
        );
        self.close_on_abrupt(
            &segments[..segment_count],
            iterator,
            done,
            exception,
            region_depth,
        );
        self.release(mark);
    }

    /// Register the destructuring's iterator with the finaliser machinery,
    /// so an exit that escapes mid-walk — a return injected at a yield, a
    /// return statement in an initialiser — closes it like a `finally`.
    fn push_iterator_finaliser(&mut self, iterator: u32, done: u32) -> bool {
        match self.finalisers.get_mut(self.finaliser_count) {
            Some(slot) => {
                *slot = Finaliser {
                    node: NONE,
                    context_depth: self.context_depth,
                    try_depth: self.try_depth,
                    scope: self.scope,
                    iterator,
                    flag: done,
                };
                self.finaliser_count += 1;
                true
            }
            None => {
                let node = Node::new(NodeKind::Null, 0, 0);
                self.fail(&node, code::EXPRESSION_TOO_DEEP);
                false
            }
        }
    }

    /// Give a destructuring's element region a handler that closes the
    /// iterator — quietly, the original reason wins — and rethrows.
    fn close_on_abrupt(
        &mut self,
        segments: &[(u32, u32)],
        iterator: u32,
        done: u32,
        exception: u32,
        region_depth: u32,
    ) {
        if segments.iter().all(|&(start, end)| start == end) {
            return;
        }
        let after = self.builder.label();
        self.builder.jump(Opcode::Jump, after);
        let handler = self.builder.length();
        for &(start, end) in segments {
            if start == end {
                continue;
            }
            if self.record_region(start, end, handler, exception, region_depth) {
                return;
            }
        }
        self.emit(
            Opcode::IteratorCloseQuiet,
            &[i64::from(iterator), i64::from(done)],
        );
        self.emit(Opcode::Ldar, &[i64::from(exception)]);
        self.emit(Opcode::Throw, &[]);
        self.builder.bind(after);
    }

    /// Copy `object`'s own enumerable properties into `rest`, leaving out
    /// the keys the pattern named. The excluded keys go into an object of
    /// their own so the copy can skip them without ever touching them on the
    /// source — a proxy source must not see its excluded keys asked about.
    fn copy_rest(&mut self, object: u32, rest: u32, keys: &[(u32, bool)]) {
        if keys.is_empty() {
            self.emit(Opcode::Ldar, &[i64::from(object)]);
            self.emit(Opcode::CopyDataProperties, &[i64::from(rest)]);
            self.emit(Opcode::Ldar, &[i64::from(rest)]);
            return;
        }
        let mark = self.registers;
        let excluded = self.allocate();
        self.emit(Opcode::CreateEmptyObject, &[]);
        self.emit(Opcode::Star, &[i64::from(excluded)]);
        for &(key, keyed) in keys {
            self.emit(Opcode::LdaUndefined, &[]);
            if keyed {
                self.emit(
                    Opcode::DefineKeyedProperty,
                    &[i64::from(excluded), i64::from(key)],
                );
            } else {
                self.emit(
                    Opcode::DefineNamedProperty,
                    &[i64::from(excluded), i64::from(key)],
                );
            }
        }
        self.emit(Opcode::Ldar, &[i64::from(object)]);
        self.emit(
            Opcode::CopyDataPropertiesExcluding,
            &[i64::from(rest), i64::from(excluded)],
        );
        self.release(mark);
        self.emit(Opcode::Ldar, &[i64::from(rest)]);
    }
    fn bind_object_pattern(&mut self, node: &Node, initialise: bool) {
        const MAX_PATTERN_KEYS: usize = 32;
        let mark = self.registers;
        let object = self.allocate();
        self.emit(Opcode::Star, &[i64::from(object)]);
        // Nothing can be read from `undefined` or `null`, and the pattern
        // says so before any key is evaluated — a read against the value
        // raises the TypeError even when the pattern lists no property.
        let coercible = self.builder.label();
        self.builder.jump(Opcode::JumpIfNotNullish, coercible);
        let probe = self.length_key_constant();
        self.emit(
            Opcode::GetNamedProperty,
            &[i64::from(object), i64::from(probe)],
        );
        self.builder.bind(coercible);
        let properties = self.arena.list(node.first, node.second);
        // When a rest property follows, every listed key is excluded from
        // its copy — so each key is kept: a constant as itself, a computed key
        // in the register its value was saved to.
        let has_rest = properties
            .last()
            .and_then(|&last| self.arena.node(last))
            .is_some_and(|last| matches!(last.kind, NodeKind::RestElement));
        let mut keys = [(0u32, false); MAX_PATTERN_KEYS];
        let mut key_count = 0usize;
        for &property in properties {
            let child = self.node(property);
            match child.kind {
                NodeKind::RestElement => {
                    let inner = self.registers;
                    let rest = self.allocate();
                    self.emit(Opcode::CreateEmptyObject, &[]);
                    self.emit(Opcode::Star, &[i64::from(rest)]);
                    self.copy_rest(object, rest, keys.get(..key_count).unwrap_or(&[]));
                    self.release(inner);
                    self.bind_target(child.first, initialise);
                }
                NodeKind::PatternProperty => {
                    let key = self.node(child.first);
                    let element = self.node(child.second);
                    let recorded = if matches!(key.kind, NodeKind::ComputedKey) {
                        // The key value outlives the read: rest, if present,
                        // excludes it.
                        let key_register = self.allocate();
                        self.expression(key.first);
                        self.emit(Opcode::ToPropertyKey, &[]);
                        self.emit(Opcode::Star, &[i64::from(key_register)]);
                        let prepared = self.prepare_binding(element.first, initialise);
                        self.emit(Opcode::Ldar, &[i64::from(key_register)]);
                        self.emit(Opcode::GetKeyedProperty, &[i64::from(object)]);
                        ((key_register, true), prepared)
                    } else {
                        let constant = self.key_constant(child.first);
                        let prepared = self.prepare_binding(element.first, initialise);
                        self.emit(
                            Opcode::GetNamedProperty,
                            &[i64::from(object), i64::from(constant)],
                        );
                        ((constant, false), prepared)
                    };
                    let (recorded, prepared) = recorded;
                    if has_rest {
                        match keys.get_mut(key_count) {
                            Some(slot) => {
                                *slot = recorded;
                                key_count += 1;
                            }
                            None => {
                                self.fail(node, code::EXPRESSION_TOO_DEEP);
                                return;
                            }
                        }
                    }
                    self.bind_prepared(element.first, element.second, initialise, prepared);
                }
                _ => {}
            }
        }
        self.release(mark);
    }

    /// Resolve a `var` target before its value is read, where the target is
    /// a name a `with` object or a direct eval could supply: ResolveBinding
    /// precedes GetV, and a `with` object's `has` trap sees the order. The
    /// environment the name resolved to is kept in a register for the write.
    fn prepare_binding(&mut self, target: u32, initialise: bool) -> Option<(u32, u32, u32)> {
        if initialise {
            return None;
        }
        let node = self.node(target);
        if !matches!(node.kind, NodeKind::Identifier) {
            return None;
        }
        let (constant, slot, depth) = self.shadowable_slot(&node)?;
        let environment = self.allocate();
        self.emit(
            Opcode::PrepareShadowable,
            &[i64::from(constant), i64::from(slot), i64::from(depth)],
        );
        self.emit(Opcode::Star, &[i64::from(environment)]);
        Some((environment, constant, slot))
    }

    /// Bind the accumulator to a target whose reference, if it needed
    /// resolving first, was prepared before the read.
    fn bind_prepared(
        &mut self,
        target: u32,
        default: u32,
        initialise: bool,
        prepared: Option<(u32, u32, u32)>,
    ) {
        let Some((environment, constant, slot)) = prepared else {
            self.bind_with_default(target, default, initialise);
            return;
        };
        if default != NONE {
            let bound = self.builder.label();
            self.builder.jump(Opcode::JumpIfNotUndefined, bound);
            let name = self.node(target);
            self.named_expression(default, &name);
            self.builder.bind(bound);
        }
        self.emit(
            Opcode::StaPrepared,
            &[i64::from(environment), i64::from(constant), i64::from(slot)],
        );
    }

    /// Declare every name a `var` target adds to the global object. A name
    /// that resolves — to the eval's own hoisted scope, or to a binding the
    /// eval site can see — is that binding: it declares nothing.
    /// Take the accumulator apart over an assignment target written as an
    /// expression: the cover grammar's array and object literals, references,
    /// and nested defaults.
    fn assign_target(&mut self, target: u32) {
        let node = self.node(target);
        match node.kind {
            NodeKind::Array => self.assign_array_pattern(&node),
            NodeKind::Object => self.assign_object_pattern(&node),
            NodeKind::Identifier => self.store_name(&node),
            NodeKind::Member | NodeKind::Index => self.store(&node, target),
            _ => self.fail(&node, code::INVALID_ASSIGNMENT_TARGET),
        }
    }

    /// The target inside an element, once a `target = default` wrapper is
    /// looked through: the target and its default, `NONE` for none.
    fn element_parts(&mut self, target: u32) -> (u32, u32) {
        let node = self.node(target);
        if matches!(node.kind, NodeKind::Assign) && node.third == binop::ASSIGN {
            (node.first, node.second)
        } else {
            (target, NONE)
        }
    }

    /// Evaluate a member target's reference before the value it will take is
    /// read, which is the order the specification gives an assignment
    /// pattern's elements.
    fn prepare_element(&mut self, target: u32) -> Option<(Node, Reference)> {
        let (inner, _) = self.element_parts(target);
        let node = self.node(inner);
        match node.kind {
            NodeKind::Member => {
                let object = self.allocate();
                self.expression(node.first);
                self.emit(Opcode::Star, &[i64::from(object)]);
                let key = self.key_constant(node.second);
                Some((node, Reference { object, key }))
            }
            NodeKind::Index => {
                // The key stays as written: the write coerces it, after the
                // source property has been read, as PutValue does.
                let object = self.allocate();
                self.expression(node.first);
                self.emit(Opcode::Star, &[i64::from(object)]);
                let key = self.allocate();
                self.expression(node.second);
                self.emit(Opcode::Star, &[i64::from(key)]);
                Some((node, Reference { object, key }))
            }
            _ => None,
        }
    }

    /// Bind the accumulator to an element whose reference, if it has one, was
    /// prepared before the read.
    fn assign_after_read(&mut self, target: u32, prepared: Option<(Node, Reference)>) {
        let (inner, default) = self.element_parts(target);
        if default != NONE {
            let bound = self.builder.label();
            self.builder.jump(Opcode::JumpIfNotUndefined, bound);
            let name = self.node(inner);
            if matches!(name.kind, NodeKind::Identifier) {
                self.named_expression(default, &name);
            } else {
                self.expression(default);
            }
            self.builder.bind(bound);
        }
        match prepared {
            Some((node, reference)) => self.write_reference(&node, &reference),
            None => self.assign_target(inner),
        }
    }

    fn assign_array_pattern(&mut self, node: &Node) {
        let mark = self.registers;
        let iterator = self.allocate();
        let done = self.allocate();
        let exception = self.allocate();
        self.emit(Opcode::GetIterator, &[]);
        self.emit(Opcode::Star, &[i64::from(iterator)]);
        if !self.push_iterator_finaliser(iterator, done) {
            return;
        }
        let region_depth = self.context_depth;
        let mut segments = [(0u32, 0u32); 32];
        let mut segment_count = 0usize;
        let elements = self.arena.list(node.first, node.second);
        for &element in elements {
            let child = self.node(element);
            match child.kind {
                NodeKind::Elision => {
                    self.emit(
                        Opcode::IteratorNext,
                        &[i64::from(iterator), i64::from(done)],
                    );
                }
                NodeKind::Spread => {
                    // Whatever the iterator still holds becomes a fresh
                    // array, and a member target's reference comes first —
                    // its own segment, so a throw preparing it closes the
                    // iterator even though a throw from `next` must not.
                    let prepare_start = self.builder.length();
                    let prepared = self.prepare_element(child.first);
                    if segment_count < segments.len() {
                        segments[segment_count] = (prepare_start, self.builder.length());
                        segment_count += 1;
                    }
                    let inner = self.registers;
                    let array = self.allocate();
                    let value = self.allocate();
                    self.emit(Opcode::CreateEmptyArray, &[]);
                    self.emit(Opcode::Star, &[i64::from(array)]);
                    let top = self.builder.label();
                    let end = self.builder.label();
                    self.builder.safe_point();
                    self.builder.bind(top);
                    self.emit(
                        Opcode::IteratorNext,
                        &[i64::from(iterator), i64::from(done)],
                    );
                    self.emit(Opcode::Star, &[i64::from(value)]);
                    self.emit(Opcode::Ldar, &[i64::from(done)]);
                    self.builder.jump(Opcode::JumpIfTrue, end);
                    self.emit(Opcode::Ldar, &[i64::from(value)]);
                    self.emit(Opcode::AppendArrayElement, &[i64::from(array)]);
                    self.builder.jump(Opcode::Jump, top);
                    self.builder.bind(end);
                    self.emit(Opcode::Ldar, &[i64::from(array)]);
                    self.release(inner);
                    let seg_start = self.builder.length();
                    self.assign_after_read(child.first, prepared);
                    if segment_count < segments.len() {
                        segments[segment_count] = (seg_start, self.builder.length());
                        segment_count += 1;
                    }
                }
                _ => {
                    let prepare_start = self.builder.length();
                    let prepared = self.prepare_element(element);
                    if segment_count < segments.len() {
                        segments[segment_count] = (prepare_start, self.builder.length());
                        segment_count += 1;
                    }
                    self.emit(
                        Opcode::IteratorNext,
                        &[i64::from(iterator), i64::from(done)],
                    );
                    let seg_start = self.builder.length();
                    self.assign_after_read(element, prepared);
                    if segment_count < segments.len() {
                        segments[segment_count] = (seg_start, self.builder.length());
                        segment_count += 1;
                    }
                }
            }
        }
        self.finaliser_count = self.finaliser_count.saturating_sub(1);
        self.emit(
            Opcode::IteratorClose,
            &[i64::from(iterator), i64::from(done)],
        );
        self.close_on_abrupt(
            &segments[..segment_count],
            iterator,
            done,
            exception,
            region_depth,
        );
        self.release(mark);
    }

    fn assign_object_pattern(&mut self, node: &Node) {
        const MAX_PATTERN_KEYS: usize = 32;
        let mark = self.registers;
        let object = self.allocate();
        self.emit(Opcode::Star, &[i64::from(object)]);
        // Nothing can be read from `undefined` or `null`, before any key.
        let coercible = self.builder.label();
        self.builder.jump(Opcode::JumpIfNotNullish, coercible);
        let probe = self.length_key_constant();
        self.emit(
            Opcode::GetNamedProperty,
            &[i64::from(object), i64::from(probe)],
        );
        self.builder.bind(coercible);
        let properties = self.arena.list(node.first, node.second);
        let has_rest = properties
            .last()
            .and_then(|&last| self.arena.node(last))
            .is_some_and(|last| matches!(last.kind, NodeKind::Spread));
        let mut keys = [(0u32, false); MAX_PATTERN_KEYS];
        let mut key_count = 0usize;
        for &property in properties {
            let child = self.node(property);
            let recorded;
            match child.kind {
                NodeKind::Spread => {
                    let prepared = self.prepare_element(child.first);
                    let inner = self.registers;
                    let rest = self.allocate();
                    self.emit(Opcode::CreateEmptyObject, &[]);
                    self.emit(Opcode::Star, &[i64::from(rest)]);
                    self.copy_rest(object, rest, keys.get(..key_count).unwrap_or(&[]));
                    self.release(inner);
                    self.assign_after_read(child.first, prepared);
                    continue;
                }
                NodeKind::ShorthandProperty => {
                    let constant = self.key_constant(child.first);
                    self.emit(
                        Opcode::GetNamedProperty,
                        &[i64::from(object), i64::from(constant)],
                    );
                    recorded = (constant, false);
                    if child.second != NONE {
                        let bound = self.builder.label();
                        self.builder.jump(Opcode::JumpIfNotUndefined, bound);
                        let name = self.node(child.first);
                        self.named_expression(child.second, &name);
                        self.builder.bind(bound);
                    }
                    let name = self.node(child.first);
                    self.store_name(&name);
                }
                NodeKind::Property if child.third == property_kind::DATA => {
                    let key = self.node(child.first);
                    if matches!(key.kind, NodeKind::ComputedKey) {
                        let key_register = self.allocate();
                        self.expression(key.first);
                        self.emit(Opcode::ToPropertyKey, &[]);
                        self.emit(Opcode::Star, &[i64::from(key_register)]);
                        let prepared = self.prepare_element(child.second);
                        self.emit(Opcode::Ldar, &[i64::from(key_register)]);
                        self.emit(Opcode::GetKeyedProperty, &[i64::from(object)]);
                        recorded = (key_register, true);
                        self.assign_after_read(child.second, prepared);
                    } else {
                        let constant = self.key_constant(child.first);
                        let prepared = self.prepare_element(child.second);
                        self.emit(
                            Opcode::GetNamedProperty,
                            &[i64::from(object), i64::from(constant)],
                        );
                        recorded = (constant, false);
                        self.assign_after_read(child.second, prepared);
                    }
                }
                _ => {
                    self.fail(&child, code::INVALID_ASSIGNMENT_TARGET);
                    return;
                }
            }
            if has_rest {
                match keys.get_mut(key_count) {
                    Some(slot) => {
                        *slot = recorded;
                        key_count += 1;
                    }
                    None => {
                        self.fail(node, code::EXPRESSION_TOO_DEEP);
                        return;
                    }
                }
            }
        }
        self.release(mark);
    }

    /// `with`: the object joins the scope chain as an object environment,
    /// and every name in the body that could resolve past it goes through
    /// the run-time chain. Strict code refuses the statement.
    fn with_statement(&mut self, node: &Node) {
        if self.strict {
            self.fail(node, code::SYNTAX_NOT_ADMITTED);
            return;
        }
        let outer = self.scope;
        // The object environment occupies a context level, which the scope
        // tree must count for every depth the body resolves past it.
        let scope = match self.program.open_scope(outer) {
            Ok(scope) => scope,
            Err(diagnostic) => {
                if self.program.failure.is_none() {
                    self.program.failure = Some(diagnostic);
                }
                return;
            }
        };
        if let Some(record) = self.program.scopes.get_mut(scope as usize) {
            record.context = true;
        }
        let mark = self.registers;
        self.expression(node.first);
        self.release(mark);
        self.emit(Opcode::PushObjectContext, &[]);
        self.context_depth += 1;
        if self.context_depth > self.max_context_depth {
            self.max_context_depth = self.context_depth;
        }
        self.scope = scope;
        self.with_depth += 1;
        self.statement(node.second);
        self.with_depth -= 1;
        self.scope = outer;
        self.pop_context();
    }

    /// The register a class member's computed key was evaluated into: the
    /// member's rank among the computed-key members, in source order.
    fn computed_key_register(&self, keys: &[u32], member: u32, entries: &[u32]) -> u32 {
        let mut rank = 0usize;
        for &candidate in entries.get(1..).unwrap_or(&[]) {
            let record = self.node(candidate);
            if record.first == NONE {
                continue;
            }
            let computed = matches!(self.node(record.first).kind, NodeKind::ComputedKey);
            if candidate == member {
                return if computed {
                    keys.get(rank).copied().unwrap_or(u32::MAX)
                } else {
                    u32::MAX
                };
            }
            if computed {
                rank += 1;
            }
        }
        u32::MAX
    }

    /// A class: heritage, constructor, prototype, and members, leaving the
    /// constructor in the accumulator. Class code is strict throughout.
    fn lower_class(&mut self, node: &Node, index: u32) {
        let _ = index;
        let mark = self.registers;
        let was_strict = self.strict;
        self.strict = true;
        // A named class binds its own name, immutably, in a scope of its
        // own: the heritage, the members, and everything they close over
        // see the constructor under that name.
        let outer_scope = self.scope;
        let mut named = false;
        if node.first != NONE {
            match self.program.open_scope(outer_scope) {
                Ok(scope) => {
                    if let Err(diagnostic) = declare_pattern(
                        self.arena,
                        node.first,
                        binding_kind::CONST,
                        scope,
                        self.program,
                    ) {
                        if self.program.failure.is_none() {
                            self.program.failure = Some(diagnostic);
                        }
                        return;
                    }
                    self.scope = scope;
                    // A pushed environment counts in resolution even when the
                    // pattern declares nothing.
                    if let Some(record) = self.program.scopes.get_mut(scope as usize) {
                        record.context = true;
                    }
                    let slots = self.program.scope(scope).count.max(1);
                    self.push_context(slots);
                    named = true;
                }
                Err(diagnostic) => {
                    if self.program.failure.is_none() {
                        self.program.failure = Some(diagnostic);
                    }
                    return;
                }
            }
        }
        let entries = self.arena.list(node.second, node.third);
        let heritage = entries.first().copied().unwrap_or(NONE);
        let derived = i64::from(heritage != NONE);

        // The prototype object, over the heritage's when one is written.
        let proto = self.allocate();
        let parent = self.allocate();
        if heritage != NONE {
            self.expression(heritage);
            self.emit(Opcode::Star, &[i64::from(parent)]);
            self.emit(Opcode::CreateEmptyObject, &[]);
            self.emit(Opcode::Star, &[i64::from(proto)]);
            // `extends null` makes a prototype with nothing above it.
            let null_case = self.builder.label();
            let wired = self.builder.label();
            self.emit(Opcode::Ldar, &[i64::from(parent)]);
            self.builder.jump(Opcode::JumpIfNullish, null_case);
            self.emit(Opcode::GetHeritagePrototype, &[i64::from(parent)]);
            self.emit(Opcode::SetPrototype, &[i64::from(proto)]);
            self.builder.jump(Opcode::Jump, wired);
            self.builder.bind(null_case);
            self.emit(Opcode::LdaNull, &[]);
            self.emit(Opcode::SetPrototype, &[i64::from(proto)]);
            self.builder.bind(wired);
        } else {
            self.emit(Opcode::CreateEmptyObject, &[]);
            self.emit(Opcode::Star, &[i64::from(proto)]);
        }

        // Whether the class body declares a private member: its methods,
        // accessors, initialisers, and constructor can then see the private
        // scope, which their direct evals inherit.
        let mut has_private = false;
        for &member in entries.get(1..).unwrap_or(&[]) {
            let record = self.node(member);
            if record.first != NONE {
                let key = self.node(record.first);
                if matches!(key.kind, NodeKind::PropertyName)
                    && self.span(key.first, key.second).first() == Some(&b'#')
                {
                    has_private = true;
                }
            }
        }
        let privates = self.privates_visible || has_private;

        // The constructor: the one written, or the default.
        let constructor = entries.get(1..).unwrap_or(&[]).iter().copied().find(|&m| {
            let member = self.arena.node(m).copied();
            member.is_some_and(|member| member.third == class_member::CONSTRUCTOR)
        });
        let ctor = self.allocate();
        match constructor {
            Some(member) => {
                let record = self.node(member);
                let function = self.queue_class_constructor(
                    record.second,
                    heritage != NONE,
                    privates,
                    has_private,
                );
                self.emit(Opcode::CreateClosure, &[i64::from(function)]);
            }
            None => {
                // The default constructor is compiled like a written one, so
                // its field initialisers run as ordinary interpreter calls.
                let function =
                    self.queue_class_constructor(NONE, heritage != NONE, privates, has_private);
                self.emit(Opcode::CreateClosure, &[i64::from(function)]);
            }
        }
        if node.first != NONE {
            let name = self.node(node.first);
            let constant = self.identifier_constant(&name);
            self.emit(Opcode::NameClosure, &[i64::from(constant)]);
        } else if let Some(constant) = self.pending_class_name.take() {
            // Named evaluation: the name is the constructor's before any
            // static initialiser can read it.
            self.emit(Opcode::NameClosure, &[i64::from(constant)]);
        }
        self.emit(Opcode::Star, &[i64::from(ctor)]);
        if heritage != NONE {
            // The constructor inherits from the parent constructor — unless
            // the heritage was null, where Function.prototype stays.
            let unlinked = self.builder.label();
            self.emit(Opcode::Ldar, &[i64::from(parent)]);
            self.builder.jump(Opcode::JumpIfNullish, unlinked);
            self.emit(Opcode::SetPrototype, &[i64::from(ctor)]);
            self.builder.bind(unlinked);
        }
        self.emit(Opcode::Ldar, &[i64::from(ctor)]);
        self.emit(Opcode::MakeClassConstructor, &[i64::from(proto), derived]);

        // Every computed key evaluates first, in source order, whatever kind
        // of element it names: a throw from one stops the rest.
        const MAX_COMPUTED_KEYS: usize = 32;
        let mut computed_keys = [u32::MAX; MAX_COMPUTED_KEYS];
        let mut computed_count = 0usize;
        for &member in entries.get(1..).unwrap_or(&[]) {
            let record = self.node(member);
            if record.first == NONE {
                continue;
            }
            let key = self.node(record.first);
            if !matches!(key.kind, NodeKind::ComputedKey) {
                continue;
            }
            if computed_count >= MAX_COMPUTED_KEYS {
                self.fail(&key, code::EXPRESSION_TOO_DEEP);
                return;
            }
            let register = self.allocate();
            self.expression(key.first);
            self.emit(Opcode::ToPropertyKey, &[]);
            self.emit(Opcode::Star, &[i64::from(register)]);
            computed_keys[computed_count] = register;
            computed_count += 1;
        }

        // Instance fields: keys and initialiser closures gathered into an
        // array the constructor's prologue walks per construction.
        let mut instance_fields = 0u32;
        for &member in entries.get(1..).unwrap_or(&[]) {
            let record = self.node(member);
            if record.third == class_member::FIELD || record.third == class_member::ACCESSOR_FIELD {
                instance_fields += 1;
            }
        }
        if instance_fields > 0 {
            let inner = self.registers;
            let fields = self.allocate();
            self.emit(Opcode::CreateEmptyArray, &[]);
            self.emit(Opcode::Star, &[i64::from(fields)]);
            for &member in entries.get(1..).unwrap_or(&[]) {
                let record = self.node(member);
                if record.third == class_member::ACCESSOR_FIELD {
                    // The backing field: hidden behind the accessor's name.
                    let constant = self.accessor_backing_constant(record.first);
                    self.emit(Opcode::LdaConstant, &[i64::from(constant)]);
                    self.emit(Opcode::AppendArrayElement, &[i64::from(fields)]);
                    if record.second == NONE {
                        self.emit(Opcode::LdaUndefined, &[]);
                    } else {
                        let function = self.queue_field_initialiser(record.second, privates);
                        self.emit(Opcode::CreateClosure, &[i64::from(function)]);
                        self.emit(Opcode::SetHome, &[i64::from(proto)]);
                    }
                    self.emit(Opcode::AppendArrayElement, &[i64::from(fields)]);
                    continue;
                }
                if record.third != class_member::FIELD {
                    continue;
                }
                let key = self.node(record.first);
                if matches!(key.kind, NodeKind::ComputedKey) {
                    let register = self.computed_key_register(&computed_keys, member, entries);
                    self.emit(Opcode::Ldar, &[i64::from(register)]);
                } else {
                    let constant = self.key_constant(record.first);
                    if self.span(key.first, key.second).first().copied() == Some(b'#') {
                        // The class marks the declaration — inner scopes
                        // shadow outer ones — and the field's stored name is
                        // this class evaluation's own key.
                        let inner_mark = self.registers;
                        let marker = self.allocate();
                        self.emit(Opcode::LdaConstant, &[i64::from(constant)]);
                        self.emit(Opcode::PrivateKey, &[i64::from(proto), 1]);
                        self.emit(Opcode::Star, &[i64::from(marker)]);
                        self.emit(Opcode::LdaTrue, &[]);
                        self.emit(
                            Opcode::DefineKeyedProperty,
                            &[i64::from(proto), i64::from(marker)],
                        );
                        self.release(inner_mark);
                        self.emit(Opcode::LdaConstant, &[i64::from(constant)]);
                        self.emit(Opcode::PrivateKey, &[i64::from(proto), 0]);
                    } else {
                        self.emit(Opcode::LdaConstant, &[i64::from(constant)]);
                    }
                }
                self.emit(Opcode::AppendArrayElement, &[i64::from(fields)]);
                if record.second == NONE {
                    self.emit(Opcode::LdaUndefined, &[]);
                } else {
                    let function = self.queue_field_initialiser(record.second, privates);
                    self.emit(Opcode::CreateClosure, &[i64::from(function)]);
                    // The initialiser's `super` resolves through the class
                    // prototype, exactly as an instance method's does.
                    self.emit(Opcode::SetHome, &[i64::from(proto)]);
                }
                self.emit(Opcode::AppendArrayElement, &[i64::from(fields)]);
            }
            self.emit(Opcode::Ldar, &[i64::from(fields)]);
            let key = self.text_key_constant(b"\0fields");
            self.emit(Opcode::DefineMethod, &[i64::from(ctor), i64::from(key)]);
            self.release(inner);
        }

        // The class's own name takes the constructor before the static
        // initialisers run: they may name the class.
        if named {
            let name = self.node(node.first);
            self.emit(Opcode::Ldar, &[i64::from(ctor)]);
            self.initialise_name(&name);
        }
        for &member in entries.get(1..).unwrap_or(&[]) {
            let record = self.node(member);
            if record.third == class_member::CONSTRUCTOR || record.third == class_member::FIELD {
                continue;
            }
            // A decorator evaluates where it stands; what it answers is not
            // applied, which keeps the member it saw.
            if record.third == class_member::DECORATOR {
                let inner = self.registers;
                self.expression(record.second);
                self.release(inner);
                continue;
            }
            // An instance `accessor` field's face goes on the prototype; its
            // backing field is defined per construction with the others.
            if record.third == class_member::ACCESSOR_FIELD {
                let inner = self.registers;
                let key_register = self.allocate();
                let constant = self.key_constant(record.first);
                self.emit(Opcode::LdaConstant, &[i64::from(constant)]);
                self.emit(Opcode::Star, &[i64::from(key_register)]);
                self.emit(
                    Opcode::DefineAutoAccessor,
                    &[i64::from(proto), i64::from(key_register)],
                );
                self.release(inner);
                continue;
            }
            // A static `accessor` field: its face on the constructor, and
            // its backing defined now, the initialiser run with `this` the
            // constructor.
            if record.third == class_member::ACCESSOR_FIELD | class_member::STATIC {
                let inner = self.registers;
                let key_register = self.allocate();
                let constant = self.key_constant(record.first);
                self.emit(Opcode::LdaConstant, &[i64::from(constant)]);
                self.emit(Opcode::Star, &[i64::from(key_register)]);
                self.emit(
                    Opcode::DefineAutoAccessor,
                    &[i64::from(ctor), i64::from(key_register)],
                );
                let callee = self.allocate();
                let receiver = self.allocate();
                if record.second == NONE {
                    self.emit(Opcode::LdaUndefined, &[]);
                } else {
                    let function = self.queue_field_initialiser(record.second, privates);
                    self.emit(Opcode::CreateClosure, &[i64::from(function)]);
                    self.emit(Opcode::SetHome, &[i64::from(ctor)]);
                    self.emit(Opcode::Star, &[i64::from(callee)]);
                    self.emit(Opcode::Ldar, &[i64::from(ctor)]);
                    self.emit(Opcode::Star, &[i64::from(receiver)]);
                    self.builder.safe_point();
                    self.emit(Opcode::Call, &[i64::from(callee), i64::from(receiver), 1]);
                }
                let value = self.allocate();
                self.emit(Opcode::Star, &[i64::from(value)]);
                let backing = self.accessor_backing_constant(record.first);
                self.emit(Opcode::LdaConstant, &[i64::from(backing)]);
                self.emit(Opcode::Star, &[i64::from(key_register)]);
                self.emit(Opcode::Ldar, &[i64::from(value)]);
                self.emit(
                    Opcode::DefineKeyedProperty,
                    &[i64::from(ctor), i64::from(key_register)],
                );
                self.release(inner);
                continue;
            }
            // A static block runs now, once, with `this` the constructor and
            // the class as its home.
            if record.third == class_member::STATIC_BLOCK | class_member::STATIC {
                let inner = self.registers;
                let callee = self.allocate();
                let receiver = self.allocate();
                let function = self.queue_field_initialiser(record.second, privates);
                self.emit(Opcode::CreateClosure, &[i64::from(function)]);
                self.emit(Opcode::SetHome, &[i64::from(ctor)]);
                self.emit(Opcode::Star, &[i64::from(callee)]);
                self.emit(Opcode::Ldar, &[i64::from(ctor)]);
                self.emit(Opcode::Star, &[i64::from(receiver)]);
                self.builder.safe_point();
                self.emit(Opcode::Call, &[i64::from(callee), i64::from(receiver), 1]);
                self.release(inner);
                continue;
            }
            // A static field: its initialiser runs now, with `this` bound to
            // the constructor, and the value becomes the constructor's own
            // property.
            if record.third == class_member::FIELD | class_member::STATIC {
                let inner = self.registers;
                let callee = self.allocate();
                let receiver = self.allocate();
                if record.second == NONE {
                    self.emit(Opcode::LdaUndefined, &[]);
                } else {
                    let function = self.queue_field_initialiser(record.second, privates);
                    self.emit(Opcode::CreateClosure, &[i64::from(function)]);
                    // A static initialiser's `super` resolves through the
                    // constructor, as a static method's does.
                    self.emit(Opcode::SetHome, &[i64::from(ctor)]);
                    self.emit(Opcode::Star, &[i64::from(callee)]);
                    self.emit(Opcode::Ldar, &[i64::from(ctor)]);
                    self.emit(Opcode::Star, &[i64::from(receiver)]);
                    self.builder.safe_point();
                    self.emit(Opcode::Call, &[i64::from(callee), i64::from(receiver), 1]);
                    // Named evaluation: an anonymous function value takes
                    // the field's name.
                    if self.is_anonymous_function(record.second) {
                        if matches!(self.node(record.first).kind, NodeKind::ComputedKey) {
                            let key_register =
                                self.computed_key_register(&computed_keys, member, entries);
                            self.emit(Opcode::NameClosureKeyed, &[i64::from(key_register)]);
                        } else {
                            let constant = self.key_constant(record.first);
                            self.emit(Opcode::NameClosure, &[i64::from(constant)]);
                        }
                    }
                }
                let key = self.node(record.first);
                if matches!(key.kind, NodeKind::ComputedKey) {
                    let key_register = self.computed_key_register(&computed_keys, member, entries);
                    self.emit(
                        Opcode::DefineKeyedProperty,
                        &[i64::from(ctor), i64::from(key_register)],
                    );
                } else {
                    let constant = self.key_constant(record.first);
                    let private = self.span(key.first, key.second).first().copied() == Some(b'#');
                    if private {
                        // The static private field stores under this class
                        // evaluation's own key.
                        let value = self.allocate();
                        self.emit(Opcode::Star, &[i64::from(value)]);
                        self.emit(Opcode::LdaConstant, &[i64::from(constant)]);
                        self.emit(Opcode::PrivateKey, &[i64::from(proto), 0]);
                        let key_register = self.allocate();
                        self.emit(Opcode::Star, &[i64::from(key_register)]);
                        self.emit(Opcode::Ldar, &[i64::from(value)]);
                        self.emit(
                            Opcode::DefineKeyedProperty,
                            &[i64::from(ctor), i64::from(key_register)],
                        );
                        self.emit(Opcode::LdaConstant, &[i64::from(constant)]);
                        self.emit(Opcode::PrivateKey, &[i64::from(proto), 1]);
                        self.emit(Opcode::Star, &[i64::from(key_register)]);
                        self.emit(Opcode::LdaTrue, &[]);
                        self.emit(
                            Opcode::DefineKeyedProperty,
                            &[i64::from(proto), i64::from(key_register)],
                        );
                    } else {
                        self.emit(
                            Opcode::DefineNamedProperty,
                            &[i64::from(ctor), i64::from(constant)],
                        );
                    }
                }
                self.release(inner);
                continue;
            }
            let target = if record.third & class_member::STATIC != 0 {
                ctor
            } else {
                proto
            };
            let kind = record.third & !class_member::STATIC;
            let key = self.node(record.first);
            let computed = matches!(key.kind, NodeKind::ComputedKey);
            let inner = self.registers;
            let key_register = if computed {
                Some(self.computed_key_register(&computed_keys, member, entries))
            } else {
                None
            };
            let function = self.queue_method(record.second, privates);
            self.emit(Opcode::CreateClosure, &[i64::from(function)]);
            if !computed {
                let constant = self.key_constant(record.first);
                self.emit(Opcode::NameClosure, &[i64::from(constant)]);
                let private = self.span(key.first, key.second).first().copied() == Some(b'#');
                if private {
                    // A private member stores under this class evaluation's
                    // own key; the keyed define carries the home and the
                    // non-writable shape all the same.
                    let closure = self.allocate();
                    self.emit(Opcode::Star, &[i64::from(closure)]);
                    self.emit(Opcode::LdaConstant, &[i64::from(constant)]);
                    self.emit(Opcode::PrivateKey, &[i64::from(proto), 0]);
                    let key_slot = self.allocate();
                    self.emit(Opcode::Star, &[i64::from(key_slot)]);
                    self.emit(Opcode::Ldar, &[i64::from(closure)]);
                    match kind {
                        class_member::GETTER | class_member::SETTER => self.emit(
                            Opcode::DefineClassAccessorKeyed,
                            &[
                                i64::from(target),
                                i64::from(key_slot),
                                i64::from(kind == class_member::SETTER),
                            ],
                        ),
                        _ => self.emit(
                            Opcode::DefineMethodKeyed,
                            &[i64::from(target), i64::from(key_slot)],
                        ),
                    }
                } else {
                    match kind {
                        class_member::GETTER | class_member::SETTER => self.emit(
                            Opcode::DefineClassAccessor,
                            &[
                                i64::from(target),
                                i64::from(constant),
                                i64::from(kind == class_member::SETTER),
                            ],
                        ),
                        _ => self.emit(
                            Opcode::DefineMethod,
                            &[i64::from(target), i64::from(constant)],
                        ),
                    }
                }
            } else if let Some(register) = key_register {
                match kind {
                    class_member::GETTER | class_member::SETTER => self.emit(
                        Opcode::DefineClassAccessorKeyed,
                        &[
                            i64::from(target),
                            i64::from(register),
                            i64::from(kind == class_member::SETTER),
                        ],
                    ),
                    _ => self.emit(
                        Opcode::DefineMethodKeyed,
                        &[i64::from(target), i64::from(register)],
                    ),
                }
            }
            self.release(inner);
        }
        self.emit(Opcode::Ldar, &[i64::from(ctor)]);
        if named {
            // The scope closes around everything the members captured.
            self.pop_context();
            self.scope = outer_scope;
        }
        self.strict = was_strict;
        self.release(mark);
    }

    fn declare_global_target(&mut self, target: u32) {
        let node = self.node(target);
        match node.kind {
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
                        NodeKind::PatternProperty => {
                            let element = self.node(record.second);
                            self.declare_global_target(element.first);
                        }
                        // A rest element or a binding element: its target.
                        _ => self.declare_global_target(record.first),
                    }
                }
            }
            _ => {
                if matches!(self.resolve(node.first, node.second), Resolved::Slot { .. }) {
                    return;
                }
                let constant = self.identifier_constant(&node);
                // Eval code declares into whatever variable environment the
                // site had; a script's `var` is a property of the global
                // object however dynamic its names are.
                let opcode = if self.dynamic_names && self.program.eval_goal {
                    Opcode::DeclareEvalVar
                } else {
                    Opcode::DeclareGlobal
                };
                self.emit(opcode, &[i64::from(constant)]);
            }
        }
    }

    fn declaration(&mut self, node: &Node) {
        let items = self.arena.list(node.first, node.second);
        let mut index = 0usize;
        while index < items.len() {
            let record = self.node(items[index]);
            let name = self.node(record.first);
            let pattern = matches!(name.kind, NodeKind::ArrayPattern | NodeKind::ObjectPattern);
            let mark = self.registers;
            if record.second == NONE {
                if node.third == declaration::VAR {
                    // A `var` with no initialiser leaves whatever is there.
                    index += 1;
                    continue;
                }
                self.emit(Opcode::LdaUndefined, &[]);
            } else if pattern {
                self.expression(record.second);
            } else if node.third == declaration::VAR && self.with_depth > 0 {
                // The reference forms before the initialiser runs: an object
                // environment that loses the name meanwhile still takes the
                // write, as the specification's resolved reference does.
                let reference = self.prepare_reference(&name);
                self.named_expression(record.second, &name);
                self.write_reference(&name, &reference);
                self.release(mark);
                index += 1;
                continue;
            } else {
                self.named_expression(record.second, &name);
            }
            if pattern {
                self.bind_target(record.first, node.third != declaration::VAR);
            } else if node.third == declaration::VAR {
                self.store_name(&name);
            } else {
                self.initialise_name(&name);
            }
            if matches!(node.third, declaration::USING | declaration::AWAIT_USING)
                && self.dispose_stack != u32::MAX
            {
                // The resource joins its list's stack, checked for its
                // disposer now rather than when it is disposed.
                let stack = self.dispose_stack;
                let opcode = if node.third == declaration::AWAIT_USING {
                    Opcode::AddDisposableAsync
                } else {
                    Opcode::AddDisposable
                };
                self.emit(opcode, &[i64::from(stack)]);
            }
            self.release(mark);
            index += 1;
        }
    }

    fn if_statement(&mut self, node: &Node) {
        let mark = self.registers;
        self.expression(node.first);
        self.release(mark);
        let otherwise = self.builder.label();
        self.builder.jump(Opcode::JumpIfToBooleanFalse, otherwise);
        self.statement(node.second);
        if node.third == NONE {
            self.builder.bind(otherwise);
            return;
        }
        // A consequent that returned, threw, or jumped away needs no jump
        // over the alternate, and the join is then the alternate's own end:
        // binding an unused label there would make whatever follows look
        // reachable when only the alternate decides that.
        let joins = !self.builder.terminated();
        let done = self.builder.label();
        if joins {
            self.builder.jump(Opcode::Jump, done);
        }
        self.builder.bind(otherwise);
        self.statement(node.third);
        if joins {
            self.builder.bind(done);
        }
    }

    fn while_statement(&mut self, node: &Node, label_start: u32, label_end: u32) {
        let top = self.builder.label();
        let done = self.builder.label();
        let again = self.builder.label();
        self.builder.safe_point();
        self.builder.bind(top);
        let mark = self.registers;
        self.expression(node.first);
        self.release(mark);
        self.builder.jump(Opcode::JumpIfToBooleanFalse, done);
        self.open_target(Target {
            label_start,
            label_end,
            breakable: true,
            continuable: true,
            break_label: done,
            continue_label: again,
            context_depth: self.context_depth,
            finaliser_depth: u32::try_from(self.finaliser_count).unwrap_or(0),
            break_used: false,
            continue_used: false,
        });
        self.statement(node.second);
        let (_break_used, continue_used) = self.target_used();
        self.close_target();
        if continue_used || !self.builder.terminated() {
            self.builder.bind(again);
            self.builder.jump(Opcode::Jump, top);
        }
        // The test always jumps here when it fails, so this is always reached.
        self.builder.bind(done);
    }

    fn do_while_statement(&mut self, node: &Node, label_start: u32, label_end: u32) {
        let top = self.builder.label();
        let again = self.builder.label();
        let done = self.builder.label();
        self.builder.safe_point();
        self.builder.bind(top);
        self.open_target(Target {
            label_start,
            label_end,
            breakable: true,
            continuable: true,
            break_label: done,
            continue_label: again,
            context_depth: self.context_depth,
            finaliser_depth: u32::try_from(self.finaliser_count).unwrap_or(0),
            break_used: false,
            continue_used: false,
        });
        self.statement(node.first);
        let (break_used, continue_used) = self.target_used();
        self.close_target();
        if continue_used || !self.builder.terminated() {
            self.builder.bind(again);
            let mark = self.registers;
            self.expression(node.second);
            self.release(mark);
            self.builder.jump(Opcode::JumpIfToBooleanTrue, top);
        }
        if break_used || !self.builder.terminated() {
            self.builder.bind(done);
        }
    }

    fn for_statement(&mut self, node: &Node, label_start: u32, label_end: u32) {
        let parts = self.arena.list(node.second, node.third);
        let test = parts.first().copied().unwrap_or(NONE);
        let update = parts.get(1).copied().unwrap_or(NONE);
        let body = parts.get(2).copied().unwrap_or(NONE);

        // A `let` in the header belongs to the loop, not to what surrounds it.
        let outer = self.scope;
        let mut pushed = false;
        let initialiser = self.node(node.first);
        if node.first != NONE
            && matches!(initialiser.kind, NodeKind::Declaration)
            && initialiser.third != declaration::VAR
        {
            let scope = match self.program.open_scope(outer) {
                Ok(scope) => scope,
                Err(diagnostic) => {
                    if self.program.failure.is_none() {
                        self.program.failure = Some(diagnostic);
                    }
                    return;
                }
            };
            self.scope = scope;
            let kind = if matches!(
                initialiser.third,
                declaration::CONST | declaration::USING | declaration::AWAIT_USING
            ) {
                binding_kind::CONST
            } else {
                binding_kind::LET
            };
            for offset in 0..initialiser.second {
                let Some(&declarator) = self
                    .arena
                    .list(initialiser.first, initialiser.second)
                    .get(offset as usize)
                else {
                    break;
                };
                let record = self.node(declarator);
                if let Err(diagnostic) =
                    declare_pattern(self.arena, record.first, kind, scope, self.program)
                {
                    if self.program.failure.is_none() {
                        self.program.failure = Some(diagnostic);
                    }
                    return;
                }
            }
            let slots = self.program.scope(scope).count;
            if slots > 0 {
                self.push_context(slots);
                pushed = true;
            }
        }

        // A head declaring with `using` holds its resources for the whole
        // loop, disposed when the loop is left — by its test, a `break`, a
        // `return`, or a throw from the head or the body.
        let disposing = node.first != NONE
            && matches!(initialiser.kind, NodeKind::Declaration)
            && matches!(
                initialiser.third,
                declaration::USING | declaration::AWAIT_USING
            );
        let awaited = disposing && initialiser.third == declaration::AWAIT_USING;
        let mut stack = u32::MAX;
        let mut caught = u32::MAX;
        let outer_stack = self.dispose_stack;
        let mut region_start = 0u32;
        let region_depth = self.context_depth;
        if disposing {
            stack = self.allocate();
            caught = self.allocate();
            self.emit(Opcode::CreateDisposeStack, &[]);
            self.emit(Opcode::Star, &[i64::from(stack)]);
            let finaliser = Finaliser {
                node: NONE,
                context_depth: self.context_depth,
                try_depth: self.try_depth,
                scope: self.scope,
                iterator: stack,
                flag: if awaited {
                    DISPOSE_STACK_ASYNC
                } else {
                    DISPOSE_STACK
                },
            };
            match self.finalisers.get_mut(self.finaliser_count) {
                Some(slot) => {
                    *slot = finaliser;
                    self.finaliser_count += 1;
                }
                None => {
                    self.fail(node, code::EXPRESSION_TOO_DEEP);
                    return;
                }
            }
            self.dispose_stack = stack;
            self.try_depth += 1;
            region_start = self.builder.length();
        }

        if node.first != NONE {
            if matches!(initialiser.kind, NodeKind::Declaration) {
                self.declaration(&initialiser);
                self.dispose_stack = outer_stack;
            } else {
                let mark = self.registers;
                self.expression(node.first);
                self.release(mark);
            }
        }

        // Each turn of a loop whose header declares with `let` gets bindings of
        // its own, so a closure made in the body keeps the value that turn had
        // rather than the one the loop stopped at.
        let per_iteration = if pushed {
            self.program.scope(self.scope).count
        } else {
            0
        };
        if per_iteration > 0 {
            self.copy_iteration(per_iteration);
        }

        let top = self.builder.label();
        let again = self.builder.label();
        let done = self.builder.label();
        self.builder.safe_point();
        self.builder.bind(top);
        if test != NONE {
            let mark = self.registers;
            self.expression(test);
            self.release(mark);
            self.builder.jump(Opcode::JumpIfToBooleanFalse, done);
        }
        self.open_target(Target {
            label_start,
            label_end,
            breakable: true,
            continuable: true,
            break_label: done,
            continue_label: again,
            context_depth: self.context_depth,
            finaliser_depth: u32::try_from(self.finaliser_count).unwrap_or(0),
            break_used: false,
            continue_used: false,
        });
        self.statement(body);
        let (break_used, continue_used) = self.target_used();
        self.close_target();
        if continue_used || !self.builder.terminated() {
            self.builder.bind(again);
            if per_iteration > 0 {
                self.copy_iteration(per_iteration);
            }
            if update != NONE {
                let mark = self.registers;
                self.expression(update);
                self.release(mark);
            }
            self.builder.jump(Opcode::Jump, top);
        }
        if break_used || test != NONE || !self.builder.terminated() {
            self.builder.bind(done);
        }
        if disposing {
            let region_end = self.builder.length();
            self.try_depth = self.try_depth.saturating_sub(1);
            self.finaliser_count = self.finaliser_count.saturating_sub(1);
            if self.program.failure.is_some() {
                return;
            }
            let after = self.builder.label();
            let mut fell_through = false;
            if !self.builder.terminated() {
                self.emit_dispose(stack, None, awaited);
                self.builder.jump(Opcode::Jump, after);
                fell_through = true;
            }
            if region_start != region_end {
                let handler = self.builder.length();
                self.emit_dispose(stack, Some(caught), awaited);
                if self.record_region(region_start, region_end, handler, caught, region_depth) {
                    return;
                }
            }
            if fell_through {
                self.builder.bind(after);
            }
        }
        if pushed {
            self.pop_context();
        }
        self.scope = outer;
    }

    /// Replace the current context with a fresh one holding the same values.
    fn copy_iteration(&mut self, slots: u32) {
        let mark = self.registers;
        let first = self.registers;
        let mut index = 0u32;
        while index < slots {
            let register = self.allocate();
            self.emit(Opcode::LdaContextSlot, &[i64::from(index), 0]);
            self.emit(Opcode::Star, &[i64::from(register)]);
            index += 1;
        }
        self.pop_context();
        self.push_context(slots);
        let mut index = 0u32;
        while index < slots {
            self.emit(Opcode::Ldar, &[i64::from(first + index)]);
            self.emit(Opcode::InitContextSlot, &[i64::from(index), 0]);
            index += 1;
        }
        self.release(mark);
    }

    /// `for (x of y)` and `for (x in y)`.
    ///
    /// Both walk something the expression produces: an iterator for `of`, and
    /// the enumerable names for `in`. A header that declares with `let` or
    /// `const` gets bindings of its own each turn, as in an ordinary `for`.
    fn for_in_of_statement(&mut self, node: &Node, label_start: u32, label_end: u32) {
        let of = node.has(flag::OF);
        let mark = self.registers;
        let source = self.allocate();
        let flag_register = self.allocate();
        // A head declaring with `using` disposes each turn's resource as the
        // turn ends: the stack lives here, made afresh per turn.
        let stack = self.allocate();

        let declaration_node = self.node(node.first);
        let declares = matches!(declaration_node.kind, NodeKind::Declaration);
        let using = declares
            && matches!(
                declaration_node.third,
                declaration::USING | declaration::AWAIT_USING
            );
        let awaited = declares && declaration_node.third == declaration::AWAIT_USING;
        let lexical = declares && declaration_node.third != declaration::VAR;
        let name_node = if declares {
            let declarator = self
                .arena
                .list(declaration_node.first, declaration_node.second)
                .first()
                .copied()
                .unwrap_or(NONE);
            self.node(declarator).first
        } else {
            node.first
        };

        let outer = self.scope;
        let mut scope = NONE;
        if lexical {
            scope = match self.program.open_scope(outer) {
                Ok(scope) => scope,
                Err(diagnostic) => {
                    if self.program.failure.is_none() {
                        self.program.failure = Some(diagnostic);
                    }
                    return;
                }
            };
            let kind = if matches!(
                declaration_node.third,
                declaration::CONST | declaration::USING | declaration::AWAIT_USING
            ) {
                binding_kind::CONST
            } else {
                binding_kind::LET
            };
            if let Err(diagnostic) =
                declare_pattern(self.arena, name_node, kind, scope, self.program)
            {
                if self.program.failure.is_none() {
                    self.program.failure = Some(diagnostic);
                }
                return;
            }
        }

        // The head's source expression runs where the bound name is already
        // declared and not yet initialised: `for (const x in { a: x })` is a
        // read in the dead zone, not a read of an outer `x`.
        if lexical {
            self.scope = scope;
            // A pushed environment counts in resolution even when the
            // pattern declares nothing.
            if let Some(record) = self.program.scopes.get_mut(scope as usize) {
                record.context = true;
            }
            let slots = self.program.scope(scope).count.max(1);
            self.push_context(slots);
            self.expression(node.second);
            self.pop_context();
            self.scope = outer;
        } else {
            self.expression(node.second);
        }
        // `for in` keeps the object it walks, so a key deleted before its
        // turn can be seen to have gone.
        let subject = if of { u32::MAX } else { self.allocate() };
        if of && node.has(flag::FOR_AWAIT) {
            self.emit(Opcode::GetAsyncIterator, &[]);
        } else if of {
            self.emit(Opcode::GetIterator, &[]);
        } else {
            self.emit(Opcode::Star, &[i64::from(subject)]);
            self.emit(Opcode::GetEnumerable, &[]);
        }
        self.emit(Opcode::Star, &[i64::from(source)]);
        // `for of` fetches `next` once, into the iterator record the loop
        // then calls: a getter that answers differently later is not asked.
        let next_method = if of { self.allocate() } else { u32::MAX };
        if of {
            let next_key = self.text_key_constant(b"next");
            self.emit(
                Opcode::GetNamedProperty,
                &[i64::from(source), i64::from(next_key)],
            );
            self.emit(Opcode::Star, &[i64::from(next_method)]);
        }

        // `for (x in y)` walks an array of names, which needs a position.
        let position = if of { u32::MAX } else { self.allocate() };
        let limit = if of { u32::MAX } else { self.allocate() };
        let caught = if of { self.allocate() } else { u32::MAX };
        if !of {
            self.emit(Opcode::LdaZero, &[]);
            self.emit(Opcode::Star, &[i64::from(position)]);
            let length = self.length_key_constant();
            self.emit(
                Opcode::GetNamedProperty,
                &[i64::from(source), i64::from(length)],
            );
            self.emit(Opcode::Star, &[i64::from(limit)]);
        }

        let top = self.builder.label();
        let again = self.builder.label();
        let done = self.builder.label();
        self.builder.safe_point();
        self.builder.bind(top);

        let value = self.allocate();
        if of && node.has(flag::FOR_AWAIT) {
            // `for await`: the iterator's result is awaited before its done
            // flag and value are read.
            let inner = self.registers;
            let callee = self.allocate();
            let receiver = self.allocate();
            let result = self.allocate();
            self.emit(Opcode::Ldar, &[i64::from(next_method)]);
            self.emit(Opcode::Star, &[i64::from(callee)]);
            self.emit(Opcode::Ldar, &[i64::from(source)]);
            self.emit(Opcode::Star, &[i64::from(receiver)]);
            self.builder.safe_point();
            self.emit(Opcode::Call, &[i64::from(callee), i64::from(receiver), 1]);
            self.builder.safe_point();
            self.emit(Opcode::Await, &[]);
            self.emit(Opcode::RequireObject, &[]);
            self.emit(Opcode::Star, &[i64::from(result)]);
            let done_key = self.text_key_constant(b"done");
            self.emit(
                Opcode::GetNamedProperty,
                &[i64::from(result), i64::from(done_key)],
            );
            self.emit(Opcode::Star, &[i64::from(flag_register)]);
            self.builder.jump(Opcode::JumpIfToBooleanTrue, done);
            let value_key = self.text_key_constant(b"value");
            self.emit(
                Opcode::GetNamedProperty,
                &[i64::from(result), i64::from(value_key)],
            );
            self.emit(Opcode::Star, &[i64::from(value)]);
            self.release(inner);
        } else if of {
            let inner = self.registers;
            let callee = self.allocate();
            let receiver = self.allocate();
            let result = self.allocate();
            self.emit(Opcode::Ldar, &[i64::from(next_method)]);
            self.emit(Opcode::Star, &[i64::from(callee)]);
            self.emit(Opcode::Ldar, &[i64::from(source)]);
            self.emit(Opcode::Star, &[i64::from(receiver)]);
            self.builder.safe_point();
            self.emit(Opcode::Call, &[i64::from(callee), i64::from(receiver), 1]);
            self.emit(Opcode::RequireObject, &[]);
            self.emit(Opcode::Star, &[i64::from(result)]);
            let done_key = self.text_key_constant(b"done");
            self.emit(
                Opcode::GetNamedProperty,
                &[i64::from(result), i64::from(done_key)],
            );
            self.emit(Opcode::Star, &[i64::from(flag_register)]);
            self.builder.jump(Opcode::JumpIfToBooleanTrue, done);
            let value_key = self.text_key_constant(b"value");
            self.emit(
                Opcode::GetNamedProperty,
                &[i64::from(result), i64::from(value_key)],
            );
            self.emit(Opcode::Star, &[i64::from(value)]);
            self.release(inner);
        } else {
            // A comparison takes its left operand from the register and its
            // right from the accumulator.
            self.emit(Opcode::Ldar, &[i64::from(limit)]);
            self.emit(Opcode::TestLess, &[i64::from(position)]);
            self.builder.jump(Opcode::JumpIfFalse, done);
            self.emit(Opcode::Ldar, &[i64::from(position)]);
            self.emit(Opcode::GetKeyedProperty, &[i64::from(source)]);
            self.emit(Opcode::Star, &[i64::from(value)]);
            // A key deleted before its turn is passed over.
            let present = self.builder.label();
            self.emit(Opcode::ForInHas, &[i64::from(subject)]);
            self.builder.jump(Opcode::JumpIfTrue, present);
            self.emit(Opcode::Ldar, &[i64::from(position)]);
            self.emit(Opcode::Inc, &[]);
            self.emit(Opcode::Star, &[i64::from(position)]);
            self.builder.jump(Opcode::Jump, top);
            self.builder.bind(present);
        }

        // From here until the body ends, an exit that escapes the loop —
        // break or continue to an outer label, a return, or a throw —
        // closes the iterator the loop still holds.
        let escape_depth = self.context_depth;
        let body_start = if of {
            match self.finalisers.get_mut(self.finaliser_count) {
                Some(slot) => {
                    *slot = Finaliser {
                        node: NONE,
                        context_depth: escape_depth,
                        try_depth: self.try_depth,
                        scope: self.scope,
                        iterator: source,
                        flag: flag_register,
                    };
                    self.finaliser_count += 1;
                }
                None => {
                    self.fail(node, code::EXPRESSION_TOO_DEEP);
                    return;
                }
            }
            self.builder.length()
        } else {
            0
        };

        // The name takes the turn's value, in a binding of its own where the
        // header declared one.
        if lexical {
            self.scope = scope;
            // A pushed environment counts in resolution even when the
            // pattern declares nothing.
            if let Some(record) = self.program.scopes.get_mut(scope as usize) {
                record.context = true;
            }
            let slots = self.program.scope(scope).count.max(1);
            self.push_context(slots);
            self.emit(Opcode::Ldar, &[i64::from(value)]);
            self.bind_target(name_node, true);
            if using {
                self.emit(Opcode::CreateDisposeStack, &[]);
                self.emit(Opcode::Star, &[i64::from(stack)]);
                self.emit(Opcode::Ldar, &[i64::from(value)]);
                let opcode = if awaited {
                    Opcode::AddDisposableAsync
                } else {
                    Opcode::AddDisposable
                };
                self.emit(opcode, &[i64::from(stack)]);
                // An exit from the body disposes the turn's resource before
                // the iterator closes.
                match self.finalisers.get_mut(self.finaliser_count) {
                    Some(slot) => {
                        *slot = Finaliser {
                            node: NONE,
                            context_depth: self.context_depth,
                            try_depth: self.try_depth,
                            scope: self.scope,
                            iterator: stack,
                            flag: if awaited {
                                DISPOSE_STACK_ASYNC
                            } else {
                                DISPOSE_STACK
                            },
                        };
                        self.finaliser_count += 1;
                    }
                    None => {
                        self.fail(node, code::EXPRESSION_TOO_DEEP);
                        return;
                    }
                }
            }
        } else if declares {
            self.emit(Opcode::Ldar, &[i64::from(value)]);
            self.bind_target(name_node, false);
        } else {
            self.emit(Opcode::Ldar, &[i64::from(value)]);
            let target = self.node(name_node);
            if matches!(target.kind, NodeKind::Array | NodeKind::Object) {
                self.assign_target(name_node);
            } else {
                self.store(&target, name_node);
            }
        }

        self.open_target(Target {
            label_start,
            label_end,
            breakable: true,
            continuable: true,
            break_label: done,
            continue_label: again,
            context_depth: if lexical {
                self.context_depth.saturating_sub(1)
            } else {
                self.context_depth
            },
            finaliser_depth: u32::try_from(self.finaliser_count).unwrap_or(0),
            break_used: false,
            continue_used: false,
        });
        self.statement(node.third);
        let body_end = self.builder.length();
        if using {
            self.finaliser_count = self.finaliser_count.saturating_sub(1);
        }
        if of {
            self.finaliser_count = self.finaliser_count.saturating_sub(1);
        }
        let (_break_used, continue_used) = self.target_used();
        self.close_target();

        if continue_used || !self.builder.terminated() {
            // The turn's binding is left before `again`, so a fall off the end
            // of the body and a `continue` — which unwinds to the depth
            // outside the binding — arrive at the same depth.
            if using {
                self.emit_dispose(stack, None, awaited);
            }
            if lexical {
                self.pop_context();
            }
            self.builder.bind(again);
            if !of {
                self.emit(Opcode::Ldar, &[i64::from(position)]);
                self.emit(Opcode::Inc, &[]);
                self.emit(Opcode::Star, &[i64::from(position)]);
            }
            self.builder.jump(Opcode::Jump, top);
        } else if lexical {
            self.context_depth = self.context_depth.saturating_sub(1);
        }
        self.builder.bind(done);
        if using {
            // A `break` leaves the turn's resource to dispose; an exhausted
            // loop arrives with the last turn's stack already emptied.
            self.emit_dispose(stack, None, awaited);
        }
        if of {
            // A loop left early — by `break`, most often — closes the
            // iterator; one that exhausted itself already reported done.
            self.emit(
                Opcode::IteratorClose,
                &[i64::from(source), i64::from(flag_register)],
            );
            // A throw from the body closes the iterator too, quietly, and
            // throws on: the body's reason outranks the close's own.
            if body_start != body_end {
                let after = self.builder.label();
                self.builder.jump(Opcode::Jump, after);
                let handler = self.builder.length();
                self.emit(
                    Opcode::IteratorCloseQuiet,
                    &[i64::from(source), i64::from(flag_register)],
                );
                if using {
                    // The turn's resource is disposed while the throw
                    // propagates, a disposer's own throw suppressing it.
                    self.emit_dispose(stack, Some(caught), awaited);
                } else {
                    self.emit(Opcode::Ldar, &[i64::from(caught)]);
                    self.emit(Opcode::Throw, &[]);
                }
                if self.record_region(body_start, body_end, handler, caught, escape_depth) {
                    return;
                }
                self.builder.bind(after);
            }
        }
        self.scope = outer;
        self.release(mark);
    }

    fn break_or_continue(&mut self, node: &Node) {
        let wants_continue = matches!(node.kind, NodeKind::Continue);
        let (label_start, label_end) = if node.first == NONE {
            (0, 0)
        } else {
            let label = self.node(node.first);
            (label.first, label.second)
        };
        let named = node.first != NONE;

        let mut index = self.target_count;
        while index > 0 {
            index -= 1;
            let Some(target) = self.targets.get(index).copied() else {
                break;
            };
            let matches_label = if named {
                self.span(target.label_start, target.label_end) == self.span(label_start, label_end)
                    && target.label_end > target.label_start
            } else if wants_continue {
                target.continuable
            } else {
                target.breakable
            };
            if !matches_label {
                continue;
            }
            if wants_continue && !target.continuable {
                self.fail(node, code::ILLEGAL_BREAK_OR_CONTINUE);
                return;
            }
            // Leaving a scope means leaving its contexts, and running any
            // `finally` that the jump escapes.
            let live = !self.builder.terminated();
            self.unwind_to(target.finaliser_depth, target.context_depth);
            if let Some(slot) = self.targets.get_mut(index) {
                if wants_continue {
                    slot.continue_used = true;
                } else {
                    slot.break_used = true;
                }
            }
            let destination = if wants_continue {
                target.continue_label
            } else {
                target.break_label
            };
            // A finaliser on the way out may itself have jumped away — a
            // `continue` inside `finally` overrides the `break` — and then
            // nothing follows it.
            if !(live && self.builder.terminated()) {
                self.builder.jump(Opcode::Jump, destination);
            }
            return;
        }
        let failure = if named {
            code::UNDECLARED_LABEL
        } else {
            code::ILLEGAL_BREAK_OR_CONTINUE
        };
        self.fail(node, failure);
    }

    /// Run every `finally` an exit escapes, and pop every context it leaves.
    ///
    /// The finalisers run innermost first, each at the context depth it was
    /// written at, which is what makes a `break` out of a `try` behave like
    /// reaching its end.
    ///
    /// The pops belong to the exiting path alone: the statements lowered after
    /// the jump sit on other paths, which still hold every context this exit
    /// left. The tracked depth is therefore put back when the unwind is done.
    fn unwind_to(&mut self, finaliser_depth: u32, context_depth: u32) {
        let restore = self.context_depth;
        self.unwind_only(finaliser_depth, context_depth);
        self.context_depth = restore;
    }

    fn unwind_only(&mut self, finaliser_depth: u32, context_depth: u32) {
        let mut index = self.finaliser_count;
        while index > finaliser_depth as usize {
            index -= 1;
            let Some(finaliser) = self.finalisers.get(index).copied() else {
                break;
            };
            while self.context_depth > finaliser.context_depth {
                self.pop_context();
            }
            if finaliser.iterator != u32::MAX {
                // An iterator the exit escapes closes here — or a dispose
                // stack empties. The close is a hole in the iterator's own
                // regions — a throw from it must not close again — while an
                // enclosing `try` still covers it.
                let from = self.builder.length();
                if matches!(finaliser.flag, DISPOSE_STACK | DISPOSE_STACK_ASYNC) {
                    self.emit_dispose(
                        finaliser.iterator,
                        None,
                        finaliser.flag == DISPOSE_STACK_ASYNC,
                    );
                } else {
                    self.emit(
                        Opcode::IteratorClose,
                        &[i64::from(finaliser.iterator), i64::from(finaliser.flag)],
                    );
                }
                let to = self.builder.length();
                match self.holes.get_mut(self.hole_count) {
                    Some(slot) => {
                        *slot = (finaliser.try_depth, from, to);
                        self.hole_count += 1;
                    }
                    None => {
                        let node = Node::new(NodeKind::Null, 0, 0);
                        self.fail(&node, code::EXPRESSION_TOO_DEEP);
                    }
                }
                continue;
            }
            let saved = self.finaliser_count;
            self.finaliser_count = index;
            // The copy runs after this exit has left every `try` inward of
            // the finaliser's own, so no region that deep may cover it — and
            // it lowers in the scope the `try` was entered in, matching the
            // contexts the pops above left standing.
            let outer_scope = self.scope;
            self.scope = finaliser.scope;
            let from = self.builder.length();
            self.finally_block(finaliser.node);
            let to = self.builder.length();
            self.scope = outer_scope;
            match self.holes.get_mut(self.hole_count) {
                Some(slot) => {
                    *slot = (finaliser.try_depth, from, to);
                    self.hole_count += 1;
                }
                None => {
                    let node = Node::new(NodeKind::Null, 0, 0);
                    self.fail(&node, code::EXPRESSION_TOO_DEEP);
                }
            }
            self.finaliser_count = saved;
        }
        while self.context_depth > context_depth {
            self.pop_context();
        }
    }

    fn return_statement(&mut self, node: &Node) {
        if !self.in_function {
            self.fail(node, code::RETURN_OUTSIDE_FUNCTION);
            return;
        }
        let mark = self.registers;
        if node.first == NONE {
            self.emit(Opcode::LdaUndefined, &[]);
        } else {
            // A call the return's value comes straight from may give up this
            // frame — unless a finaliser must still run in it.
            self.tail_span_count = 0;
            if self.tail_calls && self.finaliser_count == 0 {
                self.mark_tail_positions(node.first);
            }
            self.expression(node.first);
            self.tail_span_count = 0;
            if self.in_async_generator {
                // An async generator awaits what it returns; a bare
                // `return` has nothing to await.
                self.builder.safe_point();
                self.emit(Opcode::Await, &[]);
            }
        }
        // The value is held while the finalisers run, because one of them may
        // use the same registers.
        if self.finaliser_count > 0 || self.context_depth > 0 {
            let value = self.allocate();
            self.emit(Opcode::Star, &[i64::from(value)]);
            self.unwind_to(0, 0);
            if self.builder.terminated() {
                // A finaliser that broke out of a loop takes the exit with it,
                // and the return never happens.
                self.release(mark);
                return;
            }
            self.emit(Opcode::Ldar, &[i64::from(value)]);
        }
        self.emit(Opcode::Return, &[]);
        self.release(mark);
    }

    /// `try`, with a catch clause, a finally block, or both.
    ///
    /// The finaliser runs on every path out: falling off the end, catching,
    /// throwing on, and any `break`, `continue`, or `return` that escapes,
    /// which is why it is written into the code at each of those points rather
    /// than jumped to.
    fn try_statement(&mut self, node: &Node) {
        let has_finally = node.third != NONE;
        let has_catch = node.second != NONE;
        let normal = self.builder.label();
        let mut normal_used = false;
        let mark = self.registers;
        self.try_depth += 1;

        if has_finally {
            match self.finalisers.get_mut(self.finaliser_count) {
                Some(slot) => {
                    *slot = Finaliser {
                        node: node.third,
                        context_depth: self.context_depth,
                        try_depth: self.try_depth,
                        scope: self.scope,
                        iterator: u32::MAX,
                        flag: u32::MAX,
                    };
                    self.finaliser_count += 1;
                }
                None => {
                    self.fail(node, code::EXPRESSION_TOO_DEEP);
                    return;
                }
            }
        }

        let exception = self.allocate();
        let region_depth = self.context_depth;
        let region_start = self.builder.length();
        self.statement(node.first);
        let region_end = self.builder.length();
        if !self.builder.terminated() {
            self.builder.jump(Opcode::Jump, normal);
            normal_used = true;
        }

        // An empty protected range can throw nothing: no region, no handler,
        // and no catch code nothing could reach.
        if region_start == region_end {
            if has_finally {
                self.finaliser_count = self.finaliser_count.saturating_sub(1);
            }
            if normal_used {
                self.builder.bind(normal);
                if has_finally {
                    self.finally_block(node.third);
                }
            }
            self.try_depth = self.try_depth.saturating_sub(1);
            self.release(mark);
            return;
        }

        // The handler is where a throw inside the protected range continues.
        let handler_offset = self.builder.length();
        if self.record_region(
            region_start,
            region_end,
            handler_offset,
            exception,
            region_depth,
        ) {
            return;
        }

        if has_catch {
            let clause = self.node(node.second);
            let catch_start = self.builder.length();
            let catch_exception = self.allocate();
            let outer = self.scope;
            let mut pushed = false;
            if clause.first != NONE {
                let scope = match self.program.open_scope(outer) {
                    Ok(scope) => scope,
                    Err(diagnostic) => {
                        if self.program.failure.is_none() {
                            self.program.failure = Some(diagnostic);
                        }
                        return;
                    }
                };
                self.scope = scope;
                if let Err(diagnostic) = declare_pattern(
                    self.arena,
                    clause.first,
                    binding_kind::LET,
                    scope,
                    self.program,
                ) {
                    if self.program.failure.is_none() {
                        self.program.failure = Some(diagnostic);
                    }
                    return;
                }
                // A pushed environment counts in resolution even when the
                // pattern declares nothing.
                if let Some(record) = self.program.scopes.get_mut(scope as usize) {
                    record.context = true;
                }
                let slots = self.program.scope(scope).count.max(1);
                self.push_context(slots);
                pushed = true;
                self.emit(Opcode::Ldar, &[i64::from(exception)]);
                self.bind_target(clause.first, true);
            }
            self.statement(clause.second);
            if !self.builder.terminated() {
                if pushed {
                    self.pop_context();
                }
                self.builder.jump(Opcode::Jump, normal);
                normal_used = true;
            } else if pushed {
                self.context_depth = self.context_depth.saturating_sub(1);
            }
            self.scope = outer;

            if has_finally {
                // A throw from the catch clause is still the `try` statement's
                // to finalise, so the clause has a region of its own.
                let catch_end = self.builder.length();
                let rethrow = self.builder.length();
                if self.record_region(
                    catch_start,
                    catch_end,
                    rethrow,
                    catch_exception,
                    region_depth,
                ) {
                    return;
                }
                self.finaliser_count = self.finaliser_count.saturating_sub(1);
                self.finally_block(node.third);
                self.finaliser_count += 1;
                if !self.builder.terminated() {
                    self.emit(Opcode::Ldar, &[i64::from(catch_exception)]);
                    self.emit(Opcode::Throw, &[]);
                }
            }
        } else {
            // Without a catch, the handler runs the finaliser and throws on.
            self.finaliser_count = self.finaliser_count.saturating_sub(1);
            self.finally_block(node.third);
            self.finaliser_count += 1;
            // A finaliser that jumps away takes the exception with it, which is
            // what a `break` inside one means.
            if !self.builder.terminated() {
                self.emit(Opcode::Ldar, &[i64::from(exception)]);
                self.emit(Opcode::Throw, &[]);
            }
        }

        if has_finally {
            self.finaliser_count = self.finaliser_count.saturating_sub(1);
        }
        if normal_used {
            self.builder.bind(normal);
            if has_finally {
                self.finally_block(node.third);
            }
        }
        self.try_depth = self.try_depth.saturating_sub(1);
        self.release(mark);
    }

    /// Record one exception region, in the order the verifier requires.
    fn record_region(
        &mut self,
        start: u32,
        end: u32,
        handler: u32,
        register: u32,
        context_depth: u32,
    ) -> bool {
        // The range is split around every inline finaliser copy whose owning
        // `try` is this one or one outside it: the copy runs after the exit
        // has left this `try`, so a throw from it belongs to whatever
        // encloses the owner, never to this region.
        let mut cursor = start;
        let mut hole = 0usize;
        while hole < self.hole_count {
            let (owner_depth, from, to) = self.holes[hole];
            hole += 1;
            if owner_depth > self.try_depth || to <= cursor || from >= end {
                continue;
            }
            if from > cursor && self.push_region(cursor, from, handler, register, context_depth) {
                return true;
            }
            cursor = cursor.max(to);
        }
        if cursor < end && self.push_region(cursor, end, handler, register, context_depth) {
            return true;
        }
        false
    }

    fn push_region(
        &mut self,
        start: u32,
        end: u32,
        handler: u32,
        register: u32,
        context_depth: u32,
    ) -> bool {
        let slot = match self
            .program
            .exceptions
            .get_mut(self.program.exception_count)
        {
            Some(slot) => slot,
            None => {
                let node = Node::new(NodeKind::Null, 0, 0);
                self.fail(&node, code::CODE_TOO_LARGE);
                return true;
            }
        };
        *slot = ExceptionRegion {
            start,
            end,
            handler,
            register,
            context_depth,
        };
        self.program.exception_count += 1;
        false
    }

    fn switch_statement(&mut self, node: &Node, label_start: u32, label_end: u32) {
        let cases = self.arena.list(node.second, node.third);
        if cases.len() > MAX_CASES {
            self.fail(node, code::EXPRESSION_TOO_DEEP);
            return;
        }
        let mark = self.registers;
        let discriminant = self.allocate();
        self.expression(node.first);
        self.emit(Opcode::Star, &[i64::from(discriminant)]);

        let done = self.builder.label();
        // Where the dispatch goes when nothing matched and there is no
        // `default`: the point where the switch's context is left, which a
        // jump straight to `done` would skip.
        let fallback = self.builder.label();
        let mut bodies = [Label(0); MAX_CASES];

        // The cases share one scope, and the tests as well as the bodies run
        // inside it: a test that reads a binding a later case declares is a
        // use before initialisation, not a read of an outer name.
        let outer = self.scope;
        let scope = match self.program.open_scope(outer) {
            Ok(scope) => scope,
            Err(diagnostic) => {
                if self.program.failure.is_none() {
                    self.program.failure = Some(diagnostic);
                }
                return;
            }
        };
        self.scope = scope;
        let arena = self.arena;
        let mut index = 0usize;
        while index < cases.len() {
            bodies[index] = self.builder.label();
            let case = self.node(cases[index]);
            if let Err(diagnostic) = declare_lexical(
                arena,
                self.source,
                case.second,
                case.third,
                scope,
                self.program,
                false,
            ) {
                if self.program.failure.is_none() {
                    self.program.failure = Some(diagnostic);
                }
                self.scope = outer;
                return;
            }
            index += 1;
        }
        let slots = self.program.scope(scope).count;

        // The target is opened at the depth outside the switch's context, so a
        // `break` leaves that context on its way to `done`.
        self.open_target(Target {
            label_start,
            label_end,
            breakable: true,
            continuable: false,
            break_label: done,
            continue_label: done,
            context_depth: self.context_depth,
            finaliser_depth: u32::try_from(self.finaliser_count).unwrap_or(0),
            break_used: false,
            continue_used: false,
        });
        if slots > 0 {
            self.push_context(slots);
        }
        let mut index = 0usize;
        while index < cases.len() {
            let case = self.node(cases[index]);
            self.declare_functions(case.second, case.third);
            index += 1;
        }

        let mut default = None;
        let mut index = 0usize;
        while index < cases.len() {
            let case = self.node(cases[index]);
            if case.first == NONE {
                default = Some(index);
            } else {
                self.expression(case.first);
                self.emit(Opcode::TestStrictEqual, &[i64::from(discriminant)]);
                self.builder.jump(Opcode::JumpIfTrue, bodies[index]);
            }
            index += 1;
        }
        match default {
            Some(at) => self.builder.jump(Opcode::Jump, bodies[at]),
            None => self.builder.jump(Opcode::Jump, fallback),
        }

        let mut index = 0usize;
        while index < cases.len() {
            self.builder.bind(bodies[index]);
            let case = self.node(cases[index]);
            self.statements(case.second, case.third);
            index += 1;
        }
        let fell_out = !self.builder.terminated();
        let (break_used, _) = self.target_used();
        self.close_target();

        // The fallback is where the dispatch lands when nothing matched, so a
        // switch without a `default` always reaches it; with one, only a body
        // that falls off the end does.
        if default.is_none() {
            self.builder.bind(fallback);
        }
        let leaves = default.is_none() || fell_out;
        if slots > 0 {
            if leaves {
                self.pop_context();
            } else {
                self.context_depth = self.context_depth.saturating_sub(1);
            }
        }
        self.scope = outer;
        if leaves || break_used {
            self.builder.bind(done);
        }
        self.release(mark);
    }
}
