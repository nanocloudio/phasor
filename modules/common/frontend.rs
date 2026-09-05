//! The front end as one pipeline: source in, verified unit image out.
//!
//! Lexing, parsing, and lowering cooperate over borrowed storage, and every
//! host that compiles — the compiler fmod, the oracles, the probes — chains
//! them the same way. This file is that chain, once, over the same storage
//! the pieces already take. The capacities stay the caller's: nothing here
//! sizes anything.

use crate::arena::{Arena, Node};
use crate::diagnostic::Diagnostic;
use crate::evalsite::EvalBinding;
use crate::lex::Lexer;
use crate::lower::{self, Compiled};
use crate::parse::Parser;
use crate::source::{Limits, LineStart, LineTable};

/// What a source is compiled as.
#[derive(Clone, Copy)]
pub enum Goal<'a> {
    /// A script, as written.
    Script,
    /// A script compiled strict throughout, whatever its prologue says.
    ScriptStrict,
    /// A module.
    Module,
    /// The body of a direct or indirect `eval`, with the site's whole
    /// picture: what it can see and what it may name.
    Eval(EvalGoal<'a>),
}

/// The site an eval compiles for. An indirect eval sees nothing and may name
/// nothing, which `EvalGoal::INDIRECT` spells out.
#[derive(Clone, Copy)]
pub struct EvalGoal<'a> {
    pub scope: &'a [EvalBinding<'a>],
    pub strict: bool,
    pub function_site: bool,
    pub var_env_depth: u32,
    pub super_property: bool,
    pub super_call: bool,
    pub new_target: bool,
    pub deny_arguments: bool,
    pub privates: bool,
    pub parameter_site: bool,
}

impl EvalGoal<'_> {
    /// An indirect eval: global code that sees no caller.
    pub const INDIRECT: Self = Self {
        scope: &[],
        strict: false,
        function_site: false,
        var_env_depth: u32::MAX,
        super_property: false,
        super_call: false,
        new_target: false,
        deny_arguments: false,
        privates: false,
        parameter_site: false,
    };
}

/// The storage the lexer and parser borrow.
pub struct ParseStorage<'a> {
    pub starts: &'a mut [LineStart],
    pub nodes: &'a mut [Node],
    pub lists: &'a mut [u32],
    pub numbers: &'a mut [f64],
    pub scratch: &'a mut [u32],
}

/// The storage the whole pipeline borrows.
pub struct Storage<'a> {
    pub parse: ParseStorage<'a>,
    pub lower: lower::Storage<'a>,
}

/// Build the borrowed `ParseStorage` from any struct holding the front-end
/// buffers under their standard names.
#[allow(unused_macros, reason = "consumed by the fmods that include this file")]
macro_rules! parse_storage {
    ($s:expr) => {
        frontend::ParseStorage {
            starts: &mut $s.starts,
            nodes: &mut $s.nodes,
            lists: &mut $s.lists,
            numbers: &mut $s.numbers,
            scratch: &mut $s.scratch,
        }
    };
}

/// Build the borrowed `Storage` for the whole pipeline. `lower_storage!`
/// must be in scope: mount `lower.rs` with `#[macro_use]` before this file.
#[allow(unused_macros, reason = "consumed by the fmods that include this file")]
macro_rules! frontend_storage {
    ($s:expr) => {
        frontend::Storage {
            parse: parse_storage!($s),
            lower: lower_storage!($s),
        }
    };
}

/// Parse a source under a goal, answering the root node. The arena the tree
/// was built in is the caller's `nodes`, `lists`, and `numbers`.
pub fn parse<'a>(
    source: &[u8],
    module: bool,
    limits: Limits,
    fuel: u32,
    storage: &'a mut ParseStorage<'_>,
) -> Result<(u32, Arena<'a>), Diagnostic> {
    let table = LineTable::new(storage.starts);
    let lexer = Lexer::new(source, limits, table, fuel)?;
    let syntax = Arena::new(storage.nodes, storage.lists, storage.numbers);
    let mut parser = Parser::new(lexer, syntax, storage.scratch, limits);
    let root = if module {
        parser.parse_module()?
    } else {
        parser.parse_unit()?
    };
    Ok((root, parser.into_arena()))
}

/// Compile a source under a goal into `storage.lower.image`, answering what
/// was compiled. The image's length is `Compiled::length`.
pub fn compile(
    source: &[u8],
    goal: Goal<'_>,
    limits: Limits,
    fuel: u32,
    storage: &mut Storage<'_>,
) -> Result<Compiled, Diagnostic> {
    let module = matches!(goal, Goal::Module);
    let (root, arena) = parse(source, module, limits, fuel, &mut storage.parse)?;
    match goal {
        Goal::Script => lower::lower_script(source, &arena, root, &mut storage.lower),
        Goal::ScriptStrict => lower::lower_script_strict(source, &arena, root, &mut storage.lower),
        Goal::Module => lower::lower_module(source, &arena, root, &mut storage.lower),
        Goal::Eval(site) => lower::lower_eval(
            source,
            &arena,
            root,
            &mut storage.lower,
            site.scope,
            site.strict,
            site.function_site,
            site.var_env_depth,
            site.super_property,
            site.super_call,
            site.new_target,
            site.deny_arguments,
            site.privates,
            site.parameter_site,
        ),
    }
}
