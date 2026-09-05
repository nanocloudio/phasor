//! Realms beyond the first: which realm a function belongs to, and making
//! another.

use super::*;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// Make the current realm the one the running frame's unit belongs to.
    pub(super) fn sync_realm(&mut self) {
        if self.depth == 0 {
            return;
        }
        let module = self.frames[self.depth as usize - 1].module as usize;
        let index = self.unit_realm.get(module).copied().unwrap_or(0);
        if let Some(realm) = self.realms.get(usize::from(index)).copied().flatten() {
            self.realm = realm;
        }
    }

    /// Which realm a function belongs to: a closure's unit's, a native's by
    /// the function prototype it was made over.
    pub(super) fn realm_index_of_function(&self, function: Handle) -> u8 {
        if object::is_native(self.heap, function).unwrap_or(false) {
            let prototype = object::prototype(self.heap, function).unwrap_or(Value::UNDEFINED);
            if prototype.is_object() {
                for (index, realm) in self.realms.iter().enumerate() {
                    if let Some(realm) = realm {
                        if realm.function_prototype == prototype.as_handle() {
                            return u8::try_from(index).unwrap_or(0);
                        }
                    }
                }
            }
            return 0;
        }
        let module = object::function_module(self.heap, function).unwrap_or(0) as usize;
        self.unit_realm.get(module).copied().unwrap_or(0)
    }

    pub(super) fn realm_of_function(&self, function: Handle) -> Realm {
        let index = self.realm_index_of_function(function);
        self.realms
            .get(usize::from(index))
            .copied()
            .flatten()
            .unwrap_or(self.realm)
    }

    /// `$262.createRealm()`: a fresh realm beside this one, with its own
    /// host object, answering that object.
    pub(super) fn create_realm(&mut self) -> Result<Value, Completion> {
        let Some(slot) = self.realms.iter().position(|realm| realm.is_none()) else {
            return Err(self.throw_error_of(ErrorKind::Range));
        };
        let made = match crate::realm::create(self.heap, self.atoms) {
            Ok(made) => made,
            Err(_) => return Err(Completion::HEAP_EXHAUSTED),
        };
        if crate::realm::install_print(self.heap, self.atoms, &made).is_err()
            || crate::realm::install_262(self.heap, self.atoms, &made).is_err()
            || crate::realm::install_random(self.heap, self.atoms, &made).is_err()
        {
            return Err(Completion::HEAP_EXHAUSTED);
        }
        self.realms[slot] = Some(made);
        let key = self.ascii_key(b"$262")?;
        let host = object::get_own_property(self.heap, made.global, key)
            .map_err(|_| Completion::MALFORMED)?
            .map_or(Value::UNDEFINED, |descriptor| descriptor.value);
        Ok(host)
    }
}
