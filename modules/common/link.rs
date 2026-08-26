//! Linking: turning a registry of modules into an ordered, immutable closure.
//!
//! Linking walks the imports from the entry, checks that every one of them
//! resolved to a module the closure holds, and assigns the order the modules
//! evaluate in. The walk is iterative and bounded: it uses a caller-provided
//! stack rather than the machine's own, so a deep or wide closure cannot
//! overrun anything.
//!
//! Cycles are admitted, because ECMAScript modules may import each other. A
//! module already being visited is not visited again, and the order that comes
//! out is the depth-first finishing order, which puts every dependency that can
//! be evaluated first before its importer.

use crate::digest::Digest;
use crate::module::{Registry, Rejection, Status};

/// Per-module walk state.
const UNVISITED: u8 = 0;
const VISITING: u8 = 1;
const DONE: u8 = 2;

/// What a completed link produced.
#[derive(Clone, Copy, Debug)]
pub struct Closure {
    /// Modules in the closure.
    pub count: u32,
    /// The digest of the closure: every key in evaluation order.
    pub digest: Digest,
    /// The entry's position in that order, which is always last.
    pub entry_order: u32,
}

/// Link the closure reachable from `entry`.
///
/// `stack` and `state` are the walk's storage: `state` needs one entry per
/// registered module, and `stack` bounds how deep the import graph may be.
pub fn link(
    registry: &mut Registry<'_>,
    entry: usize,
    stack: &mut [(u32, u32)],
    state: &mut [u8],
) -> Result<Closure, Rejection> {
    let total = registry.count();
    if entry >= total {
        return Err(Rejection::WrongState);
    }
    let Some(state) = state.get_mut(..total) else {
        return Err(Rejection::RegistryFull);
    };
    for slot in state.iter_mut() {
        *slot = UNVISITED;
    }

    // Everything reachable enters linking first, so a failure part-way leaves
    // no module claiming to be linked.
    let mut order = 0u32;
    let mut depth = 0usize;
    let mut entry_order = u32::MAX;

    push(stack, &mut depth, entry)?;
    if let Some(slot) = state.get_mut(entry) {
        *slot = VISITING;
    }
    if registry.status(entry) == Some(Status::New) {
        registry.set_status(entry, Status::Linking)?;
    }

    while depth > 0 {
        let (module, cursor) = stack
            .get(depth - 1)
            .copied()
            .ok_or(Rejection::RegistryFull)?;
        let module_index = module as usize;
        let imports = registry.imports(module_index);

        if (cursor as usize) < imports.len() {
            let import = imports[cursor as usize];
            if let Some(slot) = stack.get_mut(depth - 1) {
                slot.1 = cursor + 1;
            }
            let Some(key) = import.resolved else {
                return Err(Rejection::Unresolved);
            };
            let Some(next) = registry.index_of(key) else {
                return Err(Rejection::MissingDependency);
            };
            // A module already visited, or already on the stack, is not
            // visited again: the second case is a cycle, which is admitted, and
            // its importer simply continues.
            if state.get(next).copied().unwrap_or(DONE) == UNVISITED {
                if let Some(slot) = state.get_mut(next) {
                    *slot = VISITING;
                }
                if registry.status(next) == Some(Status::New) {
                    registry.set_status(next, Status::Linking)?;
                }
                push(stack, &mut depth, next)?;
            }
            continue;
        }

        depth -= 1;
        if let Some(slot) = state.get_mut(module_index) {
            *slot = DONE;
        }
        registry.set_order(module_index, order)?;
        if registry.status(module_index) == Some(Status::Linking) {
            registry.set_status(module_index, Status::Linked)?;
        }
        if module_index == entry {
            entry_order = order;
        }
        order += 1;
    }

    Ok(Closure {
        count: order,
        digest: registry.closure_digest(),
        entry_order,
    })
}

fn push(stack: &mut [(u32, u32)], depth: &mut usize, module: usize) -> Result<(), Rejection> {
    let slot = stack.get_mut(*depth).ok_or(Rejection::ClosureTooLarge)?;
    *slot = (u32::try_from(module).unwrap_or(0), 0);
    *depth += 1;
    Ok(())
}

/// Whether every module of a linked closure is in the linked state.
///
/// A caller checks this before evaluating: a closure with a module still
/// linking, or already failed, is not one to run.
pub fn is_linked(registry: &Registry<'_>) -> bool {
    let mut linked = false;
    for record in registry.records() {
        // A module still linking means a link did not finish, whether or not it
        // reached the point of being ordered.
        if matches!(record.status, Status::Linking | Status::Failed) {
            return false;
        }
        if record.order == u32::MAX {
            // Not part of the closure that was linked.
            continue;
        }
        if !matches!(record.status, Status::Linked | Status::Evaluated) {
            return false;
        }
        linked = true;
    }
    linked
}
