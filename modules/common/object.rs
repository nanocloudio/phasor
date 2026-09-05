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

/// Where each field of an object cell sits, and the shape of a property
/// table. The collector traces through these names, so a layout change is
/// one edit.
pub mod layout {
    /// The prototype: an encoded value, an object or null.
    pub const PROTOTYPE: usize = 0;
    /// Non-zero while the object is extensible.
    pub const EXTENSIBLE: usize = 9;
    /// The exotic behaviour, from `exotic`.
    pub const EXOTIC: usize = 10;
    /// The property table handle; `Handle::NONE_INDEX` while there is none.
    pub const TABLE: usize = 12;
    /// The internal kind: plain, function, native, promise, and the rest.
    pub const INTERNAL: usize = 20;
    /// One byte the kind interprets: function flags, a generator's state, a
    /// promise's state, an iterator's kind, a regular expression's flags.
    pub const FLAGS: usize = 21;
    /// A `u32` the kind interprets: a function's code index or a native's id.
    pub const CODE: usize = 24;
    /// An encoded value the kind interprets: a function's environment, a
    /// wrapper's primitive, an iterator's target, a generator's coroutine, a
    /// promise's settled value, a native's bound value.
    pub const SLOT: usize = 28;
    /// A promise's reaction-list handle. Overlaps `EXTRA`, which no promise
    /// uses.
    pub const REACTIONS: usize = 40;
    /// A `u32` the kind interprets: a function's module, an iterator's index,
    /// a mapped arguments object's mask.
    pub const EXTRA: usize = 44;
    /// A method's home object handle; `Handle::NONE_INDEX` while there is none.
    pub const HOME: usize = 48;
    /// Bytes in an object cell.
    pub const SIZE: usize = 56;

    /// Bytes in a property table before its records: the count and the
    /// capacity, each a `u32`.
    pub const TABLE_HEADER: usize = 8;
    pub const TABLE_COUNT: usize = 0;
    pub const TABLE_CAPACITY: usize = 4;
    /// Bytes in one property record.
    pub const RECORD: usize = 32;

    /// Where each field of a property record sits.
    pub mod record {
        /// The key kind: 0 an index, 1 a name, 2 a symbol.
        pub const KIND: usize = 0;
        /// Non-zero for an accessor.
        pub const ACCESSOR: usize = 1;
        pub const ATTRIBUTES: usize = 2;
        /// The key payload: an index, or a packed handle.
        pub const KEY: usize = 8;
        /// The value, or the getter: an encoded value.
        pub const FIRST: usize = 16;
        /// The setter, in the short form.
        pub const SECOND: usize = 25;
    }
}

const OBJECT_SIZE: usize = layout::SIZE;
const RECORD: usize = layout::RECORD;
const TABLE_HEADER: usize = layout::TABLE_HEADER;
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
    prototype.encode_at(cell, layout::PROTOTYPE);
    cell[layout::EXTENSIBLE] = 1;
    Handle::write_none_at(cell, layout::TABLE);
    cell[layout::INTERNAL] = INTERNAL_PLAIN;
    // No home object: a method's `super` base, absent everywhere else.
    Handle::write_none_at(cell, layout::HOME);
    Ok(handle)
}

/// Give a method the object it was defined on, which is what `super` in its
/// body resolves through.
pub fn set_home_object(
    heap: &mut Heap<'_>,
    function: Handle,
    home: Handle,
) -> Result<(), ObjectError> {
    let cell = heap.cell_mut(function)?;
    home.write_at(cell, layout::HOME);
    Ok(())
}

/// The object a method was defined on, if `super` may be used in it.
pub fn home_object(heap: &Heap<'_>, function: Handle) -> Result<Option<Handle>, ObjectError> {
    let cell = object_cell(heap, function)?;
    Ok(Handle::read_at(cell, layout::HOME))
}

/// The internal-slot kinds an object may carry.
/// The exotic behaviours an object may carry, in the byte after its
/// extensibility flag: a proxy interposes its handler on every operation,
/// a typed array reads and writes its buffer's bytes by index.
pub mod exotic {
    pub const NONE: u8 = 0;
    pub const PROXY: u8 = 1;
    pub const ARRAY_BUFFER: u8 = 2;
    pub const TYPED_ARRAY: u8 = 3;
    pub const DATA_VIEW: u8 = 4;
    /// A module namespace: null prototype, never extensible, its bindings
    /// read live.
    pub const NAMESPACE: u8 = 5;
    /// A deferred module namespace: shaped as a namespace, but a meaningful
    /// use runs the module it names first.
    pub const DEFERRED: u8 = 6;
}

/// The exotic behaviour an object carries, if any.
pub fn exotic_kind(heap: &Heap<'_>, object: Handle) -> Result<u8, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(cell[layout::EXOTIC])
}

/// Give an object an exotic behaviour.
pub fn set_exotic_kind(heap: &mut Heap<'_>, object: Handle, kind: u8) -> Result<(), ObjectError> {
    let cell = heap.cell_mut(object)?;
    cell[layout::EXOTIC] = kind;
    Ok(())
}

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
/// A generator: its state byte and its suspended frame.
const INTERNAL_GENERATOR: u8 = 7;
/// A mapped arguments object: its function environment and how many leading
/// indices alias parameter slots.
const INTERNAL_ARGUMENTS: u8 = 8;

/// Function flags.
pub mod function_flag {
    /// The function may be used with `new`.
    pub const CONSTRUCTOR: u8 = 1 << 0;
    /// The function's body is strict.
    pub const STRICT: u8 = 1 << 1;
    /// A class constructor: callable only through `new`.
    pub const CLASS: u8 = 1 << 2;
    /// A derived class constructor: its body must `super()` before `this`
    /// settles, and its default form forwards its arguments.
    pub const DERIVED: u8 = 1 << 3;
    /// The function's `name` has been materialised — or deleted — so it is
    /// never made again.
    pub const NAMED: u8 = 1 << 4;
    /// The same for `length`.
    pub const MEASURED: u8 = 1 << 5;
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
    cell[layout::INTERNAL] = INTERNAL_FUNCTION;
    cell[layout::FLAGS] = flags;
    write_u32(cell, layout::CODE, code);
    environment.encode_at(cell, layout::SLOT);
    write_u32(cell, layout::EXTRA, module);
    Ok(handle)
}

/// The module a function belongs to.
pub fn function_module(heap: &Heap<'_>, object: Handle) -> Result<u32, ObjectError> {
    let cell = object_cell(heap, object)?;
    if cell[layout::INTERNAL] != INTERNAL_FUNCTION {
        return Ok(0);
    }
    Ok(read_u32(cell, layout::EXTRA))
}

/// Create an object wrapping a primitive value.
pub fn create_wrapper(
    heap: &mut Heap<'_>,
    prototype: Value,
    value: Value,
) -> Result<Handle, ObjectError> {
    let handle = create(heap, prototype)?;
    let cell = heap.cell_mut(handle)?;
    cell[layout::INTERNAL] = INTERNAL_WRAPPER;
    value.encode_at(cell, layout::SLOT);
    Ok(handle)
}

/// The primitive a wrapper holds, if it is one.
pub fn wrapper_value(heap: &Heap<'_>, object: Handle) -> Result<Option<Value>, ObjectError> {
    let cell = object_cell(heap, object)?;
    if cell[layout::INTERNAL] != INTERNAL_WRAPPER {
        return Ok(None);
    }
    Ok(Some(Value::decode_at(cell, layout::SLOT)))
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
    cell[layout::INTERNAL] = INTERNAL_ITERATOR;
    cell[layout::FLAGS] = kind;
    target.encode_at(cell, layout::SLOT);
    write_u32(cell, layout::EXTRA, 0);
    Ok(handle)
}

/// What an iterator is walking, where it has reached, and what it produces.
pub fn iterator_state(
    heap: &Heap<'_>,
    object: Handle,
) -> Result<Option<(Value, u32, u8)>, ObjectError> {
    let cell = object_cell(heap, object)?;
    if cell[layout::INTERNAL] != INTERNAL_ITERATOR {
        return Ok(None);
    }
    let index = read_u32(cell, layout::EXTRA);
    Ok(Some((
        Value::decode_at(cell, layout::SLOT),
        index,
        cell[layout::FLAGS],
    )))
}

/// Record where an iterator has reached.
pub fn set_iterator_index(
    heap: &mut Heap<'_>,
    object: Handle,
    index: u32,
) -> Result<(), ObjectError> {
    let cell = heap.cell_mut(object)?;
    if cell[layout::INTERNAL] != INTERNAL_ITERATOR {
        return Ok(());
    }
    write_u32(cell, layout::EXTRA, index);
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
    cell[layout::INTERNAL] = INTERNAL_REGEXP;
    cell[layout::FLAGS] = flags;
    program.encode_at(cell, layout::SLOT);
    Ok(handle)
}

/// The program and flags a regular expression carries, if it is one.
pub fn regexp_program(heap: &Heap<'_>, object: Handle) -> Result<Option<(Value, u8)>, ObjectError> {
    let cell = object_cell(heap, object)?;
    if cell[layout::INTERNAL] != INTERNAL_REGEXP {
        return Ok(None);
    }
    Ok(Some((
        Value::decode_at(cell, layout::SLOT),
        cell[layout::FLAGS],
    )))
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
    cell[layout::INTERNAL] = INTERNAL_NATIVE;
    cell[layout::FLAGS] = flags;
    write_u32(cell, layout::CODE, native);
    Value::UNDEFINED.encode_at(cell, layout::SLOT);
    Ok(handle)
}

/// Generator states. The high bit marks an async generator, whose methods
/// answer promises.
pub mod generator_state {
    pub const SUSPENDED: u8 = 0;
    pub const RUNNING: u8 = 1;
    pub const DONE: u8 = 2;
    pub const ASYNC: u8 = 1 << 7;
}

/// Create a generator: the state byte and the suspended frame it will
/// resume.
pub fn create_generator(
    heap: &mut Heap<'_>,
    prototype: Value,
    coroutine: Value,
) -> Result<Handle, ObjectError> {
    let handle = create(heap, prototype)?;
    let cell = heap.cell_mut(handle)?;
    cell[layout::INTERNAL] = INTERNAL_GENERATOR;
    cell[layout::FLAGS] = generator_state::SUSPENDED;
    coroutine.encode_at(cell, layout::SLOT);
    Ok(handle)
}

/// Mark an arguments object as mapped: reads and writes of the indices in
/// the mask go through the environment's parameter slots.
pub fn map_arguments(
    heap: &mut Heap<'_>,
    object: Handle,
    environment: Value,
    mapped: u32,
) -> Result<(), ObjectError> {
    let mask = if mapped >= 32 {
        u32::MAX
    } else {
        (1u32 << mapped) - 1
    };
    let cell = heap.cell_mut(object)?;
    cell[layout::INTERNAL] = INTERNAL_ARGUMENTS;
    environment.encode_at(cell, layout::SLOT);
    write_u32(cell, layout::EXTRA, mask);
    Ok(())
}

/// A mapped arguments object's environment and mapped-index mask.
pub fn arguments_map(heap: &Heap<'_>, object: Handle) -> Result<Option<(Value, u32)>, ObjectError> {
    let cell = object_cell(heap, object)?;
    if cell[layout::INTERNAL] != INTERNAL_ARGUMENTS {
        return Ok(None);
    }
    let mask = read_u32(cell, layout::EXTRA);
    Ok(Some((Value::decode_at(cell, layout::SLOT), mask)))
}

/// Remove one index from an arguments object's map, which `delete` and a
/// redefinition that breaks the aliasing both do.
pub fn unmap_argument(heap: &mut Heap<'_>, object: Handle, index: u32) -> Result<(), ObjectError> {
    let cell = heap.cell_mut(object)?;
    if cell[layout::INTERNAL] != INTERNAL_ARGUMENTS || index >= 32 {
        return Ok(());
    }
    let mut mask = read_u32(cell, layout::EXTRA);
    mask &= !(1u32 << index);
    write_u32(cell, layout::EXTRA, mask);
    Ok(())
}

/// Whether the object is a generator.
pub fn is_generator(heap: &Heap<'_>, object: Handle) -> Result<bool, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(cell[layout::INTERNAL] == INTERNAL_GENERATOR)
}

/// A generator's state and suspended frame.
pub fn generator(heap: &Heap<'_>, object: Handle) -> Result<Option<(u8, Value)>, ObjectError> {
    let cell = object_cell(heap, object)?;
    if cell[layout::INTERNAL] != INTERNAL_GENERATOR {
        return Ok(None);
    }
    Ok(Some((
        cell[layout::FLAGS],
        Value::decode_at(cell, layout::SLOT),
    )))
}

/// Move a generator to `state`, holding `coroutine` as its frame.
pub fn set_generator(
    heap: &mut Heap<'_>,
    object: Handle,
    state: u8,
    coroutine: Value,
) -> Result<(), ObjectError> {
    let cell = heap.cell_mut(object)?;
    if cell[layout::INTERNAL] != INTERNAL_GENERATOR {
        return Ok(());
    }
    cell[layout::FLAGS] = state;
    coroutine.encode_at(cell, layout::SLOT);
    Ok(())
}

/// Whether the object is a promise, which async machinery asks before
/// settling through a frame's promise slot.
/// Whether the object can be called.
pub fn is_callable(heap: &Heap<'_>, object: Handle) -> Result<bool, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(cell[layout::INTERNAL] == INTERNAL_FUNCTION || cell[layout::INTERNAL] == INTERNAL_NATIVE)
}

/// Whether the object is a function the engine implements itself.
pub fn is_native(heap: &Heap<'_>, object: Handle) -> Result<bool, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(cell[layout::INTERNAL] == INTERNAL_NATIVE)
}

/// Whether the object can be used with `new`.
pub fn is_constructor(heap: &Heap<'_>, object: Handle) -> Result<bool, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(cell[layout::INTERNAL] != INTERNAL_PLAIN
        && cell[layout::FLAGS] & function_flag::CONSTRUCTOR != 0)
}

/// The function's flags.
pub fn function_flags(heap: &Heap<'_>, object: Handle) -> Result<u8, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(cell[layout::FLAGS])
}

/// Add flags to a function, which shaping a class constructor does.
pub fn add_function_flags(
    heap: &mut Heap<'_>,
    object: Handle,
    flags: u8,
) -> Result<(), ObjectError> {
    let cell = heap.cell_mut(object)?;
    cell[layout::FLAGS] |= flags;
    Ok(())
}

/// The index of the function's code in its unit.
pub fn function_code(heap: &Heap<'_>, object: Handle) -> Result<u32, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(read_u32(cell, layout::CODE))
}

/// The environment the function closed over.
///
/// A function the engine implements itself has no environment; the same slot
/// carries whatever that function was bound to, such as the promise a resolve
/// function settles.
pub fn function_environment(heap: &Heap<'_>, object: Handle) -> Result<Value, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(Value::decode_at(cell, layout::SLOT))
}

/// Give a native function the value it is bound to.
pub fn set_bound_value(
    heap: &mut Heap<'_>,
    object: Handle,
    value: Value,
) -> Result<(), ObjectError> {
    let cell = heap.cell_mut(object)?;
    value.encode_at(cell, layout::SLOT);
    Ok(())
}

/// Mark an object as a promise, which gives it a state, a settled value, and a
/// list of reactions waiting on it.
pub fn make_promise(heap: &mut Heap<'_>, object: Handle) -> Result<(), ObjectError> {
    let cell = heap.cell_mut(object)?;
    cell[layout::INTERNAL] = INTERNAL_PROMISE;
    cell[layout::FLAGS] = 0;
    Value::UNDEFINED.encode_at(cell, layout::SLOT);
    Handle::write_none_at(cell, layout::REACTIONS);
    Ok(())
}

/// Whether the object is a promise.
pub fn is_promise(heap: &Heap<'_>, object: Handle) -> Result<bool, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(cell[layout::INTERNAL] == INTERNAL_PROMISE)
}

/// A promise's state: zero pending, one fulfilled, two rejected.
pub fn promise_state(heap: &Heap<'_>, object: Handle) -> Result<u8, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(cell[layout::FLAGS])
}

/// A promise's settled value, which is undefined while it is pending.
pub fn promise_value(heap: &Heap<'_>, object: Handle) -> Result<Value, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(Value::decode_at(cell, layout::SLOT))
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
    cell[layout::FLAGS] = state;
    value.encode_at(cell, layout::SLOT);
    Ok(true)
}

/// The cell holding the reactions waiting on a promise, if it has one.
pub fn promise_reactions(heap: &Heap<'_>, object: Handle) -> Result<Option<Handle>, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(Handle::read_at(cell, layout::REACTIONS))
}

/// Give a promise its reaction list.
pub fn set_promise_reactions(
    heap: &mut Heap<'_>,
    object: Handle,
    reactions: Handle,
) -> Result<(), ObjectError> {
    let cell = heap.cell_mut(object)?;
    reactions.write_at(cell, layout::REACTIONS);
    Ok(())
}

/// The object's prototype, which is an object value or null.
pub fn prototype(heap: &Heap<'_>, object: Handle) -> Result<Value, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(Value::decode_at(cell, layout::PROTOTYPE))
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
    value.encode_at(cell, layout::PROTOTYPE);
    Ok(true)
}

pub fn is_extensible(heap: &Heap<'_>, object: Handle) -> Result<bool, ObjectError> {
    let cell = object_cell(heap, object)?;
    Ok(cell[layout::EXTENSIBLE] != 0)
}

pub fn prevent_extensions(heap: &mut Heap<'_>, object: Handle) -> Result<(), ObjectError> {
    let cell = heap.cell_mut(object)?;
    cell[layout::EXTENSIBLE] = 0;
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
    write_u32(cell, layout::TABLE_COUNT, count - 1);
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
    Ok(Handle::read_at(cell, layout::TABLE))
}

fn set_table(heap: &mut Heap<'_>, object: Handle, table: Handle) -> Result<(), ObjectError> {
    let cell = heap.cell_mut(object)?;
    table.write_at(cell, layout::TABLE);
    Ok(())
}

fn table_count(heap: &Heap<'_>, table: Handle) -> Result<u32, ObjectError> {
    let cell = heap.cell(table)?;
    Ok(read_u32(cell, layout::TABLE_COUNT))
}

fn table_capacity(heap: &Heap<'_>, table: Handle) -> Result<u32, ObjectError> {
    let cell = heap.cell(table)?;
    Ok(read_u32(cell, layout::TABLE_CAPACITY))
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
    write_u32(cell, layout::TABLE_COUNT, count + 1);
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
    write_u32(cell, layout::TABLE_COUNT, 0);
    write_u32(cell, layout::TABLE_CAPACITY, capacity);
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
    write_u32(cell, layout::TABLE_COUNT, count);
    set_table(heap, object, larger)?;
    // The old table is unreachable now; collection will reclaim it.
    Ok(larger)
}

fn read_key(cell: &[u8], index: u32) -> Result<Key, ObjectError> {
    let at = record_at(index);
    let record = cell
        .get(at..at + RECORD)
        .ok_or(ObjectError::Heap(HeapError::StaleHandle))?;
    let payload = read_u64(record, layout::record::KEY);
    Ok(match record[layout::record::KIND] {
        0 => Key::Index((payload & 0xFFFF_FFFF) as u32),
        1 => Key::Name(Handle::unpack(payload)),
        _ => Key::Symbol(Handle::unpack(payload)),
    })
}

fn read_record(cell: &[u8], index: u32) -> Result<Descriptor, ObjectError> {
    let at = record_at(index);
    let record = cell
        .get(at..at + RECORD)
        .ok_or(ObjectError::Heap(HeapError::StaleHandle))?;
    let kind = if record[layout::record::ACCESSOR] == 0 {
        DescriptorKind::Data
    } else {
        DescriptorKind::Accessor
    };
    let first = Value::decode_at(record, layout::record::FIRST);
    // The second value shares the record with the first: a data property uses
    // one slot, an accessor uses both.
    let second = Value::decode_short_at(record, layout::record::SECOND);
    Ok(match kind {
        DescriptorKind::Data => Descriptor {
            kind,
            attributes: record[layout::record::ATTRIBUTES],
            value: first,
            getter: Value::UNDEFINED,
            setter: Value::UNDEFINED,
        },
        DescriptorKind::Accessor => Descriptor {
            kind,
            attributes: record[layout::record::ATTRIBUTES],
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
        Key::Name(handle) => (1u8, handle.pack()),
        Key::Symbol(handle) => (2u8, handle.pack()),
    };
    record[layout::record::KIND] = kind_byte;
    record[layout::record::ACCESSOR] =
        u8::from(matches!(descriptor.kind, DescriptorKind::Accessor));
    record[layout::record::ATTRIBUTES] = descriptor.attributes;
    record[3] = 0;
    write_u64(record, layout::record::KEY, payload);
    match descriptor.kind {
        DescriptorKind::Data => {
            descriptor.value.encode_at(record, layout::record::FIRST);
            Value::UNDEFINED.encode_short_at(record, layout::record::SECOND);
        }
        DescriptorKind::Accessor => {
            descriptor.getter.encode_at(record, layout::record::FIRST);
            descriptor
                .setter
                .encode_short_at(record, layout::record::SECOND);
        }
    }
    Ok(())
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

fn read_u64(bytes: &[u8], at: usize) -> u64 {
    crate::value::field::<8>(bytes, at).map_or(0, u64::from_le_bytes)
}

fn write_u64(bytes: &mut [u8], at: usize, value: u64) {
    if let Some(field) = bytes.get_mut(at..at + 8) {
        field.copy_from_slice(&value.to_le_bytes());
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
