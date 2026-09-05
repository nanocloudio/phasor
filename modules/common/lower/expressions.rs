//! Expressions: literals, members, operators, assignment, and the naming of anonymous functions.

use super::*;

impl Lowering<'_, '_, '_, '_> {
    /// Lower `index`, leaving its value in the accumulator.
    pub(super) fn expression(&mut self, index: u32) {
        if self.program.failure.is_some() {
            return;
        }
        let node = self.node(index);
        match node.kind {
            NodeKind::Number => {
                if self.strict && node.has(flag::LEGACY_OCTAL) {
                    self.fail(&node, code::LEGACY_OCTAL_LITERAL);
                    return;
                }
                let value = self.arena.number(node.first);
                self.load_number(value);
            }
            NodeKind::String => {
                if self.strict && node.has(flag::LEGACY_OCTAL) {
                    self.fail(&node, code::LEGACY_OCTAL_ESCAPE);
                    return;
                }
                let constant = self.text_constant(&node, ConstantKind::String, TokenKind::String);
                self.emit(Opcode::LdaConstant, &[i64::from(constant)]);
            }
            NodeKind::BigInt => {
                let constant = self.bigint_constant(&node);
                self.emit(Opcode::LdaConstant, &[i64::from(constant)]);
            }
            NodeKind::Null => self.emit(Opcode::LdaNull, &[]),
            NodeKind::True => self.emit(Opcode::LdaTrue, &[]),
            NodeKind::False => self.emit(Opcode::LdaFalse, &[]),
            NodeKind::This => self.emit(Opcode::LdaThis, &[]),
            NodeKind::Identifier => self.load_name(&node),
            NodeKind::Function => {
                let function = self.queue_function(index);
                self.emit(Opcode::CreateClosure, &[i64::from(function)]);
                // A named function expression carries its own name.
                if node.first != NONE && !node.has(flag::ARROW) {
                    let name = self.node(node.first);
                    let constant = self.identifier_constant(&name);
                    self.emit(Opcode::NameClosure, &[i64::from(constant)]);
                }
            }
            NodeKind::Class => self.lower_class(&node, index),
            NodeKind::Decorated => {
                // The decorators evaluate first; the class is the value.
                let mark = self.registers;
                for &decorator in self.arena.list(node.first, node.second) {
                    self.expression(decorator);
                }
                self.release(mark);
                self.expression(node.third);
            }
            NodeKind::SuperMember => {
                // `super.name` belongs to method-like code — and to eval
                // code whose call site allowed it. Anywhere else it is the
                // early SyntaxError.
                if !self.allow_super_property {
                    self.fail(&node, code::SYNTAX_NOT_ADMITTED);
                    return;
                }
                let constant = self.identifier_constant(&node);
                self.emit(Opcode::LdaSuperProperty, &[i64::from(constant)]);
            }
            NodeKind::SuperIndex => {
                if !self.allow_super_property {
                    self.fail(&node, code::SYNTAX_NOT_ADMITTED);
                    return;
                }
                // The super base — and the bound `this` it needs — comes
                // first; only then does the key expression run.
                let mark = self.registers;
                let base = self.allocate();
                self.emit(Opcode::GetSuperBase, &[]);
                self.emit(Opcode::Star, &[i64::from(base)]);
                self.expression(node.first);
                self.emit(Opcode::ToPropertyKey, &[]);
                self.emit(Opcode::LdaSuperKeyed, &[i64::from(base)]);
                self.release(mark);
            }
            NodeKind::SuperCall => {
                if !self.allow_super_call {
                    self.fail(&node, code::SYNTAX_NOT_ADMITTED);
                    return;
                }
                let arguments = self.arena.list(node.first, node.second);
                let mark = self.registers;
                let spread = arguments
                    .iter()
                    .any(|&argument| matches!(self.node(argument).kind, NodeKind::Spread));
                if spread {
                    let list = self.allocate();
                    let elements = Node::new(NodeKind::Array, node.start, node.end).with_payload(
                        node.first,
                        node.second,
                        0,
                    );
                    self.array(&elements);
                    self.emit(Opcode::Star, &[i64::from(list)]);
                    self.builder.safe_point();
                    self.emit(Opcode::CallSuperWithArray, &[i64::from(list)]);
                    self.emit(Opcode::BindThis, &[]);
                    let brand = self.brand_instances;
                    self.emit_init_fields(brand);
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
                    self.expression(argument);
                    self.emit(Opcode::Star, &[i64::from(register)]);
                    count += 1;
                }
                self.builder.safe_point();
                self.emit(Opcode::CallSuper, &[i64::from(first), i64::from(count)]);
                self.emit(Opcode::BindThis, &[]);
                let brand = self.brand_instances;
                self.emit_init_fields(brand);
                self.release(mark);
            }
            NodeKind::NewTarget => {
                // `new.target` belongs to function code — and to eval code
                // whose site sat in some.
                if !self.allow_new_target {
                    self.fail(&node, code::SYNTAX_NOT_ADMITTED);
                    return;
                }
                self.emit(Opcode::LdaNewTarget, &[]);
            }
            NodeKind::ImportCall => {
                // The specifier lands in the accumulator and the options —
                // undefined without a second argument — in a register the
                // import inspects on the promise's behalf.
                let mark = self.registers;
                let specifier = self.allocate();
                let options = self.allocate();
                self.emit(Opcode::LdaUndefined, &[]);
                self.emit(Opcode::Star, &[i64::from(options)]);
                for (position, &argument) in
                    self.arena.list(node.first, node.second).iter().enumerate()
                {
                    let child = self.node(argument);
                    if matches!(child.kind, NodeKind::Spread) {
                        self.fail(&child, code::LOWERING_NOT_ADMITTED);
                        return;
                    }
                    self.expression(argument);
                    if position == 0 {
                        self.emit(Opcode::Star, &[i64::from(specifier)]);
                    } else if position == 1 {
                        self.emit(Opcode::Star, &[i64::from(options)]);
                    }
                }
                self.emit(Opcode::Ldar, &[i64::from(specifier)]);
                if node.third == 2 {
                    // A source-phase import has no loader to answer it.
                    self.emit(Opcode::ImportReject, &[]);
                } else {
                    self.emit(
                        Opcode::DynamicImport,
                        &[i64::from(node.third), i64::from(options)],
                    );
                }
                self.release(mark);
            }
            NodeKind::RegExp => {
                let constant = self.regexp_constant(&node);
                self.emit(Opcode::CreateRegExp, &[i64::from(constant)]);
            }
            NodeKind::Template => self.template(&node),
            NodeKind::TaggedTemplate => self.tagged_template(&node),
            NodeKind::Array => self.array(&node),
            NodeKind::Object => self.object(&node),
            NodeKind::Member | NodeKind::Index => self.load_member(index, &node),
            NodeKind::Call => self.call(&node),
            NodeKind::New => self.construct(&node),
            NodeKind::Unary => self.unary(&node),
            NodeKind::Update => self.update(index, &node),
            NodeKind::Binary => self.binary(&node),
            NodeKind::Logical => self.logical(&node),
            NodeKind::Conditional => self.conditional(&node),
            NodeKind::Assign => self.assign(&node),
            NodeKind::Sequence => {
                for &child in self.arena.list(node.first, node.second) {
                    self.expression(child);
                }
            }
            _ => self.fail(&node, code::LOWERING_NOT_ADMITTED),
        }
    }

    /// A tagged template: the tag called with the strings array — carrying
    /// its `raw` counterpart — and the substitution values.
    pub(super) fn tagged_template(&mut self, node: &Node) {
        let mark = self.registers;
        // The strings array and its raw twin, built before the call frame's
        // registers are laid out.
        let strings = self.allocate();
        let raw = self.allocate();
        self.emit(Opcode::CreateEmptyArray, &[]);
        self.emit(Opcode::Star, &[i64::from(strings)]);
        self.emit(Opcode::CreateEmptyArray, &[]);
        self.emit(Opcode::Star, &[i64::from(raw)]);
        let template = self.node(node.second);
        let parts = self.arena.list(template.first, template.second);
        for &part in parts {
            let element = self.node(part);
            if !matches!(element.kind, NodeKind::TemplateElement) {
                continue;
            }
            if element.has(flag::COOKED_INVALID) {
                self.emit(Opcode::LdaUndefined, &[]);
            } else {
                let constant = self.text_constant(
                    &element,
                    ConstantKind::String,
                    TokenKind::NoSubstitutionTemplate,
                );
                self.emit(Opcode::LdaConstant, &[i64::from(constant)]);
            }
            self.emit(Opcode::AppendArrayElement, &[i64::from(strings)]);
            // The raw text is the source spelling, escapes and all.
            let span_start = element.first as usize;
            let span_end = element.second as usize;
            let text: &[u8] = self.source.get(span_start..span_end).unwrap_or(&[]);
            // The raw text is UTF-16 over the source's UTF-8, with line
            // terminators normalised: a carriage return, alone or before a
            // line feed, reads as a line feed.
            let mut raw_units = [0u16; 512];
            let mut taken = 0usize;
            let mut at = 0usize;
            while at < text.len() && taken < raw_units.len() {
                let byte = text[at];
                if byte == b'\r' {
                    raw_units[taken] = 0x0A;
                    taken += 1;
                    at += 1;
                    if text.get(at) == Some(&b'\n') {
                        at += 1;
                    }
                    continue;
                }
                if byte < 0x80 {
                    raw_units[taken] = u16::from(byte);
                    taken += 1;
                    at += 1;
                    continue;
                }
                let width = if byte >= 0xF0 {
                    4
                } else if byte >= 0xE0 {
                    3
                } else {
                    2
                };
                let mut point = u32::from(byte & (0x7F >> width));
                let mut offset = 1usize;
                while offset < width {
                    point = (point << 6)
                        | u32::from(text.get(at + offset).copied().unwrap_or(0) & 0x3F);
                    offset += 1;
                }
                at += width;
                if point > 0xFFFF {
                    let bias = point - 0x10000;
                    raw_units[taken] = 0xD800 + (bias >> 10) as u16;
                    taken += 1;
                    if taken < raw_units.len() {
                        raw_units[taken] = 0xDC00 + (bias & 0x3FF) as u16;
                        taken += 1;
                    }
                } else {
                    raw_units[taken] = point as u16;
                    taken += 1;
                }
            }
            let constant = self.unit_text_constant(&raw_units[..taken]);
            self.emit(Opcode::LdaConstant, &[i64::from(constant)]);
            self.emit(Opcode::AppendArrayElement, &[i64::from(raw)]);
        }
        self.emit(Opcode::Ldar, &[i64::from(raw)]);
        let raw_key = self.text_key_constant(b"raw");
        self.emit(
            Opcode::DefineNamedProperty,
            &[i64::from(strings), i64::from(raw_key)],
        );
        // The site's first template object is the site's forever: every
        // later evaluation answers the same array.
        let site = self.program.template_sites;
        self.program.template_sites += 1;
        self.emit(Opcode::Ldar, &[i64::from(strings)]);
        self.emit(Opcode::CacheTemplate, &[i64::from(site)]);
        self.emit(Opcode::Star, &[i64::from(strings)]);

        let callee = self.allocate();
        let receiver = self.allocate();
        let tag = self.node(node.first);
        if matches!(tag.kind, NodeKind::Member | NodeKind::Index) {
            self.expression(tag.first);
            self.emit(Opcode::Star, &[i64::from(receiver)]);
            if matches!(tag.kind, NodeKind::Member) {
                let key = self.key_constant(tag.second);
                self.emit(
                    Opcode::GetNamedProperty,
                    &[i64::from(receiver), i64::from(key)],
                );
            } else {
                self.expression(tag.second);
                self.emit(Opcode::GetKeyedProperty, &[i64::from(receiver)]);
            }
            self.emit(Opcode::Star, &[i64::from(callee)]);
        } else {
            self.expression(node.first);
            self.emit(Opcode::Star, &[i64::from(callee)]);
            self.emit(Opcode::LdaUndefined, &[]);
            self.emit(Opcode::Star, &[i64::from(receiver)]);
        }
        let first_argument = self.allocate();
        self.emit(Opcode::Ldar, &[i64::from(strings)]);
        self.emit(Opcode::Star, &[i64::from(first_argument)]);
        let mut count = 2u32;
        for &part in parts {
            let element = self.node(part);
            if matches!(element.kind, NodeKind::TemplateElement) {
                continue;
            }
            let register = self.allocate();
            if register != receiver + count {
                self.fail(&element, code::TOO_MANY_REGISTERS);
                return;
            }
            self.expression(part);
            self.emit(Opcode::Star, &[i64::from(register)]);
            count += 1;
        }
        self.builder.safe_point();
        let opcode = if self.in_tail_position(node) {
            Opcode::TailCall
        } else {
            Opcode::Call
        };
        self.emit(
            opcode,
            &[i64::from(callee), i64::from(receiver), i64::from(count)],
        );
        self.release(mark);
    }

    pub(super) fn template(&mut self, node: &Node) {
        let parts = self.arena.list(node.first, node.second);
        let mark = self.registers;
        let accumulator = self.allocate();
        let mut index = 0usize;
        while index < parts.len() {
            let part = self.node(parts[index]);
            if index == 0 {
                let constant = self.text_constant(
                    &part,
                    ConstantKind::String,
                    TokenKind::NoSubstitutionTemplate,
                );
                self.emit(Opcode::LdaConstant, &[i64::from(constant)]);
                self.emit(Opcode::Star, &[i64::from(accumulator)]);
                index += 1;
                continue;
            }
            if matches!(part.kind, NodeKind::TemplateElement) {
                if part.first == part.second {
                    index += 1;
                    continue;
                }
                let constant = self.text_constant(
                    &part,
                    ConstantKind::String,
                    TokenKind::NoSubstitutionTemplate,
                );
                self.emit(Opcode::LdaConstant, &[i64::from(constant)]);
            } else {
                self.expression(parts[index]);
                self.emit(Opcode::ToString, &[]);
            }
            self.emit(Opcode::Add, &[i64::from(accumulator)]);
            self.emit(Opcode::Star, &[i64::from(accumulator)]);
            index += 1;
        }
        self.emit(Opcode::Ldar, &[i64::from(accumulator)]);
        self.release(mark);
    }

    pub(super) fn array(&mut self, node: &Node) {
        let elements = self.arena.list(node.first, node.second);
        let mark = self.registers;
        let array = self.allocate();
        self.emit(Opcode::CreateEmptyArray, &[]);
        self.emit(Opcode::Star, &[i64::from(array)]);
        for &element in elements {
            let child = self.node(element);
            match child.kind {
                NodeKind::Elision => self.emit(Opcode::AppendArrayHole, &[i64::from(array)]),
                NodeKind::Spread => {
                    // Everything the operand iterates is appended in turn.
                    self.expression(child.first);
                    self.emit(Opcode::GetIterator, &[]);
                    let inner = self.registers;
                    let iterator = self.allocate();
                    let done = self.allocate();
                    self.emit(Opcode::Star, &[i64::from(iterator)]);
                    let top = self.builder.label();
                    let end = self.builder.label();
                    self.builder.safe_point();
                    self.builder.bind(top);
                    self.emit(
                        Opcode::IteratorNext,
                        &[i64::from(iterator), i64::from(done)],
                    );
                    let value = self.allocate();
                    self.emit(Opcode::Star, &[i64::from(value)]);
                    self.emit(Opcode::Ldar, &[i64::from(done)]);
                    self.builder.jump(Opcode::JumpIfTrue, end);
                    self.emit(Opcode::Ldar, &[i64::from(value)]);
                    self.emit(Opcode::AppendArrayElement, &[i64::from(array)]);
                    self.builder.jump(Opcode::Jump, top);
                    self.builder.bind(end);
                    self.release(inner);
                }
                _ => {
                    self.expression(element);
                    self.emit(Opcode::AppendArrayElement, &[i64::from(array)]);
                }
            }
        }
        self.emit(Opcode::Ldar, &[i64::from(array)]);
        self.release(mark);
    }

    pub(super) fn object(&mut self, node: &Node) {
        let properties = self.arena.list(node.first, node.second);
        let mark = self.registers;
        let object = self.allocate();
        self.emit(Opcode::CreateEmptyObject, &[]);
        self.emit(Opcode::Star, &[i64::from(object)]);
        for &property in properties {
            let child = self.node(property);
            match child.kind {
                NodeKind::Spread => {
                    self.expression(child.first);
                    self.emit(Opcode::CopyDataProperties, &[i64::from(object)]);
                }
                NodeKind::ShorthandProperty => {
                    if child.second != NONE {
                        // `{ a = 1 }` covers only a pattern; as a literal it
                        // is the syntax error the cover grammar deferred.
                        self.fail(&child, code::INVALID_ASSIGNMENT_TARGET);
                        return;
                    }
                    let name = child.first;
                    self.expression(name);
                    let key = self.key_constant(name);
                    self.emit(
                        Opcode::DefineNamedProperty,
                        &[i64::from(object), i64::from(key)],
                    );
                }
                NodeKind::Property if child.third == property_kind::METHOD => {
                    // A shorthand method: its closure takes the literal as
                    // home, which is what its `super.name` resolves through.
                    let key = self.node(child.first);
                    let function = self.queue_method(child.second, self.privates_visible);
                    if matches!(key.kind, NodeKind::ComputedKey) {
                        let inner = self.registers;
                        let key_register = self.allocate();
                        self.expression(key.first);
                        self.emit(Opcode::ToPropertyKey, &[]);
                        self.emit(Opcode::Star, &[i64::from(key_register)]);
                        self.emit(Opcode::CreateClosure, &[i64::from(function)]);
                        self.emit(Opcode::SetHome, &[i64::from(object)]);
                        self.emit(Opcode::NameClosureKeyed, &[i64::from(key_register)]);
                        self.emit(
                            Opcode::DefineKeyedProperty,
                            &[i64::from(object), i64::from(key_register)],
                        );
                        self.release(inner);
                    } else {
                        let constant = self.key_constant(child.first);
                        self.emit(Opcode::CreateClosure, &[i64::from(function)]);
                        self.emit(Opcode::SetHome, &[i64::from(object)]);
                        self.emit(Opcode::NameClosure, &[i64::from(constant)]);
                        self.emit(
                            Opcode::DefineNamedProperty,
                            &[i64::from(object), i64::from(constant)],
                        );
                    }
                }
                NodeKind::Property if child.third != property_kind::DATA => {
                    // An accessor: the closure in the accumulator becomes the
                    // getter or the setter, merging with the accessor half
                    // already defined for the key.
                    let key = self.node(child.first);
                    let getter = child.third == property_kind::GETTER;
                    if matches!(key.kind, NodeKind::ComputedKey) {
                        let inner = self.registers;
                        let key_register = self.allocate();
                        self.expression(key.first);
                        self.emit(Opcode::ToPropertyKey, &[]);
                        self.emit(Opcode::Star, &[i64::from(key_register)]);
                        let function = self.queue_method(child.second, self.privates_visible);
                        self.emit(Opcode::CreateClosure, &[i64::from(function)]);
                        self.emit(Opcode::SetHome, &[i64::from(object)]);
                        let opcode = if getter {
                            Opcode::DefineKeyedGetter
                        } else {
                            Opcode::DefineKeyedSetter
                        };
                        self.emit(opcode, &[i64::from(object), i64::from(key_register)]);
                        self.release(inner);
                    } else {
                        let constant = self.key_constant(child.first);
                        let function = self.queue_method(child.second, self.privates_visible);
                        self.emit(Opcode::CreateClosure, &[i64::from(function)]);
                        self.emit(Opcode::SetHome, &[i64::from(object)]);
                        let opcode = if getter {
                            Opcode::DefineNamedGetter
                        } else {
                            Opcode::DefineNamedSetter
                        };
                        self.emit(opcode, &[i64::from(object), i64::from(constant)]);
                    }
                }
                NodeKind::Property => {
                    let key = self.node(child.first);
                    if matches!(key.kind, NodeKind::ComputedKey) {
                        let inner = self.registers;
                        let key_register = self.allocate();
                        self.expression(key.first);
                        self.emit(Opcode::ToPropertyKey, &[]);
                        self.emit(Opcode::Star, &[i64::from(key_register)]);
                        self.expression(child.second);
                        if self.is_anonymous_function(child.second) {
                            self.emit(Opcode::NameClosureKeyed, &[i64::from(key_register)]);
                        }
                        self.emit(
                            Opcode::DefineKeyedProperty,
                            &[i64::from(object), i64::from(key_register)],
                        );
                        self.release(inner);
                    } else if matches!(key.kind, NodeKind::PropertyName)
                        && key.third != property_key::NUMBER
                        && self.span(key.first, key.second) == b"__proto__"
                    {
                        // `__proto__:` in a literal sets the prototype, and
                        // only when the value is an object or null; a computed
                        // or shorthand `__proto__` is an ordinary property.
                        self.expression(child.second);
                        self.emit(Opcode::SetPrototype, &[i64::from(object)]);
                    } else {
                        let constant = self.key_constant(child.first);
                        self.expression(child.second);
                        // The name is the key, whatever the key was written
                        // as: `NameClosure` renders the constant exactly as a
                        // property key renders, so a numeric key names its
                        // function by the number's own text.
                        if self.is_anonymous_function(child.second) {
                            self.emit(Opcode::NameClosure, &[i64::from(constant)]);
                        }
                        self.emit(
                            Opcode::DefineNamedProperty,
                            &[i64::from(object), i64::from(constant)],
                        );
                    }
                }
                _ => self.fail(&child, code::LOWERING_NOT_ADMITTED),
            }
        }
        self.emit(Opcode::Ldar, &[i64::from(object)]);
        self.release(mark);
    }

    /// Load a member or index access, short-circuiting an optional link.
    /// Open a chain: the root of an optional chain owns the label every
    /// nullish link jumps to, so the whole tail is skipped at once.
    pub(super) fn enter_chain(&mut self, node: &Node) -> Option<Option<Label>> {
        if !node.has(flag::CHAIN_ROOT) {
            return None;
        }
        let saved = self.chain_exit;
        self.chain_exit = Some(self.builder.label());
        Some(saved)
    }

    /// Close a chain at its root: the normal path jumps over the landing,
    /// and the landing answers `undefined` for the whole chain.
    pub(super) fn leave_chain(&mut self, saved: Option<Label>) {
        let exit = self.chain_exit.take();
        self.chain_exit = saved;
        let Some(exit) = exit else {
            return;
        };
        let done = self.builder.label();
        self.builder.jump(Opcode::Jump, done);
        self.builder.bind(exit);
        self.emit(Opcode::LdaUndefined, &[]);
        self.builder.bind(done);
    }

    /// Jump to the enclosing chain's exit when the accumulator is nullish.
    pub(super) fn chain_link(&mut self) {
        if let Some(exit) = self.chain_exit {
            self.builder.jump(Opcode::JumpIfNullish, exit);
        }
    }

    pub(super) fn load_member(&mut self, index: u32, node: &Node) {
        let mark = self.registers;
        let chain = self.enter_chain(node);
        let object = self.allocate();
        self.expression(node.first);

        if node.has(flag::OPTIONAL) {
            self.chain_link();
        }
        self.emit(Opcode::Star, &[i64::from(object)]);

        if matches!(node.kind, NodeKind::Member) {
            self.private_member_guard(node.second);
            let key = self.key_constant(node.second);
            self.emit(
                Opcode::GetNamedProperty,
                &[i64::from(object), i64::from(key)],
            );
        } else {
            self.expression(node.second);
            self.emit(Opcode::GetKeyedProperty, &[i64::from(object)]);
        }

        if let Some(saved) = chain {
            self.leave_chain(saved);
        }
        let _ = index;
        self.release(mark);
    }

    pub(super) fn unary(&mut self, node: &Node) {
        if node.third == unop::DELETE {
            self.delete(node);
            return;
        }
        // `typeof` on a bare identifier admits an unresolvable name, which is
        // the one place a reference may be read without resolving. Only a free
        // name takes that path: one that resolves to a binding is read exactly
        // as any other use of it would be.
        if node.third == unop::TYPEOF {
            let operand = self.node(node.first);
            if matches!(operand.kind, NodeKind::Identifier)
                && matches!(
                    self.resolve(operand.first, operand.second),
                    Resolved::Global
                )
            {
                let constant = self.key_constant(node.first);
                let opcode = if self.dynamic_names || self.with_depth > 0 {
                    Opcode::TypeofDynamic
                } else {
                    Opcode::LdaGlobalOrUndefined
                };
                self.emit(opcode, &[i64::from(constant)]);
                self.emit(Opcode::TypeOf, &[]);
                return;
            }
        }
        if node.third == unop::YIELD {
            // A yield's resumption carries its kind: a `throw` is thrown at
            // the yield, and a `return` runs the finalisers this yield sits
            // inside — which the compiler knows — before returning.
            let mark = self.registers;
            let sent = self.allocate();
            let tmp = self.allocate();
            if node.first == NONE {
                self.emit(Opcode::LdaUndefined, &[]);
            } else {
                self.expression(node.first);
            }
            if self.in_async_generator {
                // The operand is awaited before the yield hands it out.
                self.builder.safe_point();
                self.emit(Opcode::Await, &[]);
            }
            let normal = self.builder.label();
            let do_return = self.builder.label();
            self.builder.safe_point();
            self.emit(Opcode::YieldStar, &[]);
            self.emit(Opcode::Star, &[i64::from(sent)]);
            self.emit(Opcode::ResumeKind, &[]);
            self.builder.jump(Opcode::JumpIfToBooleanFalse, normal);
            self.emit(Opcode::Star, &[i64::from(tmp)]);
            self.emit(Opcode::LdaSmi, &[2]);
            self.emit(Opcode::TestStrictEqual, &[i64::from(tmp)]);
            self.builder.jump(Opcode::JumpIfTrue, do_return);
            self.emit(Opcode::Ldar, &[i64::from(sent)]);
            self.emit(Opcode::Throw, &[]);
            self.builder.bind(do_return);
            self.emit(Opcode::Ldar, &[i64::from(sent)]);
            if self.in_async_generator {
                self.builder.safe_point();
                self.emit(Opcode::Await, &[]);
            }
            self.emit(Opcode::Star, &[i64::from(sent)]);
            self.unwind_to(0, 0);
            if !self.builder.terminated() {
                self.emit(Opcode::Ldar, &[i64::from(sent)]);
                self.emit(Opcode::Return, &[]);
            }
            self.builder.bind(normal);
            self.emit(Opcode::Ldar, &[i64::from(sent)]);
            self.release(mark);
            return;
        }
        if node.third == unop::YIELD_DELEGATE {
            // Delegate: every result the inner iterator produces is yielded
            // on, and how the generator is resumed — next, throw, or return —
            // is forwarded to the inner iterator's own method, which is what
            // makes the inner iterator the one that answers all three.
            let mark = self.registers;
            let iterator = self.allocate();
            let next_method = self.allocate();
            let sent = self.allocate();
            let result = self.allocate();
            let kind = self.allocate();
            let callee = self.allocate();
            let receiver = self.allocate();
            let argument = self.allocate();
            self.expression(node.first);
            if self.in_async_generator {
                self.emit(Opcode::GetAsyncIterator, &[]);
            } else {
                self.emit(Opcode::GetIterator, &[]);
            }
            self.emit(Opcode::Star, &[i64::from(iterator)]);
            // The iterator record fetches `next` once: every step calls the
            // method that fetch produced, whatever the object does later.
            let next_key = self.text_key_constant(b"next");
            self.emit(
                Opcode::GetNamedProperty,
                &[i64::from(iterator), i64::from(next_key)],
            );
            self.emit(Opcode::Star, &[i64::from(next_method)]);
            self.emit(Opcode::LdaUndefined, &[]);
            self.emit(Opcode::Star, &[i64::from(sent)]);
            let call_next = self.builder.label();
            let examine = self.builder.label();
            let yield_point = self.builder.label();
            let return_path = self.builder.label();
            let have_throw = self.builder.label();
            let have_return = self.builder.label();
            let return_done = self.builder.label();
            let end = self.builder.label();
            let done_key = self.text_key_constant(b"done");
            let value_key = self.text_key_constant(b"value");
            self.builder.safe_point();
            self.builder.bind(call_next);
            self.emit(Opcode::Ldar, &[i64::from(next_method)]);
            self.emit(Opcode::Star, &[i64::from(callee)]);
            self.emit(Opcode::Ldar, &[i64::from(iterator)]);
            self.emit(Opcode::Star, &[i64::from(receiver)]);
            self.emit(Opcode::Ldar, &[i64::from(sent)]);
            self.emit(Opcode::Star, &[i64::from(argument)]);
            self.builder.safe_point();
            self.emit(Opcode::Call, &[i64::from(callee), i64::from(receiver), 2]);
            if self.in_async_generator {
                self.builder.safe_point();
                self.emit(Opcode::Await, &[]);
            }
            self.emit(Opcode::RequireObject, &[]);
            self.emit(Opcode::Star, &[i64::from(result)]);
            self.builder.safe_point();
            self.builder.bind(examine);
            self.emit(
                Opcode::GetNamedProperty,
                &[i64::from(result), i64::from(done_key)],
            );
            self.builder.jump(Opcode::JumpIfToBooleanTrue, end);
            if self.in_async_generator {
                self.emit(
                    Opcode::GetNamedProperty,
                    &[i64::from(result), i64::from(value_key)],
                );
            } else {
                // The resumer receives the inner result object untouched:
                // its `value` is never read while the delegation runs.
                self.emit(Opcode::Ldar, &[i64::from(result)]);
            }
            self.builder.bind(yield_point);
            self.builder.safe_point();
            if self.in_async_generator {
                self.emit(Opcode::YieldStar, &[]);
            } else {
                self.emit(Opcode::YieldDelegate, &[]);
            }
            self.emit(Opcode::Star, &[i64::from(sent)]);
            self.emit(Opcode::ResumeKind, &[]);
            self.builder.jump(Opcode::JumpIfToBooleanFalse, call_next);
            self.emit(Opcode::Star, &[i64::from(kind)]);
            self.emit(Opcode::LdaSmi, &[2]);
            self.emit(Opcode::TestStrictEqual, &[i64::from(kind)]);
            self.builder.jump(Opcode::JumpIfTrue, return_path);
            // Thrown in: the inner iterator's `throw` answers, and an
            // iterator without one is closed before the TypeError.
            let throw_key = self.text_key_constant(b"throw");
            self.emit(
                Opcode::GetNamedProperty,
                &[i64::from(iterator), i64::from(throw_key)],
            );
            self.emit(Opcode::Star, &[i64::from(callee)]);
            self.builder.jump(Opcode::JumpIfNotNullish, have_throw);
            self.emit(Opcode::LdaFalse, &[]);
            self.emit(Opcode::Star, &[i64::from(kind)]);
            self.emit(
                Opcode::IteratorClose,
                &[i64::from(iterator), i64::from(kind)],
            );
            self.emit(Opcode::LdaUndefined, &[]);
            self.emit(Opcode::RequireObject, &[]);
            self.builder.bind(have_throw);
            self.emit(Opcode::Ldar, &[i64::from(iterator)]);
            self.emit(Opcode::Star, &[i64::from(receiver)]);
            self.emit(Opcode::Ldar, &[i64::from(sent)]);
            self.emit(Opcode::Star, &[i64::from(argument)]);
            self.builder.safe_point();
            self.emit(Opcode::Call, &[i64::from(callee), i64::from(receiver), 2]);
            if self.in_async_generator {
                self.builder.safe_point();
                self.emit(Opcode::Await, &[]);
            }
            self.emit(Opcode::RequireObject, &[]);
            self.emit(Opcode::Star, &[i64::from(result)]);
            self.builder.jump(Opcode::Jump, examine);
            // Returned into: the inner iterator's `return` answers, and an
            // iterator without one lets the generator return as asked.
            self.builder.bind(return_path);
            if self.in_async_generator {
                // The value returned into is awaited before the inner
                // iterator's `return` is looked up.
                self.emit(Opcode::Ldar, &[i64::from(sent)]);
                self.builder.safe_point();
                self.emit(Opcode::Await, &[]);
                self.emit(Opcode::Star, &[i64::from(sent)]);
            }
            let return_key = self.text_key_constant(b"return");
            self.emit(
                Opcode::GetNamedProperty,
                &[i64::from(iterator), i64::from(return_key)],
            );
            self.emit(Opcode::Star, &[i64::from(callee)]);
            self.builder.jump(Opcode::JumpIfNotNullish, have_return);
            self.emit(Opcode::Ldar, &[i64::from(sent)]);
            if self.in_async_generator {
                self.builder.safe_point();
                self.emit(Opcode::Await, &[]);
            }
            self.emit(Opcode::Star, &[i64::from(sent)]);
            self.unwind_to(0, 0);
            if !self.builder.terminated() {
                self.emit(Opcode::Ldar, &[i64::from(sent)]);
                self.emit(Opcode::Return, &[]);
            }
            self.builder.bind(have_return);
            self.emit(Opcode::Ldar, &[i64::from(iterator)]);
            self.emit(Opcode::Star, &[i64::from(receiver)]);
            self.emit(Opcode::Ldar, &[i64::from(sent)]);
            self.emit(Opcode::Star, &[i64::from(argument)]);
            self.builder.safe_point();
            self.emit(Opcode::Call, &[i64::from(callee), i64::from(receiver), 2]);
            if self.in_async_generator {
                self.builder.safe_point();
                self.emit(Opcode::Await, &[]);
            }
            self.emit(Opcode::RequireObject, &[]);
            self.emit(Opcode::Star, &[i64::from(result)]);
            self.emit(
                Opcode::GetNamedProperty,
                &[i64::from(result), i64::from(done_key)],
            );
            self.builder.jump(Opcode::JumpIfToBooleanTrue, return_done);
            if self.in_async_generator {
                self.emit(
                    Opcode::GetNamedProperty,
                    &[i64::from(result), i64::from(value_key)],
                );
            } else {
                self.emit(Opcode::Ldar, &[i64::from(result)]);
            }
            self.builder.jump(Opcode::Jump, yield_point);
            self.builder.bind(return_done);
            self.emit(
                Opcode::GetNamedProperty,
                &[i64::from(result), i64::from(value_key)],
            );
            self.emit(Opcode::Star, &[i64::from(sent)]);
            self.unwind_to(0, 0);
            if !self.builder.terminated() {
                self.emit(Opcode::Ldar, &[i64::from(sent)]);
                self.emit(Opcode::Return, &[]);
            }
            self.builder.bind(end);
            self.emit(
                Opcode::GetNamedProperty,
                &[i64::from(result), i64::from(value_key)],
            );
            self.release(mark);
            return;
        }
        self.expression(node.first);
        let opcode = match node.third {
            unop::VOID => {
                self.emit(Opcode::LdaUndefined, &[]);
                return;
            }
            unop::TYPEOF => Opcode::TypeOf,
            unop::AWAIT => Opcode::Await,
            unop::PLUS => Opcode::ToNumber,
            unop::MINUS => Opcode::Negate,
            unop::BITWISE_NOT => Opcode::BitNot,
            unop::LOGICAL_NOT => Opcode::LogicalNot,
            _ => {
                self.fail(node, code::LOWERING_NOT_ADMITTED);
                return;
            }
        };
        self.emit(opcode, &[]);
    }

    pub(super) fn delete(&mut self, node: &Node) {
        let target = self.node(node.first);
        match target.kind {
            NodeKind::SuperMember => {
                // Deleting a super reference: the base — and the `this` it
                // needs — is checked, and then the ReferenceError.
                self.emit(Opcode::GetSuperBase, &[]);
                self.emit(Opcode::ThrowReference, &[]);
            }
            NodeKind::SuperIndex => {
                // The key expression runs, but never becomes a key: the
                // delete refuses the super reference first.
                self.emit(Opcode::GetSuperBase, &[]);
                self.expression(target.first);
                self.emit(Opcode::ThrowReference, &[]);
            }
            NodeKind::Member => {
                let mark = self.registers;
                let object = self.allocate();
                self.expression(target.first);
                self.emit(Opcode::Star, &[i64::from(object)]);
                let key = self.key_constant(target.second);
                self.emit(Opcode::Ldar, &[i64::from(object)]);
                self.emit(Opcode::DeleteNamedProperty, &[i64::from(key)]);
                self.release(mark);
            }
            NodeKind::Index => {
                let mark = self.registers;
                let object = self.allocate();
                self.expression(target.first);
                self.emit(Opcode::Star, &[i64::from(object)]);
                self.expression(target.second);
                self.emit(Opcode::DeleteKeyedProperty, &[i64::from(object)]);
                self.release(mark);
            }
            NodeKind::Identifier => {
                match self.resolve(target.first, target.second) {
                    // A declared binding does not go away: `delete x` answers
                    // false without reading the binding, so a name still in
                    // its dead zone is not an error here.
                    Resolved::Slot { .. } => self.emit(Opcode::LdaFalse, &[]),
                    // A free name is a property of the global object, or of
                    // nothing: deleting answers whether it is gone, and a
                    // `var` global refuses because it is not configurable.
                    // A free name is a property of the global object, or of
                    // nothing: deleting answers whether it is gone, and a
                    // `var` global refuses because it is not configurable —
                    // as does a global lexical, which no property holds.
                    Resolved::Global => {
                        let key = self.identifier_constant(&target);
                        self.emit(Opcode::DeleteDynamic, &[i64::from(key)]);
                    }
                }
            }
            _ => {
                // Deleting anything else is `true` for a non-reference and a
                // strict-mode error for a binding, which static semantics will
                // decide once they exist.
                self.expression(node.first);
                self.emit(Opcode::LdaTrue, &[]);
            }
        }
    }

    pub(super) fn update(&mut self, _index: u32, node: &Node) {
        let target = self.node(node.first);
        let mark = self.registers;
        // The reference is evaluated once and reused for the read and the
        // write, exactly as a compound assignment does — the key coerced
        // exactly once, here.
        let reference = self.prepare_reference(&target);
        if matches!(target.kind, NodeKind::Index | NodeKind::SuperIndex) {
            self.emit(Opcode::Ldar, &[i64::from(reference.key)]);
            self.emit(Opcode::ToPropertyKeyChecked, &[i64::from(reference.object)]);
            self.emit(Opcode::Star, &[i64::from(reference.key)]);
        }
        let old = self.allocate();

        self.read_reference(&target, &reference);
        self.emit(Opcode::ToNumeric, &[]);
        self.emit(Opcode::Star, &[i64::from(old)]);
        let opcode = if node.third == unop::INCREMENT {
            Opcode::Inc
        } else {
            Opcode::Dec
        };
        self.emit(opcode, &[]);
        self.write_reference(&target, &reference);
        if !node.has(flag::PREFIX) {
            self.emit(Opcode::Ldar, &[i64::from(old)]);
        }
        self.release(mark);
    }

    /// Store the accumulator into an assignment target that has already had its
    /// object subexpression evaluated where one exists.
    pub(super) fn store(&mut self, target: &Node, index: u32) {
        match target.kind {
            NodeKind::Identifier => self.store_name(target),
            NodeKind::SuperMember | NodeKind::SuperIndex => {
                let mark = self.registers;
                let value = self.allocate();
                self.emit(Opcode::Star, &[i64::from(value)]);
                let reference = self.prepare_reference(target);
                self.emit(Opcode::Ldar, &[i64::from(value)]);
                self.write_reference(target, &reference);
                self.release(mark);
            }
            NodeKind::Member => {
                let mark = self.registers;
                let value = self.allocate();
                self.emit(Opcode::Star, &[i64::from(value)]);
                let object = self.allocate();
                self.expression(target.first);
                self.emit(Opcode::Star, &[i64::from(object)]);
                self.private_member_guard(target.second);
                let key = self.key_constant(target.second);
                self.emit(Opcode::Ldar, &[i64::from(value)]);
                self.emit(
                    Opcode::SetNamedProperty,
                    &[i64::from(object), i64::from(key)],
                );
                self.release(mark);
            }
            NodeKind::Index => {
                let mark = self.registers;
                let value = self.allocate();
                self.emit(Opcode::Star, &[i64::from(value)]);
                let object = self.allocate();
                self.expression(target.first);
                self.emit(Opcode::Star, &[i64::from(object)]);
                let key = self.allocate();
                self.expression(target.second);
                self.emit(Opcode::ToPropertyKey, &[]);
                self.emit(Opcode::Star, &[i64::from(key)]);
                self.emit(Opcode::Ldar, &[i64::from(value)]);
                self.emit(
                    Opcode::SetKeyedProperty,
                    &[i64::from(object), i64::from(key)],
                );
                self.release(mark);
            }
            _ => self.fail(target, code::LOWERING_NOT_ADMITTED),
        }
        let _ = index;
    }

    pub(super) fn binary(&mut self, node: &Node) {
        // `#x in o`: the left side is a private name, not a value — the
        // check reads the site's brand rather than the property table.
        if node.third == binop::IN {
            let left = self.node(node.first);
            if matches!(left.kind, NodeKind::PrivateName) {
                if self.program.eval_goal && !self.privates_visible {
                    self.fail(&left, code::SYNTAX_NOT_ADMITTED);
                    return;
                }
                let constant = self.identifier_constant(&left);
                self.expression(node.second);
                self.emit(Opcode::TestPrivateIn, &[i64::from(constant)]);
                return;
            }
        }
        // The left operand is evaluated before its register is taken, so the
        // registers its own subexpression used are free again by then. A long
        // chain of operators therefore needs one register, not one per link,
        // which also stops a finished intermediate from being kept alive by a
        // register nothing will read again.
        let mark = self.registers;
        self.expression(node.first);
        let left = self.allocate();
        self.emit(Opcode::Star, &[i64::from(left)]);
        self.expression(node.second);
        let opcode = match node.third {
            binop::ADD => Opcode::Add,
            binop::SUBTRACT => Opcode::Sub,
            binop::MULTIPLY => Opcode::Mul,
            binop::DIVIDE => Opcode::Div,
            binop::REMAINDER => Opcode::Mod,
            binop::EXPONENT => Opcode::Exp,
            binop::BITWISE_AND => Opcode::BitAnd,
            binop::BITWISE_OR => Opcode::BitOr,
            binop::BITWISE_XOR => Opcode::BitXor,
            binop::SHIFT_LEFT => Opcode::ShiftLeft,
            binop::SHIFT_RIGHT => Opcode::ShiftRight,
            binop::UNSIGNED_SHIFT_RIGHT => Opcode::ShiftRightLogical,
            binop::EQUAL => Opcode::TestEqual,
            binop::NOT_EQUAL => Opcode::TestNotEqual,
            binop::STRICT_EQUAL => Opcode::TestStrictEqual,
            binop::STRICT_NOT_EQUAL => Opcode::TestStrictNotEqual,
            binop::LESS => Opcode::TestLess,
            binop::GREATER => Opcode::TestGreater,
            binop::LESS_EQUAL => Opcode::TestLessEqual,
            binop::GREATER_EQUAL => Opcode::TestGreaterEqual,
            binop::INSTANCEOF => Opcode::TestInstanceOf,
            binop::IN => Opcode::TestIn,
            _ => {
                self.fail(node, code::LOWERING_NOT_ADMITTED);
                return;
            }
        };
        self.emit(opcode, &[i64::from(left)]);
        self.release(mark);
    }

    pub(super) fn logical(&mut self, node: &Node) {
        self.expression(node.first);
        let label = self.builder.label();
        let opcode = match node.third {
            binop::LOGICAL_AND => Opcode::JumpIfToBooleanFalse,
            binop::LOGICAL_OR => Opcode::JumpIfToBooleanTrue,
            binop::NULLISH => Opcode::JumpIfNotNullish,
            _ => {
                self.fail(node, code::LOWERING_NOT_ADMITTED);
                return;
            }
        };
        self.builder.jump(opcode, label);
        self.expression(node.second);
        self.builder.bind(label);
    }

    pub(super) fn conditional(&mut self, node: &Node) {
        let branches = self.arena.list(node.second, 2);
        if branches.len() != 2 {
            self.fail(node, code::LOWERING_NOT_ADMITTED);
            return;
        }
        let (consequent, alternate) = (branches[0], branches[1]);
        self.expression(node.first);
        let otherwise = self.builder.label();
        let done = self.builder.label();
        self.builder.jump(Opcode::JumpIfToBooleanFalse, otherwise);
        self.expression(consequent);
        self.builder.jump(Opcode::Jump, done);
        self.builder.bind(otherwise);
        self.expression(alternate);
        self.builder.bind(done);
    }

    pub(super) fn assign(&mut self, node: &Node) {
        let target = self.node(node.first);
        if node.third == binop::ASSIGN {
            // A pattern target takes the right-hand value apart, and the
            // assignment's own value is that right-hand value.
            if matches!(target.kind, NodeKind::Array | NodeKind::Object) {
                let mark = self.registers;
                self.expression(node.second);
                let value = self.allocate();
                self.emit(Opcode::Star, &[i64::from(value)]);
                self.assign_target(node.first);
                self.emit(Opcode::Ldar, &[i64::from(value)]);
                self.release(mark);
                return;
            }
            // The reference comes first: for a member target, the base and
            // the key are evaluated before the right-hand side, in the order
            // the specification evaluates an assignment.
            if matches!(
                target.kind,
                NodeKind::Member | NodeKind::Index | NodeKind::SuperMember | NodeKind::SuperIndex
            ) {
                let mark = self.registers;
                let reference = self.prepare_reference(&target);
                self.expression(node.second);
                self.write_reference(&target, &reference);
                self.release(mark);
                return;
            }
            if matches!(target.kind, NodeKind::Identifier)
                && (self.shadowable_slot(&target).is_some() || self.strict_global_target(&target))
            {
                // The reference forms before the right side runs, so an eval
                // in the right side cannot redirect this write — and strict
                // code throws for a name that resolved nowhere then.
                let mark = self.registers;
                let reference = self.prepare_reference(&target);
                self.named_assignment(node.second, &target);
                self.write_reference(&target, &reference);
                self.release(mark);
                return;
            }
            self.named_assignment(node.second, &target);
            self.store(&target, node.first);
            return;
        }

        // A compound or logical assignment evaluates its reference once: the
        // base and the key are computed here and reused for the read and the
        // write, so a side effect in either — the key's own coercion
        // included — runs exactly once.
        let mark = self.registers;
        let reference = self.prepare_reference(&target);
        if matches!(target.kind, NodeKind::Index | NodeKind::SuperIndex) {
            self.emit(Opcode::Ldar, &[i64::from(reference.key)]);
            self.emit(Opcode::ToPropertyKeyChecked, &[i64::from(reference.object)]);
            self.emit(Opcode::Star, &[i64::from(reference.key)]);
        }

        if matches!(
            node.third,
            binop::LOGICAL_AND | binop::LOGICAL_OR | binop::NULLISH
        ) {
            self.read_reference(&target, &reference);
            let label = self.builder.label();
            let opcode = match node.third {
                binop::LOGICAL_AND => Opcode::JumpIfToBooleanFalse,
                binop::LOGICAL_OR => Opcode::JumpIfToBooleanTrue,
                _ => Opcode::JumpIfNotNullish,
            };
            self.builder.jump(opcode, label);
            self.named_assignment(node.second, &target);
            self.write_reference(&target, &reference);
            self.builder.bind(label);
            self.release(mark);
            return;
        }

        // Compound assignment: read the target, apply the operator, store back.
        self.read_reference(&target, &reference);
        let left = self.allocate();
        self.emit(Opcode::Star, &[i64::from(left)]);
        self.expression(node.second);
        let operator = node.third;
        let combined = Node::new(NodeKind::Binary, node.start, node.end).with_payload(
            node.first,
            node.second,
            operator,
        );
        let opcode = match operator {
            binop::ADD => Opcode::Add,
            binop::SUBTRACT => Opcode::Sub,
            binop::MULTIPLY => Opcode::Mul,
            binop::DIVIDE => Opcode::Div,
            binop::REMAINDER => Opcode::Mod,
            binop::EXPONENT => Opcode::Exp,
            binop::BITWISE_AND => Opcode::BitAnd,
            binop::BITWISE_OR => Opcode::BitOr,
            binop::BITWISE_XOR => Opcode::BitXor,
            binop::SHIFT_LEFT => Opcode::ShiftLeft,
            binop::SHIFT_RIGHT => Opcode::ShiftRight,
            binop::UNSIGNED_SHIFT_RIGHT => Opcode::ShiftRightLogical,
            _ => {
                self.fail(&combined, code::LOWERING_NOT_ADMITTED);
                return;
            }
        };
        self.emit(opcode, &[i64::from(left)]);
        self.write_reference(&target, &reference);
        self.release(mark);
    }

    /// The right side of an assignment: named evaluation applies when the
    /// target is a bare name — a name in parentheses, `(f) = function () {}`,
    /// is a cover the specification leaves unnamed.
    pub(super) fn named_assignment(&mut self, value_node: u32, target: &Node) {
        if matches!(target.kind, NodeKind::Identifier) && !target.has(flag::PARENTHESISED) {
            self.named_expression(value_node, target);
        } else {
            self.expression(value_node);
        }
    }

    /// Name the closure the accumulator holds, when the expression that just
    /// produced it was an anonymous function: what the specification calls
    /// named evaluation.
    pub(super) fn name_closure(&mut self, value_node: u32, name_node: &Node) {
        if !self.is_anonymous_function(value_node) {
            return;
        }
        let constant = self.identifier_constant(name_node);
        self.emit(Opcode::NameClosure, &[i64::from(constant)]);
    }

    /// Lower an expression that named evaluation applies to: an anonymous
    /// class takes the name before its static initialisers run, which may
    /// read it, and a function takes it after.
    pub(super) fn named_expression(&mut self, value_node: u32, name_node: &Node) {
        let anonymous_class = matches!(self.node(value_node).kind, NodeKind::Class)
            && self.node(value_node).first == NONE;
        if anonymous_class {
            self.pending_class_name = Some(self.identifier_constant(name_node));
        }
        self.expression(value_node);
        self.pending_class_name = None;
        self.name_closure(value_node, name_node);
    }

    /// Whether an expression is a function written with no name of its own,
    /// which is what named evaluation applies to.
    pub(super) fn is_anonymous_function(&self, value_node: u32) -> bool {
        let value = self.node(value_node);
        matches!(value.kind, NodeKind::Function | NodeKind::Class) && value.first == NONE
    }
}
