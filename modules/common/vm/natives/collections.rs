//! `Map`, `Set`, `WeakMap`, `WeakSet`, and `WeakRef`.

use super::*;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// `new WeakRef(target)`: the target rides as a hidden property, held
    /// as strongly as any reference — a collection is never observed here.
    pub(in crate::vm) fn construct_weak_ref(&mut self, target: Value) -> Result<Value, Completion> {
        if !target.is_object() && !matches!(target.tag(), Tag::Symbol) {
            return Err(self.throw_type_error());
        }
        let made = self.new_instance_of(self.realm.weak_ref_prototype)?;
        let key = self.ascii_key(b"\0target")?;
        object::define_own_property(
            self.heap,
            made,
            key,
            Descriptor::data(target, attribute::WRITABLE),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(Value::object(made))
    }

    /// Whether two values are the same for a keyed collection: strict
    /// equality with NaN equal to itself.
    pub(in crate::vm) fn same_value_zero(
        &mut self,
        left: Value,
        right: Value,
    ) -> Result<bool, Completion> {
        if self.strict_equals(left, right)? {
            return Ok(true);
        }
        let left_nan = matches!(left.tag(), Tag::Number) && left.as_number().is_nan();
        let right_nan = matches!(right.tag(), Tag::Number) && right.as_number().is_nan();
        Ok(left_nan && right_nan)
    }

    /// The two hidden arrays behind a Map, or the one behind a Set.
    pub(in crate::vm) fn collection_arrays(
        &mut self,
        target: Value,
        make: bool,
    ) -> Result<Option<(Value, Value)>, Completion> {
        self.branded_arrays(target, make, false)
    }

    /// The hidden arrays under either brand: `weak` names a WeakMap or
    /// WeakSet, whose methods must not answer for a Map or Set.
    pub(in crate::vm) fn branded_arrays(
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
            .map_err(|_| Completion::MALFORMED)?;
        if let Some(descriptor) = held {
            let vals = object::get_own_property(self.heap, target.as_handle(), vals_key)
                .map_err(|_| Completion::MALFORMED)?
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
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        Ok(Some((keys, vals)))
    }

    /// Where `key` sits in the collection's key array, if it is a member.
    pub(in crate::vm) fn collection_find(
        &mut self,
        keys: Value,
        key: Value,
    ) -> Result<Option<u32>, Completion> {
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
    pub(in crate::vm) fn collection_remove(
        &mut self,
        array: Value,
        at: u32,
    ) -> Result<(), Completion> {
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
    pub(in crate::vm) fn collection_native(
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
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                Ok(Value::object(handle))
            }
            _ => Err(Completion::Terminated(Termination::NotImplemented)),
        }
    }

    /// `WeakMap` and `WeakSet`: a key is an object or a symbol, and nothing
    /// else, since only those could ever be held weakly. This engine never
    /// observes a collection, so a member stays until deleted.
    pub(in crate::vm) fn weak_collection_native(
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
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
}
