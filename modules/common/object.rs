//! Ordinary objects: properties, descriptors, and the prototype chain.
//!
//! An object cell holds its prototype, its extensibility, and a handle to a
//! property table. The table is a flat array of fixed-width records, so a
//! lookup is a scan and growth is a new table rather than a new object: an
//! object's identity is its handle and never changes.
//!
//! Property access that reaches an accessor stops here and reports the accessor
//! to the caller, because calling it needs an interpreter. Everything that does
//! not need to call anything, which is all of the data-property behaviour, is
//! implemented.

use crate::heap::{CellKind, Heap, HeapError};
use crate::string::Key;
use crate::value::{Handle, Tag, Value};

/// Property attributes, as the specification names them.
pub mod attribute {
    pub const WRITABLE: u8 = 1 << 0;
    pub const ENUMERABLE: u8 = 1 << 1;
    pub const CONFIGURABLE: u8 = 1 << 2;
    /// The default for a property created by an assignment.
    pub const DEFAULT: u8 = WRITABLE | ENUMERABLE | CONFIGURABLE;
}

/// Whether a property holds a value or a pair of accessors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DescriptorKind {
    Data,
    Accessor,
}

/// One property's full description.
#[derive(Clone, Copy, Debug)]
pub struct Descriptor {
    pub kind: DescriptorKind,
    pub attributes: u8,
    pub value: Value,
    pub getter: Value,
    pub setter: Value,
}

impl Descriptor {
    /// A data property.
    pub const fn data(value: Value, attributes: u8) -> Self {
        Self {
            kind: DescriptorKind::Data,
            attributes,
            value,
            getter: Value::UNDEFINED,
            setter: Value::UNDEFINED,
        }
    }

    /// An accessor property.
    pub const fn accessor(getter: Value, setter: Value, attributes: u8) -> Self {
        Self {
            kind: DescriptorKind::Accessor,
            attributes,
            value: Value::UNDEFINED,
            getter,
            setter,
        }
    }

    pub const fn has(&self, attribute: u8) -> bool {
        self.attributes & attribute != 0
    }
}

/// Bytes in one property record.
const RECORD: usize = 32;
/// Bytes in an object cell before its fields.
///
/// The layout is the prototype, the extensibility flag, the property table
/// handle, and the internal slots a callable object needs.
const OBJECT_SIZE: usize = 56;
/// Bytes in a property table before its records.
const TABLE_HEADER: usize = 8;
/// Records a new table holds.
const INITIAL_CAPACITY: u32 = 4;
/// The longest prototype chain a lookup follows before reporting a cycle.
pub const MAX_PROTOTYPE_DEPTH: u32 = 256;

/// Why an object operation did not happen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectError {
    Heap(HeapError),
    /// The prototype chain is longer than the admitted depth, which a cycle
    /// would also produce.
    PrototypeChainTooDeep,
    /// More keys than the caller's storage admits. Reporting this rather than
    /// writing what fits is the difference between a bounded failure and a
    /// wrong answer.
    TooManyKeys,
}

impl From<HeapError> for ObjectError {
    fn from(error: HeapError) -> Self {
        Self::Heap(error)
    }
}

/// Create an ordinary object with `prototype`, which is an object or null.
pub fn create(heap: &mut Heap<'_>, prototype: Value) -> Result<Handle, ObjectError> {
    let handle = heap.allocate(CellKind::Object, u32::try_from(OBJECT_SIZE).unwrap_or(0))?;
    let cell = heap.cell_mut(handle)?;
    write_value(&mut cell[0..9], prototype);
    cell[9] = 1; // extensible
    cell[12..16].copy_from_slice(&u32::MAX.to_le_bytes()); // no table yet
    cell[16..20].copy_from_slice(&0u32.to_le_bytes());
    cell[20] = INTERNAL_PLAIN;
    Ok(handle)
}

/// The internal-slot kinds an object may carry.
const INTERNAL_PLAIN: u8 = 0;
const INTERNAL_FUNCTION: u8 = 1;
const INTERNAL_NATIVE: u8 = 2;
const INTERNAL_PROMISE: u8 = 3;
/// An object wrapping a primitive: what `new String('a')` produces, and what a
/// method called on a primitive is given as its receiver.
const INTERNAL_WRAPPER: u8 = 4;
/// An iterator over something the engine iterates itself: an array's elements
/// or a string's code points.
const INTERNAL_ITERATOR: u8 = 5;
/// A regular expression, carrying the program its pattern compiled to.
const INTERNAL_REGEXP: u8 = 6;

/// Function flags.
pub mod function_flag {
    /// The function may be used with `new`.
    pub const CONSTRUCTOR: u8 = 1 << 0;
    /// The function's body is strict.
    pub const STRICT: u8 = 1 << 1;
}

/// Create a function: an ordinary object that also carries the code it runs and
/// the environment it closed over.
pub fn create_function(
    heap: &mut Heap<'_>,
    prototype: Value,
    code: u32,
    environment: Value,
    flags: u8,
) -> Result<Handle, ObjectError> {
    create_function_in(heap, prototype, code, environment, flags, 0)
}

/// Create a function that belongs to a module, which is what says which unit
/// its code is in.
pub fn create_function_in(
    heap: &mut Heap<'_>,
    prototype: Value,
    code: u32,
    environment: Value,
    flags: u8,
    module: u32,
) -> Result<Handle, ObjectError> {
    let handle = create(heap, prototype)?;
    let cell = heap.cell_mut(handle)?;
    cell[20] = INTERNAL_FUNCTION;
    cell[21] = flags;
    cell[24..28].copy_from_slice(&code.to_le_bytes());
    write_value(&mut cell[28..37], environment);
    cell[44..48].copy_from_slice(&module.to_le_bytes());
    Ok(handle)
}

/// The module a function belongs to.
pub fn function_module(heap: &Heap<'_>, object: Handle) -> Result<u32, ObjectError> {
    let cell = object_cell(heap, object)?;
    if cell[20] != INTERNAL_FUNCTION {
        return Ok(0);
    }
    Ok(u32::from_le_bytes([cell[44], cell[45], cell[46], cell[47]]))
}

/// Create an object wrapping a primitive value.
pub fn create_wrapper(
    heap: &mut Heap<'_>,
    prototype: Value,
    value: Value,
) -> Result<Handle, ObjectError> {
    let handle = create(heap, prototype)?;
    let cell = heap.cell_mut(handle)?;
    cell[20] = INTERNAL_WRAPPER;
    write_value(&mut cell[28..37], value);
    Ok(handle)
}

/// The primitive a wrapper holds, if it is one.
pub fn wrapper_value(heap: &Heap<'_>, object: Handle) -> Result<Option<Value>, ObjectError> {
    let cell = object_cell(heap, object)?;
    if cell[20] != INTERNAL_WRAPPER {
        return Ok(None);
    }
    Ok(Some(read_value(&cell[28..37])))
}

/// Create an iterator over `target`, starting at its first entry.
///
/// The kind says what each step produces: a value, a key, or both.
pub fn create_iterator(
    heap: &mut Heap<'_>,
    prototype: Value,
    target: Value,
    kind: u8,
) -> Result<Handle, ObjectError> {
    let handle = create(heap, prototype)?;
    let cell = heap.cell_mut(handle)?;
    cell[20] = INTERNAL_ITERATOR;
    cell[21] = kind;
    write_value(&mut cell[28..37], target);
    cell[44..48].copy_from_slice(&0u32.to_le_bytes());
    Ok(handle)
}

/// What an iterator is walking, where it has reached, and what it produces.
pub fn iterator_state(
    heap: &Heap<'_>,
    object: Handle,
) -> Result<Option<(Value, u32, u8)>, ObjectError> {
    let cell = object_cell(heap, object)?;
    if cell[20] != INTERNAL_ITERATOR {
        return Ok(None);
    }
    let index = u32::from_le_bytes([cell[44], cell[45], cell[46], cell[47]]);
    Ok(Some((read_value(&cell[28..37]), index, cell[21])))
}

/// Record where an iterator has reached.
pub fn set_iterator_index(
    heap: &mut Heap<'_>,
    object: Handle,
    index: u32,
) -> Result<(), ObjectError> {
    let cell = heap.cell_mut(object)?;
    if cell[20] != INTERNAL_ITERATOR {
        return Ok(());
    }
    cell[44..48].copy_from_slice(&index.to_le_bytes());
    Ok(())
}

/// Create a regular expression, carrying the compiled program its pattern
/// became and the flags it was written with.
pub fn create_regexp(
    heap: &mut Heap<'_>,
    prototype: Value,
    program: Value,
    flags: u8,
) -> Result<Handle, ObjectError> {
    let handle = create(heap, prototype)?;
    let cell = heap.cell_mut(handle)?;
    cell[20] = INTERNAL_REGEXP;
    cell[21] = flags;
    write_value(&mut cell[28..37], program);
    Ok(handle)
}

/// The program and flags a regular expression carries, if it is one.
pub fn regexp_program(heap: &Heap<'_>, object: Handle) -> Result<Option<(Value, u8)>, ObjectError> {
    let cell = object_cell(heap, object)?;
    if cell[20] != INTERNAL_REGEXP {
        return Ok(None);
    }
    Ok(Some((read_value(&cell[28..37]), cell[21])))
}

/// Create a function the engine implements itself rather than one made of
/// bytecode. The code field names which one.
pub fn create_native(
    heap: &mut Heap<'_>,
    prototype: Value,
    native: u32,
    flags: u8,
) -> Result<Handle, ObjectError> {
    let handle = create(heap, prototype)?;
    let cell = heap.cell_mut(handle)?;
    cell[20] = INTERNAL_NATIVE;
    cell[21] = flags;
    cell[24..28].copy_from_slice(&native.to_le_bytes());
    write_value(&mut cell[28..37], Value::UNDEFINED);
    Ok(handle)
}

/// Whether the object can be called.
pub fn is_callable(heap: &Heap<'_>, object: Handle) -> Result<bool, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(cell[20] == INTERNAL_FUNCTION || cell[20] == INTERNAL_NATIVE)
}

/// Whether the object is a function the engine implements itself.
pub fn is_native(heap: &Heap<'_>, object: Handle) -> Result<bool, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(cell[20] == INTERNAL_NATIVE)
}

/// Whether the object can be used with `new`.
pub fn is_constructor(heap: &Heap<'_>, object: Handle) -> Result<bool, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(cell[20] != INTERNAL_PLAIN && cell[21] & function_flag::CONSTRUCTOR != 0)
}

/// The function's flags.
pub fn function_flags(heap: &Heap<'_>, object: Handle) -> Result<u8, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(cell[21])
}

/// The index of the function's code in its unit.
pub fn function_code(heap: &Heap<'_>, object: Handle) -> Result<u32, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(u32::from_le_bytes([cell[24], cell[25], cell[26], cell[27]]))
}

/// The environment the function closed over.
///
/// A function the engine implements itself has no environment; the same slot
/// carries whatever that function was bound to, such as the promise a resolve
/// function settles.
pub fn function_environment(heap: &Heap<'_>, object: Handle) -> Result<Value, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(read_value(&cell[28..37]))
}

/// Give a native function the value it is bound to.
pub fn set_bound_value(
    heap: &mut Heap<'_>,
    object: Handle,
    value: Value,
) -> Result<(), ObjectError> {
    let cell = heap.cell_mut(object)?;
    write_value(&mut cell[28..37], value);
    Ok(())
}

/// Mark an object as a promise, which gives it a state, a settled value, and a
/// list of reactions waiting on it.
pub fn make_promise(heap: &mut Heap<'_>, object: Handle) -> Result<(), ObjectError> {
    let cell = heap.cell_mut(object)?;
    cell[20] = INTERNAL_PROMISE;
    cell[21] = 0;
    write_value(&mut cell[28..37], Value::UNDEFINED);
    cell[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
    cell[44..48].copy_from_slice(&0u32.to_le_bytes());
    Ok(())
}

/// Whether the object is a promise.
pub fn is_promise(heap: &Heap<'_>, object: Handle) -> Result<bool, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(cell[20] == INTERNAL_PROMISE)
}

/// A promise's state: zero pending, one fulfilled, two rejected.
pub fn promise_state(heap: &Heap<'_>, object: Handle) -> Result<u8, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(cell[21])
}

/// A promise's settled value, which is undefined while it is pending.
pub fn promise_value(heap: &Heap<'_>, object: Handle) -> Result<Value, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(read_value(&cell[28..37]))
}

/// Settle a promise. A promise settles once: a later attempt is ignored, which
/// is what the specification's already-resolved flag does.
pub fn settle_promise(
    heap: &mut Heap<'_>,
    object: Handle,
    state: u8,
    value: Value,
) -> Result<bool, ObjectError> {
    if promise_state(heap, object)? != 0 {
        return Ok(false);
    }
    let cell = heap.cell_mut(object)?;
    cell[21] = state;
    write_value(&mut cell[28..37], value);
    Ok(true)
}

/// The cell holding the reactions waiting on a promise, if it has one.
pub fn promise_reactions(heap: &Heap<'_>, object: Handle) -> Result<Option<Handle>, ObjectError> {
    let cell = object_cell(heap, object)?;
    let index = u32::from_le_bytes([cell[40], cell[41], cell[42], cell[43]]);
    if index == u32::MAX {
        return Ok(None);
    }
    let generation = u32::from_le_bytes([cell[44], cell[45], cell[46], cell[47]]);
    Ok(Some(Handle::new(index, generation)))
}

/// Give a promise its reaction list.
pub fn set_promise_reactions(
    heap: &mut Heap<'_>,
    object: Handle,
    reactions: Handle,
) -> Result<(), ObjectError> {
    let cell = heap.cell_mut(object)?;
    cell[40..44].copy_from_slice(&reactions.index.to_le_bytes());
    cell[44..48].copy_from_slice(&reactions.generation.to_le_bytes());
    Ok(())
}

/// The object's prototype, which is an object value or null.
pub fn prototype(heap: &Heap<'_>, object: Handle) -> Result<Value, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(read_value(&cell[0..9]))
}

/// Set the prototype. A non-extensible object keeps the one it has, which the
/// caller observes as a false result.
pub fn set_prototype(
    heap: &mut Heap<'_>,
    object: Handle,
    value: Value,
) -> Result<bool, ObjectError> {
    let current = prototype(heap, object)?;
    if same_reference(&current, &value) {
        return Ok(true);
    }
    if !is_extensible(heap, object)? {
        return Ok(false);
    }
    // A prototype may not be made to reach the object itself.
    if value.is_object() {
        let mut walker = value;
        let mut depth = 0u32;
        while walker.is_object() {
            if walker.as_handle() == object {
                return Ok(false);
            }
            depth += 1;
            if depth > MAX_PROTOTYPE_DEPTH {
                return Err(ObjectError::PrototypeChainTooDeep);
            }
            walker = prototype(heap, walker.as_handle())?;
        }
    }
    let cell = heap.cell_mut(object)?;
    write_value(&mut cell[0..9], value);
    Ok(true)
}

pub fn is_extensible(heap: &Heap<'_>, object: Handle) -> Result<bool, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(cell[9] != 0)
}

pub fn prevent_extensions(heap: &mut Heap<'_>, object: Handle) -> Result<(), ObjectError> {
    let cell = heap.cell_mut(object)?;
    cell[9] = 0;
    Ok(())
}

/// The object's own property for `key`.
pub fn get_own_property(
    heap: &Heap<'_>,
    object: Handle,
    key: Key,
) -> Result<Option<Descriptor>, ObjectError> {
    let Some((table, index)) = find(heap, object, key)? else {
        return Ok(None);
    };
    let cell = heap.cell(table)?;
    Ok(Some(read_record(cell, index)?))
}

/// Define or redefine an own property, following the ordinary validation rules.
///
/// The result is whether the definition was admitted; a rejected definition
/// leaves the object unchanged, and the caller decides whether that is a thrown
/// type error or a false result.
pub fn define_own_property(
    heap: &mut Heap<'_>,
    object: Handle,
    key: Key,
    descriptor: Descriptor,
) -> Result<bool, ObjectError> {
    if let Some((table, index)) = find(heap, object, key)? {
        let current = read_record(heap.cell(table)?, index)?;
        if !is_compatible(&current, &descriptor) {
            return Ok(false);
        }
        let merged = merge(&current, &descriptor);
        let cell = heap.cell_mut(table)?;
        write_record(cell, index, key, &merged)?;
        return Ok(true);
    }

    if !is_extensible(heap, object)? {
        return Ok(false);
    }
    append(heap, object, key, &descriptor)?;
    Ok(true)
}

/// What a property read produced.
#[derive(Clone, Copy, Debug)]
pub enum Lookup {
    /// The property is absent from the object and its prototypes.
    Absent,
    /// A data property's value.
    Value(Value),
    /// An accessor property, which the caller must call with the receiver.
    Accessor(Value),
}

/// Read `key` from `object`, following the prototype chain.
pub fn get(heap: &Heap<'_>, object: Handle, key: Key) -> Result<Lookup, ObjectError> {
    let mut current = object;
    let mut depth = 0u32;
    loop {
        if let Some(descriptor) = get_own_property(heap, current, key)? {
            return Ok(match descriptor.kind {
                DescriptorKind::Data => Lookup::Value(descriptor.value),
                DescriptorKind::Accessor => Lookup::Accessor(descriptor.getter),
            });
        }
        let parent = prototype(heap, current)?;
        if !parent.is_object() {
            return Ok(Lookup::Absent);
        }
        depth += 1;
        if depth > MAX_PROTOTYPE_DEPTH {
            return Err(ObjectError::PrototypeChainTooDeep);
        }
        current = parent.as_handle();
    }
}

/// Whether `key` is present on the object or its prototypes.
pub fn has_property(heap: &Heap<'_>, object: Handle, key: Key) -> Result<bool, ObjectError> {
    Ok(!matches!(get(heap, object, key)?, Lookup::Absent))
}

/// What a property write requires.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Assignment {
    /// The write happened.
    Done,
    /// The write was refused: a non-writable property, or a non-extensible
    /// object.
    Refused,
    /// A setter must be called with the receiver and the value.
    Setter(Handle),
}

/// Write `key` on `object`, following the prototype chain for accessors and for
/// a non-writable inherited property, as ordinary assignment does.
pub fn set(
    heap: &mut Heap<'_>,
    object: Handle,
    key: Key,
    value: Value,
) -> Result<Assignment, ObjectError> {
    // An own data property is written in place.
    if let Some(descriptor) = get_own_property(heap, object, key)? {
        match descriptor.kind {
            DescriptorKind::Data => {
                if !descriptor.has(attribute::WRITABLE) {
                    return Ok(Assignment::Refused);
                }
                let Some((table, index)) = find(heap, object, key)? else {
                    return Ok(Assignment::Refused);
                };
                let updated = Descriptor {
                    value,
                    ..descriptor
                };
                let cell = heap.cell_mut(table)?;
                write_record(cell, index, key, &updated)?;
                return Ok(Assignment::Done);
            }
            DescriptorKind::Accessor => {
                if descriptor.setter.is_object() {
                    return Ok(Assignment::Setter(descriptor.setter.as_handle()));
                }
                return Ok(Assignment::Refused);
            }
        }
    }

    // An inherited property decides whether a new own property may appear.
    let mut current = prototype(heap, object)?;
    let mut depth = 0u32;
    while current.is_object() {
        if let Some(descriptor) = get_own_property(heap, current.as_handle(), key)? {
            match descriptor.kind {
                DescriptorKind::Data => {
                    if !descriptor.has(attribute::WRITABLE) {
                        return Ok(Assignment::Refused);
                    }
                    break;
                }
                DescriptorKind::Accessor => {
                    if descriptor.setter.is_object() {
                        return Ok(Assignment::Setter(descriptor.setter.as_handle()));
                    }
                    return Ok(Assignment::Refused);
                }
            }
        }
        depth += 1;
        if depth > MAX_PROTOTYPE_DEPTH {
            return Err(ObjectError::PrototypeChainTooDeep);
        }
        current = prototype(heap, current.as_handle())?;
    }

    if !is_extensible(heap, object)? {
        return Ok(Assignment::Refused);
    }
    append(
        heap,
        object,
        key,
        &Descriptor::data(value, attribute::DEFAULT),
    )?;
    Ok(Assignment::Done)
}

/// Delete an own property. A non-configurable property stays.
pub fn delete(heap: &mut Heap<'_>, object: Handle, key: Key) -> Result<bool, ObjectError> {
    let Some((table, index)) = find(heap, object, key)? else {
        return Ok(true);
    };
    let descriptor = read_record(heap.cell(table)?, index)?;
    if !descriptor.has(attribute::CONFIGURABLE) {
        return Ok(false);
    }
    let count = table_count(heap, table)?;
    let cell = heap.cell_mut(table)?;
    // Keep insertion order by moving the later records down.
    let mut cursor = index;
    while cursor + 1 < count {
        let (from, to) = (record_at(cursor + 1), record_at(cursor));
        let mut buffer = [0u8; RECORD];
        buffer.copy_from_slice(cell.get(from..from + RECORD).unwrap_or(&[0; RECORD]));
        if let Some(target) = cell.get_mut(to..to + RECORD) {
            target.copy_from_slice(&buffer);
        }
        cursor += 1;
    }
    cell[0..4].copy_from_slice(&(count - 1).to_le_bytes());
    Ok(true)
}

/// The object's own keys, in the order the specification requires: integer
/// indices in ascending order, then names and symbols in the order they were
/// added.
pub fn own_keys(heap: &Heap<'_>, object: Handle, out: &mut [Key]) -> Result<usize, ObjectError> {
    let Some(table) = table_of(heap, object)? else {
        return Ok(0);
    };
    let count = table_count(heap, table)?;
    let cell = heap.cell(table)?;
    let mut written = 0usize;

    // Indices first, ascending.
    let mut smallest_written: Option<u32> = None;
    loop {
        let mut next: Option<u32> = None;
        let mut index = 0u32;
        while index < count {
            if let Key::Index(value) = read_key(cell, index)? {
                let after = match smallest_written {
                    Some(previous) => value > previous,
                    None => true,
                };
                let better = match next {
                    Some(candidate) => value < candidate,
                    None => true,
                };
                if after && better {
                    next = Some(value);
                }
            }
            index += 1;
        }
        let Some(value) = next else {
            break;
        };
        let Some(slot) = out.get_mut(written) else {
            return Err(ObjectError::TooManyKeys);
        };
        *slot = Key::Index(value);
        written += 1;
        smallest_written = Some(value);
    }

    // Then names, then symbols, each in insertion order.
    for wanted_symbol in [false, true] {
        let mut index = 0u32;
        while index < count {
            let key = read_key(cell, index)?;
            let is_symbol = matches!(key, Key::Symbol(_));
            if !matches!(key, Key::Index(_)) && is_symbol == wanted_symbol {
                let Some(slot) = out.get_mut(written) else {
                    return Err(ObjectError::TooManyKeys);
                };
                *slot = key;
                written += 1;
            }
            index += 1;
        }
    }
    Ok(written)
}

/// The number of own properties.
pub fn own_property_count(heap: &Heap<'_>, object: Handle) -> Result<u32, ObjectError> {
    match table_of(heap, object)? {
        Some(table) => table_count(heap, table),
        None => Ok(0),
    }
}

// Storage.

fn object_cell<'h>(heap: &'h Heap<'_>, object: Handle) -> Result<&'h [u8], ObjectError> {
    if heap.kind(object)? != CellKind::Object {
        return Err(ObjectError::Heap(HeapError::StaleHandle));
    }
    let cell = heap.cell(object)?;
    if cell.len() < OBJECT_SIZE {
        return Err(ObjectError::Heap(HeapError::StaleHandle));
    }
    Ok(cell)
}

fn table_of(heap: &Heap<'_>, object: Handle) -> Result<Option<Handle>, ObjectError> {
    let cell = object_cell(heap, object)?;
    let index = u32::from_le_bytes([cell[12], cell[13], cell[14], cell[15]]);
    if index == u32::MAX {
        return Ok(None);
    }
    let generation = u32::from_le_bytes([cell[16], cell[17], cell[18], cell[19]]);
    Ok(Some(Handle::new(index, generation)))
}

fn set_table(heap: &mut Heap<'_>, object: Handle, table: Handle) -> Result<(), ObjectError> {
    let cell = heap.cell_mut(object)?;
    cell[12..16].copy_from_slice(&table.index.to_le_bytes());
    cell[16..20].copy_from_slice(&table.generation.to_le_bytes());
    Ok(())
}

fn table_count(heap: &Heap<'_>, table: Handle) -> Result<u32, ObjectError> {
    let cell = heap.cell(table)?;
    Ok(u32::from_le_bytes([cell[0], cell[1], cell[2], cell[3]]))
}

fn table_capacity(heap: &Heap<'_>, table: Handle) -> Result<u32, ObjectError> {
    let cell = heap.cell(table)?;
    Ok(u32::from_le_bytes([cell[4], cell[5], cell[6], cell[7]]))
}

const fn record_at(index: u32) -> usize {
    TABLE_HEADER + index as usize * RECORD
}

/// Find the record index of an own property.
fn find(heap: &Heap<'_>, object: Handle, key: Key) -> Result<Option<(Handle, u32)>, ObjectError> {
    let Some(table) = table_of(heap, object)? else {
        return Ok(None);
    };
    let count = table_count(heap, table)?;
    let cell = heap.cell(table)?;
    let mut index = 0u32;
    while index < count {
        if same_key(read_key(cell, index)?, key) {
            return Ok(Some((table, index)));
        }
        index += 1;
    }
    Ok(None)
}

/// Append a property, growing the table when it is full.
fn append(
    heap: &mut Heap<'_>,
    object: Handle,
    key: Key,
    descriptor: &Descriptor,
) -> Result<(), ObjectError> {
    let table = match table_of(heap, object)? {
        Some(table) => {
            let count = table_count(heap, table)?;
            let capacity = table_capacity(heap, table)?;
            if count == capacity {
                grow(heap, object, table, capacity)?
            } else {
                table
            }
        }
        None => {
            let table = allocate_table(heap, INITIAL_CAPACITY)?;
            set_table(heap, object, table)?;
            table
        }
    };

    let count = table_count(heap, table)?;
    let cell = heap.cell_mut(table)?;
    write_record(cell, count, key, descriptor)?;
    cell[0..4].copy_from_slice(&(count + 1).to_le_bytes());
    Ok(())
}

/// Make room for `count` properties before any are added.
///
/// A table that grows by doubling leaves the tables it outgrew behind, and an
/// object whose whole shape is known when it is built — an intrinsic, say —
/// need not pay for that.
pub fn reserve(heap: &mut Heap<'_>, object: Handle, count: u32) -> Result<(), ObjectError> {
    if count == 0 {
        return Ok(());
    }
    match table_of(heap, object)? {
        Some(table) => {
            let capacity = table_capacity(heap, table)?;
            let used = table_count(heap, table)?;
            if capacity >= used + count {
                return Ok(());
            }
            grow(heap, object, table, (used + count).saturating_sub(1))?;
            Ok(())
        }
        None => {
            let table = allocate_table(heap, count)?;
            set_table(heap, object, table)?;
            Ok(())
        }
    }
}

fn allocate_table(heap: &mut Heap<'_>, capacity: u32) -> Result<Handle, ObjectError> {
    let size = u32::try_from(TABLE_HEADER + capacity as usize * RECORD)
        .map_err(|_| ObjectError::Heap(HeapError::ArenaFull))?;
    let table = heap.allocate(CellKind::PropertyTable, size)?;
    let cell = heap.cell_mut(table)?;
    cell[0..4].copy_from_slice(&0u32.to_le_bytes());
    cell[4..8].copy_from_slice(&capacity.to_le_bytes());
    Ok(table)
}

fn grow(
    heap: &mut Heap<'_>,
    object: Handle,
    table: Handle,
    capacity: u32,
) -> Result<Handle, ObjectError> {
    let count = table_count(heap, table)?;
    let larger = allocate_table(heap, capacity.saturating_mul(2).max(INITIAL_CAPACITY))?;
    let mut index = 0u32;
    while index < count {
        let mut buffer = [0u8; RECORD];
        {
            let cell = heap.cell(table)?;
            let at = record_at(index);
            buffer.copy_from_slice(cell.get(at..at + RECORD).unwrap_or(&[0; RECORD]));
        }
        let cell = heap.cell_mut(larger)?;
        let at = record_at(index);
        if let Some(target) = cell.get_mut(at..at + RECORD) {
            target.copy_from_slice(&buffer);
        }
        index += 1;
    }
    let cell = heap.cell_mut(larger)?;
    cell[0..4].copy_from_slice(&count.to_le_bytes());
    set_table(heap, object, larger)?;
    // The old table is unreachable now; collection will reclaim it.
    Ok(larger)
}

fn read_key(cell: &[u8], index: u32) -> Result<Key, ObjectError> {
    let at = record_at(index);
    let record = cell
        .get(at..at + RECORD)
        .ok_or(ObjectError::Heap(HeapError::StaleHandle))?;
    let payload = u64::from_le_bytes([
        record[8], record[9], record[10], record[11], record[12], record[13], record[14],
        record[15],
    ]);
    Ok(match record[0] {
        0 => Key::Index((payload & 0xFFFF_FFFF) as u32),
        1 => Key::Name(Handle::new(
            (payload & 0xFFFF_FFFF) as u32,
            (payload >> 32) as u32,
        )),
        _ => Key::Symbol(Handle::new(
            (payload & 0xFFFF_FFFF) as u32,
            (payload >> 32) as u32,
        )),
    })
}

fn read_record(cell: &[u8], index: u32) -> Result<Descriptor, ObjectError> {
    let at = record_at(index);
    let record = cell
        .get(at..at + RECORD)
        .ok_or(ObjectError::Heap(HeapError::StaleHandle))?;
    let kind = if record[1] == 0 {
        DescriptorKind::Data
    } else {
        DescriptorKind::Accessor
    };
    let first = read_value(&record[16..25]);
    // The second value shares the record with the first: a data property uses
    // one slot, an accessor uses both.
    let second = read_value_short(&record[25..32]);
    Ok(match kind {
        DescriptorKind::Data => Descriptor {
            kind,
            attributes: record[2],
            value: first,
            getter: Value::UNDEFINED,
            setter: Value::UNDEFINED,
        },
        DescriptorKind::Accessor => Descriptor {
            kind,
            attributes: record[2],
            value: Value::UNDEFINED,
            getter: first,
            setter: second,
        },
    })
}

fn write_record(
    cell: &mut [u8],
    index: u32,
    key: Key,
    descriptor: &Descriptor,
) -> Result<(), ObjectError> {
    let at = record_at(index);
    let record = cell
        .get_mut(at..at + RECORD)
        .ok_or(ObjectError::Heap(HeapError::ArenaFull))?;
    let (kind_byte, payload) = match key {
        Key::Index(value) => (0u8, u64::from(value)),
        Key::Name(handle) => (
            1u8,
            (u64::from(handle.generation) << 32) | u64::from(handle.index),
        ),
        Key::Symbol(handle) => (
            2u8,
            (u64::from(handle.generation) << 32) | u64::from(handle.index),
        ),
    };
    record[0] = kind_byte;
    record[1] = u8::from(matches!(descriptor.kind, DescriptorKind::Accessor));
    record[2] = descriptor.attributes;
    record[3] = 0;
    record[8..16].copy_from_slice(&payload.to_le_bytes());
    match descriptor.kind {
        DescriptorKind::Data => {
            write_value(&mut record[16..25], descriptor.value);
            write_value_short(&mut record[25..32], Value::UNDEFINED);
        }
        DescriptorKind::Accessor => {
            write_value(&mut record[16..25], descriptor.getter);
            write_value_short(&mut record[25..32], descriptor.setter);
        }
    }
    Ok(())
}

/// Encode a value as a tag byte and eight payload bytes.
fn write_value(out: &mut [u8], value: Value) {
    out[0] = value.tag() as u8;
    let payload = payload_of(&value);
    out[1..9].copy_from_slice(&payload.to_le_bytes());
}

fn read_value(bytes: &[u8]) -> Value {
    let payload = u64::from_le_bytes([
        bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7], bytes[8],
    ]);
    value_from(bytes[0], payload)
}

/// Encode a reference value in seven bytes: a tag and a handle.
///
/// Only a reference or a simple value can be stored here, which is all an
/// accessor slot ever holds.
fn write_value_short(out: &mut [u8], value: Value) {
    out[0] = value.tag() as u8;
    let handle = value.as_handle();
    out[1..5].copy_from_slice(&handle.index.to_le_bytes());
    out[5..7].copy_from_slice(&(handle.generation as u16).to_le_bytes());
}

fn read_value_short(bytes: &[u8]) -> Value {
    let index = u32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
    let generation = u32::from(u16::from_le_bytes([bytes[5], bytes[6]]));
    match bytes[0] {
        7 => Value::object(Handle::new(index, generation)),
        _ => Value::UNDEFINED,
    }
}

fn payload_of(value: &Value) -> u64 {
    match value.tag() {
        Tag::Number => value.as_number().to_bits(),
        Tag::Boolean => u64::from(value.as_boolean()),
        Tag::Undefined | Tag::Null => 0,
        _ => {
            let handle = value.as_handle();
            (u64::from(handle.generation) << 32) | u64::from(handle.index)
        }
    }
}

fn value_from(tag: u8, payload: u64) -> Value {
    let handle = Handle::new((payload & 0xFFFF_FFFF) as u32, (payload >> 32) as u32);
    match tag {
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

fn same_key(left: Key, right: Key) -> bool {
    match (left, right) {
        (Key::Index(a), Key::Index(b)) => a == b,
        (Key::Name(a), Key::Name(b)) | (Key::Symbol(a), Key::Symbol(b)) => a == b,
        _ => false,
    }
}

fn same_reference(left: &Value, right: &Value) -> bool {
    if left.tag() != right.tag() {
        return false;
    }
    match left.tag() {
        Tag::Null | Tag::Undefined => true,
        _ => left.as_handle() == right.as_handle(),
    }
}

/// Whether a redefinition is admitted, which is the specification's validation
/// of a descriptor against the one already present.
fn is_compatible(current: &Descriptor, proposed: &Descriptor) -> bool {
    if current.has(attribute::CONFIGURABLE) {
        return true;
    }
    // A non-configurable property may not become configurable or change
    // enumerability, and may not change kind.
    if proposed.has(attribute::CONFIGURABLE) {
        return false;
    }
    if current.has(attribute::ENUMERABLE) != proposed.has(attribute::ENUMERABLE) {
        return false;
    }
    if current.kind != proposed.kind {
        return false;
    }
    match current.kind {
        DescriptorKind::Data => {
            if current.has(attribute::WRITABLE) {
                return true;
            }
            // A non-writable, non-configurable property keeps its value.
            !proposed.has(attribute::WRITABLE)
                && crate::value::strict_equals(&current.value, &proposed.value) == Some(true)
        }
        DescriptorKind::Accessor => {
            same_reference(&current.getter, &proposed.getter)
                && same_reference(&current.setter, &proposed.setter)
        }
    }
}

fn merge(current: &Descriptor, proposed: &Descriptor) -> Descriptor {
    let _ = current;
    *proposed
}
