//! Declarations and hoisting: what a scope binds, and what an eval may declare.

use super::*;

/// Where eval code `var`-declares or function-declares `needle`, walking the
/// same statements whose `var`s the caller's variable environment receives.
pub(super) fn eval_declared_name(
    arena: &Arena<'_>,
    source: &[u8],
    list: u32,
    length: u32,
    needle: &[u8],
    lexical: bool,
) -> Option<u32> {
    for &statement in arena.list(list, length) {
        if let Some(at) = eval_declared_in_statement(arena, source, statement, needle, lexical) {
            return Some(at);
        }
    }
    None
}

pub(super) fn eval_declared_in_statement(
    arena: &Arena<'_>,
    source: &[u8],
    index: u32,
    needle: &[u8],
    lexical: bool,
) -> Option<u32> {
    if index == NONE {
        return None;
    }
    let node = arena.node(index).copied()?;
    match node.kind {
        NodeKind::Declaration if node.third == declaration::VAR || lexical => {
            for &declarator in arena.list(node.first, node.second) {
                let record = arena.node(declarator).copied()?;
                if let Some(at) = target_declares_name(arena, source, record.first, needle) {
                    return Some(at);
                }
            }
            None
        }
        NodeKind::Function if node.first != NONE => {
            let name = arena.node(node.first).copied()?;
            (source.get(name.first as usize..name.second as usize) == Some(needle))
                .then_some(name.start)
        }
        NodeKind::Class if lexical && node.first != NONE => {
            let name = arena.node(node.first).copied()?;
            (source.get(name.first as usize..name.second as usize) == Some(needle))
                .then_some(name.start)
        }
        NodeKind::Decorated => {
            eval_declared_in_statement(arena, source, node.third, needle, lexical)
        }
        NodeKind::Block => {
            eval_declared_name(arena, source, node.first, node.second, needle, lexical)
        }
        NodeKind::If => eval_declared_in_statement(arena, source, node.second, needle, lexical)
            .or_else(|| eval_declared_in_statement(arena, source, node.third, needle, lexical)),
        NodeKind::While => eval_declared_in_statement(arena, source, node.second, needle, lexical),
        NodeKind::DoWhile => eval_declared_in_statement(arena, source, node.first, needle, lexical),
        NodeKind::For => eval_declared_in_statement(arena, source, node.first, needle, lexical)
            .or_else(|| {
                arena
                    .list(node.second, node.third)
                    .get(2)
                    .and_then(|&body| {
                        eval_declared_in_statement(arena, source, body, needle, lexical)
                    })
            }),
        NodeKind::ForInOf => eval_declared_in_statement(arena, source, node.first, needle, lexical)
            .or_else(|| eval_declared_in_statement(arena, source, node.third, needle, lexical)),
        NodeKind::Labelled => {
            eval_declared_in_statement(arena, source, node.second, needle, lexical)
        }
        NodeKind::With => eval_declared_in_statement(arena, source, node.second, needle, lexical),
        NodeKind::Try => eval_declared_in_statement(arena, source, node.first, needle, lexical)
            .or_else(|| {
                let handler = arena.node(node.second).copied()?;
                if let Some(at) = lexical
                    .then(|| target_declares_name(arena, source, handler.first, needle))
                    .flatten()
                {
                    return Some(at);
                }
                eval_declared_in_statement(arena, source, handler.second, needle, lexical)
            })
            .or_else(|| eval_declared_in_statement(arena, source, node.third, needle, lexical)),
        NodeKind::Switch => {
            for &case in arena.list(node.second, node.third) {
                let record = arena.node(case).copied()?;
                if let Some(at) =
                    eval_declared_name(arena, source, record.second, record.third, needle, lexical)
                {
                    return Some(at);
                }
            }
            None
        }
        _ => None,
    }
}

/// Where a binding target binds a name equal to `needle`, if it does.
pub(super) fn target_declares_name(
    arena: &Arena<'_>,
    source: &[u8],
    target: u32,
    needle: &[u8],
) -> Option<u32> {
    if target == NONE {
        return None;
    }
    let node = arena.node(target).copied()?;
    match node.kind {
        NodeKind::ArrayPattern | NodeKind::ObjectPattern => {
            for &child in arena.list(node.first, node.second) {
                let record = arena.node(child).copied()?;
                let found = match record.kind {
                    NodeKind::Elision => None,
                    NodeKind::PatternProperty => {
                        let element = arena.node(record.second).copied()?;
                        target_declares_name(arena, source, element.first, needle)
                    }
                    _ => target_declares_name(arena, source, record.first, needle),
                };
                if found.is_some() {
                    return found;
                }
            }
            None
        }
        _ => (source.get(node.first as usize..node.second as usize) == Some(needle))
            .then_some(node.start),
    }
}

/// Declare what a module's top level introduces: what it imports, what it
/// declares, and what it exports.
///
/// An import is a binding like any other, except that reading it goes through
/// the module that exports it, and assigning to it is refused.
pub(super) fn declare_module(
    arena: &Arena<'_>,
    source: &[u8],
    list: u32,
    length: u32,
    scope: u32,
    program: &mut Program<'_>,
) -> Result<(), Diagnostic> {
    for &statement in arena.list(list, length) {
        let Some(node) = arena.node(statement).copied() else {
            continue;
        };
        match node.kind {
            NodeKind::Import => {
                let clauses = arena.list(node.first, node.second);
                if clauses.is_empty() {
                    // A bare `import './x'` binds nothing, but its edge to
                    // the module it names must still be in the table.
                    let slot = program
                        .imports
                        .get_mut(program.import_count)
                        .ok_or_else(|| failure(code::CODE_TOO_LARGE))?;
                    *slot = ImportRecord {
                        specifier: u32::MAX,
                        name: u32::MAX,
                        slot: u32::MAX,
                    };
                    program.import_count += 1;
                }
                for &clause in clauses {
                    let Some(record) = arena.node(clause).copied() else {
                        continue;
                    };
                    let Some(local) = arena.node(record.first).copied() else {
                        continue;
                    };
                    let index = u32::try_from(program.import_count).unwrap_or(0);
                    program.declare(
                        scope,
                        Binding {
                            start: local.first,
                            end: local.second,
                            kind: binding_kind::IMPORT,
                            slot: index,
                        },
                    )?;
                    let slot = program
                        .imports
                        .get_mut(program.import_count)
                        .ok_or_else(|| failure(code::CODE_TOO_LARGE))?;
                    // The names are filled in when the module is lowered; what
                    // is known here is which clause each import is.
                    *slot = ImportRecord {
                        specifier: u32::MAX,
                        name: u32::MAX,
                        slot: clause,
                    };
                    program.import_count += 1;
                }
            }
            NodeKind::Export if node.has(flag::PREFIX) => {
                // A default-exported named function declaration also binds
                // its own name in the module's scope.
                if let Some(inner) = arena.node(node.first) {
                    if matches!(inner.kind, NodeKind::Function)
                        && inner.has(flag::DECLARATION)
                        && inner.first != NONE
                    {
                        if let Some(name) = arena.node(inner.first) {
                            program.declare(
                                scope,
                                Binding {
                                    start: name.first,
                                    end: name.second,
                                    kind: binding_kind::FUNCTION,
                                    slot: 0,
                                },
                            )?;
                        }
                    }
                }
                // A default export is held in a binding whose name is empty,
                // which is a name no program can write.
                program.declare(
                    scope,
                    Binding {
                        start: 0,
                        end: 0,
                        kind: binding_kind::LET,
                        slot: 0,
                    },
                )?;
            }
            NodeKind::Export if node.has(flag::OF) => {
                // A re-export binds nothing, but each clause holds a place
                // in the import table, exactly as a bare import does — and
                // an empty one still keeps its edge to the module it names.
                let places = arena.list(node.second, node.third).len().max(1);
                for _ in 0..places {
                    let slot = program
                        .imports
                        .get_mut(program.import_count)
                        .ok_or_else(|| failure(code::CODE_TOO_LARGE))?;
                    *slot = ImportRecord {
                        specifier: u32::MAX,
                        name: u32::MAX,
                        slot: u32::MAX,
                    };
                    program.import_count += 1;
                }
            }
            NodeKind::Export if node.first != NONE => {
                // `export` in front of a declaration declares it as usual.
                let mut single = [node.first];
                let inner =
                    arena
                        .node(node.first)
                        .copied()
                        .unwrap_or(Node::new(NodeKind::Null, 0, 0));
                let _ = &mut single;
                match inner.kind {
                    NodeKind::Declaration | NodeKind::Function | NodeKind::Class => {
                        declare_one(arena, source, node.first, scope, program)?;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    // Everything else a module's top level declares behaves as a block's does.
    declare_lexical(arena, source, list, length, scope, program, false)
}

/// At a module's top level a function declaration is lexical: a `var` of
/// the same name — or a second function — is a duplicate binding. Checked
/// after `var` hoisting, when every top-level name is in the scope.
pub(super) fn check_module_duplicates(
    source: &[u8],
    scope: u32,
    program: &Program<'_>,
) -> Result<(), Diagnostic> {
    let record = program.scope(scope);
    let first = record.first as usize;
    let mut index = 0u32;
    while index < record.count {
        let Some(binding) = program.bindings.get(first + index as usize).copied() else {
            break;
        };
        if binding.kind == binding_kind::FUNCTION {
            let held = source
                .get(binding.start as usize..binding.end as usize)
                .unwrap_or(&[]);
            let mut other = 0u32;
            while other < record.count {
                if other != index {
                    if let Some(candidate) = program.bindings.get(first + other as usize) {
                        if matches!(
                            candidate.kind,
                            binding_kind::VARIABLE | binding_kind::FUNCTION
                        ) && same_name(
                            held,
                            source
                                .get(candidate.start as usize..candidate.end as usize)
                                .unwrap_or(&[]),
                        ) {
                            return Err(failure(code::DUPLICATE_BINDING));
                        }
                    }
                }
                other += 1;
            }
        }
        index += 1;
    }
    Ok(())
}

/// Declare the names one statement introduces, as a list of one would.
pub(super) fn declare_one(
    arena: &Arena<'_>,
    source: &[u8],
    statement: u32,
    scope: u32,
    program: &mut Program<'_>,
) -> Result<(), Diagnostic> {
    let Some(node) = arena.node(statement).copied() else {
        return Ok(());
    };
    match node.kind {
        NodeKind::Declaration if node.third != declaration::VAR => {
            let kind = if matches!(
                node.third,
                declaration::CONST | declaration::USING | declaration::AWAIT_USING
            ) {
                binding_kind::CONST
            } else {
                binding_kind::LET
            };
            for &declarator in arena.list(node.first, node.second) {
                let Some(record) = arena.node(declarator) else {
                    continue;
                };
                declare_pattern(arena, record.first, kind, scope, program)?;
            }
        }
        NodeKind::Declaration => hoist_statement(arena, statement, scope, program)?,
        NodeKind::Class if node.first != NONE => {
            if let Some(name) = arena.node(node.first) {
                program.declare(
                    scope,
                    Binding {
                        start: name.first,
                        end: name.second,
                        kind: binding_kind::LET,
                        slot: 0,
                    },
                )?;
            }
        }
        NodeKind::Function if node.first != NONE => {
            if let Some(name) = arena.node(node.first) {
                program.declare(
                    scope,
                    Binding {
                        start: name.first,
                        end: name.second,
                        kind: binding_kind::FUNCTION,
                        slot: 0,
                    },
                )?;
            }
        }
        _ => {}
    }
    let _ = source;
    Ok(())
}

/// Declare the `let`, `const`, and function names a statement list introduces.
///
/// At a script's top level a function declaration becomes a property of the
/// global object rather than a slot, which is what makes one script's functions
/// visible to the next.
/// Declare every name a binding target contains, in source order: a bare
/// name is one binding; a pattern is each of its elements in turn.
pub(super) fn declare_pattern(
    arena: &Arena<'_>,
    target: u32,
    kind: u8,
    scope: u32,
    program: &mut Program<'_>,
) -> Result<(), Diagnostic> {
    if target == NONE {
        return Ok(());
    }
    let Some(node) = arena.node(target).copied() else {
        return Ok(());
    };
    match node.kind {
        NodeKind::ArrayPattern | NodeKind::ObjectPattern => {
            for &child in arena.list(node.first, node.second) {
                let Some(record) = arena.node(child).copied() else {
                    continue;
                };
                match record.kind {
                    NodeKind::Elision => {}
                    NodeKind::RestElement => {
                        declare_pattern(arena, record.first, kind, scope, program)?;
                    }
                    NodeKind::PatternProperty => {
                        let element = arena.node(record.second).copied();
                        if let Some(element) = element {
                            declare_pattern(arena, element.first, kind, scope, program)?;
                        }
                    }
                    // A binding element: the target, whatever its default.
                    _ => declare_pattern(arena, record.first, kind, scope, program)?,
                }
            }
            Ok(())
        }
        _ => program
            .declare(
                scope,
                Binding {
                    start: node.first,
                    end: node.second,
                    kind,
                    slot: 0,
                },
            )
            .map(|_| ()),
    }
}

pub(super) fn declare_lexical(
    arena: &Arena<'_>,
    source: &[u8],
    list: u32,
    length: u32,
    scope: u32,
    program: &mut Program<'_>,
    script: bool,
) -> Result<(), Diagnostic> {
    let _ = source;
    for &statement in arena.list(list, length) {
        let Some(node) = arena.node(statement) else {
            continue;
        };
        let node = if matches!(node.kind, NodeKind::Decorated) {
            match arena.node(node.third) {
                Some(inner) => inner,
                None => continue,
            }
        } else {
            node
        };
        match node.kind {
            NodeKind::Declaration if node.third != declaration::VAR => {
                let kind = if matches!(
                    node.third,
                    declaration::CONST | declaration::USING | declaration::AWAIT_USING
                ) {
                    binding_kind::CONST
                } else {
                    binding_kind::LET
                };
                for &declarator in arena.list(node.first, node.second) {
                    let Some(record) = arena.node(declarator) else {
                        continue;
                    };
                    declare_pattern(arena, record.first, kind, scope, program)?;
                }
            }
            NodeKind::Class if node.first != NONE => {
                if let Some(name) = arena.node(node.first) {
                    program.declare(
                        scope,
                        Binding {
                            start: name.first,
                            end: name.second,
                            kind: binding_kind::LET,
                            slot: 0,
                        },
                    )?;
                }
            }
            NodeKind::Function if !script && node.first != NONE => {
                if let Some(name) = arena.node(node.first) {
                    program.declare(
                        scope,
                        Binding {
                            start: name.first,
                            end: name.second,
                            kind: binding_kind::FUNCTION,
                            slot: 0,
                        },
                    )?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Collect the `var` names a function body declares, wherever they are written.
///
/// The walk enters blocks, loops, and handlers but not nested functions: a
/// `var` inside one belongs to that function, not to this one.
pub(super) fn hoist_vars(
    arena: &Arena<'_>,
    list: u32,
    length: u32,
    scope: u32,
    program: &mut Program<'_>,
) -> Result<(), Diagnostic> {
    for &statement in arena.list(list, length) {
        hoist_statement(arena, statement, scope, program)?;
    }
    Ok(())
}

pub(super) fn hoist_statement(
    arena: &Arena<'_>,
    index: u32,
    scope: u32,
    program: &mut Program<'_>,
) -> Result<(), Diagnostic> {
    if index == NONE {
        return Ok(());
    }
    let Some(node) = arena.node(index).copied() else {
        return Ok(());
    };
    match node.kind {
        NodeKind::Declaration if node.third == declaration::VAR => {
            for &declarator in arena.list(node.first, node.second) {
                let Some(record) = arena.node(declarator) else {
                    continue;
                };
                declare_pattern(arena, record.first, binding_kind::VARIABLE, scope, program)?;
            }
        }
        NodeKind::Block => hoist_vars(arena, node.first, node.second, scope, program)?,
        NodeKind::If => {
            hoist_statement(arena, node.second, scope, program)?;
            hoist_statement(arena, node.third, scope, program)?;
        }
        NodeKind::While => hoist_statement(arena, node.second, scope, program)?,
        NodeKind::DoWhile => hoist_statement(arena, node.first, scope, program)?,
        NodeKind::For => {
            hoist_statement(arena, node.first, scope, program)?;
            if let Some(&body) = arena.list(node.second, node.third).get(2) {
                hoist_statement(arena, body, scope, program)?;
            }
        }
        NodeKind::ForInOf => {
            hoist_statement(arena, node.first, scope, program)?;
            hoist_statement(arena, node.third, scope, program)?;
        }
        NodeKind::Labelled => hoist_statement(arena, node.second, scope, program)?,
        NodeKind::With => hoist_statement(arena, node.second, scope, program)?,
        NodeKind::Try => {
            hoist_statement(arena, node.first, scope, program)?;
            if node.second != NONE {
                if let Some(handler) = arena.node(node.second).copied() {
                    hoist_statement(arena, handler.second, scope, program)?;
                }
            }
            hoist_statement(arena, node.third, scope, program)?;
        }
        NodeKind::Switch => {
            for &case in arena.list(node.second, node.third) {
                let Some(record) = arena.node(case).copied() else {
                    continue;
                };
                hoist_vars(arena, record.second, record.third, scope, program)?;
            }
        }
        _ => {}
    }
    Ok(())
}

impl Lowering<'_, '_, '_, '_> {
    /// Make the closures a scope's function declarations name, before any of
    /// its statements run.
    pub(super) fn declare_functions(&mut self, list: u32, length: u32) {
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

    pub(super) fn declare_script_functions_in_blocks(&mut self, _list: u32, _length: u32) {}
}
