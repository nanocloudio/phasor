//! Functions: emitting one, its prologue and parameters, and the queue of bodies still to lower.

use super::*;

/// Lower one function record: its scope, its prologue, its body, and its exit.
#[expect(
    clippy::too_many_arguments,
    reason = "the per-function buffers are the caller's storage and grouping them would hide which is which"
)]
pub(super) fn emit_function(
    source: &[u8],
    arena: &Arena<'_>,
    body: Body,
    enclosing_scope: u32,
    index: u32,
    enclosing_strict: bool,
    enclosing_with: bool,
    class_constructor: bool,
    derived_constructor: bool,
    method: bool,
    super_call: bool,
    new_target: bool,
    deny_arguments: bool,
    privates: bool,
    brand: bool,
    program: &mut Program<'_>,
    code: &mut [u8],
    safe_points: &mut [u32],
    patches: &mut [Patch],
    labels: &mut [u32],
) -> Result<(), Diagnostic> {
    let scope = program.open_scope(enclosing_scope)?;
    let mut body_scope = scope;
    let mut arrow = false;
    let mut parameters = 0u32;
    let mut parameter_bindings = 0u32;
    let mut has_rest = false;
    let mut simple = true;
    // `Function.prototype.length`: the parameters before the first default
    // or the rest, however the later ones are written.
    let mut arity = 0u32;
    let mut arity_frozen = false;
    let mut polluted = false;
    let mut asynchronous = false;
    let mut generator = false;
    let mut self_name = false;
    let mut restricted_self_name = None;
    let mut uses_arguments = false;
    // Module code is strict throughout, whatever its prologue says.
    let mut strict = enclosing_strict || program.module;

    // What the function or script declares is known before anything runs: a
    // name is in scope throughout the body it belongs to, whatever order the
    // statements are written in.
    let (statements, statement_count, concise) = match body {
        Body::Script(root) => {
            let node = arena
                .node(root)
                .copied()
                .unwrap_or(Node::new(NodeKind::Null, 0, 0));
            if program.module && matches!(node.kind, NodeKind::Script) {
                // A module's top level is a scope of its own: its `var`s, its
                // declarations, its imports, and its function declarations all
                // live in the environment the module is given.
                declare_module(arena, source, node.first, node.second, scope, program)?;
                hoist_vars(arena, node.first, node.second, scope, program)?;
                check_module_duplicates(source, scope, program)?;
                if let Some(record) = program.scopes.get_mut(scope as usize) {
                    record.context = true;
                }
                (node.first, node.second, false)
            } else if matches!(node.kind, NodeKind::Script) {
                // Strict eval code keeps its `var`s and its functions: they
                // are bindings of the eval's own scope, not properties of
                // the global object and not writes into the caller.
                let strict_eval = program.eval_goal
                    && (enclosing_strict
                        || directive_prologue_is_strict(arena, source, node.first, node.second));
                declare_lexical(
                    arena,
                    source,
                    node.first,
                    node.second,
                    scope,
                    program,
                    !strict_eval,
                )?;
                if strict_eval {
                    // Declaring is what marks the scope a context, so an
                    // eval that hoists nothing claims none and pushes none.
                    hoist_vars(arena, node.first, node.second, scope, program)?;
                } else if program.eval_goal {
                    // A sloppy eval's own `var` names never resolve into the
                    // caller: the declaration makes a fresh binding in the
                    // variable environment, whatever the site could see.
                    program.eval_var_statements = (node.first, node.second);
                }
                // A script that is one expression compiles to exactly that
                // expression: there is nothing for a completion value to
                // outlive, so it needs no register of its own.
                let statements = arena.list(node.first, node.second);
                let single = statements.len() == 1
                    && program.scope(scope).count == 0
                    && arena
                        .node(statements[0])
                        .is_some_and(|only| matches!(only.kind, NodeKind::ExpressionStatement));
                if single {
                    let only = arena.node(statements[0]).map_or(NONE, |only| only.first);
                    (only, u32::MAX, true)
                } else {
                    (node.first, node.second, false)
                }
            } else {
                // A caller that parsed one expression rather than a script.
                (root, u32::MAX, true)
            }
        }
        Body::DefaultConstructor => {
            // Class bodies are strict, and the synthesised constructor has
            // no parameters and no written statements of its own. The
            // derived form stages every actual argument for `super`.
            strict = true;
            has_rest = true;
            (NONE, 0, false)
        }
        Body::Function(node_index) => {
            let node = arena
                .node(node_index)
                .copied()
                .unwrap_or(Node::new(NodeKind::Null, 0, 0));
            arrow = node.has(flag::ARROW);
            asynchronous = node.has(flag::ASYNC);
            generator = node.has(flag::GENERATOR);
            // A function whose text mentions `eval` may direct-eval into its
            // own environment, so that environment keeps spare capacity. The
            // mention is decided conservatively: an unused reserve costs
            // slots, never behaviour.
            polluted = source
                .get(node.start as usize..node.end as usize)
                .is_some_and(|body| find_text(body, b"eval").is_some());
            let entries = arena.list(node.second, node.third);
            let body_index = entries.first().copied().unwrap_or(NONE);
            // A named function expression can call itself by its own name —
            // declared after everything else, so a parameter or a `var` of
            // the same name shadows it, as the specification's separate
            // environment for the name does.
            let self_name_node = if !arrow && node.first != NONE && !node.has(flag::DECLARATION) {
                self_name = true;
                arena.node(node.first).copied()
            } else {
                None
            };
            if let Some(name) = self_name_node {
                if matches!(
                    source.get(name.first as usize..name.second as usize),
                    Some(b"eval" | b"arguments")
                ) {
                    restricted_self_name = Some(name.start);
                }
            }
            for &parameter in entries.get(1..).unwrap_or(&[]) {
                let Some(record) = arena.node(parameter).copied() else {
                    continue;
                };
                if record.third == parameter_kind::REST {
                    has_rest = true;
                    simple = false;
                    arity_frozen = true;
                    let target = arena.node(record.first).map_or(NONE, |rest| rest.first);
                    declare_pattern(arena, target, binding_kind::VARIABLE, scope, program)?;
                    continue;
                }
                if record.second != NONE {
                    simple = false;
                    arity_frozen = true;
                } else if !arity_frozen {
                    arity += 1;
                }
                if !arena
                    .node(record.first)
                    .is_some_and(|target| matches!(target.kind, NodeKind::Identifier))
                {
                    simple = false;
                }
                declare_pattern(arena, record.first, binding_kind::VARIABLE, scope, program)?;
                parameters += 1;
            }
            parameter_bindings = program.scope(scope).count;
            // The `arguments` a body may read: a binding the call fills with
            // what was actually supplied. It is declared only when the source
            // mentions the name at all — a program that never asks pays
            // nothing — and never over a parameter of that name, which wins.
            if !arrow {
                if let Some(at) = find_text(source, b"arguments") {
                    let taken = {
                        let record = program.scope(scope);
                        let first = record.first as usize;
                        let mut index = 0u32;
                        let mut found = false;
                        while index < record.count {
                            if let Some(binding) = program.bindings.get(first + index as usize) {
                                let name = source.get(binding.start as usize..binding.end as usize);
                                if name == Some(b"arguments") {
                                    found = true;
                                    break;
                                }
                            }
                            index += 1;
                        }
                        found
                    };
                    if !taken {
                        program.declare(
                            scope,
                            Binding {
                                start: at,
                                end: at + 9,
                                kind: binding_kind::VARIABLE,
                                slot: 0,
                            },
                        )?;
                        // The call leaves what it supplied in the frame's
                        // first registers, and only as many as the frame has:
                        // a frame that reads `arguments` holds room for the
                        // most a call may pass.
                        uses_arguments = true;
                    }
                }
            }
            let body_node =
                arena
                    .node(body_index)
                    .copied()
                    .unwrap_or(Node::new(NodeKind::Null, 0, 0));
            if let Some(name) = self_name_node {
                let text = source.get(name.first as usize..name.second as usize);
                let record = program.scope(scope);
                let first = record.first as usize;
                let mut taken = false;
                let mut index = 0u32;
                while index < record.count {
                    if let Some(binding) = program.bindings.get(first + index as usize) {
                        if let (Some(spelled), Some(wanted)) = (
                            source.get(binding.start as usize..binding.end as usize),
                            text,
                        ) {
                            if same_name(spelled, wanted) {
                                taken = true;
                                break;
                            }
                        }
                    }
                    index += 1;
                }
                // A `var` or function of the same name in the body makes its
                // own binding; the immutable self name never appears.
                if !taken
                    && !node.has(flag::CONCISE_BODY)
                    && text.is_some_and(|needle| {
                        eval_declared_name(
                            arena,
                            source,
                            body_node.first,
                            body_node.second,
                            needle,
                            false,
                        )
                        .is_some()
                    })
                {
                    taken = true;
                }
                if !taken {
                    program.declare(
                        scope,
                        Binding {
                            start: name.first,
                            end: name.second,
                            kind: binding_kind::SELF,
                            slot: 0,
                        },
                    )?;
                }
            }
            let body = if node.has(flag::CONCISE_BODY) {
                (body_index, u32::MAX, true)
            } else {
                // A body's `var` declarations belong to the function — but a
                // non-simple parameter list keeps an environment of its own,
                // so a default's closure never sees the body's bindings.
                if !simple {
                    let split = program.open_scope(scope)?;
                    if let Some(record) = program.scopes.get_mut(split as usize) {
                        record.context = true;
                    }
                    body_scope = split;
                }
                hoist_vars(
                    arena,
                    body_node.first,
                    body_node.second,
                    body_scope,
                    program,
                )?;
                declare_lexical(
                    arena,
                    source,
                    body_node.first,
                    body_node.second,
                    body_scope,
                    program,
                    false,
                )?;
                (body_node.first, body_node.second, false)
            };
            body
        }
    };

    // A directive prologue makes the body strict code: leading statements
    // that are string literals, one of which reads exactly `use strict`.
    if !strict
        && statement_count != u32::MAX
        && directive_prologue_is_strict(arena, source, statements, statement_count)
    {
        strict = true;
    }

    // A directive prologue that makes the code strict refuses a legacy
    // escape in any of its directives, `"use strict"` itself included.
    if strict && statement_count != u32::MAX {
        for &statement in arena.list(statements, statement_count) {
            let Some(node) = arena.node(statement) else {
                break;
            };
            if !matches!(node.kind, NodeKind::ExpressionStatement) {
                break;
            }
            let Some(expression) = arena.node(node.first) else {
                break;
            };
            if !matches!(expression.kind, NodeKind::String) {
                break;
            }
            if expression.has(flag::LEGACY_OCTAL) {
                return Err(Diagnostic::at(
                    code::LEGACY_OCTAL_ESCAPE,
                    Severity::Error,
                    expression.start,
                ));
            }
        }
    }

    // A strict function expression may not be named `eval` or `arguments`.
    if strict {
        if let Some(at) = restricted_self_name {
            return Err(Diagnostic::at(
                code::STRICT_INVALID_PARAMETER,
                Severity::Error,
                at,
            ));
        }
    }

    // Strict code may not `var`- or function-declare a restricted word:
    // `eval`, `arguments`, or a strict-mode reserved word.
    if strict && statement_count != u32::MAX {
        for name in [
            &b"eval"[..],
            b"arguments",
            b"implements",
            b"interface",
            b"let",
            b"package",
            b"private",
            b"protected",
            b"public",
            b"static",
            b"yield",
        ] {
            if let Some(at) =
                eval_declared_name(arena, source, statements, statement_count, name, true)
            {
                return Err(Diagnostic::at(
                    code::STRICT_INVALID_PARAMETER,
                    Severity::Error,
                    at,
                ));
            }
        }
    }

    // A function whose parameters carry a pattern, a default, or a rest may
    // not declare its own strictness: the directive would govern the very
    // list that precedes it.
    if !simple
        && !enclosing_strict
        && statement_count != u32::MAX
        && directive_prologue_is_strict(arena, source, statements, statement_count)
        && matches!(body, Body::Function(_))
    {
        if let Some(binding) = program.bindings.get(program.scope(scope).first as usize) {
            return Err(Diagnostic::at(
                code::STRICT_INVALID_PARAMETER,
                Severity::Error,
                binding.start,
            ));
        }
    }

    // A strict function refuses a parameter named `eval` or `arguments`, and
    // strict or pattern-bearing lists both refuse two parameters with one
    // name — early errors, before anything runs.
    if (strict || !simple) && parameter_bindings > 0 {
        let record = program.scope(scope);
        let first = record.first as usize;
        let mut index = 0usize;
        while index < parameter_bindings as usize {
            let Some(binding) = program.bindings.get(first + index) else {
                break;
            };
            let name = source
                .get(binding.start as usize..binding.end as usize)
                .unwrap_or(&[]);
            if strict && (name == b"eval" || name == b"arguments") {
                return Err(Diagnostic::at(
                    code::STRICT_INVALID_PARAMETER,
                    Severity::Error,
                    binding.start,
                ));
            }
            let mut earlier = 0usize;
            while earlier < index {
                let Some(other) = program.bindings.get(first + earlier) else {
                    break;
                };
                if source.get(other.start as usize..other.end as usize) == Some(name) {
                    return Err(Diagnostic::at(
                        code::STRICT_INVALID_PARAMETER,
                        Severity::Error,
                        binding.start,
                    ));
                }
                earlier += 1;
            }
            index += 1;
        }
    }

    // A sloppy simple list may repeat a name: the later parameter is the
    // one the name reaches, so an earlier binding loses its name while
    // keeping its slot for the argument it still receives.
    if !strict && simple && parameter_bindings > 1 {
        let first = program.scope(scope).first as usize;
        let count = parameter_bindings as usize;
        let mut index = 0usize;
        while index < count {
            let name = program
                .bindings
                .get(first + index)
                .map(|binding| (binding.start, binding.end));
            let mut later = index + 1;
            let mut shadowed = false;
            while later < count {
                if let (Some((start, end)), Some(other)) =
                    (name, program.bindings.get(first + later))
                {
                    if source.get(start as usize..end as usize)
                        == source.get(other.start as usize..other.end as usize)
                    {
                        shadowed = true;
                    }
                }
                later += 1;
            }
            if shadowed {
                if let Some(binding) = program.bindings.get_mut(first + index) {
                    binding.start = 0;
                    binding.end = 0;
                }
            }
            index += 1;
        }
    }

    let slots = program.scope(scope).count;
    if matches!(body, Body::Function(_) | Body::DefaultConstructor) {
        // A call always makes the callee an environment, so its scope owns a
        // context even when it declares nothing.
        if let Some(record) = program.scopes.get_mut(scope as usize) {
            record.context = true;
        }
    }

    let module_body = program.module && matches!(body, Body::Script(_));
    // Any direct eval in the source can create a binding a closure compiled
    // here may capture, so the mention is measured over the whole source:
    // dynamic fallbacks behave exactly as global ones until an eval actually
    // declares something. Strict code resolves dynamically too: its own
    // eval adds nothing to this scope, but a sloppy eval in an enclosing
    // one may have.
    let dynamic_names = !module_body && (program.eval_goal || find_text(source, b"eval").is_some());
    // Where `super` may appear: method-like code — or, for the whole of an
    // eval source, wherever the recorded call site allowed it.
    let (
        allow_super_property,
        allow_super_call,
        allow_new_target,
        deny_arguments,
        privates_visible,
    ) = match body {
        Body::Script(_) if program.eval_goal => (
            program.eval_super_property,
            program.eval_super_call,
            program.eval_new_target,
            program.eval_deny_arguments,
            program.eval_privates,
        ),
        Body::Script(_) => (false, false, false, false, false),
        Body::Function(_) => (
            method || class_constructor,
            super_call || derived_constructor,
            new_target,
            deny_arguments,
            privates,
        ),
        Body::DefaultConstructor => (true, derived_constructor, true, false, privates),
    };
    let (code_length, register_count, context_depth, safe_point_count, exception_first) = {
        let mut builder = CodeBuilder::new(code, safe_points, patches, labels);
        let exception_first = program.exception_count;
        let mut lowering = Lowering {
            source,
            arena,
            builder: &mut builder,
            program,
            function_index: index,
            registers: 0,
            high_water: if uses_arguments || has_rest {
                MAX_CALL_ARGUMENTS
            } else if parameters > 1 {
                parameters
            } else {
                1
            },
            scope,
            context_depth: 0,
            max_context_depth: 0,
            targets: [Target::EMPTY; MAX_TARGETS],
            target_count: 0,
            finalisers: [Finaliser::EMPTY; MAX_FINALISERS],
            finaliser_count: 0,
            try_depth: 0,
            chain_exit: None,
            holes: [(0, 0, 0); MAX_HOLES],
            hole_count: 0,
            in_function: matches!(body, Body::Function(_) | Body::DefaultConstructor),
            function_scope: scope,
            in_arrow: arrow,
            in_parameters: false,
            dynamic_names,
            pending_class_name: None,
            dispose_stack: u32::MAX,
            with_depth: u32::from(enclosing_with),
            in_async_generator: asynchronous && generator,
            tail_calls: strict && !generator && !asynchronous && matches!(body, Body::Function(_)),
            tail_spans: [(0, 0); MAX_TAIL_SPANS],
            tail_span_count: 0,
            allow_super_property,
            allow_super_call,
            allow_new_target,
            deny_arguments,
            privates_visible,
            brand_instances: brand,
            strict,
            completion: u32::MAX,
            default_export_slot: 0,
        };
        lowering.builder.safe_point();

        match body {
            Body::DefaultConstructor => {
                if derived_constructor {
                    // `constructor(...args) { super(...args); }`
                    let list = lowering.allocate();
                    lowering.emit(Opcode::CreateRestArguments, &[0]);
                    lowering.emit(Opcode::Star, &[i64::from(list)]);
                    lowering.builder.safe_point();
                    lowering.emit(Opcode::CallSuperWithArray, &[i64::from(list)]);
                    lowering.emit(Opcode::BindThis, &[]);
                }
                {
                    let brand = lowering.brand_instances;
                    lowering.emit_init_fields(brand);
                }
                lowering.emit(Opcode::LdaUndefined, &[]);
                lowering.emit(Opcode::Return, &[]);
            }
            Body::Function(function_node) => {
                lowering.function_prologue(
                    self_name,
                    function_node,
                    simple,
                    parameters,
                    parameter_bindings,
                    statements,
                    statement_count,
                    concise,
                );
                if body_scope != scope {
                    // The body's own environment, over the parameters'. Its
                    // `var` and function slots start as undefined — only the
                    // body's lexicals keep a dead zone.
                    lowering.scope = body_scope;
                    let slots = lowering.program.scope(body_scope).count.max(1);
                    lowering.push_context(slots);
                    let record = lowering.program.scope(body_scope);
                    let first = record.first as usize;
                    let count = record.count;
                    let mut index = 0u32;
                    while index < count {
                        let Some(binding) = lowering
                            .program
                            .bindings
                            .get(first + index as usize)
                            .copied()
                        else {
                            break;
                        };
                        if matches!(
                            binding.kind,
                            binding_kind::VARIABLE | binding_kind::FUNCTION
                        ) {
                            lowering.emit(Opcode::LdaUndefined, &[]);
                            lowering.emit(Opcode::InitContextSlot, &[i64::from(binding.slot), 0]);
                        }
                        index += 1;
                    }
                    lowering.declare_functions(statements, statement_count);
                }
                if generator {
                    // The call binds the parameters, then answers the
                    // generator object; the body runs when `next` does.
                    lowering.builder.safe_point();
                    lowering.emit(Opcode::InitialYield, &[]);
                }
                if class_constructor && !derived_constructor {
                    // A base class's instance fields take their values
                    // before the body runs; a derived class's wait for
                    // `super()` to make `this`.
                    {
                        let brand = lowering.brand_instances;
                        lowering.emit_init_fields(brand);
                    }
                }
                if concise {
                    lowering.expression(statements);
                    lowering.emit(Opcode::Return, &[]);
                } else {
                    lowering.statements(statements, statement_count);
                    // A body that ends by returning needs no return of its own,
                    // and one written after it would be unreachable.
                    if !lowering.builder.terminated() {
                        lowering.emit(Opcode::LdaUndefined, &[]);
                        while lowering.context_depth > 0 {
                            lowering.pop_context();
                        }
                        lowering.emit(Opcode::Return, &[]);
                    }
                }
            }
            Body::Script(_) if module_body => {
                // A module's environment is made before it runs and given to
                // it, so its slots need no context of their own.
                lowering.module_prologue(statements, statement_count);
                lowering.statements(statements, statement_count);
                if !lowering.builder.terminated() {
                    lowering.emit(Opcode::LdaUndefined, &[]);
                    lowering.emit(Opcode::Return, &[]);
                }
            }
            Body::Script(_) => {
                if concise {
                    lowering.expression(statements);
                    lowering.emit(Opcode::Return, &[]);
                } else {
                    let pushed = slots > 0;
                    if pushed {
                        lowering.emit(Opcode::PushContext, &[i64::from(slots)]);
                        lowering.context_depth = 1;
                        lowering.max_context_depth = 1;
                        // A `var` exists as `undefined` before anything runs;
                        // only the lexical declarations keep their dead zone.
                        lowering.initialise_hoisted(scope);
                    }
                    lowering.script_prologue(statements, statement_count);
                    let completion = lowering.allocate();
                    lowering.emit(Opcode::LdaUndefined, &[]);
                    lowering.emit(Opcode::Star, &[i64::from(completion)]);
                    lowering.completion = completion;
                    lowering.statements(statements, statement_count);
                    if !lowering.builder.terminated() {
                        lowering.emit(Opcode::Ldar, &[i64::from(completion)]);
                        if pushed {
                            lowering.emit(Opcode::PopContext, &[]);
                            lowering.context_depth = 0;
                        }
                        lowering.emit(Opcode::Return, &[]);
                    }
                }
            }
        }

        let register_count = lowering.high_water;
        let context_depth = lowering.max_context_depth;
        let safe_point_count = builder.safe_points().len();
        let length = builder.finish().map_err(build_failure)?;
        (
            length,
            register_count,
            context_depth,
            safe_point_count,
            exception_first,
        )
    };

    if let Some(failure) = program.failure.take() {
        return Err(failure);
    }

    // The function's code joins the unit's code section, and its safe points
    // and exception regions are recorded as a range inside the unit's own.
    let offset = program.unit_code_length;
    let source_code = code
        .get(..code_length as usize)
        .ok_or_else(|| failure(code::CODE_TOO_LARGE))?;
    let target = program
        .unit_code
        .get_mut(offset..offset + source_code.len())
        .ok_or_else(|| failure(code::CODE_TOO_LARGE))?;
    target.copy_from_slice(source_code);
    program.unit_code_length += source_code.len();

    let safe_point_offset = program.unit_safe_point_count;
    let points = safe_points
        .get(..safe_point_count)
        .ok_or_else(|| failure(code::CODE_TOO_LARGE))?;
    let target = program
        .unit_safe_points
        .get_mut(safe_point_offset..safe_point_offset + points.len())
        .ok_or_else(|| failure(code::CODE_TOO_LARGE))?;
    target.copy_from_slice(points);
    program.unit_safe_point_count += points.len();

    // The regions were recorded as each `try` finished, so an inner one comes
    // before the outer one that contains it. The image wants them by position,
    // with a container before what it contains.
    let exception_end = program.exception_count;
    sort_regions(
        program
            .exceptions
            .get_mut(exception_first..exception_end)
            .unwrap_or(&mut []),
    );

    let record = Function {
        code_offset: u32::try_from(offset).unwrap_or(0),
        code_length,
        register_count,
        argument_count: arity,
        frame_extent: register_count,
        exception_offset: u32::try_from(exception_first).unwrap_or(0),
        exception_count: u32::try_from(program.exception_count - exception_first).unwrap_or(0),
        safe_point_offset: u32::try_from(safe_point_offset).unwrap_or(0),
        safe_point_count: u32::try_from(safe_point_count).unwrap_or(0),
        context_depth,
        context_slots: if matches!(body, Body::Function(_)) || module_body {
            slots
        } else {
            0
        },
        flags: {
            let mut flags = if arrow { function_flag::ARROW } else { 0 };
            if strict {
                flags |= function_flag::STRICT;
            }
            if polluted && !strict {
                flags |= function_flag::DYNAMIC;
            }
            if asynchronous {
                flags |= function_flag::ASYNC;
            }
            if generator {
                flags |= function_flag::GENERATOR;
            }
            if derived_constructor {
                flags |= function_flag::DERIVED_CONSTRUCTOR;
            }
            if method && !class_constructor {
                flags |= function_flag::METHOD;
            }
            if module_body && find_text(source, b"await").is_some() {
                // Top-level await: the module's own frame can suspend, so it
                // runs as async code and completes through the job queue.
                flags |= function_flag::ASYNC;
            }
            flags
        },
    };
    let slot = program
        .functions
        .get_mut(index as usize)
        .ok_or_else(|| failure(code::CODE_TOO_LARGE))?;
    *slot = record;
    Ok(())
}

/// Order one function's exception regions by where they start, with a region
/// that contains another written first.
pub(super) fn sort_regions(regions: &mut [ExceptionRegion]) {
    let mut index = 1usize;
    while index < regions.len() {
        let mut at = index;
        while at > 0 {
            let (left, right) = (regions[at - 1], regions[at]);
            let ordered =
                left.start < right.start || (left.start == right.start && left.end >= right.end);
            if ordered {
                break;
            }
            regions[at - 1] = right;
            regions[at] = left;
            at -= 1;
        }
        index += 1;
    }
}

impl Lowering<'_, '_, '_, '_> {
    /// Queue a class constructor's body: a function whose prologue also
    /// runs the class's instance fields.
    /// Initialise the running constructor's instance fields on `this`: each
    /// stored initialiser runs as an ordinary interpreter call — which is
    /// what lets a field's own direct eval pause for the compiler — and the
    /// value lands through DefineField.
    pub(super) fn emit_init_fields(&mut self, brand: bool) {
        let mark = self.registers;
        let saved = self.allocate();
        let arr = self.allocate();
        let len = self.allocate();
        let position = self.allocate();
        let name = self.allocate();
        let callee = self.allocate();
        let receiver = self.allocate();
        let loop_head = self.builder.label();
        let no_init = self.builder.label();
        let have = self.builder.label();
        let end = self.builder.label();
        let fields_key = self.text_key_constant(b"\0fields");
        let length_key = self.text_key_constant(b"length");
        // The construct that ran before this — `super()`, most of all — left
        // its value in the accumulator, and the walk must not lose it.
        self.emit(Opcode::Star, &[i64::from(saved)]);
        if brand {
            // The class declares a private member: the instance takes the
            // prototype's brand before any initialiser can observe it.
            self.emit(Opcode::Brand, &[]);
        }
        self.emit(Opcode::LdaCallee, &[]);
        self.emit(Opcode::Star, &[i64::from(arr)]);
        self.emit(
            Opcode::GetNamedProperty,
            &[i64::from(arr), i64::from(fields_key)],
        );
        self.emit(Opcode::Star, &[i64::from(arr)]);
        self.builder.jump(Opcode::JumpIfNullish, end);
        self.emit(
            Opcode::GetNamedProperty,
            &[i64::from(arr), i64::from(length_key)],
        );
        self.emit(Opcode::Star, &[i64::from(len)]);
        self.emit(Opcode::LdaZero, &[]);
        self.emit(Opcode::Star, &[i64::from(position)]);
        self.builder.safe_point();
        self.builder.bind(loop_head);
        self.emit(Opcode::Ldar, &[i64::from(len)]);
        self.emit(Opcode::TestLess, &[i64::from(position)]);
        self.builder.jump(Opcode::JumpIfFalse, end);
        self.emit(Opcode::Ldar, &[i64::from(position)]);
        self.emit(Opcode::GetKeyedProperty, &[i64::from(arr)]);
        self.emit(Opcode::Star, &[i64::from(name)]);
        self.emit(Opcode::Ldar, &[i64::from(position)]);
        self.emit(Opcode::Inc, &[]);
        self.emit(Opcode::Star, &[i64::from(position)]);
        self.emit(Opcode::GetKeyedProperty, &[i64::from(arr)]);
        self.emit(Opcode::Star, &[i64::from(callee)]);
        self.emit(Opcode::Ldar, &[i64::from(position)]);
        self.emit(Opcode::Inc, &[]);
        self.emit(Opcode::Star, &[i64::from(position)]);
        self.emit(Opcode::Ldar, &[i64::from(callee)]);
        self.builder.jump(Opcode::JumpIfNullish, no_init);
        self.emit(Opcode::LdaThis, &[]);
        self.emit(Opcode::Star, &[i64::from(receiver)]);
        self.builder.safe_point();
        self.emit(Opcode::Call, &[i64::from(callee), i64::from(receiver), 1]);
        self.builder.jump(Opcode::Jump, have);
        self.builder.bind(no_init);
        self.emit(Opcode::LdaUndefined, &[]);
        self.builder.bind(have);
        self.emit(Opcode::DefineField, &[i64::from(name)]);
        self.builder.jump(Opcode::Jump, loop_head);
        self.builder.bind(end);
        self.emit(Opcode::Ldar, &[i64::from(saved)]);
        self.release(mark);
    }

    pub(super) fn queue_class_constructor(
        &mut self,
        index: u32,
        derived: bool,
        privates: bool,
        brand: bool,
    ) -> u32 {
        let scope = self.scope;
        let reserved = match self.program.reserve_function() {
            Ok(reserved) => reserved,
            Err(diagnostic) => {
                if self.program.failure.is_none() {
                    self.program.failure = Some(diagnostic);
                }
                return 0;
            }
        };
        if let Err(diagnostic) = self.program.queue(
            index,
            scope,
            reserved,
            self.strict,
            self.with_depth > 0,
            true,
            derived,
            true,
            derived,
            true,
            false,
            privates,
            brand,
        ) {
            if self.program.failure.is_none() {
                self.program.failure = Some(diagnostic);
            }
        }
        reserved
    }

    /// Queue a function's body and answer the index its record will take.
    /// An arrow keeps the `super` its surroundings allowed; anything else
    /// starts from what its own definition site grants.
    pub(super) fn queue_function(&mut self, index: u32) -> u32 {
        let arrow = self
            .arena
            .node(index)
            .is_some_and(|node| node.has(flag::ARROW));
        self.queue_callable(
            index,
            if arrow {
                self.allow_super_property
            } else {
                false
            },
            if arrow { self.allow_super_call } else { false },
            if arrow { self.allow_new_target } else { true },
            if arrow { self.deny_arguments } else { false },
            self.privates_visible,
        )
    }

    /// Queue a method or an accessor: code defined with a home object,
    /// whose direct evals may use `super.name`.
    pub(super) fn queue_method(&mut self, index: u32, privates: bool) -> u32 {
        self.queue_callable(index, true, false, true, false, privates)
    }

    /// Queue a field initialiser: home-carrying code that may not
    /// reference `arguments`.
    pub(super) fn queue_field_initialiser(&mut self, index: u32, privates: bool) -> u32 {
        self.queue_callable(index, true, false, true, true, privates)
    }

    pub(super) fn queue_callable(
        &mut self,
        index: u32,
        method: bool,
        super_call: bool,
        new_target: bool,
        deny_arguments: bool,
        privates: bool,
    ) -> u32 {
        let scope = self.scope;
        let reserved = match self.program.reserve_function() {
            Ok(reserved) => reserved,
            Err(diagnostic) => {
                if self.program.failure.is_none() {
                    self.program.failure = Some(diagnostic);
                }
                return 0;
            }
        };
        if let Err(diagnostic) = self.program.queue(
            index,
            scope,
            reserved,
            self.strict,
            self.with_depth > 0,
            false,
            false,
            method,
            super_call,
            new_target,
            deny_arguments,
            privates,
            false,
        ) {
            if self.program.failure.is_none() {
                self.program.failure = Some(diagnostic);
            }
        }
        reserved
    }

    /// A function's parameters and hoisted names get their values before its
    /// first statement runs. A `let` or `const` does not: it stays
    /// uninitialised until its declaration, which is its dead zone.
    #[expect(
        clippy::too_many_arguments,
        reason = "the prologue is one seam between the scope walk and the parameter list, and a struct would only rename the arity"
    )]
    pub(super) fn function_prologue(
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
    pub(super) fn bind_with_default(&mut self, target: u32, default: u32, initialise: bool) {
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
    pub(super) fn bind_target(&mut self, target: u32, initialise: bool) {
        let node = self.node(target);
        match node.kind {
            NodeKind::ArrayPattern => self.bind_array_pattern(&node, initialise),
            NodeKind::ObjectPattern => self.bind_object_pattern(&node, initialise),
            _ if initialise => self.initialise_name(&node),
            _ => self.store_name(&node),
        }
    }
}
