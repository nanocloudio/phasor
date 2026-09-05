//! Promises and their reactions.
//!
//! A promise is an ordinary object with three more internal slots: its state,
//! the value it settled with, and the reactions waiting on it. A reaction is
//! recorded while the promise is pending and becomes a job when it settles, so
//! a handler never runs during the call that attached it.

use crate::heap::{CellKind, Heap};
use crate::job::{Job, JobKind, Queue, QueueError};
use crate::object::{self, ObjectError};
use crate::value::{Handle, Value};

/// A promise's states.
pub const PENDING: u8 = 0;
pub const FULFILLED: u8 = 1;
pub const REJECTED: u8 = 2;

/// The shape of a reaction list: a count and a capacity, then records of a
/// kind byte and three encoded values. The collector traces through these
/// names.
pub mod layout {
    pub const COUNT: usize = 0;
    pub const CAPACITY: usize = 4;
    pub const HEADER: usize = 8;
    /// A kind byte and three encoded values, padded.
    pub const RECORD: usize = 32;
    pub mod record {
        pub const KIND: usize = 0;
        pub const ON_FULFILLED: usize = 1;
        pub const ON_REJECTED: usize = 10;
        pub const DERIVED: usize = 19;
    }
}

const HEADER: usize = layout::HEADER;
const RECORD: usize = layout::RECORD;
/// Reactions a new list holds.
const INITIAL_CAPACITY: u32 = 4;

/// Why a promise operation did not happen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromiseError {
    Object(ObjectError),
    Queue(QueueError),
    /// The value is not a promise.
    NotAPromise,
}

impl From<ObjectError> for PromiseError {
    fn from(error: ObjectError) -> Self {
        Self::Object(error)
    }
}

impl From<crate::heap::HeapError> for PromiseError {
    fn from(error: crate::heap::HeapError) -> Self {
        Self::Object(ObjectError::Heap(error))
    }
}

impl From<QueueError> for PromiseError {
    fn from(error: QueueError) -> Self {
        Self::Queue(error)
    }
}

/// Create a pending promise with `prototype`.
pub fn create(heap: &mut Heap<'_>, prototype: Value) -> Result<Handle, PromiseError> {
    let handle = object::create(heap, prototype)?;
    object::make_promise(heap, handle)?;
    Ok(handle)
}

/// Record a reaction, or make it a job when the promise has already settled.
///
/// `on_fulfilled` and `on_rejected` may be undefined, in which case the derived
/// promise takes the same outcome as this one.
pub fn react(
    heap: &mut Heap<'_>,
    queue: &mut Queue<'_>,
    promise: Handle,
    on_fulfilled: Value,
    on_rejected: Value,
    derived: Value,
) -> Result<(), PromiseError> {
    if !object::is_promise(heap, promise)? {
        return Err(PromiseError::NotAPromise);
    }
    let state = object::promise_state(heap, promise)?;
    if state == PENDING {
        append(heap, promise, on_fulfilled, on_rejected, derived)?;
        return Ok(());
    }

    let value = object::promise_value(heap, promise)?;
    let handler = if state == FULFILLED {
        on_fulfilled
    } else {
        on_rejected
    };
    enqueue(queue, state, handler, value, derived)?;
    Ok(())
}

/// Settle a promise and turn every reaction waiting on it into a job.
///
/// A promise settles once. A later attempt changes nothing and says so.
pub fn settle(
    heap: &mut Heap<'_>,
    queue: &mut Queue<'_>,
    promise: Handle,
    state: u8,
    value: Value,
) -> Result<bool, PromiseError> {
    if !object::is_promise(heap, promise)? {
        return Err(PromiseError::NotAPromise);
    }
    if !object::settle_promise(heap, promise, state, value)? {
        return Ok(false);
    }

    let Some(reactions) = object::promise_reactions(heap, promise)? else {
        return Ok(true);
    };
    let count = reaction_count(heap, reactions)?;
    let mut index = 0u32;
    while index < count {
        let (on_fulfilled, on_rejected, derived) = read_reaction(heap, reactions, index)?;
        let handler = if state == FULFILLED {
            on_fulfilled
        } else {
            on_rejected
        };
        enqueue(queue, state, handler, value, derived)?;
        index += 1;
    }
    // The list is spent: a promise settles once, so nothing else can be added.
    set_reaction_count(heap, reactions, 0)?;
    Ok(true)
}

/// Turn one reaction into a job.
fn enqueue(
    queue: &mut Queue<'_>,
    state: u8,
    handler: Value,
    value: Value,
    derived: Value,
) -> Result<(), PromiseError> {
    if handler.is_undefined() {
        // With no handler, the derived promise takes this promise's outcome
        // directly.
        if derived.is_object() {
            queue.push(Job {
                kind: JobKind::Settle,
                target: derived,
                argument: value,
                derived: Value::number(f64::from(state)),
            })?;
        }
        return Ok(());
    }
    queue.push(Job {
        kind: JobKind::Reaction,
        target: handler,
        argument: value,
        derived,
    })?;
    Ok(())
}

fn append(
    heap: &mut Heap<'_>,
    promise: Handle,
    on_fulfilled: Value,
    on_rejected: Value,
    derived: Value,
) -> Result<(), PromiseError> {
    let reactions = match object::promise_reactions(heap, promise)? {
        Some(reactions) => {
            let count = reaction_count(heap, reactions)?;
            let capacity = reaction_capacity(heap, reactions)?;
            if count == capacity {
                grow(heap, promise, reactions, capacity)?
            } else {
                reactions
            }
        }
        None => {
            let list = allocate(heap, INITIAL_CAPACITY)?;
            object::set_promise_reactions(heap, promise, list)?;
            list
        }
    };
    let count = reaction_count(heap, reactions)?;
    write_reaction(heap, reactions, count, on_fulfilled, on_rejected, derived)?;
    set_reaction_count(heap, reactions, count + 1)?;
    Ok(())
}

fn allocate(heap: &mut Heap<'_>, capacity: u32) -> Result<Handle, PromiseError> {
    let size = u32::try_from(HEADER + capacity as usize * RECORD)
        .map_err(|_| PromiseError::Object(ObjectError::Heap(crate::heap::HeapError::ArenaFull)))?;
    let handle = heap.allocate(CellKind::Elements, size)?;
    let cell = heap.cell_mut(handle)?;
    cell[0..4].copy_from_slice(&0u32.to_le_bytes());
    cell[4..8].copy_from_slice(&capacity.to_le_bytes());
    Ok(handle)
}

fn grow(
    heap: &mut Heap<'_>,
    promise: Handle,
    reactions: Handle,
    capacity: u32,
) -> Result<Handle, PromiseError> {
    let count = reaction_count(heap, reactions)?;
    let larger = allocate(heap, capacity.saturating_mul(2).max(INITIAL_CAPACITY))?;
    let mut index = 0u32;
    while index < count {
        let (on_fulfilled, on_rejected, derived) = read_reaction(heap, reactions, index)?;
        write_reaction(heap, larger, index, on_fulfilled, on_rejected, derived)?;
        index += 1;
    }
    set_reaction_count(heap, larger, count)?;
    object::set_promise_reactions(heap, promise, larger)?;
    Ok(larger)
}

fn reaction_count(heap: &Heap<'_>, reactions: Handle) -> Result<u32, PromiseError> {
    let cell = heap.cell(reactions)?;
    let Some(header) = cell.get(0..4) else {
        return Ok(0);
    };
    Ok(u32::from_le_bytes([
        header[0], header[1], header[2], header[3],
    ]))
}

fn reaction_capacity(heap: &Heap<'_>, reactions: Handle) -> Result<u32, PromiseError> {
    let cell = heap.cell(reactions)?;
    let Some(header) = cell.get(4..8) else {
        return Ok(0);
    };
    Ok(u32::from_le_bytes([
        header[0], header[1], header[2], header[3],
    ]))
}

fn set_reaction_count(
    heap: &mut Heap<'_>,
    reactions: Handle,
    count: u32,
) -> Result<(), PromiseError> {
    let cell = heap.cell_mut(reactions)?;
    if let Some(header) = cell.get_mut(0..4) {
        header.copy_from_slice(&count.to_le_bytes());
    }
    Ok(())
}

fn read_reaction(
    heap: &Heap<'_>,
    reactions: Handle,
    index: u32,
) -> Result<(Value, Value, Value), PromiseError> {
    let cell = heap.cell(reactions)?;
    let at = HEADER + index as usize * RECORD;
    let Some(record) = cell.get(at..at + RECORD) else {
        return Ok((Value::UNDEFINED, Value::UNDEFINED, Value::UNDEFINED));
    };
    Ok((
        Value::decode_at(record, layout::record::ON_FULFILLED),
        Value::decode_at(record, layout::record::ON_REJECTED),
        Value::decode_at(record, layout::record::DERIVED),
    ))
}

fn write_reaction(
    heap: &mut Heap<'_>,
    reactions: Handle,
    index: u32,
    on_fulfilled: Value,
    on_rejected: Value,
    derived: Value,
) -> Result<(), PromiseError> {
    let cell = heap.cell_mut(reactions)?;
    let at = HEADER + index as usize * RECORD;
    let Some(record) = cell.get_mut(at..at + RECORD) else {
        return Ok(());
    };
    record[layout::record::KIND] = 0;
    on_fulfilled.encode_at(record, layout::record::ON_FULFILLED);
    on_rejected.encode_at(record, layout::record::ON_REJECTED);
    derived.encode_at(record, layout::record::DERIVED);
    Ok(())
}
