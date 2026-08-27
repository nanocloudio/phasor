//! Lowering from the syntax arena to verified bytecode.
//!
//! The lowering walks the arena once per function, emits instructions into
//! caller-provided storage, interns the constants it needs, assembles a
//! canonical unit image, and then verifies that image. Nothing is published
//! until verification passes, so a caller can only ever receive bytecode the
//! verifier accepted.
//!
//! Registers follow a stack discipline: an expression takes registers above the
//! ones its parent is holding and releases them when it finishes, and the
//! high-water mark becomes the function's declared register count.
//!
//! Names are resolved here, not at run time. Every scope a program declares is
//! a record in a tree that outlives the walk, so a function body lowered after
//! its enclosing code can still see exactly the scopes it was written inside.
//! A name found there becomes a context slot addressed by depth and index; a
//! name found nowhere becomes a global, resolved by the isolate.

use crate::arena::{binary_operator as binop, declaration, flag, unary_operator as unop};
use crate::arena::{
    class_member, parameter_kind, property_key, property_kind, Arena, Node, NodeKind, NONE,
};
use crate::bytecode::Unit;
use crate::bytecode::{
    function_flag, unit_flag, Constant, ConstantKind, ExceptionRegion, ExportRecord, Function,
    ImportRecord, Opcode,
};
use crate::diagnostic::{code, Diagnostic, Severity};
use crate::emit::{BuildError, CodeBuilder, Label, Patch, UnitWriter};
use crate::evalsite::{
    EvalBinding, FLAG_FUNCTION, FLAG_NEW_TARGET, FLAG_NO_ARGUMENTS, FLAG_PARAMETERS, FLAG_PRIVATES,
    FLAG_STRICT, FLAG_SUPER_CALL, FLAG_SUPER_PROPERTY, FLAG_TRUNCATED,
};
use crate::lex::{Token, TokenKind};

/// Storage the lowering writes into. Every buffer is caller-provided, so the
/// compiler allocates nothing.
pub struct Storage<'a> {
    /// One function's code while it is being built.
    pub code: &'a mut [u8],
    pub image: &'a mut [u8],
    pub constants: &'a mut [Constant],
    pub constant_data: &'a mut [u8],
    /// One function's safe points while it is being built.
    pub safe_points: &'a mut [u32],
    pub patches: &'a mut [Patch],
    pub labels: &'a mut [u32],
    pub verifier_state: &'a mut [i32],
    /// Every function's code, concatenated in the order they are emitted.
    pub unit_code: &'a mut [u8],
    /// Every function's safe points, in the same order.
    pub unit_safe_points: &'a mut [u32],
    pub functions: &'a mut [Function],
    pub exceptions: &'a mut [ExceptionRegion],
    pub scopes: &'a mut [Scope],
    pub bindings: &'a mut [Binding],
    pub pending: &'a mut [Pending],
    /// What a module imports and exports, when the source is one.
    pub imports: &'a mut [ImportRecord],
    pub exports: &'a mut [ExportRecord],
    /// Eval-site records: the bindings each direct `eval` call could see.
    pub eval_sites: &'a mut [u8],
}

/// Build the borrowed `Storage` from any struct holding the standard
/// front-end buffers under their standard names. One edit here adds a
/// buffer for every consumer, instead of one edit per fixture.
#[allow(unused_macros, reason = "consumed by the fmods that include this file")]
macro_rules! lower_storage {
    ($s:expr) => {
        lower::Storage {
            code: &mut $s.code,
            image: &mut $s.image,
            constants: &mut $s.constants,
            constant_data: &mut $s.constant_data,
            safe_points: &mut $s.safe_points,
            patches: &mut $s.patches,
            labels: &mut $s.labels,
            verifier_state: &mut $s.verifier_state,
            unit_code: &mut $s.unit_code,
            unit_safe_points: &mut $s.unit_safe_points,
            functions: &mut $s.functions,
            exceptions: &mut $s.exceptions,
            scopes: &mut $s.scopes,
            bindings: &mut $s.lexical,
            pending: &mut $s.pending,
            imports: &mut $s.imports,
            exports: &mut $s.exports,
            eval_sites: &mut $s.eval_sites,
        }
    };
}

/// What the lowering produced.
#[derive(Clone, Copy, Debug)]
pub struct Compiled {
    /// Length of the unit image in `Storage::image`.
    pub length: usize,
    pub register_count: u32,
    pub constant_count: u32,
    pub code_length: u32,
    pub function_count: u32,
}

/// One scope in the tree the whole compilation shares.
#[derive(Clone, Copy, Debug)]
pub struct Scope {
    /// The scope this one is nested in, or `NONE` for the outermost.
    pub parent: u32,
    /// Where this scope's bindings start in the binding table.
    pub first: u32,
    pub count: u32,
    /// Whether entering this scope creates a context at run time. A scope that
    /// declares nothing does not, so a block costs nothing unless it binds.
    pub context: bool,
}

impl Scope {
    pub const EMPTY: Self = Self {
        parent: NONE,
        first: 0,
        count: 0,
        context: false,
    };
}

/// How a name was declared, which decides what may be done with it.
pub mod binding_kind {
    /// A parameter, or a `var`, which may be reassigned and has no dead zone.
    pub const VARIABLE: u8 = 0;
    /// A `let`, which may be reassigned and starts uninitialised.
    pub const LET: u8 = 1;
    /// A `const`, which may not be reassigned.
    pub const CONST: u8 = 2;
    /// A function declaration, which is initialised before anything runs.
    pub const FUNCTION: u8 = 3;
    /// A name another module exports, which is read through that module every
    /// time and may not be assigned here.
    pub const IMPORT: u8 = 4;
    /// A named function expression's own name, which its body reads but may
    /// not reassign: a sloppy write is ignored, as an immutable binding is.
    pub const SELF: u8 = 5;
}

/// One name a scope declares.
#[derive(Clone, Copy, Debug)]
pub struct Binding {
    /// The name's span in the source.
    pub start: u32,
    pub end: u32,
    pub kind: u8,
    /// The slot the name occupies in its scope's context.
    pub slot: u32,
}

impl Binding {
    pub const EMPTY: Self = Self {
        start: 0,
        end: 0,
        kind: binding_kind::VARIABLE,
        slot: 0,
    };
}

/// A function whose body is still to be emitted.
#[derive(Clone, Copy, Debug)]
pub struct Pending {
    /// The `Function` node.
    pub node: u32,
    /// The scope the function was written inside, which its body is lowered in.
    pub scope: u32,
    /// The index its record will take in the unit.
    pub index: u32,
    /// Whether the function was written inside strict code, which its body
    /// then is whatever its own prologue says.
    pub strict: bool,
    /// Whether the function was written inside a `with` body, whose object
    /// its free names must resolve through at run time.
    pub in_with: bool,
    /// Whether the function is a class constructor, whose prologue runs the
    /// class's instance fields.
    pub class_constructor: bool,
    /// Whether that constructor is derived: `this` waits for `super()`.
    pub derived_constructor: bool,
    /// Whether the function carries a home object — a method, an accessor,
    /// or a field initialiser — so its direct evals may use `super.name`.
    pub method: bool,
    /// Whether its direct evals may call `super()`: an arrow written where
    /// the call was already allowed.
    pub super_call: bool,
    /// Whether `new.target` is admitted: function code proper, or an arrow
    /// written inside it.
    pub new_target: bool,
    /// Whether `arguments` is refused: a field initialiser, or an arrow
    /// written inside one.
    pub deny_arguments: bool,
    /// Whether a private scope is visible, so direct evals may reference
    /// private members.
    pub privates: bool,
    /// Whether this constructor stamps its instances with the private
    /// brand: the class declares a private member.
    pub brand: bool,
}

impl Pending {
    pub const EMPTY: Self = Self {
        node: 0,
        scope: NONE,
        index: 0,
        strict: false,
        in_with: false,
        class_constructor: false,
        derived_constructor: false,
        method: false,
        super_call: false,
        new_target: false,
        deny_arguments: false,
        privates: false,
        brand: false,
    };
}

/// Compile one parsed script as strict code throughout, which is how a host
/// runs a strict-only conformance case without a directive of its own.
pub fn lower_script_strict(
    source: &[u8],
    arena: &Arena<'_>,
    root: u32,
    storage: &mut Storage<'_>,
) -> Result<Compiled, Diagnostic> {
    lower_inner(
        source,
        arena,
        root,
        storage,
        true,
        false,
        &[],
        true,
        false,
        u32::MAX,
        false,
        false,
        false,
        false,
        false,
    )
}

/// Compile one parsed script into a verified unit image.
pub fn lower_script(
    source: &[u8],
    arena: &Arena<'_>,
    root: u32,
    storage: &mut Storage<'_>,
) -> Result<Compiled, Diagnostic> {
    lower_script_inner(source, arena, root, storage, true)
}

fn lower_script_inner(
    source: &[u8],
    arena: &Arena<'_>,
    root: u32,
    storage: &mut Storage<'_>,
    verify_image: bool,
) -> Result<Compiled, Diagnostic> {
    lower_inner(
        source,
        arena,
        root,
        storage,
        verify_image,
        false,
        &[],
        false,
        false,
        u32::MAX,
        false,
        false,
        false,
        false,
        false,
    )
}

/// Compile one parsed source as the body of a direct `eval`.
///
/// `scope` is what the call site could see, decoded from the caller image's
/// eval-site record: free names resolve against it before falling to the
/// global object, so the compiled unit runs over the caller's environments.
/// `strict` is the strictness of the code the call was written in, which the
/// eval source inherits before its own directive says anything.
#[allow(
    clippy::too_many_arguments,
    reason = "the eval goal carries the site's whole picture — scope, strictness, function-ness, and variable-environment depth — and a struct would only rename the arity"
)]
pub fn lower_eval(
    source: &[u8],
    arena: &Arena<'_>,
    root: u32,
    storage: &mut Storage<'_>,
    scope: &[EvalBinding<'_>],
    strict: bool,
    function_site: bool,
    var_env_depth: u32,
    super_property: bool,
    super_call: bool,
    new_target: bool,
    deny_arguments: bool,
    privates: bool,
    parameter_site: bool,
) -> Result<Compiled, Diagnostic> {
    // Sloppy eval code whose variable environment is a function environment
    // may not `var`-declare `arguments`: the declaration would shadow the
    // function's own, which the specification refuses as an early error.
    if !strict && function_site {
        if let Some(node) = arena.node(root).copied() {
            if matches!(node.kind, NodeKind::Script)
                && !directive_prologue_is_strict(arena, source, node.first, node.second)
            {
                if let Some(at) =
                    eval_declared_name(arena, source, node.first, node.second, b"arguments", false)
                {
                    return Err(Diagnostic::at(
                        code::EVAL_RESTRICTED_DECLARATION,
                        Severity::Error,
                        at,
                    ));
                }
            }
        }
    }
    // A sloppy eval's `var` may not land where a lexical binding of the same
    // name is visible between the eval and its variable environment: the
    // site record carries each visible binding's kind, so the clash is the
    // compile-time SyntaxError the specification makes it.
    if !strict {
        if let Some(node) = arena.node(root).copied() {
            if matches!(node.kind, NodeKind::Script)
                && !directive_prologue_is_strict(arena, source, node.first, node.second)
            {
                for binding in scope {
                    let kind = u8::try_from(binding.kind).unwrap_or(u8::MAX);
                    let lexical = matches!(kind, binding_kind::LET | binding_kind::CONST);
                    // In a parameter initialiser the parameters themselves
                    // refuse redeclaration: the eval's variable environment
                    // is the parameter environment.
                    let parameter = parameter_site
                        && kind == binding_kind::VARIABLE
                        && binding.depth == var_env_depth;
                    if !lexical && !parameter {
                        continue;
                    }
                    if let Some(at) = eval_declared_name(
                        arena,
                        source,
                        node.first,
                        node.second,
                        binding.name,
                        false,
                    ) {
                        return Err(Diagnostic::at(
                            code::EVAL_RESTRICTED_DECLARATION,
                            Severity::Error,
                            at,
                        ));
                    }
                }
            }
        }
    }
    lower_inner(
        source,
        arena,
        root,
        storage,
        true,
        false,
        scope,
        strict,
        true,
        var_env_depth,
        super_property,
        super_call,
        new_target,
        deny_arguments,
        privates,
    )
}

/// Where eval code `var`-declares or function-declares `needle`, walking the
/// same statements whose `var`s the caller's variable environment receives.
fn eval_declared_name(
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

fn eval_declared_in_statement(
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

/// Whether two identifier spellings name the same binding: equal text, or —
/// where either carries a Unicode escape — equal once cooked to code units.
fn same_name(left: &[u8], right: &[u8]) -> bool {
    if left == right {
        return true;
    }
    if !has_escape(left) && !has_escape(right) {
        return false;
    }
    let mut left_units = [0u16; 64];
    let mut right_units = [0u16; 64];
    match (
        cook_identifier(left, &mut left_units),
        cook_identifier(right, &mut right_units),
    ) {
        (Some(l), Some(r)) => left_units[..l] == right_units[..r],
        _ => false,
    }
}

/// Whether an identifier's spelling carries an escape. A loop rather than
/// `contains`: the slice search pulls in a memchr the bare targets lack.
fn has_escape(text: &[u8]) -> bool {
    for &unit in text {
        if unit == b'\\' {
            return true;
        }
    }
    false
}

/// Cook an identifier's spelling, escapes and all, into code units.
fn cook_identifier(text: &[u8], out: &mut [u16]) -> Option<usize> {
    let token = Token {
        kind: TokenKind::Identifier,
        start: 0,
        end: u32::try_from(text.len()).ok()?,
        inner_start: 0,
        inner_end: u32::try_from(text.len()).ok()?,
        line_break_before: false,
        escaped: true,
        spells_reserved: false,
        cooked_valid: true,
        number: 0.0,
        code_units: u32::try_from(text.len()).ok()?,
        radix: 10,
        flags: 0,
    };
    crate::lex::cook(text, &token, out)
}

/// Where a binding target binds a name equal to `needle`, if it does.
fn target_declares_name(
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

/// Compile one parsed module into a verified unit image.
///
/// A module differs from a script in three ways: its top level is a scope of
/// its own rather than the global object, it may import names from other
/// modules, and it says which of its own names other modules may have.
pub fn lower_module(
    source: &[u8],
    arena: &Arena<'_>,
    root: u32,
    storage: &mut Storage<'_>,
) -> Result<Compiled, Diagnostic> {
    lower_inner(
        source,
        arena,
        root,
        storage,
        true,
        true,
        &[],
        false,
        false,
        u32::MAX,
        false,
        false,
        false,
        false,
        false,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "one internal seam carries every compilation goal — script, module, and eval with its scope and strictness — and a parameter struct would only rename the arity"
)]
fn lower_inner(
    source: &[u8],
    arena: &Arena<'_>,
    root: u32,
    storage: &mut Storage<'_>,
    verify_image: bool,
    module: bool,
    eval_scope: &[EvalBinding<'_>],
    strict: bool,
    eval_goal: bool,
    var_env_depth: u32,
    eval_super_property: bool,
    eval_super_call: bool,
    eval_new_target: bool,
    eval_deny_arguments: bool,
    eval_privates: bool,
) -> Result<Compiled, Diagnostic> {
    let mut program = Program {
        constants: storage.constants,
        constant_count: 0,
        constant_data: storage.constant_data,
        constant_data_length: 0,
        functions: storage.functions,
        function_count: 0,
        unit_code: storage.unit_code,
        unit_code_length: 0,
        unit_safe_points: storage.unit_safe_points,
        unit_safe_point_count: 0,
        exceptions: storage.exceptions,
        exception_count: 0,
        scopes: storage.scopes,
        scope_count: 0,
        bindings: storage.bindings,
        binding_count: 0,
        pending: storage.pending,
        pending_count: 0,
        pending_next: 0,
        eval_sites: storage.eval_sites,
        eval_site_length: 0,
        eval_site_count: 0,
        eval_scope,
        eval_goal,
        eval_super_property,
        eval_super_call,
        eval_new_target,
        eval_deny_arguments,
        eval_privates,
        template_sites: 0,
        eval_var_statements: (NONE, 0),
        eval_var_env_depth: var_env_depth,
        imports: storage.imports,
        import_count: 0,
        exports: storage.exports,
        export_count: 0,
        module,
        failure: None,
    };

    // The script itself is function zero, so a unit always has an entry.
    let script = program.reserve_function()?;
    emit_function(
        source,
        arena,
        Body::Script(root),
        NONE,
        script,
        strict,
        false,
        false,
        false,
        false,
        false,
        true,
        false,
        false,
        false,
        &mut program,
        storage.code,
        storage.safe_points,
        storage.patches,
        storage.labels,
    )?;

    // Every function a body created is emitted in turn, and a function defined
    // inside one of those joins the same queue, so nesting needs no recursion.
    while program.pending_next < program.pending_count {
        let entry = program.pending[program.pending_next];
        program.pending_next += 1;
        emit_function(
            source,
            arena,
            if entry.node == NONE && entry.class_constructor {
                Body::DefaultConstructor
            } else {
                Body::Function(entry.node)
            },
            entry.scope,
            entry.index,
            entry.strict,
            entry.in_with,
            entry.class_constructor,
            entry.derived_constructor,
            entry.method,
            entry.super_call,
            entry.new_target,
            entry.deny_arguments,
            entry.privates,
            entry.brand,
            &mut program,
            storage.code,
            storage.safe_points,
            storage.patches,
            storage.labels,
        )?;
    }
    if let Some(failure) = program.failure {
        return Err(failure);
    }

    let register_count = program
        .functions
        .first()
        .map_or(0, |function| function.register_count);
    let function_count = u32::try_from(program.function_count).unwrap_or(0);
    let constant_count = program.constant_count;
    let code_length = u32::try_from(program.unit_code_length).unwrap_or(0);

    let functions = program
        .functions
        .get(..program.function_count)
        .ok_or_else(|| failure(code::CODE_TOO_LARGE))?;
    let constants = program
        .constants
        .get(..constant_count)
        .ok_or_else(|| failure(code::TOO_MANY_CONSTANTS))?;
    let constant_data = program
        .constant_data
        .get(..program.constant_data_length)
        .ok_or_else(|| failure(code::TOO_MANY_CONSTANTS))?;
    let code = program
        .unit_code
        .get(..program.unit_code_length)
        .ok_or_else(|| failure(code::CODE_TOO_LARGE))?;
    let exceptions = program
        .exceptions
        .get(..program.exception_count)
        .ok_or_else(|| failure(code::CODE_TOO_LARGE))?;
    let safe_points = program
        .unit_safe_points
        .get(..program.unit_safe_point_count)
        .ok_or_else(|| failure(code::CODE_TOO_LARGE))?;

    let imports = program
        .imports
        .get(..program.import_count)
        .ok_or_else(|| failure(code::CODE_TOO_LARGE))?;
    let exports = program
        .exports
        .get(..program.export_count)
        .ok_or_else(|| failure(code::CODE_TOO_LARGE))?;
    let flags = if program.module { unit_flag::MODULE } else { 0 };
    let eval_sites = if program.eval_site_count == 0 {
        &[]
    } else {
        program
            .eval_sites
            .get(..program.eval_site_length)
            .unwrap_or(&[])
    };

    let length = UnitWriter::new(storage.image)
        .write_module(
            functions,
            constants,
            constant_data,
            code,
            exceptions,
            safe_points,
            0,
            imports,
            exports,
            flags,
            eval_sites,
        )
        .map_err(build_failure)?;

    let image = storage
        .image
        .get(..length)
        .ok_or_else(|| failure(code::CODE_TOO_LARGE))?;
    if verify_image {
        crate::verify::admit(image, storage.verifier_state)?;
    }

    Ok(Compiled {
        length,
        register_count,
        constant_count: u32::try_from(constant_count).unwrap_or(0),
        code_length,
        function_count,
    })
}

/// Compile a script and hand back the image without verifying it, which is for
/// a caller diagnosing the lowering itself.
pub fn lower_script_unverified(
    source: &[u8],
    arena: &Arena<'_>,
    root: u32,
    storage: &mut Storage<'_>,
) -> Result<Compiled, Diagnostic> {
    let mut state = [0i32; 0];
    let _ = &mut state;
    lower_script_inner(source, arena, root, storage, false)
}

/// Compile one parsed expression into a verified unit image.
///
/// This is the script path with a root that is one expression, which is what a
/// caller that parsed an expression rather than a script has.
pub fn lower_expression(
    source: &[u8],
    arena: &Arena<'_>,
    root: u32,
    storage: &mut Storage<'_>,
) -> Result<Compiled, Diagnostic> {
    lower_script(source, arena, root, storage)
}

/// A view over a compiled image, for a caller that wants to read it back.
pub fn unit<'a>(image: &'a [u8], length: usize) -> Option<Unit<'a>> {
    Unit::parse(image.get(..length)?).ok()
}

fn failure(failure: u16) -> Diagnostic {
    Diagnostic::at(failure, Severity::Error, 0)
}

fn build_failure(error: BuildError) -> Diagnostic {
    let failure = match error {
        BuildError::Full => code::CODE_TOO_LARGE,
        BuildError::JumpTooFar => code::JUMP_TOO_FAR,
        BuildError::Label => code::LOWERING_NOT_ADMITTED,
    };
    Diagnostic::at(failure, Severity::Error, 0)
}

/// What is being lowered into one function record.
#[derive(Clone, Copy)]
enum Body {
    /// The whole script, whose value is its last expression statement's.
    Script(u32),
    /// A function node.
    Function(u32),
    /// A class's default constructor, which no node wrote: the base form
    /// initialises fields; the derived form forwards every argument to
    /// `super` first.
    DefaultConstructor,
}

/// State every function of one compilation shares.
struct Program<'a> {
    constants: &'a mut [Constant],
    constant_count: usize,
    constant_data: &'a mut [u8],
    constant_data_length: usize,
    functions: &'a mut [Function],
    function_count: usize,
    unit_code: &'a mut [u8],
    unit_code_length: usize,
    unit_safe_points: &'a mut [u32],
    unit_safe_point_count: usize,
    exceptions: &'a mut [ExceptionRegion],
    exception_count: usize,
    scopes: &'a mut [Scope],
    scope_count: usize,
    bindings: &'a mut [Binding],
    binding_count: usize,
    pending: &'a mut [Pending],
    pending_count: usize,
    pending_next: usize,
    eval_sites: &'a mut [u8],
    eval_site_length: usize,
    eval_site_count: u32,
    /// Bindings the caller's eval-site record says are visible: what this
    /// compilation is an eval inside of.
    eval_scope: &'a [EvalBinding<'a>],
    /// The top-level statements of a sloppy eval unit, whose var-declared
    /// names bind fresh at run time rather than resolving into the caller.
    eval_var_statements: (u32, u32),
    /// The context depth, from the eval unit's own start, of the caller's
    /// variable environment; `u32::MAX` when it is the global object.
    eval_var_env_depth: u32,
    /// Whether the source is the body of a direct eval, whose strict form
    /// keeps its `var` declarations to itself.
    eval_goal: bool,
    /// Whether the eval site admits `super.name` in the source.
    eval_super_property: bool,
    /// Whether the eval site admits `super()`.
    eval_super_call: bool,
    /// Whether the eval site admits `new.target`.
    eval_new_target: bool,
    /// Whether the eval site refuses `arguments`.
    eval_deny_arguments: bool,
    /// Whether the eval site can see a private scope.
    eval_privates: bool,
    /// Tagged-template sites numbered so far, each with a cached object.
    template_sites: u32,
    imports: &'a mut [ImportRecord],
    import_count: usize,
    exports: &'a mut [ExportRecord],
    export_count: usize,
    /// Whether the source is a module.
    module: bool,
    failure: Option<Diagnostic>,
}

impl Program<'_> {
    /// Take the next function index, which a closure can name before the body
    /// behind it exists.
    fn reserve_function(&mut self) -> Result<u32, Diagnostic> {
        let index = self.function_count;
        if index >= self.functions.len() {
            return Err(failure(code::CODE_TOO_LARGE));
        }
        self.function_count += 1;
        Ok(u32::try_from(index).unwrap_or(0))
    }

    #[allow(
        clippy::fn_params_excessive_bools,
        clippy::too_many_arguments,
        reason = "four orthogonal context bits, named at every call site by position"
    )]
    fn queue(
        &mut self,
        node: u32,
        scope: u32,
        index: u32,
        strict: bool,
        in_with: bool,
        class_constructor: bool,
        derived_constructor: bool,
        method: bool,
        super_call: bool,
        new_target: bool,
        deny_arguments: bool,
        privates: bool,
        brand: bool,
    ) -> Result<(), Diagnostic> {
        let slot = self
            .pending
            .get_mut(self.pending_count)
            .ok_or_else(|| failure(code::CODE_TOO_LARGE))?;
        *slot = Pending {
            node,
            scope,
            index,
            strict,
            in_with,
            class_constructor,
            derived_constructor,
            method,
            super_call,
            new_target,
            deny_arguments,
            privates,
            brand,
        };
        self.pending_count += 1;
        Ok(())
    }

    fn open_scope(&mut self, parent: u32) -> Result<u32, Diagnostic> {
        let index = self.scope_count;
        let slot = self
            .scopes
            .get_mut(index)
            .ok_or_else(|| failure(code::TOO_MANY_REGISTERS))?;
        *slot = Scope {
            parent,
            first: u32::try_from(self.binding_count).unwrap_or(0),
            count: 0,
            context: false,
        };
        self.scope_count += 1;
        Ok(u32::try_from(index).unwrap_or(0))
    }

    /// Add a name to a scope. The scope must be the one most recently opened,
    /// because bindings are contiguous.
    fn declare(&mut self, scope: u32, binding: Binding) -> Result<u32, Diagnostic> {
        let slot = self
            .bindings
            .get_mut(self.binding_count)
            .ok_or_else(|| failure(code::TOO_MANY_REGISTERS))?;
        let record = self
            .scopes
            .get_mut(scope as usize)
            .ok_or_else(|| failure(code::TOO_MANY_REGISTERS))?;
        let index = record.count;
        *slot = Binding {
            slot: index,
            ..binding
        };
        record.count += 1;
        record.context = true;
        self.binding_count += 1;
        Ok(index)
    }

    fn scope(&self, index: u32) -> Scope {
        self.scopes
            .get(index as usize)
            .copied()
            .unwrap_or(Scope::EMPTY)
    }
}

/// Where a name was found.
#[derive(Clone, Copy, Debug)]
enum Resolved {
    /// A context slot, `depth` contexts out and at `slot`.
    Slot { depth: u32, slot: u32, kind: u8 },
    /// Not declared anywhere the program can see: the isolate resolves it.
    Global,
}

/// A loop, switch, or labelled statement that `break` and `continue` can leave.
#[derive(Clone, Copy)]
struct Target {
    /// The label's name span, or `(0, 0)` for an unlabelled target.
    label_start: u32,
    label_end: u32,
    /// Whether `break` alone and `continue` alone reach this target.
    breakable: bool,
    continuable: bool,
    break_label: Label,
    continue_label: Label,
    /// Contexts open at the target's own level, so leaving it pops the right
    /// number.
    context_depth: u32,
    /// Finalisers open at the target's own level, which leaving must run.
    finaliser_depth: u32,
    /// Whether anything actually jumps to each label. A label nothing reaches
    /// must not be bound, because the code at it would be unreachable.
    break_used: bool,
    continue_used: bool,
}

/// A `finally` block that any exit from its `try` must run first.
#[derive(Clone, Copy)]
struct Finaliser {
    node: u32,
    context_depth: u32,
    /// How deeply the owning `try` is nested, which decides which exception
    /// regions an inline copy of this finaliser is excluded from.
    try_depth: u32,
    /// The scope the `try` was entered in. An inline copy lowers in it, so a
    /// name resolves at the depth the exit's pops leave behind, not at the
    /// depth of whatever scope the exit was written inside.
    scope: u32,
    /// A `for of` loop's iterator register, closed when an exit escapes the
    /// loop, or `u32::MAX` for an ordinary `finally` node.
    iterator: u32,
    /// The loop's done flag, which says the iterator finished on its own —
    /// or `DISPOSE_STACK`, marking `iterator` as a dispose stack instead.
    flag: u32,
}

/// A finaliser's `flag` value marking its register as a stack of resources
/// to dispose rather than an iterator to close.
const DISPOSE_STACK: u32 = u32::MAX - 1;
/// As `DISPOSE_STACK`, for a stack whose disposals are awaited.
const DISPOSE_STACK_ASYNC: u32 = u32::MAX - 2;

/// Lower one function record: its scope, its prologue, its body, and its exit.
#[allow(
    clippy::too_many_arguments,
    reason = "the per-function buffers are the caller's storage and grouping them would hide which is which"
)]
fn emit_function(
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
fn sort_regions(regions: &mut [ExceptionRegion]) {
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

/// Declare what a module's top level introduces: what it imports, what it
/// declares, and what it exports.
///
/// An import is a binding like any other, except that reading it goes through
/// the module that exports it, and assigning to it is refused.
fn declare_module(
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
fn check_module_duplicates(
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
fn declare_one(
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
fn declare_pattern(
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

fn declare_lexical(
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
fn hoist_vars(
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

fn hoist_statement(
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

/// Cases one switch may hold.
const MAX_CASES: usize = 64;
/// Registers a frame that reads `arguments` reserves, matching the most
/// arguments one call may pass.
const MAX_CALL_ARGUMENTS: u32 = 16;
/// Loops, switches, and labelled statements open at once.
const MAX_TARGETS: usize = 32;
/// `finally` blocks open at once.
const MAX_FINALISERS: usize = 16;
/// Calls one `return` may put in tail position.
const MAX_TAIL_SPANS: usize = 8;

/// Inline finaliser copies one function may hold.
const MAX_HOLES: usize = 48;

impl Target {
    const EMPTY: Self = Self {
        label_start: 0,
        label_end: 0,
        breakable: false,
        continuable: false,
        break_label: Label(0),
        continue_label: Label(0),
        context_depth: 0,
        finaliser_depth: 0,
        break_used: false,
        continue_used: false,
    };
}

impl Finaliser {
    const EMPTY: Self = Self {
        node: NONE,
        context_depth: 0,
        try_depth: 0,
        scope: NONE,
        iterator: u32::MAX,
        flag: u32::MAX,
    };
}

struct Lowering<'a, 'b, 'c, 'p> {
    source: &'a [u8],
    arena: &'a Arena<'b>,
    builder: &'a mut CodeBuilder<'c>,
    program: &'a mut Program<'p>,
    /// The unit index of the function being emitted, which an eval-site
    /// record names.
    function_index: u32,
    /// Registers currently held by enclosing expressions.
    registers: u32,
    /// Highest register count reached, which the function declares.
    high_water: u32,
    /// The scope statements are being lowered in.
    scope: u32,
    /// Contexts open at this point, which the verifier checks against.
    context_depth: u32,
    max_context_depth: u32,
    targets: [Target; MAX_TARGETS],
    target_count: usize,
    finalisers: [Finaliser; MAX_FINALISERS],
    finaliser_count: usize,
    /// How many `try` statements enclose the position being lowered.
    try_depth: u32,
    /// Where a nullish link of the optional chain being lowered jumps: the
    /// chain's root binds it, so `a?.b.c` skips the whole tail at once.
    chain_exit: Option<Label>,
    /// Ranges holding an inline finaliser copy, each tagged with its owning
    /// `try`'s depth: a region belonging to a `try` nested that deep or
    /// deeper must not cover the copy, or a throwing `finally` would run
    /// itself again.
    holes: [(u32, u32, u32); MAX_HOLES],
    hole_count: usize,
    /// Whether `return` is admitted here.
    in_function: bool,
    /// The root scope of the function being lowered, whose context is the
    /// variable environment a direct eval declares into.
    function_scope: u32,
    in_arrow: bool,
    in_parameters: bool,
    /// Unresolved names read and write through the environment chain rather
    /// than straight to the global object: this code, or an eval it can
    /// trigger, may create bindings at run time.
    dynamic_names: bool,
    /// The name an anonymous class expression takes under named
    /// evaluation, for the class lowering to apply before static fields.
    pending_class_name: Option<u32>,
    /// The register holding the dispose stack of the innermost statement
    /// list with a `using` declaration, or `u32::MAX` outside one.
    dispose_stack: u32,
    /// How many `with` statements enclose the position: their objects can
    /// shadow anything an outer scope binds.
    with_depth: u32,
    /// Whether the function is an async generator, whose `yield*` walks the
    /// async iteration protocol.
    in_async_generator: bool,
    /// Whether a call in tail position of a `return` may give up this
    /// frame: strict code in a plain function, as the specification's
    /// proper tail calls require.
    tail_calls: bool,
    /// The spans of the call nodes in tail position of the `return`
    /// being lowered, which `call` emits as `TailCall`.
    tail_spans: [(u32, u32); MAX_TAIL_SPANS],
    tail_span_count: usize,
    /// Whether `super.name` is admitted here, which a direct eval inherits.
    allow_super_property: bool,
    /// Whether `super()` is admitted here.
    allow_super_call: bool,
    /// Whether `new.target` is admitted here.
    allow_new_target: bool,
    /// Whether `arguments` is refused here: field-initialiser code.
    deny_arguments: bool,
    /// Whether a private scope is visible here.
    privates_visible: bool,
    /// Whether this constructor stamps instances with the private brand.
    brand_instances: bool,
    /// Whether this body is strict code, which the functions written inside
    /// it inherit.
    strict: bool,
    /// The register holding a script's completion value.
    completion: u32,
    /// The slot a module's default export is held in, when it has one.
    default_export_slot: u32,
}

impl Lowering<'_, '_, '_, '_> {
    fn fail(&mut self, node: &Node, failure: u16) {
        if self.program.failure.is_none() {
            self.program.failure = Some(Diagnostic::new(
                failure,
                Severity::Error,
                node.start,
                node.end.saturating_sub(node.start),
            ));
        }
    }

    fn node(&self, index: u32) -> Node {
        match self.arena.node(index) {
            Some(node) => *node,
            None => Node::new(NodeKind::Null, 0, 0),
        }
    }

    /// Take the next free register.
    fn allocate(&mut self) -> u32 {
        let register = self.registers;
        self.registers += 1;
        if self.registers > self.high_water {
            self.high_water = self.registers;
        }
        register
    }

    /// Release every register taken since `mark`.
    fn release(&mut self, mark: u32) {
        self.registers = mark;
    }

    fn emit(&mut self, opcode: Opcode, operands: &[i64]) {
        self.builder.emit(opcode, operands);
    }

    fn push_context(&mut self, slots: u32) {
        self.emit(Opcode::PushContext, &[i64::from(slots)]);
        self.context_depth += 1;
        if self.context_depth > self.max_context_depth {
            self.max_context_depth = self.context_depth;
        }
    }

    fn pop_context(&mut self) {
        // A block whose last statement returned, threw, or jumped out has
        // already left its context behind: the frame drops what it pushed. The
        // pop would be an instruction nothing can reach, which the verifier
        // refuses — rightly, since it cannot tell that one apart from a
        // statement written after a `return`.
        if !self.builder.terminated() {
            self.emit(Opcode::PopContext, &[]);
        }
        self.context_depth = self.context_depth.saturating_sub(1);
    }

    /// A context this function reads through, which its record must declare so
    /// the verifier can check every depth operand against it.
    fn note_depth(&mut self, depth: u32) {
        let reach = depth.saturating_add(1);
        if reach > self.max_context_depth {
            self.max_context_depth = reach;
        }
    }

    /// Where a name written in the current scope resolves to.
    fn resolve(&self, start: u32, end: u32) -> Resolved {
        let text = self.span(start, end);
        let mut scope = self.scope;
        let mut depth = 0u32;
        while scope != NONE {
            let record = self.program.scope(scope);
            let first = record.first as usize;
            let mut index = 0u32;
            while index < record.count {
                let Some(binding) = self.program.bindings.get(first + index as usize) else {
                    break;
                };
                if self.same_name(self.span(binding.start, binding.end), text) {
                    if record.parent == NONE
                        && !self.program.eval_goal
                        && !self.program.module
                        && matches!(binding.kind, binding_kind::LET | binding_kind::CONST)
                    {
                        // A script's top-level lexical lives in the global
                        // lexical environment, reached by name, so every
                        // script — and an indirect eval — sees the same one.
                        return Resolved::Global;
                    }
                    return Resolved::Slot {
                        depth,
                        slot: binding.slot,
                        kind: binding.kind,
                    };
                }
                index += 1;
            }
            if record.context {
                depth += 1;
            }
            scope = record.parent;
        }
        // A name this eval's own code `var`-declares binds fresh in the
        // caller's variable environment at run time: it must not resolve to
        // the older binding the site could see.
        let (eval_list, eval_length) = self.program.eval_var_statements;
        if eval_length != 0
            && eval_declared_name(self.arena, self.source, eval_list, eval_length, text, false)
                .is_some()
        {
            // Unless the variable environment already binds the name — then
            // the declaration re-uses that binding rather than making one.
            let existing = self.program.eval_scope.iter().any(|binding| {
                binding.name == text && binding.depth == self.program.eval_var_env_depth
            });
            if !existing {
                return Resolved::Global;
            }
        }
        // A name the eval site's record says is visible resolves into the
        // caller's environments: its depth is counted from the call site's
        // innermost context, which is exactly where this code's own chain
        // ran out.
        for binding in self.program.eval_scope {
            if binding.name == text {
                return Resolved::Slot {
                    depth: depth + binding.depth,
                    slot: binding.slot,
                    kind: u8::try_from(binding.kind).unwrap_or(binding_kind::VARIABLE),
                };
            }
        }
        Resolved::Global
    }

    /// Load a name, wherever it was declared.
    /// Strict code may not use a strict-mode reserved word as a name at
    /// all: the reference is the early SyntaxError the declaration is.
    fn strict_reserved_guard(&mut self, node: &Node) -> bool {
        if !self.strict {
            return false;
        }
        let span = self.span(node.first, node.second);
        let reserved = matches!(
            span,
            b"implements"
                | b"interface"
                | b"let"
                | b"package"
                | b"private"
                | b"protected"
                | b"public"
                | b"static"
                | b"yield"
        );
        if reserved {
            self.fail(node, code::SYNTAX_NOT_ADMITTED);
        }
        reserved
    }

    fn load_name(&mut self, node: &Node) {
        // A field initialiser — and any eval its site admits — may not
        // reference `arguments`: the early error the specification makes it.
        if self.deny_arguments && self.span(node.first, node.second) == b"arguments" {
            self.fail(node, code::SYNTAX_NOT_ADMITTED);
            return;
        }
        if self.strict_reserved_guard(node) {
            return;
        }
        match self.resolve(node.first, node.second) {
            Resolved::Slot {
                slot,
                kind: binding_kind::IMPORT,
                ..
            } => {
                // An import loads by its record's index in the module's
                // import table, which bare imports and re-exports share.
                let index = self.import_record_index(slot);
                self.emit(Opcode::LdaImport, &[i64::from(index)]);
            }
            Resolved::Slot { depth, slot, kind }
                if kind == binding_kind::VARIABLE
                    && depth >= 1
                    && (self.dynamic_names || self.with_depth > 0) =>
            {
                // A direct eval between here and the slot can declare a
                // nearer binding of this name at run time, which then wins.
                let constant = self.identifier_constant(node);
                self.note_depth(depth);
                self.emit(
                    Opcode::LdaShadowable,
                    &[i64::from(constant), i64::from(slot), i64::from(depth)],
                );
            }
            Resolved::Slot { depth, slot, .. } => {
                self.note_depth(depth);
                self.emit(Opcode::LdaContextSlot, &[i64::from(slot), i64::from(depth)]);
            }
            Resolved::Global => {
                let constant = self.identifier_constant(node);
                let opcode = if self.dynamic_names || self.with_depth > 0 {
                    Opcode::LdaDynamic
                } else {
                    Opcode::LdaGlobal
                };
                self.emit(opcode, &[i64::from(constant)]);
            }
        }
    }

    /// The position of an import binding among the module's imports: the
    /// import table is in declaration order, as the scope's bindings are.
    fn import_index_of(&mut self, slot: u32) -> u32 {
        // Imports live only in the module's own top scope; a read from a
        // closure deep inside still loads by that scope's ordering.
        let mut scope = self.function_scope;
        loop {
            let parent = self.program.scope(scope).parent;
            if parent == NONE {
                break;
            }
            scope = parent;
        }
        let record = self.program.scope(scope);
        let first = record.first as usize;
        let mut index = 0u32;
        let mut counted = 0u32;
        while index < record.count {
            if let Some(binding) = self.program.bindings.get(first + index as usize) {
                if binding.kind == binding_kind::IMPORT {
                    if binding.slot == slot {
                        return counted;
                    }
                    counted += 1;
                }
            }
            index += 1;
        }
        counted
    }

    /// The index of an import binding's record in the module's import table.
    /// The table also holds records no binding names — bare imports and
    /// re-exports, whose slot stays unwritten — so the binding's position
    /// among its kind is mapped over the records that do carry one.
    fn import_record_index(&mut self, slot: u32) -> u32 {
        let ordinal = self.import_index_of(slot);
        let mut seen = 0u32;
        let mut index = 0usize;
        while index < self.program.import_count {
            if let Some(record) = self.program.imports.get(index) {
                if record.slot != u32::MAX {
                    if seen == ordinal {
                        return u32::try_from(index).unwrap_or(ordinal);
                    }
                    seen += 1;
                }
            }
            index += 1;
        }
        ordinal
    }

    /// Store the accumulator into a name. Assigning to a `const` is refused
    /// here, because a program that does it can never be right.
    fn store_name(&mut self, node: &Node) {
        // Strict code refuses to assign the names `eval` and `arguments`,
        // whatever they resolve to.
        if self.strict && matches!(self.span(node.first, node.second), b"eval" | b"arguments") {
            self.fail(node, code::STRICT_ASSIGNMENT_TO_RESTRICTED_NAME);
            return;
        }
        if self.strict_reserved_guard(node) {
            return;
        }
        match self.resolve(node.first, node.second) {
            Resolved::Slot { depth, slot, kind } => {
                if kind == binding_kind::IMPORT {
                    // An imported name belongs to the module that exports
                    // it: the write parses, evaluates its value, and throws
                    // the TypeError at run time.
                    self.emit(Opcode::ThrowSelfAssignment, &[]);
                    return;
                }
                if kind == binding_kind::CONST {
                    // Assigning a constant evaluates its value and then
                    // throws the TypeError, at run time.
                    self.emit(Opcode::ThrowSelfAssignment, &[]);
                    return;
                }
                if kind == binding_kind::SELF {
                    // A named function expression's own name is immutable:
                    // strict code refuses the write, sloppy discards it.
                    if self.strict {
                        self.emit(Opcode::ThrowSelfAssignment, &[]);
                    }
                    return;
                }
                self.note_depth(depth);
                if kind == binding_kind::VARIABLE
                    && depth >= 1
                    && (self.dynamic_names || self.with_depth > 0)
                {
                    let constant = self.identifier_constant(node);
                    self.emit(
                        Opcode::StaShadowable,
                        &[i64::from(constant), i64::from(slot), i64::from(depth)],
                    );
                } else {
                    self.emit(Opcode::StaContextSlot, &[i64::from(slot), i64::from(depth)]);
                }
            }
            Resolved::Global => {
                let constant = self.identifier_constant(node);
                // Strict code assigns only what exists; sloppy code creates.
                let opcode = if self.strict {
                    Opcode::StaGlobalStrict
                } else if self.dynamic_names || self.with_depth > 0 {
                    Opcode::StaDynamic
                } else {
                    Opcode::StaGlobal
                };
                self.emit(opcode, &[i64::from(constant)]);
            }
        }
    }

    /// Give a declared name its first value.
    fn initialise_name(&mut self, node: &Node) {
        match self.resolve(node.first, node.second) {
            Resolved::Slot { depth, slot, .. } => {
                self.note_depth(depth);
                self.emit(
                    Opcode::InitContextSlot,
                    &[i64::from(slot), i64::from(depth)],
                );
            }
            Resolved::Global => {
                let constant = self.identifier_constant(node);
                if !self.program.eval_goal && !self.program.module {
                    // A script's top-level lexical: its global binding takes
                    // the value and leaves its dead zone.
                    self.emit(Opcode::InitGlobalLexical, &[i64::from(constant)]);
                    return;
                }
                let opcode = if self.dynamic_names || self.with_depth > 0 {
                    Opcode::StaDynamic
                } else {
                    Opcode::StaGlobal
                };
                self.emit(opcode, &[i64::from(constant)]);
            }
        }
    }

    // Statements.

    /// The constant holding the name `default`, which is what a default export
    /// is bound to and a default import asks for.
    fn default_key_constant(&mut self) -> u32 {
        let offset = self.program.constant_data_length;
        let text = [
            b'd', 0, b'e', 0, b'f', 0, b'a', 0, b'u', 0, b'l', 0, b't', 0,
        ];
        let Some(space) = self
            .program
            .constant_data
            .get_mut(offset..offset + text.len())
        else {
            let node = Node::new(NodeKind::Null, 0, 0);
            self.fail(&node, code::TOO_MANY_CONSTANTS);
            return 0;
        };
        space.copy_from_slice(&text);
        if let Some(existing) = self.find_text(ConstantKind::Key, offset, text.len()) {
            return existing;
        }
        self.program.constant_data_length += text.len();
        self.intern(Constant {
            kind: ConstantKind::Key,
            first: u32::try_from(offset).unwrap_or(0),
            second: 7,
        })
    }

    /// The slot a default export's value is held in.
    fn default_slot(&mut self) -> u32 {
        // The default export's binding is the one with no name: no
        // identifier can spell an empty span, so it is unmistakable.
        let record = self.program.scope(self.function_scope);
        let first = record.first as usize;
        let count = record.count;
        let mut index = 0u32;
        while index < count {
            if let Some(binding) = self.program.bindings.get(first + index as usize) {
                if binding.start == binding.end && binding.kind == binding_kind::LET {
                    return binding.slot;
                }
            }
            index += 1;
        }
        self.default_export_slot
    }

    /// The constant holding the name `length`, which the loops that walk an
    /// array-like need.
    fn length_key_constant(&mut self) -> u32 {
        self.text_key_constant(b"length")
    }

    /// A key constant for a name the lowering itself needs, staged as the
    /// UTF-16 the constant table holds.
    /// A key constant from UTF-16 units directly.
    fn unit_text_constant(&mut self, units: &[u16]) -> u32 {
        let offset = self.program.constant_data_length;
        let length = units.len() * 2;
        let Some(space) = self.program.constant_data.get_mut(offset..offset + length) else {
            let node = Node::new(NodeKind::Null, 0, 0);
            self.fail(&node, code::TOO_MANY_CONSTANTS);
            return 0;
        };
        for (index, &unit) in units.iter().enumerate() {
            let bytes = unit.to_le_bytes();
            space[index * 2] = bytes[0];
            space[index * 2 + 1] = bytes[1];
        }
        if let Some(existing) = self.find_text(ConstantKind::Key, offset, length) {
            return existing;
        }
        self.program.constant_data_length += length;
        self.intern(Constant {
            kind: ConstantKind::Key,
            first: u32::try_from(offset).unwrap_or(0),
            second: u32::try_from(units.len()).unwrap_or(0),
        })
    }

    fn text_key_constant(&mut self, name: &[u8]) -> u32 {
        let offset = self.program.constant_data_length;
        let length = name.len() * 2;
        let Some(space) = self.program.constant_data.get_mut(offset..offset + length) else {
            let node = Node::new(NodeKind::Null, 0, 0);
            self.fail(&node, code::TOO_MANY_CONSTANTS);
            return 0;
        };
        for (index, &byte) in name.iter().enumerate() {
            space[index * 2] = byte;
            space[index * 2 + 1] = 0;
        }
        if let Some(existing) = self.find_text(ConstantKind::Key, offset, length) {
            return existing;
        }
        self.program.constant_data_length += length;
        self.intern(Constant {
            kind: ConstantKind::Key,
            first: u32::try_from(offset).unwrap_or(0),
            second: u32::try_from(name.len()).unwrap_or(0),
        })
    }

    /// Queue a class constructor's body: a function whose prologue also
    /// runs the class's instance fields.
    /// Initialise the running constructor's instance fields on `this`: each
    /// stored initialiser runs as an ordinary interpreter call — which is
    /// what lets a field's own direct eval pause for the compiler — and the
    /// value lands through DefineField.
    fn emit_init_fields(&mut self, brand: bool) {
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

    fn queue_class_constructor(
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
    fn queue_function(&mut self, index: u32) -> u32 {
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
    fn queue_method(&mut self, index: u32, privates: bool) -> u32 {
        self.queue_callable(index, true, false, true, false, privates)
    }

    /// Queue a field initialiser: home-carrying code that may not
    /// reference `arguments`.
    fn queue_field_initialiser(&mut self, index: u32, privates: bool) -> u32 {
        self.queue_callable(index, true, false, true, true, privates)
    }

    fn queue_callable(
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
    // Constant interning. Equal constants share a slot, so the table stays
    // small and identical sources produce identical images.

    fn intern(&mut self, constant: Constant) -> u32 {
        let mut index = 0usize;
        while index < self.program.constant_count {
            let existing = self.program.constants[index];
            if existing.kind as u8 == constant.kind as u8
                && existing.first == constant.first
                && existing.second == constant.second
            {
                return u32::try_from(index).unwrap_or(0);
            }
            index += 1;
        }
        match self.program.constants.get_mut(self.program.constant_count) {
            Some(slot) => {
                *slot = constant;
                let index = u32::try_from(self.program.constant_count).unwrap_or(0);
                self.program.constant_count += 1;
                index
            }
            None => {
                if self.program.failure.is_none() {
                    self.program.failure = Some(failure(code::TOO_MANY_CONSTANTS));
                }
                0
            }
        }
    }

    fn number_constant(&mut self, value: f64) -> u32 {
        self.intern(Constant::number(value))
    }

    /// Intern the cooked text of a literal or name span as UTF-16 data.
    fn text_constant(&mut self, node: &Node, kind: ConstantKind, token_kind: TokenKind) -> u32 {
        let token = Token {
            kind: token_kind,
            start: node.start,
            end: node.end,
            inner_start: node.first,
            inner_end: node.second,
            line_break_before: false,
            escaped: false,
            spells_reserved: false,
            cooked_valid: true,
            number: 0.0,
            code_units: node.end.saturating_sub(node.start),
            radix: 10,
            flags: 0,
        };
        let offset = self.program.constant_data_length;
        let Some(space) = self.program.constant_data.get_mut(offset..) else {
            self.fail(node, code::TOO_MANY_CONSTANTS);
            return 0;
        };
        // Cook straight into the spare data area as UTF-16, two bytes per
        // code unit: a literal is as long as the data area has room for.
        let mut sink = crate::lex::LittleEndianUnits(space);
        let Some(written) = crate::lex::cook_into(self.source, &token, &mut sink) else {
            self.fail(node, code::TOO_MANY_CONSTANTS);
            return 0;
        };
        // Reuse an identical constant rather than storing its text twice, so
        // the same source always produces the same table.
        if let Some(existing) = self.find_text(kind, offset, written * 2) {
            return existing;
        }
        self.program.constant_data_length += written * 2;
        self.intern(Constant {
            kind,
            first: u32::try_from(offset).unwrap_or(0),
            second: u32::try_from(written).unwrap_or(0),
        })
    }

    /// A module specifier's constant with the marker its import attributes
    /// chose appended after a 0x01 unit: the loader stages `type: 'json'`,
    /// `'text'`, and `'bytes'` variants under exactly that spelling, and an
    /// attribute nothing supports spells a name that links to nothing.
    /// A module specifier's constant: plain, or spelled with its import
    /// attributes' marker.
    fn specifier_constant(&mut self, node: &Node, attributes: u8) -> u32 {
        match attributes {
            0 => self.text_constant(node, ConstantKind::String, TokenKind::String),
            1 => self.attributed_specifier_constant(node, b'j'),
            2 => self.attributed_specifier_constant(node, b't'),
            3 => self.attributed_specifier_constant(node, b'b'),
            _ => self.attributed_specifier_constant(node, b'?'),
        }
    }

    fn attributed_specifier_constant(&mut self, node: &Node, marker: u8) -> u32 {
        let token = Token {
            kind: TokenKind::String,
            start: node.start,
            end: node.end,
            inner_start: node.first,
            inner_end: node.second,
            line_break_before: false,
            escaped: false,
            spells_reserved: false,
            cooked_valid: true,
            number: 0.0,
            code_units: node.end.saturating_sub(node.start),
            radix: 10,
            flags: 0,
        };
        let offset = self.program.constant_data_length;
        let Some(space) = self.program.constant_data.get_mut(offset..) else {
            self.fail(node, code::TOO_MANY_CONSTANTS);
            return 0;
        };
        let mut sink = crate::lex::LittleEndianUnits(space);
        let Some(written) = crate::lex::cook_into(self.source, &token, &mut sink) else {
            self.fail(node, code::TOO_MANY_CONSTANTS);
            return 0;
        };
        let tail = written * 2;
        let Some(space) = self
            .program
            .constant_data
            .get_mut(offset + tail..offset + tail + 4)
        else {
            self.fail(node, code::TOO_MANY_CONSTANTS);
            return 0;
        };
        space[0] = 1;
        space[1] = 0;
        space[2] = marker;
        space[3] = 0;
        let written = written + 2;
        if let Some(existing) = self.find_text(ConstantKind::String, offset, written * 2) {
            return existing;
        }
        self.program.constant_data_length += written * 2;
        self.intern(Constant {
            kind: ConstantKind::String,
            first: u32::try_from(offset).unwrap_or(0),
            second: u32::try_from(written).unwrap_or(0),
        })
    }

    /// The hidden name an `accessor` field stores behind: NUL, `acc `, and
    /// the key's own text — the NUL keeps it off every reflective surface,
    /// and the getter and setter derive the same name from the key.
    fn accessor_backing_constant(&mut self, index: u32) -> u32 {
        let node = self.node(index);
        let token_kind = match node.kind {
            NodeKind::PropertyName => match node.third {
                property_key::STRING => TokenKind::String,
                property_key::NUMBER => {
                    self.fail(&node, code::LOWERING_NOT_ADMITTED);
                    return 0;
                }
                _ => TokenKind::Identifier,
            },
            NodeKind::Identifier => TokenKind::Identifier,
            _ => {
                self.fail(&node, code::LOWERING_NOT_ADMITTED);
                return 0;
            }
        };
        let token = Token {
            kind: token_kind,
            start: node.start,
            end: node.end,
            inner_start: node.first,
            inner_end: node.second,
            line_break_before: false,
            escaped: false,
            spells_reserved: false,
            cooked_valid: true,
            number: 0.0,
            code_units: node.end.saturating_sub(node.start),
            radix: 10,
            flags: 0,
        };
        let offset = self.program.constant_data_length;
        let Some(space) = self.program.constant_data.get_mut(offset..) else {
            self.fail(&node, code::TOO_MANY_CONSTANTS);
            return 0;
        };
        if space.len() < 10 {
            self.fail(&node, code::TOO_MANY_CONSTANTS);
            return 0;
        }
        let prefix = [
            0u16,
            u16::from(b'a'),
            u16::from(b'c'),
            u16::from(b'c'),
            u16::from(b' '),
        ];
        let mut at = 0usize;
        for &unit in &prefix {
            space[at..at + 2].copy_from_slice(&unit.to_le_bytes());
            at += 2;
        }
        let mut sink = crate::lex::LittleEndianUnits(&mut space[10..]);
        let Some(written) = crate::lex::cook_into(self.source, &token, &mut sink) else {
            self.fail(&node, code::TOO_MANY_CONSTANTS);
            return 0;
        };
        let total = written + 5;
        if let Some(existing) = self.find_text(ConstantKind::Key, offset, total * 2) {
            return existing;
        }
        self.program.constant_data_length += total * 2;
        self.intern(Constant {
            kind: ConstantKind::Key,
            first: u32::try_from(offset).unwrap_or(0),
            second: u32::try_from(total).unwrap_or(0),
        })
    }

    /// The index of a constant whose data equals the `length` bytes just
    /// written at `offset`, if the table already holds one.
    fn find_text(&self, kind: ConstantKind, offset: usize, length: usize) -> Option<u32> {
        let mut index = 0usize;
        while index < self.program.constant_count {
            let existing = self.program.constants[index];
            if existing.kind as u8 == kind as u8 {
                let start = existing.first as usize;
                let existing_length = match kind {
                    ConstantKind::BigInt => existing.second as usize,
                    _ => existing.second as usize * 2,
                };
                if existing_length == length {
                    let left = self.program.constant_data.get(start..start + length);
                    let right = self.program.constant_data.get(offset..offset + length);
                    if left.is_some() && left == right {
                        return u32::try_from(index).ok();
                    }
                }
            }
            index += 1;
        }
        None
    }

    /// A property key constant from an identifier-like name node.
    /// A private member access in eval code is admissible only where the
    /// call site could see a private scope; anywhere else it is the early
    /// SyntaxError, before anything runs.
    fn private_member_guard(&mut self, index: u32) {
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

    fn key_constant(&mut self, index: u32) -> u32 {
        let node = self.node(index);
        match node.kind {
            NodeKind::PropertyName => match node.third {
                property_key::STRING => {
                    self.text_constant(&node, ConstantKind::Key, TokenKind::String)
                }
                property_key::NUMBER => {
                    // A numeric key is its Number value's canonical text, which
                    // the isolate produces; the constant carries the value the
                    // lexer read, whatever form the source wrote it in.
                    let value = self.arena.number(node.first);
                    self.number_constant(value)
                }
                _ => self.text_constant(&node, ConstantKind::Key, TokenKind::Identifier),
            },
            NodeKind::Identifier => {
                self.text_constant(&node, ConstantKind::Key, TokenKind::Identifier)
            }
            // An export or import name can be any string: the key is its
            // cooked text, exactly as a quoted property name's would be.
            NodeKind::String => self.text_constant(&node, ConstantKind::Key, TokenKind::String),
            _ => {
                self.fail(&node, code::LOWERING_NOT_ADMITTED);
                0
            }
        }
    }

    /// Whether two identifier spellings name the same binding: equal text,
    /// or — where either carries a Unicode escape — equal once cooked.
    fn same_name(&self, left: &[u8], right: &[u8]) -> bool {
        same_name(left, right)
    }

    fn span(&self, start: u32, end: u32) -> &[u8] {
        if end <= start {
            return &[];
        }
        match self.source.get(start as usize..end as usize) {
            Some(slice) => slice,
            None => &[],
        }
    }

    /// Lower `index`, leaving its value in the accumulator.
    fn expression(&mut self, index: u32) {
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

    /// A pattern constant: the flags in one code unit, then the pattern.
    fn regexp_constant(&mut self, node: &Node) -> u32 {
        let offset = self.program.constant_data_length;
        let flags = u16::try_from(node.third).unwrap_or(0).to_le_bytes();
        match self.program.constant_data.get_mut(offset..offset + 2) {
            Some(slot) => {
                slot[0] = flags[0];
                slot[1] = flags[1];
            }
            None => {
                self.fail(node, code::TOO_MANY_CONSTANTS);
                return 0;
            }
        }
        // The pattern is source text, and its own escapes belong to the pattern
        // grammar rather than to the string grammar, so it is copied as it was
        // written — decoded from the transport's UTF-8 into the code units
        // JavaScript strings are made of, a pair for anything beyond one.
        let mut units = [0u16; 512];
        let mut count = 0usize;
        let text = self.span(node.first, node.second);
        let mut at = 0usize;
        while at < text.len() {
            let byte = text.get(at).copied().unwrap_or(0);
            let tail =
                |offset: usize| u32::from(text.get(at + offset).copied().unwrap_or(0) & 0x3F);
            let (scalar, width) = if byte < 0x80 {
                (u32::from(byte), 1)
            } else if byte & 0xE0 == 0xC0 && at + 1 < text.len() {
                ((u32::from(byte & 0x1F) << 6) | tail(1), 2)
            } else if byte & 0xF0 == 0xE0 && at + 2 < text.len() {
                ((u32::from(byte & 0x0F) << 12) | (tail(1) << 6) | tail(2), 3)
            } else if byte & 0xF8 == 0xF0 && at + 3 < text.len() {
                (
                    (u32::from(byte & 0x07) << 18) | (tail(1) << 12) | (tail(2) << 6) | tail(3),
                    4,
                )
            } else {
                (u32::from(byte), 1)
            };
            at += width;
            let needed = if scalar > 0xFFFF { 2 } else { 1 };
            if count + needed > units.len() {
                self.fail(node, code::TOO_MANY_CONSTANTS);
                return 0;
            }
            if scalar > 0xFFFF {
                let value = scalar - 0x1_0000;
                if let Some(slot) = units.get_mut(count) {
                    *slot = 0xD800 | u16::try_from(value >> 10).unwrap_or(0);
                }
                if let Some(slot) = units.get_mut(count + 1) {
                    *slot = 0xDC00 | u16::try_from(value & 0x3FF).unwrap_or(0);
                }
                count += 2;
            } else {
                if let Some(slot) = units.get_mut(count) {
                    *slot = u16::try_from(scalar).unwrap_or(0);
                }
                count += 1;
            }
        }
        let mut length = 2usize;
        let mut index = 0usize;
        while index < count {
            let bytes = units[index].to_le_bytes();
            match self
                .program
                .constant_data
                .get_mut(offset + length..offset + length + 2)
            {
                Some(slot) => {
                    slot[0] = bytes[0];
                    slot[1] = bytes[1];
                    length += 2;
                }
                None => {
                    self.fail(node, code::TOO_MANY_CONSTANTS);
                    return 0;
                }
            }
            index += 1;
        }
        if let Some(existing) = self.find_text(ConstantKind::RegExp, offset, length) {
            return existing;
        }
        self.program.constant_data_length += length;
        self.intern(Constant {
            kind: ConstantKind::RegExp,
            first: u32::try_from(offset).unwrap_or(0),
            second: u32::try_from(length / 2).unwrap_or(0),
        })
    }

    fn bigint_constant(&mut self, node: &Node) -> u32 {
        // The digits are copied through a fixed buffer so the source borrow
        // ends before the constant data is written. The radix prefix is kept:
        // it is part of what the literal denotes.
        let mut digits = [0u8; 1024];
        let mut digit_count = 0usize;
        for &byte in self.span(node.first, node.second) {
            if byte == b'_' {
                continue;
            }
            match digits.get_mut(digit_count) {
                Some(slot) => {
                    *slot = byte;
                    digit_count += 1;
                }
                None => {
                    self.fail(node, code::TOO_MANY_CONSTANTS);
                    return 0;
                }
            }
        }
        // The radix goes in front of the digits: what a literal denotes is the
        // digits read in the radix it was written in, and the constant must
        // carry both.
        let offset = self.program.constant_data_length;
        match self.program.constant_data.get_mut(offset) {
            Some(slot) => *slot = u8::try_from(node.third).unwrap_or(10),
            None => {
                self.fail(node, code::TOO_MANY_CONSTANTS);
                return 0;
            }
        }
        let mut length = 1usize;
        while length <= digit_count {
            let byte = digits[length - 1];
            match self.program.constant_data.get_mut(offset + length) {
                Some(slot) => {
                    *slot = byte;
                    length += 1;
                }
                None => {
                    self.fail(node, code::TOO_MANY_CONSTANTS);
                    return 0;
                }
            }
        }
        if let Some(existing) = self.find_text(ConstantKind::BigInt, offset, length) {
            return existing;
        }
        self.program.constant_data_length += length;
        self.intern(Constant {
            kind: ConstantKind::BigInt,
            first: u32::try_from(offset).unwrap_or(0),
            second: u32::try_from(length).unwrap_or(0),
        })
    }

    fn load_number(&mut self, value: f64) {
        // Small integers encode as an immediate; everything else is a constant.
        // The test is made on the bits, so it holds on targets with no
        // floating-point instructions, and negative zero stays a constant
        // because it is not the same value as zero.
        if value.to_bits() == 0 {
            self.emit(Opcode::LdaZero, &[]);
            return;
        }
        if let Some(integer) = crate::numeric::exact_i32(value) {
            if integer != 0 {
                self.emit(Opcode::LdaSmi, &[i64::from(integer)]);
                return;
            }
        }
        let constant = self.number_constant(value);
        self.emit(Opcode::LdaConstant, &[i64::from(constant)]);
    }

    /// A tagged template: the tag called with the strings array — carrying
    /// its `raw` counterpart — and the substitution values.
    fn tagged_template(&mut self, node: &Node) {
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

    fn template(&mut self, node: &Node) {
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

    fn array(&mut self, node: &Node) {
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

    fn object(&mut self, node: &Node) {
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
    fn enter_chain(&mut self, node: &Node) -> Option<Option<Label>> {
        if !node.has(flag::CHAIN_ROOT) {
            return None;
        }
        let saved = self.chain_exit;
        self.chain_exit = Some(self.builder.label());
        Some(saved)
    }

    /// Close a chain at its root: the normal path jumps over the landing,
    /// and the landing answers `undefined` for the whole chain.
    fn leave_chain(&mut self, saved: Option<Label>) {
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
    fn chain_link(&mut self) {
        if let Some(exit) = self.chain_exit {
            self.builder.jump(Opcode::JumpIfNullish, exit);
        }
    }

    fn load_member(&mut self, index: u32, node: &Node) {
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

    fn call(&mut self, node: &Node) {
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
    fn in_tail_position(&self, node: &Node) -> bool {
        self.tail_spans
            .get(..self.tail_span_count)
            .unwrap_or(&[])
            .contains(&(node.start, node.end))
    }

    /// Mark the calls in tail position of a returned expression: the
    /// expression itself, or — through a conditional, a comma, or a logical
    /// operator — the operands that can be its value.
    fn mark_tail_positions(&mut self, index: u32) {
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
    fn is_direct_eval_callee(&mut self, callee: &Node) -> bool {
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
    fn record_eval_site(&mut self) {
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

    fn construct(&mut self, node: &Node) {
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

    fn unary(&mut self, node: &Node) {
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

    fn delete(&mut self, node: &Node) {
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

    fn update(&mut self, _index: u32, node: &Node) {
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
    fn store(&mut self, target: &Node, index: u32) {
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

    fn identifier_constant(&mut self, node: &Node) -> u32 {
        self.text_constant(node, ConstantKind::Key, TokenKind::Identifier)
    }

    fn binary(&mut self, node: &Node) {
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

    fn logical(&mut self, node: &Node) {
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

    fn conditional(&mut self, node: &Node) {
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

    fn assign(&mut self, node: &Node) {
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
    fn named_assignment(&mut self, value_node: u32, target: &Node) {
        if matches!(target.kind, NodeKind::Identifier) && !target.has(flag::PARENTHESISED) {
            self.named_expression(value_node, target);
        } else {
            self.expression(value_node);
        }
    }

    /// Name the closure the accumulator holds, when the expression that just
    /// produced it was an anonymous function: what the specification calls
    /// named evaluation.
    fn name_closure(&mut self, value_node: u32, name_node: &Node) {
        if !self.is_anonymous_function(value_node) {
            return;
        }
        let constant = self.identifier_constant(name_node);
        self.emit(Opcode::NameClosure, &[i64::from(constant)]);
    }

    /// Lower an expression that named evaluation applies to: an anonymous
    /// class takes the name before its static initialisers run, which may
    /// read it, and a function takes it after.
    fn named_expression(&mut self, value_node: u32, name_node: &Node) {
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
    fn is_anonymous_function(&self, value_node: u32) -> bool {
        let value = self.node(value_node);
        matches!(value.kind, NodeKind::Function | NodeKind::Class) && value.first == NONE
    }

    /// Evaluate a target's base and key once, into registers a read and a
    /// write both use. A plain name needs no registers at all.
    /// Whether a name resolves to a slot a run-time eval binding could
    /// shadow, and the pieces the prepared-reference instructions need.
    fn shadowable_slot(&mut self, node: &Node) -> Option<(u32, u32, u32)> {
        if !self.dynamic_names && self.with_depth == 0 {
            return None;
        }
        match self.resolve(node.first, node.second) {
            Resolved::Slot { depth, slot, kind }
                if kind == binding_kind::VARIABLE && depth >= 1 =>
            {
                let constant = self.identifier_constant(node);
                self.note_depth(depth);
                Some((constant, slot, depth))
            }
            // A free name: its reference may capture a `with` object or the
            // global object, marked by the depth no chain reaches.
            Resolved::Global => {
                let constant = self.identifier_constant(node);
                Some((constant, 0, u32::MAX))
            }
            _ => None,
        }
    }

    fn prepare_reference(&mut self, target: &Node) -> Reference {
        match target.kind {
            NodeKind::Member => {
                let object = self.allocate();
                self.expression(target.first);
                self.emit(Opcode::Star, &[i64::from(object)]);
                Reference {
                    object,
                    key: {
                        self.private_member_guard(target.second);
                        self.key_constant(target.second)
                    },
                }
            }
            NodeKind::Index => {
                let object = self.allocate();
                self.expression(target.first);
                self.emit(Opcode::Star, &[i64::from(object)]);
                let key = self.allocate();
                self.expression(target.second);
                // The key stays the value the expression produced: making a
                // property key of it happens at the access, after the right
                // side has run, as the specification orders an assignment.
                self.emit(Opcode::Star, &[i64::from(key)]);
                Reference { object, key }
            }
            NodeKind::SuperMember | NodeKind::SuperIndex => {
                // A super reference: the base — the home object's prototype
                // — is fetched as the reference forms, before any key
                // expression or right side runs.
                if !self.allow_super_property {
                    self.fail(target, code::SYNTAX_NOT_ADMITTED);
                    return Reference { object: 0, key: 0 };
                }
                let object = self.allocate();
                self.emit(Opcode::GetSuperBase, &[]);
                self.emit(Opcode::Star, &[i64::from(object)]);
                if matches!(target.kind, NodeKind::SuperMember) {
                    let key = self.identifier_constant(target);
                    Reference { object, key }
                } else {
                    let key = self.allocate();
                    self.expression(target.first);
                    self.emit(Opcode::Star, &[i64::from(key)]);
                    Reference { object, key }
                }
            }
            NodeKind::Identifier => {
                // A slot an eval could shadow resolves when the reference
                // forms: the environment is taken now, used at the write.
                if let Some((constant, slot, depth)) = self.shadowable_slot(target) {
                    let object = self.allocate();
                    self.emit(
                        Opcode::PrepareShadowable,
                        &[i64::from(constant), i64::from(slot), i64::from(depth)],
                    );
                    self.emit(Opcode::Star, &[i64::from(object)]);
                    return Reference {
                        object,
                        key: constant,
                    };
                }
                if self.strict_global_target(target) {
                    // Strict code resolves the reference before the value
                    // is made: an unresolvable one throws at the write,
                    // whatever the value's evaluation added to the global.
                    let constant = self.identifier_constant(target);
                    let object = self.allocate();
                    self.emit(Opcode::HasGlobal, &[i64::from(constant)]);
                    self.emit(Opcode::Star, &[i64::from(object)]);
                    return Reference {
                        object,
                        key: constant,
                    };
                }
                Reference { object: 0, key: 0 }
            }
            _ => Reference { object: 0, key: 0 },
        }
    }

    /// Whether an identifier target is a strict write to a name that
    /// resolves nowhere the program can see, through no dynamic scope.
    fn strict_global_target(&mut self, target: &Node) -> bool {
        self.strict
            && !self.dynamic_names
            && self.with_depth == 0
            && matches!(self.resolve(target.first, target.second), Resolved::Global)
    }

    fn read_reference(&mut self, target: &Node, reference: &Reference) {
        match target.kind {
            NodeKind::Member => self.emit(
                Opcode::GetNamedProperty,
                &[i64::from(reference.object), i64::from(reference.key)],
            ),
            NodeKind::SuperMember => {
                self.emit(Opcode::LdaConstant, &[i64::from(reference.key)]);
                self.emit(Opcode::LdaSuperKeyed, &[i64::from(reference.object)]);
            }
            NodeKind::SuperIndex => {
                self.emit(Opcode::Ldar, &[i64::from(reference.key)]);
                self.emit(Opcode::LdaSuperKeyed, &[i64::from(reference.object)]);
            }
            NodeKind::Index => {
                self.emit(Opcode::Ldar, &[i64::from(reference.key)]);
                self.emit(Opcode::GetKeyedProperty, &[i64::from(reference.object)]);
            }
            NodeKind::Identifier => {
                if let Some((_, slot, _)) = self.shadowable_slot(target) {
                    self.emit(
                        Opcode::LdaPrepared,
                        &[
                            i64::from(reference.object),
                            i64::from(reference.key),
                            i64::from(slot),
                        ],
                    );
                    return;
                }
                self.expression_target_read(target);
            }
            _ => self.expression_target_read(target),
        }
    }

    fn expression_target_read(&mut self, target: &Node) {
        match target.kind {
            NodeKind::Identifier => self.load_name(target),
            _ => self.fail(target, code::LOWERING_NOT_ADMITTED),
        }
    }

    fn write_reference(&mut self, target: &Node, reference: &Reference) {
        match target.kind {
            NodeKind::Member => self.emit(
                Opcode::SetNamedProperty,
                &[i64::from(reference.object), i64::from(reference.key)],
            ),
            NodeKind::SuperMember => self.emit(
                Opcode::StaSuperNamed,
                &[i64::from(reference.object), i64::from(reference.key)],
            ),
            NodeKind::SuperIndex => self.emit(
                Opcode::StaSuperKeyed,
                &[i64::from(reference.object), i64::from(reference.key)],
            ),
            NodeKind::Index => self.emit(
                Opcode::SetKeyedProperty,
                &[i64::from(reference.object), i64::from(reference.key)],
            ),
            NodeKind::Identifier => {
                // Strict code refuses the restricted and reserved names on
                // every store path, the prepared one included.
                if self.strict {
                    if matches!(
                        self.span(target.first, target.second),
                        b"eval" | b"arguments"
                    ) {
                        self.fail(target, code::STRICT_ASSIGNMENT_TO_RESTRICTED_NAME);
                        return;
                    }
                    if self.strict_reserved_guard(target) {
                        return;
                    }
                }
                if let Some((_, slot, _)) = self.shadowable_slot(target) {
                    self.emit(
                        Opcode::StaPrepared,
                        &[
                            i64::from(reference.object),
                            i64::from(reference.key),
                            i64::from(slot),
                        ],
                    );
                    return;
                }
                if self.strict_global_target(target) {
                    if matches!(
                        self.span(target.first, target.second),
                        b"eval" | b"arguments"
                    ) {
                        self.fail(target, code::STRICT_ASSIGNMENT_TO_RESTRICTED_NAME);
                        return;
                    }
                    if self.strict_reserved_guard(target) {
                        return;
                    }
                    self.emit(
                        Opcode::StaGlobalResolved,
                        &[i64::from(reference.key), i64::from(reference.object)],
                    );
                    return;
                }
                self.store_name(target);
            }
            _ => self.store_name(target),
        }
    }
}

/// A prepared assignment target: the base register and, for an indexed
/// target, the key register — for a named one, the key constant.
struct Reference {
    object: u32,
    key: u32,
}

/// Whether a statement list opens with a `use strict` directive.
fn directive_prologue_is_strict(arena: &Arena<'_>, source: &[u8], list: u32, length: u32) -> bool {
    for &statement in arena.list(list, length) {
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
        // The node's inner span is the literal's text without its quotes.
        let text = source
            .get(expression.first as usize..expression.second as usize)
            .unwrap_or(&[]);
        if text == b"use strict" {
            return true;
        }
    }
    false
}

/// Bindings one eval-site record may hold.
const MAX_EVAL_BINDINGS: u32 = 64;

/// Whether a name is already recorded in the site being built.
fn site_holds(blob: &[u8], bindings_at: usize, end: usize, name: &[u8]) -> bool {
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
fn push_site_binding(
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

/// The first occurrence of `needle` in `haystack`, as a source position.
fn find_text(haystack: &[u8], needle: &[u8]) -> Option<u32> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let mut index = 0usize;
    while index + needle.len() <= haystack.len() {
        if &haystack[index..index + needle.len()] == needle {
            return u32::try_from(index).ok();
        }
        index += 1;
    }
    None
}

include!("statements.rs");
