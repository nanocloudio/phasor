//! The iterator helpers and the iterator prototypes.

use super::*;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// One step of an iterator the engine made itself.
    pub(in crate::vm) fn iterator_native(
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
        let Some((target, index, kind)) = object::iterator_state(self.heap, this.as_handle())
            .map_err(|_| Completion::HEAP_EXHAUSTED)?
        else {
            return Err(self.throw_type_error());
        };
        let result = object::create(self.heap, Value::object(self.realm.object_prototype))
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
                .map_err(|_| Completion::HEAP_EXHAUSTED)?
            {
                Some((_, width)) => {
                    let piece = string::slice(self.heap, handle, index, index + width)
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        self.define_property(result, value_key, value)?;
        self.define_property(result, done_key, Value::boolean(done))?;
        Ok(result)
    }
}
