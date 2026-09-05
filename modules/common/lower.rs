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

// One type across these files: each child adds `impl` blocks and sees the
// parent through `use super::*`; the parent sees what a child marks
// `pub(super)` through these globs.
#[path = "lower/calls.rs"]
mod calls;
#[path = "lower/classes.rs"]
mod classes;
#[path = "lower/constants.rs"]
mod constants;
#[path = "lower/exceptions.rs"]
mod exceptions;
#[path = "lower/expressions.rs"]
mod expressions;
#[path = "lower/functions.rs"]
mod functions;
#[path = "lower/loops.rs"]
mod loops;
#[path = "lower/modules.rs"]
mod modules;
#[path = "lower/patterns.rs"]
mod patterns;
#[path = "lower/prologues.rs"]
mod prologues;
#[path = "lower/references.rs"]
mod references;
#[path = "lower/scopes.rs"]
mod scopes;
#[path = "lower/statements.rs"]
mod statements;
use calls::*;
use classes::*;
use constants::*;
use exceptions::*;
use expressions::*;
use functions::*;
use loops::*;
use modules::*;
use patterns::*;
use prologues::*;
use references::*;
use scopes::*;
use statements::*;

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
#[expect(
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

#[expect(
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

    #[expect(
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
