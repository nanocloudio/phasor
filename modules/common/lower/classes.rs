//! Class bodies: the constructor, methods, accessors, fields, private members, and static blocks.

use super::*;

/// Computed keys one class body may carry.
const MAX_COMPUTED_KEYS: usize = 32;

impl Lowering<'_, '_, '_, '_> {
    /// A property key constant from an identifier-like name node.
    /// A private member access in eval code is admissible only where the
    /// call site could see a private scope; anywhere else it is the early
    /// SyntaxError, before anything runs.
    pub(super) fn private_member_guard(&mut self, index: u32) {
        if !self.program.eval_goal || self.privates_visible {
            return;
        }
        let node = self.node(index);
        if matches!(node.kind, NodeKind::PropertyName)
            && self.span(node.first, node.second).first() == Some(&b'#')
        {
            self.fail(&node, code::SYNTAX_NOT_ADMITTED);
        }
    }

    /// The register a class member's computed key was evaluated into: the
    /// member's rank among the computed-key members, in source order.
    pub(super) fn computed_key_register(&self, keys: &[u32], member: u32, entries: &[u32]) -> u32 {
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
    pub(super) fn lower_class(&mut self, node: &Node, index: u32) {
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
            self.lower_instance_fields(entries, &computed_keys, proto, ctor, privates);
        }

        // The class's own name takes the constructor before the static
        // initialisers run: they may name the class.
        if named {
            let name = self.node(node.first);
            self.emit(Opcode::Ldar, &[i64::from(ctor)]);
            self.initialise_name(&name);
        }
        for &member in entries.get(1..).unwrap_or(&[]) {
            self.lower_class_element(member, entries, &computed_keys, proto, ctor, privates);
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

    /// Instance fields: keys and initialiser closures gathered into an array
    /// the constructor's prologue walks per construction.
    pub(super) fn lower_instance_fields(
        &mut self,
        entries: &[u32],
        computed_keys: &[u32; MAX_COMPUTED_KEYS],
        proto: u32,
        ctor: u32,
        privates: bool,
    ) {
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
                let register = self.computed_key_register(computed_keys, member, entries);
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

    /// One element of a class body after the constructor and the instance
    /// fields: a method, an accessor, a static field or block, or a decorator.
    pub(super) fn lower_class_element(
        &mut self,
        member: u32,
        entries: &[u32],
        computed_keys: &[u32; MAX_COMPUTED_KEYS],
        proto: u32,
        ctor: u32,
        privates: bool,
    ) {
        let record = self.node(member);
        if record.third == class_member::CONSTRUCTOR || record.third == class_member::FIELD {
            return;
        }
        // A decorator evaluates where it stands; what it answers is not
        // applied, which keeps the member it saw.
        if record.third == class_member::DECORATOR {
            let inner = self.registers;
            self.expression(record.second);
            self.release(inner);
            return;
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
            return;
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
            return;
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
            return;
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
                            self.computed_key_register(computed_keys, member, entries);
                        self.emit(Opcode::NameClosureKeyed, &[i64::from(key_register)]);
                    } else {
                        let constant = self.key_constant(record.first);
                        self.emit(Opcode::NameClosure, &[i64::from(constant)]);
                    }
                }
            }
            let key = self.node(record.first);
            if matches!(key.kind, NodeKind::ComputedKey) {
                let key_register = self.computed_key_register(computed_keys, member, entries);
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
            return;
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
            Some(self.computed_key_register(computed_keys, member, entries))
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
}
