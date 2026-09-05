//! `ArrayBuffer`, the typed arrays, and `DataView`.

use super::*;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    // ArrayBuffer, Uint8Array, DataView

    /// The array of numbers an ArrayBuffer holds its bytes in.
    pub(super) fn array_buffer_bytes(&mut self, buffer: Value) -> Result<Value, Completion> {
        if buffer.is_object()
            && object::exotic_kind(self.heap, buffer.as_handle()).unwrap_or(0)
                == object::exotic::ARRAY_BUFFER
        {
            let key = self.ascii_key(b"\0bytes")?;
            if let Some(descriptor) = object::get_own_property(self.heap, buffer.as_handle(), key)
                .map_err(|_| Completion::MALFORMED)?
            {
                return Ok(descriptor.value);
            }
        }
        Err(self.throw_type_error())
    }

    /// `new ArrayBuffer(length, { maxByteLength })`, or the shared kind.
    pub(super) fn construct_array_buffer(
        &mut self,
        length: Value,
        options: Value,
        shared: bool,
    ) -> Result<Value, Completion> {
        let count = self.coerce_to_index(length)?;
        let mut maximum = Value::UNDEFINED;
        if options.is_object() {
            let max_key = self.ascii_key(b"maxByteLength")?;
            let wanted = self.get_property(options, max_key)?;
            if !wanted.is_undefined() {
                let limit = self.coerce_to_index(wanted)?;
                if limit < count {
                    return Err(self.throw_error_of(ErrorKind::Range));
                }
                maximum = Value::number(f64::from(limit));
            }
        }
        let bound = match maximum {
            value if value.is_undefined() => count,
            value => value::to_uint32(value.as_number()),
        };
        if bound > 1_048_576 {
            return Err(self.throw_error_of(ErrorKind::Range));
        }
        let bytes = self.create_array()?;
        let mut index = 0u32;
        while index < count {
            self.append_element(bytes, Some(Value::number(0.0)))?;
            index += 1;
        }
        let prototype = if shared {
            self.realm.shared_array_buffer_prototype
        } else {
            self.realm.array_buffer_prototype
        };
        let made = self.new_instance_of(prototype)?;
        for (name, held) in [(&b"\0bytes"[..], bytes), (&b"\0max"[..], maximum)] {
            let key = self.ascii_key(name)?;
            object::define_own_property(
                self.heap,
                made,
                key,
                Descriptor::data(held, attribute::WRITABLE),
            )
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        object::set_exotic_kind(self.heap, made, object::exotic::ARRAY_BUFFER)
            .map_err(|_| Completion::MALFORMED)?;
        Ok(Value::object(made))
    }

    /// A buffer's `maxByteLength` record: undefined where it is fixed.
    /// Whether a buffer was made immutable: its bytes never change again.
    pub(super) fn array_buffer_immutable(&mut self, buffer: Value) -> Result<bool, Completion> {
        self.array_buffer_bytes(buffer)?;
        let key = self.ascii_key(b"\0immutable")?;
        Ok(object::get_own_property(self.heap, buffer.as_handle(), key)
            .map_err(|_| Completion::MALFORMED)?
            .is_some_and(|descriptor| descriptor.value.is_boolean()))
    }

    pub(super) fn array_buffer_maximum(&mut self, buffer: Value) -> Result<Value, Completion> {
        self.array_buffer_bytes(buffer)?;
        let key = self.ascii_key(b"\0max")?;
        Ok(object::get_own_property(self.heap, buffer.as_handle(), key)
            .map_err(|_| Completion::MALFORMED)?
            .map_or(Value::UNDEFINED, |descriptor| descriptor.value))
    }

    /// `buffer.resize(newLength)`: the bytes grow with zeros or shrink, within
    /// the maximum the buffer was made with.
    pub(super) fn array_buffer_resize(
        &mut self,
        buffer: Value,
        length: Value,
    ) -> Result<(), Completion> {
        let bytes = self.array_buffer_bytes(buffer)?;
        let maximum = self.array_buffer_maximum(buffer)?;
        if maximum.is_undefined() {
            return Err(self.throw_type_error());
        }
        let wanted = self.coerce_to_index(length)?;
        if wanted > value::to_uint32(maximum.as_number()) {
            return Err(self.throw_error_of(ErrorKind::Range));
        }
        let current = self.length_of(bytes)?;
        if wanted <= current {
            self.set_length(bytes, wanted)?;
        } else {
            let mut index = current;
            while index < wanted {
                self.append_element(bytes, Some(Value::number(0.0)))?;
                index += 1;
            }
        }
        Ok(())
    }

    /// The parts of a typed array: its buffer, kind, byte offset, and fixed
    /// element count — none for a view that tracks its buffer's length.
    pub(super) fn typed_array_parts(
        &mut self,
        view: Value,
    ) -> Result<(Value, u8, u32, Option<u32>), Completion> {
        if !view.is_object()
            || object::exotic_kind(self.heap, view.as_handle()).unwrap_or(0)
                != object::exotic::TYPED_ARRAY
        {
            return Err(self.throw_type_error());
        }
        let mut parts = [Value::UNDEFINED; 4];
        for (slot, name) in parts.iter_mut().zip([
            &b"\0buffer"[..],
            &b"\0kind"[..],
            &b"\0offset"[..],
            &b"\0length"[..],
        ]) {
            let key = self.ascii_key(name)?;
            *slot = object::get_own_property(self.heap, view.as_handle(), key)
                .map_err(|_| Completion::MALFORMED)?
                .map_or(Value::UNDEFINED, |descriptor| descriptor.value);
        }
        let kind = value::to_uint32(parts[1].as_number()) as u8;
        let offset = value::to_uint32(parts[2].as_number());
        let fixed = if parts[3].is_undefined() {
            None
        } else {
            Some(value::to_uint32(parts[3].as_number()))
        };
        Ok((parts[0], kind, offset, fixed))
    }

    /// A typed array's element count, or none where its buffer has shrunk
    /// out from under it.
    pub(super) fn typed_array_length(&mut self, view: Value) -> Result<Option<u32>, Completion> {
        let (buffer, kind, offset, fixed) = self.typed_array_parts(view)?;
        let bytes = self.array_buffer_bytes(buffer)?;
        let available = self.length_of(bytes)?;
        let size = crate::realm::typed_array_element_size(kind);
        if offset > available {
            return Ok(None);
        }
        match fixed {
            Some(count) => {
                let needed = u64::from(offset) + u64::from(count) * u64::from(size);
                if needed > u64::from(available) {
                    Ok(None)
                } else {
                    Ok(Some(count))
                }
            }
            None => Ok(Some((available - offset) / size)),
        }
    }

    /// A typed array over a buffer: its instance object, of the kind's
    /// prototype unless `prototype` names another.
    pub(super) fn make_typed_array(
        &mut self,
        kind: u8,
        buffer: Value,
        offset: u32,
        fixed: Option<u32>,
    ) -> Result<Value, Completion> {
        let prototype = self
            .realm
            .typed_array_prototypes
            .get(usize::from(kind))
            .copied()
            .unwrap_or(self.realm.typed_array_prototype);
        let made = self.new_instance_of(prototype)?;
        let length = match fixed {
            Some(count) => Value::number(f64::from(count)),
            None => Value::UNDEFINED,
        };
        for (name, held) in [
            (&b"\0buffer"[..], buffer),
            (&b"\0kind"[..], Value::number(f64::from(kind))),
            (&b"\0offset"[..], Value::number(f64::from(offset))),
            (&b"\0length"[..], length),
        ] {
            let key = self.ascii_key(name)?;
            object::define_own_property(
                self.heap,
                made,
                key,
                Descriptor::data(held, attribute::WRITABLE),
            )
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        object::set_exotic_kind(self.heap, made, object::exotic::TYPED_ARRAY)
            .map_err(|_| Completion::MALFORMED)?;
        Ok(Value::object(made))
    }

    /// The kind a typed array constructor makes.
    pub(super) fn typed_array_kind_of(&mut self, constructor: Value) -> Result<u8, Completion> {
        if !constructor.is_object() {
            return Err(self.throw_type_error());
        }
        let key = self.ascii_key(b"\0kind")?;
        let held = object::get_own_property(self.heap, constructor.as_handle(), key)
            .map_err(|_| Completion::MALFORMED)?
            .map_or(Value::UNDEFINED, |descriptor| descriptor.value);
        if !held.is_number() {
            return Err(self.throw_type_error());
        }
        Ok(value::to_uint32(held.as_number()) as u8)
    }

    /// `new Int8Array(...)` and its kin: over a length, a buffer with an
    /// offset and length, another typed array, an iterable, or an array-like.
    pub(super) fn construct_typed_array(
        &mut self,
        callee: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let kind = self.typed_array_kind_of(callee)?;
        let size = crate::realm::typed_array_element_size(kind);
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        if !first.is_object() {
            let count = self.coerce_to_index(first)?;
            let buffer = self.construct_array_buffer(
                Value::number(f64::from(count) * f64::from(size)),
                Value::UNDEFINED,
                false,
            )?;
            return self.make_typed_array(kind, buffer, 0, Some(count));
        }
        let exotic = object::exotic_kind(self.heap, first.as_handle()).unwrap_or(0);
        if exotic == object::exotic::ARRAY_BUFFER {
            let offset =
                self.coerce_to_index(arguments.get(1).copied().unwrap_or(Value::UNDEFINED))?;
            if offset % size != 0 {
                return Err(self.throw_error_of(ErrorKind::Range));
            }
            let length = arguments.get(2).copied().unwrap_or(Value::UNDEFINED);
            let bytes = self.array_buffer_bytes(first)?;
            let available = self.length_of(bytes)?;
            let maximum = self.array_buffer_maximum(first)?;
            let fixed = if length.is_undefined() {
                if maximum.is_undefined() {
                    // A fixed buffer's view is fixed at what is there now.
                    if available < offset || (available - offset) % size != 0 {
                        return Err(self.throw_error_of(ErrorKind::Range));
                    }
                    Some((available - offset) / size)
                } else {
                    if offset > available {
                        return Err(self.throw_error_of(ErrorKind::Range));
                    }
                    None
                }
            } else {
                let count = self.coerce_to_index(length)?;
                if u64::from(offset) + u64::from(count) * u64::from(size) > u64::from(available) {
                    return Err(self.throw_error_of(ErrorKind::Range));
                }
                Some(count)
            };
            return self.make_typed_array(kind, first, offset, fixed);
        }
        // A typed array, an iterable, or an array-like: its values, copied.
        let values = self.new_array()?;
        if exotic == object::exotic::TYPED_ARRAY {
            let count = self.typed_array_length(first)?.unwrap_or(0);
            let mut index = 0u32;
            while index < count {
                let held = self.typed_array_read(first, index)?;
                self.append_element(values, Some(held))?;
                index += 1;
            }
        } else if let Some(iterator) = self.iterator_of(first)? {
            while let Some(element) = self.iterator_step(iterator)? {
                self.append_element(values, Some(element))?;
            }
        } else {
            let count = self.length_of(first)?;
            let mut index = 0u32;
            while index < count {
                let held = self.element(first, index)?;
                self.append_element(values, Some(held))?;
                index += 1;
            }
        }
        let count = self.length_of(values)?;
        let buffer = self.construct_array_buffer(
            Value::number(f64::from(count) * f64::from(size)),
            Value::UNDEFINED,
            false,
        )?;
        let made = self.make_typed_array(kind, buffer, 0, Some(count))?;
        let mut index = 0u32;
        while index < count {
            let held = self.element(values, index)?;
            self.typed_array_write(made, index, held)?;
            index += 1;
        }
        Ok(made)
    }

    pub(super) fn construct_data_view(&mut self, buffer: Value) -> Result<Value, Completion> {
        let bytes = self.array_buffer_bytes(buffer)?;
        let length = self.length_of(bytes)?;
        let made = self.new_instance_of(self.realm.data_view_prototype)?;
        for (name, held) in [
            (&b"buffer"[..], buffer),
            (&b"byteLength"[..], Value::number(f64::from(length))),
            (&b"byteOffset"[..], Value::number(0.0)),
        ] {
            let key = self.ascii_key(name)?;
            object::define_own_property(
                self.heap,
                made,
                key,
                Descriptor::data(held, attribute::CONFIGURABLE),
            )
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        object::set_exotic_kind(self.heap, made, object::exotic::DATA_VIEW)
            .map_err(|_| Completion::MALFORMED)?;
        Ok(Value::object(made))
    }

    /// Read one element: the bytes at its place, little-endian, as the
    /// kind's value. Undefined past the end, or where the view is out of
    /// its buffer's bounds.
    pub(super) fn typed_array_read(
        &mut self,
        view: Value,
        index: u32,
    ) -> Result<Value, Completion> {
        let Some(count) = self.typed_array_length(view)? else {
            return Ok(Value::UNDEFINED);
        };
        if index >= count {
            return Ok(Value::UNDEFINED);
        }
        let (buffer, kind, offset, _) = self.typed_array_parts(view)?;
        let bytes = self.array_buffer_bytes(buffer)?;
        let size = crate::realm::typed_array_element_size(kind);
        let start = offset + index * size;
        let mut raw = 0u64;
        let mut byte = 0u32;
        while byte < size {
            let held = self.element(bytes, start + byte)?;
            let value = value::to_uint32(held.as_number()) & 0xFF;
            raw |= u64::from(value) << (8 * byte);
            byte += 1;
        }
        let number = match kind {
            0 => f64::from(raw as u8 as i8),
            1 | 2 => f64::from(raw as u8),
            3 => f64::from(raw as u16 as i16),
            4 => f64::from(raw as u16),
            5 => f64::from(raw as u32 as i32),
            6 => f64::from(raw as u32),
            7 => crate::softfloat::from_f32_bits(raw as u32),
            8 => f64::from_bits(raw),
            _ => {
                let negative = kind == 9 && raw & (1u64 << 63) != 0;
                let magnitude = if negative { raw.wrapping_neg() } else { raw };
                let mut big = crate::bigint::Number::ZERO;
                big.limbs[0] = magnitude as u32;
                big.limbs[1] = (magnitude >> 32) as u32;
                big.length = if big.limbs[1] != 0 {
                    2
                } else if big.limbs[0] != 0 {
                    1
                } else {
                    0
                };
                big.negative = negative && big.length != 0;
                return self.big_int_value(&big);
            }
        };
        Ok(Value::number(number))
    }

    /// Write one element: the value converted to the kind first — which may
    /// run a program's `valueOf` — then stored, if the index is in bounds.
    pub(super) fn typed_array_write(
        &mut self,
        view: Value,
        index: u32,
        value: Value,
    ) -> Result<(), Completion> {
        let (buffer, kind, offset, _) = self.typed_array_parts(view)?;
        // Immutable bytes take no write.
        if self.array_buffer_immutable(buffer).unwrap_or(false) {
            return Err(self.throw_type_error());
        }
        let raw: u64 = if kind >= 9 {
            let big = self.big_int_of(value)?;
            let number = self.big_int_operand(big)?;
            let magnitude = u64::from(number.limbs.first().copied().unwrap_or(0))
                | (u64::from(number.limbs.get(1).copied().unwrap_or(0)) << 32);
            if number.negative {
                magnitude.wrapping_neg()
            } else {
                magnitude
            }
        } else {
            let number = self.coerce_to_number(value)?;
            match kind {
                0 | 1 => u64::from(value::to_uint32(number) & 0xFF),
                2 => {
                    // Clamped: to the nearest, ties to even, within 0..=255.
                    let clamped = if number.is_nan() || number <= 0.0 {
                        0.0
                    } else if number >= 255.0 {
                        255.0
                    } else {
                        let floor = value::floor(number);
                        let fraction = number - floor;
                        if fraction < 0.5 {
                            floor
                        } else if fraction > 0.5 {
                            floor + 1.0
                        } else if floor - value::floor(floor / 2.0) * 2.0 == 0.0 {
                            floor
                        } else {
                            floor + 1.0
                        }
                    };
                    clamped as u64
                }
                3 | 4 => u64::from(value::to_uint32(number) & 0xFFFF),
                5 | 6 => u64::from(value::to_uint32(number)),
                7 => u64::from(crate::softfloat::to_f32_bits(number)),
                _ => number.to_bits(),
            }
        };
        let Some(count) = self.typed_array_length(view)? else {
            return Ok(());
        };
        if index >= count {
            return Ok(());
        }
        let bytes = self.array_buffer_bytes(buffer)?;
        let size = crate::realm::typed_array_element_size(kind);
        let start = offset + index * size;
        let mut byte = 0u32;
        while byte < size {
            let piece = (raw >> (8 * byte)) & 0xFF;
            self.set_element(bytes, start + byte, Value::number(piece as f64))?;
            byte += 1;
        }
        Ok(())
    }

    /// `%TypedArray%.of`, `from`, the accessors, and the methods.
    pub(super) fn typed_array_native(
        &mut self,
        id: u32,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        match id {
            native::TYPED_ARRAY_OF | native::TYPED_ARRAY_FROM => {
                self.typed_array_from(id, this, first, arguments)
            }
            native::TYPED_ARRAY_LENGTH => {
                let count = self.typed_array_length(this)?.unwrap_or(0);
                Ok(Value::number(f64::from(count)))
            }
            native::TYPED_ARRAY_BYTE_LENGTH => {
                let (_, kind, _, _) = self.typed_array_parts(this)?;
                let count = self.typed_array_length(this)?.unwrap_or(0);
                let size = crate::realm::typed_array_element_size(kind);
                Ok(Value::number(f64::from(count) * f64::from(size)))
            }
            native::TYPED_ARRAY_BYTE_OFFSET => {
                let (_, _, offset, _) = self.typed_array_parts(this)?;
                if self.typed_array_length(this)?.is_none() {
                    return Ok(Value::number(0.0));
                }
                Ok(Value::number(f64::from(offset)))
            }
            native::TYPED_ARRAY_BUFFER => {
                let (buffer, _, _, _) = self.typed_array_parts(this)?;
                Ok(buffer)
            }
            native::TYPED_ARRAY_TAG => {
                if !this.is_object()
                    || object::exotic_kind(self.heap, this.as_handle()).unwrap_or(0)
                        != object::exotic::TYPED_ARRAY
                {
                    return Ok(Value::UNDEFINED);
                }
                let (_, kind, _, _) = self.typed_array_parts(this)?;
                let (name, length) = crate::realm::typed_array_name(kind);
                self.ascii_string(name.get(..length).unwrap_or(&[]))
            }
            native::TYPED_ARRAY_VALUES | native::TYPED_ARRAY_KEYS | native::TYPED_ARRAY_ENTRIES => {
                if self.typed_array_length(this)?.is_none() {
                    return Err(self.throw_type_error());
                }
                let kind = match id {
                    native::TYPED_ARRAY_KEYS => ITERATE_KEYS,
                    native::TYPED_ARRAY_ENTRIES => ITERATE_ENTRIES,
                    _ => ITERATE_VALUES,
                };
                let handle = object::create_iterator(
                    self.heap,
                    Value::object(self.realm.iterator_prototype),
                    this,
                    kind,
                )
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                Ok(Value::object(handle))
            }
            native::TYPED_ARRAY_SUBARRAY => {
                let (buffer, kind, offset, fixed) = self.typed_array_parts(this)?;
                let count = self.typed_array_length(this)?.unwrap_or(0);
                let size = crate::realm::typed_array_element_size(kind);
                let begin = self.relative_index(first, count, 0)?;
                let end_value = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
                let new_fixed = if fixed.is_none() && end_value.is_undefined() {
                    None
                } else {
                    let end = self.relative_index(end_value, count, count)?;
                    Some(end.saturating_sub(begin))
                };
                self.make_typed_array(kind, buffer, offset + begin * size, new_fixed)
            }
            native::TYPED_ARRAY_SET => {
                let target_offset =
                    self.coerce_to_index(arguments.get(1).copied().unwrap_or(Value::UNDEFINED))?;
                let count = self.typed_array_length(this)?.unwrap_or(0);
                let source = self.coerce_to_object(first)?;
                let source_count = if object::exotic_kind(self.heap, source.as_handle())
                    .unwrap_or(0)
                    == object::exotic::TYPED_ARRAY
                {
                    self.typed_array_length(source)?.unwrap_or(0)
                } else {
                    self.length_of(source)?
                };
                if u64::from(target_offset) + u64::from(source_count) > u64::from(count) {
                    return Err(self.throw_error_of(ErrorKind::Range));
                }
                let mut index = 0u32;
                while index < source_count {
                    let held = self.element(source, index)?;
                    self.typed_array_write(this, target_offset + index, held)?;
                    index += 1;
                }
                Ok(Value::UNDEFINED)
            }
            native::TYPED_ARRAY_FILL => {
                let (_, kind, _, _) = self.typed_array_parts(this)?;
                let count = self.typed_array_length(this)?.unwrap_or(0);
                let value = if kind >= 9 {
                    self.big_int_of(first)?
                } else {
                    Value::number(self.coerce_to_number(first)?)
                };
                let start = self.relative_index(
                    arguments.get(1).copied().unwrap_or(Value::UNDEFINED),
                    count,
                    0,
                )?;
                let end = self.relative_index(
                    arguments.get(2).copied().unwrap_or(Value::UNDEFINED),
                    count,
                    count,
                )?;
                let mut index = start;
                while index < end {
                    self.typed_array_write(this, index, value)?;
                    index += 1;
                }
                Ok(this)
            }
            native::ARRAY_BUFFER_RESIZE => {
                if self.array_buffer_immutable(this)? {
                    return Err(self.throw_type_error());
                }
                self.array_buffer_resize(this, first)?;
                Ok(Value::UNDEFINED)
            }
            native::ARRAY_BUFFER_IMMUTABLE => {
                Ok(Value::boolean(self.array_buffer_immutable(this)?))
            }
            native::ARRAY_BUFFER_TRANSFER_TO_IMMUTABLE => {
                self.array_buffer_transfer_to_immutable(this)
            }
            native::ARRAY_BUFFER_BYTE_LENGTH => {
                let bytes = self.array_buffer_bytes(this)?;
                let count = self.length_of(bytes)?;
                Ok(Value::number(f64::from(count)))
            }
            native::ARRAY_BUFFER_MAX_BYTE_LENGTH => {
                let maximum = self.array_buffer_maximum(this)?;
                if maximum.is_undefined() {
                    let bytes = self.array_buffer_bytes(this)?;
                    let count = self.length_of(bytes)?;
                    return Ok(Value::number(f64::from(count)));
                }
                Ok(maximum)
            }
            native::ARRAY_BUFFER_RESIZABLE => {
                let maximum = self.array_buffer_maximum(this)?;
                Ok(Value::boolean(!maximum.is_undefined()))
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
    }

    /// `%TypedArray%.of` and `.from`: gather the values, map them where a
    /// mapper was given, and construct the receiver over the count.
    pub(super) fn typed_array_from(
        &mut self,
        id: u32,
        this: Value,
        first: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        if !this.is_object()
            || !object::is_constructor(self.heap, this.as_handle()).unwrap_or(false)
        {
            return Err(self.throw_type_error());
        }
        let values = self.new_array()?;
        if id == native::TYPED_ARRAY_OF {
            for &argument in arguments {
                self.append_element(values, Some(argument))?;
            }
        } else {
            let map = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
            if !map.is_undefined() && !self.is_callable_value(map) {
                return Err(self.throw_type_error());
            }
            let receiver = arguments.get(2).copied().unwrap_or(Value::UNDEFINED);
            if let Some(iterator) = self.iterator_of(first)? {
                while let Some(element) = self.iterator_step(iterator)? {
                    self.append_element(values, Some(element))?;
                }
            } else {
                let source = self.coerce_to_object(first)?;
                let count = self.length_of(source)?;
                let mut index = 0u32;
                while index < count {
                    let held = self.element(source, index)?;
                    self.append_element(values, Some(held))?;
                    index += 1;
                }
            }
            if !map.is_undefined() {
                let count = self.length_of(values)?;
                let mut index = 0u32;
                while index < count {
                    let held = self.element(values, index)?;
                    let position = Value::number(f64::from(index));
                    let mapped = self.call_value(map, receiver, &[held, position])?;
                    self.set_element(values, index, mapped)?;
                    index += 1;
                }
            }
        }
        let count = self.length_of(values)?;
        self.pending_new_target = this;
        let made = self.construct(this, &[Value::number(f64::from(count))])?;
        let mut index = 0u32;
        while index < count {
            let held = self.element(values, index)?;
            let key = Key::Index(index);
            self.set_property(made, key, held)?;
            index += 1;
        }
        Ok(made)
    }

    /// The bytes move to a fresh buffer nothing can change; the old buffer
    /// keeps them, its claim on immutability gone — detachment is not
    /// modelled here, only the new buffer's permanence is.
    pub(super) fn array_buffer_transfer_to_immutable(
        &mut self,
        this: Value,
    ) -> Result<Value, Completion> {
        // The bytes move to a fresh buffer nothing can change; the
        // old buffer keeps them, its claim on immutability gone —
        // detachment is not modelled here, only the new buffer's
        // permanence is.
        let bytes = self.array_buffer_bytes(this)?;
        let count = self.length_of(bytes)?;
        let held = object::create(self.heap, Value::object(self.realm.array_buffer_prototype))
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let target = Value::object(held);
        object::set_exotic_kind(self.heap, held, object::exotic::ARRAY_BUFFER)
            .map_err(|_| Completion::MALFORMED)?;
        let copied = self.new_array()?;
        let mut index = 0u32;
        while index < count {
            let value = self.element(bytes, index)?;
            self.set_element(copied, index, value)?;
            index += 1;
        }
        self.set_length(copied, count)?;
        let bytes_key = self.ascii_key(b"\0bytes")?;
        object::define_own_property(self.heap, held, bytes_key, Descriptor::data(copied, 0))
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let marker = self.ascii_key(b"\0immutable")?;
        object::define_own_property(
            self.heap,
            held,
            marker,
            Descriptor::data(Value::boolean(true), 0),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(target)
    }
}
