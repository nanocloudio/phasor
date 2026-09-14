//! The functions the engine implements itself: the entry that routes a
//! native id to its area, and the promise combinators.

use super::*;
#[path = "arrays.rs"]
mod arrays;
#[path = "collections.rs"]
mod collections;
#[path = "functions.rs"]
mod functions;
#[path = "iterators.rs"]
mod iterators;
#[path = "math.rs"]
mod math;
#[path = "numbers.rs"]
mod numbers;
#[path = "objects.rs"]
mod objects;
#[path = "reflect.rs"]
mod reflect;
#[path = "regexps.rs"]
mod regexps;
#[path = "strings.rs"]
mod strings;
#[path = "symbols.rs"]
mod symbols;
#[path = "uri.rs"]
mod uri;
use arrays::*;
use collections::*;
use functions::*;
use iterators::*;
use math::*;
use numbers::*;
use objects::*;
use reflect::*;
use regexps::*;
use strings::*;
use symbols::*;
use uri::*;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// `Symbol`, and what a symbol carries.
    /// One of the four Promise combinators over an iterable of values.
    pub(super) fn promise_combinator(
        &mut self,
        id: u32,
        iterable: Value,
    ) -> Result<Value, Completion> {
        let mode = match id {
            native::PROMISE_RACE => 1.0,
            native::PROMISE_ALL_SETTLED => 2.0,
            native::PROMISE_ANY => 3.0,
            _ => 0.0,
        };
        let promise = self.new_promise()?;
        let results = self.new_array()?;
        let record =
            object::create(self.heap, Value::NULL).map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
    pub(super) fn combinator_adjust(
        &mut self,
        record: Value,
        delta: f64,
    ) -> Result<f64, Completion> {
        let key = self.ascii_key(b"remaining")?;
        let current = self.get_property(record, key)?.as_number();
        let next = current + delta;
        object::define_own_property(
            self.heap,
            record.as_handle(),
            key,
            Descriptor::data(Value::number(next), attribute::WRITABLE),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(next)
    }

    /// Settle a combinator whose count ran out: what the mode gathers wins.
    pub(super) fn combinator_finish(&mut self, record: Value) -> Result<(), Completion> {
        let results_key = self.ascii_key(b"results")?;
        let promise_key = self.ascii_key(b"promise")?;
        let mode_key = self.ascii_key(b"mode")?;
        let results = self.get_property(record, results_key)?;
        let promise = self.get_property(record, promise_key)?;
        let mode = self.get_property(record, mode_key)?.as_number();
        if !promise.is_object() {
            return Err(Completion::MALFORMED);
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
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                object::define_own_property(
                    self.heap,
                    reason.as_handle(),
                    errors_key,
                    Descriptor::data(results, attribute::DEFAULT),
                )
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            }
            return self.settle(promise.as_handle(), promise::REJECTED, reason);
        }
        self.settle(promise.as_handle(), promise::FULFILLED, results)
    }

    /// One combinator element settled: fold it into the record.
    pub(super) fn combine_settled(
        &mut self,
        fulfilled: bool,
        value: Value,
    ) -> Result<(), Completion> {
        let Some(function) = self.current_native else {
            return Err(Completion::MALFORMED);
        };
        let state =
            object::function_environment(self.heap, function).map_err(|_| Completion::MALFORMED)?;
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
            return Err(Completion::MALFORMED);
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
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                object::define_own_property(
                    self.heap,
                    entry,
                    value_key,
                    Descriptor::data(value, attribute::DEFAULT),
                )
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                self.set_property(results, Key::Index(index), Value::object(entry))?;
                if self.combinator_adjust(record, -1.0)? == 0.0 {
                    self.combinator_finish(record)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Call one of the functions the engine implements itself.
    pub(super) fn call_native(
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
                    .map_err(|_| Completion::MALFORMED)?;
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
                    return Err(Completion::MALFORMED);
                };
                let callback = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::MALFORMED)?;
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
                        .map_err(|_| Completion::MALFORMED)?;
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
                        .map_err(|_| Completion::MALFORMED)?;
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
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
                    return Err(Completion::MALFORMED);
                };
                let generator = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::MALFORMED)?;
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
                    return Err(Completion::MALFORMED);
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
                    return Err(Completion::MALFORMED);
                };
                let binding = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::MALFORMED)?;
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
                    return Err(Completion::MALFORMED);
                };
                let binding = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::MALFORMED)?;
                let module = value::to_uint32(self.element(binding, 0)?.as_number());
                let reason = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                self.poison_cycle(module, reason);
                Err(Completion::Throw(reason))
            }
            native::ACCESSOR_GET | native::ACCESSOR_SET => {
                // An auto-accessor: the function carries the hidden name its
                // field stores behind on the receiver.
                let Some(function) = self.current_native else {
                    return Err(Completion::MALFORMED);
                };
                let backing = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::MALFORMED)?;
                let key = self.coerce_to_key(backing)?;
                if !this.is_object() {
                    return Err(self.throw_type_error());
                }
                if id == native::ACCESSOR_GET {
                    let held = object::get_own_property(self.heap, this.as_handle(), key)
                        .map_err(|_| Completion::MALFORMED)?;
                    Ok(held.map_or(Value::UNDEFINED, |descriptor| descriptor.value))
                } else {
                    let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                    object::define_own_property(
                        self.heap,
                        this.as_handle(),
                        key,
                        Descriptor::data(first, attribute::WRITABLE),
                    )
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                let held = self.ascii_key(b"\0proxy")?;
                object::define_own_property(
                    self.heap,
                    revoke,
                    held,
                    Descriptor::data(proxy, attribute::WRITABLE),
                )
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                let result = object::create(self.heap, Value::object(self.realm.object_prototype))
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                }
                Ok(Value::object(result))
            }
            native::PROXY_REVOKE => {
                // Revoking cuts the proxy from its target and handler: every
                // later operation on it is a TypeError, though `typeof` still
                // answers what it always did.
                let Some(function) = self.current_native else {
                    return Err(Completion::MALFORMED);
                };
                let held = self.ascii_key(b"\0proxy")?;
                let proxy = object::get_own_property(self.heap, function, held)
                    .map_err(|_| Completion::MALFORMED)?
                    .map_or(Value::UNDEFINED, |descriptor| descriptor.value);
                if proxy.is_object() {
                    object::define_own_property(
                        self.heap,
                        function,
                        held,
                        Descriptor::data(Value::NULL, attribute::WRITABLE),
                    )
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                    for name in [&b"\0target"[..], &b"\0handler"[..]] {
                        let key = self.ascii_key(name)?;
                        object::define_own_property(
                            self.heap,
                            proxy.as_handle(),
                            key,
                            Descriptor::data(Value::NULL, attribute::WRITABLE),
                        )
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
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
                        .map_err(|_| Completion::MALFORMED)?
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
                    return Err(Completion::MALFORMED);
                };
                let wrapper = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::MALFORMED)?;
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
                        return Err(Completion::MALFORMED);
                    };
                    let sync = object::function_environment(self.heap, function)
                        .map_err(|_| Completion::MALFORMED)?;
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
                    return Err(Completion::MALFORMED);
                };
                let generator = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::MALFORMED)?;
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
                    return Err(Completion::MALFORMED);
                };
                let generator = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::MALFORMED)?;
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
                    return Err(Completion::MALFORMED);
                };
                let bound = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::MALFORMED)?;
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
                self.emit_print(text.as_handle())?;
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
                    return Err(Completion::MALFORMED);
                };
                let coroutine = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::MALFORMED)?;
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
                    return Err(Completion::MALFORMED);
                };
                let bound = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::MALFORMED)?;
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
            native::DECODE_UTF8 | native::ENCODE_UTF8 => {
                let first = arguments.first().copied().unwrap_or(Value::UNDEFINED);
                let text = self.coerce_to_string(first)?;
                let handle = text.as_handle();
                let converted = if id == native::DECODE_UTF8 {
                    crate::string::decode_utf8(self.heap, handle)
                } else {
                    crate::string::encode_utf8(self.heap, handle)
                };
                match converted {
                    Ok(result) => Ok(Value::string(result)),
                    Err(_) => Err(Completion::HEAP_EXHAUSTED),
                }
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
                    return Err(Completion::MALFORMED);
                };
                let binding = object::function_environment(self.heap, function)
                    .map_err(|_| Completion::MALFORMED)?;
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
