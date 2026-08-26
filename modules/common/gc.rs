//! Tracing and the collection driver.
//!
//! The heap knows how to mark, move, and reclaim; this module knows what a cell
//! points at. Keeping the layouts here means the heap needs no knowledge of the
//! object model, and the object model needs none of collection.

use crate::heap::{CellKind, Heap, HeapError, Phase};
use crate::value::{Handle, Tag, Value};

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
        // A string, symbol, or BigInt holds no reference, and the remaining
        // kinds are not built yet.
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
        push_value(&mut children, &mut count, read_value(cell, 0));
        let Some(table) = field8(cell, 12) else {
            return;
        };
        let table_index = u32::from_le_bytes([table[0], table[1], table[2], table[3]]);
        if table_index != u32::MAX {
            let generation = u32::from_le_bytes([table[4], table[5], table[6], table[7]]);
            push_handle(
                &mut children,
                &mut count,
                Handle::new(table_index, generation),
            );
        }
        push_value(&mut children, &mut count, read_value(cell, 28));
        // A promise's reaction list, when it has one.
        if let Some(reactions) = field8(cell, 40) {
            let index =
                u32::from_le_bytes([reactions[0], reactions[1], reactions[2], reactions[3]]);
            if index != u32::MAX {
                let generation =
                    u32::from_le_bytes([reactions[4], reactions[5], reactions[6], reactions[7]]);
                push_handle(&mut children, &mut count, Handle::new(index, generation));
            }
        }
    }
    for &child in children.get(..count).unwrap_or(&[]) {
        heap.shade(child);
    }
}

fn trace_property_table(heap: &mut Heap<'_>, handle: Handle) {
    const HEADER: usize = 8;
    const RECORD: usize = 32;
    let mut index = 0usize;
    loop {
        let mut children = [Handle::new(0, 0); CHUNK];
        let mut count = 0usize;
        let mut records = 0usize;
        {
            let Ok(cell) = heap.cell(handle) else {
                return;
            };
            let Some(header) = field4(cell, 0) else {
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
                if record.first().copied().unwrap_or(0) != 0 {
                    if let Some(payload) = field8(record, 8) {
                        let value = u64::from_le_bytes(payload);
                        push_handle(
                            &mut children,
                            &mut count,
                            Handle::new((value & 0xFFFF_FFFF) as u32, (value >> 32) as u32),
                        );
                    }
                }
                push_value(&mut children, &mut count, read_value(record, 16));
                push_short(&mut children, &mut count, record, 25);
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
    const HEADER: usize = 32;
    const BINDING: usize = 24;
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
                push_value(&mut children, &mut count, read_value(cell, 12));
                push_value(&mut children, &mut count, read_value(cell, 21));
                first = false;
            }
            let Some(header) = field4(cell, 4) else {
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
                if let Some(name) = field8(record, 4) {
                    push_handle(
                        &mut children,
                        &mut count,
                        Handle::new(
                            u32::from_le_bytes([name[0], name[1], name[2], name[3]]),
                            u32::from_le_bytes([name[4], name[5], name[6], name[7]]),
                        ),
                    );
                }
                push_value(&mut children, &mut count, read_value(record, 12));
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
    const HEADER: usize = 8;
    const RECORD: usize = 32;
    let mut index = 0usize;
    loop {
        let mut children = [Handle::new(0, 0); CHUNK];
        let mut count = 0usize;
        let mut records = 0usize;
        {
            let Ok(cell) = heap.cell(handle) else {
                return;
            };
            let Some(header) = field4(cell, 0) else {
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
                push_value(&mut children, &mut count, read_value(record, 1));
                push_value(&mut children, &mut count, read_value(record, 10));
                push_value(&mut children, &mut count, read_value(record, 19));
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
    let Some(fields) = field7(bytes, at) else {
        return;
    };
    if fields[0] != Tag::Object as u8 {
        return;
    }
    let index = u32::from_le_bytes([fields[1], fields[2], fields[3], fields[4]]);
    let generation = u32::from(u16::from_le_bytes([fields[5], fields[6]]));
    push_handle(children, count, Handle::new(index, generation));
}

const fn holds_reference(value: &Value) -> bool {
    matches!(
        value.tag(),
        Tag::String | Tag::Symbol | Tag::BigInt | Tag::Object
    )
}

/// Fixed-width reads, each checked once, so no access can fail at run time.
fn field4(bytes: &[u8], at: usize) -> Option<[u8; 4]> {
    <[u8; 4]>::try_from(bytes.get(at..at + 4)?).ok()
}

fn field7(bytes: &[u8], at: usize) -> Option<[u8; 7]> {
    <[u8; 7]>::try_from(bytes.get(at..at + 7)?).ok()
}

fn field8(bytes: &[u8], at: usize) -> Option<[u8; 8]> {
    <[u8; 8]>::try_from(bytes.get(at..at + 8)?).ok()
}

fn field9(bytes: &[u8], at: usize) -> Option<[u8; 9]> {
    <[u8; 9]>::try_from(bytes.get(at..at + 9)?).ok()
}

/// Decode a value stored as a tag byte and eight payload bytes.
fn read_value(bytes: &[u8], at: usize) -> Value {
    let Some(fields) = field9(bytes, at) else {
        return Value::UNDEFINED;
    };
    let payload = u64::from_le_bytes([
        fields[1], fields[2], fields[3], fields[4], fields[5], fields[6], fields[7], fields[8],
    ]);
    let handle = Handle::new((payload & 0xFFFF_FFFF) as u32, (payload >> 32) as u32);
    match fields[0] {
        1 => Value::NULL,
        2 => Value::boolean(payload != 0),
        3 => Value::number(f64::from_bits(payload)),
        4 => Value::string(handle),
        5 => Value::symbol(handle),
        6 => Value::big_int(handle),
        7 => Value::object(handle),
        _ => Value::UNDEFINED,
    }
}
