//! `try`: exception regions, finalisers, and what runs on every way out.

use super::*;

impl Lowering<'_, '_, '_, '_> {
    /// `try`, with a catch clause, a finally block, or both.
    ///
    /// The finaliser runs on every path out: falling off the end, catching,
    /// throwing on, and any `break`, `continue`, or `return` that escapes,
    /// which is why it is written into the code at each of those points rather
    /// than jumped to.
    pub(super) fn try_statement(&mut self, node: &Node) {
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
    pub(super) fn record_region(
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

    pub(super) fn push_region(
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
}
