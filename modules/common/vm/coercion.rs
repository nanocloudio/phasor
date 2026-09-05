//! The abstract operations over values: the coercions, equality, ordering,
//! and the constants a unit carries.

use super::*;

/// Whether a code unit is whitespace to the string grammar: the whitespace
/// characters, the line terminators, and the Unicode space separators — the
/// set `StringToBigInt` and `StringToNumber` skip at either end.
pub(super) fn is_string_white_space(unit: u16) -> bool {
    matches!(
        unit,
        0x0009 | 0x000A | 0x000B | 0x000C | 0x000D | 0x0020 | 0x00A0 | 0x2028 | 0x2029 | 0xFEFF
    ) || crate::unicode_id::is_space_separator(u32::from(unit))
}

/// The preference `ToPrimitive` starts with.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Hint {
    Default,
    Number,
    String,
}

/// The longest string the interpreter stages on its own stack.
pub(super) const MAX_STRING_UNITS: usize = 256;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    // Coercions.

    pub(super) fn coerce_to_boolean(&mut self, value: Value) -> Result<bool, Completion> {
        if let Some(truth) = value::to_boolean_primitive(&value) {
            return Ok(truth);
        }
        match value.tag() {
            Tag::String => {
                let length = string::length(self.heap, value.as_handle())
                    .map_err(|_| Completion::MALFORMED)?;
                Ok(length != 0)
            }
            Tag::BigInt => {
                let number = self.big_int_operand(value)?;
                Ok(!number.is_zero())
            }
            _ => Ok(true),
        }
    }

    pub(super) fn coerce_to_number(&mut self, value: Value) -> Result<f64, Completion> {
        if let Some(number) = value::to_number_primitive(&value) {
            return Ok(number);
        }
        match value.tag() {
            Tag::String => {
                let mut units = [0u16; MAX_STRING_UNITS];
                let length = string::length(self.heap, value.as_handle())
                    .map_err(|_| Completion::MALFORMED)? as usize;
                if length > units.len() {
                    return Ok(f64::NAN);
                }
                string::copy_units(self.heap, value.as_handle(), &mut units[..length])
                    .map_err(|_| Completion::MALFORMED)?;
                Ok(value::string_to_number(&units[..length]))
            }
            Tag::Object => {
                let primitive = self.coerce_to_primitive(value, Hint::Number)?;
                if primitive.is_object() {
                    return Err(self.throw_type_error());
                }
                self.coerce_to_number(primitive)
            }
            _ => Err(self.throw_type_error()),
        }
    }

    /// `ToPrimitive` for an object: try `valueOf`, then `toString`, calling
    /// whichever is callable, in the order the hint requires.
    pub(super) fn coerce_to_primitive(
        &mut self,
        value: Value,
        hint: Hint,
    ) -> Result<Value, Completion> {
        if !value.is_object() {
            return Ok(value);
        }
        // A `Symbol.toPrimitive` method decides for itself, hint and all.
        let hint_value = Value::string(match hint {
            Hint::String => self.realm.hint_string,
            Hint::Number => self.realm.hint_number,
            _ => self.realm.hint_default,
        });
        let exotic = self.get_property(value, Key::Symbol(self.realm.to_primitive_symbol))?;
        if self.is_callable_value(exotic) {
            let result = self.call_value(exotic, value, &[hint_value])?;
            if !result.is_object() {
                return Ok(result);
            }
            return Err(self.throw_type_error());
        }
        // GetMethod: a present Symbol.toPrimitive that is not callable is a
        // TypeError, not a fall-through to valueOf and toString.
        if !exotic.is_nullish() {
            return Err(self.throw_type_error());
        }
        let order: [&[u8]; 2] = match hint {
            Hint::String => [b"toString", b"valueOf"],
            _ => [b"valueOf", b"toString"],
        };
        for name in order {
            let key = self.ascii_key(name)?;
            let method = self.get_property(value, key)?;
            if method.is_object()
                && object::is_callable(self.heap, method.as_handle()).unwrap_or(false)
            {
                let result = self.call_value(method, value, &[])?;
                if !result.is_object() {
                    return Ok(result);
                }
            }
        }
        Err(self.throw_type_error())
    }

    pub(super) fn coerce_to_string(&mut self, value: Value) -> Result<Value, Completion> {
        match value.tag() {
            Tag::String => Ok(value),
            Tag::BigInt => self.big_int_text(value, 10),
            Tag::Number => {
                let mut units = [0u16; 40];
                let written = string::number_to_string(value.as_number(), &mut units);
                self.make_string(&units[..written])
            }
            Tag::Undefined => self.ascii_string(b"undefined"),
            Tag::Null => self.ascii_string(b"null"),
            Tag::Boolean => {
                if value.as_boolean() {
                    self.ascii_string(b"true")
                } else {
                    self.ascii_string(b"false")
                }
            }
            Tag::Object => {
                let primitive = self.coerce_to_primitive(value, Hint::String)?;
                if primitive.is_object() {
                    return Err(self.throw_type_error());
                }
                self.coerce_to_string(primitive)
            }
            _ => Err(self.throw_type_error()),
        }
    }

    pub(super) fn make_string(&mut self, units: &[u16]) -> Result<Value, Completion> {
        let handle = string::create(self.heap, units).map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(Value::string(handle))
    }

    pub(super) fn ascii_string(&mut self, text: &[u8]) -> Result<Value, Completion> {
        let handle =
            string::create_ascii(self.heap, text).map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(Value::string(handle))
    }

    pub(super) fn ascii_key(&mut self, text: &[u8]) -> Result<Key, Completion> {
        let mut units = [0u16; 32];
        let mut length = 0usize;
        for &byte in text {
            if length < units.len() {
                units[length] = u16::from(byte);
                length += 1;
            }
        }
        let handle = self
            .atoms
            .intern(self.heap, &units[..length])
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(Key::Name(handle))
    }

    /// The key a value denotes, interning a string so that key comparison stays
    /// a handle comparison.
    pub(super) fn coerce_to_key(&mut self, value: Value) -> Result<Key, Completion> {
        let value = if value.is_object() {
            self.coerce_to_primitive(value, Hint::String)?
        } else {
            value
        };
        if matches!(value.tag(), Tag::Symbol) {
            return Ok(Key::Symbol(value.as_handle()));
        }
        let text = self.coerce_to_string(value)?;
        let handle = text.as_handle();
        let length = string::length(self.heap, handle).map_err(|_| Completion::MALFORMED)? as usize;
        let mut units = [0u16; MAX_STRING_UNITS];
        if length > units.len() {
            // A long name is never an array index; it is interned by the
            // string it already is.
            let interned = self
                .atoms
                .intern_string(self.heap, handle)
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            return Ok(Key::Name(interned));
        }
        string::copy_units(self.heap, handle, &mut units[..length])
            .map_err(|_| Completion::MALFORMED)?;
        if let Some(index) = string::array_index(&units[..length]) {
            return Ok(Key::Index(index));
        }
        let interned = self
            .atoms
            .intern(self.heap, &units[..length])
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(Key::Name(interned))
    }

    pub(super) fn constant_key(&mut self, index: u32) -> Result<Key, Completion> {
        let value = self.load_constant(index)?;
        self.coerce_to_key(value)
    }

    pub(super) fn load_constant(&mut self, index: u32) -> Result<Value, Completion> {
        let Some(constant) = self.unit().constant(index) else {
            return Err(Completion::MALFORMED);
        };
        match constant.kind {
            ConstantKind::Number => Ok(Value::number(constant.value())),
            ConstantKind::String | ConstantKind::Key => {
                let mut units = [0u16; MAX_STRING_UNITS];
                let length = constant.second as usize;
                if length > units.len() {
                    // A long constant goes to the heap straight from the
                    // image, with no buffer of its own between.
                    let bytes = self
                        .unit()
                        .constant_unit_bytes(&constant)
                        .ok_or(Completion::MALFORMED)?;
                    let handle = string::create_from_le_bytes(self.heap, bytes)
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                    return Ok(Value::string(handle));
                }
                if self
                    .unit()
                    .constant_units(&constant, &mut units[..length])
                    .is_none()
                {
                    return Err(Completion::MALFORMED);
                }
                self.make_string(&units[..length])
            }
            ConstantKind::RegExp => Err(Completion::MALFORMED),
            ConstantKind::BigInt => {
                // Read out of the image where it lies: a literal of any length
                // is the number it was written as, or an image the machine
                // refuses, never a prefix of itself.
                let bytes = self
                    .unit()
                    .constant_bytes(&constant)
                    .ok_or(Completion::MALFORMED)?;
                // The constant carries its radix in front of its digits.
                let radix = u32::from(bytes.first().copied().unwrap_or(10));
                let number =
                    crate::bigint::from_digits(bytes.get(1..).unwrap_or(&[]), radix, false)
                        .map_err(|_| Completion::MALFORMED)?;
                let handle = crate::bigint::write(self.heap, &number)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                Ok(Value::big_int(handle))
            }
        }
    }

    // Operators.

    pub(super) fn add_values(&mut self, left: Value, right: Value) -> Result<Value, Completion> {
        let left = self.coerce_to_primitive(left, Hint::Default)?;
        let right = self.coerce_to_primitive(right, Hint::Default)?;
        if self.either_is_big_int(left, right)
            && !matches!(left.tag(), Tag::String)
            && !matches!(right.tag(), Tag::String)
        {
            return self.big_int_arithmetic(Opcode::Add, left, right);
        }
        if matches!(left.tag(), Tag::String) || matches!(right.tag(), Tag::String) {
            let left_text = self.coerce_to_string(left)?;
            let right_text = self.coerce_to_string(right)?;
            let joined = string::concat(self.heap, left_text.as_handle(), right_text.as_handle())
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            return Ok(Value::string(joined));
        }
        let left_number = self.coerce_to_number(left)?;
        let right_number = self.coerce_to_number(right)?;
        Ok(Value::number(value::add(left_number, right_number)))
    }

    pub(super) fn strict_equals(&mut self, left: Value, right: Value) -> Result<bool, Completion> {
        if let Some(equal) = value::strict_equals(&left, &right) {
            return Ok(equal);
        }
        // Two distinct string cells with the same contents are the same value.
        if matches!(left.tag(), Tag::String) && matches!(right.tag(), Tag::String) {
            return string::equals(self.heap, left.as_handle(), right.as_handle())
                .map_err(|_| Completion::MALFORMED);
        }
        // Two BigInt cells are the same value when they hold the same integer.
        if matches!(left.tag(), Tag::BigInt) && matches!(right.tag(), Tag::BigInt) {
            let left = self.big_int_operand(left)?;
            let right = self.big_int_operand(right)?;
            return Ok(crate::bigint::compare(&left, &right) == core::cmp::Ordering::Equal);
        }
        Ok(false)
    }

    pub(super) fn loose_equals(&mut self, left: Value, right: Value) -> Result<bool, Completion> {
        if left.tag() == right.tag() {
            return self.strict_equals(left, right);
        }
        match (left.tag(), right.tag()) {
            (Tag::Null, Tag::Undefined) | (Tag::Undefined, Tag::Null) => Ok(true),
            (Tag::BigInt, Tag::Number)
            | (Tag::Number, Tag::BigInt)
            | (Tag::BigInt, Tag::String)
            | (Tag::String, Tag::BigInt) => {
                // A BigInt equals a Number when they are the same mathematical
                // value, whatever their types.
                let ordering = self.big_int_ordering(left, right)?;
                Ok(ordering == Some(core::cmp::Ordering::Equal))
            }
            (Tag::Number, Tag::String) | (Tag::String, Tag::Number) => {
                let left_number = self.coerce_to_number(left)?;
                let right_number = self.coerce_to_number(right)?;
                Ok(value::number_equals(left_number, right_number))
            }
            (Tag::Boolean, _) => {
                let number = self.coerce_to_number(left)?;
                self.loose_equals(Value::number(number), right)
            }
            (_, Tag::Boolean) => {
                let number = self.coerce_to_number(right)?;
                self.loose_equals(left, Value::number(number))
            }
            (Tag::Object, Tag::Number | Tag::String | Tag::BigInt | Tag::Symbol) => {
                let primitive = self.coerce_to_primitive(left, Hint::Default)?;
                self.loose_equals(primitive, right)
            }
            (Tag::Number | Tag::String | Tag::BigInt | Tag::Symbol, Tag::Object) => {
                let primitive = self.coerce_to_primitive(right, Hint::Default)?;
                self.loose_equals(left, primitive)
            }
            _ => Ok(false),
        }
    }

    pub(super) fn compare(
        &mut self,
        opcode: Opcode,
        left: Value,
        right: Value,
    ) -> Result<Value, Completion> {
        let left_primitive = self.coerce_to_primitive(left, Hint::Number)?;
        let right_primitive = self.coerce_to_primitive(right, Hint::Number)?;
        if matches!(left_primitive.tag(), Tag::String)
            && matches!(right_primitive.tag(), Tag::String)
        {
            let ordering = string::compare(
                self.heap,
                left_primitive.as_handle(),
                right_primitive.as_handle(),
            )
            .map_err(|_| Completion::MALFORMED)?;
            let result = match opcode {
                Opcode::TestLess => ordering == core::cmp::Ordering::Less,
                Opcode::TestGreater => ordering == core::cmp::Ordering::Greater,
                Opcode::TestLessEqual => ordering != core::cmp::Ordering::Greater,
                _ => ordering != core::cmp::Ordering::Less,
            };
            return Ok(Value::boolean(result));
        }
        if self.either_is_big_int(left_primitive, right_primitive) {
            let Some(ordering) = self.big_int_ordering(left_primitive, right_primitive)? else {
                return Ok(Value::FALSE);
            };
            let result = match opcode {
                Opcode::TestLess => ordering == core::cmp::Ordering::Less,
                Opcode::TestGreater => ordering == core::cmp::Ordering::Greater,
                Opcode::TestLessEqual => ordering != core::cmp::Ordering::Greater,
                _ => ordering != core::cmp::Ordering::Less,
            };
            return Ok(Value::boolean(result));
        }
        let left_number = self.coerce_to_number(left_primitive)?;
        let right_number = self.coerce_to_number(right_primitive)?;
        let comparison = value::compare_numbers(left_number, right_number);
        if matches!(comparison, value::Comparison::Undefined) {
            return Ok(Value::FALSE);
        }
        let result = match opcode {
            Opcode::TestLess => matches!(comparison, value::Comparison::Less),
            Opcode::TestGreater => matches!(comparison, value::Comparison::Greater),
            Opcode::TestLessEqual => !matches!(comparison, value::Comparison::Greater),
            _ => !matches!(comparison, value::Comparison::Less),
        };
        Ok(Value::boolean(result))
    }

    pub(super) fn instance_of(&mut self, left: Value, right: Value) -> Result<bool, Completion> {
        // `Symbol.hasInstance` decides first when the right side carries one:
        // whatever it answers, coerced to a boolean, is the result.
        if right.is_object() {
            let method = self.get_property(right, Key::Symbol(self.realm.has_instance_symbol))?;
            if !method.is_nullish() {
                if !self.is_callable_value(method) {
                    return Err(self.throw_type_error());
                }
                let answer = self.call_value(method, right, &[left])?;
                return self.coerce_to_boolean(answer);
            }
        }
        if !right.is_object() || !object::is_callable(self.heap, right.as_handle()).unwrap_or(false)
        {
            return Err(self.throw_type_error());
        }
        if !left.is_object() {
            return Ok(false);
        }
        let key = self.ascii_key(b"prototype")?;
        let prototype = self.get_property(right, key)?;
        if !prototype.is_object() {
            return Err(self.throw_type_error());
        }
        let mut current =
            object::prototype(self.heap, left.as_handle()).map_err(|_| Completion::MALFORMED)?;
        let mut depth = 0u32;
        while current.is_object() {
            if current.as_handle() == prototype.as_handle() {
                return Ok(true);
            }
            depth += 1;
            if depth > object::MAX_PROTOTYPE_DEPTH {
                return Err(Completion::MALFORMED);
            }
            current = object::prototype(self.heap, current.as_handle())
                .map_err(|_| Completion::MALFORMED)?;
        }
        Ok(false)
    }

    /// The string a value is, as a handle, for the methods that work on one.
    pub(super) fn string_handle(&mut self, value: Value) -> Result<Handle, Completion> {
        let text = self.coerce_to_string(value)?;
        Ok(text.as_handle())
    }

    /// The receiver a method on a primitive was called with, unwrapped where it
    /// is an object built around one.
    /// The primitive `this` a wrapper method demands, of exactly `tag`:
    /// anything else is the TypeError the specification's thisValue steps
    /// throw — which is also what keeps `toString` on a plain object from
    /// coercing itself forever.
    pub(super) fn this_primitive_of(&mut self, this: Value, tag: Tag) -> Result<Value, Completion> {
        let value = self.primitive_this(this)?;
        if value.tag() as u8 == tag as u8 {
            Ok(value)
        } else {
            Err(self.throw_type_error())
        }
    }

    pub(super) fn primitive_this(&mut self, this: Value) -> Result<Value, Completion> {
        if !this.is_object() {
            return Ok(this);
        }
        match object::wrapper_value(self.heap, this.as_handle()) {
            Ok(Some(value)) => Ok(value),
            _ => Ok(this),
        }
    }

    /// An object for a value: itself where it is one, and a wrapper around it
    /// where it is a primitive.
    pub(super) fn coerce_to_object(&mut self, value: Value) -> Result<Value, Completion> {
        if value.is_object() {
            return Ok(value);
        }
        let prototype = match value.tag() {
            Tag::String => self.realm.string_prototype,
            Tag::Number => self.realm.number_prototype,
            Tag::Boolean => self.realm.boolean_prototype,
            Tag::Symbol => self.realm.symbol_prototype,
            Tag::BigInt => self.realm.big_int_prototype,
            _ => return Err(self.throw_type_error()),
        };
        let handle = object::create_wrapper(self.heap, Value::object(prototype), value)
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        if matches!(value.tag(), Tag::String) {
            // A string wrapper carries the length its primitive has.
            let length = string::length(self.heap, value.as_handle())
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            let key = self.ascii_key(b"length")?;
            object::define_own_property(
                self.heap,
                handle,
                key,
                Descriptor::data(
                    Value::number(crate::softfloat::from_u64(u64::from(length))),
                    0,
                ),
            )
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        Ok(Value::object(handle))
    }

    /// `Symbol(description)` as text, which is the only way to see one.
    pub(super) fn symbol_text(&mut self, value: Value) -> Result<Value, Completion> {
        if !matches!(value.tag(), Tag::Symbol) {
            return Err(self.throw_type_error());
        }
        let open = self.ascii_string(b"Symbol(")?;
        let close = self.ascii_string(b")")?;
        let body = string::concat(self.heap, open.as_handle(), value.as_handle())
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let text = string::concat(self.heap, body, close.as_handle())
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(Value::string(text))
    }

    /// The Number a string denotes when a program asks for it by name.
    pub(super) fn parse_number(
        &mut self,
        text: Handle,
        radix: u32,
        float: bool,
    ) -> Result<Value, Completion> {
        let length =
            string::length(self.heap, text).map_err(|_| Completion::HEAP_EXHAUSTED)? as usize;
        let mut units = [0u16; 128];
        let room = length.min(units.len());
        string::copy_units(self.heap, text, units.get_mut(..room).unwrap_or(&mut []))
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let units = units.get(..room).unwrap_or(&[]);
        if float {
            return Ok(Value::number(crate::numeric::parse_float_prefix(units)));
        }
        Ok(Value::number(crate::numeric::parse_int_prefix(
            units, radix,
        )))
    }

    // BigInts. A BigInt is exact, so it never mixes with a Number in
    // arithmetic: the specification makes that a type error rather than a
    // conversion, because either direction would lose something.

    /// ToNumeric: the primitive first, kept when it is a BigInt, a Number
    /// otherwise — with a symbol refused as the TypeError it is.
    pub(super) fn numeric_value_of(&mut self, value: Value) -> Result<Value, Completion> {
        let primitive = if value.is_object() {
            self.coerce_to_primitive(value, Hint::Number)?
        } else {
            value
        };
        if matches!(primitive.tag(), Tag::BigInt) {
            return Ok(primitive);
        }
        Ok(Value::number(self.coerce_to_number(primitive)?))
    }

    /// One code unit of a string, or zero past its end.
    pub(super) fn string_unit(&self, handle: Handle, at: usize) -> Result<u16, Completion> {
        Ok(
            string::unit_at(self.heap, handle, u32::try_from(at).unwrap_or(u32::MAX))
                .map_err(|_| Completion::HEAP_EXHAUSTED)?
                .unwrap_or(0),
        )
    }

    /// `Array.prototype.join`: each element's string, with a separator, where
    /// `null` and `undefined` contribute nothing.
    pub(super) fn join_array(
        &mut self,
        array: Value,
        separator: Option<Value>,
    ) -> Result<Value, Completion> {
        if !array.is_object() {
            return Err(self.throw_type_error());
        }
        let length_key = self.ascii_key(b"length")?;
        let length_value = self.get_property(array, length_key)?;
        let length = value::to_uint32(self.coerce_to_number(length_value)?);
        let separator = match separator {
            Some(value) => value,
            None => self.ascii_string(b",")?,
        };

        let mut result = self.ascii_string(b"")?;
        let mut index = 0u32;
        while index < length {
            if index > 0 {
                result = self.concat_values(result, separator)?;
            }
            let element = self.get_property(array, Key::Index(index))?;
            if !element.is_nullish() {
                let text = self.coerce_to_string(element)?;
                result = self.concat_values(result, text)?;
            }
            index += 1;
        }
        Ok(result)
    }

    pub(super) fn concat_values(&mut self, left: Value, right: Value) -> Result<Value, Completion> {
        let joined = string::concat(self.heap, left.as_handle(), right.as_handle())
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(Value::string(joined))
    }

    /// ToIndex: an integer in 0..=2^32-1, or a RangeError.
    pub(super) fn coerce_to_index(&mut self, value: Value) -> Result<u32, Completion> {
        if value.is_undefined() {
            return Ok(0);
        }
        let wanted = value::truncate(self.coerce_to_number(value)?);
        if !(0.0..=4_294_967_295.0).contains(&wanted) {
            return Err(self.throw_error_of(ErrorKind::Range));
        }
        Ok(wanted as u32)
    }

    /// OrdinarySet through a primitive's prototype chain: a setter runs with
    /// the primitive as receiver, a proxy's `set` trap sees the write, and
    /// anything else answers false — nothing was taken.
    pub(super) fn primitive_write(
        &mut self,
        target: Value,
        key: Key,
        value: Value,
    ) -> Result<bool, Completion> {
        let mut current = match target.tag() {
            Tag::String => Value::object(self.realm.string_prototype),
            Tag::Number => Value::object(self.realm.number_prototype),
            Tag::Boolean => Value::object(self.realm.boolean_prototype),
            Tag::Symbol => Value::object(self.realm.symbol_prototype),
            Tag::BigInt => Value::object(self.realm.big_int_prototype),
            _ => return Ok(false),
        };
        let mut depth = 0u32;
        while current.is_object() && depth < object::MAX_PROTOTYPE_DEPTH {
            let handle = current.as_handle();
            if object::exotic_kind(self.heap, handle).unwrap_or(0) == object::exotic::PROXY {
                let (proxy_target, handler) = self.proxy_parts(current)?;
                let trap = self.proxy_trap(handler, b"set")?;
                if trap.is_undefined() {
                    current = proxy_target;
                    depth += 1;
                    continue;
                }
                let name = self.key_to_value(key)?;
                let answer =
                    self.call_value(trap, handler, &[proxy_target, name, value, target])?;
                return self.coerce_to_boolean(answer);
            }
            let own = object::get_own_property(self.heap, handle, key)
                .map_err(|_| Completion::MALFORMED)?;
            if let Some(descriptor) = own {
                if matches!(descriptor.kind, object::DescriptorKind::Accessor)
                    && descriptor.setter.is_object()
                {
                    self.call_value(descriptor.setter, target, &[value])?;
                    return Ok(true);
                }
                return Ok(false);
            }
            current = object::prototype(self.heap, handle).map_err(|_| Completion::MALFORMED)?;
            depth += 1;
        }
        Ok(false)
    }

    /// The bitwise and shift operators over the integer conversions, or over BigInts.
    pub(super) fn op_bitwise(
        &mut self,
        frame: &Frame,
        opcode: Opcode,
        operands: &[u32; 3],
    ) -> Result<Flow, Completion> {
        let left_value = self.register(frame, operands[0]);
        let right_value = self.accumulator;
        let left_value = self.numeric_value_of(left_value)?;
        let right_value = self.numeric_value_of(right_value)?;
        if self.either_is_big_int(left_value, right_value) {
            self.accumulator = self.big_int_bitwise(opcode, left_value, right_value)?;
            return Ok(Flow::Continue);
        }
        let left = self.coerce_to_number(left_value)?;
        let right = self.coerce_to_number(right_value)?;
        let result = match opcode {
            Opcode::BitAnd => value::bitwise_and(left, right),
            Opcode::BitOr => value::bitwise_or(left, right),
            Opcode::BitXor => value::bitwise_xor(left, right),
            Opcode::ShiftLeft => value::shift_left(left, right),
            Opcode::ShiftRight => value::shift_right(left, right),
            _ => value::unsigned_shift_right(left, right),
        };
        self.accumulator = Value::number(result);

        Ok(Flow::Continue)
    }

    /// The numeric unary operators, over a Number or a BigInt.
    pub(super) fn op_unary_numeric(&mut self, opcode: Opcode) -> Step {
        let number = self.coerce_to_number(self.accumulator)?;
        let result = match opcode {
            Opcode::Inc => value::add(number, 1.0),
            Opcode::Dec => value::subtract(number, 1.0),
            Opcode::Negate => value::unary_minus(number),
            Opcode::BitNot => value::bitwise_not(number),
            _ => number,
        };
        self.accumulator = Value::number(result);

        Ok(())
    }

    /// The arithmetic operators other than `+`, over Numbers or BigInts.
    pub(super) fn op_arithmetic(
        &mut self,
        frame: &Frame,
        opcode: Opcode,
        operands: &[u32; 3],
    ) -> Result<Flow, Completion> {
        let left_value = self.register(frame, operands[0]);
        let right_value = self.accumulator;
        // ToNumeric completes for the left operand — wrapper
        // unwrapped, symbol refused — before the right's begins.
        let left_value = self.numeric_value_of(left_value)?;
        let right_value = self.numeric_value_of(right_value)?;
        if self.either_is_big_int(left_value, right_value) {
            self.accumulator = self.big_int_arithmetic(opcode, left_value, right_value)?;
            return Ok(Flow::Continue);
        }
        let left = left_value.as_number();
        let right = right_value.as_number();
        let result = match opcode {
            Opcode::Sub => value::subtract(left, right),
            Opcode::Mul => value::multiply(left, right),
            Opcode::Div => value::divide(left, right),
            Opcode::Mod => value::remainder(left, right),
            _ => crate::numeric::power(left, right),
        };
        self.accumulator = Value::number(result);

        Ok(Flow::Continue)
    }

    /// `typeof`, as a string.
    pub(super) fn op_typeof(&mut self) -> Step {
        let value = self.accumulator;
        let callable =
            value.is_object() && object::is_callable(self.heap, value.as_handle()).unwrap_or(false);
        let name: &[u8] = match value::type_of(&value, callable) {
            value::TypeOf::Undefined => b"undefined",
            value::TypeOf::Object => b"object",
            value::TypeOf::Boolean => b"boolean",
            value::TypeOf::Number => b"number",
            value::TypeOf::String => b"string",
            value::TypeOf::Symbol => b"symbol",
            value::TypeOf::BigInt => b"bigint",
            value::TypeOf::Function => b"function",
        };
        self.accumulator = self.ascii_string(name)?;

        Ok(())
    }
}
