//! Private names: the brand a class stamps, the storage a private member
//! lives in, and the resolution of `#name` through the enclosing scopes.

use super::*;

/// How an instruction affected control.
/// How a private member resolved against an access site.
pub(super) enum PrivateResolution {
    /// A field the receiver itself holds, under its storage key.
    Field(Descriptor, Key),
    /// A method or accessor a class object holds, under its storage key.
    Member(Descriptor, Key),
}

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// Whether a key belongs to the engine's hidden namespaces — a private
    /// member's `#` spelling or an internal `\0` record — which reflection
    /// never reports.
    pub(super) fn hidden_key(&self, key: Key) -> bool {
        let Key::Name(handle) = key else {
            return false;
        };
        matches!(crate::string::unit_at(self.heap, handle, 0), Ok(Some(0)))
    }

    /// A private member read or write demands the member exists: touching
    /// `#name` on an object without it is a TypeError, never `undefined`.
    /// Whether the key is a private name.
    pub(super) fn is_private_key(&self, key: Key) -> bool {
        if let Key::Name(handle) = key {
            crate::string::unit_at(self.heap, handle, 0) == Ok(Some(u16::from(b'#')))
        } else {
            false
        }
    }

    /// The storage key one class evaluation's private `name` lives under: a
    /// NUL prefix keeps it off every reflective surface, and the identity of
    /// the class's prototype keeps same-named privates of other classes —
    /// and other evaluations of this class — apart.
    pub(super) fn private_storage_units(
        &mut self,
        name: Handle,
        class: Handle,
        marker: bool,
        units: &mut [u16; 512],
    ) -> Result<usize, Completion> {
        let length =
            crate::string::length(self.heap, name).map_err(|_| Completion::MALFORMED)? as usize;
        if length + 25 > units.len() {
            return Err(Completion::HEAP_EXHAUSTED);
        }
        units[0] = 0;
        let mut start = 1;
        if marker {
            // A declaration marker: the class declares the name, whether or
            // not any object holds a member under it yet.
            units[1] = u16::from(b'!');
            start = 2;
        }
        crate::string::copy_units(self.heap, name, &mut units[start..start + length])
            .map_err(|_| Completion::MALFORMED)?;
        let mut at = start + length;
        units[at] = u16::from(b'@');
        at += 1;
        for part in [class.index, class.generation] {
            let mut value = part;
            let start = at;
            loop {
                units[at] = u16::from(b'0') + (value % 10) as u16;
                at += 1;
                value /= 10;
                if value == 0 {
                    break;
                }
            }
            units[start..at].reverse();
            units[at] = u16::from(b'.');
            at += 1;
        }
        Ok(at)
    }

    pub(super) fn private_storage_key(
        &mut self,
        name: Handle,
        class: Handle,
        marker: bool,
    ) -> Result<Key, Completion> {
        let mut units = [0u16; 512];
        let at = self.private_storage_units(name, class, marker, &mut units)?;
        let interned = self
            .atoms
            .intern(self.heap, &units[..at])
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(Key::Name(interned))
    }

    pub(super) fn private_storage_string(
        &mut self,
        name: Handle,
        class: Handle,
        marker: bool,
    ) -> Result<Value, Completion> {
        let mut units = [0u16; 512];
        let at = self.private_storage_units(name, class, marker, &mut units)?;
        let handle = crate::string::create(self.heap, &units[..at])
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(Value::string(handle))
    }

    /// The class evaluation an access site belongs to: the running method's
    /// home names its prototype and constructor.
    pub(super) fn private_site(&mut self, frame: &Frame) -> Result<(Value, Value), Completion> {
        let mut site_prototype = Value::UNDEFINED;
        let mut site_constructor = Value::UNDEFINED;
        if frame.callee.is_object() {
            if let Ok(Some(home)) = object::home_object(self.heap, frame.callee.as_handle()) {
                let home_value = Value::object(home);
                if object::is_callable(self.heap, home).unwrap_or(false) {
                    site_constructor = home_value;
                    let key = self.ascii_key(b"prototype")?;
                    site_prototype = self.get_property(home_value, key)?;
                } else {
                    site_prototype = home_value;
                    let key = self.ascii_key(b"constructor")?;
                    site_constructor = self.get_property(home_value, key)?;
                }
            }
        }
        Ok((site_prototype, site_constructor))
    }

    /// Whether the receiver carries the site's private member: the brand
    /// check `#x in o` performs.
    pub(super) fn private_find(
        &mut self,
        frame: &Frame,
        target: Value,
        key: Key,
    ) -> Result<bool, Completion> {
        Ok(self.private_resolve(frame, target, key).is_ok())
    }

    /// Whether `target` was stamped with `expected`'s brand.
    pub(super) fn carries_brand(
        &mut self,
        target: Value,
        expected: Handle,
    ) -> Result<bool, Completion> {
        let brand_key = self.ascii_key(b"\0brand")?;
        let held = object::get_own_property(self.heap, target.as_handle(), brand_key)
            .map_err(|_| Completion::MALFORMED)?;
        if let Some(descriptor) = held {
            let list = descriptor.value;
            if list.is_object() {
                let count = self.length_of(list)?;
                let mut index = 0u32;
                while index < count {
                    let stamped = self.element(list, index)?;
                    if stamped.is_object() && stamped.as_handle() == expected {
                        return Ok(true);
                    }
                    index += 1;
                }
            }
        }
        Ok(false)
    }

    /// The class prototype lexically enclosing `proto`'s class, when one was
    /// recorded at its definition.
    pub(super) fn outer_private_scope(
        &mut self,
        proto: Value,
    ) -> Result<Option<Value>, Completion> {
        if !proto.is_object() {
            return Ok(None);
        }
        let outer_key = self.ascii_key(b"\0outer")?;
        let held = object::get_own_property(self.heap, proto.as_handle(), outer_key)
            .map_err(|_| Completion::MALFORMED)?;
        Ok(held.map(|descriptor| descriptor.value))
    }

    /// Resolve a private member against the site's class scopes, innermost
    /// first: the descriptor and where it was found, or the field-bearing
    /// receiver, or nothing the site can see.
    pub(super) fn private_resolve(
        &mut self,
        frame: &Frame,
        target: Value,
        key: Key,
    ) -> Result<PrivateResolution, Completion> {
        let Key::Name(name) = key else {
            return Err(self.throw_type_error());
        };
        let (site_prototype, site_constructor) = self.private_site(frame)?;
        let mut proto = site_prototype;
        let mut ctor = site_constructor;
        let mut depth = 0u32;
        while depth <= env::MAX_SCOPE_DEPTH {
            if !proto.is_object() && !ctor.is_object() {
                break;
            }
            let class = if proto.is_object() {
                proto.as_handle()
            } else {
                ctor.as_handle()
            };
            let mangled = self.private_storage_key(name, class, false)?;
            for (site, is_static) in [(proto, false), (ctor, true)] {
                if !site.is_object() {
                    continue;
                }
                let found = object::get_own_property(self.heap, site.as_handle(), mangled)
                    .map_err(|_| Completion::MALFORMED)?;
                let Some(descriptor) = found else {
                    continue;
                };
                if site.as_handle() != target.as_handle() {
                    // A static private lives on the constructor and answers
                    // to it alone; an instance member asks for the brand.
                    if is_static {
                        return Err(self.throw_type_error());
                    }
                    let branded =
                        proto.is_object() && self.carries_brand(target, proto.as_handle())?;
                    if !branded {
                        return Err(self.throw_type_error());
                    }
                }
                return Ok(PrivateResolution::Member(descriptor, mangled));
            }
            let own = object::get_own_property(self.heap, target.as_handle(), mangled)
                .map_err(|_| Completion::MALFORMED)?;
            if let Some(descriptor) = own {
                return Ok(PrivateResolution::Field(descriptor, mangled));
            }
            // A class that declares the name — a field no receiver of this
            // walk holds — shadows every outer scope: the resolution stops
            // here rather than reading an outer class's same-named member.
            if proto.is_object() {
                let marker = self.private_storage_key(name, class, true)?;
                let declared = object::get_own_property(self.heap, proto.as_handle(), marker)
                    .map_err(|_| Completion::MALFORMED)?;
                if declared.is_some() {
                    return Err(self.throw_type_error());
                }
            }
            let Some(next) = self.outer_private_scope(proto)? else {
                break;
            };
            proto = next;
            ctor = if next.is_object() {
                let constructor_key = self.ascii_key(b"constructor")?;
                self.get_property(next, constructor_key)?
            } else {
                Value::UNDEFINED
            };
            depth += 1;
        }
        Err(self.throw_type_error())
    }

    /// Read a private member the site can see.
    pub(super) fn private_get(
        &mut self,
        frame: &Frame,
        target: Value,
        key: Key,
    ) -> Result<Value, Completion> {
        if !target.is_object() {
            return Err(self.throw_type_error());
        }
        match self.private_resolve(frame, target, key)? {
            PrivateResolution::Field(descriptor, _) => Ok(descriptor.value),
            PrivateResolution::Member(descriptor, _) => match descriptor.kind {
                object::DescriptorKind::Data => Ok(descriptor.value),
                object::DescriptorKind::Accessor => {
                    if self.is_callable_value(descriptor.getter) {
                        self.call_value(descriptor.getter, target, &[])
                    } else {
                        Err(self.throw_type_error())
                    }
                }
            },
        }
    }

    /// Write a private member the site can see: a field takes the value, an
    /// accessor's setter runs, a method refuses.
    pub(super) fn private_set(
        &mut self,
        frame: &Frame,
        target: Value,
        key: Key,
        value: Value,
    ) -> Result<(), Completion> {
        if !target.is_object() {
            return Err(self.throw_type_error());
        }
        match self.private_resolve(frame, target, key)? {
            PrivateResolution::Field(descriptor, mangled) => {
                object::define_own_property(
                    self.heap,
                    target.as_handle(),
                    mangled,
                    Descriptor::data(value, descriptor.attributes),
                )
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                Ok(())
            }
            PrivateResolution::Member(descriptor, mangled) => match descriptor.kind {
                // A static private field is writable data on the class
                // object; a private method is not writable and refuses.
                object::DescriptorKind::Data => {
                    if descriptor.attributes & attribute::WRITABLE != 0 {
                        object::define_own_property(
                            self.heap,
                            target.as_handle(),
                            mangled,
                            Descriptor::data(value, descriptor.attributes),
                        )
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                        Ok(())
                    } else {
                        Err(self.throw_type_error())
                    }
                }
                object::DescriptorKind::Accessor => {
                    if self.is_callable_value(descriptor.setter) {
                        self.call_value(descriptor.setter, target, &[value])?;
                        Ok(())
                    } else {
                        Err(self.throw_type_error())
                    }
                }
            },
        }
    }

    /// Stamp an instance with a class's private brand.
    pub(super) fn op_brand(&mut self, frame: &Frame) -> Step {
        let this = self.this_value(frame)?;
        let (callee, _) = self.super_constructor_of(frame)?;
        if this.is_object() && callee.is_object() {
            // A non-extensible instance takes no private methods.
            if !object::is_extensible(self.heap, this.as_handle()).unwrap_or(true) {
                return Err(self.throw_type_error());
            }
            let key = self.ascii_key(b"prototype")?;
            let proto = self.get_property(callee, key)?;
            if proto.is_object() {
                // Initialising the same object under the same class
                // twice is the TypeError the specification makes it.
                if self.carries_brand(this, proto.as_handle())? {
                    return Err(self.throw_type_error());
                }
                let brand_key = self.ascii_key(b"\0brand")?;
                let held = object::get_own_property(self.heap, this.as_handle(), brand_key)
                    .map_err(|_| Completion::MALFORMED)?;
                let list = match held {
                    Some(descriptor) if descriptor.value.is_object() => descriptor.value,
                    _ => {
                        let made = self.new_array()?;
                        object::define_own_property(
                            self.heap,
                            this.as_handle(),
                            brand_key,
                            Descriptor::data(made, attribute::WRITABLE),
                        )
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                        made
                    }
                };
                self.append_element(list, Some(proto))?;
            }
        }

        Ok(())
    }
}
