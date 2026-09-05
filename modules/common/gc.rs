//! Tracing and the collection driver.
//!
//! The heap knows how to mark, move, and reclaim; this module knows what a cell
//! points at. Keeping the layouts here means the heap needs no knowledge of the
//! object model, and the object model needs none of collection.

use crate::env::layout as environment;
use crate::heap::{CellKind, Heap, HeapError, Phase};
use crate::object::layout as object;
use crate::promise::layout as reactions;
use crate::value::{field, Handle, Tag, Value};

/// Child handles read from one cell before they are shaded.
const CHUNK: usize = 32;

/// Advance a collection by one slice.
///
/// `budget` bounds both the cells visited while marking and the bytes moved
/// while compacting, so one slice does a bounded amount of work and the whole
/// collection can be spread over several module steps.
pub fn collect_slice(heap: &mut Heap<'_>, budget: u32) -> Phase {
    match heap.phase() {
        Phase::Idle => Phase::Idle,
        Phase::Marking => {
            let mut visited = 0u32;
            while visited < budget {
                let Some(handle) = heap.next_grey() else {
                    heap.begin_compaction();
                    return heap.phase();
                };
                trace(heap, handle);
                visited += 1;
            }
            Phase::Marking
        }
        Phase::Compacting => {
            if heap.compact_step(budget) {
                Phase::Idle
            } else {
                Phase::Compacting
            }
        }
    }
}

/// Run a whole collection, in slices of `budget`, from `roots`.
pub fn collect(heap: &mut Heap<'_>, roots: &[Handle], budget: u32) -> Result<(), HeapError> {
    heap.begin_collection(roots)?;
    while heap.phase() != Phase::Idle {
        collect_slice(heap, budget);
    }
    Ok(())
}

/// Shade everything the cell refers to.
fn trace(heap: &mut Heap<'_>, handle: Handle) {
    let Ok(kind) = heap.kind(handle) else {
        return;
    };
    match kind {
        CellKind::Object => trace_object(heap, handle),
        CellKind::PropertyTable => trace_property_table(heap, handle),
        CellKind::Environment => trace_environment(heap, handle),
        CellKind::Elements => trace_reactions(heap, handle),
        // A string, symbol, or BigInt holds no reference.
        _ => {}
    }
}

fn trace_object(heap: &mut Heap<'_>, handle: Handle) {
    let mut children = [Handle::new(0, 0); 5];
    let mut count = 0usize;
    {
        let Ok(cell) = heap.cell(handle) else {
            return;
        };
        push_value(
            &mut children,
            &mut count,
            Value::decode_at(cell, object::PROTOTYPE),
        );
        if cell.len() < object::INTERNAL {
            return;
        }
        if let Some(table) = Handle::read_at(cell, object::TABLE) {
            push_handle(&mut children, &mut count, table);
        }
        push_value(
            &mut children,
            &mut count,
            Value::decode_at(cell, object::SLOT),
        );
        // A method's home object, when it has one.
        if let Some(home) = Handle::read_at(cell, object::HOME) {
            push_handle(&mut children, &mut count, home);
        }
        // A promise's reaction list, when it has one.
        if let Some(reactions) = Handle::read_at(cell, object::REACTIONS) {
            push_handle(&mut children, &mut count, reactions);
        }
    }
    for &child in children.get(..count).unwrap_or(&[]) {
        heap.shade(child);
    }
}

fn trace_property_table(heap: &mut Heap<'_>, handle: Handle) {
    const HEADER: usize = object::TABLE_HEADER;
    const RECORD: usize = object::RECORD;
    let mut index = 0usize;
    loop {
        let mut children = [Handle::new(0, 0); CHUNK];
        let mut count = 0usize;
        let mut records = 0usize;
        {
            let Ok(cell) = heap.cell(handle) else {
                return;
            };
            let Some(header) = field::<4>(cell, object::TABLE_COUNT) else {
                return;
            };
            let total = u32::from_le_bytes(header) as usize;
            if index >= total {
                return;
            }
            while index < total && count + 3 <= children.len() {
                let at = HEADER + index * RECORD;
                let Some(record) = cell.get(at..at + RECORD) else {
                    break;
                };
                // A name or symbol key is a handle; an index key is not.
                if record.get(object::record::KIND).copied().unwrap_or(0) != 0 {
                    if let Some(payload) = field::<8>(record, object::record::KEY) {
                        push_handle(
                            &mut children,
                            &mut count,
                            Handle::unpack(u64::from_le_bytes(payload)),
                        );
                    }
                }
                push_value(
                    &mut children,
                    &mut count,
                    Value::decode_at(record, object::record::FIRST),
                );
                push_short(&mut children, &mut count, record, object::record::SECOND);
                index += 1;
                records += 1;
            }
        }
        for &child in children.get(..count).unwrap_or(&[]) {
            heap.shade(child);
        }
        if records == 0 {
            return;
        }
    }
}

fn trace_environment(heap: &mut Heap<'_>, handle: Handle) {
    const HEADER: usize = environment::HEADER;
    const BINDING: usize = environment::BINDING;
    let mut index = 0usize;
    let mut first = true;
    loop {
        let mut children = [Handle::new(0, 0); CHUNK];
        let mut count = 0usize;
        let mut bindings = 0usize;
        {
            let Ok(cell) = heap.cell(handle) else {
                return;
            };
            if first {
                push_value(
                    &mut children,
                    &mut count,
                    Value::decode_at(cell, environment::PARENT),
                );
                push_value(
                    &mut children,
                    &mut count,
                    Value::decode_at(cell, environment::THIS),
                );
                push_value(
                    &mut children,
                    &mut count,
                    Value::decode_at(cell, environment::NEW_TARGET),
                );
                push_value(
                    &mut children,
                    &mut count,
                    Value::decode_at(cell, environment::FUNCTION),
                );
                first = false;
            }
            let Some(header) = field::<4>(cell, environment::COUNT) else {
                return;
            };
            let total = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
            if index >= total && count == 0 {
                return;
            }
            while index < total && count + 2 <= children.len() {
                let at = HEADER + index * BINDING;
                let Some(record) = cell.get(at..at + BINDING) else {
                    break;
                };
                if let Some(name) = field::<8>(record, environment::binding::NAME) {
                    push_handle(
                        &mut children,
                        &mut count,
                        Handle::new(
                            u32::from_le_bytes([name[0], name[1], name[2], name[3]]),
                            u32::from_le_bytes([name[4], name[5], name[6], name[7]]),
                        ),
                    );
                }
                push_value(
                    &mut children,
                    &mut count,
                    Value::decode_at(record, environment::binding::VALUE),
                );
                index += 1;
                bindings += 1;
            }
        }
        for &child in children.get(..count).unwrap_or(&[]) {
            heap.shade(child);
        }
        if bindings == 0 {
            return;
        }
    }
}

/// A reaction list: a count, then records of a kind and two values.
fn trace_reactions(heap: &mut Heap<'_>, handle: Handle) {
    const HEADER: usize = reactions::HEADER;
    const RECORD: usize = reactions::RECORD;
    let mut index = 0usize;
    loop {
        let mut children = [Handle::new(0, 0); CHUNK];
        let mut count = 0usize;
        let mut records = 0usize;
        {
            let Ok(cell) = heap.cell(handle) else {
                return;
            };
            let Some(header) = field::<4>(cell, reactions::COUNT) else {
                return;
            };
            let total = u32::from_le_bytes(header) as usize;
            if index >= total {
                return;
            }
            while index < total && count + 3 <= children.len() {
                let at = HEADER + index * RECORD;
                let Some(record) = cell.get(at..at + RECORD) else {
                    break;
                };
                push_value(
                    &mut children,
                    &mut count,
                    Value::decode_at(record, reactions::record::ON_FULFILLED),
                );
                push_value(
                    &mut children,
                    &mut count,
                    Value::decode_at(record, reactions::record::ON_REJECTED),
                );
                push_value(
                    &mut children,
                    &mut count,
                    Value::decode_at(record, reactions::record::DERIVED),
                );
                index += 1;
                records += 1;
            }
        }
        for &child in children.get(..count).unwrap_or(&[]) {
            heap.shade(child);
        }
        if records == 0 {
            return;
        }
    }
}

fn push_value(children: &mut [Handle], count: &mut usize, value: Value) {
    if !holds_reference(&value) {
        return;
    }
    push_handle(children, count, value.as_handle());
}

fn push_handle(children: &mut [Handle], count: &mut usize, handle: Handle) {
    if let Some(slot) = children.get_mut(*count) {
        *slot = handle;
        *count += 1;
    }
}

fn push_short(children: &mut [Handle], count: &mut usize, bytes: &[u8], at: usize) {
    push_value(children, count, Value::decode_short_at(bytes, at));
}

const fn holds_reference(value: &Value) -> bool {
    matches!(
        value.tag(),
        Tag::String | Tag::Symbol | Tag::BigInt | Tag::Object
    )
}
