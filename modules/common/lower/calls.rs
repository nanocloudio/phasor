//! Calls and construction, tail positions, and the eval-site records a direct eval needs.

use super::*;

/// Registers a frame that reads `arguments` reserves, matching the most
/// arguments one call may pass.
pub(super) const MAX_CALL_ARGUMENTS: u32 = 16;

/// Bindings one eval-site record may hold.
pub(super) const MAX_EVAL_BINDINGS: u32 = 64;

/// Whether a name is already recorded in the site being built.
pub(super) fn site_holds(blob: &[u8], bindings_at: usize, end: usize, name: &[u8]) -> bool {
    let mut at = bindings_at;
    while at + 16 <= end {
        let length =
            u32::from_le_bytes([blob[at + 12], blob[at + 13], blob[at + 14], blob[at + 15]])
                as usize;
        if blob.get(at + 16..at + 16 + length) == Some(name) {
            return true;
        }
        at += 16 + length;
    }
    false
}

/// Append one binding record, answering whether it fit.
pub(super) fn push_site_binding(
    blob: &mut [u8],
    at: &mut usize,
    slot: u32,
    depth: u32,
    kind: u32,
    name: &[u8],
) -> bool {
    let needed = 16 + name.len();
    let Some(target) = blob.get_mut(*at..*at + needed) else {
        return false;
    };
    target[0..4].copy_from_slice(&slot.to_le_bytes());
    target[4..8].copy_from_slice(&depth.to_le_bytes());
    target[8..12].copy_from_slice(&kind.to_le_bytes());
    target[12..16].copy_from_slice(&u32::try_from(name.len()).unwrap_or(0).to_le_bytes());
    target[16..].copy_from_slice(name);
    *at += needed;
    true
}

impl Lowering<'_, '_, '_, '_> {
    pub(super) fn call(&mut self, node: &Node) {
        let arguments = self.arena.list(node.second, node.third);
        let callee_node = self.node(node.first);
        let mark = self.registers;
        let chain = self.enter_chain(node);
        let callee = self.allocate();
        let receiver = self.allocate();

        if matches!(callee_node.kind, NodeKind::SuperMember) {
            // A super method call reads through the home object but runs on
            // this frame's `this`.
            self.emit(Opcode::LdaThis, &[]);
            self.emit(Opcode::Star, &[i64::from(receiver)]);
            let constant = self.identifier_constant(&callee_node);
            self.emit(Opcode::LdaSuperProperty, &[i64::from(constant)]);
            if node.has(flag::OPTIONAL) {
                self.chain_link();
            }
            self.emit(Opcode::Star, &[i64::from(callee)]);
        } else if matches!(callee_node.kind, NodeKind::SuperIndex) {
            if !self.allow_super_property {
                self.fail(&callee_node, code::SYNTAX_NOT_ADMITTED);
                return;
            }
            self.emit(Opcode::LdaThis, &[]);
            self.emit(Opcode::Star, &[i64::from(receiver)]);
            let base = self.allocate();
            self.emit(Opcode::GetSuperBase, &[]);
            self.emit(Opcode::Star, &[i64::from(base)]);
            self.expression(callee_node.first);
            self.emit(Opcode::ToPropertyKey, &[]);
            self.emit(Opcode::LdaSuperKeyed, &[i64::from(base)]);
            if node.has(flag::OPTIONAL) {
                self.chain_link();
            }
            self.emit(Opcode::Star, &[i64::from(callee)]);
        } else if matches!(callee_node.kind, NodeKind::Member | NodeKind::Index) {
            // A method call passes the object it was found on as the receiver.
            self.expression(callee_node.first);
            if callee_node.has(flag::OPTIONAL) {
                self.chain_link();
            }
            self.emit(Opcode::Star, &[i64::from(receiver)]);
            if matches!(callee_node.kind, NodeKind::Member) {
                self.private_member_guard(callee_node.second);
                let key = self.key_constant(callee_node.second);
                self.emit(
                    Opcode::GetNamedProperty,
                    &[i64::from(receiver), i64::from(key)],
                );
            } else {
                self.expression(callee_node.second);
                self.emit(Opcode::GetKeyedProperty, &[i64::from(receiver)]);
            }
            if node.has(flag::OPTIONAL) {
                self.chain_link();
            }
            self.emit(Opcode::Star, &[i64::from(callee)]);
        } else {
            let under_with =
                matches!(callee_node.kind, NodeKind::Identifier) && self.with_depth > 0;
            if under_with
                && matches!(
                    self.resolve(callee_node.first, callee_node.second),
                    Resolved::Global
                )
            {
                // A free name under `with` resolves once, for the callee and
                // for the receiver: the `with` object that bound it, if one
                // did, is the reference's base.
                let key = self.key_constant(node.first);
                self.emit(
                    Opcode::LdaDynamicCallee,
                    &[i64::from(key), i64::from(receiver)],
                );
                if node.has(flag::OPTIONAL) {
                    self.chain_link();
                }
                self.emit(Opcode::Star, &[i64::from(callee)]);
            } else {
                self.expression(node.first);
                if node.has(flag::OPTIONAL) {
                    self.chain_link();
                }
                self.emit(Opcode::Star, &[i64::from(callee)]);
                if under_with {
                    // A name a `with` object supplied calls with that object
                    // as its receiver: the reference's base.
                    let key = self.key_constant(node.first);
                    self.emit(Opcode::LdaWithReceiver, &[i64::from(key)]);
                } else {
                    self.emit(Opcode::LdaUndefined, &[]);
                }
                self.emit(Opcode::Star, &[i64::from(receiver)]);
            }
        }

        // A spread argument means the count is not known here, so the
        // arguments are gathered into an array and the call takes that.
        let spread = arguments
            .iter()
            .any(|&argument| matches!(self.node(argument).kind, NodeKind::Spread));
        if spread {
            let list = self.allocate();
            let elements = Node::new(NodeKind::Array, node.start, node.end).with_payload(
                node.second,
                node.third,
                0,
            );
            self.array(&elements);
            self.emit(Opcode::Star, &[i64::from(list)]);
            // A spread list does not stop the call being a direct eval.
            if !node.has(flag::OPTIONAL) && self.is_direct_eval_callee(&callee_node) {
                self.record_eval_site();
            }
            self.builder.safe_point();
            self.emit(
                Opcode::CallWithArray,
                &[i64::from(callee), i64::from(receiver), i64::from(list)],
            );
            if let Some(saved) = chain {
                self.leave_chain(saved);
            }
            self.release(mark);
            return;
        }

        let mut count = 1u32;
        for &argument in arguments {
            let child = self.node(argument);
            let register = self.allocate();
            if register != receiver + count {
                self.fail(&child, code::TOO_MANY_REGISTERS);
                return;
            }
            self.expression(argument);
            self.emit(Opcode::Star, &[i64::from(register)]);
            count += 1;
        }

        // A call written as the bare name `eval`, where nothing shadows it,
        // is a direct eval: the site records what was visible here, so a host
        // can compile the source against this exact scope.
        // An optional call `eval?.()` is never direct.
        let direct_eval = !node.has(flag::OPTIONAL) && self.is_direct_eval_callee(&callee_node);
        if direct_eval {
            self.record_eval_site();
        }
        self.builder.safe_point();
        // A call written `eval(...)` is a tail call too, when what `eval`
        // names turns out not to be the real one: the machine decides.
        let opcode = if chain.is_none() && self.in_tail_position(node) {
            Opcode::TailCall
        } else {
            Opcode::Call
        };
        self.emit(
            opcode,
            &[i64::from(callee), i64::from(receiver), i64::from(count)],
        );
        if let Some(saved) = chain {
            self.leave_chain(saved);
        }
        self.release(mark);
    }

    /// Whether a call node is one the `return` being lowered put in tail
    /// position.
    pub(super) fn in_tail_position(&self, node: &Node) -> bool {
        self.tail_spans
            .get(..self.tail_span_count)
            .unwrap_or(&[])
            .contains(&(node.start, node.end))
    }

    /// Mark the calls in tail position of a returned expression: the
    /// expression itself, or — through a conditional, a comma, or a logical
    /// operator — the operands that can be its value.
    pub(super) fn mark_tail_positions(&mut self, index: u32) {
        if index == NONE {
            return;
        }
        let node = self.node(index);
        match node.kind {
            NodeKind::Call | NodeKind::TaggedTemplate => {
                if self.tail_span_count < MAX_TAIL_SPANS {
                    self.tail_spans[self.tail_span_count] = (node.start, node.end);
                    self.tail_span_count += 1;
                }
            }
            NodeKind::Conditional => {
                let branches = self.arena.list(node.second, 2);
                if let [consequent, alternate] = branches {
                    let (consequent, alternate) = (*consequent, *alternate);
                    self.mark_tail_positions(consequent);
                    self.mark_tail_positions(alternate);
                }
            }
            NodeKind::Logical => self.mark_tail_positions(node.second),
            NodeKind::Sequence => {
                if let Some(&last) = self.arena.list(node.first, node.second).last() {
                    self.mark_tail_positions(last);
                }
            }
            _ => {}
        }
    }

    /// Whether a callee is the bare name `eval` with nothing shadowing it.
    pub(super) fn is_direct_eval_callee(&mut self, callee: &Node) -> bool {
        matches!(callee.kind, NodeKind::Identifier)
            && self.span(callee.first, callee.second) == b"eval"
            && matches!(self.resolve(callee.first, callee.second), Resolved::Global)
    }

    /// Record the bindings visible here, keyed by the function and the
    /// position of the `Call` about to be emitted.
    ///
    /// The record is advisory: when it does not fit — too many bindings, or
    /// a full table — the site is left without one, and an eval with no
    /// record to compile against runs as global code.
    pub(super) fn record_eval_site(&mut self) {
        let pc = self.builder.length();
        let start = if self.program.eval_site_length == 0 {
            // The blob opens with its site count.
            if self.program.eval_sites.len() < 4 {
                return;
            }
            self.program.eval_sites[0..4].copy_from_slice(&0u32.to_le_bytes());
            4
        } else {
            self.program.eval_site_length
        };
        let mut at = start;
        let mut flags = if self.strict { FLAG_STRICT } else { 0 };
        // Only a parameter initialiser of a non-arrow function refuses an
        // eval-declared `arguments`: there the declaration would collide
        // with the binding the call is still constructing. A body eval maps
        // onto the finished binding, and an arrow has none to collide with.
        if self.in_function && !self.in_arrow && self.in_parameters {
            flags |= FLAG_FUNCTION;
        }
        if self.in_function && self.in_parameters {
            flags |= FLAG_PARAMETERS;
        }
        if self.allow_super_property {
            flags |= FLAG_SUPER_PROPERTY;
        }
        if self.allow_super_call {
            flags |= FLAG_SUPER_CALL;
        }
        if self.allow_new_target {
            flags |= FLAG_NEW_TARGET;
        }
        if self.deny_arguments {
            flags |= FLAG_NO_ARGUMENTS;
        }
        if self.privates_visible {
            flags |= FLAG_PRIVATES;
        }
        let header_at = at;
        at += 20;
        if at > self.program.eval_sites.len() {
            return;
        }
        let mut written = 0u32;
        // The context depth of the variable environment the site's sloppy
        // eval code declares into: the function root's context, or through
        // an enclosing eval, its site's — the global object when neither.
        let mut var_depth = u32::MAX;

        // Walk the scope chain exactly as `resolve` does, first match by
        // name winning, then append what this compilation's own eval scope
        // carried, so an eval inside an eval still sees the whole chain.
        let mut scope = self.scope;
        let mut depth = 0u32;
        while scope != NONE {
            let record = self.program.scope(scope);
            if scope == self.function_scope && self.in_function {
                var_depth = depth;
            }
            let first = record.first as usize;
            let mut index = 0u32;
            while index < record.count {
                let Some(binding) = self.program.bindings.get(first + index as usize) else {
                    break;
                };
                let (name_start, name_end, slot, kind) =
                    (binding.start, binding.end, binding.slot, binding.kind);
                index += 1;
                if record.parent == NONE
                    && !self.program.eval_goal
                    && !self.program.module
                    && matches!(kind, binding_kind::LET | binding_kind::CONST)
                {
                    // A script's top-level lexical is a global lexical: the
                    // eval reaches it by name, not through a slot.
                    continue;
                }
                let name: &[u8] = self
                    .source
                    .get(name_start as usize..name_end as usize)
                    .unwrap_or(&[]);
                if name.is_empty() || site_holds(self.program.eval_sites, header_at + 20, at, name)
                {
                    continue;
                }
                if written >= MAX_EVAL_BINDINGS
                    || !push_site_binding(
                        self.program.eval_sites,
                        &mut at,
                        slot,
                        depth,
                        u32::from(kind),
                        name,
                    )
                {
                    flags |= FLAG_TRUNCATED;
                    break;
                }
                written += 1;
            }
            if record.context {
                depth += 1;
            }
            scope = record.parent;
        }
        if flags & FLAG_TRUNCATED == 0 {
            for binding in self.program.eval_scope {
                if site_holds(self.program.eval_sites, header_at + 20, at, binding.name) {
                    continue;
                }
                if written >= MAX_EVAL_BINDINGS
                    || !push_site_binding(
                        self.program.eval_sites,
                        &mut at,
                        binding.slot,
                        depth + binding.depth,
                        binding.kind,
                        binding.name,
                    )
                {
                    flags |= FLAG_TRUNCATED;
                    break;
                }
                written += 1;
            }
        }
        if flags & FLAG_TRUNCATED != 0 {
            // Half a scope is worse than none: the site keeps no record.
            return;
        }
        if !self.in_function
            && self.program.eval_goal
            && self.program.eval_var_env_depth != u32::MAX
        {
            var_depth = depth.saturating_add(self.program.eval_var_env_depth);
        }

        let header = self.program.eval_sites.get_mut(header_at..header_at + 20);
        let Some(header) = header else {
            return;
        };
        header[0..4].copy_from_slice(&self.function_index.to_le_bytes());
        header[4..8].copy_from_slice(&pc.to_le_bytes());
        header[8..12].copy_from_slice(&flags.to_le_bytes());
        header[12..16].copy_from_slice(&written.to_le_bytes());
        header[16..20].copy_from_slice(&var_depth.to_le_bytes());
        self.program.eval_site_count += 1;
        let count = self.program.eval_site_count;
        self.program.eval_sites[0..4].copy_from_slice(&count.to_le_bytes());
        self.program.eval_site_length = at;
    }

    pub(super) fn construct(&mut self, node: &Node) {
        let arguments = self.arena.list(node.second, node.third);
        let mark = self.registers;
        let callee = self.allocate();
        self.expression(node.first);
        self.emit(Opcode::Star, &[i64::from(callee)]);

        // A spread makes the count a run-time fact: the arguments gather
        // into an array and the construction takes that.
        let spread = arguments
            .iter()
            .any(|&argument| matches!(self.node(argument).kind, NodeKind::Spread));
        if spread {
            let list = self.allocate();
            let elements = Node::new(NodeKind::Array, node.start, node.end).with_payload(
                node.second,
                node.third,
                0,
            );
            self.array(&elements);
            self.emit(Opcode::Star, &[i64::from(list)]);
            self.builder.safe_point();
            self.emit(
                Opcode::ConstructWithArray,
                &[i64::from(callee), i64::from(list)],
            );
            self.release(mark);
            return;
        }

        let first = self.registers;
        let mut count = 0u32;
        for &argument in arguments {
            let child = self.node(argument);
            if matches!(child.kind, NodeKind::Spread) {
                self.fail(&child, code::LOWERING_NOT_ADMITTED);
                return;
            }
            let register = self.allocate();
            if register != first + count {
                self.fail(&child, code::TOO_MANY_REGISTERS);
                return;
            }
            self.expression(argument);
            self.emit(Opcode::Star, &[i64::from(register)]);
            count += 1;
        }
        if count == 0 {
            // The window must still name a register inside the frame.
            let register = self.allocate();
            self.emit(Opcode::LdaUndefined, &[]);
            self.emit(Opcode::Star, &[i64::from(register)]);
        }

        self.builder.safe_point();
        self.emit(
            Opcode::Construct,
            &[i64::from(callee), i64::from(first), i64::from(count)],
        );
        self.release(mark);
    }
}
