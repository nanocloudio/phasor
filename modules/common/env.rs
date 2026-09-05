//! Lexical environments.
//!
//! An environment record is a heap cell holding a parent, a kind, and a flat
//! array of bindings. A binding has a name, a mutability, and an initialisation
//! flag, so a read before initialisation is a distinct outcome from a read of a
//! missing name, which is what the temporal dead zone requires.
//!
//! A global environment holds no bindings of its own: it names an object whose
//! properties are the bindings, which is how a global reference reaches a
//! property of the global object.

use crate::heap::{CellKind, Heap, HeapError};
use crate::value::{Handle, Tag, Value};

/// What kind of environment a record is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum EnvironmentKind {
    /// Bindings declared by `let`, `const`, a function's parameters, or a
    /// catch clause.
    Declarative = 0,
    /// A function's top-level environment, which also carries `this`.
    Function = 1,
    /// The environment whose bindings are properties of an object.
    Object = 2,
    /// An arrow call's environment: a variable environment for the `var`s a
    /// direct eval declares, with no `this` of its own.
    Arrow = 3,
}

/// Binding flags.
pub mod binding {
    /// The binding may be assigned after it is initialised.
    pub const MUTABLE: u8 = 1 << 0;
    /// The binding has been given its value.
    pub const INITIALISED: u8 = 1 << 1;
    /// Assigning to this binding when it is not writable throws rather than
    /// failing silently.
    pub const STRICT: u8 = 1 << 2;
    /// The binding can never be deleted: a script's global lexical, which
    /// `delete` answers false for rather than removing.
    pub const PERMANENT: u8 = 1 << 3;
}

/// Why an environment operation did not happen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnvironmentError {
    Heap(HeapError),
    /// The name is not bound in this environment or any parent.
    Unresolvable,
    /// The binding exists but has not been initialised, which is the temporal
    /// dead zone.
    Uninitialised,
    /// The binding exists and may not be assigned.
    Immutable,
    /// The record has no room for another binding.
    Full,
    /// The scope chain is longer than the admitted depth.
    ChainTooDeep,
}

impl From<HeapError> for EnvironmentError {
    fn from(error: HeapError) -> Self {
        Self::Heap(error)
    }
}

/// The longest scope chain a lookup walks.
pub const MAX_SCOPE_DEPTH: u32 = 256;

/// Where each field of an environment record sits. The collector traces
/// through these names.
pub mod layout {
    /// The kind, from `EnvironmentKind`.
    pub const KIND: usize = 0;
    /// Bindings in use, a `u32`.
    pub const COUNT: usize = 4;
    /// Bindings the record has room for, a `u32`.
    pub const CAPACITY: usize = 8;
    /// The parent environment: an encoded value.
    pub const PARENT: usize = 12;
    /// A function environment's `this`, or an object environment's binding
    /// object: an encoded value.
    pub const THIS: usize = 21;
    /// Non-zero while a derived constructor's `this` is still unbound.
    pub const THIS_UNINITIALISED: usize = 30;
    /// A function environment's `new.target`: an encoded value.
    pub const NEW_TARGET: usize = 32;
    /// The function a function environment was made for: an encoded value.
    pub const FUNCTION: usize = 41;
    /// Bytes before the bindings.
    pub const HEADER: usize = 56;

    /// Bytes in one binding.
    pub const BINDING: usize = 24;
    /// Where each field of a binding sits.
    pub mod binding {
        /// The flags, from `binding`.
        pub const FLAGS: usize = 0;
        /// The name: a handle as index and generation.
        pub const NAME: usize = 4;
        /// The value: an encoded value.
        pub const VALUE: usize = 12;
    }
}

const HEADER: usize = layout::HEADER;
const BINDING: usize = layout::BINDING;

/// Create an environment record with room for `capacity` bindings.
pub fn create(
    heap: &mut Heap<'_>,
    kind: EnvironmentKind,
    parent: Value,
    capacity: u32,
) -> Result<Handle, EnvironmentError> {
    let size = u32::try_from(HEADER + capacity as usize * BINDING)
        .map_err(|_| EnvironmentError::Heap(HeapError::ArenaFull))?;
    let handle = heap.allocate(CellKind::Environment, size)?;
    let cell = heap.cell_mut(handle)?;
    cell[layout::KIND] = kind as u8;
    write_u32(cell, layout::COUNT, 0);
    write_u32(cell, layout::CAPACITY, capacity);
    parent.encode_at(cell, layout::PARENT);
    Value::UNDEFINED.encode_at(cell, layout::THIS);
    cell[layout::THIS_UNINITIALISED] = 0;
    Value::UNDEFINED.encode_at(cell, layout::NEW_TARGET);
    Value::UNDEFINED.encode_at(cell, layout::FUNCTION);
    Ok(handle)
}

/// The function a function environment was made for: what an arrow's
/// `super()` constructs through.
pub fn function(heap: &Heap<'_>, environment: Handle) -> Result<Value, EnvironmentError> {
    let cell = environment_cell(heap, environment)?;
    Ok(Value::decode_at(cell, layout::FUNCTION))
}

/// Record the function a function environment was made for.
pub fn set_function(
    heap: &mut Heap<'_>,
    environment: Handle,
    value: Value,
) -> Result<(), EnvironmentError> {
    let cell = heap.cell_mut(environment)?;
    value.encode_at(cell, layout::FUNCTION);
    Ok(())
}

/// Whether a function environment's `this` is still in its dead zone: a
/// derived constructor's, before `super()` binds it.
pub fn this_uninitialised(heap: &Heap<'_>, environment: Handle) -> Result<bool, EnvironmentError> {
    let cell = environment_cell(heap, environment)?;
    Ok(cell[layout::THIS_UNINITIALISED] != 0)
}

/// Put a function environment's `this` in its dead zone until `set_this`.
pub fn mark_this_uninitialised(
    heap: &mut Heap<'_>,
    environment: Handle,
) -> Result<(), EnvironmentError> {
    let cell = heap.cell_mut(environment)?;
    cell[layout::THIS_UNINITIALISED] = 1;
    Ok(())
}

/// Create the environment whose bindings are the properties of `object`.
pub fn create_object_environment(
    heap: &mut Heap<'_>,
    parent: Value,
    object: Handle,
) -> Result<Handle, EnvironmentError> {
    let handle = create(heap, EnvironmentKind::Object, parent, 0)?;
    let cell = heap.cell_mut(handle)?;
    Value::object(object).encode_at(cell, layout::THIS);
    Ok(handle)
}

pub fn kind(heap: &Heap<'_>, environment: Handle) -> Result<EnvironmentKind, EnvironmentError> {
    let cell = environment_cell(heap, environment)?;
    Ok(match cell[layout::KIND] {
        1 => EnvironmentKind::Function,
        2 => EnvironmentKind::Object,
        3 => EnvironmentKind::Arrow,
        _ => EnvironmentKind::Declarative,
    })
}

pub fn parent(heap: &Heap<'_>, environment: Handle) -> Result<Value, EnvironmentError> {
    let cell = environment_cell(heap, environment)?;
    Ok(Value::decode_at(cell, layout::PARENT))
}

/// The object whose properties are this environment's bindings, if it has one.
pub fn binding_object(heap: &Heap<'_>, environment: Handle) -> Result<Value, EnvironmentError> {
    let cell = environment_cell(heap, environment)?;
    Ok(Value::decode_at(cell, layout::THIS))
}

/// The `this` value a function environment carries.
pub fn this_value(heap: &Heap<'_>, environment: Handle) -> Result<Value, EnvironmentError> {
    let cell = environment_cell(heap, environment)?;
    Ok(Value::decode_at(cell, layout::THIS))
}

/// Give a function environment its `this` value.
pub fn set_this(
    heap: &mut Heap<'_>,
    environment: Handle,
    value: Value,
) -> Result<(), EnvironmentError> {
    let cell = heap.cell_mut(environment)?;
    value.encode_at(cell, layout::THIS);
    cell[layout::THIS_UNINITIALISED] = 0;
    Ok(())
}

/// The `new.target` a function environment carries.
pub fn new_target(heap: &Heap<'_>, environment: Handle) -> Result<Value, EnvironmentError> {
    let cell = environment_cell(heap, environment)?;
    Ok(Value::decode_at(cell, layout::NEW_TARGET))
}

/// Give a function environment its `new.target`.
pub fn set_new_target(
    heap: &mut Heap<'_>,
    environment: Handle,
    value: Value,
) -> Result<(), EnvironmentError> {
    let cell = heap.cell_mut(environment)?;
    value.encode_at(cell, layout::NEW_TARGET);
    Ok(())
}

/// Add an uninitialised binding.
pub fn declare(
    heap: &mut Heap<'_>,
    environment: Handle,
    name: Handle,
    flags: u8,
) -> Result<u32, EnvironmentError> {
    let count = count(heap, environment)?;
    let capacity = capacity(heap, environment)?;
    if count >= capacity {
        return Err(EnvironmentError::Full);
    }
    let cell = heap.cell_mut(environment)?;
    let at = HEADER + count as usize * BINDING;
    let record = cell
        .get_mut(at..at + BINDING)
        .ok_or(EnvironmentError::Full)?;
    record[layout::binding::FLAGS] = flags & !binding::INITIALISED;
    name.write_at(record, layout::binding::NAME);
    Value::UNDEFINED.encode_at(record, layout::binding::VALUE);
    write_u32(cell, layout::COUNT, count + 1);
    Ok(count)
}

/// Add a binding that already has its value, which is what a parameter or a
/// `var` declaration produces.
pub fn declare_initialised(
    heap: &mut Heap<'_>,
    environment: Handle,
    name: Handle,
    flags: u8,
    value: Value,
) -> Result<u32, EnvironmentError> {
    let index = declare(heap, environment, name, flags)?;
    initialise(heap, environment, index, value)?;
    Ok(index)
}

/// Give a binding its value, which is what a declaration's initialiser does.
pub fn initialise(
    heap: &mut Heap<'_>,
    environment: Handle,
    index: u32,
    value: Value,
) -> Result<(), EnvironmentError> {
    let cell = heap.cell_mut(environment)?;
    let at = HEADER + index as usize * BINDING;
    let record = cell
        .get_mut(at..at + BINDING)
        .ok_or(EnvironmentError::Unresolvable)?;
    record[layout::binding::FLAGS] |= binding::INITIALISED;
    value.encode_at(record, layout::binding::VALUE);
    Ok(())
}

/// The index of a binding in this record alone.
pub fn index_of(
    heap: &Heap<'_>,
    environment: Handle,
    name: Handle,
) -> Result<Option<u32>, EnvironmentError> {
    let count = count(heap, environment)?;
    let cell = environment_cell(heap, environment)?;
    let mut index = 0u32;
    while index < count {
        let at = HEADER + index as usize * BINDING;
        let record = cell
            .get(at..at + BINDING)
            .ok_or(EnvironmentError::Unresolvable)?;
        let slot = Handle::new(
            u32::from_le_bytes([record[4], record[5], record[6], record[7]]),
            u32::from_le_bytes([record[8], record[9], record[10], record[11]]),
        );
        if slot == name {
            return Ok(Some(index));
        }
        index += 1;
    }
    Ok(None)
}

/// The flags a binding carries.
pub fn binding_flags(
    heap: &Heap<'_>,
    environment: Handle,
    index: u32,
) -> Result<u8, EnvironmentError> {
    Ok(binding_record(heap, environment, index)?[0])
}

/// Give an environment a new parent: what threads a fresh record into the
/// global lexical chain when the head runs out of room.
pub fn set_parent(
    heap: &mut Heap<'_>,
    environment: Handle,
    parent: Value,
) -> Result<(), EnvironmentError> {
    let cell = heap.cell_mut(environment)?;
    parent.encode_at(cell, layout::PARENT);
    Ok(())
}

/// Read a binding by index in this record.
pub fn slot_value(
    heap: &Heap<'_>,
    environment: Handle,
    index: u32,
) -> Result<Value, EnvironmentError> {
    let record = binding_record(heap, environment, index)?;
    if record[layout::binding::FLAGS] & binding::INITIALISED == 0 {
        return Err(EnvironmentError::Uninitialised);
    }
    Ok(Value::decode_at(record, layout::binding::VALUE))
}

/// Write a binding by index in this record.
pub fn set_slot(
    heap: &mut Heap<'_>,
    environment: Handle,
    index: u32,
    value: Value,
) -> Result<(), EnvironmentError> {
    let flags = binding_record(heap, environment, index)?[0];
    if flags & binding::INITIALISED == 0 {
        return Err(EnvironmentError::Uninitialised);
    }
    if flags & binding::MUTABLE == 0 {
        return Err(EnvironmentError::Immutable);
    }
    let cell = heap.cell_mut(environment)?;
    let at = HEADER + index as usize * BINDING;
    let record = cell
        .get_mut(at..at + BINDING)
        .ok_or(EnvironmentError::Unresolvable)?;
    value.encode_at(record, layout::binding::VALUE);
    Ok(())
}

/// Remove a binding by index: its name is blanked so no lookup matches it
/// again. The record keeps its length, so every other index stays what it
/// was.
pub fn remove(
    heap: &mut Heap<'_>,
    environment: Handle,
    index: u32,
) -> Result<(), EnvironmentError> {
    let count = count(heap, environment)?;
    if index >= count {
        return Err(EnvironmentError::Unresolvable);
    }
    let cell = heap.cell_mut(environment)?;
    let at = HEADER + index as usize * BINDING;
    let record = cell
        .get_mut(at..at + BINDING)
        .ok_or(EnvironmentError::Unresolvable)?;
    record[layout::binding::FLAGS] = binding::MUTABLE;
    Handle::new(0, 0).write_at(record, layout::binding::NAME);
    Value::UNDEFINED.encode_at(record, layout::binding::VALUE);
    Ok(())
}

/// Where a name resolved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Resolution {
    pub environment: Handle,
    pub index: u32,
    pub depth: u32,
}

/// Find the environment that binds `name`, walking outwards.
///
/// An object environment matches when the object has the property, which is
/// what makes a global reference resolve to a property of the global object.
pub fn resolve(
    heap: &Heap<'_>,
    environment: Handle,
    name: Handle,
) -> Result<Option<Resolution>, EnvironmentError> {
    let mut current = environment;
    let mut depth = 0u32;
    loop {
        match kind(heap, current)? {
            EnvironmentKind::Object => {
                let object = binding_object(heap, current)?;
                if object.is_object() {
                    let key = crate::string::Key::Name(name);
                    let present = crate::object::has_property(heap, object.as_handle(), key)
                        .map_err(|_| EnvironmentError::Unresolvable)?;
                    if present {
                        return Ok(Some(Resolution {
                            environment: current,
                            index: u32::MAX,
                            depth,
                        }));
                    }
                }
            }
            _ => {
                if let Some(index) = index_of(heap, current, name)? {
                    return Ok(Some(Resolution {
                        environment: current,
                        index,
                        depth,
                    }));
                }
            }
        }
        let parent = parent(heap, current)?;
        if !parent.is_object() && !matches!(parent.tag(), Tag::Object) {
            return Ok(None);
        }
        depth += 1;
        if depth > MAX_SCOPE_DEPTH {
            return Err(EnvironmentError::ChainTooDeep);
        }
        current = parent.as_handle();
    }
}

pub fn count(heap: &Heap<'_>, environment: Handle) -> Result<u32, EnvironmentError> {
    let cell = environment_cell(heap, environment)?;
    Ok(read_u32(cell, layout::COUNT))
}

pub fn capacity(heap: &Heap<'_>, environment: Handle) -> Result<u32, EnvironmentError> {
    let cell = environment_cell(heap, environment)?;
    Ok(read_u32(cell, layout::CAPACITY))
}

/// Little-endian integer fields, each checked once.
fn read_u32(bytes: &[u8], at: usize) -> u32 {
    crate::value::field::<4>(bytes, at).map_or(0, u32::from_le_bytes)
}

fn write_u32(bytes: &mut [u8], at: usize, value: u32) {
    if let Some(field) = bytes.get_mut(at..at + 4) {
        field.copy_from_slice(&value.to_le_bytes());
    }
}

fn binding_record<'h>(
    heap: &'h Heap<'_>,
    environment: Handle,
    index: u32,
) -> Result<&'h [u8], EnvironmentError> {
    let count = count(heap, environment)?;
    if index >= count {
        return Err(EnvironmentError::Unresolvable);
    }
    let cell = environment_cell(heap, environment)?;
    let at = HEADER + index as usize * BINDING;
    cell.get(at..at + BINDING)
        .ok_or(EnvironmentError::Unresolvable)
}

fn environment_cell<'h>(
    heap: &'h Heap<'_>,
    environment: Handle,
) -> Result<&'h [u8], EnvironmentError> {
    if heap.kind(environment)? != CellKind::Environment {
        return Err(EnvironmentError::Heap(HeapError::StaleHandle));
    }
    let cell = heap.cell(environment)?;
    if cell.len() < HEADER {
        return Err(EnvironmentError::Heap(HeapError::StaleHandle));
    }
    Ok(cell)
}
