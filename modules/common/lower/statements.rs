//! Statements: blocks, labels, `if`, `switch`, `with`, `return`, and the completion value a script keeps.

use super::*;

/// Cases one switch may hold.
pub(super) const MAX_CASES: usize = 64;

impl Lowering<'_, '_, '_, '_> {
    pub(super) fn statements(&mut self, list: u32, length: u32) {
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

    pub(super) fn statement_list(&mut self, list: u32, length: u32) {
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
    pub(super) fn disposing_statements(&mut self, list: u32, length: u32, awaited: bool) {
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
    pub(super) fn emit_dispose(&mut self, stack: u32, caught: Option<u32>, awaited: bool) {
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

    pub(super) fn statement(&mut self, index: u32) {
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
    pub(super) fn finally_block(&mut self, body: u32) {
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

    pub(super) fn reset_completion(&mut self) {
        if self.in_function || self.completion == u32::MAX {
            return;
        }
        self.emit(Opcode::LdaUndefined, &[]);
        let completion = self.completion;
        self.emit(Opcode::Star, &[i64::from(completion)]);
    }

    pub(super) fn labelled_statement(&mut self, node: &Node) {
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

    pub(super) fn open_target(&mut self, target: Target) {
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
    pub(super) fn target_used(&self) -> (bool, bool) {
        match self.targets.get(self.target_count.saturating_sub(1)) {
            Some(target) => (target.break_used, target.continue_used),
            None => (false, false),
        }
    }

    pub(super) fn close_target(&mut self) {
        self.target_count = self.target_count.saturating_sub(1);
    }

    pub(super) fn block(&mut self, node: &Node) {
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

    /// `with`: the object joins the scope chain as an object environment,
    /// and every name in the body that could resolve past it goes through
    /// the run-time chain. Strict code refuses the statement.
    pub(super) fn with_statement(&mut self, node: &Node) {
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

    pub(super) fn declare_global_target(&mut self, target: u32) {
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

    pub(super) fn declaration(&mut self, node: &Node) {
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

    pub(super) fn if_statement(&mut self, node: &Node) {
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

    pub(super) fn break_or_continue(&mut self, node: &Node) {
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
    pub(super) fn unwind_to(&mut self, finaliser_depth: u32, context_depth: u32) {
        let restore = self.context_depth;
        self.unwind_only(finaliser_depth, context_depth);
        self.context_depth = restore;
    }

    pub(super) fn unwind_only(&mut self, finaliser_depth: u32, context_depth: u32) {
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

    pub(super) fn return_statement(&mut self, node: &Node) {
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

    pub(super) fn switch_statement(&mut self, node: &Node, label_start: u32, label_end: u32) {
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
