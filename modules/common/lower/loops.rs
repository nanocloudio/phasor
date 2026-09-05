//! `while`, `do`, `for`, and `for`-`in`/`of`, with the per-iteration bindings the specification gives them.

use super::*;

/// The registers a `for`-`in`/`of` loop keeps across its turns.
pub(super) struct LoopRegisters {
    pub(super) source: u32,
    pub(super) flag_register: u32,
    pub(super) next_method: u32,
    pub(super) subject: u32,
    pub(super) position: u32,
    pub(super) limit: u32,
    pub(super) value: u32,
}

impl Lowering<'_, '_, '_, '_> {
    pub(super) fn while_statement(&mut self, node: &Node, label_start: u32, label_end: u32) {
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

    pub(super) fn do_while_statement(&mut self, node: &Node, label_start: u32, label_end: u32) {
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

    pub(super) fn for_statement(&mut self, node: &Node, label_start: u32, label_end: u32) {
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
    pub(super) fn copy_iteration(&mut self, slots: u32) {
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
    pub(super) fn for_in_of_statement(&mut self, node: &Node, label_start: u32, label_end: u32) {
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
        self.for_in_of_next(
            of,
            node.has(flag::FOR_AWAIT),
            LoopRegisters {
                source,
                flag_register,
                next_method,
                subject,
                position,
                limit,
                value,
            },
            top,
            done,
        );

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

    /// Fetch the turn's value: the iterator's next result, awaited under
    /// `for await`, or the next name of the object `for in` walks, skipping
    /// a key deleted before its turn. Jumps to `done` when there is none.
    pub(super) fn for_in_of_next(
        &mut self,
        of: bool,
        awaited: bool,
        registers: LoopRegisters,
        top: Label,
        done: Label,
    ) {
        if of && awaited {
            // `for await`: the iterator's result is awaited before its done
            // flag and registers.value are read.
            let inner = self.registers;
            let callee = self.allocate();
            let receiver = self.allocate();
            let result = self.allocate();
            self.emit(Opcode::Ldar, &[i64::from(registers.next_method)]);
            self.emit(Opcode::Star, &[i64::from(callee)]);
            self.emit(Opcode::Ldar, &[i64::from(registers.source)]);
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
            self.emit(Opcode::Star, &[i64::from(registers.flag_register)]);
            self.builder.jump(Opcode::JumpIfToBooleanTrue, done);
            let value_key = self.text_key_constant(b"value");
            self.emit(
                Opcode::GetNamedProperty,
                &[i64::from(result), i64::from(value_key)],
            );
            self.emit(Opcode::Star, &[i64::from(registers.value)]);
            self.release(inner);
        } else if of {
            let inner = self.registers;
            let callee = self.allocate();
            let receiver = self.allocate();
            let result = self.allocate();
            self.emit(Opcode::Ldar, &[i64::from(registers.next_method)]);
            self.emit(Opcode::Star, &[i64::from(callee)]);
            self.emit(Opcode::Ldar, &[i64::from(registers.source)]);
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
            self.emit(Opcode::Star, &[i64::from(registers.flag_register)]);
            self.builder.jump(Opcode::JumpIfToBooleanTrue, done);
            let value_key = self.text_key_constant(b"value");
            self.emit(
                Opcode::GetNamedProperty,
                &[i64::from(result), i64::from(value_key)],
            );
            self.emit(Opcode::Star, &[i64::from(registers.value)]);
            self.release(inner);
        } else {
            // A comparison takes its left operand from the register and its
            // right from the accumulator.
            self.emit(Opcode::Ldar, &[i64::from(registers.limit)]);
            self.emit(Opcode::TestLess, &[i64::from(registers.position)]);
            self.builder.jump(Opcode::JumpIfFalse, done);
            self.emit(Opcode::Ldar, &[i64::from(registers.position)]);
            self.emit(Opcode::GetKeyedProperty, &[i64::from(registers.source)]);
            self.emit(Opcode::Star, &[i64::from(registers.value)]);
            // A key deleted before its turn is passed over.
            let present = self.builder.label();
            self.emit(Opcode::ForInHas, &[i64::from(registers.subject)]);
            self.builder.jump(Opcode::JumpIfTrue, present);
            self.emit(Opcode::Ldar, &[i64::from(registers.position)]);
            self.emit(Opcode::Inc, &[]);
            self.emit(Opcode::Star, &[i64::from(registers.position)]);
            self.builder.jump(Opcode::Jump, top);
            self.builder.bind(present);
        }
    }
}
