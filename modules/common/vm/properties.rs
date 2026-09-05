//! Property access: reading, writing, defining, and deleting through the
//! ordinary and exotic behaviours, `super`, private names, and arrays.

use super::*;

/// The most arguments one call passes.
/// Own keys one operation may walk at once.
pub(super) const MAX_OWN_KEYS: usize = 128;

/// Interpreter loops one host stack may nest: a native that runs a callback
/// which reaches another native that runs a callback, so far and no further.
pub(super) const MAX_NESTED_ENTRIES: u32 = 64;

/// The most own keys one spread copies.
pub(super) const MAX_COPIED_KEYS: usize = 64;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    // Properties.

    pub(super) fn get_property(&mut self, target: Value, key: Key) -> Result<Value, Completion> {
        if target.is_nullish() {
            return Err(self.throw_type_error());
        }
        // A mapped arguments index reads its parameter's slot.
        if let (Key::Index(index), true) = (key, target.is_object()) {
            if let Ok(Some((environment, mapped))) =
                object::arguments_map(self.heap, target.as_handle())
            {
                if index < 32 && mapped & (1 << index) != 0 && environment.is_object() {
                    if let Ok(value) = env::slot_value(self.heap, environment.as_handle(), index) {
                        return Ok(value);
                    }
                }
            }
        }
        self.materialise_prototype(target, key)?;
        self.materialise_function_facts(target, key)?;
        if matches!(target.tag(), Tag::String) {
            if let Some(value) = self.string_property(target, key)? {
                return Ok(value);
            }
            // A string's other properties are its prototype's, and a method
            // found there is called with the string itself as its receiver.
            return self.prototype_property(self.realm.string_prototype, target, key);
        }
        if !target.is_object() {
            let prototype = match target.tag() {
                Tag::Number => self.realm.number_prototype,
                Tag::Boolean => self.realm.boolean_prototype,
                Tag::Symbol => self.realm.symbol_prototype,
                Tag::BigInt => self.realm.big_int_prototype,
                _ => return Ok(Value::UNDEFINED),
            };
            return self.prototype_property(prototype, target, key);
        }
        match object::exotic_kind(self.heap, target.as_handle()).unwrap_or(0) {
            object::exotic::PROXY if !self.hidden_key(key) => {
                return self.proxy_get(target, key);
            }
            object::exotic::TYPED_ARRAY => {
                if let Key::Index(index) = key {
                    return self.typed_array_read(target, index);
                }
            }
            object::exotic::DEFERRED if !self.hidden_key(key) => {
                self.deferred_trigger(target, Some(key))?;
                // `then` on a module still deferred answers undefined, so
                // resolving a promise with the namespace never runs it.
                if !self.deferred_done(target) {
                    let then = self.ascii_key(b"then")?;
                    if key == then {
                        return Ok(Value::UNDEFINED);
                    }
                }
            }
            _ => {}
        }
        if self.deferred_live && !self.hidden_key(key) {
            self.deferred_chain_trigger(target, key)?;
        }
        let lookup =
            object::get(self.heap, target.as_handle(), key).map_err(|_| Completion::MALFORMED)?;
        match lookup {
            Lookup::Absent => Ok(Value::UNDEFINED),
            Lookup::Value(value) => Ok(value),
            Lookup::Accessor(getter) => {
                if getter.is_undefined() {
                    return Ok(Value::UNDEFINED);
                }
                self.call_value(getter, target, &[])
            }
        }
    }

    /// Give a function the `prototype` object it is supposed to have, the first
    /// time anything asks for it.
    ///
    /// Every ordinary function has one, and an instance made from the function
    /// inherits from it — that is what makes `A.prototype.method = ...` reach
    /// every `new A()`. Building it when it is asked for rather than when the
    /// closure is made costs a program that never uses it nothing, and a heap
    /// this size notices the difference: a callback in a loop is a closure too.
    pub(super) fn materialise_prototype(
        &mut self,
        target: Value,
        key: Key,
    ) -> Result<(), Completion> {
        if !target.is_object() {
            return Ok(());
        }
        let handle = target.as_handle();
        let flags = object::function_flags(self.heap, handle).unwrap_or(0);
        if flags & object::function_flag::CONSTRUCTOR == 0 {
            return Ok(());
        }
        // A bound function constructs through its target but owns no
        // `prototype` of its own; nor does `Proxy`, or a proxy over a
        // constructor.
        if object::is_native(self.heap, handle).unwrap_or(false)
            && matches!(
                object::function_code(self.heap, handle),
                Ok(native::BOUND_FUNCTION | native::PROXY | native::PROXY_CALL)
            )
        {
            return Ok(());
        }
        let name = self.ascii_key(b"prototype")?;
        if key != name {
            return Ok(());
        }
        if object::get_own_property(self.heap, handle, name)
            .unwrap_or(None)
            .is_some()
        {
            return Ok(());
        }
        let prototype = object::create(self.heap, Value::object(self.realm.object_prototype))
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        // `constructor` points back, and neither property is enumerable: they
        // are the object's machinery, not its contents.
        let constructor = self.ascii_key(b"constructor")?;
        object::define_own_property(
            self.heap,
            prototype,
            constructor,
            object::Descriptor::data(
                target,
                object::attribute::WRITABLE | object::attribute::CONFIGURABLE,
            ),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        object::define_own_property(
            self.heap,
            handle,
            name,
            object::Descriptor::data(Value::object(prototype), object::attribute::WRITABLE),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(())
    }

    /// Give a function its `length` and `name` the first time either is read.
    ///
    /// The length is the declared parameter count, straight from the function
    /// record; the name of a function made of bytecode is the empty string
    /// until the image carries one.
    pub(super) fn materialise_function_facts(
        &mut self,
        target: Value,
        key: Key,
    ) -> Result<(), Completion> {
        if !target.is_object() {
            return Ok(());
        }
        let handle = target.as_handle();
        if object::is_callable(self.heap, handle) != Ok(true) {
            return Ok(());
        }
        let native = object::is_native(self.heap, handle).unwrap_or(true);
        let length_key = self.ascii_key(b"length")?;
        let name_key = self.ascii_key(b"name")?;
        if key != length_key && key != name_key {
            return Ok(());
        }
        // Once made — or deleted, which asks for it first — a fact is never
        // made again: a deleted `name` stays deleted.
        let mark = if key == length_key {
            object::function_flag::MEASURED
        } else {
            object::function_flag::NAMED
        };
        let flags = object::function_flags(self.heap, handle).unwrap_or(0);
        if flags & mark != 0 {
            return Ok(());
        }
        let _ = object::add_function_flags(self.heap, handle, mark);
        if object::get_own_property(self.heap, handle, key)
            .unwrap_or(None)
            .is_some()
        {
            return Ok(());
        }
        let value = if key == length_key {
            let count = if native {
                let id = object::function_code(self.heap, handle).unwrap_or(0);
                crate::realm::native::arity(id)
            } else {
                let code = object::function_code(self.heap, handle).unwrap_or(0);
                let module = object::function_module(self.heap, handle).unwrap_or(0);
                self.unit_of(module)
                    .function(code)
                    .map_or(0, |record| record.argument_count)
            };
            Value::number(crate::softfloat::from_u64(u64::from(count)))
        } else {
            self.ascii_string(b"")?
        };
        object::define_own_property(
            self.heap,
            handle,
            key,
            object::Descriptor::data(value, object::attribute::CONFIGURABLE),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(())
    }

    /// Give a bound function the `length` and `name` its target implies.
    ///
    /// The length is what is left of the target's parameters once the bound
    /// arguments are counted off, never below zero and never from a target
    /// whose `length` is not a number. The name is the target's, behind
    /// `bound `.
    pub(super) fn name_bound_function(
        &mut self,
        bound: Handle,
        target: Value,
        bound_count: u32,
    ) -> Result<(), Completion> {
        let length_key = self.ascii_key(b"length")?;
        let declared = self.get_property(target, length_key)?;
        let remaining = if declared.is_number() {
            // `+Infinity` stays infinite however much is bound; anything else
            // is the declared count less the bound arguments, floored at zero.
            let count = crate::value::to_integer_or_infinity(declared.as_number());
            if count == f64::INFINITY {
                count
            } else {
                let bound_arguments = crate::softfloat::from_u64(u64::from(bound_count));
                let left = crate::softfloat::sub(count, bound_arguments);
                if crate::softfloat::compare(left, 0.0) > 0 {
                    left
                } else {
                    0.0
                }
            }
        } else {
            0.0
        };
        object::define_own_property(
            self.heap,
            bound,
            length_key,
            object::Descriptor::data(Value::number(remaining), object::attribute::CONFIGURABLE),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;

        let name_key = self.ascii_key(b"name")?;
        let declared = self.get_property(target, name_key)?;
        let prefix = self.ascii_string(b"bound ")?;
        let name = if declared.is_string() {
            let joined = string::concat(self.heap, prefix.as_handle(), declared.as_handle())
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            Value::string(joined)
        } else {
            prefix
        };
        object::define_own_property(
            self.heap,
            bound,
            name_key,
            object::Descriptor::data(name, object::attribute::CONFIGURABLE),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(())
    }

    /// Read a property from a primitive's prototype, with the primitive as the
    /// receiver, so a method sees the value it was called on rather than a
    /// wrapper made for the occasion.
    pub(super) fn prototype_property(
        &mut self,
        prototype: Handle,
        receiver: Value,
        key: Key,
    ) -> Result<Value, Completion> {
        let lookup = object::get(self.heap, prototype, key).map_err(|_| Completion::MALFORMED)?;
        match lookup {
            Lookup::Absent => Ok(Value::UNDEFINED),
            Lookup::Value(value) => Ok(value),
            Lookup::Accessor(getter) => {
                if getter.is_undefined() {
                    return Ok(Value::UNDEFINED);
                }
                self.call_value(getter, receiver, &[])
            }
        }
    }

    /// A string's own properties: its length and its indexed code units.
    pub(super) fn string_property(
        &mut self,
        target: Value,
        key: Key,
    ) -> Result<Option<Value>, Completion> {
        let handle = target.as_handle();
        let length = string::length(self.heap, handle).map_err(|_| Completion::MALFORMED)?;
        match key {
            Key::Index(index) => {
                let unit =
                    string::unit_at(self.heap, handle, index).map_err(|_| Completion::MALFORMED)?;
                match unit {
                    Some(unit) => Ok(Some(self.make_string(&[unit])?)),
                    None => Ok(None),
                }
            }
            Key::Name(name) => {
                let expected = self.ascii_key(b"length")?;
                if Key::Name(name) == expected {
                    return Ok(Some(Value::number(crate::softfloat::from_u64(u64::from(
                        length,
                    )))));
                }
                Ok(None)
            }
            Key::Symbol(_) => Ok(None),
        }
    }

    /// Store through `object::set`, collecting and retrying when the heap is
    /// full: a table that outgrew its copies leaves them as garbage, and a
    /// failure counts only when the live data truly does not fit.
    pub(super) fn set_with_room(
        &mut self,
        target: Value,
        key: Key,
        value: Value,
    ) -> Result<Assignment, Completion> {
        match object::set(self.heap, target.as_handle(), key, value) {
            Ok(outcome) => Ok(outcome),
            Err(object::ObjectError::Heap(
                crate::heap::HeapError::ArenaFull | crate::heap::HeapError::SlotsFull,
            )) => {
                if let Some(stop) = self.collect_now() {
                    return Err(stop);
                }
                object::set(self.heap, target.as_handle(), key, value).map_err(Self::object_failure)
            }
            Err(error) => Err(Self::object_failure(error)),
        }
    }

    pub(super) fn set_property(
        &mut self,
        target: Value,
        key: Key,
        value: Value,
    ) -> Result<(), Completion> {
        self.set_property_of(target, key, value, false)
    }

    /// Assign a property, with strict code's refusals: a write a receiver
    /// refuses — non-writable, setter-less, non-extensible, or a primitive —
    /// is a TypeError under strict code and silence outside it.
    pub(super) fn set_property_of(
        &mut self,
        target: Value,
        key: Key,
        value: Value,
        strict: bool,
    ) -> Result<(), Completion> {
        if target.is_nullish() {
            return Err(self.throw_type_error());
        }
        // A mapped arguments index writes its parameter's slot.
        if let (Key::Index(index), true) = (key, target.is_object()) {
            if let Ok(Some((environment, mapped))) =
                object::arguments_map(self.heap, target.as_handle())
            {
                if index < 32 && mapped & (1 << index) != 0 && environment.is_object() {
                    let _ = env::set_slot(self.heap, environment.as_handle(), index, value);
                    return Ok(());
                }
            }
        }
        if !target.is_object() {
            // A write to a primitive walks its prototype chain first: a
            // setter or a proxy there sees the write, with the primitive as
            // receiver; anything else discards it outside strict mode.
            if self.primitive_write(target, key, value)? {
                return Ok(());
            }
            if strict {
                return Err(self.throw_type_error());
            }
            return Ok(());
        }
        match object::exotic_kind(self.heap, target.as_handle()).unwrap_or(0) {
            object::exotic::PROXY if !self.hidden_key(key) => {
                let done = self.proxy_set(target, key, value)?;
                if !done && strict {
                    return Err(self.throw_type_error());
                }
                return Ok(());
            }
            object::exotic::NAMESPACE | object::exotic::DEFERRED => {
                // A namespace takes no write; looking first surfaces the
                // ReferenceError a binding still in its dead zone throws.
                // A write is no meaningful use of a deferred namespace, so
                // an unevaluated module stays that way.
                if self.deferred_done(target) {
                    self.namespace_touch(target, key)?;
                }
                if strict {
                    return Err(self.throw_type_error());
                }
                return Ok(());
            }
            object::exotic::TYPED_ARRAY => {
                if let Key::Index(index) = key {
                    return self.typed_array_write(target, index, value);
                }
            }
            _ => {}
        }
        // A typed array on the chain swallows a write under a numeric name
        // that is not one of its indices: TypedArray [[Set]] answers true
        // and stores nothing, so nothing lands on the receiver either.
        if self.is_canonical_numeric_key(key)? {
            let mut holder = object::prototype(self.heap, target.as_handle())
                .map_err(|_| Completion::MALFORMED)?;
            let mut depth = 0u32;
            while holder.is_object() && depth < object::MAX_PROTOTYPE_DEPTH {
                if object::exotic_kind(self.heap, holder.as_handle()).unwrap_or(0)
                    == object::exotic::TYPED_ARRAY
                {
                    return Ok(());
                }
                holder = object::prototype(self.heap, holder.as_handle())
                    .map_err(|_| Completion::MALFORMED)?;
                depth += 1;
            }
        }
        // A function's `length` and `name` exist before a write can miss
        // them, so the write meets the read-only property they are.
        self.materialise_function_facts(target, key)?;
        // Assigning an array's `length` drops what is now past the end.
        if let Key::Name(_) = key {
            if self.is_array(target)? {
                let length_key = self.ascii_key(b"length")?;
                if key == length_key {
                    let old = self.length_of(target)?;
                    let wanted = self.coerce_to_number(value)?;
                    let new = value::to_uint32(wanted);
                    let outcome = self.set_with_room(target, key, value)?;
                    if matches!(outcome, Assignment::Done) {
                        let mut index = new;
                        while index < old {
                            self.delete_element(target, index)?;
                            index += 1;
                        }
                    }
                    return Ok(());
                }
            }
        }
        let outcome = self.set_with_room(target, key, value)?;
        match outcome {
            Assignment::Done | Assignment::Refused => {
                if strict && matches!(outcome, Assignment::Refused) {
                    return Err(self.throw_type_error());
                }
                // An array's `length` follows its highest index: a store past
                // the end grows it.
                if let Key::Index(index) = key {
                    if matches!(outcome, Assignment::Done) && self.is_array(target)? {
                        let length = self.length_of(target)?;
                        if index >= length && index < u32::MAX {
                            self.set_length(target, index + 1)?;
                        }
                    }
                }
                Ok(())
            }
            Assignment::Setter(setter) => {
                let _ = self.call_value(Value::object(setter), target, &[value])?;
                Ok(())
            }
        }
    }

    /// Define one half of an accessor, keeping the other half an existing
    /// accessor already carries.
    pub(super) fn define_accessor(
        &mut self,
        target: Value,
        key: Key,
        closure: Value,
        getter: bool,
        attributes: u8,
    ) -> Result<(), Completion> {
        if !target.is_object() {
            return Err(self.throw_type_error());
        }
        let handle = target.as_handle();
        let existing = object::get_own_property(self.heap, handle, key)
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let (mut get, mut set) = match existing {
            Some(found) if matches!(found.kind, object::DescriptorKind::Accessor) => {
                (found.getter, found.setter)
            }
            _ => (Value::UNDEFINED, Value::UNDEFINED),
        };
        if getter {
            get = closure;
        } else {
            set = closure;
        }
        // The accessor's function is named for the key, behind `get ` or
        // `set `: what named evaluation gives an accessor.
        if closure.is_object() && !object::is_native(self.heap, closure.as_handle()).unwrap_or(true)
        {
            let prefix: &[u8] = if getter { b"get " } else { b"set " };
            let name = self.name_for_key(key, Some(prefix))?;
            let name_key = self.ascii_key(b"name")?;
            object::define_own_property(
                self.heap,
                closure.as_handle(),
                name_key,
                object::Descriptor::data(name, attribute::CONFIGURABLE),
            )
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        let admitted = object::define_own_property(
            self.heap,
            handle,
            key,
            object::Descriptor::accessor(get, set, attributes),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        if !admitted {
            // An accessor over a property that refuses redefinition —
            // `static ['prototype']`, most of all — is a TypeError.
            return Err(self.throw_type_error());
        }
        Ok(())
    }

    /// Define an `accessor` field's face: a getter and a setter over the
    /// hidden name the field stores behind, `NUL acc ` and the key's text,
    /// which reflection never reports.
    pub(super) fn define_auto_accessor(
        &mut self,
        target: Value,
        key: Key,
    ) -> Result<(), Completion> {
        if !target.is_object() {
            return Err(self.throw_type_error());
        }
        let name = self.key_to_value(key)?;
        if !name.is_string() {
            return Err(self.throw_type_error());
        }
        let prefix_units = [
            0u16,
            u16::from(b'a'),
            u16::from(b'c'),
            u16::from(b'c'),
            u16::from(b' '),
        ];
        let prefix = self
            .atoms
            .intern(self.heap, &prefix_units)
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let backing = string::concat(self.heap, prefix, name.as_handle())
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let backing = Value::string(backing);
        let mut pair = [Value::UNDEFINED; 2];
        for (slot, id) in pair
            .iter_mut()
            .zip([native::ACCESSOR_GET, native::ACCESSOR_SET])
        {
            let function = object::create_native(
                self.heap,
                Value::object(self.realm.function_prototype),
                id,
                0,
            )
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            object::set_bound_value(self.heap, function, backing)
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            *slot = Value::object(function);
        }
        object::define_own_property(
            self.heap,
            target.as_handle(),
            key,
            object::Descriptor::accessor(pair[0], pair[1], attribute::CONFIGURABLE),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(())
    }

    /// Name a closure for the key it is defined under, when it carries no
    /// name yet: named evaluation through a computed key or a method.
    pub(super) fn name_closure_for(&mut self, closure: Value, key: Key) -> Result<(), Completion> {
        if !closure.is_object() {
            return Ok(());
        }
        let handle = closure.as_handle();
        if object::is_native(self.heap, handle).unwrap_or(true) {
            return Ok(());
        }
        let name_key = self.ascii_key(b"name")?;
        let flags = object::function_flags(self.heap, handle).unwrap_or(0);
        if flags & object::function_flag::NAMED != 0
            || object::get_own_property(self.heap, handle, name_key)
                .unwrap_or(None)
                .is_some()
        {
            return Ok(());
        }
        let name = self.name_for_key(key, None)?;
        object::define_own_property(
            self.heap,
            handle,
            name_key,
            object::Descriptor::data(name, attribute::CONFIGURABLE),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let _ = object::add_function_flags(self.heap, handle, object::function_flag::NAMED);
        Ok(())
    }

    /// The `name` a function takes from a property key: the key's text, a
    /// symbol as `[description]` — or nothing for one without — behind an
    /// optional prefix such as `get `.
    pub(super) fn name_for_key(
        &mut self,
        key: Key,
        prefix: Option<&[u8]>,
    ) -> Result<Value, Completion> {
        let body = match key {
            Key::Symbol(handle) => {
                let length = string::length(self.heap, handle).unwrap_or(0);
                if length == 0 {
                    self.ascii_string(b"")?
                } else {
                    let open = self.ascii_string(b"[")?;
                    let close = self.ascii_string(b"]")?;
                    let inner = string::concat(self.heap, open.as_handle(), handle)
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                    let whole = string::concat(self.heap, inner, close.as_handle())
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                    Value::string(whole)
                }
            }
            _ => self.key_to_value(key)?,
        };
        let Some(prefix) = prefix else {
            return Ok(body);
        };
        let head = self.ascii_string(prefix)?;
        let whole = string::concat(self.heap, head.as_handle(), body.as_handle())
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(Value::string(whole))
    }

    /// The value a mapped arguments index reports: its parameter's slot,
    /// where the object maps that index.
    pub(super) fn mapped_argument(&mut self, target: Value, key: Key) -> Option<Value> {
        let Key::Index(index) = key else {
            return None;
        };
        if !target.is_object() {
            return None;
        }
        let (environment, mapped) = object::arguments_map(self.heap, target.as_handle()).ok()??;
        if index >= 32 || mapped & (1 << index) == 0 || !environment.is_object() {
            return None;
        }
        env::slot_value(self.heap, environment.as_handle(), index).ok()
    }

    pub(super) fn define_property(
        &mut self,
        target: Value,
        key: Key,
        value: Value,
    ) -> Result<(), Completion> {
        if !target.is_object() {
            return Err(self.throw_type_error());
        }
        if object::exotic_kind(self.heap, target.as_handle()).unwrap_or(0) == object::exotic::PROXY
            && !self.hidden_key(key)
        {
            if !self.proxy_define(target, key, value, attribute::DEFAULT)? {
                return Err(self.throw_type_error());
            }
            return Ok(());
        }
        let descriptor = Descriptor::data(value, attribute::DEFAULT);
        match object::define_own_property(self.heap, target.as_handle(), key, descriptor) {
            Ok(true) => Ok(()),
            // CreateDataPropertyOrThrow: a property that cannot take the
            // definition — `prototype` on a class constructor, most of all —
            // is a TypeError.
            Ok(false) => Err(self.throw_type_error()),
            Err(object::ObjectError::Heap(
                crate::heap::HeapError::ArenaFull | crate::heap::HeapError::SlotsFull,
            )) => {
                // The arena may hold reclaimable garbage the pressure check
                // did not see; a failure counts only after a collection.
                if let Some(stop) = self.collect_now() {
                    return Err(stop);
                }
                object::define_own_property(self.heap, target.as_handle(), key, descriptor)
                    .map(|_| ())
                    .map_err(Self::object_failure)
            }
            Err(error) => Err(Self::object_failure(error)),
        }
    }

    pub(super) fn delete_property(&mut self, target: Value, key: Key) -> Result<bool, Completion> {
        // A reference through nothing is a type error, exactly as a read
        // through it is; a primitive base holds nothing deletable and
        // answers true.
        if target.is_nullish() {
            return Err(self.throw_type_error());
        }
        if !target.is_object() {
            return Ok(true);
        }
        self.materialise_function_facts(target, key)?;
        if object::exotic_kind(self.heap, target.as_handle()).unwrap_or(0) == object::exotic::PROXY
            && !self.hidden_key(key)
        {
            return self.proxy_delete(target, key);
        }
        if !self.hidden_key(key) {
            self.deferred_trigger(target, Some(key))?;
        }
        let removed =
            object::delete(self.heap, target.as_handle(), key).map_err(Self::object_failure)?;
        // Deleting a mapped arguments index breaks the aliasing for good —
        // but only a delete that succeeded: a non-configurable index keeps
        // its mapping.
        if removed {
            if let Key::Index(index) = key {
                let _ = object::unmap_argument(self.heap, target.as_handle(), index);
            }
        }
        Ok(removed)
    }

    pub(super) fn copy_data_properties(
        &mut self,
        target: Value,
        source: Value,
    ) -> Result<(), Completion> {
        self.copy_data_properties_excluding(target, source, Value::UNDEFINED)
    }

    /// CopyDataProperties: the source's own enumerable properties, in own-key
    /// order, defined on the target — except the keys `excluded` holds as
    /// own properties, which are never asked about on the source. A proxy
    /// source answers through its `ownKeys`, `getOwnPropertyDescriptor`,
    /// and `get` traps, in that order for each key.
    pub(super) fn copy_data_properties_excluding(
        &mut self,
        target: Value,
        source: Value,
        excluded: Value,
    ) -> Result<(), Completion> {
        if !target.is_object() {
            return Ok(());
        }
        // A string's own enumerable properties are its indexed characters:
        // what ToObject would expose, without making the wrapper.
        if source.is_string() {
            let length = crate::string::length(self.heap, source.as_handle()).unwrap_or(0);
            let mut index = 0u32;
            while index < length {
                let key = Key::Index(index);
                index += 1;
                if self.is_excluded(excluded, key)? {
                    continue;
                }
                let value = self.get_property(source, key)?;
                self.define_property(target, key, value)?;
            }
            return Ok(());
        }
        if !source.is_object() {
            return Ok(());
        }
        let proxy = object::exotic_kind(self.heap, source.as_handle()).unwrap_or(0)
            == object::exotic::PROXY;
        let mut keys = [Key::Index(0); MAX_COPIED_KEYS];
        let written = if proxy {
            self.proxy_own_keys(source, &mut keys)?
        } else {
            object::own_keys(self.heap, source.as_handle(), &mut keys).map_err(Self::key_failure)?
        };
        let mut index = 0usize;
        while index < written {
            let key = keys[index];
            index += 1;
            if self.is_excluded(excluded, key)? {
                continue;
            }
            // Only the enumerable own properties cross: a spread copies what
            // enumeration would see.
            let enumerable = if proxy {
                self.proxy_own_enumerable(source, key)?
            } else {
                self.is_enumerable(source, key)?
            };
            if !enumerable {
                continue;
            }
            let value = self.get_property(source, key)?;
            self.define_property(target, key, value)?;
        }
        Ok(())
    }

    /// OrdinarySet with a receiver of its own: the property is found on the
    /// target's chain, and a data property lands on the receiver — through
    /// its own record, or its traps where it is a proxy. Answers whether
    /// the write was taken.
    pub(super) fn set_with_receiver(
        &mut self,
        target: Value,
        key: Key,
        value: Value,
        receiver: Value,
    ) -> Result<bool, Completion> {
        let mut holder = target;
        let found = loop {
            if !holder.is_object() {
                break None;
            }
            if object::exotic_kind(self.heap, holder.as_handle()).unwrap_or(0)
                == object::exotic::PROXY
            {
                let (proxy_target, handler) = self.proxy_parts(holder)?;
                let trap = self.proxy_trap(handler, b"set")?;
                if trap.is_undefined() {
                    holder = proxy_target;
                    continue;
                }
                let name = self.key_to_value(key)?;
                let answer =
                    self.call_value(trap, handler, &[proxy_target, name, value, receiver])?;
                return self.coerce_to_boolean(answer);
            }
            let own = object::get_own_property(self.heap, holder.as_handle(), key)
                .map_err(|_| Completion::MALFORMED)?;
            if own.is_some() {
                break own;
            }
            holder = object::prototype(self.heap, holder.as_handle())
                .map_err(|_| Completion::MALFORMED)?;
        };
        if let Some(descriptor) = found {
            if matches!(descriptor.kind, object::DescriptorKind::Accessor) {
                if !self.is_callable_value(descriptor.setter) {
                    return Ok(false);
                }
                self.call_value(descriptor.setter, receiver, &[value])?;
                return Ok(true);
            }
            if !descriptor.has(attribute::WRITABLE) {
                return Ok(false);
            }
        }
        if !receiver.is_object() {
            return Ok(false);
        }
        if object::exotic_kind(self.heap, receiver.as_handle()).unwrap_or(0)
            == object::exotic::PROXY
        {
            let existing = self.proxy_own_descriptor(receiver, key)?;
            let attributes = if existing.is_undefined() {
                attribute::DEFAULT
            } else {
                let mut attributes = 0u8;
                for (name, bit) in [
                    (&b"writable"[..], attribute::WRITABLE),
                    (&b"enumerable"[..], attribute::ENUMERABLE),
                    (&b"configurable"[..], attribute::CONFIGURABLE),
                ] {
                    let field = self.ascii_key(name)?;
                    let flag = self.get_property(existing, field)?;
                    if self.coerce_to_boolean(flag)? {
                        attributes |= bit;
                    }
                }
                let has_accessor = {
                    let get_key = self.ascii_key(b"get")?;
                    let set_key = self.ascii_key(b"set")?;
                    self.has_property_of(existing, get_key)?
                        || self.has_property_of(existing, set_key)?
                };
                if has_accessor || attributes & attribute::WRITABLE == 0 {
                    return Ok(false);
                }
                attributes
            };
            return self.proxy_define(receiver, key, value, attributes);
        }
        // Defining on a deferred namespace is a meaningful use of it — the
        // definition itself is refused, but the module runs.
        if object::exotic_kind(self.heap, receiver.as_handle()).unwrap_or(0)
            == object::exotic::DEFERRED
            && !self.hidden_key(key)
        {
            self.deferred_trigger(receiver, Some(key))?;
        }
        let existing = object::get_own_property(self.heap, receiver.as_handle(), key)
            .map_err(|_| Completion::MALFORMED)?;
        let attributes = match existing {
            Some(descriptor) => {
                if matches!(descriptor.kind, object::DescriptorKind::Accessor)
                    || !descriptor.has(attribute::WRITABLE)
                {
                    return Ok(false);
                }
                descriptor.attributes
            }
            None => {
                // A new property needs an extensible receiver.
                let extensible = object::is_extensible(self.heap, receiver.as_handle())
                    .map_err(|_| Completion::MALFORMED)?;
                if !extensible {
                    return Ok(false);
                }
                attribute::DEFAULT
            }
        };
        object::define_own_property(
            self.heap,
            receiver.as_handle(),
            key,
            Descriptor::data(value, attributes),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(true)
    }

    /// A proxy's own property descriptor for a key as the trap reports it:
    /// the descriptor object, or undefined.
    pub(super) fn proxy_own_descriptor(
        &mut self,
        proxy: Value,
        key: Key,
    ) -> Result<Value, Completion> {
        let (target, handler) = self.proxy_parts(proxy)?;
        let trap = self.proxy_trap(handler, b"getOwnPropertyDescriptor")?;
        if trap.is_undefined() {
            let own = object::get_own_property(self.heap, target.as_handle(), key)
                .map_err(|_| Completion::MALFORMED)?;
            return match own {
                Some(descriptor) => self.descriptor_object(descriptor),
                None => Ok(Value::UNDEFINED),
            };
        }
        let name = self.key_to_value(key)?;
        let descriptor = self.call_value(trap, handler, &[target, name])?;
        if !descriptor.is_undefined() && !descriptor.is_object() {
            return Err(self.throw_type_error());
        }
        Ok(descriptor)
    }

    /// A property descriptor as the object reflection hands out.
    pub(super) fn descriptor_object(&mut self, found: Descriptor) -> Result<Value, Completion> {
        let result = object::create(self.heap, Value::object(self.realm.object_prototype))
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let result = Value::object(result);
        let fields: [(&[u8], Value); 4] = if matches!(found.kind, object::DescriptorKind::Data) {
            [
                (b"value", found.value),
                (b"writable", Value::boolean(found.has(attribute::WRITABLE))),
                (
                    b"enumerable",
                    Value::boolean(found.has(attribute::ENUMERABLE)),
                ),
                (
                    b"configurable",
                    Value::boolean(found.has(attribute::CONFIGURABLE)),
                ),
            ]
        } else {
            [
                (b"get", found.getter),
                (b"set", found.setter),
                (
                    b"enumerable",
                    Value::boolean(found.has(attribute::ENUMERABLE)),
                ),
                (
                    b"configurable",
                    Value::boolean(found.has(attribute::CONFIGURABLE)),
                ),
            ]
        };
        for (name, value) in fields {
            let key = self.ascii_key(name)?;
            self.define_property(result, key, value)?;
        }
        Ok(result)
    }

    /// Whether a key is one the exclusion object names.
    pub(super) fn is_excluded(&mut self, excluded: Value, key: Key) -> Result<bool, Completion> {
        if !excluded.is_object() {
            return Ok(false);
        }
        let held = object::get_own_property(self.heap, excluded.as_handle(), key)
            .map_err(|_| Completion::MALFORMED)?;
        Ok(held.is_some())
    }

    // Arrays.

    pub(super) fn create_array(&mut self) -> Result<Value, Completion> {
        let array = object::create(self.heap, Value::object(self.realm.array_prototype))
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let length_key = self.ascii_key(b"length")?;
        object::define_own_property(
            self.heap,
            array,
            length_key,
            Descriptor::data(Value::number(0.0), attribute::WRITABLE),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(Value::object(array))
    }

    pub(super) fn append_element(
        &mut self,
        array: Value,
        value: Option<Value>,
    ) -> Result<(), Completion> {
        if !array.is_object() {
            return Err(self.throw_type_error());
        }
        let length_key = self.ascii_key(b"length")?;
        let length = self.get_property(array, length_key)?;
        let index = value::to_uint32(length.as_number());
        if let Some(value) = value {
            self.define_property(array, Key::Index(index), value)?;
        }
        let next = Value::number(crate::softfloat::from_u64(u64::from(index) + 1));
        object::define_own_property(
            self.heap,
            array.as_handle(),
            length_key,
            Descriptor::data(next, attribute::WRITABLE),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(())
    }

    /// Read `key` starting at the super base, running any getter on the
    /// frame's own `this` rather than on the base.
    pub(super) fn super_get(
        &mut self,
        base: Value,
        key: Key,
        receiver: Value,
    ) -> Result<Value, Completion> {
        let mut holder = base;
        let mut depth = 0u32;
        while holder.is_object() && depth <= object::MAX_PROTOTYPE_DEPTH {
            if self.deferred_live
                && !self.hidden_key(key)
                && object::exotic_kind(self.heap, holder.as_handle()).unwrap_or(0)
                    == object::exotic::DEFERRED
            {
                self.deferred_trigger(holder, Some(key))?;
            }
            let found = object::get_own_property(self.heap, holder.as_handle(), key)
                .map_err(|_| Completion::MALFORMED)?;
            if let Some(descriptor) = found {
                return match descriptor.kind {
                    object::DescriptorKind::Data => Ok(descriptor.value),
                    object::DescriptorKind::Accessor => {
                        if self.is_callable_value(descriptor.getter) {
                            self.call_value(descriptor.getter, receiver, &[])
                        } else {
                            Ok(Value::UNDEFINED)
                        }
                    }
                };
            }
            holder = object::prototype(self.heap, holder.as_handle())
                .map_err(|_| Completion::MALFORMED)?;
            depth += 1;
        }
        Ok(Value::UNDEFINED)
    }

    /// Write through a super reference: the base chain decides — a setter
    /// runs with the receiver, a read-only property refuses — and otherwise
    /// the value lands as the receiver's own data property.
    pub(super) fn super_set(
        &mut self,
        base: Value,
        key: Key,
        value: Value,
        receiver: Value,
        strict: bool,
    ) -> Result<(), Completion> {
        let mut holder = base;
        let mut depth = 0u32;
        let mut refused = false;
        while holder.is_object() && depth <= object::MAX_PROTOTYPE_DEPTH {
            let found = object::get_own_property(self.heap, holder.as_handle(), key)
                .map_err(|_| Completion::MALFORMED)?;
            if let Some(descriptor) = found {
                match descriptor.kind {
                    object::DescriptorKind::Accessor => {
                        if self.is_callable_value(descriptor.setter) {
                            self.call_value(descriptor.setter, receiver, &[value])?;
                            return Ok(());
                        }
                        refused = true;
                    }
                    object::DescriptorKind::Data => {
                        refused = !descriptor.has(attribute::WRITABLE);
                    }
                }
                break;
            }
            holder = object::prototype(self.heap, holder.as_handle())
                .map_err(|_| Completion::MALFORMED)?;
            depth += 1;
        }
        if !refused {
            // A namespace receiver reads the binding first, so a name still
            // in its dead zone throws before the write is refused.
            self.namespace_touch(receiver, key)?;
            if receiver.is_object() {
                let own = object::get_own_property(self.heap, receiver.as_handle(), key)
                    .map_err(|_| Completion::MALFORMED)?;
                let admitted = match own {
                    Some(existing) => {
                        if matches!(existing.kind, object::DescriptorKind::Accessor)
                            || !existing.has(attribute::WRITABLE)
                        {
                            false
                        } else {
                            object::define_own_property(
                                self.heap,
                                receiver.as_handle(),
                                key,
                                Descriptor::data(value, existing.attributes),
                            )
                            .map_err(|_| Completion::HEAP_EXHAUSTED)?
                        }
                    }
                    None => object::define_own_property(
                        self.heap,
                        receiver.as_handle(),
                        key,
                        Descriptor::data(value, attribute::DEFAULT),
                    )
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?,
                };
                if admitted {
                    return Ok(());
                }
            }
            refused = true;
        }
        if refused && strict {
            return Err(self.throw_type_error());
        }
        Ok(())
    }

    // The library the engine implements itself. Everything here is a pure
    // function of its arguments and the heap: nothing reaches outside the
    // isolate, which is why there is no `Math.random` and no clock.

    /// The length an array-like says it has.
    pub(super) fn length_of(&mut self, target: Value) -> Result<u32, Completion> {
        let key = self.ascii_key(b"length")?;
        let value = self.get_property(target, key)?;
        let number = self.coerce_to_number(value)?;
        Ok(value::to_uint32(number))
    }

    pub(super) fn set_length(&mut self, target: Value, length: u32) -> Result<(), Completion> {
        let key = self.ascii_key(b"length")?;
        let value = Value::number(crate::softfloat::from_u64(u64::from(length)));
        self.set_property(target, key, value)
    }

    pub(super) fn element(&mut self, target: Value, index: u32) -> Result<Value, Completion> {
        self.get_property(target, Key::Index(index))
    }

    pub(super) fn set_element(
        &mut self,
        target: Value,
        index: u32,
        value: Value,
    ) -> Result<(), Completion> {
        self.define_property(target, Key::Index(index), value)
    }

    /// A new array holding nothing.
    pub(super) fn new_array(&mut self) -> Result<Value, Completion> {
        self.create_array()
    }

    /// The integer an argument denotes, clamped into `[0, length]`, with a
    /// negative value counted back from the end. This is what every method that
    /// takes a range does with its arguments.
    pub(super) fn relative_index(
        &mut self,
        value: Value,
        length: u32,
        default: u32,
    ) -> Result<u32, Completion> {
        if value.is_undefined() {
            return Ok(default);
        }
        let number = self.coerce_to_number(value)?;
        if number.is_nan() {
            return Ok(0);
        }
        let length = f64::from(length);
        let index = value::truncate(number);
        let index = if index < 0.0 { length + index } else { index };
        let index = if index < 0.0 {
            0.0
        } else if index > length {
            length
        } else {
            index
        };
        Ok(value::to_uint32(index))
    }

    /// Whether an own property is enumerable, which is what the key lists ask.
    pub(super) fn is_enumerable(&mut self, object: Value, key: Key) -> Result<bool, Completion> {
        if !object.is_object() {
            return Ok(false);
        }
        let descriptor = object::get_own_property(self.heap, object.as_handle(), key)
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(descriptor.is_some_and(|descriptor| descriptor.has(attribute::ENUMERABLE)))
    }

    /// The own enumerable keys, values, or key/value pairs of an object.
    pub(super) fn own_entries(&mut self, target: Value, kind: u32) -> Result<Value, Completion> {
        let object = self.coerce_to_object(target)?;
        // A function's `length` and `name` are own properties whether or
        // not anything has read them yet.
        let length_key = self.ascii_key(b"length")?;
        let name_key = self.ascii_key(b"name")?;
        self.materialise_function_facts(object, length_key)?;
        self.materialise_function_facts(object, name_key)?;
        let result = self.new_array()?;
        // A typed array's own keys begin with its indices.
        if object::exotic_kind(self.heap, object.as_handle()).unwrap_or(0)
            == object::exotic::TYPED_ARRAY
            && kind != native::OBJECT_GET_OWN_PROPERTY_SYMBOLS
        {
            let count = self.typed_array_length(object)?.unwrap_or(0);
            let mut index = 0u32;
            while index < count {
                let name = self.key_to_value(Key::Index(index))?;
                let entry = match kind {
                    native::OBJECT_VALUES => self.typed_array_read(object, index)?,
                    native::OBJECT_ENTRIES => {
                        let pair = self.new_array()?;
                        let value = self.typed_array_read(object, index)?;
                        self.set_element(pair, 0, name)?;
                        self.set_element(pair, 1, value)?;
                        self.set_length(pair, 2)?;
                        pair
                    }
                    _ => name,
                };
                self.append_element(result, Some(entry))?;
                index += 1;
            }
        }
        // Listing a deferred namespace's keys is a meaningful use.
        self.deferred_trigger(object, None)?;
        let mut keys = [Key::Index(0); MAX_OWN_KEYS];
        let count = object::own_keys(self.heap, object.as_handle(), &mut keys)
            .map_err(Self::key_failure)?;
        let mut written = self.length_of(result)?;
        let symbols = kind == native::OBJECT_GET_OWN_PROPERTY_SYMBOLS;
        // Reflect.ownKeys answers every own key, names before symbols.
        let everything = kind == native::REFLECT_OWN_KEYS;
        let phases: u32 = if everything { 2 } else { 1 };
        let mut phase = 0u32;
        while phase < phases {
            for &key in keys.get(..count).unwrap_or(&[]) {
                let symbol = matches!(key, Key::Symbol(_));
                let wanted = if everything { phase == 1 } else { symbols };
                if symbol != wanted || self.hidden_key(key) {
                    continue;
                }
                // Listing keys reads no binding: a name still in its dead
                // zone lists fine, and only reading its value throws.
                if kind != native::OBJECT_GET_OWN_PROPERTY_NAMES
                    && kind != native::OBJECT_GET_OWN_PROPERTY_SYMBOLS
                    && kind != native::REFLECT_OWN_KEYS
                {
                    self.namespace_touch(object, key)?;
                }
                if !symbols
                    && !everything
                    && kind != native::OBJECT_GET_OWN_PROPERTY_NAMES
                    && !self.is_enumerable(object, key)?
                {
                    continue;
                }
                let name = self.key_to_value(key)?;
                let entry = match kind {
                    native::OBJECT_KEYS
                    | native::OBJECT_GET_OWN_PROPERTY_NAMES
                    | native::OBJECT_GET_OWN_PROPERTY_SYMBOLS
                    | native::REFLECT_OWN_KEYS => name,
                    native::OBJECT_VALUES => self.get_property(object, key)?,
                    _ => {
                        let pair = self.new_array()?;
                        let value = self.get_property(object, key)?;
                        self.set_element(pair, 0, name)?;
                        self.set_element(pair, 1, value)?;
                        self.set_length(pair, 2)?;
                        pair
                    }
                };
                self.set_element(result, written, entry)?;
                written += 1;
            }
            phase += 1;
        }
        self.set_length(result, written)?;
        Ok(result)
    }

    /// A property key as the value a program sees.
    pub(super) fn key_to_value(&mut self, key: Key) -> Result<Value, Completion> {
        match key {
            Key::Index(index) => {
                let number = Value::number(crate::softfloat::from_u64(u64::from(index)));
                self.coerce_to_string(number)
            }
            Key::Name(handle) => Ok(Value::string(handle)),
            Key::Symbol(handle) => Ok(Value::symbol(handle)),
        }
    }

    pub(super) fn is_array(&mut self, value: Value) -> Result<bool, Completion> {
        if !value.is_object() {
            return Ok(false);
        }
        // Array.prototype anywhere up the chain: the array itself, or an
        // instance of a class extending Array.
        let mut current = value;
        let mut depth = 0u32;
        while current.is_object() && depth < 8 {
            let prototype = object::prototype(self.heap, current.as_handle())
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            if prototype.is_object() && prototype.as_handle() == self.realm.array_prototype {
                return Ok(true);
            }
            current = prototype;
            depth += 1;
        }
        Ok(false)
    }

    pub(super) fn delete_element(&mut self, target: Value, index: u32) -> Result<(), Completion> {
        if target.is_object() {
            object::delete(self.heap, target.as_handle(), Key::Index(index))
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        Ok(())
    }

    /// The array methods that call a function once per element.
    pub(super) fn array_walk(
        &mut self,
        id: u32,
        this: Value,
        callback: Value,
        receiver: Value,
    ) -> Result<Value, Completion> {
        let length = self.length_of(this)?;
        let result = match id {
            native::ARRAY_MAP | native::ARRAY_FILTER => self.new_array()?,
            _ => Value::UNDEFINED,
        };
        let mut written = 0u32;
        let mut index = 0u32;
        while index < length {
            let value = self.element(this, index)?;
            let position = Value::number(crate::softfloat::from_u64(u64::from(index)));
            let outcome = self.call_with(callback, receiver, &[value, position, this])?;
            let truth = self.coerce_to_boolean(outcome)?;
            match id {
                native::ARRAY_MAP => {
                    self.set_element(result, index, outcome)?;
                    written = index + 1;
                }
                native::ARRAY_FILTER => {
                    if truth {
                        self.set_element(result, written, value)?;
                        written += 1;
                    }
                }
                native::ARRAY_SOME => {
                    if truth {
                        return Ok(Value::boolean(true));
                    }
                }
                native::ARRAY_EVERY => {
                    if !truth {
                        return Ok(Value::boolean(false));
                    }
                }
                native::ARRAY_FIND => {
                    if truth {
                        return Ok(value);
                    }
                }
                native::ARRAY_FIND_INDEX => {
                    if truth {
                        return Ok(position);
                    }
                }
                _ => {}
            }
            index += 1;
        }
        match id {
            native::ARRAY_MAP | native::ARRAY_FILTER => {
                self.set_length(result, written)?;
                Ok(result)
            }
            native::ARRAY_SOME => Ok(Value::boolean(false)),
            native::ARRAY_EVERY => Ok(Value::boolean(true)),
            native::ARRAY_FIND => Ok(Value::UNDEFINED),
            native::ARRAY_FIND_INDEX => Ok(Value::number(-1.0)),
            _ => Ok(Value::UNDEFINED),
        }
    }

    /// Sort an array in place, by a comparison function or by string order.
    ///
    /// The sort is an insertion sort: it is stable, it allocates nothing, and
    /// the arrays an isolate this size holds are small.
    pub(super) fn sort_array(
        &mut self,
        this: Value,
        comparator: Value,
    ) -> Result<Value, Completion> {
        let length = self.length_of(this)?;
        let mut index = 1u32;
        while index < length {
            let value = self.element(this, index)?;
            let mut at = index;
            while at > 0 {
                let left = self.element(this, at - 1)?;
                let ordered = if comparator.is_undefined() {
                    let left_text = self.coerce_to_string(left)?;
                    let right_text = self.coerce_to_string(value)?;
                    string::compare(self.heap, left_text.as_handle(), right_text.as_handle())
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?
                        != core::cmp::Ordering::Greater
                } else {
                    let outcome = self.call_with(comparator, Value::UNDEFINED, &[left, value])?;
                    let number = self.coerce_to_number(outcome)?;
                    // A comparison that answers NaN leaves the order alone,
                    // which is what treating it as "not greater" does.
                    matches!(
                        number.partial_cmp(&0.0),
                        Some(core::cmp::Ordering::Less | core::cmp::Ordering::Equal) | None
                    )
                };
                if ordered {
                    break;
                }
                self.set_element(this, at, left)?;
                at -= 1;
            }
            self.set_element(this, at, value)?;
            index += 1;
        }
        Ok(this)
    }

    /// An argument used as a position: an integer at or above zero.
    pub(super) fn index_argument(&mut self, value: Value) -> Result<u32, Completion> {
        if value.is_undefined() {
            return Ok(0);
        }
        let number = self.coerce_to_number(value)?;
        if number.is_nan() || number < 0.0 {
            return Ok(0);
        }
        Ok(value::to_uint32(value::truncate(number)))
    }

    /// An argument used as a position inside a string, clamped to its length.
    pub(super) fn clamped_index(
        &mut self,
        value: Value,
        length: u32,
        default: u32,
    ) -> Result<u32, Completion> {
        if value.is_undefined() {
            return Ok(default);
        }
        Ok(self.index_argument(value)?.min(length))
    }

    /// The enumerable string keys of a value and its prototypes, in order,
    /// with a name seen once however many times it appears in the chain.
    pub(super) fn enumerable_keys(&mut self, value: Value) -> Result<Value, Completion> {
        let array = self.new_array()?;
        if value.is_nullish() {
            return Ok(array);
        }
        let object = self.coerce_to_object(value)?;
        let mut written = 0u32;
        // A typed array enumerates its indices first.
        if object::exotic_kind(self.heap, object.as_handle()).unwrap_or(0)
            == object::exotic::TYPED_ARRAY
        {
            let count = self.typed_array_length(object)?.unwrap_or(0);
            while written < count {
                let name = self.key_to_value(Key::Index(written))?;
                self.set_element(array, written, name)?;
                written += 1;
            }
        }
        let mut current = object;
        let mut depth = 0u32;
        while current.is_object() && depth < object::MAX_PROTOTYPE_DEPTH {
            let mut keys = [Key::Index(0); MAX_OWN_KEYS];
            let count = object::own_keys(self.heap, current.as_handle(), &mut keys)
                .map_err(Self::key_failure)?;
            for &key in keys.get(..count).unwrap_or(&[]) {
                if matches!(key, Key::Symbol(_)) {
                    continue;
                }
                self.namespace_touch(current, key)?;
                // A property an earlier object of the chain owns shadows this
                // one whatever its attributes: a non-enumerable own property
                // hides an enumerable inherited one rather than revealing it.
                let mut shadowed = false;
                let mut ancestor = object;
                while ancestor.is_object() {
                    if ancestor.as_handle() == current.as_handle() {
                        break;
                    }
                    if object::get_own_property(self.heap, ancestor.as_handle(), key)
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?
                        .is_some()
                    {
                        shadowed = true;
                        break;
                    }
                    ancestor = object::prototype(self.heap, ancestor.as_handle())
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                }
                if shadowed || !self.is_enumerable(current, key)? {
                    continue;
                }
                let name = self.key_to_value(key)?;
                let mut seen = false;
                let mut index = 0u32;
                while index < written {
                    let existing = self.element(array, index)?;
                    if self.strict_equals(existing, name)? {
                        seen = true;
                        break;
                    }
                    index += 1;
                }
                if !seen {
                    self.set_element(array, written, name)?;
                    written += 1;
                }
            }
            current = object::prototype(self.heap, current.as_handle())
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            depth += 1;
        }
        self.set_length(array, written)?;
        Ok(array)
    }

    /// Whether an object has a property, through a proxy's `has` trap
    /// where the object is one.
    pub(super) fn has_property_of(&mut self, object: Value, key: Key) -> Result<bool, Completion> {
        if !object.is_object() {
            return Ok(false);
        }
        match object::exotic_kind(self.heap, object.as_handle()).unwrap_or(0) {
            object::exotic::PROXY => return self.proxy_has(object, key),
            object::exotic::DEFERRED if !self.hidden_key(key) => {
                self.deferred_trigger(object, Some(key))?;
            }
            _ if self.deferred_live && !self.hidden_key(key) => {
                self.deferred_chain_trigger(object, key)?;
            }
            object::exotic::TYPED_ARRAY => {
                // An integer index is a property while it is in bounds; any
                // other numeric key names nothing, on the view or beyond it.
                if let Key::Index(index) = key {
                    return Ok(self
                        .typed_array_length(object)?
                        .is_some_and(|count| index < count));
                }
                if self.is_canonical_numeric_key(key)? {
                    return Ok(false);
                }
            }
            _ => {}
        }
        object::has_property(self.heap, object.as_handle(), key).map_err(|_| Completion::MALFORMED)
    }

    /// Whether a name is a canonical numeric string that is not an integer
    /// index — `"NaN"`, `"-0"`, `"1.5"`, `"Infinity"` — which a typed array
    /// treats as one of its own, and never holds.
    pub(super) fn is_canonical_numeric_key(&mut self, key: Key) -> Result<bool, Completion> {
        let Key::Name(handle) = key else {
            return Ok(false);
        };
        let first = string::unit_at(self.heap, handle, 0)
            .map_err(|_| Completion::HEAP_EXHAUSTED)?
            .unwrap_or(0);
        if !(first == u16::from(b'-')
            || first == u16::from(b'N')
            || first == u16::from(b'I')
            || (u16::from(b'0')..=u16::from(b'9')).contains(&first))
        {
            return Ok(false);
        }
        let text = Value::string(handle);
        let number = self.coerce_to_number(text)?;
        let minus_zero = self.ascii_string(b"-0")?;
        if self.strict_equals(text, minus_zero)? {
            return Ok(true);
        }
        let back = self.coerce_to_string(Value::number(number))?;
        self.strict_equals(back, text)
    }

    /// Find the environment that binds `name`, walking outwards: the
    /// environment module's own walk, except that an object environment's
    /// HasProperty goes through a proxy's `has` trap.
    pub(super) fn resolve_chain(
        &mut self,
        environment: Handle,
        name: Handle,
    ) -> Result<Option<env::Resolution>, Completion> {
        let mut current = environment;
        let mut depth = 0u32;
        loop {
            let kind = env::kind(self.heap, current).map_err(|_| Completion::MALFORMED)?;
            if kind == EnvironmentKind::Object {
                let object =
                    env::binding_object(self.heap, current).map_err(|_| Completion::MALFORMED)?;
                if self.has_property_of(object, Key::Name(name))? {
                    return Ok(Some(env::Resolution {
                        environment: current,
                        index: u32::MAX,
                        depth,
                    }));
                }
            } else if let Some(index) =
                env::index_of(self.heap, current, name).map_err(|_| Completion::MALFORMED)?
            {
                return Ok(Some(env::Resolution {
                    environment: current,
                    index,
                    depth,
                }));
            }
            let parent = env::parent(self.heap, current).map_err(|_| Completion::MALFORMED)?;
            if !parent.is_object() {
                return Ok(None);
            }
            depth += 1;
            if depth > env::MAX_SCOPE_DEPTH {
                return Err(Completion::MALFORMED);
            }
            current = parent.as_handle();
        }
    }

    /// A method of an object literal or a class body, with its home object.
    pub(super) fn op_define_method(
        &mut self,
        frame: &Frame,
        opcode: Opcode,
        operands: &[u32; 3],
    ) -> Step {
        let target = self.register(frame, operands[0]);
        let key = if matches!(opcode, Opcode::DefineMethod) {
            self.constant_key(operands[1])?
        } else {
            let value = self.register(frame, operands[1]);
            self.coerce_to_key(value)?
        };
        let method = self.accumulator;
        if !target.is_object() {
            return Err(Completion::MALFORMED);
        }
        if method.is_object() {
            let _ = object::set_home_object(self.heap, method.as_handle(), target.as_handle());
        }
        // A method is named for its key; a private one keeps the
        // name its definition gave it.
        if !self.hidden_key(key) {
            self.name_closure_for(method, key)?;
        }
        // A private method is not writable — a private write finding
        // one refuses — while a public method stays an ordinary
        // writable property.
        let attributes = if self.hidden_key(key) {
            attribute::CONFIGURABLE
        } else {
            attribute::WRITABLE | attribute::CONFIGURABLE
        };
        let admitted = object::define_own_property(
            self.heap,
            target.as_handle(),
            key,
            Descriptor::data(method, attributes),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        if !admitted {
            // `static ['prototype']` and its kin: the definition the
            // property refuses is a TypeError.
            return Err(self.throw_type_error());
        }

        Ok(())
    }

    /// A getter or setter of a class body, joined to any half already there.
    pub(super) fn op_define_class_accessor(
        &mut self,
        frame: &Frame,
        opcode: Opcode,
        operands: &[u32; 3],
    ) -> Step {
        let target = self.register(frame, operands[0]);
        let key = if matches!(opcode, Opcode::DefineClassAccessor) {
            self.constant_key(operands[1])?
        } else {
            let value = self.register(frame, operands[1]);
            self.coerce_to_key(value)?
        };
        let accessor = self.accumulator;
        if !target.is_object() {
            return Err(Completion::MALFORMED);
        }
        if accessor.is_object() {
            let _ = object::set_home_object(self.heap, accessor.as_handle(), target.as_handle());
        }
        self.define_accessor(
            target,
            key,
            accessor,
            operands[2] == 0,
            attribute::CONFIGURABLE,
        )?;

        Ok(())
    }

    /// Read `super.name` through the home object's prototype.
    pub(super) fn op_lda_super_property(&mut self, frame: &Frame, operands: &[u32; 3]) -> Step {
        if frame.this_pending {
            return Err(self.throw_reference_error());
        }
        let key = self.constant_key(operands[0])?;
        let callee = frame.callee;
        if !callee.is_object() {
            return Err(self.throw_type_error());
        }
        let home = object::home_object(self.heap, callee.as_handle())
            .map_err(|_| Completion::MALFORMED)?;
        let Some(home) = home else {
            return Err(self.throw_type_error());
        };
        let parent = object::prototype(self.heap, home).map_err(|_| Completion::MALFORMED)?;
        let receiver = self.this_value(frame)?;
        if !parent.is_object() {
            // A base that is not an object cannot be read through.
            return Err(self.throw_type_error());
        }
        self.accumulator = self.super_get(parent, key, receiver)?;

        Ok(())
    }

    /// Give an anonymous function the name its binding gives it.
    pub(super) fn op_name_closure(&mut self, operands: &[u32; 3]) -> Step {
        let closure = self.accumulator;
        if closure.is_object() {
            let handle = closure.as_handle();
            let name_key = self.ascii_key(b"name")?;
            let absent = object::get_own_property(self.heap, handle, name_key)
                .unwrap_or(None)
                .is_none();
            if absent {
                let constant = self.constant_key(operands[0])?;
                let name = self.name_for_key(constant, None)?;
                object::define_own_property(
                    self.heap,
                    handle,
                    name_key,
                    object::Descriptor::data(name, attribute::CONFIGURABLE),
                )
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            }
        }

        Ok(())
    }

    /// `key in object`, through a proxy where there is one.
    pub(super) fn op_test_in(&mut self, frame: &Frame, operands: &[u32; 3]) -> Step {
        let key_value = self.register(frame, operands[0]);
        let target = self.accumulator;
        if !target.is_object() {
            return Err(self.throw_type_error());
        }
        let key = self.coerce_to_key(key_value)?;
        self.materialise_function_facts(target, key)?;
        // A private member is not a property, and the engine's own
        // hidden records are nobody's: neither answers to `in`.
        let present = if self.hidden_key(key) {
            false
        } else {
            self.has_property_of(target, key)?
        };
        self.accumulator = Value::boolean(present);

        Ok(())
    }

    /// Coerce the accumulator to a property key.
    pub(super) fn op_to_property_key(&mut self) -> Step {
        let value = self.accumulator;
        // A symbol is already a key; an object becomes its
        // primitive first, which may itself be a symbol; anything
        // else becomes one by becoming a string.
        let primitive = if value.is_object() {
            self.coerce_to_primitive(value, Hint::String)?
        } else {
            value
        };
        if !matches!(primitive.tag(), Tag::Symbol) {
            self.accumulator = self.coerce_to_string(primitive)?;
        } else {
            self.accumulator = primitive;
        }

        Ok(())
    }

    /// Store through `super`.
    pub(super) fn op_sta_super(
        &mut self,
        frame: &Frame,
        opcode: Opcode,
        operands: &[u32; 3],
    ) -> Step {
        let base = self.register(frame, operands[0]);
        let key = if matches!(opcode, Opcode::StaSuperNamed) {
            self.constant_key(operands[1])?
        } else {
            let key_value = self.register(frame, operands[1]);
            self.coerce_to_key(key_value)?
        };
        let receiver = self.this_value(frame)?;
        let value = self.accumulator;
        if !base.is_object() {
            return Err(self.throw_type_error());
        }
        let strict = self.frame_is_strict(frame);
        self.super_set(base, key, value, receiver, strict)?;
        self.accumulator = value;

        Ok(())
    }

    /// The object `super` reads through.
    pub(super) fn op_get_super_base(&mut self, frame: &Frame) -> Step {
        if frame.this_pending {
            return Err(self.throw_reference_error());
        }
        let callee = frame.callee;
        if !callee.is_object() {
            return Err(self.throw_type_error());
        }
        let home = object::home_object(self.heap, callee.as_handle())
            .map_err(|_| Completion::MALFORMED)?;
        let Some(home) = home else {
            return Err(self.throw_type_error());
        };
        self.accumulator = object::prototype(self.heap, home).map_err(|_| Completion::MALFORMED)?;

        Ok(())
    }

    /// Read a property with an explicit receiver.
    pub(super) fn op_lda_with_receiver(&mut self, frame: &Frame, operands: &[u32; 3]) -> Step {
        let key = self.constant_key(operands[0])?;
        let mut receiver = Value::UNDEFINED;
        if let (Key::Name(name), true) = (key, frame.environment.is_object()) {
            if let Ok(Some(found)) = self.resolve_name(frame.environment.as_handle(), name) {
                if found.index == u32::MAX {
                    receiver = env::binding_object(self.heap, found.environment)
                        .unwrap_or(Value::UNDEFINED);
                }
            }
        }
        self.accumulator = receiver;

        Ok(())
    }
}
