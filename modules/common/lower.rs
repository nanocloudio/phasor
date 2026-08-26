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
use crate::arena::{property_key, property_kind, Arena, Node, NodeKind, NONE};
use crate::bytecode::Unit;
use crate::bytecode::{
    function_flag, unit_flag, Constant, ConstantKind, ExceptionRegion, ExportRecord, Function,
    ImportRecord, Opcode,
};
use crate::diagnostic::{code, Diagnostic, Severity};
use crate::emit::{BuildError, CodeBuilder, Label, Patch, UnitWriter};
use crate::evalsite::{EvalBinding, FLAG_STRICT, FLAG_TRUNCATED};
use crate::lex::{cook, Token, TokenKind};

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
}

impl Pending {
    pub const EMPTY: Self = Self {
        node: 0,
        scope: NONE,
        index: 0,
        strict: false,
    };
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
    )
}

/// Compile one parsed source as the body of a direct `eval`.
///
/// `scope` is what the call site could see, decoded from the caller image's
/// eval-site record: free names resolve against it before falling to the
/// global object, so the compiled unit runs over the caller's environments.
/// `strict` is the strictness of the code the call was written in, which the
/// eval source inherits before its own directive says anything.
pub fn lower_eval(
    source: &[u8],
    arena: &Arena<'_>,
    root: u32,
    storage: &mut Storage<'_>,
    scope: &[EvalBinding<'_>],
    strict: bool,
) -> Result<Compiled, Diagnostic> {
    lower_inner(
        source, arena, root, storage, true, false, scope, strict, true,
    )
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
    lower_inner(source, arena, root, storage, true, true, &[], false, false)
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
            Body::Function(entry.node),
            entry.scope,
            entry.index,
            entry.strict,
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
    /// Whether the source is the body of a direct eval, whose strict form
    /// keeps its `var` declarations to itself.
    eval_goal: bool,
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

    fn queue(&mut self, node: u32, scope: u32, index: u32, strict: bool) -> Result<(), Diagnostic> {
        let slot = self
            .pending
            .get_mut(self.pending_count)
            .ok_or_else(|| failure(code::CODE_TOO_LARGE))?;
        *slot = Pending {
            node,
            scope,
            index,
            strict,
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
}

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
    program: &mut Program<'_>,
    code: &mut [u8],
    safe_points: &mut [u32],
    patches: &mut [Patch],
    labels: &mut [u32],
) -> Result<(), Diagnostic> {
    let scope = program.open_scope(enclosing_scope)?;
    let mut arrow = false;
    let mut parameters = 0u32;
    let mut self_name = false;
    let mut uses_arguments = false;
    let mut strict = enclosing_strict;

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
                if let Some(record) = program.scopes.get_mut(scope as usize) {
                    record.context = true;
                }
                (node.first, node.second, false)
            } else if matches!(node.kind, NodeKind::Script) {
                declare_lexical(arena, source, node.first, node.second, scope, program, true)?;
                // Strict eval code keeps its `var`s: they are bindings of the
                // eval's own scope, not properties of the global object and
                // not writes into the caller.
                if program.eval_goal
                    && (enclosing_strict
                        || directive_prologue_is_strict(arena, source, node.first, node.second))
                {
                    // Declaring is what marks the scope a context, so an
                    // eval that hoists nothing claims none and pushes none.
                    hoist_vars(arena, node.first, node.second, scope, program)?;
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
        Body::Function(node_index) => {
            let node = arena
                .node(node_index)
                .copied()
                .unwrap_or(Node::new(NodeKind::Null, 0, 0));
            arrow = node.has(flag::ARROW);
            let entries = arena.list(node.second, node.third);
            let body_index = entries.first().copied().unwrap_or(NONE);
            // A named function expression can call itself by its own name.
            if !arrow && node.first != NONE {
                self_name = true;
                if let Some(name) = arena.node(node.first) {
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
            for &parameter in entries.get(1..).unwrap_or(&[]) {
                let Some(record) = arena.node(parameter) else {
                    continue;
                };
                let Some(name) = arena.node(record.first) else {
                    continue;
                };
                program.declare(
                    scope,
                    Binding {
                        start: name.first,
                        end: name.second,
                        kind: binding_kind::VARIABLE,
                        slot: 0,
                    },
                )?;
                parameters += 1;
            }
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
            if node.has(flag::CONCISE_BODY) {
                (body_index, u32::MAX, true)
            } else {
                // A body's `var` declarations belong to the function, wherever
                // in it they are written.
                hoist_vars(arena, body_node.first, body_node.second, scope, program)?;
                declare_lexical(
                    arena,
                    source,
                    body_node.first,
                    body_node.second,
                    scope,
                    program,
                    false,
                )?;
                (body_node.first, body_node.second, false)
            }
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

    // A strict function refuses a parameter named `eval` or `arguments`, and
    // refuses two parameters with one name — early errors, before anything
    // runs.
    if strict && parameters > 0 {
        let record = program.scope(scope);
        let first = record.first as usize + usize::from(self_name);
        let mut index = 0usize;
        while index < parameters as usize {
            let Some(binding) = program.bindings.get(first + index) else {
                break;
            };
            let name = source
                .get(binding.start as usize..binding.end as usize)
                .unwrap_or(&[]);
            if name == b"eval" || name == b"arguments" {
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

    let slots = program.scope(scope).count;
    if matches!(body, Body::Function(_)) {
        // A call always makes the callee an environment, so its scope owns a
        // context even when it declares nothing.
        if let Some(record) = program.scopes.get_mut(scope as usize) {
            record.context = true;
        }
    }

    let module_body = program.module && matches!(body, Body::Script(_));
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
            high_water: if uses_arguments {
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
            holes: [(0, 0, 0); MAX_HOLES],
            hole_count: 0,
            in_function: matches!(body, Body::Function(_)),
            strict,
            completion: 0,
            default_export_slot: 0,
        };
        lowering.builder.safe_point();

        match body {
            Body::Function(_) => {
                lowering.function_prologue(
                    self_name,
                    parameters,
                    statements,
                    statement_count,
                    concise,
                );
                if concise {
                    lowering.expression(statements);
                    lowering.emit(Opcode::Return, &[]);
                } else {
                    lowering.statements(statements, statement_count);
                    // A body that ends by returning needs no return of its own,
                    // and one written after it would be unreachable.
                    if !lowering.builder.terminated() {
                        lowering.emit(Opcode::LdaUndefined, &[]);
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
        argument_count: parameters,
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
                for &clause in arena.list(node.first, node.second) {
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
                    NodeKind::Declaration | NodeKind::Function => {
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
            let kind = if node.third == declaration::CONST {
                binding_kind::CONST
            } else {
                binding_kind::LET
            };
            for &declarator in arena.list(node.first, node.second) {
                let Some(record) = arena.node(declarator) else {
                    continue;
                };
                let Some(name) = arena.node(record.first) else {
                    continue;
                };
                program.declare(
                    scope,
                    Binding {
                        start: name.first,
                        end: name.second,
                        kind,
                        slot: 0,
                    },
                )?;
            }
        }
        NodeKind::Declaration => hoist_statement(arena, statement, scope, program)?,
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
        match node.kind {
            NodeKind::Declaration if node.third != declaration::VAR => {
                let kind = if node.third == declaration::CONST {
                    binding_kind::CONST
                } else {
                    binding_kind::LET
                };
                for &declarator in arena.list(node.first, node.second) {
                    let Some(record) = arena.node(declarator) else {
                        continue;
                    };
                    let Some(name) = arena.node(record.first) else {
                        continue;
                    };
                    program.declare(
                        scope,
                        Binding {
                            start: name.first,
                            end: name.second,
                            kind,
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
                let Some(name) = arena.node(record.first) else {
                    continue;
                };
                program.declare(
                    scope,
                    Binding {
                        start: name.first,
                        end: name.second,
                        kind: binding_kind::VARIABLE,
                        slot: 0,
                    },
                )?;
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
    /// Ranges holding an inline finaliser copy, each tagged with its owning
    /// `try`'s depth: a region belonging to a `try` nested that deep or
    /// deeper must not cover the copy, or a throwing `finally` would run
    /// itself again.
    holes: [(u32, u32, u32); MAX_HOLES],
    hole_count: usize,
    /// Whether `return` is admitted here.
    in_function: bool,
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
                if self.span(binding.start, binding.end) == text {
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
    fn load_name(&mut self, node: &Node) {
        match self.resolve(node.first, node.second) {
            Resolved::Slot {
                slot,
                kind: binding_kind::IMPORT,
                ..
            } => {
                // The slot of an import is its index in the module's import
                // table; what it names lives in another module.
                self.emit(Opcode::LdaImport, &[i64::from(slot)]);
            }
            Resolved::Slot { depth, slot, .. } => {
                self.note_depth(depth);
                self.emit(Opcode::LdaContextSlot, &[i64::from(slot), i64::from(depth)]);
            }
            Resolved::Global => {
                let constant = self.identifier_constant(node);
                self.emit(Opcode::LdaGlobal, &[i64::from(constant)]);
            }
        }
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
        match self.resolve(node.first, node.second) {
            Resolved::Slot { depth, slot, kind } => {
                if kind == binding_kind::CONST || kind == binding_kind::IMPORT {
                    // An imported name belongs to the module that exports it.
                    self.fail(node, code::ASSIGNMENT_TO_CONSTANT);
                    return;
                }
                if kind == binding_kind::SELF {
                    // A named function expression's own name is immutable:
                    // the write is evaluated and discarded.
                    return;
                }
                self.note_depth(depth);
                self.emit(Opcode::StaContextSlot, &[i64::from(slot), i64::from(depth)]);
            }
            Resolved::Global => {
                let constant = self.identifier_constant(node);
                // Strict code assigns only what exists; sloppy code creates.
                let opcode = if self.strict {
                    Opcode::StaGlobalStrict
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
                self.emit(Opcode::StaGlobal, &[i64::from(constant)]);
            }
        }
    }

    // Statements.

    fn statements(&mut self, list: u32, length: u32) {
        let items = self.arena.list(list, length);
        let mut index = 0usize;
        while index < items.len() {
            let statement = items[index];
            self.statement(statement);
            if self.program.failure.is_some() {
                return;
            }
            // A statement written after one that returns, throws, or jumps away
            // can never run, so nothing is emitted for it.
            if self.builder.terminated() {
                return;
            }
            index += 1;
        }
    }

    fn statement(&mut self, index: u32) {
        if self.program.failure.is_some() || index == NONE {
            return;
        }
        let node = self.node(index);
        match node.kind {
            NodeKind::ExpressionStatement => {
                let mark = self.registers;
                self.expression(node.first);
                if !self.in_function {
                    // A script's value is its last expression statement's.
                    let completion = self.completion;
                    self.emit(Opcode::Star, &[i64::from(completion)]);
                }
                self.release(mark);
            }
            NodeKind::Declaration => self.declaration(&node),
            NodeKind::Block => self.block(&node),
            NodeKind::Empty | NodeKind::Debugger => {}
            NodeKind::Function => {
                // Declarations were hoisted; the closure was made on entry.
            }
            NodeKind::Import => {
                // An import binds nothing here: the module that exports the
                // name holds it, and a read goes through that module.
            }
            NodeKind::Export => {
                if node.has(flag::PREFIX) {
                    // `export default expression;`
                    let mark = self.registers;
                    self.expression(node.first);
                    let slot = self.default_slot();
                    self.emit(Opcode::InitContextSlot, &[i64::from(slot), 0]);
                    self.release(mark);
                } else if node.first != NONE {
                    // A function declaration was already made on entry.
                    let inner = self.node(node.first);
                    if !matches!(inner.kind, NodeKind::Function) {
                        self.statement(node.first);
                    }
                }
            }
            NodeKind::If => {
                self.reset_completion();
                self.if_statement(&node);
            }
            NodeKind::While => {
                self.reset_completion();
                self.while_statement(&node, 0, 0);
            }
            NodeKind::DoWhile => {
                self.reset_completion();
                self.do_while_statement(&node, 0, 0);
            }
            NodeKind::For => {
                self.reset_completion();
                self.for_statement(&node, 0, 0);
            }
            NodeKind::ForInOf => {
                self.reset_completion();
                self.for_in_of_statement(&node, 0, 0);
            }
            NodeKind::Break | NodeKind::Continue => self.break_or_continue(&node),
            NodeKind::Return => self.return_statement(&node),
            NodeKind::Throw => {
                let mark = self.registers;
                self.expression(node.first);
                self.emit(Opcode::Throw, &[]);
                self.release(mark);
            }
            NodeKind::Try => {
                self.reset_completion();
                self.try_statement(&node);
            }
            NodeKind::Switch => {
                self.reset_completion();
                self.switch_statement(&node, 0, 0);
            }
            NodeKind::Labelled => {
                self.reset_completion();
                self.labelled_statement(&node);
            }
            _ => self.fail(&node, code::LOWERING_NOT_ADMITTED),
        }
    }

    /// A labelled statement passes its label to the statement it labels, so
    /// `break label` and `continue label` reach the right one.
    /// A script's value comes only from the statements inside a compound
    /// statement, never from before it: `1; if (true) {}` is `undefined`.
    fn reset_completion(&mut self) {
        if self.in_function {
            return;
        }
        self.emit(Opcode::LdaUndefined, &[]);
        let completion = self.completion;
        self.emit(Opcode::Star, &[i64::from(completion)]);
    }

    fn labelled_statement(&mut self, node: &Node) {
        let label = self.node(node.first);
        let body = self.node(node.second);
        match body.kind {
            NodeKind::While => self.while_statement(&body, label.first, label.second),
            NodeKind::DoWhile => self.do_while_statement(&body, label.first, label.second),
            NodeKind::For => self.for_statement(&body, label.first, label.second),
            NodeKind::ForInOf => self.for_in_of_statement(&body, label.first, label.second),
            NodeKind::Switch => self.switch_statement(&body, label.first, label.second),
            _ => {
                // A label on anything else can still be broken out of.
                let done = self.builder.label();
                self.open_target(Target {
                    label_start: label.first,
                    label_end: label.second,
                    breakable: false,
                    continuable: false,
                    break_label: done,
                    continue_label: done,
                    context_depth: self.context_depth,
                    finaliser_depth: u32::try_from(self.finaliser_count).unwrap_or(0),
                    break_used: false,
                    continue_used: false,
                });
                self.statement(node.second);
                let (break_used, _) = self.target_used();
                self.close_target();
                if break_used || !self.builder.terminated() {
                    self.builder.bind(done);
                }
            }
        }
    }

    fn open_target(&mut self, target: Target) {
        match self.targets.get_mut(self.target_count) {
            Some(slot) => {
                *slot = target;
                self.target_count += 1;
            }
            None => {
                let node = Node::new(NodeKind::Null, 0, 0);
                self.fail(&node, code::EXPRESSION_TOO_DEEP);
            }
        }
    }

    /// Which of the innermost target's labels anything jumps to.
    fn target_used(&self) -> (bool, bool) {
        match self.targets.get(self.target_count.saturating_sub(1)) {
            Some(target) => (target.break_used, target.continue_used),
            None => (false, false),
        }
    }

    fn close_target(&mut self) {
        self.target_count = self.target_count.saturating_sub(1);
    }

    fn block(&mut self, node: &Node) {
        let outer = self.scope;
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
        let arena = self.arena;
        if let Err(diagnostic) = declare_lexical(
            arena,
            self.source,
            node.first,
            node.second,
            scope,
            self.program,
            false,
        ) {
            if self.program.failure.is_none() {
                self.program.failure = Some(diagnostic);
            }
            self.scope = outer;
            return;
        }
        let slots = self.program.scope(scope).count;
        if slots > 0 {
            self.push_context(slots);
            self.declare_functions(node.first, node.second);
        }
        self.statements(node.first, node.second);
        if slots > 0 {
            self.pop_context();
        }
        self.scope = outer;
    }

    /// Make the closures a scope's function declarations name, before any of
    /// its statements run.
    fn declare_functions(&mut self, list: u32, length: u32) {
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
            index += 1;
        }
    }

    /// A module's own names get their values before its first statement runs:
    /// its `var`s are undefined, its function declarations are closures, and
    /// its imports and exports are recorded in the image.
    fn module_prologue(&mut self, list: u32, length: u32) {
        let scope = self.scope;
        let record = self.program.scope(scope);
        let first = record.first as usize;
        let mut slot = 0u32;
        while slot < record.count {
            let Some(binding) = self.program.bindings.get(first + slot as usize).copied() else {
                break;
            };
            if binding.kind == binding_kind::VARIABLE {
                self.emit(Opcode::LdaUndefined, &[]);
                self.emit(Opcode::InitContextSlot, &[i64::from(binding.slot), 0]);
            }
            slot += 1;
        }
        self.declare_functions(list, length);
        self.record_module_tables(list, length);
    }

    /// Fill in what the module imports and exports, now that its names have
    /// slots and its specifiers can be interned.
    fn record_module_tables(&mut self, list: u32, length: u32) {
        let items = self.arena.list(list, length);
        let mut index = 0usize;
        let mut import = 0usize;
        while index < items.len() {
            let node = self.node(items[index]);
            match node.kind {
                NodeKind::Import => {
                    let specifier_node = self.node(node.third);
                    let specifier = self.text_constant(
                        &specifier_node,
                        ConstantKind::String,
                        TokenKind::String,
                    );
                    let clauses = self.arena.list(node.first, node.second);
                    let mut clause_index = 0usize;
                    while clause_index < clauses.len() {
                        let clause = self.node(clauses[clause_index]);
                        let local = self.node(clause.first);
                        let name = if clause.has(flag::NAMESPACE) {
                            // A namespace import names the module itself.
                            u32::MAX
                        } else if clause.second == NONE {
                            self.default_key_constant()
                        } else {
                            self.key_constant(clause.second)
                        };
                        let slot = match self.resolve(local.first, local.second) {
                            Resolved::Slot { slot, .. } => slot,
                            Resolved::Global => 0,
                        };
                        if let Some(record) = self.program.imports.get_mut(import) {
                            *record = ImportRecord {
                                specifier,
                                name,
                                slot,
                            };
                        }
                        import += 1;
                        clause_index += 1;
                    }
                }
                NodeKind::Export => self.record_export(&node),
                _ => {}
            }
            index += 1;
        }
    }

    /// Record what one `export` makes available.
    fn record_export(&mut self, node: &Node) {
        if node.has(flag::PREFIX) {
            // `export default expression;` binds the value to the name
            // `default`, which is a name no identifier can be.
            let name = self.default_key_constant();
            let slot = self.default_slot();
            self.push_export(name, slot);
            return;
        }
        if node.first != NONE {
            let inner = self.node(node.first);
            match inner.kind {
                NodeKind::Declaration => {
                    for offset in 0..inner.second {
                        let Some(&declarator) = self
                            .arena
                            .list(inner.first, inner.second)
                            .get(offset as usize)
                        else {
                            break;
                        };
                        let record = self.node(declarator);
                        let name_node = self.node(record.first);
                        let name = self.key_constant(record.first);
                        if let Resolved::Slot { slot, .. } =
                            self.resolve(name_node.first, name_node.second)
                        {
                            self.push_export(name, slot);
                        }
                    }
                }
                NodeKind::Function if inner.first != NONE => {
                    let name_node = self.node(inner.first);
                    let name = self.key_constant(inner.first);
                    if let Resolved::Slot { slot, .. } =
                        self.resolve(name_node.first, name_node.second)
                    {
                        self.push_export(name, slot);
                    }
                }
                _ => {}
            }
            return;
        }
        for offset in 0..node.third {
            let Some(&clause) = self
                .arena
                .list(node.second, node.third)
                .get(offset as usize)
            else {
                break;
            };
            let record = self.node(clause);
            let local = self.node(record.first);
            let name = self.key_constant(record.second);
            if let Resolved::Slot { slot, .. } = self.resolve(local.first, local.second) {
                self.push_export(name, slot);
            }
        }
    }

    fn push_export(&mut self, name: u32, slot: u32) {
        let at = self.program.export_count;
        match self.program.exports.get_mut(at) {
            Some(record) => {
                *record = ExportRecord { name, slot };
                self.program.export_count += 1;
            }
            None => {
                let node = Node::new(NodeKind::Null, 0, 0);
                self.fail(&node, code::CODE_TOO_LARGE);
            }
        }
    }

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
        self.default_export_slot
    }

    /// A script's `var` names become properties of the global object, and its
    /// function declarations become properties holding closures.
    fn script_prologue(&mut self, list: u32, length: u32) {
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
                // script, the eval's own scope when strict eval hoisted it.
                let name = self.node(node.first);
                let constant = self.identifier_constant(&name);
                self.emit(Opcode::NameClosure, &[i64::from(constant)]);
                self.store_name(&name);
            }
            index += 1;
        }
        // The script's own lexical declarations are initialised where they are
        // written; only its functions exist before the first statement runs.
        self.declare_script_functions_in_blocks(list, length);
    }

    fn declare_script_functions_in_blocks(&mut self, _list: u32, _length: u32) {}

    /// Define the global properties a `var` introduces, so a name that is
    /// declared but never assigned still reads as `undefined`.
    fn declare_global_vars(&mut self, index: u32) {
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
                    let name = self.node(record.first);
                    // A `var` that resolves — to the eval's own hoisted scope,
                    // or to a binding the eval site can see — is that binding:
                    // it declares nothing on the global object.
                    if matches!(self.resolve(name.first, name.second), Resolved::Slot { .. }) {
                        continue;
                    }
                    let constant = self.identifier_constant(&name);
                    self.emit(Opcode::DeclareGlobal, &[i64::from(constant)]);
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
    fn initialise_hoisted(&mut self, scope: u32) {
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

    /// A function's parameters and hoisted names get their values before its
    /// first statement runs. A `let` or `const` does not: it stays
    /// uninitialised until its declaration, which is its dead zone.
    fn function_prologue(
        &mut self,
        self_name: bool,
        parameters: u32,
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
            let parameter = if self_name {
                slot.checked_sub(1)
            } else {
                Some(slot)
            };
            match parameter {
                Some(argument)
                    if argument < parameters && binding.kind == binding_kind::VARIABLE =>
                {
                    // The arguments arrive in the first registers, in order.
                    self.emit(Opcode::Ldar, &[i64::from(argument)]);
                }
                _ if self_name && slot == 0 => self.emit(Opcode::LdaCallee, &[]),
                // The binding a body reads as `arguments` starts as the array
                // of what the call supplied; a `var arguments` shares it, as
                // the specification says it does.
                _ if self.span(binding.start, binding.end) == b"arguments" => {
                    self.emit(Opcode::CreateArguments, &[]);
                }
                _ => self.emit(Opcode::LdaUndefined, &[]),
            }
            self.emit(Opcode::InitContextSlot, &[i64::from(binding.slot), 0]);
            slot += 1;
        }
        if !concise {
            self.declare_functions(list, length);
        }
    }

    fn declaration(&mut self, node: &Node) {
        let items = self.arena.list(node.first, node.second);
        let mut index = 0usize;
        while index < items.len() {
            let record = self.node(items[index]);
            let name = self.node(record.first);
            let mark = self.registers;
            if record.second == NONE {
                if node.third == declaration::VAR {
                    // A `var` with no initialiser leaves whatever is there.
                    index += 1;
                    continue;
                }
                self.emit(Opcode::LdaUndefined, &[]);
            } else {
                self.expression(record.second);
                self.name_closure(record.second, &name);
            }
            if node.third == declaration::VAR {
                self.store_name(&name);
            } else {
                self.initialise_name(&name);
            }
            self.release(mark);
            index += 1;
        }
    }

    fn if_statement(&mut self, node: &Node) {
        let mark = self.registers;
        self.expression(node.first);
        self.release(mark);
        let otherwise = self.builder.label();
        self.builder.jump(Opcode::JumpIfToBooleanFalse, otherwise);
        self.statement(node.second);
        if node.third == NONE {
            self.builder.bind(otherwise);
            return;
        }
        // A consequent that returned, threw, or jumped away needs no jump
        // over the alternate, and the join is then the alternate's own end:
        // binding an unused label there would make whatever follows look
        // reachable when only the alternate decides that.
        let joins = !self.builder.terminated();
        let done = self.builder.label();
        if joins {
            self.builder.jump(Opcode::Jump, done);
        }
        self.builder.bind(otherwise);
        self.statement(node.third);
        if joins {
            self.builder.bind(done);
        }
    }

    fn while_statement(&mut self, node: &Node, label_start: u32, label_end: u32) {
        let top = self.builder.label();
        let done = self.builder.label();
        let again = self.builder.label();
        self.builder.safe_point();
        self.builder.bind(top);
        let mark = self.registers;
        self.expression(node.first);
        self.release(mark);
        self.builder.jump(Opcode::JumpIfToBooleanFalse, done);
        self.open_target(Target {
            label_start,
            label_end,
            breakable: true,
            continuable: true,
            break_label: done,
            continue_label: again,
            context_depth: self.context_depth,
            finaliser_depth: u32::try_from(self.finaliser_count).unwrap_or(0),
            break_used: false,
            continue_used: false,
        });
        self.statement(node.second);
        let (_break_used, continue_used) = self.target_used();
        self.close_target();
        if continue_used || !self.builder.terminated() {
            self.builder.bind(again);
            self.builder.jump(Opcode::Jump, top);
        }
        // The test always jumps here when it fails, so this is always reached.
        self.builder.bind(done);
    }

    fn do_while_statement(&mut self, node: &Node, label_start: u32, label_end: u32) {
        let top = self.builder.label();
        let again = self.builder.label();
        let done = self.builder.label();
        self.builder.safe_point();
        self.builder.bind(top);
        self.open_target(Target {
            label_start,
            label_end,
            breakable: true,
            continuable: true,
            break_label: done,
            continue_label: again,
            context_depth: self.context_depth,
            finaliser_depth: u32::try_from(self.finaliser_count).unwrap_or(0),
            break_used: false,
            continue_used: false,
        });
        self.statement(node.first);
        let (break_used, continue_used) = self.target_used();
        self.close_target();
        if continue_used || !self.builder.terminated() {
            self.builder.bind(again);
            let mark = self.registers;
            self.expression(node.second);
            self.release(mark);
            self.builder.jump(Opcode::JumpIfToBooleanTrue, top);
        }
        if break_used || !self.builder.terminated() {
            self.builder.bind(done);
        }
    }

    fn for_statement(&mut self, node: &Node, label_start: u32, label_end: u32) {
        let parts = self.arena.list(node.second, node.third);
        let test = parts.first().copied().unwrap_or(NONE);
        let update = parts.get(1).copied().unwrap_or(NONE);
        let body = parts.get(2).copied().unwrap_or(NONE);

        // A `let` in the header belongs to the loop, not to what surrounds it.
        let outer = self.scope;
        let mut pushed = false;
        let initialiser = self.node(node.first);
        if node.first != NONE
            && matches!(initialiser.kind, NodeKind::Declaration)
            && initialiser.third != declaration::VAR
        {
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
            let kind = if initialiser.third == declaration::CONST {
                binding_kind::CONST
            } else {
                binding_kind::LET
            };
            for offset in 0..initialiser.second {
                let Some(&declarator) = self
                    .arena
                    .list(initialiser.first, initialiser.second)
                    .get(offset as usize)
                else {
                    break;
                };
                let record = self.node(declarator);
                let name = self.node(record.first);
                if let Err(diagnostic) = self.program.declare(
                    scope,
                    Binding {
                        start: name.first,
                        end: name.second,
                        kind,
                        slot: 0,
                    },
                ) {
                    if self.program.failure.is_none() {
                        self.program.failure = Some(diagnostic);
                    }
                    return;
                }
            }
            let slots = self.program.scope(scope).count;
            if slots > 0 {
                self.push_context(slots);
                pushed = true;
            }
        }

        if node.first != NONE {
            if matches!(initialiser.kind, NodeKind::Declaration) {
                self.declaration(&initialiser);
            } else {
                let mark = self.registers;
                self.expression(node.first);
                self.release(mark);
            }
        }

        // Each turn of a loop whose header declares with `let` gets bindings of
        // its own, so a closure made in the body keeps the value that turn had
        // rather than the one the loop stopped at.
        let per_iteration = if pushed {
            self.program.scope(self.scope).count
        } else {
            0
        };
        if per_iteration > 0 {
            self.copy_iteration(per_iteration);
        }

        let top = self.builder.label();
        let again = self.builder.label();
        let done = self.builder.label();
        self.builder.safe_point();
        self.builder.bind(top);
        if test != NONE {
            let mark = self.registers;
            self.expression(test);
            self.release(mark);
            self.builder.jump(Opcode::JumpIfToBooleanFalse, done);
        }
        self.open_target(Target {
            label_start,
            label_end,
            breakable: true,
            continuable: true,
            break_label: done,
            continue_label: again,
            context_depth: self.context_depth,
            finaliser_depth: u32::try_from(self.finaliser_count).unwrap_or(0),
            break_used: false,
            continue_used: false,
        });
        self.statement(body);
        let (break_used, continue_used) = self.target_used();
        self.close_target();
        if continue_used || !self.builder.terminated() {
            self.builder.bind(again);
            if per_iteration > 0 {
                self.copy_iteration(per_iteration);
            }
            if update != NONE {
                let mark = self.registers;
                self.expression(update);
                self.release(mark);
            }
            self.builder.jump(Opcode::Jump, top);
        }
        if break_used || test != NONE || !self.builder.terminated() {
            self.builder.bind(done);
        }
        if pushed {
            self.pop_context();
        }
        self.scope = outer;
    }

    /// Replace the current context with a fresh one holding the same values.
    fn copy_iteration(&mut self, slots: u32) {
        let mark = self.registers;
        let first = self.registers;
        let mut index = 0u32;
        while index < slots {
            let register = self.allocate();
            self.emit(Opcode::LdaContextSlot, &[i64::from(index), 0]);
            self.emit(Opcode::Star, &[i64::from(register)]);
            index += 1;
        }
        self.pop_context();
        self.push_context(slots);
        let mut index = 0u32;
        while index < slots {
            self.emit(Opcode::Ldar, &[i64::from(first + index)]);
            self.emit(Opcode::InitContextSlot, &[i64::from(index), 0]);
            index += 1;
        }
        self.release(mark);
    }

    /// `for (x of y)` and `for (x in y)`.
    ///
    /// Both walk something the expression produces: an iterator for `of`, and
    /// the enumerable names for `in`. A header that declares with `let` or
    /// `const` gets bindings of its own each turn, as in an ordinary `for`.
    fn for_in_of_statement(&mut self, node: &Node, label_start: u32, label_end: u32) {
        let of = node.has(flag::OF);
        let mark = self.registers;
        let source = self.allocate();
        let flag_register = self.allocate();

        let declaration_node = self.node(node.first);
        let declares = matches!(declaration_node.kind, NodeKind::Declaration);
        let lexical = declares && declaration_node.third != declaration::VAR;
        let name_node = if declares {
            let declarator = self
                .arena
                .list(declaration_node.first, declaration_node.second)
                .first()
                .copied()
                .unwrap_or(NONE);
            self.node(declarator).first
        } else {
            node.first
        };

        let outer = self.scope;
        let mut scope = NONE;
        if lexical {
            scope = match self.program.open_scope(outer) {
                Ok(scope) => scope,
                Err(diagnostic) => {
                    if self.program.failure.is_none() {
                        self.program.failure = Some(diagnostic);
                    }
                    return;
                }
            };
            let name = self.node(name_node);
            let kind = if declaration_node.third == declaration::CONST {
                binding_kind::CONST
            } else {
                binding_kind::LET
            };
            if let Err(diagnostic) = self.program.declare(
                scope,
                Binding {
                    start: name.first,
                    end: name.second,
                    kind,
                    slot: 0,
                },
            ) {
                if self.program.failure.is_none() {
                    self.program.failure = Some(diagnostic);
                }
                return;
            }
        }

        // The head's source expression runs where the bound name is already
        // declared and not yet initialised: `for (const x in { a: x })` is a
        // read in the dead zone, not a read of an outer `x`.
        if lexical {
            self.scope = scope;
            self.push_context(1);
            self.expression(node.second);
            self.pop_context();
            self.scope = outer;
        } else {
            self.expression(node.second);
        }
        if of {
            self.emit(Opcode::GetIterator, &[]);
        } else {
            self.emit(Opcode::GetEnumerable, &[]);
        }
        self.emit(Opcode::Star, &[i64::from(source)]);

        // `for (x in y)` walks an array of names, which needs a position.
        let position = if of { u32::MAX } else { self.allocate() };
        let limit = if of { u32::MAX } else { self.allocate() };
        if !of {
            self.emit(Opcode::LdaZero, &[]);
            self.emit(Opcode::Star, &[i64::from(position)]);
            let length = self.length_key_constant();
            self.emit(
                Opcode::GetNamedProperty,
                &[i64::from(source), i64::from(length)],
            );
            self.emit(Opcode::Star, &[i64::from(limit)]);
        }

        let top = self.builder.label();
        let again = self.builder.label();
        let done = self.builder.label();
        self.builder.safe_point();
        self.builder.bind(top);

        let value = self.allocate();
        if of {
            self.emit(
                Opcode::IteratorNext,
                &[i64::from(source), i64::from(flag_register)],
            );
            self.emit(Opcode::Star, &[i64::from(value)]);
            self.emit(Opcode::Ldar, &[i64::from(flag_register)]);
            self.builder.jump(Opcode::JumpIfTrue, done);
        } else {
            // A comparison takes its left operand from the register and its
            // right from the accumulator.
            self.emit(Opcode::Ldar, &[i64::from(limit)]);
            self.emit(Opcode::TestLess, &[i64::from(position)]);
            self.builder.jump(Opcode::JumpIfFalse, done);
            self.emit(Opcode::Ldar, &[i64::from(position)]);
            self.emit(Opcode::GetKeyedProperty, &[i64::from(source)]);
            self.emit(Opcode::Star, &[i64::from(value)]);
        }

        // The name takes the turn's value, in a binding of its own where the
        // header declared one.
        if lexical {
            self.scope = scope;
            self.push_context(1);
            self.emit(Opcode::Ldar, &[i64::from(value)]);
            let name = self.node(name_node);
            self.initialise_name(&name);
        } else {
            self.emit(Opcode::Ldar, &[i64::from(value)]);
            let target = self.node(name_node);
            self.store(&target, name_node);
        }

        self.open_target(Target {
            label_start,
            label_end,
            breakable: true,
            continuable: true,
            break_label: done,
            continue_label: again,
            context_depth: if lexical {
                self.context_depth.saturating_sub(1)
            } else {
                self.context_depth
            },
            finaliser_depth: u32::try_from(self.finaliser_count).unwrap_or(0),
            break_used: false,
            continue_used: false,
        });
        self.statement(node.third);
        let (_break_used, continue_used) = self.target_used();
        self.close_target();

        if continue_used || !self.builder.terminated() {
            // The turn's binding is left before `again`, so a fall off the end
            // of the body and a `continue` — which unwinds to the depth
            // outside the binding — arrive at the same depth.
            if lexical {
                self.pop_context();
            }
            self.builder.bind(again);
            if !of {
                self.emit(Opcode::Ldar, &[i64::from(position)]);
                self.emit(Opcode::Inc, &[]);
                self.emit(Opcode::Star, &[i64::from(position)]);
            }
            self.builder.jump(Opcode::Jump, top);
        } else if lexical {
            self.context_depth = self.context_depth.saturating_sub(1);
        }
        self.builder.bind(done);
        self.scope = outer;
        self.release(mark);
    }

    /// The constant holding the name `length`, which the loops that walk an
    /// array-like need.
    fn length_key_constant(&mut self) -> u32 {
        self.text_key_constant(b"length")
    }

    /// A key constant for a name the lowering itself needs, staged as the
    /// UTF-16 the constant table holds.
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

    fn break_or_continue(&mut self, node: &Node) {
        let wants_continue = matches!(node.kind, NodeKind::Continue);
        let (label_start, label_end) = if node.first == NONE {
            (0, 0)
        } else {
            let label = self.node(node.first);
            (label.first, label.second)
        };
        let named = node.first != NONE;

        let mut index = self.target_count;
        while index > 0 {
            index -= 1;
            let Some(target) = self.targets.get(index).copied() else {
                break;
            };
            let matches_label = if named {
                self.span(target.label_start, target.label_end) == self.span(label_start, label_end)
                    && target.label_end > target.label_start
            } else if wants_continue {
                target.continuable
            } else {
                target.breakable
            };
            if !matches_label {
                continue;
            }
            if wants_continue && !target.continuable {
                self.fail(node, code::ILLEGAL_BREAK_OR_CONTINUE);
                return;
            }
            // Leaving a scope means leaving its contexts, and running any
            // `finally` that the jump escapes.
            self.unwind_to(target.finaliser_depth, target.context_depth);
            if let Some(slot) = self.targets.get_mut(index) {
                if wants_continue {
                    slot.continue_used = true;
                } else {
                    slot.break_used = true;
                }
            }
            let destination = if wants_continue {
                target.continue_label
            } else {
                target.break_label
            };
            self.builder.jump(Opcode::Jump, destination);
            return;
        }
        let failure = if named {
            code::UNDECLARED_LABEL
        } else {
            code::ILLEGAL_BREAK_OR_CONTINUE
        };
        self.fail(node, failure);
    }

    /// Run every `finally` an exit escapes, and pop every context it leaves.
    ///
    /// The finalisers run innermost first, each at the context depth it was
    /// written at, which is what makes a `break` out of a `try` behave like
    /// reaching its end.
    ///
    /// The pops belong to the exiting path alone: the statements lowered after
    /// the jump sit on other paths, which still hold every context this exit
    /// left. The tracked depth is therefore put back when the unwind is done.
    fn unwind_to(&mut self, finaliser_depth: u32, context_depth: u32) {
        let restore = self.context_depth;
        self.unwind_only(finaliser_depth, context_depth);
        self.context_depth = restore;
    }

    fn unwind_only(&mut self, finaliser_depth: u32, context_depth: u32) {
        let mut index = self.finaliser_count;
        while index > finaliser_depth as usize {
            index -= 1;
            let Some(finaliser) = self.finalisers.get(index).copied() else {
                break;
            };
            while self.context_depth > finaliser.context_depth {
                self.pop_context();
            }
            let saved = self.finaliser_count;
            self.finaliser_count = index;
            // The copy runs after this exit has left every `try` inward of
            // the finaliser's own, so no region that deep may cover it — and
            // it lowers in the scope the `try` was entered in, matching the
            // contexts the pops above left standing.
            let outer_scope = self.scope;
            self.scope = finaliser.scope;
            let from = self.builder.length();
            self.statement(finaliser.node);
            let to = self.builder.length();
            self.scope = outer_scope;
            match self.holes.get_mut(self.hole_count) {
                Some(slot) => {
                    *slot = (finaliser.try_depth, from, to);
                    self.hole_count += 1;
                }
                None => {
                    let node = Node::new(NodeKind::Null, 0, 0);
                    self.fail(&node, code::EXPRESSION_TOO_DEEP);
                }
            }
            self.finaliser_count = saved;
        }
        while self.context_depth > context_depth {
            self.pop_context();
        }
    }

    fn return_statement(&mut self, node: &Node) {
        if !self.in_function {
            self.fail(node, code::RETURN_OUTSIDE_FUNCTION);
            return;
        }
        let mark = self.registers;
        if node.first == NONE {
            self.emit(Opcode::LdaUndefined, &[]);
        } else {
            self.expression(node.first);
        }
        // The value is held while the finalisers run, because one of them may
        // use the same registers.
        if self.finaliser_count > 0 || self.context_depth > 0 {
            let value = self.allocate();
            self.emit(Opcode::Star, &[i64::from(value)]);
            self.unwind_to(0, 0);
            if self.builder.terminated() {
                // A finaliser that broke out of a loop takes the exit with it,
                // and the return never happens.
                self.release(mark);
                return;
            }
            self.emit(Opcode::Ldar, &[i64::from(value)]);
        }
        self.emit(Opcode::Return, &[]);
        self.release(mark);
    }

    /// `try`, with a catch clause, a finally block, or both.
    ///
    /// The finaliser runs on every path out: falling off the end, catching,
    /// throwing on, and any `break`, `continue`, or `return` that escapes,
    /// which is why it is written into the code at each of those points rather
    /// than jumped to.
    fn try_statement(&mut self, node: &Node) {
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
                    self.statement(node.third);
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
                let name = self.node(clause.first);
                if let Err(diagnostic) = self.program.declare(
                    scope,
                    Binding {
                        start: name.first,
                        end: name.second,
                        kind: binding_kind::LET,
                        slot: 0,
                    },
                ) {
                    if self.program.failure.is_none() {
                        self.program.failure = Some(diagnostic);
                    }
                    return;
                }
                self.push_context(1);
                pushed = true;
                self.emit(Opcode::Ldar, &[i64::from(exception)]);
                self.initialise_name(&name);
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
                self.statement(node.third);
                self.finaliser_count += 1;
                if !self.builder.terminated() {
                    self.emit(Opcode::Ldar, &[i64::from(catch_exception)]);
                    self.emit(Opcode::Throw, &[]);
                }
            }
        } else {
            // Without a catch, the handler runs the finaliser and throws on.
            self.finaliser_count = self.finaliser_count.saturating_sub(1);
            self.statement(node.third);
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
                self.statement(node.third);
            }
        }
        self.try_depth = self.try_depth.saturating_sub(1);
        self.release(mark);
    }

    /// Record one exception region, in the order the verifier requires.
    fn record_region(
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

    fn push_region(
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

    fn switch_statement(&mut self, node: &Node, label_start: u32, label_end: u32) {
        let cases = self.arena.list(node.second, node.third);
        if cases.len() > MAX_CASES {
            self.fail(node, code::EXPRESSION_TOO_DEEP);
            return;
        }
        let mark = self.registers;
        let discriminant = self.allocate();
        self.expression(node.first);
        self.emit(Opcode::Star, &[i64::from(discriminant)]);

        let done = self.builder.label();
        // Where the dispatch goes when nothing matched and there is no
        // `default`: the point where the switch's context is left, which a
        // jump straight to `done` would skip.
        let fallback = self.builder.label();
        let mut bodies = [Label(0); MAX_CASES];

        // The cases share one scope, and the tests as well as the bodies run
        // inside it: a test that reads a binding a later case declares is a
        // use before initialisation, not a read of an outer name.
        let outer = self.scope;
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
        let arena = self.arena;
        let mut index = 0usize;
        while index < cases.len() {
            bodies[index] = self.builder.label();
            let case = self.node(cases[index]);
            if let Err(diagnostic) = declare_lexical(
                arena,
                self.source,
                case.second,
                case.third,
                scope,
                self.program,
                false,
            ) {
                if self.program.failure.is_none() {
                    self.program.failure = Some(diagnostic);
                }
                self.scope = outer;
                return;
            }
            index += 1;
        }
        let slots = self.program.scope(scope).count;

        // The target is opened at the depth outside the switch's context, so a
        // `break` leaves that context on its way to `done`.
        self.open_target(Target {
            label_start,
            label_end,
            breakable: true,
            continuable: false,
            break_label: done,
            continue_label: done,
            context_depth: self.context_depth,
            finaliser_depth: u32::try_from(self.finaliser_count).unwrap_or(0),
            break_used: false,
            continue_used: false,
        });
        if slots > 0 {
            self.push_context(slots);
        }
        let mut index = 0usize;
        while index < cases.len() {
            let case = self.node(cases[index]);
            self.declare_functions(case.second, case.third);
            index += 1;
        }

        let mut default = None;
        let mut index = 0usize;
        while index < cases.len() {
            let case = self.node(cases[index]);
            if case.first == NONE {
                default = Some(index);
            } else {
                self.expression(case.first);
                self.emit(Opcode::TestStrictEqual, &[i64::from(discriminant)]);
                self.builder.jump(Opcode::JumpIfTrue, bodies[index]);
            }
            index += 1;
        }
        match default {
            Some(at) => self.builder.jump(Opcode::Jump, bodies[at]),
            None => self.builder.jump(Opcode::Jump, fallback),
        }

        let mut index = 0usize;
        while index < cases.len() {
            self.builder.bind(bodies[index]);
            let case = self.node(cases[index]);
            self.statements(case.second, case.third);
            index += 1;
        }
        let fell_out = !self.builder.terminated();
        let (break_used, _) = self.target_used();
        self.close_target();

        // The fallback is where the dispatch lands when nothing matched, so a
        // switch without a `default` always reaches it; with one, only a body
        // that falls off the end does.
        if default.is_none() {
            self.builder.bind(fallback);
        }
        let leaves = default.is_none() || fell_out;
        if slots > 0 {
            if leaves {
                self.pop_context();
            } else {
                self.context_depth = self.context_depth.saturating_sub(1);
            }
        }
        self.scope = outer;
        if leaves || break_used {
            self.builder.bind(done);
        }
        self.release(mark);
    }

    /// Queue a function's body and answer the index its record will take.
    fn queue_function(&mut self, index: u32) -> u32 {
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
        if let Err(diagnostic) = self.program.queue(index, scope, reserved, self.strict) {
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
        // Cook into the spare data area as UTF-16, two bytes per code unit.
        let capacity = space.len() / 2;
        let mut units = [0u16; 256];
        let room = if capacity < units.len() {
            capacity
        } else {
            units.len()
        };
        let Some(written) = cook(
            self.source,
            &token,
            units.get_mut(..room).unwrap_or(&mut []),
        ) else {
            self.fail(node, code::TOO_MANY_CONSTANTS);
            return 0;
        };
        let mut index = 0usize;
        while index < written {
            let bytes = units[index].to_le_bytes();
            space[index * 2] = bytes[0];
            space[index * 2 + 1] = bytes[1];
            index += 1;
        }
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
            _ => {
                self.fail(&node, code::LOWERING_NOT_ADMITTED);
                0
            }
        }
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
                let value = self.arena.number(node.first);
                self.load_number(value);
            }
            NodeKind::String => {
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
            NodeKind::RegExp => {
                let constant = self.regexp_constant(&node);
                self.emit(Opcode::CreateRegExp, &[i64::from(constant)]);
            }
            NodeKind::Template => self.template(&node),
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
        let mut digits = [0u8; 128];
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
                    let name = child.first;
                    self.expression(name);
                    let key = self.key_constant(name);
                    self.emit(
                        Opcode::DefineNamedProperty,
                        &[i64::from(object), i64::from(key)],
                    );
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
                        self.expression(child.second);
                        let opcode = if getter {
                            Opcode::DefineKeyedGetter
                        } else {
                            Opcode::DefineKeyedSetter
                        };
                        self.emit(opcode, &[i64::from(object), i64::from(key_register)]);
                        self.release(inner);
                    } else {
                        let constant = self.key_constant(child.first);
                        self.expression(child.second);
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
    fn load_member(&mut self, index: u32, node: &Node) {
        let mark = self.registers;
        let object = self.allocate();
        self.expression(node.first);
        self.emit(Opcode::Star, &[i64::from(object)]);

        let skip = if node.has(flag::OPTIONAL) {
            let label = self.builder.label();
            self.builder.jump(Opcode::JumpIfNullish, label);
            Some(label)
        } else {
            None
        };

        if matches!(node.kind, NodeKind::Member) {
            let key = self.key_constant(node.second);
            self.emit(
                Opcode::GetNamedProperty,
                &[i64::from(object), i64::from(key)],
            );
        } else {
            self.expression(node.second);
            self.emit(Opcode::GetKeyedProperty, &[i64::from(object)]);
        }

        if let Some(label) = skip {
            // A short-circuited chain's value is `undefined`, whichever of
            // `null` or `undefined` cut it short.
            let done = self.builder.label();
            self.builder.jump(Opcode::Jump, done);
            self.builder.bind(label);
            self.emit(Opcode::LdaUndefined, &[]);
            self.builder.bind(done);
        }
        let _ = index;
        self.release(mark);
    }

    fn call(&mut self, node: &Node) {
        let arguments = self.arena.list(node.second, node.third);
        let callee_node = self.node(node.first);
        let mark = self.registers;
        let callee = self.allocate();
        let receiver = self.allocate();

        if matches!(callee_node.kind, NodeKind::Member | NodeKind::Index) {
            // A method call passes the object it was found on as the receiver.
            self.expression(callee_node.first);
            self.emit(Opcode::Star, &[i64::from(receiver)]);
            if matches!(callee_node.kind, NodeKind::Member) {
                let key = self.key_constant(callee_node.second);
                self.emit(
                    Opcode::GetNamedProperty,
                    &[i64::from(receiver), i64::from(key)],
                );
            } else {
                self.expression(callee_node.second);
                self.emit(Opcode::GetKeyedProperty, &[i64::from(receiver)]);
            }
            self.emit(Opcode::Star, &[i64::from(callee)]);
        } else {
            self.expression(node.first);
            self.emit(Opcode::Star, &[i64::from(callee)]);
            self.emit(Opcode::LdaUndefined, &[]);
            self.emit(Opcode::Star, &[i64::from(receiver)]);
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
            self.builder.safe_point();
            self.emit(
                Opcode::CallWithArray,
                &[i64::from(callee), i64::from(receiver), i64::from(list)],
            );
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
        if matches!(callee_node.kind, NodeKind::Identifier)
            && self.span(callee_node.first, callee_node.second) == b"eval"
            && matches!(
                self.resolve(callee_node.first, callee_node.second),
                Resolved::Global
            )
        {
            self.record_eval_site();
        }
        self.builder.safe_point();
        self.emit(
            Opcode::Call,
            &[i64::from(callee), i64::from(receiver), i64::from(count)],
        );
        self.release(mark);
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
        let header_at = at;
        at += 16;
        if at > self.program.eval_sites.len() {
            return;
        }
        let mut written = 0u32;

        // Walk the scope chain exactly as `resolve` does, first match by
        // name winning, then append what this compilation's own eval scope
        // carried, so an eval inside an eval still sees the whole chain.
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
                let (name_start, name_end, slot, kind) =
                    (binding.start, binding.end, binding.slot, binding.kind);
                index += 1;
                let name: &[u8] = self
                    .source
                    .get(name_start as usize..name_end as usize)
                    .unwrap_or(&[]);
                if name.is_empty() || site_holds(self.program.eval_sites, header_at + 16, at, name)
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
                if site_holds(self.program.eval_sites, header_at + 16, at, binding.name) {
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

        let header = self.program.eval_sites.get_mut(header_at..header_at + 16);
        let Some(header) = header else {
            return;
        };
        header[0..4].copy_from_slice(&self.function_index.to_le_bytes());
        header[4..8].copy_from_slice(&pc.to_le_bytes());
        header[8..12].copy_from_slice(&flags.to_le_bytes());
        header[12..16].copy_from_slice(&written.to_le_bytes());
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
                self.emit(Opcode::LdaGlobalOrUndefined, &[i64::from(constant)]);
                self.emit(Opcode::TypeOf, &[]);
                return;
            }
        }
        self.expression(node.first);
        let opcode = match node.third {
            unop::VOID => {
                self.emit(Opcode::LdaUndefined, &[]);
                return;
            }
            unop::TYPEOF => Opcode::TypeOf,
            unop::PLUS => Opcode::ToNumeric,
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
                    Resolved::Global => {
                        let global = self.text_key_constant(b"globalThis");
                        let key = self.identifier_constant(&target);
                        self.emit(Opcode::LdaGlobal, &[i64::from(global)]);
                        self.emit(Opcode::DeleteNamedProperty, &[i64::from(key)]);
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
        // write, exactly as a compound assignment does.
        let reference = self.prepare_reference(&target);
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
            NodeKind::Member => {
                let mark = self.registers;
                let value = self.allocate();
                self.emit(Opcode::Star, &[i64::from(value)]);
                let object = self.allocate();
                self.expression(target.first);
                self.emit(Opcode::Star, &[i64::from(object)]);
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
            self.expression(node.second);
            if matches!(target.kind, NodeKind::Identifier) {
                self.name_closure(node.second, &target);
            }
            self.store(&target, node.first);
            return;
        }

        // A compound or logical assignment evaluates its reference once: the
        // base and the key are computed here and reused for the read and the
        // write, so a side effect in either runs exactly once.
        let mark = self.registers;
        let reference = self.prepare_reference(&target);

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
            self.expression(node.second);
            if matches!(target.kind, NodeKind::Identifier) {
                self.name_closure(node.second, &target);
            }
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

    /// Whether an expression is a function written with no name of its own,
    /// which is what named evaluation applies to.
    fn is_anonymous_function(&self, value_node: u32) -> bool {
        let value = self.node(value_node);
        matches!(value.kind, NodeKind::Function) && value.first == NONE
    }

    /// Evaluate a target's base and key once, into registers a read and a
    /// write both use. A plain name needs no registers at all.
    fn prepare_reference(&mut self, target: &Node) -> Reference {
        match target.kind {
            NodeKind::Member => {
                let object = self.allocate();
                self.expression(target.first);
                self.emit(Opcode::Star, &[i64::from(object)]);
                Reference {
                    object,
                    key: self.key_constant(target.second),
                }
            }
            NodeKind::Index => {
                let object = self.allocate();
                self.expression(target.first);
                self.emit(Opcode::Star, &[i64::from(object)]);
                let key = self.allocate();
                self.expression(target.second);
                // The base must be coercible before the key is, and the key
                // is coerced exactly once: the read and the write both take
                // the property key this leaves.
                self.emit(Opcode::ToPropertyKeyChecked, &[i64::from(object)]);
                self.emit(Opcode::Star, &[i64::from(key)]);
                Reference { object, key }
            }
            _ => Reference { object: 0, key: 0 },
        }
    }

    fn read_reference(&mut self, target: &Node, reference: &Reference) {
        match target.kind {
            NodeKind::Member => self.emit(
                Opcode::GetNamedProperty,
                &[i64::from(reference.object), i64::from(reference.key)],
            ),
            NodeKind::Index => {
                self.emit(Opcode::Ldar, &[i64::from(reference.key)]);
                self.emit(Opcode::GetKeyedProperty, &[i64::from(reference.object)]);
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
            NodeKind::Index => self.emit(
                Opcode::SetKeyedProperty,
                &[i64::from(reference.object), i64::from(reference.key)],
            ),
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
