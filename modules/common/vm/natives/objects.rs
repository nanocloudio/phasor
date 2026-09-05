//! The natives of `Object` and `Object.prototype`.

use super::*;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    pub(in crate::vm) fn object_native(
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
                            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                }
                Ok(first)
            }
            native::OBJECT_IS_EXTENSIBLE => {
                let extensible = first.is_object()
                    && object::is_extensible(self.heap, first.as_handle())
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                Ok(Value::boolean(extensible))
            }
            native::OBJECT_SEAL => {
                if first.is_object() {
                    object::prevent_extensions(self.heap, first.as_handle())
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                    let mut keys = [Key::Index(0); MAX_OWN_KEYS];
                    let count = object::own_keys(self.heap, first.as_handle(), &mut keys)
                        .map_err(Self::key_failure)?;
                    for &key in keys.get(..count).unwrap_or(&[]) {
                        let Some(descriptor) =
                            object::get_own_property(self.heap, first.as_handle(), key)
                                .map_err(|_| Completion::HEAP_EXHAUSTED)?
                        else {
                            continue;
                        };
                        let sealed = Descriptor {
                            attributes: descriptor.attributes & !attribute::CONFIGURABLE,
                            ..descriptor
                        };
                        object::define_own_property(self.heap, first.as_handle(), key, sealed)
                            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                    }
                }
                Ok(first)
            }
            native::OBJECT_IS_SEALED => {
                if !first.is_object() {
                    return Ok(Value::boolean(true));
                }
                let handle = first.as_handle();
                if object::is_extensible(self.heap, handle)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?
                {
                    return Ok(Value::boolean(false));
                }
                let mut keys = [Key::Index(0); MAX_OWN_KEYS];
                let count =
                    object::own_keys(self.heap, handle, &mut keys).map_err(Self::key_failure)?;
                for &key in keys.get(..count).unwrap_or(&[]) {
                    let Some(descriptor) = object::get_own_property(self.heap, handle, key)
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?
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
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                    let mut keys = [Key::Index(0); MAX_OWN_KEYS];
                    let count = object::own_keys(self.heap, first.as_handle(), &mut keys)
                        .map_err(Self::key_failure)?;
                    for &key in keys.get(..count).unwrap_or(&[]) {
                        let Some(descriptor) =
                            object::get_own_property(self.heap, first.as_handle(), key)
                                .map_err(|_| Completion::HEAP_EXHAUSTED)?
                        else {
                            continue;
                        };
                        let frozen = Descriptor {
                            attributes: descriptor.attributes
                                & !(attribute::WRITABLE | attribute::CONFIGURABLE),
                            ..descriptor
                        };
                        object::define_own_property(self.heap, first.as_handle(), key, frozen)
                            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?
                {
                    return Ok(Value::boolean(false));
                }
                let mut keys = [Key::Index(0); MAX_OWN_KEYS];
                let count = object::own_keys(self.heap, first.as_handle(), &mut keys)
                    .map_err(Self::key_failure)?;
                for &key in keys.get(..count).unwrap_or(&[]) {
                    let Some(descriptor) =
                        object::get_own_property(self.heap, first.as_handle(), key)
                            .map_err(|_| Completion::HEAP_EXHAUSTED)?
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
                object::prototype(self.heap, object.as_handle())
                    .map_err(|_| Completion::HEAP_EXHAUSTED)
            }
            native::OBJECT_SET_PROTOTYPE_OF => {
                if first.is_object() {
                    let admitted = object::set_prototype(self.heap, first.as_handle(), second)
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
                            .map_err(|_| Completion::HEAP_EXHAUSTED)?
                            .is_some();
                    if !own {
                        return Err(self.throw_type_error());
                    }
                    for name in [&b"get"[..], &b"set"[..]] {
                        let field = self.ascii_key(name)?;
                        if object::has_property(self.heap, descriptor.as_handle(), field)
                            .map_err(|_| Completion::HEAP_EXHAUSTED)?
                        {
                            return Err(self.throw_type_error());
                        }
                    }
                    let (expected_value, expected_attributes) = if matches!(key, Key::Symbol(_)) {
                        let found = object::get_own_property(self.heap, first.as_handle(), key)
                            .map_err(|_| Completion::HEAP_EXHAUSTED)?
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
                            .map_err(|_| Completion::HEAP_EXHAUSTED)?
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
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?
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
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?
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
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?
                    {
                        described_accessor = true;
                        *slot = self.get_property(descriptor, field)?;
                    }
                }
                let value_field = self.ascii_key(b"value")?;
                if object::has_property(self.heap, descriptor.as_handle(), value_field)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?
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
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
                    object::create(self.heap, prototype).map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?
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
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?
                        .is_some();
                Ok(Value::boolean(present))
            }
            native::OBJECT_IS_PROTOTYPE_OF => {
                if !first.is_object() || !this.is_object() {
                    return Ok(Value::boolean(false));
                }
                let mut current = object::prototype(self.heap, first.as_handle())
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                let mut depth = 0u32;
                while current.is_object() && depth < object::MAX_PROTOTYPE_DEPTH {
                    if current.as_handle() == this.as_handle() {
                        return Ok(Value::boolean(true));
                    }
                    current = object::prototype(self.heap, current.as_handle())
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
}
