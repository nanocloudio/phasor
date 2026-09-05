//! `ArrayBuffer`, `SharedArrayBuffer`, the typed arrays, and `DataView`.

use super::*;

/// `ArrayBuffer`, `SharedArrayBuffer`, the typed arrays, and `DataView`.
pub(super) fn build_buffers(
    heap: &mut Heap<'_>,
    atoms: &mut Atoms<'_>,
    function_prototype: Handle,
    global: Handle,
    object_prototype: Handle,
    symbols: Symbols,
) -> Result<(Handle, Handle, Handle, [Handle; TYPED_ARRAY_KINDS], Handle), ObjectError> {
    // `ArrayBuffer`, `Uint8Array`, and `DataView`: bytes as numbers in an
    // array the buffer owns, read and written by index through the view.
    let array_buffer_prototype = object::create(heap, Value::object(object_prototype))?;
    let array_buffer_constructor = constructor(
        heap,
        atoms,
        global,
        b"ArrayBuffer",
        native::ARRAY_BUFFER,
        array_buffer_prototype,
        function_prototype,
    )?;
    method(
        heap,
        atoms,
        array_buffer_prototype,
        b"slice",
        native::ARRAY_BUFFER_SLICE,
        function_prototype,
    )?;
    method(
        heap,
        atoms,
        array_buffer_prototype,
        b"resize",
        native::ARRAY_BUFFER_RESIZE,
        function_prototype,
    )?;
    method(
        heap,
        atoms,
        array_buffer_prototype,
        b"transferToImmutable",
        native::ARRAY_BUFFER_TRANSFER_TO_IMMUTABLE,
        function_prototype,
    )?;
    let entries = [
        Entry::Getter(b"byteLength", native::ARRAY_BUFFER_BYTE_LENGTH),
        Entry::Getter(b"maxByteLength", native::ARRAY_BUFFER_MAX_BYTE_LENGTH),
        Entry::Getter(b"resizable", native::ARRAY_BUFFER_RESIZABLE),
        Entry::Getter(b"immutable", native::ARRAY_BUFFER_IMMUTABLE),
    ];
    install(
        heap,
        atoms,
        array_buffer_prototype,
        function_prototype,
        &entries,
    )?;
    // `SharedArrayBuffer`: the same bytes under a name of its own; nothing
    // here shares them, since the engine has no threads.
    let shared_array_buffer_prototype = object::create(heap, Value::object(object_prototype))?;
    constructor(
        heap,
        atoms,
        global,
        b"SharedArrayBuffer",
        native::SHARED_ARRAY_BUFFER,
        shared_array_buffer_prototype,
        function_prototype,
    )?;
    method(
        heap,
        atoms,
        shared_array_buffer_prototype,
        b"slice",
        native::ARRAY_BUFFER_SLICE,
        function_prototype,
    )?;
    accessor(
        heap,
        atoms,
        shared_array_buffer_prototype,
        b"byteLength",
        native::ARRAY_BUFFER_BYTE_LENGTH,
        function_prototype,
    )?;
    let species_getter = object::create_native(
        heap,
        Value::object(function_prototype),
        native::SPECIES_GETTER,
        0,
    )?;
    object::define_own_property(
        heap,
        array_buffer_constructor,
        Key::Symbol(symbols.species),
        Descriptor::accessor(
            Value::object(species_getter),
            Value::UNDEFINED,
            attribute::CONFIGURABLE,
        ),
    )?;
    // The typed arrays: `%TypedArray%` — reached only through a kind's
    // `constructor` chain — and one constructor per element kind beneath it,
    // each carrying its kind and element size.
    let typed_array_prototype = object::create(heap, Value::object(object_prototype))?;
    let typed_array_base = intrinsic_constructor(
        heap,
        atoms,
        b"TypedArray",
        native::TYPED_ARRAY_BASE,
        typed_array_prototype,
        function_prototype,
    )?;
    let entries = [
        Entry::Method(b"of", native::TYPED_ARRAY_OF),
        Entry::Method(b"from", native::TYPED_ARRAY_FROM),
    ];
    install(heap, atoms, typed_array_base, function_prototype, &entries)?;
    let entries = [
        Entry::Getter(b"length", native::TYPED_ARRAY_LENGTH),
        Entry::Getter(b"byteLength", native::TYPED_ARRAY_BYTE_LENGTH),
        Entry::Getter(b"byteOffset", native::TYPED_ARRAY_BYTE_OFFSET),
        Entry::Getter(b"buffer", native::TYPED_ARRAY_BUFFER),
    ];
    install(
        heap,
        atoms,
        typed_array_prototype,
        function_prototype,
        &entries,
    )?;
    let entries = [
        Entry::Method(b"values", native::TYPED_ARRAY_VALUES),
        Entry::Method(b"keys", native::TYPED_ARRAY_KEYS),
        Entry::Method(b"entries", native::TYPED_ARRAY_ENTRIES),
        Entry::Method(b"subarray", native::TYPED_ARRAY_SUBARRAY),
        Entry::Method(b"set", native::TYPED_ARRAY_SET),
        Entry::Method(b"fill", native::TYPED_ARRAY_FILL),
    ];
    install(
        heap,
        atoms,
        typed_array_prototype,
        function_prototype,
        &entries,
    )?;
    {
        let values_key = key_of(heap, atoms, b"values")?;
        let values = object::get_own_property(heap, typed_array_prototype, values_key)?
            .map_or(Value::UNDEFINED, |found| found.value);
        object::define_own_property(
            heap,
            typed_array_prototype,
            Key::Symbol(symbols.iterator),
            Descriptor::data(values, attribute::WRITABLE | attribute::CONFIGURABLE),
        )?;
        let tag_getter = object::create_native(
            heap,
            Value::object(function_prototype),
            native::TYPED_ARRAY_TAG,
            0,
        )?;
        object::define_own_property(
            heap,
            typed_array_prototype,
            Key::Symbol(symbols.to_string_tag),
            Descriptor::accessor(
                Value::object(tag_getter),
                Value::UNDEFINED,
                attribute::CONFIGURABLE,
            ),
        )?;
    }
    let mut typed_array_prototypes = [typed_array_prototype; TYPED_ARRAY_KINDS];
    let mut kind = 0u8;
    while usize::from(kind) < TYPED_ARRAY_KINDS {
        let prototype = object::create(heap, Value::object(typed_array_prototype))?;
        let (name, name_length) = typed_array_name(kind);
        let made = constructor(
            heap,
            atoms,
            global,
            name.get(..name_length).unwrap_or(&[]),
            native::TYPED_ARRAY,
            prototype,
            function_prototype,
        )?;
        object::set_prototype(heap, made, Value::object(typed_array_base))?;
        let size = Value::number(f64::from(typed_array_element_size(kind)));
        for target in [made, prototype] {
            define(heap, atoms, target, b"BYTES_PER_ELEMENT", size, 0)?;
        }
        define(
            heap,
            atoms,
            made,
            b"\0kind",
            Value::number(f64::from(kind)),
            attribute::WRITABLE,
        )?;
        typed_array_prototypes[usize::from(kind)] = prototype;
        kind += 1;
    }
    let data_view_prototype = object::create(heap, Value::object(object_prototype))?;
    constructor(
        heap,
        atoms,
        global,
        b"DataView",
        native::DATA_VIEW,
        data_view_prototype,
        function_prototype,
    )?;
    for (prototype, tag) in [
        (array_buffer_prototype, &b"ArrayBuffer"[..]),
        (shared_array_buffer_prototype, &b"SharedArrayBuffer"[..]),
        (data_view_prototype, &b"DataView"[..]),
    ] {
        let text = crate::string::create_ascii(heap, tag)?;
        object::define_own_property(
            heap,
            prototype,
            Key::Symbol(symbols.to_string_tag),
            Descriptor::data(Value::string(text), attribute::CONFIGURABLE),
        )?;
    }
    Ok((
        array_buffer_prototype,
        shared_array_buffer_prototype,
        typed_array_prototype,
        typed_array_prototypes,
        data_view_prototype,
    ))
}
