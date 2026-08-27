// The functions the engine implements itself.
//
// This file is included by `vm.rs` and extends the machine with the native
// dispatch: one method per library area, and the entry that routes a native
// id to its area. Everything here runs on the host's stack under the
// machine's own fuel, like any other native.

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// The natives of `Object`.
    /// The URI codecs: percent-encoding over UTF-8, with each function's
    /// own set of untouched characters, and a URIError for a malformed
    /// escape or a lone surrogate.
    fn uri_native(&mut self, id: u32, value: Value) -> Result<Value, Completion> {
        let text = self.coerce_to_string(value)?;
        let handle = text.as_handle();
        let length = crate::string::length(self.heap, handle)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?
            as usize;
        let mut units = [0u16; 1024];
        if length > units.len() {
            return Err(Completion::Terminated(Termination::HeapExhausted));
        }
        crate::string::copy_units(self.heap, handle, &mut units[..length])
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        let mut out = [0u16; 3072];
        let mut written = 0usize;
        let decode = matches!(id, native::DECODE_URI | native::DECODE_URI_COMPONENT);
        let unreserved = |unit: u16, component: bool| -> bool {
            let byte = unit as u8;
            if unit > 0x7F {
                return false;
            }
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
                )
                || (!component
                    && matches!(
                        byte,
                        b';' | b'/' | b'?' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b',' | b'#'
                    ))
        };
        let reserved_kept = |byte: u8, component: bool| -> bool {
            !component
                && matches!(
                    byte,
                    b';' | b'/' | b'?' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b',' | b'#'
                )
        };
        let component = matches!(
            id,
            native::ENCODE_URI_COMPONENT | native::DECODE_URI_COMPONENT
        );
        let uri_error = |vm: &mut Self| -> Completion {
            let reason = match vm.create_error(ErrorKind::Uri, Value::UNDEFINED) {
                Ok(reason) => reason,
                Err(completion) => return completion,
            };
            Completion::Throw(reason)
        };
        let mut push = |slot: u16, written: &mut usize| -> bool {
            if let Some(cell) = out.get_mut(*written) {
                *cell = slot;
                *written += 1;
                true
            } else {
                false
            }
        };
        if decode {
            let hex = |unit: u16| -> Option<u8> {
                match unit {
                    0x30..=0x39 => Some(unit as u8 - b'0'),
                    0x41..=0x46 => Some(unit as u8 - b'A' + 10),
                    0x61..=0x66 => Some(unit as u8 - b'a' + 10),
                    _ => None,
                }
            };
            let mut index = 0usize;
            while index < length {
                let unit = units[index];
                if unit != u16::from(b'%') {
                    if !push(unit, &mut written) {
                        return Err(Completion::Terminated(Termination::HeapExhausted));
                    }
                    index += 1;
                    continue;
                }
                let byte_at = |at: usize| -> Option<u8> {
                    if at + 2 < length && units[at] == u16::from(b'%') {
                        Some((hex(units[at + 1])? << 4) | hex(units[at + 2])?)
                    } else {
                        None
                    }
                };
                let Some(first_byte) = byte_at(index) else {
                    return Err(uri_error(self));
                };
                let count = if first_byte < 0x80 {
                    1
                } else if (0xC2..0xE0).contains(&first_byte) {
                    2
                } else if (0xE0..0xF0).contains(&first_byte) {
                    3
                } else if (0xF0..0xF5).contains(&first_byte) {
                    4
                } else {
                    return Err(uri_error(self));
                };
                if count == 1 {
                    if reserved_kept(first_byte, component) {
                        // decodeURI leaves an escaped reserved character as
                        // its escape.
                        for offset in 0..3 {
                            if !push(units[index + offset], &mut written) {
                                return Err(Completion::Terminated(Termination::HeapExhausted));
                            }
                        }
                    } else if !push(u16::from(first_byte), &mut written) {
                        return Err(Completion::Terminated(Termination::HeapExhausted));
                    }
                    index += 3;
                    continue;
                }
                let mut point = u32::from(first_byte & (0x7F >> count));
                let mut offset = 3usize;
                let mut trailing = 1usize;
                while trailing < count {
                    let Some(byte) = byte_at(index + offset) else {
                        return Err(uri_error(self));
                    };
                    if byte & 0xC0 != 0x80 {
                        return Err(uri_error(self));
                    }
                    point = (point << 6) | u32::from(byte & 0x3F);
                    offset += 3;
                    trailing += 1;
                }
                if point > 0x10FFFF || (0xD800..0xE000).contains(&point) {
                    return Err(uri_error(self));
                }
                if point > 0xFFFF {
                    let bias = point - 0x10000;
                    if !push(0xD800 + (bias >> 10) as u16, &mut written)
                        || !push(0xDC00 + (bias & 0x3FF) as u16, &mut written)
                    {
                        return Err(Completion::Terminated(Termination::HeapExhausted));
                    }
                } else if !push(point as u16, &mut written) {
                    return Err(Completion::Terminated(Termination::HeapExhausted));
                }
                index += 3 * count;
            }
        } else {
            let hex_digit = |value: u8| -> u16 {
                u16::from(if value < 10 {
                    b'0' + value
                } else {
                    b'A' + value - 10
                })
            };
            let mut index = 0usize;
            while index < length {
                let unit = units[index];
                if unreserved(unit, component) {
                    if !push(unit, &mut written) {
                        return Err(Completion::Terminated(Termination::HeapExhausted));
                    }
                    index += 1;
                    continue;
                }
                // The unit — or a pair — becomes UTF-8 percent escapes.
                let point = if (0xD800..0xDC00).contains(&unit) {
                    let Some(&low) = units.get(index + 1) else {
                        return Err(uri_error(self));
                    };
                    if !(0xDC00..0xE000).contains(&low) || index + 1 >= length {
                        return Err(uri_error(self));
                    }
                    index += 2;
                    0x10000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(low) - 0xDC00)
                } else if (0xDC00..0xE000).contains(&unit) {
                    return Err(uri_error(self));
                } else {
                    index += 1;
                    u32::from(unit)
                };
                let mut bytes = [0u8; 4];
                let encoded = char::from_u32(point)
                    .map(|c| c.encode_utf8(&mut bytes).len())
                    .unwrap_or(0);
                for &byte in bytes.get(..encoded).unwrap_or(&[]) {
                    if !push(u16::from(b'%'), &mut written)
                        || !push(hex_digit(byte >> 4), &mut written)
                        || !push(hex_digit(byte & 0xF), &mut written)
                    {
                        return Err(Completion::Terminated(Termination::HeapExhausted));
                    }
                }
            }
        }
        let made = crate::string::create(self.heap, out.get(..written).unwrap_or(&[]))
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(Value::string(made))
    }

    /// Whether two values are the same for a keyed collection: strict
    /// equality with NaN equal to itself.
    fn same_value_zero(&mut self, left: Value, right: Value) -> Result<bool, Completion> {
        if self.strict_equals(left, right)? {
            return Ok(true);
        }
        let left_nan = matches!(left.tag(), Tag::Number) && left.as_number().is_nan();
        let right_nan = matches!(right.tag(), Tag::Number) && right.as_number().is_nan();
        Ok(left_nan && right_nan)
    }

    /// The two hidden arrays behind a Map, or the one behind a Set.
    fn collection_arrays(
        &mut self,
        target: Value,
        make: bool,
    ) -> Result<Option<(Value, Value)>, Completion> {
        self.branded_arrays(target, make, false)
    }

    /// The hidden arrays under either brand: `weak` names a WeakMap or
    /// WeakSet, whose methods must not answer for a Map or Set.
    fn branded_arrays(
        &mut self,
        target: Value,
        make: bool,
        weak: bool,
    ) -> Result<Option<(Value, Value)>, Completion> {
        if !target.is_object() {
            return Err(self.throw_type_error());
        }
        let keys_key = self.ascii_key(if weak { b"\0wk" } else { b"\0ck" })?;
        let vals_key = self.ascii_key(if weak { b"\0wv" } else { b"\0cv" })?;
        let held = object::get_own_property(self.heap, target.as_handle(), keys_key)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        if let Some(descriptor) = held {
            let vals = object::get_own_property(self.heap, target.as_handle(), vals_key)
                .map_err(|_| Completion::Terminated(Termination::Malformed))?
                .map_or(Value::UNDEFINED, |found| found.value);
            return Ok(Some((descriptor.value, vals)));
        }
        if !make {
            return Ok(None);
        }
        let keys = self.new_array()?;
        let vals = self.new_array()?;
        for (key, value) in [(keys_key, keys), (vals_key, vals)] {
            object::define_own_property(
                self.heap,
                target.as_handle(),
                key,
                Descriptor::data(value, attribute::WRITABLE),
            )
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        }
        Ok(Some((keys, vals)))
    }

    /// Where `key` sits in the collection's key array, if it is a member.
    fn collection_find(&mut self, keys: Value, key: Value) -> Result<Option<u32>, Completion> {
        let count = self.length_of(keys)?;
        let mut index = 0u32;
        while index < count {
            let held = self.element(keys, index)?;
            if self.same_value_zero(held, key)? {
                return Ok(Some(index));
            }
            index += 1;
        }
        Ok(None)
    }

    /// Remove index `at` from a collection array, keeping insertion order.
    fn collection_remove(&mut self, array: Value, at: u32) -> Result<(), Completion> {
        let count = self.length_of(array)?;
        let mut index = at;
        while index + 1 < count {
            let next = self.element(array, index + 1)?;
            self.set_element(array, index, next)?;
            index += 1;
        }
        self.set_length(array, count.saturating_sub(1))?;
        Ok(())
    }

    /// `Map` and `Set`: deterministic keyed collections over hidden arrays.
    fn collection_native(
        &mut self,
        id: u32,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        match id {
            native::MAP | native::SET => {
                let map = id == native::MAP;
                let prototype = if map {
                    self.realm.map_prototype
                } else {
                    self.realm.set_prototype
                };
                let instance = object::create(self.heap, Value::object(prototype))
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                let instance = Value::object(instance);
                self.collection_arrays(instance, true)?;
                if !first.is_nullish() {
                    let Some(iterator) = self.iterator_of(first)? else {
                        return Err(self.throw_type_error());
                    };
                    while let Some(element) = self.iterator_step(iterator)? {
                        if map {
                            if !element.is_object() {
                                return Err(self.throw_type_error());
                            }
                            let key = self.element(element, 0)?;
                            let value = self.element(element, 1)?;
                            self.collection_native(native::MAP_SET, instance, &[key, value])?;
                        } else {
                            self.collection_native(native::SET_ADD, instance, &[element])?;
                        }
                    }
                }
                Ok(instance)
            }
            native::MAP_SET | native::SET_ADD => {
                let Some((keys, vals)) = self.collection_arrays(this, false)? else {
                    return Err(self.throw_type_error());
                };
                let value = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
                match self.collection_find(keys, first)? {
                    Some(at) if id == native::MAP_SET => {
                        self.set_element(vals, at, value)?;
                    }
                    Some(_) => {}
                    None => {
                        self.append_element(keys, Some(first))?;
                        if id == native::MAP_SET {
                            self.append_element(vals, Some(value))?;
                        }
                    }
                }
                Ok(this)
            }
            native::MAP_GET => {
                let Some((keys, vals)) = self.collection_arrays(this, false)? else {
                    return Err(self.throw_type_error());
                };
                match self.collection_find(keys, first)? {
                    Some(at) => self.element(vals, at),
                    None => Ok(Value::UNDEFINED),
                }
            }
            native::MAP_HAS | native::SET_HAS => {
                let Some((keys, _)) = self.collection_arrays(this, false)? else {
                    return Err(self.throw_type_error());
                };
                Ok(Value::boolean(self.collection_find(keys, first)?.is_some()))
            }
            native::MAP_DELETE | native::SET_DELETE => {
                let Some((keys, vals)) = self.collection_arrays(this, false)? else {
                    return Err(self.throw_type_error());
                };
                match self.collection_find(keys, first)? {
                    Some(at) => {
                        self.collection_remove(keys, at)?;
                        if id == native::MAP_DELETE {
                            self.collection_remove(vals, at)?;
                        }
                        Ok(Value::boolean(true))
                    }
                    None => Ok(Value::boolean(false)),
                }
            }
            native::MAP_CLEAR | native::SET_CLEAR => {
                let Some((keys, vals)) = self.collection_arrays(this, false)? else {
                    return Err(self.throw_type_error());
                };
                self.set_length(keys, 0)?;
                if id == native::MAP_CLEAR {
                    self.set_length(vals, 0)?;
                }
                Ok(Value::UNDEFINED)
            }
            native::MAP_SIZE | native::SET_SIZE => {
                let Some((keys, _)) = self.collection_arrays(this, false)? else {
                    return Err(self.throw_type_error());
                };
                let count = self.length_of(keys)?;
                Ok(Value::number(f64::from(count)))
            }
            native::MAP_FOR_EACH | native::SET_FOR_EACH => {
                let Some((keys, vals)) = self.collection_arrays(this, false)? else {
                    return Err(self.throw_type_error());
                };
                if !self.is_callable_value(first) {
                    return Err(self.throw_type_error());
                }
                let receiver = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
                let mut index = 0u32;
                while index < self.length_of(keys)? {
                    let key = self.element(keys, index)?;
                    let value = if id == native::MAP_FOR_EACH {
                        self.element(vals, index)?
                    } else {
                        key
                    };
                    self.call_value(first, receiver, &[value, key, this])?;
                    index += 1;
                }
                Ok(Value::UNDEFINED)
            }
            native::MAP_ENTRIES
            | native::SET_ENTRIES
            | native::MAP_KEYS
            | native::MAP_VALUES
            | native::SET_VALUES => {
                let Some((keys, vals)) = self.collection_arrays(this, false)? else {
                    return Err(self.throw_type_error());
                };
                // The iterator walks the live storage: what the collection
                // gains or loses mid-iteration is what a walker sees.
                let (target, kind) = match id {
                    native::MAP_ENTRIES => (this, ITERATE_MAP_ENTRIES),
                    native::SET_ENTRIES => (this, ITERATE_SET_ENTRIES),
                    native::MAP_VALUES => (vals, ITERATE_VALUES),
                    _ => (keys, ITERATE_VALUES),
                };
                let handle = object::create_iterator(
                    self.heap,
                    Value::object(self.realm.iterator_prototype),
                    target,
                    kind,
                )
                .map_err(|_| self.heap_failure())?;
                Ok(Value::object(handle))
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
    }

    /// `WeakMap` and `WeakSet`: a key is an object or a symbol, and nothing
    /// else, since only those could ever be held weakly. This engine never
    /// observes a collection, so a member stays until deleted.
    fn weak_collection_native(
        &mut self,
        id: u32,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        let holdable = first.is_object() || matches!(first.tag(), Tag::Symbol);
        match id {
            native::WEAK_MAP | native::WEAK_SET => {
                let map = id == native::WEAK_MAP;
                let prototype = if map {
                    self.realm.weak_map_prototype
                } else {
                    self.realm.weak_set_prototype
                };
                let instance = object::create(self.heap, Value::object(prototype))
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                let instance = Value::object(instance);
                self.branded_arrays(instance, true, true)?;
                if !first.is_nullish() {
                    let Some(iterator) = self.iterator_of(first)? else {
                        return Err(self.throw_type_error());
                    };
                    while let Some(element) = self.iterator_step(iterator)? {
                        if map {
                            if !element.is_object() {
                                return Err(self.throw_type_error());
                            }
                            let key = self.element(element, 0)?;
                            let value = self.element(element, 1)?;
                            self.weak_collection_native(
                                native::WEAK_MAP_SET,
                                instance,
                                &[key, value],
                            )?;
                        } else {
                            self.weak_collection_native(
                                native::WEAK_SET_ADD,
                                instance,
                                &[element],
                            )?;
                        }
                    }
                }
                Ok(instance)
            }
            native::WEAK_MAP_SET | native::WEAK_SET_ADD => {
                let Some((keys, vals)) = self.branded_arrays(this, false, true)? else {
                    return Err(self.throw_type_error());
                };
                if !holdable {
                    return Err(self.throw_type_error());
                }
                let value = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
                match self.collection_find(keys, first)? {
                    Some(at) if id == native::WEAK_MAP_SET => {
                        self.set_element(vals, at, value)?;
                    }
                    Some(_) => {}
                    None => {
                        self.append_element(keys, Some(first))?;
                        if id == native::WEAK_MAP_SET {
                            self.append_element(vals, Some(value))?;
                        }
                    }
                }
                Ok(this)
            }
            native::WEAK_MAP_GET => {
                let Some((keys, vals)) = self.branded_arrays(this, false, true)? else {
                    return Err(self.throw_type_error());
                };
                if !holdable {
                    return Ok(Value::UNDEFINED);
                }
                match self.collection_find(keys, first)? {
                    Some(at) => self.element(vals, at),
                    None => Ok(Value::UNDEFINED),
                }
            }
            native::WEAK_MAP_HAS | native::WEAK_SET_HAS => {
                let Some((keys, _)) = self.branded_arrays(this, false, true)? else {
                    return Err(self.throw_type_error());
                };
                if !holdable {
                    return Ok(Value::boolean(false));
                }
                Ok(Value::boolean(self.collection_find(keys, first)?.is_some()))
            }
            native::WEAK_MAP_DELETE | native::WEAK_SET_DELETE => {
                let Some((keys, vals)) = self.branded_arrays(this, false, true)? else {
                    return Err(self.throw_type_error());
                };
                if !holdable {
                    return Ok(Value::boolean(false));
                }
                match self.collection_find(keys, first)? {
                    Some(at) => {
                        self.collection_remove(keys, at)?;
                        if id == native::WEAK_MAP_DELETE {
                            self.collection_remove(vals, at)?;
                        }
                        Ok(Value::boolean(true))
                    }
                    None => Ok(Value::boolean(false)),
                }
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
    }

    /// `Reflect`: the object operations, refusing anything but an object.
    fn reflect_native(&mut self, id: u32, arguments: &[Value]) -> Result<Value, Completion> {
        let target = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        if !target.is_object() {
            return Err(self.throw_type_error());
        }
        let second = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
        match id {
            native::REFLECT_GET => {
                let key = self.coerce_to_key(second)?;
                if self.hidden_key(key) {
                    return Ok(Value::UNDEFINED);
                }
                let receiver = arguments.get(2).copied().unwrap_or(target);
                self.super_get(target, key, receiver)
            }
            native::REFLECT_SET => {
                let key = self.coerce_to_key(second)?;
                let value = arguments.get(2).copied().unwrap_or(Value::UNDEFINED);
                // A receiver of its own routes the write through OrdinarySet:
                // the receiver's own record, or a proxy's traps, take it.
                if let Some(&receiver) = arguments.get(3) {
                    if !(receiver.is_object() && receiver.as_handle() == target.as_handle()) {
                        let done = self.set_with_receiver(target, key, value, receiver)?;
                        return Ok(Value::boolean(done));
                    }
                }
                // A namespace refuses every write; the refusal is the answer,
                // not a throw — though a binding in its dead zone still is.
                if matches!(
                    object::exotic_kind(self.heap, target.as_handle()).unwrap_or(0),
                    object::exotic::NAMESPACE | object::exotic::DEFERRED
                ) {
                    if self.deferred_done(target) {
                        self.namespace_touch(target, key)?;
                    }
                    return Ok(Value::boolean(false));
                }
                self.set_property_of(target, key, value, false)?;
                Ok(Value::boolean(true))
            }
            native::REFLECT_HAS => {
                let key = self.coerce_to_key(second)?;
                let present = !self.hidden_key(key)
                    && object::has_property(self.heap, target.as_handle(), key)
                        .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                Ok(Value::boolean(present))
            }
            native::REFLECT_DELETE => {
                let key = self.coerce_to_key(second)?;
                self.materialise_function_facts(target, key)?;
                let removed = object::delete(self.heap, target.as_handle(), key)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                Ok(Value::boolean(removed))
            }
            native::REFLECT_OWN_KEYS => self.own_entries(target, native::REFLECT_OWN_KEYS),
            native::REFLECT_GET_PROTOTYPE => object::prototype(self.heap, target.as_handle())
                .map_err(|_| Completion::Terminated(Termination::Malformed)),
            native::REFLECT_SET_PROTOTYPE => {
                if !second.is_object() && !second.is_null() {
                    return Err(self.throw_type_error());
                }
                let admitted = object::set_prototype(self.heap, target.as_handle(), second)
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                Ok(Value::boolean(admitted))
            }
            native::REFLECT_IS_EXTENSIBLE => {
                let extensible = object::is_extensible(self.heap, target.as_handle())
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                Ok(Value::boolean(extensible))
            }
            native::REFLECT_PREVENT_EXTENSIONS => {
                object::prevent_extensions(self.heap, target.as_handle())
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                Ok(Value::boolean(true))
            }
            native::REFLECT_DEFINE_PROPERTY => {
                match self.object_native(
                    native::OBJECT_DEFINE_PROPERTY,
                    Value::UNDEFINED,
                    arguments,
                ) {
                    Ok(_) => Ok(Value::boolean(true)),
                    Err(Completion::Throw(_)) => Ok(Value::boolean(false)),
                    Err(other) => Err(other),
                }
            }
            native::REFLECT_GET_OWN_DESCRIPTOR => self.object_native(
                native::OBJECT_GET_OWN_PROPERTY_DESCRIPTOR,
                Value::UNDEFINED,
                arguments,
            ),
            native::REFLECT_APPLY => {
                let list = arguments.get(2).copied().unwrap_or(Value::UNDEFINED);
                let mut values = [Value::UNDEFINED; MAX_ARGUMENTS];
                let count = if list.is_object() {
                    let length = self.length_of(list)?;
                    let count = (length as usize).min(values.len());
                    let mut index = 0usize;
                    while index < count {
                        values[index] = self.element(list, u32::try_from(index).unwrap_or(0))?;
                        index += 1;
                    }
                    count
                } else {
                    0
                };
                self.call_value(target, second, &values[..count])
            }
            native::REFLECT_CONSTRUCT => {
                let list = second;
                let mut values = [Value::UNDEFINED; MAX_ARGUMENTS];
                let count = if list.is_object() {
                    let length = self.length_of(list)?;
                    let count = (length as usize).min(values.len());
                    let mut index = 0usize;
                    while index < count {
                        values[index] = self.element(list, u32::try_from(index).unwrap_or(0))?;
                        index += 1;
                    }
                    count
                } else {
                    0
                };
                if let Some(&named) = arguments.get(2) {
                    if !named.is_object()
                        || !object::is_constructor(self.heap, named.as_handle()).unwrap_or(false)
                    {
                        return Err(self.throw_type_error());
                    }
                    self.pending_new_target = named;
                }
                self.construct(target, &values[..count])
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
    }

    fn object_native(
        &mut self,
        id: u32,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        let second = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
        match id {
            native::OBJECT => {
                if first.is_nullish() {
                    let object =
                        object::create(self.heap, Value::object(self.realm.object_prototype))
                            .map_err(|_| self.heap_failure())?;
                    return Ok(Value::object(object));
                }
                self.coerce_to_object(first)
            }
            native::OBJECT_KEYS | native::OBJECT_VALUES | native::OBJECT_ENTRIES => {
                self.own_entries(first, id)
            }
            native::OBJECT_ASSIGN => {
                let target = self.coerce_to_object(first)?;
                for &source in arguments.get(1..).unwrap_or(&[]) {
                    if source.is_nullish() {
                        continue;
                    }
                    let source = self.coerce_to_object(source)?;
                    let mut keys = [Key::Index(0); MAX_OWN_KEYS];
                    let count = object::own_keys(self.heap, source.as_handle(), &mut keys)
                        .map_err(Self::key_failure)?;
                    for &key in keys.get(..count).unwrap_or(&[]) {
                        if !self.is_enumerable(source, key)? {
                            continue;
                        }
                        let value = self.get_property(source, key)?;
                        self.set_property(target, key, value)?;
                    }
                }
                Ok(target)
            }
            native::OBJECT_PREVENT_EXTENSIONS => {
                if first.is_object() {
                    object::prevent_extensions(self.heap, first.as_handle())
                        .map_err(|_| self.heap_failure())?;
                }
                Ok(first)
            }
            native::OBJECT_IS_EXTENSIBLE => {
                let extensible = first.is_object()
                    && object::is_extensible(self.heap, first.as_handle())
                        .map_err(|_| self.heap_failure())?;
                Ok(Value::boolean(extensible))
            }
            native::OBJECT_SEAL => {
                if first.is_object() {
                    object::prevent_extensions(self.heap, first.as_handle())
                        .map_err(|_| self.heap_failure())?;
                    let mut keys = [Key::Index(0); MAX_OWN_KEYS];
                    let count = object::own_keys(self.heap, first.as_handle(), &mut keys)
                        .map_err(Self::key_failure)?;
                    for &key in keys.get(..count).unwrap_or(&[]) {
                        let Some(descriptor) =
                            object::get_own_property(self.heap, first.as_handle(), key)
                                .map_err(|_| self.heap_failure())?
                        else {
                            continue;
                        };
                        let sealed = Descriptor {
                            attributes: descriptor.attributes & !attribute::CONFIGURABLE,
                            ..descriptor
                        };
                        object::define_own_property(self.heap, first.as_handle(), key, sealed)
                            .map_err(|_| self.heap_failure())?;
                    }
                }
                Ok(first)
            }
            native::OBJECT_IS_SEALED => {
                if !first.is_object() {
                    return Ok(Value::boolean(true));
                }
                let handle = first.as_handle();
                if object::is_extensible(self.heap, handle).map_err(|_| self.heap_failure())? {
                    return Ok(Value::boolean(false));
                }
                let mut keys = [Key::Index(0); MAX_OWN_KEYS];
                let count =
                    object::own_keys(self.heap, handle, &mut keys).map_err(Self::key_failure)?;
                for &key in keys.get(..count).unwrap_or(&[]) {
                    let Some(descriptor) = object::get_own_property(self.heap, handle, key)
                        .map_err(|_| self.heap_failure())?
                    else {
                        continue;
                    };
                    if descriptor.attributes & attribute::CONFIGURABLE != 0 {
                        return Ok(Value::boolean(false));
                    }
                }
                Ok(Value::boolean(true))
            }
            native::OBJECT_FREEZE => {
                if first.is_object() {
                    // A namespace's exports stay writable: freezing asks for
                    // writable false on each, which its definition refuses.
                    if matches!(
                        object::exotic_kind(self.heap, first.as_handle()).unwrap_or(0),
                        object::exotic::NAMESPACE | object::exotic::DEFERRED
                    ) {
                        self.deferred_trigger(first, None)?;
                        let mut keys = [Key::Index(0); MAX_OWN_KEYS];
                        let count = object::own_keys(self.heap, first.as_handle(), &mut keys)
                            .map_err(Self::key_failure)?;
                        for &key in keys.get(..count).unwrap_or(&[]) {
                            if !matches!(key, Key::Symbol(_)) && !self.hidden_key(key) {
                                return Err(self.throw_type_error());
                            }
                        }
                        return Ok(first);
                    }
                    object::prevent_extensions(self.heap, first.as_handle())
                        .map_err(|_| self.heap_failure())?;
                    let mut keys = [Key::Index(0); MAX_OWN_KEYS];
                    let count = object::own_keys(self.heap, first.as_handle(), &mut keys)
                        .map_err(Self::key_failure)?;
                    for &key in keys.get(..count).unwrap_or(&[]) {
                        let Some(descriptor) =
                            object::get_own_property(self.heap, first.as_handle(), key)
                                .map_err(|_| self.heap_failure())?
                        else {
                            continue;
                        };
                        let frozen = Descriptor {
                            attributes: descriptor.attributes
                                & !(attribute::WRITABLE | attribute::CONFIGURABLE),
                            ..descriptor
                        };
                        object::define_own_property(self.heap, first.as_handle(), key, frozen)
                            .map_err(|_| self.heap_failure())?;
                    }
                }
                Ok(first)
            }
            native::OBJECT_IS_FROZEN => {
                if !first.is_object() {
                    return Ok(Value::boolean(true));
                }
                // A namespace with any export holds a writable name.
                if matches!(
                    object::exotic_kind(self.heap, first.as_handle()).unwrap_or(0),
                    object::exotic::NAMESPACE | object::exotic::DEFERRED
                ) {
                    let mut keys = [Key::Index(0); MAX_OWN_KEYS];
                    let count = object::own_keys(self.heap, first.as_handle(), &mut keys)
                        .map_err(Self::key_failure)?;
                    for &key in keys.get(..count).unwrap_or(&[]) {
                        if !matches!(key, Key::Symbol(_)) && !self.hidden_key(key) {
                            return Ok(Value::boolean(false));
                        }
                    }
                    return Ok(Value::boolean(true));
                }
                if object::is_extensible(self.heap, first.as_handle())
                    .map_err(|_| self.heap_failure())?
                {
                    return Ok(Value::boolean(false));
                }
                let mut keys = [Key::Index(0); MAX_OWN_KEYS];
                let count = object::own_keys(self.heap, first.as_handle(), &mut keys)
                    .map_err(Self::key_failure)?;
                for &key in keys.get(..count).unwrap_or(&[]) {
                    let Some(descriptor) =
                        object::get_own_property(self.heap, first.as_handle(), key)
                            .map_err(|_| self.heap_failure())?
                    else {
                        continue;
                    };
                    if descriptor.has(attribute::WRITABLE)
                        || descriptor.has(attribute::CONFIGURABLE)
                    {
                        return Ok(Value::boolean(false));
                    }
                }
                Ok(Value::boolean(true))
            }
            native::OBJECT_GET_PROTOTYPE_OF => {
                let object = self.coerce_to_object(first)?;
                object::prototype(self.heap, object.as_handle()).map_err(|_| self.heap_failure())
            }
            native::OBJECT_SET_PROTOTYPE_OF => {
                if first.is_object() {
                    let admitted = object::set_prototype(self.heap, first.as_handle(), second)
                        .map_err(|_| self.heap_failure())?;
                    if !admitted {
                        // A prototype refused — a non-extensible receiver,
                        // a namespace — is the TypeError the caller gets.
                        return Err(self.throw_type_error());
                    }
                }
                Ok(first)
            }
            native::OBJECT_DEFINE_PROPERTIES => {
                if !first.is_object() {
                    return Err(self.throw_type_error());
                }
                let properties = self.coerce_to_object(second)?;
                let mut keys = [Key::Index(0); MAX_OWN_KEYS];
                let count = object::own_keys(self.heap, properties.as_handle(), &mut keys)
                    .map_err(Self::key_failure)?;
                for &key in keys.get(..count).unwrap_or(&[]) {
                    if !self.is_enumerable(properties, key)? {
                        continue;
                    }
                    let descriptor = self.get_property(properties, key)?;
                    let name = self.key_to_value(key)?;
                    self.object_native(
                        native::OBJECT_DEFINE_PROPERTY,
                        Value::UNDEFINED,
                        &[first, name, descriptor],
                    )?;
                }
                Ok(first)
            }
            native::OBJECT_DEFINE_PROPERTY => {
                if !first.is_object() {
                    return Err(self.throw_type_error());
                }
                let key = self.coerce_to_key(second)?;
                self.materialise_function_facts(first, key)?;
                let descriptor = arguments.get(2).copied().unwrap_or(Value::UNDEFINED);
                if !descriptor.is_object() {
                    return Err(self.throw_type_error());
                }
                // A namespace admits a definition only when it changes
                // nothing: the request must match the property exactly as
                // it stands, and anything else — a missing name included —
                // is refused.
                if matches!(
                    object::exotic_kind(self.heap, first.as_handle()).unwrap_or(0),
                    object::exotic::NAMESPACE | object::exotic::DEFERRED
                ) {
                    self.deferred_trigger(first, Some(key))?;
                    let own = !self.hidden_key(key)
                        && object::get_own_property(self.heap, first.as_handle(), key)
                            .map_err(|_| self.heap_failure())?
                            .is_some();
                    if !own {
                        return Err(self.throw_type_error());
                    }
                    for name in [&b"get"[..], &b"set"[..]] {
                        let field = self.ascii_key(name)?;
                        if object::has_property(self.heap, descriptor.as_handle(), field)
                            .map_err(|_| self.heap_failure())?
                        {
                            return Err(self.throw_type_error());
                        }
                    }
                    let (expected_value, expected_attributes) = if matches!(key, Key::Symbol(_)) {
                        let found = object::get_own_property(self.heap, first.as_handle(), key)
                            .map_err(|_| self.heap_failure())?
                            .unwrap_or(Descriptor::data(Value::UNDEFINED, 0));
                        (found.value, found.attributes)
                    } else {
                        // Reading surfaces the ReferenceError a binding
                        // still in its dead zone throws.
                        (
                            self.get_property(first, key)?,
                            attribute::WRITABLE | attribute::ENUMERABLE,
                        )
                    };
                    for (name, bit) in [
                        (&b"writable"[..], attribute::WRITABLE),
                        (&b"enumerable"[..], attribute::ENUMERABLE),
                        (&b"configurable"[..], attribute::CONFIGURABLE),
                    ] {
                        let field = self.ascii_key(name)?;
                        if object::has_property(self.heap, descriptor.as_handle(), field)
                            .map_err(|_| self.heap_failure())?
                        {
                            let flag = self.get_property(descriptor, field)?;
                            let wanted = self.coerce_to_boolean(flag)?;
                            if wanted != (expected_attributes & bit != 0) {
                                return Err(self.throw_type_error());
                            }
                        }
                    }
                    let field = self.ascii_key(b"value")?;
                    if object::has_property(self.heap, descriptor.as_handle(), field)
                        .map_err(|_| self.heap_failure())?
                    {
                        let wanted = self.get_property(descriptor, field)?;
                        // Strings compare by their text; numbers keep the
                        // sign of zero apart, as SameValue does.
                        let same = if wanted.is_number() && expected_value.is_number() {
                            value::same_value(wanted, expected_value)
                        } else {
                            self.same_value_zero(wanted, expected_value)?
                        };
                        if !same {
                            return Err(self.throw_type_error());
                        }
                    }
                    return Ok(first);
                }
                // A redefinition changes only what the descriptor states: an
                // absent field keeps what the property already has.
                let current = object::get_own_property(self.heap, first.as_handle(), key)
                    .map_err(|_| self.heap_failure())?;
                let mut attributes = current.map_or(0, |existing| existing.attributes);
                let mut described_value = false;
                let mut described_accessor = false;
                let mut value = current.map_or(Value::UNDEFINED, |existing| existing.value);
                let mut getter = current.map_or(Value::UNDEFINED, |existing| existing.getter);
                let mut setter = current.map_or(Value::UNDEFINED, |existing| existing.setter);
                for (name, bit) in [
                    (&b"writable"[..], attribute::WRITABLE),
                    (&b"enumerable"[..], attribute::ENUMERABLE),
                    (&b"configurable"[..], attribute::CONFIGURABLE),
                ] {
                    let field = self.ascii_key(name)?;
                    if object::has_property(self.heap, descriptor.as_handle(), field)
                        .map_err(|_| self.heap_failure())?
                    {
                        let flag = self.get_property(descriptor, field)?;
                        if self.coerce_to_boolean(flag)? {
                            attributes |= bit;
                        } else {
                            attributes &= !bit;
                        }
                    }
                }
                for (name, slot) in [(&b"get"[..], &mut getter), (&b"set"[..], &mut setter)] {
                    let field = self.ascii_key(name)?;
                    if object::has_property(self.heap, descriptor.as_handle(), field)
                        .map_err(|_| self.heap_failure())?
                    {
                        described_accessor = true;
                        *slot = self.get_property(descriptor, field)?;
                    }
                }
                let value_field = self.ascii_key(b"value")?;
                if object::has_property(self.heap, descriptor.as_handle(), value_field)
                    .map_err(|_| self.heap_failure())?
                {
                    described_value = true;
                    value = self.get_property(descriptor, value_field)?;
                }
                if described_value && described_accessor {
                    return Err(self.throw_type_error());
                }
                let accessor = described_accessor
                    || (!described_value
                        && current.is_some_and(|existing| {
                            matches!(existing.kind, object::DescriptorKind::Accessor)
                        }));
                if let Key::Index(index) = key {
                    // A mapped index going non-writable takes the binding's
                    // current value with it, before the aliasing breaks.
                    if !described_value && !accessor && attributes & attribute::WRITABLE == 0 {
                        if let Ok(Some((environment, mask))) =
                            object::arguments_map(self.heap, first.as_handle())
                        {
                            if index < 32 && mask & (1 << index) != 0 && environment.is_object() {
                                if let Ok(held) = crate::env::slot_value(
                                    self.heap,
                                    environment.as_handle(),
                                    index,
                                ) {
                                    value = held;
                                }
                            }
                        }
                    }
                }
                let completed = if accessor {
                    Descriptor::accessor(getter, setter, attributes)
                } else {
                    Descriptor::data(value, attributes)
                };
                let admitted =
                    object::define_own_property(self.heap, first.as_handle(), key, completed)
                        .map_err(|_| self.heap_failure())?;
                if !admitted {
                    return Err(self.throw_type_error());
                }
                // An array index at or past the length lengthens the array.
                if let Key::Index(index) = key {
                    if self.is_array(first)? {
                        let length = self.length_of(first)?;
                        if index >= length {
                            self.set_length(first, index.saturating_add(1))?;
                        }
                    }
                }
                // A mapped arguments index: a defined value still writes the
                // parameter, and an accessor or a non-writable definition
                // breaks the aliasing.
                if let Key::Index(index) = key {
                    if let Ok(Some((environment, mask))) =
                        object::arguments_map(self.heap, first.as_handle())
                    {
                        if index < 32 && mask & (1 << index) != 0 {
                            if described_value && !accessor && environment.is_object() {
                                let _ = crate::env::set_slot(
                                    self.heap,
                                    environment.as_handle(),
                                    index,
                                    value,
                                );
                            }
                            if accessor || attributes & attribute::WRITABLE == 0 {
                                let _ = object::unmap_argument(self.heap, first.as_handle(), index);
                            }
                        }
                    }
                }
                Ok(first)
            }
            native::OBJECT_GET_OWN_PROPERTY_NAMES => {
                self.own_entries(first, native::OBJECT_GET_OWN_PROPERTY_NAMES)
            }
            native::OBJECT_GET_OWN_PROPERTY_SYMBOLS => {
                self.own_entries(first, native::OBJECT_GET_OWN_PROPERTY_SYMBOLS)
            }
            native::OBJECT_CREATE => {
                let prototype = if first.is_nullish() {
                    Value::NULL
                } else {
                    first
                };
                let object =
                    object::create(self.heap, prototype).map_err(|_| self.heap_failure())?;
                let made = Value::object(object);
                // A second argument defines properties, descriptor by
                // descriptor, exactly as Object.defineProperty does.
                if second.is_object() {
                    let mut keys = [Key::Index(0); MAX_OWN_KEYS];
                    let count = object::own_keys(self.heap, second.as_handle(), &mut keys)
                        .map_err(Self::key_failure)?;
                    for &key in keys.get(..count).unwrap_or(&[]) {
                        if self.hidden_key(key) || !self.is_enumerable(second, key)? {
                            continue;
                        }
                        let descriptor = self.get_property(second, key)?;
                        let name = self.key_to_value(key)?;
                        self.object_native(
                            native::OBJECT_DEFINE_PROPERTY,
                            Value::UNDEFINED,
                            &[made, name, descriptor],
                        )?;
                    }
                }
                Ok(made)
            }
            native::OBJECT_IS => Ok(Value::boolean(value::same_value(first, second))),
            native::OBJECT_GET_OWN_PROPERTY_DESCRIPTOR => {
                if !first.is_object() {
                    return Err(self.throw_type_error());
                }
                let key = self.coerce_to_key(second)?;
                if self.hidden_key(key) {
                    return Ok(Value::UNDEFINED);
                }
                self.materialise_function_facts(first, key)?;
                self.deferred_trigger(first, Some(key))?;
                let Some(mut found) = object::get_own_property(self.heap, first.as_handle(), key)
                    .map_err(|_| self.heap_failure())?
                else {
                    return Ok(Value::UNDEFINED);
                };
                // A mapped argument's value is its parameter's: the record
                // reports what the slot holds now.
                if let Some(value) = self.mapped_argument(first, key) {
                    found.value = value;
                }
                // A namespace's string-keyed property reports as the data
                // record the specification gives it: the binding's value,
                // read now — a binding still in its dead zone throws.
                if matches!(
                    object::exotic_kind(self.heap, first.as_handle()).unwrap_or(0),
                    object::exotic::NAMESPACE | object::exotic::DEFERRED
                ) && !matches!(key, Key::Symbol(_))
                {
                    found.kind = object::DescriptorKind::Data;
                    found.value = self.get_property(first, key)?;
                    found.attributes = attribute::WRITABLE | attribute::ENUMERABLE;
                }
                let result = object::create(self.heap, Value::object(self.realm.object_prototype))
                    .map_err(|_| self.heap_failure())?;
                let result = Value::object(result);
                if matches!(found.kind, object::DescriptorKind::Data) {
                    let value_key = self.ascii_key(b"value")?;
                    self.set_property(result, value_key, found.value)?;
                    let writable_key = self.ascii_key(b"writable")?;
                    let writable = Value::boolean(found.has(attribute::WRITABLE));
                    self.set_property(result, writable_key, writable)?;
                } else {
                    let get_key = self.ascii_key(b"get")?;
                    self.set_property(result, get_key, found.getter)?;
                    let set_key = self.ascii_key(b"set")?;
                    self.set_property(result, set_key, found.setter)?;
                }
                let enumerable_key = self.ascii_key(b"enumerable")?;
                let enumerable = Value::boolean(found.has(attribute::ENUMERABLE));
                self.set_property(result, enumerable_key, enumerable)?;
                let configurable_key = self.ascii_key(b"configurable")?;
                let configurable = Value::boolean(found.has(attribute::CONFIGURABLE));
                self.set_property(result, configurable_key, configurable)?;
                Ok(result)
            }
            native::OBJECT_HAS_OWN_PROPERTY => {
                let object = self.coerce_to_object(this)?;
                let key = self.coerce_to_key(first)?;
                self.materialise_function_facts(object, key)?;
                self.namespace_touch(object, key)?;
                if object::exotic_kind(self.heap, object.as_handle()).unwrap_or(0)
                    == object::exotic::TYPED_ARRAY
                {
                    if let Key::Index(index) = key {
                        let count = self.typed_array_length(object)?.unwrap_or(0);
                        return Ok(Value::boolean(index < count));
                    }
                }
                let present = !self.hidden_key(key)
                    && object::get_own_property(self.heap, object.as_handle(), key)
                        .map_err(|_| self.heap_failure())?
                        .is_some();
                Ok(Value::boolean(present))
            }
            native::OBJECT_IS_PROTOTYPE_OF => {
                if !first.is_object() || !this.is_object() {
                    return Ok(Value::boolean(false));
                }
                let mut current = object::prototype(self.heap, first.as_handle())
                    .map_err(|_| self.heap_failure())?;
                let mut depth = 0u32;
                while current.is_object() && depth < object::MAX_PROTOTYPE_DEPTH {
                    if current.as_handle() == this.as_handle() {
                        return Ok(Value::boolean(true));
                    }
                    current = object::prototype(self.heap, current.as_handle())
                        .map_err(|_| self.heap_failure())?;
                    depth += 1;
                }
                Ok(Value::boolean(false))
            }
            native::OBJECT_PROPERTY_IS_ENUMERABLE => {
                let object = self.coerce_to_object(this)?;
                let key = self.coerce_to_key(first)?;
                self.materialise_function_facts(object, key)?;
                self.namespace_touch(object, key)?;
                Ok(Value::boolean(self.is_enumerable(object, key)?))
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
    }

    /// The natives of `Array`.
    fn array_native(
        &mut self,
        id: u32,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        let second = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
        match id {
            native::ARRAY => {
                let array = self.new_array()?;
                if arguments.len() == 1 && matches!(first.tag(), Tag::Number) {
                    let length = value::to_uint32(first.as_number());
                    self.set_length(array, length)?;
                    return Ok(array);
                }
                for (index, &value) in arguments.iter().enumerate() {
                    let index = u32::try_from(index).unwrap_or(0);
                    self.set_element(array, index, value)?;
                }
                self.set_length(array, u32::try_from(arguments.len()).unwrap_or(0))?;
                Ok(array)
            }
            native::ARRAY_IS_ARRAY => {
                let array = self.is_array(first)?;
                Ok(Value::boolean(array))
            }
            native::ARRAY_OF => {
                let array = self.new_array()?;
                for (index, &value) in arguments.iter().enumerate() {
                    self.set_element(array, u32::try_from(index).unwrap_or(0), value)?;
                }
                self.set_length(array, u32::try_from(arguments.len()).unwrap_or(0))?;
                Ok(array)
            }
            native::ARRAY_FROM => {
                let array = self.new_array()?;
                let mut written = 0u32;
                let mapper = second;
                if let Some(iterator) = self.iterator_of(first)? {
                    loop {
                        let Some(value) = self.iterator_step(iterator)? else {
                            break;
                        };
                        let value = if mapper.is_undefined() {
                            value
                        } else {
                            let index =
                                Value::number(crate::softfloat::from_u64(u64::from(written)));
                            self.call_with(mapper, Value::UNDEFINED, &[value, index])?
                        };
                        self.set_element(array, written, value)?;
                        written += 1;
                    }
                } else {
                    let length = self.length_of(first)?;
                    while written < length {
                        let value = self.element(first, written)?;
                        let value = if mapper.is_undefined() {
                            value
                        } else {
                            let index =
                                Value::number(crate::softfloat::from_u64(u64::from(written)));
                            self.call_with(mapper, Value::UNDEFINED, &[value, index])?
                        };
                        self.set_element(array, written, value)?;
                        written += 1;
                    }
                }
                self.set_length(array, written)?;
                Ok(array)
            }
            native::ARRAY_PUSH => {
                let mut length = self.length_of(this)?;
                for &value in arguments {
                    self.set_element(this, length, value)?;
                    length += 1;
                }
                self.set_length(this, length)?;
                Ok(Value::number(crate::softfloat::from_u64(u64::from(length))))
            }
            native::ARRAY_POP => {
                let length = self.length_of(this)?;
                if length == 0 {
                    return Ok(Value::UNDEFINED);
                }
                let value = self.element(this, length - 1)?;
                self.delete_element(this, length - 1)?;
                self.set_length(this, length - 1)?;
                Ok(value)
            }
            native::ARRAY_SHIFT => {
                let length = self.length_of(this)?;
                if length == 0 {
                    return Ok(Value::UNDEFINED);
                }
                let value = self.element(this, 0)?;
                let mut index = 1u32;
                while index < length {
                    let moved = self.element(this, index)?;
                    self.set_element(this, index - 1, moved)?;
                    index += 1;
                }
                self.delete_element(this, length - 1)?;
                self.set_length(this, length - 1)?;
                Ok(value)
            }
            native::ARRAY_UNSHIFT => {
                let length = self.length_of(this)?;
                let count = u32::try_from(arguments.len()).unwrap_or(0);
                let mut index = length;
                while index > 0 {
                    index -= 1;
                    let moved = self.element(this, index)?;
                    self.set_element(this, index + count, moved)?;
                }
                for (offset, &value) in arguments.iter().enumerate() {
                    self.set_element(this, u32::try_from(offset).unwrap_or(0), value)?;
                }
                let total = length + count;
                self.set_length(this, total)?;
                Ok(Value::number(crate::softfloat::from_u64(u64::from(total))))
            }
            native::ARRAY_SLICE => {
                let length = self.length_of(this)?;
                let start = self.relative_index(first, length, 0)?;
                let end = self.relative_index(second, length, length)?;
                let array = self.new_array()?;
                let mut index = start;
                let mut written = 0u32;
                while index < end {
                    let value = self.element(this, index)?;
                    self.set_element(array, written, value)?;
                    written += 1;
                    index += 1;
                }
                self.set_length(array, written)?;
                Ok(array)
            }
            native::ARRAY_INDEX_OF | native::ARRAY_INCLUDES => {
                let length = self.length_of(this)?;
                let mut index = 0u32;
                while index < length {
                    let value = self.element(this, index)?;
                    let same = if id == native::ARRAY_INCLUDES {
                        value::same_value_zero(value, first)
                    } else {
                        self.strict_equals(value, first)?
                    };
                    if same {
                        return Ok(if id == native::ARRAY_INCLUDES {
                            Value::boolean(true)
                        } else {
                            Value::number(crate::softfloat::from_u64(u64::from(index)))
                        });
                    }
                    index += 1;
                }
                Ok(if id == native::ARRAY_INCLUDES {
                    Value::boolean(false)
                } else {
                    Value::number(-1.0)
                })
            }
            native::ARRAY_CONCAT => {
                let array = self.new_array()?;
                let mut written = 0u32;
                let length = self.length_of(this)?;
                let mut index = 0u32;
                while index < length {
                    let value = self.element(this, index)?;
                    self.set_element(array, written, value)?;
                    written += 1;
                    index += 1;
                }
                for &argument in arguments {
                    if self.is_array(argument)? {
                        let length = self.length_of(argument)?;
                        let mut index = 0u32;
                        while index < length {
                            let value = self.element(argument, index)?;
                            self.set_element(array, written, value)?;
                            written += 1;
                            index += 1;
                        }
                    } else {
                        self.set_element(array, written, argument)?;
                        written += 1;
                    }
                }
                self.set_length(array, written)?;
                Ok(array)
            }
            native::ARRAY_MAP
            | native::ARRAY_FILTER
            | native::ARRAY_FOR_EACH
            | native::ARRAY_SOME
            | native::ARRAY_EVERY
            | native::ARRAY_FIND
            | native::ARRAY_FIND_INDEX => self.array_walk(id, this, first, second),
            native::ARRAY_REDUCE => {
                let length = self.length_of(this)?;
                let mut index = 0u32;
                let mut accumulator = if arguments.len() > 1 {
                    second
                } else {
                    if length == 0 {
                        return Err(self.throw_type_error());
                    }
                    index = 1;
                    self.element(this, 0)?
                };
                while index < length {
                    let value = self.element(this, index)?;
                    let position = Value::number(crate::softfloat::from_u64(u64::from(index)));
                    accumulator = self.call_with(
                        first,
                        Value::UNDEFINED,
                        &[accumulator, value, position, this],
                    )?;
                    index += 1;
                }
                Ok(accumulator)
            }
            native::ARRAY_REVERSE => {
                let length = self.length_of(this)?;
                let mut low = 0u32;
                let mut high = length.saturating_sub(1);
                while low < high {
                    let left = self.element(this, low)?;
                    let right = self.element(this, high)?;
                    self.set_element(this, low, right)?;
                    self.set_element(this, high, left)?;
                    low += 1;
                    high -= 1;
                }
                Ok(this)
            }
            native::ARRAY_FILL => {
                let length = self.length_of(this)?;
                let start = self.relative_index(second, length, 0)?;
                let end = self.relative_index(
                    arguments.get(2).copied().unwrap_or(Value::UNDEFINED),
                    length,
                    length,
                )?;
                let mut index = start;
                while index < end {
                    self.set_element(this, index, first)?;
                    index += 1;
                }
                Ok(this)
            }
            native::ARRAY_SORT => self.sort_array(this, first),
            native::ARRAY_VALUES | native::ARRAY_KEYS | native::ARRAY_ENTRIES => {
                let kind = match id {
                    native::ARRAY_KEYS => ITERATE_KEYS,
                    native::ARRAY_ENTRIES => ITERATE_ENTRIES,
                    _ => ITERATE_VALUES,
                };
                let handle = object::create_iterator(
                    self.heap,
                    Value::object(self.realm.iterator_prototype),
                    this,
                    kind,
                )
                .map_err(|_| self.heap_failure())?;
                Ok(Value::object(handle))
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
    }

    /// The natives of `String`.
    fn string_native(
        &mut self,
        id: u32,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        let second = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
        if id == native::STRING {
            if arguments.is_empty() {
                return self.ascii_string(b"");
            }
            if matches!(first.tag(), Tag::Symbol) {
                return self.symbol_text(first);
            }
            return self.coerce_to_string(first);
        }
        if id == native::STRING_FROM_CHAR_CODE {
            let mut units = [0u16; MAX_ARGUMENTS];
            let mut count = 0usize;
            for &argument in arguments {
                let number = self.coerce_to_number(argument)?;
                if let Some(slot) = units.get_mut(count) {
                    *slot = value::to_uint16(number);
                    count += 1;
                }
            }
            return self.make_string(units.get(..count).unwrap_or(&[]));
        }

        if id == native::STRING_TO_STRING {
            // thisStringValue: the primitive, a wrapper's value, or a
            // TypeError — never a coercion, which would call back here.
            return self.this_primitive_of(this, Tag::String);
        }
        let receiver = self.primitive_this(this)?;
        let text = self.string_handle(receiver)?;
        let length = string::length(self.heap, text).map_err(|_| self.heap_failure())?;
        match id {
            native::STRING_TO_STRING => Ok(Value::string(text)),
            native::STRING_CHAR_AT | native::STRING_AT => {
                let number = if first.is_undefined() {
                    0.0
                } else {
                    self.coerce_to_number(first)?
                };
                let index = value::truncate(number);
                let index = if id == native::STRING_AT && index < 0.0 {
                    f64::from(length) + index
                } else {
                    index
                };
                if index < 0.0 || index >= f64::from(length) {
                    return if id == native::STRING_AT {
                        Ok(Value::UNDEFINED)
                    } else {
                        self.ascii_string(b"")
                    };
                }
                let unit = string::unit_at(self.heap, text, value::to_uint32(index))
                    .map_err(|_| self.heap_failure())?
                    .unwrap_or(0);
                self.make_string(&[unit])
            }
            native::STRING_CHAR_CODE_AT => {
                let index = self.index_argument(first)?;
                match string::unit_at(self.heap, text, index).map_err(|_| self.heap_failure())? {
                    Some(unit) => Ok(Value::number(crate::softfloat::from_u64(u64::from(unit)))),
                    None => Ok(Value::number(f64::NAN)),
                }
            }
            native::STRING_CODE_POINT_AT => {
                let index = self.index_argument(first)?;
                match string::code_point_at(self.heap, text, index)
                    .map_err(|_| self.heap_failure())?
                {
                    Some((point, _)) => {
                        Ok(Value::number(crate::softfloat::from_u64(u64::from(point))))
                    }
                    None => Ok(Value::UNDEFINED),
                }
            }
            native::STRING_INDEX_OF | native::STRING_LAST_INDEX_OF | native::STRING_INCLUDES => {
                let needle = self.string_handle(first)?;
                let found = if id == native::STRING_LAST_INDEX_OF {
                    string::last_index_of(self.heap, text, needle)
                        .map_err(|_| self.heap_failure())?
                } else {
                    let from = if second.is_undefined() {
                        0
                    } else {
                        self.index_argument(second)?
                    };
                    string::index_of(self.heap, text, needle, from)
                        .map_err(|_| self.heap_failure())?
                };
                if id == native::STRING_INCLUDES {
                    return Ok(Value::boolean(found.is_some()));
                }
                Ok(match found {
                    Some(index) => Value::number(crate::softfloat::from_u64(u64::from(index))),
                    None => Value::number(-1.0),
                })
            }
            native::STRING_STARTS_WITH | native::STRING_ENDS_WITH => {
                let needle = self.string_handle(first)?;
                let needle_length =
                    string::length(self.heap, needle).map_err(|_| self.heap_failure())?;
                let at = if id == native::STRING_STARTS_WITH {
                    if second.is_undefined() {
                        0
                    } else {
                        self.index_argument(second)?
                    }
                } else {
                    let end = if second.is_undefined() {
                        length
                    } else {
                        self.index_argument(second)?.min(length)
                    };
                    match end.checked_sub(needle_length) {
                        Some(at) => at,
                        None => return Ok(Value::boolean(false)),
                    }
                };
                let matched = string::matches_at(self.heap, text, needle, at)
                    .map_err(|_| self.heap_failure())?;
                Ok(Value::boolean(matched))
            }
            native::STRING_SLICE => {
                let start = self.relative_index(first, length, 0)?;
                let end = self.relative_index(second, length, length)?;
                let handle = string::slice(self.heap, text, start, end.max(start))
                    .map_err(|_| self.heap_failure())?;
                Ok(Value::string(handle))
            }
            native::STRING_SUBSTRING => {
                let start = self.clamped_index(first, length, 0)?;
                let end = self.clamped_index(second, length, length)?;
                let (start, end) = if start <= end {
                    (start, end)
                } else {
                    (end, start)
                };
                let handle =
                    string::slice(self.heap, text, start, end).map_err(|_| self.heap_failure())?;
                Ok(Value::string(handle))
            }
            native::STRING_TO_UPPER_CASE | native::STRING_TO_LOWER_CASE => {
                let handle =
                    string::convert_case(self.heap, text, id == native::STRING_TO_UPPER_CASE)
                        .map_err(|_| self.heap_failure())?;
                Ok(Value::string(handle))
            }
            native::STRING_TRIM => {
                let handle =
                    string::trim(self.heap, text, true, true).map_err(|_| self.heap_failure())?;
                Ok(Value::string(handle))
            }
            native::STRING_REPEAT => {
                let number = self.coerce_to_number(first)?;
                if number < 0.0 || !number.is_finite() {
                    return Err(self.throw_error_of(ErrorKind::Range));
                }
                let count = value::to_uint32(value::truncate(number));
                let handle =
                    string::repeat(self.heap, text, count).map_err(|_| self.heap_failure())?;
                Ok(Value::string(handle))
            }
            native::STRING_PAD_START | native::STRING_PAD_END => {
                let target = self.index_argument(first)?;
                let filler = if second.is_undefined() {
                    self.ascii_string(b" ")?
                } else {
                    self.coerce_to_string(second)?
                };
                let handle = string::pad(
                    self.heap,
                    text,
                    target,
                    filler.as_handle(),
                    id == native::STRING_PAD_START,
                )
                .map_err(|_| self.heap_failure())?;
                Ok(Value::string(handle))
            }
            native::STRING_CONCAT => {
                let mut result = text;
                for &argument in arguments {
                    let other = self.string_handle(argument)?;
                    result = string::concat(self.heap, result, other)
                        .map_err(|_| self.heap_failure())?;
                }
                Ok(Value::string(result))
            }
            native::STRING_REPLACE
                if first.is_object()
                    && object::regexp_program(self.heap, first.as_handle())
                        .map_err(|_| self.heap_failure())?
                        .is_some() =>
            {
                self.replace_with_pattern(text, first, second)
            }
            native::STRING_SPLIT
                if first.is_object()
                    && object::regexp_program(self.heap, first.as_handle())
                        .map_err(|_| self.heap_failure())?
                        .is_some() =>
            {
                self.split_by_pattern(text, first)
            }
            native::STRING_REPLACE => {
                let needle = self.string_handle(first)?;
                let Some(at) = string::index_of(self.heap, text, needle, 0)
                    .map_err(|_| self.heap_failure())?
                else {
                    return Ok(Value::string(text));
                };
                let needle_length =
                    string::length(self.heap, needle).map_err(|_| self.heap_failure())?;
                let replacement = if self.is_callable_value(second) {
                    let matched = string::slice(self.heap, text, at, at + needle_length)
                        .map_err(|_| self.heap_failure())?;
                    let position = Value::number(crate::softfloat::from_u64(u64::from(at)));
                    let outcome = self.call_with(
                        second,
                        Value::UNDEFINED,
                        &[Value::string(matched), position, Value::string(text)],
                    )?;
                    self.string_handle(outcome)?
                } else {
                    self.string_handle(second)?
                };
                let head =
                    string::slice(self.heap, text, 0, at).map_err(|_| self.heap_failure())?;
                let tail = string::slice(self.heap, text, at + needle_length, length)
                    .map_err(|_| self.heap_failure())?;
                let joined = string::concat(self.heap, head, replacement)
                    .map_err(|_| self.heap_failure())?;
                let joined =
                    string::concat(self.heap, joined, tail).map_err(|_| self.heap_failure())?;
                Ok(Value::string(joined))
            }
            native::STRING_SPLIT => {
                let array = self.new_array()?;
                if first.is_undefined() {
                    self.set_element(array, 0, Value::string(text))?;
                    self.set_length(array, 1)?;
                    return Ok(array);
                }
                let separator = self.string_handle(first)?;
                let separator_length =
                    string::length(self.heap, separator).map_err(|_| self.heap_failure())?;
                let mut written = 0u32;
                let mut start = 0u32;
                if separator_length == 0 {
                    // An empty separator splits into single code units.
                    while start < length {
                        let piece = string::slice(self.heap, text, start, start + 1)
                            .map_err(|_| self.heap_failure())?;
                        self.set_element(array, written, Value::string(piece))?;
                        written += 1;
                        start += 1;
                    }
                    self.set_length(array, written)?;
                    return Ok(array);
                }
                loop {
                    let found = string::index_of(self.heap, text, separator, start)
                        .map_err(|_| self.heap_failure())?;
                    let at = found.unwrap_or(length);
                    let piece = string::slice(self.heap, text, start, at)
                        .map_err(|_| self.heap_failure())?;
                    self.set_element(array, written, Value::string(piece))?;
                    written += 1;
                    match found {
                        Some(at) => start = at + separator_length,
                        None => break,
                    }
                }
                self.set_length(array, written)?;
                Ok(array)
            }
            native::STRING_VALUES => {
                let handle = object::create_iterator(
                    self.heap,
                    Value::object(self.realm.iterator_prototype),
                    Value::string(text),
                    ITERATE_CODE_POINTS,
                )
                .map_err(|_| self.heap_failure())?;
                Ok(Value::object(handle))
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
    }

    /// The natives of `Number`, `Boolean`, and the global conversions.
    fn number_native(
        &mut self,
        id: u32,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        match id {
            native::NUMBER => {
                if arguments.is_empty() {
                    return Ok(Value::number(0.0));
                }
                if matches!(first.tag(), Tag::BigInt) {
                    // The conversion is explicit here, so it is allowed to
                    // round; an arithmetic operation that mixed them would not.
                    let number = self.big_int_operand(first)?;
                    return Ok(Value::number(number.to_f64()));
                }
                Ok(Value::number(self.coerce_to_number(first)?))
            }
            native::BOOLEAN => Ok(Value::boolean(self.coerce_to_boolean(first)?)),
            native::NUMBER_IS_INTEGER | native::NUMBER_IS_SAFE_INTEGER => {
                let integral = matches!(first.tag(), Tag::Number)
                    && value::is_integral(first.as_number())
                    && (id == native::NUMBER_IS_INTEGER
                        || first.as_number().abs() <= 9_007_199_254_740_991.0);
                Ok(Value::boolean(integral))
            }
            native::NUMBER_IS_FINITE => Ok(Value::boolean(
                matches!(first.tag(), Tag::Number) && first.as_number().is_finite(),
            )),
            native::NUMBER_IS_NAN => Ok(Value::boolean(
                matches!(first.tag(), Tag::Number) && first.as_number().is_nan(),
            )),
            native::IS_NAN => {
                let number = self.coerce_to_number(first)?;
                Ok(Value::boolean(number.is_nan()))
            }
            native::IS_FINITE => {
                let number = self.coerce_to_number(first)?;
                Ok(Value::boolean(number.is_finite()))
            }
            native::PARSE_INT | native::PARSE_FLOAT => {
                let text = self.string_handle(first)?;
                let radix = if id == native::PARSE_INT {
                    let value = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
                    let number = self.coerce_to_number(value)?;
                    let radix = value::to_uint32(number);
                    if radix == 0 {
                        10
                    } else {
                        radix
                    }
                } else {
                    10
                };
                self.parse_number(text, radix, id == native::PARSE_FLOAT)
            }
            native::NUMBER_TO_STRING => {
                let receiver = self.this_primitive_of(this, Tag::Number)?;
                let number = self.coerce_to_number(receiver)?;
                let radix = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                if radix.is_undefined() {
                    return self.coerce_to_string(Value::number(number));
                }
                let radix = value::to_uint32(self.coerce_to_number(radix)?);
                if !(2..=36).contains(&radix) {
                    return Err(self.throw_error_of(ErrorKind::Range));
                }
                if radix == 10 {
                    return self.coerce_to_string(Value::number(number));
                }
                let mut units = [0u16; 72];
                let written = crate::numeric::radix_text(number, radix, &mut units);
                self.make_string(units.get(..written).unwrap_or(&[]))
            }
            native::NUMBER_TO_FIXED => {
                let receiver = self.this_primitive_of(this, Tag::Number)?;
                let number = self.coerce_to_number(receiver)?;
                let digits = value::to_uint32(self.coerce_to_number(first)?);
                if digits > 100 {
                    return Err(self.throw_error_of(ErrorKind::Range));
                }
                let mut units = [0u16; 128];
                let written = crate::dtoa::fixed(number, digits, &mut units);
                self.make_string(units.get(..written).unwrap_or(&[]))
            }
            native::NUMBER_TO_EXPONENTIAL => {
                let receiver = self.this_primitive_of(this, Tag::Number)?;
                let number = self.coerce_to_number(receiver)?;
                let digits = if first.is_undefined() {
                    None
                } else {
                    let requested = self.coerce_to_number(first)?;
                    let requested = value::truncate(requested);
                    if !number.is_finite() {
                        None
                    } else if !(0.0..=100.0).contains(&requested) {
                        return Err(self.throw_error_of(ErrorKind::Range));
                    } else {
                        Some(requested as u32)
                    }
                };
                let mut units = [0u16; 128];
                let written = crate::dtoa::exponential(number, digits, &mut units);
                self.make_string(units.get(..written).unwrap_or(&[]))
            }
            native::NUMBER_TO_PRECISION => {
                let receiver = self.this_primitive_of(this, Tag::Number)?;
                let number = self.coerce_to_number(receiver)?;
                if first.is_undefined() {
                    return self.coerce_to_string(Value::number(number));
                }
                let requested = value::truncate(self.coerce_to_number(first)?);
                if !number.is_finite() {
                    return self.coerce_to_string(Value::number(number));
                }
                if !(1.0..=100.0).contains(&requested) {
                    return Err(self.throw_error_of(ErrorKind::Range));
                }
                let mut units = [0u16; 128];
                let written = crate::dtoa::precision(number, requested as u32, &mut units);
                self.make_string(units.get(..written).unwrap_or(&[]))
            }
            native::NUMBER_VALUE_OF => self.this_primitive_of(this, Tag::Number),
            native::BOOLEAN_VALUE_OF => self.this_primitive_of(this, Tag::Boolean),
            native::BOOLEAN_TO_STRING => {
                let receiver = self.this_primitive_of(this, Tag::Boolean)?;
                self.coerce_to_string(receiver)
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
    }

    /// The natives of `Math`, each a pure function of its arguments.
    fn math_native(&mut self, id: u32, arguments: &[Value]) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        let value = self.coerce_to_number(first)?;
        let result = match id {
            native::MATH_ABS => {
                if value < 0.0 {
                    -value
                } else {
                    value
                }
            }
            native::MATH_FLOOR => value::floor(value),
            native::MATH_CEIL => value::ceil(value),
            native::MATH_ROUND => value::floor(value + 0.5),
            native::MATH_TRUNC => value::truncate(value),
            native::MATH_SQRT => crate::numeric::sqrt(value),
            native::MATH_SIGN => {
                if value.is_nan() {
                    value
                } else if value > 0.0 {
                    1.0
                } else if value < 0.0 {
                    -1.0
                } else {
                    value
                }
            }
            native::MATH_POW => {
                let exponent =
                    self.coerce_to_number(arguments.get(1).copied().unwrap_or(Value::UNDEFINED))?;
                crate::numeric::power(value, exponent)
            }
            native::MATH_MIN | native::MATH_MAX => {
                let mut best = if id == native::MATH_MIN {
                    f64::INFINITY
                } else {
                    f64::NEG_INFINITY
                };
                for &argument in arguments {
                    let number = self.coerce_to_number(argument)?;
                    if number.is_nan() {
                        best = f64::NAN;
                        break;
                    }
                    let better = if id == native::MATH_MIN {
                        number < best
                    } else {
                        number > best
                    };
                    if better {
                        best = number;
                    }
                }
                best
            }
            native::MATH_SIN => crate::numeric::sin(value),
            native::MATH_COS => crate::numeric::cos(value),
            native::MATH_TAN => crate::numeric::tan(value),
            native::MATH_ASIN => crate::numeric::asin(value),
            native::MATH_ACOS => crate::numeric::acos(value),
            native::MATH_ATAN => crate::numeric::atan(value),
            native::MATH_ATAN2 => {
                let x =
                    self.coerce_to_number(arguments.get(1).copied().unwrap_or(Value::UNDEFINED))?;
                crate::numeric::atan2(value, x)
            }
            native::MATH_EXP => crate::numeric::exp(value),
            native::MATH_LOG => crate::numeric::log(value),
            native::MATH_LOG2 => crate::numeric::log_2(value),
            native::MATH_LOG10 => crate::numeric::log_10(value),
            native::MATH_CBRT => crate::numeric::cbrt(value),
            native::MATH_HYPOT => {
                let mut total = 0.0f64;
                for &argument in arguments {
                    let number = self.coerce_to_number(argument)?;
                    total += number * number;
                }
                crate::numeric::sqrt(total)
            }
            _ => return Err(Completion::Terminated(Termination::NotImplemented)),
        };
        Ok(Value::number(result))
    }

    /// `Symbol`, and what a symbol carries.
    /// One of the four Promise combinators over an iterable of values.
    fn promise_combinator(&mut self, id: u32, iterable: Value) -> Result<Value, Completion> {
        let mode = match id {
            native::PROMISE_RACE => 1.0,
            native::PROMISE_ALL_SETTLED => 2.0,
            native::PROMISE_ANY => 3.0,
            _ => 0.0,
        };
        let promise = self.new_promise()?;
        let results = self.new_array()?;
        let record = object::create(self.heap, Value::NULL)
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        let rec_value = Value::object(record);
        for (name, value) in [
            (&b"results"[..], results),
            (&b"promise"[..], Value::object(promise)),
            (&b"remaining"[..], Value::number(1.0)),
            (&b"mode"[..], Value::number(mode)),
        ] {
            let key = self.ascii_key(name)?;
            object::define_own_property(
                self.heap,
                record,
                key,
                Descriptor::data(value, attribute::WRITABLE),
            )
            .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        }
        let iterator = match self.iterator_of(iterable) {
            Ok(Some(iterator)) => iterator,
            Ok(None) => {
                let reason = self.create_error(ErrorKind::Type, Value::UNDEFINED)?;
                self.settle(promise, promise::REJECTED, reason)?;
                return Ok(Value::object(promise));
            }
            Err(Completion::Throw(reason)) => {
                self.settle(promise, promise::REJECTED, reason)?;
                return Ok(Value::object(promise));
            }
            Err(other) => return Err(other),
        };
        let mut index = 0u32;
        loop {
            match self.iterator_step(iterator) {
                Ok(Some(value)) => {
                    self.combinator_adjust(rec_value, 1.0)?;
                    self.append_element(results, Some(Value::UNDEFINED))?;
                    let element = if value.is_object()
                        && object::is_promise(self.heap, value.as_handle()).unwrap_or(false)
                    {
                        value
                    } else {
                        let made = self.new_promise()?;
                        self.resolve(made, value)?;
                        Value::object(made)
                    };
                    let state = object::create(self.heap, Value::NULL)
                        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                    for (name, held) in [
                        (&b"rec"[..], rec_value),
                        (&b"i"[..], Value::number(f64::from(index))),
                    ] {
                        let key = self.ascii_key(name)?;
                        object::define_own_property(
                            self.heap,
                            state,
                            key,
                            Descriptor::data(held, attribute::WRITABLE),
                        )
                        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                    }
                    let fulfilled =
                        self.finally_function(native::COMBINE_FULFILLED, Value::object(state))?;
                    let rejected =
                        self.finally_function(native::COMBINE_REJECTED, Value::object(state))?;
                    self.promise_then(element, fulfilled, rejected)?;
                    index += 1;
                }
                Ok(None) => break,
                Err(Completion::Throw(reason)) => {
                    self.settle(promise, promise::REJECTED, reason)?;
                    return Ok(Value::object(promise));
                }
                Err(other) => return Err(other),
            }
        }
        // The guard count added before the walk comes off: only now can the
        // combinator finish on an empty or already-settled set.
        if self.combinator_adjust(rec_value, -1.0)? == 0.0 {
            self.combinator_finish(rec_value)?;
        }
        Ok(Value::object(promise))
    }

    /// Move a combinator's remaining count and answer the new value.
    fn combinator_adjust(&mut self, record: Value, delta: f64) -> Result<f64, Completion> {
        let key = self.ascii_key(b"remaining")?;
        let current = self.get_property(record, key)?.as_number();
        let next = current + delta;
        object::define_own_property(
            self.heap,
            record.as_handle(),
            key,
            Descriptor::data(Value::number(next), attribute::WRITABLE),
        )
        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
        Ok(next)
    }

    /// Settle a combinator whose count ran out: what the mode gathers wins.
    fn combinator_finish(&mut self, record: Value) -> Result<(), Completion> {
        let results_key = self.ascii_key(b"results")?;
        let promise_key = self.ascii_key(b"promise")?;
        let mode_key = self.ascii_key(b"mode")?;
        let results = self.get_property(record, results_key)?;
        let promise = self.get_property(record, promise_key)?;
        let mode = self.get_property(record, mode_key)?.as_number();
        if !promise.is_object() {
            return Err(Completion::Terminated(Termination::Malformed));
        }
        if mode == 3.0 {
            // `Promise.any` with nothing fulfilled: every reason, together.
            let reason = self.create_error(ErrorKind::Type, Value::UNDEFINED)?;
            if reason.is_object() {
                let name_key = self.ascii_key(b"name")?;
                let name = self.ascii_string(b"AggregateError")?;
                let errors_key = self.ascii_key(b"errors")?;
                object::define_own_property(
                    self.heap,
                    reason.as_handle(),
                    name_key,
                    Descriptor::data(name, attribute::DEFAULT),
                )
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                object::define_own_property(
                    self.heap,
                    reason.as_handle(),
                    errors_key,
                    Descriptor::data(results, attribute::DEFAULT),
                )
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
            }
            return self.settle(promise.as_handle(), promise::REJECTED, reason);
        }
        self.settle(promise.as_handle(), promise::FULFILLED, results)
    }

    /// One combinator element settled: fold it into the record.
    fn combine_settled(&mut self, fulfilled: bool, value: Value) -> Result<(), Completion> {
        let Some(function) = self.current_native else {
            return Err(Completion::Terminated(Termination::Malformed));
        };
        let state = object::function_environment(self.heap, function)
            .map_err(|_| Completion::Terminated(Termination::Malformed))?;
        let rec_key = self.ascii_key(b"rec")?;
        let i_key = self.ascii_key(b"i")?;
        let record = self.get_property(state, rec_key)?;
        let index = self.get_property(state, i_key)?.as_number() as u32;
        let results_key = self.ascii_key(b"results")?;
        let promise_key = self.ascii_key(b"promise")?;
        let mode_key = self.ascii_key(b"mode")?;
        let results = self.get_property(record, results_key)?;
        let promise = self.get_property(record, promise_key)?;
        let mode = self.get_property(record, mode_key)?.as_number();
        if !promise.is_object() {
            return Err(Completion::Terminated(Termination::Malformed));
        }
        let handle = promise.as_handle();
        match (mode as u32, fulfilled) {
            (1, true) | (3, true) => self.settle(handle, promise::FULFILLED, value),
            (1, false) | (0, false) => self.settle(handle, promise::REJECTED, value),
            (0, true) | (3, false) => {
                self.set_property(results, Key::Index(index), value)?;
                if self.combinator_adjust(record, -1.0)? == 0.0 {
                    self.combinator_finish(record)?;
                }
                Ok(())
            }
            (2, _) => {
                let entry = object::create(self.heap, Value::object(self.realm.object_prototype))
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                let status_key = self.ascii_key(b"status")?;
                let status = if fulfilled {
                    self.ascii_string(b"fulfilled")?
                } else {
                    self.ascii_string(b"rejected")?
                };
                let value_key = if fulfilled {
                    self.ascii_key(b"value")?
                } else {
                    self.ascii_key(b"reason")?
                };
                object::define_own_property(
                    self.heap,
                    entry,
                    status_key,
                    Descriptor::data(status, attribute::DEFAULT),
                )
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                object::define_own_property(
                    self.heap,
                    entry,
                    value_key,
                    Descriptor::data(value, attribute::DEFAULT),
                )
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                self.set_property(results, Key::Index(index), Value::object(entry))?;
                if self.combinator_adjust(record, -1.0)? == 0.0 {
                    self.combinator_finish(record)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn symbol_native(
        &mut self,
        id: u32,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        match id {
            native::SYMBOL => {
                let description = if first.is_undefined() {
                    self.ascii_string(b"")?
                } else {
                    self.coerce_to_string(first)?
                };
                let length = string::length(self.heap, description.as_handle())
                    .map_err(|_| self.heap_failure())? as usize;
                let mut units = [0u16; 128];
                let room = length.min(units.len());
                string::copy_units(
                    self.heap,
                    description.as_handle(),
                    units.get_mut(..room).unwrap_or(&mut []),
                )
                .map_err(|_| self.heap_failure())?;
                let handle = string::create_symbol(self.heap, units.get(..room).unwrap_or(&[]))
                    .map_err(|_| self.heap_failure())?;
                Ok(Value::symbol(handle))
            }
            native::SYMBOL_TO_STRING => {
                let receiver = self.primitive_this(this)?;
                self.symbol_text(receiver)
            }
            native::SYMBOL_VALUE_OF => {
                let receiver = self.primitive_this(this)?;
                if !matches!(receiver.tag(), Tag::Symbol) {
                    return Err(self.throw_type_error());
                }
                Ok(receiver)
            }
            native::SYMBOL_DESCRIPTION => {
                let receiver = self.primitive_this(this)?;
                if !matches!(receiver.tag(), Tag::Symbol) {
                    return Err(self.throw_type_error());
                }
                Ok(Value::string(receiver.as_handle()))
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
    }

    /// `call`, `apply`, and `bind`, which are how a receiver is chosen.
    fn function_native(
        &mut self,
        id: u32,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        match id {
            native::FUNCTION_PROTOTYPE_CALL => {
                let rest = arguments.get(1..).unwrap_or(&[]);
                self.call_value(this, first, rest)
            }
            native::FUNCTION_PROTOTYPE_APPLY => {
                let list = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
                if list.is_nullish() {
                    return self.call_value(this, first, &[]);
                }
                let length = self.length_of(list)?;
                let mut values = [Value::UNDEFINED; MAX_ARGUMENTS];
                let count = (length as usize).min(values.len());
                let mut index = 0usize;
                while index < count {
                    values[index] = self.element(list, u32::try_from(index).unwrap_or(0))?;
                    index += 1;
                }
                self.call_value(this, first, values.get(..count).unwrap_or(&[]))
            }
            native::FUNCTION_PROTOTYPE_BIND => {
                if !self.is_callable_value(this) {
                    return Err(self.throw_type_error());
                }
                // A bound function is a native that carries what it was bound
                // to: the target and the receiver, in an ordinary array.
                let record = self.new_array()?;
                self.set_element(record, 0, this)?;
                self.set_element(record, 1, first)?;
                let mut written = 2u32;
                for &argument in arguments.get(1..).unwrap_or(&[]) {
                    self.set_element(record, written, argument)?;
                    written += 1;
                }
                self.set_length(record, written)?;
                // A bound function constructs exactly when its target does.
                let flags = if this.is_object()
                    && object::is_constructor(self.heap, this.as_handle()).unwrap_or(false)
                {
                    object::function_flag::CONSTRUCTOR
                } else {
                    0
                };
                let bound = object::create_native(
                    self.heap,
                    Value::object(self.realm.function_prototype),
                    native::BOUND_FUNCTION,
                    flags,
                )
                .map_err(|_| self.heap_failure())?;
                object::set_bound_value(self.heap, bound, record)
                    .map_err(|_| self.heap_failure())?;
                // A bound function's `length` and `name` come from what it was
                // bound to and are settled here: they are the one pair the
                // lazy path cannot work out from the callable alone, because
                // the answer is the target's, less what is already bound.
                self.name_bound_function(bound, this, written.saturating_sub(2))?;
                Ok(Value::object(bound))
            }
            native::BOUND_FUNCTION => {
                let Some(function) = self.current_native else {
                    return Err(Completion::Terminated(Termination::Malformed));
                };
                let record = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                let target = self.element(record, 0)?;
                let receiver = self.element(record, 1)?;
                let bound_count = self.length_of(record)?.saturating_sub(2);
                let mut values = [Value::UNDEFINED; MAX_ARGUMENTS];
                let mut count = 0usize;
                let mut index = 0u32;
                while index < bound_count && count < values.len() {
                    values[count] = self.element(record, index + 2)?;
                    count += 1;
                    index += 1;
                }
                for &argument in arguments {
                    if count < values.len() {
                        values[count] = argument;
                        count += 1;
                    }
                }
                self.call_value(target, receiver, values.get(..count).unwrap_or(&[]))
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
    }

    /// One step of an iterator the engine made itself.
    fn iterator_native(
        &mut self,
        id: u32,
        this: Value,
        _arguments: &[Value],
    ) -> Result<Value, Completion> {
        if id == native::ITERATOR_SELF {
            return Ok(this);
        }
        if !this.is_object() {
            return Err(self.throw_type_error());
        }
        let Some((target, index, kind)) =
            object::iterator_state(self.heap, this.as_handle()).map_err(|_| self.heap_failure())?
        else {
            return Err(self.throw_type_error());
        };
        let result = object::create(self.heap, Value::object(self.realm.object_prototype))
            .map_err(|_| self.heap_failure())?;
        let result = Value::object(result);
        let value_key = self.ascii_key(b"value")?;
        let done_key = self.ascii_key(b"done")?;

        let (value, done, next) = if kind == ITERATE_MAP_ENTRIES || kind == ITERATE_SET_ENTRIES {
            let held = self.collection_arrays(target, false)?;
            let Some((keys, vals)) = held else {
                return Err(self.throw_type_error());
            };
            let length = self.length_of(keys)?;
            if index >= length {
                (Value::UNDEFINED, true, index)
            } else {
                let key = self.element(keys, index)?;
                let value = if kind == ITERATE_MAP_ENTRIES {
                    self.element(vals, index)?
                } else {
                    key
                };
                let pair = self.new_array()?;
                self.set_element(pair, 0, key)?;
                self.set_element(pair, 1, value)?;
                self.set_length(pair, 2)?;
                (pair, false, index + 1)
            }
        } else if kind == ITERATE_CODE_POINTS {
            let handle = target.as_handle();
            match string::code_point_at(self.heap, handle, index)
                .map_err(|_| self.heap_failure())?
            {
                Some((_, width)) => {
                    let piece = string::slice(self.heap, handle, index, index + width)
                        .map_err(|_| self.heap_failure())?;
                    (Value::string(piece), false, index + width)
                }
                None => (Value::UNDEFINED, true, index),
            }
        } else {
            if target.is_object()
                && object::exotic_kind(self.heap, target.as_handle()).unwrap_or(0)
                    == object::exotic::TYPED_ARRAY
                && self.typed_array_length(target)?.is_none()
            {
                // A view its buffer shrank out from under cannot be walked.
                return Err(self.throw_type_error());
            }
            let length = self.length_of(target)?;
            if index >= length {
                (Value::UNDEFINED, true, index)
            } else {
                let position = Value::number(crate::softfloat::from_u64(u64::from(index)));
                let value = match kind {
                    ITERATE_KEYS => position,
                    ITERATE_ENTRIES => {
                        let pair = self.new_array()?;
                        let element = self.element(target, index)?;
                        self.set_element(pair, 0, position)?;
                        self.set_element(pair, 1, element)?;
                        self.set_length(pair, 2)?;
                        pair
                    }
                    _ => self.element(target, index)?,
                };
                (value, false, index + 1)
            }
        };
        object::set_iterator_index(self.heap, this.as_handle(), next)
            .map_err(|_| self.heap_failure())?;
        self.define_property(result, value_key, value)?;
        self.define_property(result, done_key, Value::boolean(done))?;
        Ok(result)
    }

    /// The natives of `RegExp`, and the string methods that take a pattern.
    fn regexp_native(
        &mut self,
        id: u32,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
        match id {
            native::REG_EXP => {
                // `RegExp(x)` answers `x` when it is already one, and compiles
                // it otherwise.
                if first.is_object()
                    && object::regexp_program(self.heap, first.as_handle())
                        .map_err(|_| self.heap_failure())?
                        .is_some()
                    && arguments.len() == 1
                {
                    return Ok(first);
                }
                let pattern = self.string_handle(first)?;
                let flags = match arguments.get(1).copied() {
                    Some(value) if !value.is_undefined() => {
                        let text = self.string_handle(value)?;
                        let length = string::length(self.heap, text)
                            .map_err(|_| self.heap_failure())?
                            as usize;
                        let mut units = [0u16; 16];
                        let room = length.min(units.len());
                        string::copy_units(
                            self.heap,
                            text,
                            units.get_mut(..room).unwrap_or(&mut []),
                        )
                        .map_err(|_| self.heap_failure())?;
                        crate::regexp::flags_of(units.get(..room).unwrap_or(&[]))
                            .map_err(|_| self.throw_error_of(ErrorKind::Syntax))?
                    }
                    _ => 0,
                };
                let length =
                    string::length(self.heap, pattern).map_err(|_| self.heap_failure())? as usize;
                let mut units = [0u16; 512];
                let room = length.min(units.len());
                string::copy_units(self.heap, pattern, units.get_mut(..room).unwrap_or(&mut []))
                    .map_err(|_| self.heap_failure())?;
                self.create_regexp(units.get(..room).unwrap_or(&[]), flags)
            }
            native::REG_EXP_EXEC | native::REG_EXP_TEST => {
                let subject = self.string_handle(first)?;
                let (global, sticky) = self.regexp_kind(this)?;
                let start = if global || sticky {
                    let key = self.ascii_key(b"lastIndex")?;
                    let value = self.get_property(this, key)?;
                    value::to_uint32(self.coerce_to_number(value)?)
                } else {
                    0
                };
                let outcome = self.match_regexp(this, subject, start, sticky)?;
                if global || sticky {
                    let key = self.ascii_key(b"lastIndex")?;
                    let next = outcome.map_or(0, |slots| slots[1]);
                    let value = Value::number(crate::softfloat::from_u64(u64::from(next)));
                    self.set_property(this, key, value)?;
                }
                let Some(slots) = outcome else {
                    return Ok(if id == native::REG_EXP_TEST {
                        Value::boolean(false)
                    } else {
                        Value::NULL
                    });
                };
                if id == native::REG_EXP_TEST {
                    return Ok(Value::boolean(true));
                }
                self.match_result(subject, &slots, this)
            }
            native::REG_EXP_TO_STRING => {
                let source_key = self.ascii_key(b"source")?;
                let flags_key = self.ascii_key(b"flags")?;
                let source = self.get_property(this, source_key)?;
                let flags = self.get_property(this, flags_key)?;
                let slash = self.ascii_string(b"/")?;
                let source = self.coerce_to_string(source)?;
                let flags = self.coerce_to_string(flags)?;
                let text = string::concat(self.heap, slash.as_handle(), source.as_handle())
                    .map_err(|_| self.heap_failure())?;
                let text = string::concat(self.heap, text, slash.as_handle())
                    .map_err(|_| self.heap_failure())?;
                let text = string::concat(self.heap, text, flags.as_handle())
                    .map_err(|_| self.heap_failure())?;
                Ok(Value::string(text))
            }
            native::STRING_MATCH | native::STRING_SEARCH => {
                let receiver = self.primitive_this(this)?;
                let subject = self.string_handle(receiver)?;
                let pattern = self.as_regexp(first)?;
                let (global, sticky) = self.regexp_kind(pattern)?;
                if id == native::STRING_SEARCH || !global {
                    let outcome = self.match_regexp(pattern, subject, 0, sticky)?;
                    let Some(slots) = outcome else {
                        return Ok(if id == native::STRING_SEARCH {
                            Value::number(-1.0)
                        } else {
                            Value::NULL
                        });
                    };
                    if id == native::STRING_SEARCH {
                        return Ok(Value::number(crate::softfloat::from_u64(u64::from(
                            slots[0],
                        ))));
                    }
                    return self.match_result(subject, &slots, pattern);
                }
                // A global match answers every match's text, and nothing about
                // the groups, which is what the specification says.
                let array = self.new_array()?;
                let mut written = 0u32;
                let mut start = 0u32;
                loop {
                    let Some(slots) = self.match_regexp(pattern, subject, start, false)? else {
                        break;
                    };
                    let text = string::slice(self.heap, subject, slots[0], slots[1])
                        .map_err(|_| self.heap_failure())?;
                    self.set_element(array, written, Value::string(text))?;
                    written += 1;
                    start = if slots[1] > slots[0] {
                        slots[1]
                    } else {
                        slots[1] + 1
                    };
                }
                self.set_length(array, written)?;
                if written == 0 {
                    return Ok(Value::NULL);
                }
                Ok(array)
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
    }

    /// Call one of the functions the engine implements itself.
    fn call_native(
        &mut self,
        id: u32,
        this: Value,
        arguments: &[Value],
    ) -> Result<Value, Completion> {
        match id {
            native::OBJECT_TO_STRING => {
                // The tag names the bracket: `Symbol.toStringTag` when the
                // receiver carries a string one, `Object` otherwise.
                if this.is_object() {
                    let tag =
                        self.get_property(this, Key::Symbol(self.realm.to_string_tag_symbol))?;
                    if tag.is_string() {
                        let open = self.ascii_string(b"[object ")?;
                        let close = self.ascii_string(b"]")?;
                        let named = self.concat_values(open, tag)?;
                        return self.concat_values(named, close);
                    }
                }
                self.ascii_string(b"[object Object]")
            }
            // Building a function from source needs the compiler, which is
            // not in the machine: refusing is a type error the program can
            // catch, not a termination.
            native::FUNCTION
            | native::GENERATOR_FUNCTION
            | native::ASYNC_GENERATOR_FUNCTION
            | native::ASYNC_FUNCTION => Err(self.throw_type_error()),
            native::THROW_TYPE_ERROR => {
                if this.is_object()
                    && object::is_callable(self.heap, this.as_handle()) == Ok(true)
                    && !object::is_native(self.heap, this.as_handle()).unwrap_or(true)
                {
                    let code = object::function_code(self.heap, this.as_handle()).unwrap_or(0);
                    let module = object::function_module(self.heap, this.as_handle()).unwrap_or(0);
                    let sloppy = self.unit_of(module).function(code).is_some_and(|record| {
                        record.flags
                            & (record_flag::STRICT
                                | record_flag::ARROW
                                | record_flag::GENERATOR
                                | record_flag::ASYNC
                                | record_flag::METHOD)
                            == 0
                    });
                    if sloppy {
                        return Ok(Value::UNDEFINED);
                    }
                }
                Err(self.throw_type_error())
            }
            native::FUNCTION_PROTOTYPE => Ok(Value::UNDEFINED),
            native::EVAL => {
                // On the host's own stack there is no way to pause for the
                // compiler; a non-string answers itself, and a string is
                // refused rather than half-run.
                let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                if first.is_string() {
                    Err(self.throw_type_error())
                } else {
                    Ok(first)
                }
            }
            native::OBJECT_VALUE_OF => Ok(this),
            native::ARRAY_TO_STRING => self.join_array(this, None),
            native::ERROR_TO_STRING => {
                let name_key = self.ascii_key(b"name")?;
                let message_key = self.ascii_key(b"message")?;
                let name = self.get_property(this, name_key)?;
                let message = self.get_property(this, message_key)?;
                let name = if name.is_undefined() {
                    self.ascii_string(b"Error")?
                } else {
                    self.coerce_to_string(name)?
                };
                let message = if message.is_undefined() {
                    self.ascii_string(b"")?
                } else {
                    self.coerce_to_string(message)?
                };
                let message_length = string::length(self.heap, message.as_handle())
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                if message_length == 0 {
                    return Ok(name);
                }
                let separator = self.ascii_string(b": ")?;
                let joined = self.concat_values(name, separator)?;
                self.concat_values(joined, message)
            }
            native::ERROR
            | native::TYPE_ERROR
            | native::RANGE_ERROR
            | native::REFERENCE_ERROR
            | native::SYNTAX_ERROR
            | native::EVAL_ERROR
            | native::URI_ERROR
            | native::SUPPRESSED_ERROR
            | native::AGGREGATE_ERROR => {
                // Called without `new`, an error constructor builds an error
                // just the same.
                let kind = Realm::kind_of(id)
                    .ok_or(Completion::Terminated(Termination::NotImplemented))?;
                self.error_from_arguments(kind, arguments)
            }
            native::ARRAY_JOIN => {
                let separator = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                let separator = if separator.is_undefined() {
                    None
                } else {
                    Some(self.coerce_to_string(separator)?)
                };
                self.join_array(this, separator)
            }
            native::PROMISE_THEN => {
                let on_fulfilled = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                let on_rejected = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
                self.promise_then(this, on_fulfilled, on_rejected)
            }
            native::PROMISE_CATCH => {
                let on_rejected = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                self.promise_then(this, Value::UNDEFINED, on_rejected)
            }
            native::PROMISE_FINALLY => {
                let callback = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                let step = self.finally_function(native::PROMISE_FINALLY_STEP, callback)?;
                let rethrow = self.finally_function(native::PROMISE_FINALLY_RETHROW, callback)?;
                self.promise_then(this, step, rethrow)
            }
            native::PROMISE_FINALLY_STEP | native::PROMISE_FINALLY_RETHROW => {
                let Some(function) = self.current_native else {
                    return Err(Completion::Terminated(Termination::Malformed));
                };
                let callback = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                if self.is_callable_value(callback) {
                    let _ = self.call_value(callback, Value::UNDEFINED, &[])?;
                }
                let value = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                if id == native::PROMISE_FINALLY_RETHROW {
                    return Err(Completion::Throw(value));
                }
                Ok(value)
            }
            native::OBJECT_DEFINE_GETTER | native::OBJECT_DEFINE_SETTER => {
                let target = self.coerce_to_object(this)?;
                let key_value = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                let key = self.coerce_to_key(key_value)?;
                let accessor = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
                if !self.is_callable_value(accessor) {
                    return Err(self.throw_type_error());
                }
                let getter = id == native::OBJECT_DEFINE_GETTER;
                self.define_accessor(
                    target,
                    key,
                    accessor,
                    getter,
                    attribute::ENUMERABLE | attribute::CONFIGURABLE,
                )?;
                Ok(Value::UNDEFINED)
            }
            native::OBJECT_LOOKUP_GETTER | native::OBJECT_LOOKUP_SETTER => {
                let target = self.coerce_to_object(this)?;
                let key_value = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                let key = self.coerce_to_key(key_value)?;
                if self.hidden_key(key) {
                    return Ok(Value::UNDEFINED);
                }
                let mut holder = target;
                while holder.is_object() {
                    let found = object::get_own_property(self.heap, holder.as_handle(), key)
                        .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                    if let Some(descriptor) = found {
                        if matches!(descriptor.kind, object::DescriptorKind::Accessor) {
                            return Ok(if id == native::OBJECT_LOOKUP_GETTER {
                                descriptor.getter
                            } else {
                                descriptor.setter
                            });
                        }
                        return Ok(Value::UNDEFINED);
                    }
                    holder = object::prototype(self.heap, holder.as_handle())
                        .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                }
                Ok(Value::UNDEFINED)
            }
            native::PROMISE_ALL
            | native::PROMISE_RACE
            | native::PROMISE_ALL_SETTLED
            | native::PROMISE_ANY => {
                let iterable = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                self.promise_combinator(id, iterable)
            }
            native::COMBINE_FULFILLED | native::COMBINE_REJECTED => {
                let value = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                self.combine_settled(id == native::COMBINE_FULFILLED, value)?;
                Ok(Value::UNDEFINED)
            }
            native::PROMISE_WITH_RESOLVERS => {
                // A promise beside the functions that settle it.
                let promise = self.new_promise()?;
                let resolve = self.settle_function(native::PROMISE_SETTLE_FULFILLED, promise)?;
                let reject = self.settle_function(native::PROMISE_SETTLE_REJECTED, promise)?;
                let result = object::create(self.heap, Value::object(self.realm.object_prototype))
                    .map_err(|_| self.heap_failure())?;
                let held = Value::object(result);
                for (name, value) in [
                    (&b"promise"[..], Value::object(promise)),
                    (&b"resolve"[..], resolve),
                    (&b"reject"[..], reject),
                ] {
                    let key = self.ascii_key(name)?;
                    self.set_property(held, key, value)?;
                }
                Ok(held)
            }
            native::PROMISE_RESOLVE | native::PROMISE_REJECT => {
                let value = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                // A promise whose constructor is this very Promise passes
                // through Promise.resolve as itself.
                if id == native::PROMISE_RESOLVE
                    && value.is_object()
                    && object::is_promise(self.heap, value.as_handle()).unwrap_or(false)
                {
                    let constructor_key = self.ascii_key(b"constructor")?;
                    let constructor = self.get_property(value, constructor_key)?;
                    if constructor.is_object()
                        && this.is_object()
                        && constructor.as_handle() == this.as_handle()
                    {
                        return Ok(value);
                    }
                }
                let handle = self.new_promise()?;
                if id == native::PROMISE_RESOLVE {
                    self.resolve(handle, value)?;
                } else {
                    self.settle(handle, promise::REJECTED, value)?;
                }
                Ok(Value::object(handle))
            }
            native::ASYNC_GEN_DRAIN => {
                let Some(function) = self.current_native else {
                    return Err(Completion::Terminated(Termination::Malformed));
                };
                let generator = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                self.drain_async_generator(generator)?;
                Ok(Value::UNDEFINED)
            }
            native::WEAK_REF => {
                // A WeakRef answers only to `new`.
                Err(self.throw_type_error())
            }
            native::PROXY
            | native::ARRAY_BUFFER
            | native::SHARED_ARRAY_BUFFER
            | native::TYPED_ARRAY
            | native::TYPED_ARRAY_BASE
            | native::DATA_VIEW => {
                // Each answers only to `new` — and `%TypedArray%` not even
                // to that.
                Err(self.throw_type_error())
            }
            native::TYPED_ARRAY_OF..=native::SHARED_ARRAY_BUFFER
            | native::ARRAY_BUFFER_IMMUTABLE
            | native::ARRAY_BUFFER_TRANSFER_TO_IMMUTABLE => {
                self.typed_array_native(id, this, arguments)
            }
            native::PROXY_CALL => {
                // A proxy over a callable: the `apply` trap, or the target.
                let Some(function) = self.current_native else {
                    return Err(Completion::Terminated(Termination::Malformed));
                };
                let (target, handler) = self.proxy_parts(Value::object(function))?;
                let trap = self.proxy_trap(handler, b"apply")?;
                if trap.is_undefined() {
                    return self.call_value(target, this, arguments);
                }
                let list = self.create_array()?;
                for &argument in arguments {
                    self.append_element(list, Some(argument))?;
                }
                self.call_value(trap, handler, &[target, this, list])
            }
            native::SPECIES_GETTER => Ok(this),
            native::DYNAMIC_IMPORT_STEP => {
                // One watched completion settled; whatever else the target
                // still waits on is waited out through a fresh promise the
                // reaction's own answer adopts. Nothing left waiting means
                // the namespace — or the error some dependency recorded.
                let Some(function) = self.current_native else {
                    return Err(Completion::Terminated(Termination::Malformed));
                };
                let binding = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                let module = value::to_uint32(self.element(binding, 0)?.as_number());
                // A body left untouched behind a gate is tried again now.
                if self.module_status(module) == 0 {
                    let mut gate = Value::UNDEFINED;
                    self.evaluate_module_gated(module, false, &mut gate)?;
                    if gate.is_object() {
                        let next = self.new_promise()?;
                        self.chain_namespace(next, gate, module, module, false)?;
                        return Ok(Value::object(next));
                    }
                }
                let mut pending = Value::UNDEFINED;
                let mut owner = module;
                let mut seen = [u32::MAX; MAX_UNIT_REALMS];
                let mut count = 0usize;
                self.evaluate_async_reachable(
                    module,
                    &mut seen,
                    &mut count,
                    &mut pending,
                    &mut owner,
                )?;
                if pending.is_object() {
                    let next = self.new_promise()?;
                    self.chain_namespace(next, pending, module, owner, false)?;
                    return Ok(Value::object(next));
                }
                if self.module_status(module) == 3 {
                    let reason = self.module_completion(module);
                    return Err(Completion::Throw(reason));
                }
                self.namespace_of(module)
            }
            native::DYNAMIC_IMPORT_REJECTED => {
                // A module's completion rejected: the whole cycle takes the
                // error, and the rejection carries on to the import.
                let Some(function) = self.current_native else {
                    return Err(Completion::Terminated(Termination::Malformed));
                };
                let binding = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                let module = value::to_uint32(self.element(binding, 0)?.as_number());
                let reason = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                self.poison_cycle(module, reason);
                Err(Completion::Throw(reason))
            }
            native::ACCESSOR_GET | native::ACCESSOR_SET => {
                // An auto-accessor: the function carries the hidden name its
                // field stores behind on the receiver.
                let Some(function) = self.current_native else {
                    return Err(Completion::Terminated(Termination::Malformed));
                };
                let backing = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                let key = self.coerce_to_key(backing)?;
                if !this.is_object() {
                    return Err(self.throw_type_error());
                }
                if id == native::ACCESSOR_GET {
                    let held = object::get_own_property(self.heap, this.as_handle(), key)
                        .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                    Ok(held.map_or(Value::UNDEFINED, |descriptor| descriptor.value))
                } else {
                    let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                    object::define_own_property(
                        self.heap,
                        this.as_handle(),
                        key,
                        Descriptor::data(first, attribute::WRITABLE),
                    )
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                    Ok(Value::UNDEFINED)
                }
            }
            native::PROXY_REVOCABLE => {
                let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                let second = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
                let proxy = self.construct_proxy(first, second)?;
                let revoke = object::create_native(
                    self.heap,
                    Value::object(self.realm.function_prototype),
                    native::PROXY_REVOKE,
                    0,
                )
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                let held = self.ascii_key(b"\0proxy")?;
                object::define_own_property(
                    self.heap,
                    revoke,
                    held,
                    Descriptor::data(proxy, attribute::WRITABLE),
                )
                .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                let result = object::create(self.heap, Value::object(self.realm.object_prototype))
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                for (name, value) in [
                    (&b"proxy"[..], proxy),
                    (&b"revoke"[..], Value::object(revoke)),
                ] {
                    let key = self.ascii_key(name)?;
                    object::define_own_property(
                        self.heap,
                        result,
                        key,
                        Descriptor::data(value, attribute::DEFAULT),
                    )
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                }
                Ok(Value::object(result))
            }
            native::PROXY_REVOKE => {
                // Revoking cuts the proxy from its target and handler: every
                // later operation on it is a TypeError, though `typeof` still
                // answers what it always did.
                let Some(function) = self.current_native else {
                    return Err(Completion::Terminated(Termination::Malformed));
                };
                let held = self.ascii_key(b"\0proxy")?;
                let proxy = object::get_own_property(self.heap, function, held)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?
                    .map_or(Value::UNDEFINED, |descriptor| descriptor.value);
                if proxy.is_object() {
                    object::define_own_property(
                        self.heap,
                        function,
                        held,
                        Descriptor::data(Value::NULL, attribute::WRITABLE),
                    )
                    .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                    for name in [&b"\0target"[..], &b"\0handler"[..]] {
                        let key = self.ascii_key(name)?;
                        object::define_own_property(
                            self.heap,
                            proxy.as_handle(),
                            key,
                            Descriptor::data(Value::NULL, attribute::WRITABLE),
                        )
                        .map_err(|_| Completion::Terminated(Termination::HeapExhausted))?;
                    }
                }
                Ok(Value::UNDEFINED)
            }
            native::CREATE_REALM => self.create_realm(),
            native::ARRAY_BUFFER_SLICE => {
                let bytes = self.array_buffer_bytes(this)?;
                let length = self.length_of(bytes)?;
                let start = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                let end = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
                let from = self.relative_index(start, length, 0)?;
                let to = self.relative_index(end, length, length)?;
                let count = to.saturating_sub(from);
                // SpeciesConstructor: the receiver's constructor's species,
                // or ArrayBuffer itself.
                let constructor_key = self.ascii_key(b"constructor")?;
                let own_constructor = self.get_property(this, constructor_key)?;
                let mut species = Value::UNDEFINED;
                if own_constructor.is_object() {
                    species =
                        self.get_property(own_constructor, Key::Symbol(self.realm.species_symbol))?;
                } else if !own_constructor.is_undefined() {
                    return Err(self.throw_type_error());
                }
                let made = if species.is_nullish() {
                    self.construct_array_buffer(
                        Value::number(f64::from(count)),
                        Value::UNDEFINED,
                        false,
                    )?
                } else {
                    self.pending_new_target = species;
                    self.construct(species, &[Value::number(f64::from(count))])?
                };
                let target_bytes = self.array_buffer_bytes(made)?;
                let mut index = 0u32;
                while index < count {
                    let held = self.element(bytes, from + index)?;
                    self.set_element(target_bytes, index, held)?;
                    index += 1;
                }
                Ok(made)
            }
            native::DATE => {
                // Called, `Date` answers the string of now, arguments ignored.
                let now = self.date_now();
                self.date_to_string(now, DateForm::Full)
            }
            native::DATE_NOW => Ok(Value::number(self.date_now())),
            native::DATE_UTC => self.date_from_components(arguments),
            native::DATE_PARSE => {
                let text = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                let text = self.coerce_to_string(text)?;
                let parsed = self.date_parse(text.as_handle())?;
                Ok(Value::number(parsed))
            }
            native::DATE_GET_TIME
            | native::DATE_GET_FULL_YEAR
            | native::DATE_GET_MONTH
            | native::DATE_GET_DATE
            | native::DATE_GET_DAY
            | native::DATE_GET_HOURS
            | native::DATE_GET_MINUTES
            | native::DATE_GET_SECONDS
            | native::DATE_GET_MILLISECONDS
            | native::DATE_GET_TIMEZONE_OFFSET => {
                let time = self.date_time_of(this)?;
                if time.is_nan() {
                    return Ok(Value::number(f64::NAN));
                }
                let fields = date_fields(time);
                let answer = match id {
                    native::DATE_GET_TIME => time,
                    native::DATE_GET_FULL_YEAR => f64::from(fields.year),
                    native::DATE_GET_MONTH => f64::from(fields.month),
                    native::DATE_GET_DATE => f64::from(fields.date),
                    native::DATE_GET_DAY => f64::from(fields.weekday),
                    native::DATE_GET_HOURS => f64::from(fields.hours),
                    native::DATE_GET_MINUTES => f64::from(fields.minutes),
                    native::DATE_GET_SECONDS => f64::from(fields.seconds),
                    native::DATE_GET_MILLISECONDS => f64::from(fields.milliseconds),
                    _ => 0.0,
                };
                Ok(Value::number(answer))
            }
            native::DATE_SET_FULL_YEAR
            | native::DATE_SET_MONTH
            | native::DATE_SET_DATE
            | native::DATE_SET_HOURS
            | native::DATE_SET_MINUTES
            | native::DATE_SET_SECONDS
            | native::DATE_SET_MILLISECONDS => {
                // The fields the call names replace the date's own; the
                // rest stand, and a NaN date takes a year but nothing else.
                let time = self.date_time_of(this)?;
                let base = if time.is_nan() {
                    if id == native::DATE_SET_FULL_YEAR {
                        0.0
                    } else {
                        return Ok(Value::number(f64::NAN));
                    }
                } else {
                    time
                };
                let fields = date_fields(base);
                let mut parts = [
                    f64::from(fields.year),
                    f64::from(fields.month),
                    f64::from(fields.date),
                    f64::from(fields.hours),
                    f64::from(fields.minutes),
                    f64::from(fields.seconds),
                    f64::from(fields.milliseconds),
                ];
                let (first, most) = match id {
                    native::DATE_SET_FULL_YEAR => (0usize, 3usize),
                    native::DATE_SET_MONTH => (1, 2),
                    native::DATE_SET_DATE => (2, 1),
                    native::DATE_SET_HOURS => (3, 4),
                    native::DATE_SET_MINUTES => (4, 3),
                    native::DATE_SET_SECONDS => (5, 2),
                    _ => (6, 1),
                };
                let mut index = 0usize;
                while index < most {
                    let argument = arguments.get(index).copied();
                    let Some(argument) = argument else {
                        if index == 0 {
                            parts[first] = f64::NAN;
                        }
                        break;
                    };
                    parts[first + index] = self.coerce_to_number(argument)?;
                    index += 1;
                }
                let made = time_clip(make_date(
                    parts[0], parts[1], parts[2], parts[3], parts[4], parts[5], parts[6],
                ));
                self.date_set_time(this, made)?;
                Ok(Value::number(made))
            }
            native::DATE_SET_TIME => {
                let time = self.date_time_of(this)?;
                let _ = time;
                let value = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                let clipped = time_clip(self.coerce_to_number(value)?);
                self.date_set_time(this, clipped)?;
                Ok(Value::number(clipped))
            }
            native::DATE_TO_STRING
            | native::DATE_TO_UTC_STRING
            | native::DATE_TO_DATE_STRING
            | native::DATE_TO_TIME_STRING => {
                let time = self.date_time_of(this)?;
                let form = match id {
                    native::DATE_TO_UTC_STRING => DateForm::Utc,
                    native::DATE_TO_DATE_STRING => DateForm::Date,
                    native::DATE_TO_TIME_STRING => DateForm::Time,
                    _ => DateForm::Full,
                };
                self.date_to_string(time, form)
            }
            native::DATE_TO_ISO_STRING => {
                let time = self.date_time_of(this)?;
                if time.is_nan() {
                    return Err(self.throw_error_of(ErrorKind::Range));
                }
                self.date_to_string(time, DateForm::Iso)
            }
            native::DATE_TO_JSON => {
                // Generic over its receiver: an invalid date is null, any
                // other calls its own toISOString.
                let primitive = self.coerce_to_primitive(this, Hint::Number)?;
                if matches!(primitive.tag(), Tag::Number) && !primitive.as_number().is_finite() {
                    return Ok(Value::NULL);
                }
                let key = self.ascii_key(b"toISOString")?;
                let method = self.get_property(this, key)?;
                if !self.is_callable_value(method) {
                    return Err(self.throw_type_error());
                }
                self.call_value(method, this, &[])
            }
            native::DATE_TO_PRIMITIVE => {
                // Date's own: a default hint asks for a string first.
                if !this.is_object() {
                    return Err(self.throw_type_error());
                }
                let hint = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                let number_hint = self.ascii_string(b"number")?;
                let string_hint = self.ascii_string(b"string")?;
                let default_hint = self.ascii_string(b"default")?;
                let order: [&[u8]; 2] = if self.strict_equals(hint, number_hint)? {
                    [b"valueOf", b"toString"]
                } else if self.strict_equals(hint, string_hint)?
                    || self.strict_equals(hint, default_hint)?
                {
                    [b"toString", b"valueOf"]
                } else {
                    return Err(self.throw_type_error());
                };
                for name in order {
                    let key = self.ascii_key(name)?;
                    let method = self.get_property(this, key)?;
                    if self.is_callable_value(method) {
                        let result = self.call_value(method, this, &[])?;
                        if !result.is_object() {
                            return Ok(result);
                        }
                    }
                }
                Err(self.throw_type_error())
            }
            native::WEAK_REF_DEREF => {
                let key = self.ascii_key(b"\0target")?;
                let held = if this.is_object() {
                    object::get_own_property(self.heap, this.as_handle(), key)
                        .map_err(|_| Completion::Terminated(Termination::Malformed))?
                } else {
                    None
                };
                match held {
                    Some(descriptor) => Ok(descriptor.value),
                    None => Err(self.throw_type_error()),
                }
            }
            native::MATH_RANDOM => {
                // xorshift64*: a fixed sequence, replayable exactly, with
                // 53 bits of it scaled into [0, 1).
                let mut state = self.random_state;
                state ^= state >> 12;
                state ^= state << 25;
                state ^= state >> 27;
                self.random_state = state;
                let mixed = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
                let fraction = crate::softfloat::from_u64(mixed >> 11) / 9_007_199_254_740_992.0;
                Ok(Value::number(fraction))
            }
            native::JSON_PARSE => {
                let text = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                let reviver = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
                self.json_parse(text, reviver)
            }
            native::JSON_STRINGIFY => {
                let value = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                let replacer = arguments.get(1).copied().unwrap_or(Value::UNDEFINED);
                let space = arguments.get(2).copied().unwrap_or(Value::UNDEFINED);
                self.json_stringify(value, replacer, space)
            }
            native::ASYNC_FROM_SYNC_NEXT
            | native::ASYNC_FROM_SYNC_RETURN
            | native::ASYNC_FROM_SYNC_THROW => {
                let Some(function) = self.current_native else {
                    return Err(Completion::Terminated(Termination::Malformed));
                };
                let wrapper = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                self.async_from_sync_step(id, wrapper, arguments.first().copied())
            }
            native::ASYNC_FROM_SYNC_MORE | native::ASYNC_FROM_SYNC_DONE => {
                let value = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                self.iteration_result(value, id == native::ASYNC_FROM_SYNC_DONE)
            }
            native::ASYNC_FROM_SYNC_CLOSE | native::ASYNC_FROM_SYNC_PASS => {
                let reason = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                if id == native::ASYNC_FROM_SYNC_CLOSE {
                    let Some(function) = self.current_native else {
                        return Err(Completion::Terminated(Termination::Malformed));
                    };
                    let sync = object::function_environment(self.heap, function)
                        .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                    // The rejection outranks whatever the close throws.
                    match self.close_iterator(sync) {
                        Ok(()) | Err(Completion::Throw(_)) => {}
                        Err(other) => return Err(other),
                    }
                }
                Err(Completion::Throw(reason))
            }
            native::ASYNC_GEN_RETURN_FULFILLED | native::ASYNC_GEN_RETURN_REJECTED => {
                let Some(function) = self.current_native else {
                    return Err(Completion::Terminated(Termination::Malformed));
                };
                let generator = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                let value = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                if generator.is_object() {
                    let async_bit = self.generator_async_bit(generator);
                    let _ = object::set_generator(
                        self.heap,
                        generator.as_handle(),
                        object::generator_state::DONE | async_bit,
                        Value::UNDEFINED,
                    );
                }
                let outcome = if id == native::ASYNC_GEN_RETURN_FULFILLED {
                    Ok(value)
                } else {
                    Err(value)
                };
                self.settle_pending_next(generator, outcome, true)?;
                Ok(Value::UNDEFINED)
            }
            native::ASYNC_GEN_YIELD_FULFILLED | native::ASYNC_GEN_YIELD_REJECTED => {
                let Some(function) = self.current_native else {
                    return Err(Completion::Terminated(Termination::Malformed));
                };
                let generator = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                let value = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                if id == native::ASYNC_GEN_YIELD_FULFILLED {
                    self.settle_pending_next(generator, Ok(value), false)?;
                } else {
                    if generator.is_object() {
                        let async_bit = self.generator_async_bit(generator);
                        let _ = object::set_generator(
                            self.heap,
                            generator.as_handle(),
                            object::generator_state::DONE | async_bit,
                            Value::UNDEFINED,
                        );
                    }
                    self.settle_pending_next(generator, Err(value), true)?;
                }
                Ok(Value::UNDEFINED)
            }
            native::GENERATOR_NEXT | native::GENERATOR_RETURN | native::GENERATOR_THROW => {
                let Some(function) = self.current_native else {
                    return Err(Completion::Terminated(Termination::Malformed));
                };
                let bound = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                let argument = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                self.generator_resume(bound, id, argument)
            }
            native::DEFAULT_CONSTRUCTOR => {
                // A class constructor answers only to `new`.
                Err(self.throw_type_error())
            }
            native::PRINT => {
                let value = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                let text = self.coerce_to_string(value)?;
                let expected = self.ascii_string(b"Test262:AsyncTestComplete")?;
                let complete = self.strict_equals(text, expected)?;
                // The first report wins: the async test protocol prints once.
                if self.print_status == 0 {
                    self.print_status = if complete { 1 } else { 2 };
                }
                Ok(Value::UNDEFINED)
            }
            native::ASYNC_RESUME_FULFILLED | native::ASYNC_RESUME_REJECTED => {
                // A resume function carries the suspended frame it wakes.
                let Some(function) = self.current_native else {
                    return Err(Completion::Terminated(Termination::Malformed));
                };
                let coroutine = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                let value = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                self.resume_coroutine(
                    coroutine,
                    value,
                    if id == native::ASYNC_RESUME_REJECTED {
                        resume::THROW
                    } else {
                        resume::NEXT
                    },
                )
            }
            native::PROMISE_SETTLE_FULFILLED | native::PROMISE_SETTLE_REJECTED => {
                // A resolve or reject function carries the promise it settles.
                // It is called as a plain function, so the promise comes from
                // the function object rather than from a receiver.
                let Some(function) = self.current_native else {
                    return Err(Completion::Terminated(Termination::Malformed));
                };
                let bound = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                if !bound.is_object() {
                    return Ok(Value::UNDEFINED);
                }
                let value = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                if id == native::PROMISE_SETTLE_FULFILLED {
                    self.resolve(bound.as_handle(), value)?;
                } else {
                    self.settle(bound.as_handle(), promise::REJECTED, value)?;
                }
                Ok(Value::UNDEFINED)
            }
            native::SYMBOL
            | native::SYMBOL_TO_STRING
            | native::SYMBOL_DESCRIPTION
            | native::SYMBOL_VALUE_OF => self.symbol_native(id, this, arguments),
            native::OBJECT
            | native::OBJECT_GET_OWN_PROPERTY_DESCRIPTOR
            | native::OBJECT_KEYS..=native::OBJECT_IS
            | native::OBJECT_PREVENT_EXTENSIONS..=native::OBJECT_IS_SEALED => {
                self.object_native(id, this, arguments)
            }
            native::REFLECT_GET..=native::REFLECT_CONSTRUCT => self.reflect_native(id, arguments),
            native::OBJECT_GET_OWN_PROPERTY_SYMBOLS | native::OBJECT_DEFINE_PROPERTIES => {
                self.object_native(id, this, arguments)
            }
            native::MAP..=native::SET_VALUES => self.collection_native(id, this, arguments),
            native::WEAK_MAP..=native::WEAK_SET_DELETE => {
                self.weak_collection_native(id, this, arguments)
            }
            native::ENCODE_URI..=native::DECODE_URI_COMPONENT => {
                let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                self.uri_native(id, first)
            }
            native::ARRAY | native::ARRAY_IS_ARRAY..=native::ARRAY_SORT => {
                self.array_native(id, this, arguments)
            }
            native::STRING | native::STRING_FROM_CHAR_CODE..=native::STRING_VALUES => {
                self.string_native(id, this, arguments)
            }
            native::NUMBER
            | native::BOOLEAN
            | native::NUMBER_IS_INTEGER..=native::BOOLEAN_VALUE_OF
            | native::NUMBER_TO_EXPONENTIAL
            | native::NUMBER_TO_PRECISION => self.number_native(id, this, arguments),
            native::MATH_ABS..=native::MATH_HYPOT | native::MATH_SIN..=native::MATH_CBRT => {
                self.math_native(id, arguments)
            }
            native::FUNCTION_PROTOTYPE_CALL..=native::BOUND_FUNCTION => {
                self.function_native(id, this, arguments)
            }
            native::ITERATOR_NEXT | native::ITERATOR_SELF => {
                self.iterator_native(id, this, arguments)
            }
            native::BIG_INT => {
                let value = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                let primitive = self.coerce_to_primitive(value, Hint::Number)?;
                self.big_int_of(primitive)
            }
            native::BIG_INT_TO_STRING => {
                let receiver = self.primitive_this(this)?;
                let radix = match arguments.first().copied() {
                    Some(value) if !value.is_undefined() => {
                        let radix = value::to_uint32(self.coerce_to_number(value)?);
                        if !(2..=36).contains(&radix) {
                            return Err(self.throw_error_of(ErrorKind::Range));
                        }
                        radix
                    }
                    _ => 10,
                };
                self.big_int_text(receiver, radix)
            }
            native::BIG_INT_VALUE_OF => self.primitive_this(this),
            native::NAMESPACE_GET => {
                // The getter carries which module and which slot it reads.
                let Some(function) = self.current_native else {
                    return Err(Completion::Terminated(Termination::Malformed));
                };
                let binding = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::Terminated(Termination::Malformed))?;
                let module = value::to_uint32(self.element(binding, 0)?.as_number());
                let slot = value::to_uint32(self.element(binding, 1)?.as_number());
                let held = self.element(binding, 2)?;
                if slot == u32::MAX {
                    // The binding names the module itself: a dynamic
                    // import's reaction answers the namespace — the
                    // deferred one, when the import was `import.defer`.
                    if held.is_number() && value::to_uint32(held.as_number()) & 4 != 0 {
                        return self.deferred_namespace_of(module);
                    }
                    return self.namespace_of(module);
                }
                if held.is_number() {
                    // A deferred namespace's getter: reading an export is a
                    // meaningful use — except `then`, which answers
                    // undefined while the module waits.
                    let flags = value::to_uint32(held.as_number());
                    if flags & 1 != 0 && self.module_status(module) != 2 {
                        if flags & 2 != 0 {
                            return Ok(Value::UNDEFINED);
                        }
                        match self.module_status(module) {
                            0 => {
                                self.deferred_ready(module, true)?;
                                self.evaluate_module_now(module, true)?;
                            }
                            1 | 4 => return Err(self.throw_type_error()),
                            3 => {
                                let error = self.module_completion(module);
                                return Err(Completion::Throw(error));
                            }
                            _ => {}
                        }
                    }
                }
                if slot & crate::bytecode::EXPORT_IMPORT_MARK != 0 {
                    // The export is the module's own import: read through it.
                    return self.import_value(module, slot & !crate::bytecode::EXPORT_IMPORT_MARK);
                }
                let environment = self.module_environment(module);
                if !environment.is_object() {
                    return Ok(Value::UNDEFINED);
                }
                match env::slot_value(self.heap, environment.as_handle(), slot) {
                    Ok(value) => Ok(value),
                    Err(env::EnvironmentError::Uninitialised) => Err(self.throw_reference_error()),
                    Err(_) => Ok(Value::UNDEFINED),
                }
            }
            native::REG_EXP
            | native::REG_EXP_EXEC
            | native::REG_EXP_TEST
            | native::REG_EXP_TO_STRING
            | native::STRING_MATCH
            | native::STRING_SEARCH => self.regexp_native(id, this, arguments),
            id if id >= native::BINDING_BASE => {
                self.host_call(id - native::BINDING_BASE, arguments)
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
    }
}
