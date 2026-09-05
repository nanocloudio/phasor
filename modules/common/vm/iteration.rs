//! The iteration protocols, sync and async.

use super::*;

/// What an iterator produces at each step.
pub(super) const ITERATE_VALUES: u8 = 0;

pub(super) const ITERATE_KEYS: u8 = 1;

pub(super) const ITERATE_ENTRIES: u8 = 2;

pub(super) const ITERATE_CODE_POINTS: u8 = 3;

/// A Map's live [key, value] pairs, and a Set's live [value, value] pairs.
pub(super) const ITERATE_MAP_ENTRIES: u8 = 4;

pub(super) const ITERATE_SET_ENTRIES: u8 = 5;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// The iterator a value offers, or nothing where it offers none.
    /// The iterator a `for await` walks: the async protocol's when the
    /// value carries one, the sync protocol's otherwise — whose results the
    /// loop awaits either way.
    pub(super) fn async_iterator_of(&mut self, value: Value) -> Result<Option<Value>, Completion> {
        if value.is_nullish() {
            return Ok(None);
        }
        let key = Key::Symbol(self.realm.async_iterator_symbol);
        let method = self.get_property(value, key)?;
        if self.is_callable_value(method) {
            let iterator = self.call_value(method, value, &[])?;
            if !iterator.is_object() {
                return Err(self.throw_type_error());
            }
            return Ok(Some(iterator));
        }
        // GetMethod: only an absent method falls back to the sync iterator;
        // a present value that is not callable is a TypeError.
        if !method.is_nullish() {
            return Err(self.throw_type_error());
        }
        let Some(sync) = self.iterator_of(value)? else {
            return Ok(None);
        };
        self.async_from_sync(sync).map(Some)
    }

    /// Wrap a sync iterator as an async one: `next`, `return`, and `throw`
    /// call through and answer a promise settled once the result's value
    /// has been awaited — the specification's %AsyncFromSyncIteratorPrototype%.
    pub(super) fn async_from_sync(&mut self, sync: Value) -> Result<Value, Completion> {
        let wrapper = object::create(self.heap, Value::object(self.realm.object_prototype))
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        // The iterator record fetches `next` once, at creation: every step
        // calls what that fetch produced.
        let next_key = self.ascii_key(b"next")?;
        let next_method = self.get_property(sync, next_key)?;
        for (name, held) in [(&b"\0sync"[..], sync), (&b"\0next"[..], next_method)] {
            let key = self.ascii_key(name)?;
            object::define_own_property(
                self.heap,
                wrapper,
                key,
                Descriptor::data(held, attribute::WRITABLE),
            )
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        for (name, id) in [
            (&b"next"[..], native::ASYNC_FROM_SYNC_NEXT),
            (&b"return"[..], native::ASYNC_FROM_SYNC_RETURN),
            (&b"throw"[..], native::ASYNC_FROM_SYNC_THROW),
        ] {
            let method = self.settle_function(id, wrapper)?;
            let key = self.ascii_key(name)?;
            object::define_own_property(
                self.heap,
                wrapper,
                key,
                Descriptor::data(method, attribute::WRITABLE | attribute::CONFIGURABLE),
            )
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        Ok(Value::object(wrapper))
    }

    /// One step of an async-from-sync wrapper: call the sync iterator's
    /// method and answer a promise that settles once the result's value has
    /// been awaited — a rejection closing the sync iterator where the step
    /// was not already done, unless the step was a `return`.
    pub(super) fn async_from_sync_step(
        &mut self,
        id: u32,
        wrapper: Value,
        argument: Option<Value>,
    ) -> Result<Value, Completion> {
        let promise = self.new_promise()?;
        let answer = Value::object(promise);
        let sync_key = self.ascii_key(b"\0sync")?;
        let sync = if wrapper.is_object() {
            object::get_own_property(self.heap, wrapper.as_handle(), sync_key)
                .map_err(|_| Completion::MALFORMED)?
                .map_or(Value::UNDEFINED, |descriptor| descriptor.value)
        } else {
            Value::UNDEFINED
        };
        let name: &[u8] = match id {
            native::ASYNC_FROM_SYNC_RETURN => b"return",
            native::ASYNC_FROM_SYNC_THROW => b"throw",
            _ => b"\0next",
        };
        let key = self.ascii_key(name)?;
        let fetched = if id == native::ASYNC_FROM_SYNC_NEXT {
            if wrapper.is_object() {
                object::get_own_property(self.heap, wrapper.as_handle(), key)
                    .map_err(|_| Completion::MALFORMED)
                    .map(|held| held.map_or(Value::UNDEFINED, |descriptor| descriptor.value))
            } else {
                Ok(Value::UNDEFINED)
            }
        } else {
            self.get_property(sync, key)
        };
        let method = match fetched {
            Ok(method) => method,
            Err(Completion::Throw(reason)) => {
                self.settle(promise, promise::REJECTED, reason)?;
                return Ok(answer);
            }
            Err(other) => return Err(other),
        };
        if id != native::ASYNC_FROM_SYNC_NEXT && method.is_nullish() {
            if id == native::ASYNC_FROM_SYNC_RETURN {
                // No `return`: the iterator counts as closed, the value
                // handed back done.
                let result = self.iteration_result(argument.unwrap_or(Value::UNDEFINED), true)?;
                self.resolve(promise, result)?;
                return Ok(answer);
            }
            // No `throw`: the iterator is closed, and the caller told so.
            match self.close_iterator(sync) {
                Ok(()) => {}
                Err(Completion::Throw(reason)) => {
                    self.settle(promise, promise::REJECTED, reason)?;
                    return Ok(answer);
                }
                Err(other) => return Err(other),
            }
            let reason = self.create_error(ErrorKind::Type, Value::UNDEFINED)?;
            self.settle(promise, promise::REJECTED, reason)?;
            return Ok(answer);
        }
        let called = match argument {
            Some(argument) => self.call_value(method, sync, &[argument]),
            None => self.call_value(method, sync, &[]),
        };
        let result = match called {
            Ok(result) => result,
            Err(Completion::Throw(reason)) => {
                self.settle(promise, promise::REJECTED, reason)?;
                return Ok(answer);
            }
            Err(other) => return Err(other),
        };
        if !result.is_object() {
            let reason = self.create_error(ErrorKind::Type, Value::UNDEFINED)?;
            self.settle(promise, promise::REJECTED, reason)?;
            return Ok(answer);
        }
        let done_key = self.ascii_key(b"done")?;
        let value_key = self.ascii_key(b"value")?;
        let fields = match self.get_property(result, done_key) {
            Ok(done) => match self.coerce_to_boolean(done) {
                Ok(done) => match self.get_property(result, value_key) {
                    Ok(value) => Ok((done, value)),
                    Err(error) => Err(error),
                },
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        let (done, value) = match fields {
            Ok(fields) => fields,
            Err(Completion::Throw(reason)) => {
                self.settle(promise, promise::REJECTED, reason)?;
                return Ok(answer);
            }
            Err(other) => return Err(other),
        };
        let close_on_rejection = id != native::ASYNC_FROM_SYNC_RETURN && !done;
        let wrapped = match self.promise_for(value) {
            Ok(wrapped) => wrapped,
            Err(Completion::Throw(reason)) => {
                if close_on_rejection {
                    match self.close_iterator(sync) {
                        Ok(()) | Err(Completion::Throw(_)) => {}
                        Err(other) => return Err(other),
                    }
                }
                self.settle(promise, promise::REJECTED, reason)?;
                return Ok(answer);
            }
            Err(other) => return Err(other),
        };
        let on_ok = self.settle_function(
            if done {
                native::ASYNC_FROM_SYNC_DONE
            } else {
                native::ASYNC_FROM_SYNC_MORE
            },
            promise,
        )?;
        let on_err = if close_on_rejection && sync.is_object() {
            self.settle_function(native::ASYNC_FROM_SYNC_CLOSE, sync.as_handle())?
        } else {
            self.settle_function(native::ASYNC_FROM_SYNC_PASS, promise)?
        };
        let Some(queue) = self.queue.as_deref_mut() else {
            return Err(Completion::Terminated(Termination::NotImplemented));
        };
        promise::react(self.heap, queue, wrapped, on_ok, on_err, answer)
            .map_err(|_| Completion::QUOTA_EXCEEDED)?;
        Ok(answer)
    }

    pub(super) fn iterator_of(&mut self, value: Value) -> Result<Option<Value>, Completion> {
        if value.is_nullish() {
            return Ok(None);
        }
        let key = Key::Symbol(self.realm.iterator_symbol);
        let method = self.get_property(value, key)?;
        if !self.is_callable_value(method) {
            return Ok(None);
        }
        let iterator = self.call_value(method, value, &[])?;
        if !iterator.is_object() {
            return Err(self.throw_type_error());
        }
        Ok(Some(iterator))
    }

    /// Close an iterator the language finished with early: its `return`
    /// method runs, and a result that is not an object is the TypeError the
    /// specification makes it.
    pub(super) fn close_iterator(&mut self, iterator: Value) -> Result<(), Completion> {
        if !iterator.is_object() {
            return Ok(());
        }
        let return_key = self.ascii_key(b"return")?;
        let method = self.get_property(iterator, return_key)?;
        if method.is_nullish() {
            return Ok(());
        }
        if !self.is_callable_value(method) {
            return Err(self.throw_type_error());
        }
        let result = self.call_value(method, iterator, &[])?;
        if !result.is_object() {
            return Err(self.throw_type_error());
        }
        Ok(())
    }

    /// The next value an iterator produces, or nothing when it is done.
    pub(super) fn iterator_step(&mut self, iterator: Value) -> Result<Option<Value>, Completion> {
        let next_key = self.ascii_key(b"next")?;
        let next = self.get_property(iterator, next_key)?;
        if !self.is_callable_value(next) {
            return Err(self.throw_type_error());
        }
        let result = self.call_value(next, iterator, &[])?;
        if !result.is_object() {
            return Err(self.throw_type_error());
        }
        let done_key = self.ascii_key(b"done")?;
        let done = self.get_property(result, done_key)?;
        if self.coerce_to_boolean(done)? {
            return Ok(None);
        }
        let value_key = self.ascii_key(b"value")?;
        Ok(Some(self.get_property(result, value_key)?))
    }
}
