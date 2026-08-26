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

/// Bytes before the bindings.
const HEADER: usize = 32;
/// Bytes in one binding.
const BINDING: usize = 24;

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
    cell[0] = kind as u8;
    cell[4..8].copy_from_slice(&0u32.to_le_bytes());
    cell[8..12].copy_from_slice(&capacity.to_le_bytes());
    write_value(&mut cell[12..21], parent);
    write_value(&mut cell[21..30], Value::UNDEFINED);
    Ok(handle)
}

/// Create the environment whose bindings are the properties of `object`.
pub fn create_object_environment(
    heap: &mut Heap<'_>,
    parent: Value,
    object: Handle,
) -> Result<Handle, EnvironmentError> {
    let handle = create(heap, EnvironmentKind::Object, parent, 0)?;
    let cell = heap.cell_mut(handle)?;
    write_value(&mut cell[21..30], Value::object(object));
    Ok(handle)
}

pub fn kind(heap: &Heap<'_>, environment: Handle) -> Result<EnvironmentKind, EnvironmentError> {
    let cell = environment_cell(heap, environment)?;
    Ok(match cell[0] {
        1 => EnvironmentKind::Function,
        2 => EnvironmentKind::Object,
        _ => EnvironmentKind::Declarative,
    })
}

pub fn parent(heap: &Heap<'_>, environment: Handle) -> Result<Value, EnvironmentError> {
    let cell = environment_cell(heap, environment)?;
    Ok(read_value(&cell[12..21]))
}

/// The object whose properties are this environment's bindings, if it has one.
pub fn binding_object(heap: &Heap<'_>, environment: Handle) -> Result<Value, EnvironmentError> {
    let cell = environment_cell(heap, environment)?;
    Ok(read_value(&cell[21..30]))
}

/// The `this` value a function environment carries.
pub fn this_value(heap: &Heap<'_>, environment: Handle) -> Result<Value, EnvironmentError> {
    let cell = environment_cell(heap, environment)?;
    Ok(read_value(&cell[21..30]))
}

/// Give a function environment its `this` value.
pub fn set_this(
    heap: &mut Heap<'_>,
    environment: Handle,
    value: Value,
) -> Result<(), EnvironmentError> {
    let cell = heap.cell_mut(environment)?;
    write_value(&mut cell[21..30], value);
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
    record[0] = flags & !binding::INITIALISED;
    record[4..8].copy_from_slice(&name.index.to_le_bytes());
    record[8..12].copy_from_slice(&name.generation.to_le_bytes());
    write_value(&mut record[12..21], Value::UNDEFINED);
    cell[4..8].copy_from_slice(&(count + 1).to_le_bytes());
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
    record[0] |= binding::INITIALISED;
    write_value(&mut record[12..21], value);
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

/// Read a binding by index in this record.
pub fn slot_value(
    heap: &Heap<'_>,
    environment: Handle,
    index: u32,
) -> Result<Value, EnvironmentError> {
    let record = binding_record(heap, environment, index)?;
    if record[0] & binding::INITIALISED == 0 {
        return Err(EnvironmentError::Uninitialised);
    }
    Ok(read_value(&record[12..21]))
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
    write_value(&mut record[12..21], value);
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
    Ok(u32::from_le_bytes([cell[4], cell[5], cell[6], cell[7]]))
}

pub fn capacity(heap: &Heap<'_>, environment: Handle) -> Result<u32, EnvironmentError> {
    let cell = environment_cell(heap, environment)?;
    Ok(u32::from_le_bytes([cell[8], cell[9], cell[10], cell[11]]))
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

/// Encode a value as a tag byte and eight payload bytes.
fn write_value(out: &mut [u8], value: Value) {
    out[0] = value.tag() as u8;
    let payload = match value.tag() {
        Tag::Number => value.as_number().to_bits(),
        Tag::Boolean => u64::from(value.as_boolean()),
        Tag::Undefined | Tag::Null => 0,
        _ => {
            let handle = value.as_handle();
            (u64::from(handle.generation) << 32) | u64::from(handle.index)
        }
    };
    out[1..9].copy_from_slice(&payload.to_le_bytes());
}

fn read_value(bytes: &[u8]) -> Value {
    let payload = u64::from_le_bytes([
        bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7], bytes[8],
    ]);
    let handle = Handle::new((payload & 0xFFFF_FFFF) as u32, (payload >> 32) as u32);
    match bytes[0] {
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
