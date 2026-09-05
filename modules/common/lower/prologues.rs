//! Script prologues: global lexical and `var` names, their checks against earlier scripts, and hoisted initialisation.

use super::*;

impl Lowering<'_, '_, '_, '_> {
    /// A script's `var` names become properties of the global object, and its
    /// function declarations become properties holding closures.
    pub(super) fn script_prologue(&mut self, list: u32, length: u32) {
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

    /// Emit `opcode` for each top-level lexical name a script declares —
    /// `let`, `const`, and `class` — with an immediate saying whether the
    /// binding is a const where the opcode takes one.
    pub(super) fn global_lexical_names(&mut self, list: u32, length: u32, opcode: Opcode) {
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
    pub(super) fn global_lexical_target(
        &mut self,
        target: u32,
        opcode: Opcode,
        constant_binding: bool,
    ) {
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

    pub(super) fn emit_global_lexical(
        &mut self,
        opcode: Opcode,
        constant: u32,
        constant_binding: bool,
    ) {
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
    pub(super) fn global_var_names(&mut self, index: u32, opcode: Opcode) {
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
    pub(super) fn global_var_target(&mut self, target: u32, opcode: Opcode) {
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
    pub(super) fn declare_global_vars(&mut self, index: u32) {
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
    pub(super) fn initialise_hoisted(&mut self, scope: u32) {
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
}
