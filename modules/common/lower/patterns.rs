//! Destructuring: binding and assignment through array and object patterns.

use super::*;

impl Lowering<'_, '_, '_, '_> {
    pub(super) fn bind_array_pattern(&mut self, node: &Node, initialise: bool) {
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
    pub(super) fn push_iterator_finaliser(&mut self, iterator: u32, done: u32) -> bool {
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
    pub(super) fn close_on_abrupt(
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
    pub(super) fn copy_rest(&mut self, object: u32, rest: u32, keys: &[(u32, bool)]) {
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

    pub(super) fn bind_object_pattern(&mut self, node: &Node, initialise: bool) {
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
    pub(super) fn prepare_binding(
        &mut self,
        target: u32,
        initialise: bool,
    ) -> Option<(u32, u32, u32)> {
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
    pub(super) fn bind_prepared(
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
    pub(super) fn assign_target(&mut self, target: u32) {
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
    pub(super) fn element_parts(&mut self, target: u32) -> (u32, u32) {
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
    pub(super) fn prepare_element(&mut self, target: u32) -> Option<(Node, Reference)> {
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
    pub(super) fn assign_after_read(&mut self, target: u32, prepared: Option<(Node, Reference)>) {
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

    pub(super) fn assign_array_pattern(&mut self, node: &Node) {
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

    pub(super) fn assign_object_pattern(&mut self, node: &Node) {
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
}
