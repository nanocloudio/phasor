//! `JSON.parse` and `JSON.stringify`.

use super::*;

/// Nesting `JSON.parse` and `JSON.stringify` admit before refusing with a
/// RangeError, which bounds the host stack they recurse on.
pub(super) const JSON_DEPTH: u32 = 128;

/// What one `JSON.stringify` carries down its recursion.
pub(super) struct JsonState {
    replacer_function: Value,
    property_list: Value,
    gap: [u16; 10],
    gap_length: usize,
    stack: [Handle; JSON_DEPTH as usize],
    depth: usize,
}

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    // JSON.parse

    /// `JSON.parse(text, reviver)`.
    pub(super) fn json_parse(&mut self, text: Value, reviver: Value) -> Result<Value, Completion> {
        let source = self.string_handle(text)?;
        let length = string::length(self.heap, source).map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let mut at = 0u32;
        self.json_skip_space(source, length, &mut at)?;
        let value = self.json_value(source, length, &mut at, 0)?;
        self.json_skip_space(source, length, &mut at)?;
        if at != length {
            return Err(self.throw_error_of(ErrorKind::Syntax));
        }
        if !self.is_callable_value(reviver) {
            return Ok(value);
        }
        let root = object::create(self.heap, Value::object(self.realm.object_prototype))
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let empty = self.ascii_key(b"")?;
        object::define_own_property(
            self.heap,
            root,
            empty,
            Descriptor::data(value, attribute::DEFAULT),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        self.json_internalize(Value::object(root), empty, reviver, 0)
    }

    pub(super) fn json_unit(&mut self, source: Handle, at: u32) -> Result<Option<u16>, Completion> {
        string::unit_at(self.heap, source, at).map_err(|_| Completion::HEAP_EXHAUSTED)
    }

    pub(super) fn json_skip_space(
        &mut self,
        source: Handle,
        length: u32,
        at: &mut u32,
    ) -> Result<(), Completion> {
        while *at < length {
            match self.json_unit(source, *at)? {
                Some(0x20 | 0x09 | 0x0A | 0x0D) => *at += 1,
                _ => break,
            }
        }
        Ok(())
    }

    pub(super) fn json_expect_word(
        &mut self,
        source: Handle,
        length: u32,
        at: &mut u32,
        word: &[u8],
    ) -> Result<(), Completion> {
        for &byte in word {
            if *at >= length || self.json_unit(source, *at)? != Some(u16::from(byte)) {
                return Err(self.throw_error_of(ErrorKind::Syntax));
            }
            *at += 1;
        }
        Ok(())
    }

    pub(super) fn json_value(
        &mut self,
        source: Handle,
        length: u32,
        at: &mut u32,
        depth: u32,
    ) -> Result<Value, Completion> {
        if depth > JSON_DEPTH {
            return Err(self.throw_error_of(ErrorKind::Range));
        }
        let Some(unit) = self.json_unit(source, *at)? else {
            return Err(self.throw_error_of(ErrorKind::Syntax));
        };
        match unit {
            0x7B => {
                *at += 1;
                let made = object::create(self.heap, Value::object(self.realm.object_prototype))
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                self.json_skip_space(source, length, at)?;
                if self.json_unit(source, *at)? == Some(0x7D) {
                    *at += 1;
                    return Ok(Value::object(made));
                }
                loop {
                    self.json_skip_space(source, length, at)?;
                    if self.json_unit(source, *at)? != Some(0x22) {
                        return Err(self.throw_error_of(ErrorKind::Syntax));
                    }
                    let name = self.json_string(source, length, at)?;
                    self.json_skip_space(source, length, at)?;
                    if self.json_unit(source, *at)? != Some(0x3A) {
                        return Err(self.throw_error_of(ErrorKind::Syntax));
                    }
                    *at += 1;
                    self.json_skip_space(source, length, at)?;
                    let value = self.json_value(source, length, at, depth + 1)?;
                    let key = self.coerce_to_key(name)?;
                    object::define_own_property(
                        self.heap,
                        made,
                        key,
                        Descriptor::data(value, attribute::DEFAULT),
                    )
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                    self.json_skip_space(source, length, at)?;
                    match self.json_unit(source, *at)? {
                        Some(0x2C) => *at += 1,
                        Some(0x7D) => {
                            *at += 1;
                            return Ok(Value::object(made));
                        }
                        _ => return Err(self.throw_error_of(ErrorKind::Syntax)),
                    }
                }
            }
            0x5B => {
                *at += 1;
                let array = self.create_array()?;
                self.json_skip_space(source, length, at)?;
                if self.json_unit(source, *at)? == Some(0x5D) {
                    *at += 1;
                    return Ok(array);
                }
                loop {
                    self.json_skip_space(source, length, at)?;
                    let value = self.json_value(source, length, at, depth + 1)?;
                    self.append_element(array, Some(value))?;
                    self.json_skip_space(source, length, at)?;
                    match self.json_unit(source, *at)? {
                        Some(0x2C) => *at += 1,
                        Some(0x5D) => {
                            *at += 1;
                            return Ok(array);
                        }
                        _ => return Err(self.throw_error_of(ErrorKind::Syntax)),
                    }
                }
            }
            0x22 => self.json_string(source, length, at),
            0x74 => {
                self.json_expect_word(source, length, at, b"true")?;
                Ok(Value::TRUE)
            }
            0x66 => {
                self.json_expect_word(source, length, at, b"false")?;
                Ok(Value::FALSE)
            }
            0x6E => {
                self.json_expect_word(source, length, at, b"null")?;
                Ok(Value::NULL)
            }
            0x2D | 0x30..=0x39 => self.json_number(source, length, at),
            _ => Err(self.throw_error_of(ErrorKind::Syntax)),
        }
    }

    pub(super) fn json_number(
        &mut self,
        source: Handle,
        length: u32,
        at: &mut u32,
    ) -> Result<Value, Completion> {
        let mut digits = [0u16; 512];
        let mut count = 0usize;
        let mut take = |unit: u16, count: &mut usize| -> bool {
            if *count < digits.len() {
                digits[*count] = unit;
                *count += 1;
                true
            } else {
                false
            }
        };
        let mut unit = self.json_unit(source, *at)?;
        if unit == Some(0x2D) {
            take(0x2D, &mut count);
            *at += 1;
            unit = self.json_unit(source, *at)?;
        }
        // The integer part: a lone zero, or a nonzero digit and any more.
        match unit {
            Some(0x30) => {
                take(0x30, &mut count);
                *at += 1;
            }
            Some(0x31..=0x39) => {
                while let Some(digit @ 0x30..=0x39) = self.json_unit(source, *at)? {
                    if !take(digit, &mut count) {
                        return Err(self.throw_error_of(ErrorKind::Range));
                    }
                    *at += 1;
                }
            }
            _ => return Err(self.throw_error_of(ErrorKind::Syntax)),
        }
        if self.json_unit(source, *at)? == Some(0x2E) {
            take(0x2E, &mut count);
            *at += 1;
            let mut any = false;
            while let Some(digit @ 0x30..=0x39) = self.json_unit(source, *at)? {
                if !take(digit, &mut count) {
                    return Err(self.throw_error_of(ErrorKind::Range));
                }
                *at += 1;
                any = true;
            }
            if !any {
                return Err(self.throw_error_of(ErrorKind::Syntax));
            }
        }
        if matches!(self.json_unit(source, *at)?, Some(0x65 | 0x45)) {
            take(0x65, &mut count);
            *at += 1;
            if let Some(sign @ (0x2B | 0x2D)) = self.json_unit(source, *at)? {
                take(sign, &mut count);
                *at += 1;
            }
            let mut any = false;
            while let Some(digit @ 0x30..=0x39) = self.json_unit(source, *at)? {
                if !take(digit, &mut count) {
                    return Err(self.throw_error_of(ErrorKind::Range));
                }
                *at += 1;
                any = true;
            }
            if !any {
                return Err(self.throw_error_of(ErrorKind::Syntax));
            }
        }
        let _ = length;
        Ok(Value::number(value::string_to_number(&digits[..count])))
    }

    /// A JSON string, its opening quote at `at`.
    pub(super) fn json_string(
        &mut self,
        source: Handle,
        length: u32,
        at: &mut u32,
    ) -> Result<Value, Completion> {
        *at += 1;
        let mut chunk = [0u16; 256];
        let mut filled = 0usize;
        let mut built: Option<Handle> = None;
        loop {
            if *at >= length {
                return Err(self.throw_error_of(ErrorKind::Syntax));
            }
            let Some(unit) = self.json_unit(source, *at)? else {
                return Err(self.throw_error_of(ErrorKind::Syntax));
            };
            *at += 1;
            let out = match unit {
                0x22 => break,
                0x5C => {
                    let Some(escape) = self.json_unit(source, *at)? else {
                        return Err(self.throw_error_of(ErrorKind::Syntax));
                    };
                    *at += 1;
                    match escape {
                        0x22 => 0x22,
                        0x5C => 0x5C,
                        0x2F => 0x2F,
                        0x62 => 0x08,
                        0x66 => 0x0C,
                        0x6E => 0x0A,
                        0x72 => 0x0D,
                        0x74 => 0x09,
                        0x75 => {
                            let mut code = 0u16;
                            for _ in 0..4 {
                                let Some(hex) = self.json_unit(source, *at)? else {
                                    return Err(self.throw_error_of(ErrorKind::Syntax));
                                };
                                *at += 1;
                                let digit = match hex {
                                    0x30..=0x39 => hex - 0x30,
                                    0x41..=0x46 => hex - 0x41 + 10,
                                    0x61..=0x66 => hex - 0x61 + 10,
                                    _ => return Err(self.throw_error_of(ErrorKind::Syntax)),
                                };
                                code = (code << 4) | digit;
                            }
                            code
                        }
                        _ => return Err(self.throw_error_of(ErrorKind::Syntax)),
                    }
                }
                0x00..=0x1F => return Err(self.throw_error_of(ErrorKind::Syntax)),
                other => other,
            };
            if filled == chunk.len() {
                let piece = self.make_string(&chunk)?.as_handle();
                built = Some(match built {
                    Some(so_far) => string::concat(self.heap, so_far, piece)
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?,
                    None => piece,
                });
                filled = 0;
            }
            chunk[filled] = out;
            filled += 1;
        }
        let piece = self.make_string(&chunk[..filled])?.as_handle();
        let whole = match built {
            Some(so_far) => {
                string::concat(self.heap, so_far, piece).map_err(|_| Completion::HEAP_EXHAUSTED)?
            }
            None => piece,
        };
        Ok(Value::string(whole))
    }

    /// InternalizeJSONProperty: the reviver over every value, leaves first.
    pub(super) fn json_internalize(
        &mut self,
        holder: Value,
        key: Key,
        reviver: Value,
        depth: u32,
    ) -> Result<Value, Completion> {
        if depth > JSON_DEPTH {
            return Err(self.throw_error_of(ErrorKind::Range));
        }
        let value = self.get_property(holder, key)?;
        if value.is_object() {
            if self.is_array(value)? {
                let length = self.length_of(value)?;
                let mut index = 0u32;
                while index < length {
                    let element =
                        self.json_internalize(value, Key::Index(index), reviver, depth + 1)?;
                    if element.is_undefined() {
                        self.delete_property(value, Key::Index(index))?;
                    } else {
                        object::define_own_property(
                            self.heap,
                            value.as_handle(),
                            Key::Index(index),
                            Descriptor::data(element, attribute::DEFAULT),
                        )
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                    }
                    index += 1;
                }
            } else {
                let mut keys = [Key::Index(0); MAX_OWN_KEYS];
                let count = object::own_keys(self.heap, value.as_handle(), &mut keys)
                    .map_err(|_| Completion::MALFORMED)?;
                for &name in keys.iter().take(count) {
                    if matches!(name, Key::Symbol(_)) || !self.is_enumerable(value, name)? {
                        continue;
                    }
                    let element = self.json_internalize(value, name, reviver, depth + 1)?;
                    if element.is_undefined() {
                        self.delete_property(value, name)?;
                    } else {
                        object::define_own_property(
                            self.heap,
                            value.as_handle(),
                            name,
                            Descriptor::data(element, attribute::DEFAULT),
                        )
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                    }
                }
            }
        }
        let name = self.key_to_value(key)?;
        let name = self.coerce_to_string(name)?;
        self.call_value(reviver, holder, &[name, value])
    }

    // JSON.stringify

    /// `JSON.stringify(value, replacer, space)`.
    pub(super) fn json_stringify(
        &mut self,
        value: Value,
        replacer: Value,
        space: Value,
    ) -> Result<Value, Completion> {
        let mut state = JsonState {
            replacer_function: Value::UNDEFINED,
            property_list: Value::UNDEFINED,
            gap: [0u16; 10],
            gap_length: 0,
            stack: [Handle::new(0, 0); JSON_DEPTH as usize],
            depth: 0,
        };
        if replacer.is_object() {
            if self.is_callable_value(replacer) {
                state.replacer_function = replacer;
            } else if self.is_array(replacer)? {
                // A property list: strings and numbers, wrapped or not, once
                // each, in order.
                let list = self.create_array()?;
                let length = self.length_of(replacer)?;
                let mut index = 0u32;
                while index < length {
                    let entry = self.element(replacer, index)?;
                    index += 1;
                    let item = match entry.tag() {
                        Tag::String => Some(entry),
                        Tag::Number => Some(self.coerce_to_string(entry)?),
                        Tag::Object => match object::wrapper_value(self.heap, entry.as_handle()) {
                            Ok(Some(inner)) if matches!(inner.tag(), Tag::String | Tag::Number) => {
                                Some(self.coerce_to_string(entry)?)
                            }
                            _ => None,
                        },
                        _ => None,
                    };
                    let Some(item) = item else {
                        continue;
                    };
                    let count = self.length_of(list)?;
                    let mut seen = false;
                    let mut scan = 0u32;
                    while scan < count {
                        let held = self.element(list, scan)?;
                        if self.strict_equals(held, item)? {
                            seen = true;
                            break;
                        }
                        scan += 1;
                    }
                    if !seen {
                        self.append_element(list, Some(item))?;
                    }
                }
                state.property_list = list;
            }
        }
        // The gap: up to ten spaces for a number, the first ten units of a
        // string, wrappers unwrapped first.
        let space = if space.is_object() {
            match object::wrapper_value(self.heap, space.as_handle()) {
                Ok(Some(inner)) if matches!(inner.tag(), Tag::Number) => {
                    Value::number(self.coerce_to_number(space)?)
                }
                Ok(Some(inner)) if matches!(inner.tag(), Tag::String) => {
                    self.coerce_to_string(space)?
                }
                _ => space,
            }
        } else {
            space
        };
        if matches!(space.tag(), Tag::Number) {
            let count = value::truncate(space.as_number()).clamp(0.0, 10.0) as usize;
            let mut index = 0usize;
            while index < count {
                state.gap[index] = 0x20;
                index += 1;
            }
            state.gap_length = count;
        } else if matches!(space.tag(), Tag::String) {
            let handle = space.as_handle();
            let length =
                string::length(self.heap, handle).map_err(|_| Completion::HEAP_EXHAUSTED)?;
            let count = (length as usize).min(10);
            let mut index = 0usize;
            while index < count {
                state.gap[index] = string::unit_at(self.heap, handle, index as u32)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?
                    .unwrap_or(0x20);
                index += 1;
            }
            state.gap_length = count;
        }
        let wrapper = object::create(self.heap, Value::object(self.realm.object_prototype))
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let empty = self.ascii_key(b"")?;
        object::define_own_property(
            self.heap,
            wrapper,
            empty,
            Descriptor::data(value, attribute::DEFAULT),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let empty_name = self.ascii_string(b"")?;
        match self.json_property(empty_name, Value::object(wrapper), &mut state, 0)? {
            Some(text) => Ok(text),
            None => Ok(Value::UNDEFINED),
        }
    }

    /// SerializeJSONProperty: the text a property serialises to, or nothing
    /// where it is left out.
    pub(super) fn json_property(
        &mut self,
        name: Value,
        holder: Value,
        state: &mut JsonState,
        indent: usize,
    ) -> Result<Option<Value>, Completion> {
        let key = self.coerce_to_key(name)?;
        let mut value = self.get_property(holder, key)?;
        if value.is_object() || matches!(value.tag(), Tag::BigInt) {
            let to_json_key = self.ascii_key(b"toJSON")?;
            let to_json = self.get_property(value, to_json_key)?;
            if self.is_callable_value(to_json) {
                value = self.call_value(to_json, value, &[name])?;
            }
        }
        if !state.replacer_function.is_undefined() {
            let replacer = state.replacer_function;
            value = self.call_value(replacer, holder, &[name, value])?;
        }
        if value.is_object() {
            if let Ok(Some(inner)) = object::wrapper_value(self.heap, value.as_handle()) {
                match inner.tag() {
                    Tag::Number => value = Value::number(self.coerce_to_number(value)?),
                    Tag::String => value = self.coerce_to_string(value)?,
                    Tag::Boolean | Tag::BigInt => value = inner,
                    _ => {}
                }
            }
        }
        match value.tag() {
            Tag::Null => return self.ascii_string(b"null").map(Some),
            Tag::Boolean => {
                return self
                    .ascii_string(if value.as_boolean() {
                        b"true"
                    } else {
                        b"false"
                    })
                    .map(Some);
            }
            Tag::String => return self.json_quote(value.as_handle()).map(Some),
            Tag::Number => {
                if value.as_number().is_finite() {
                    return self.coerce_to_string(value).map(Some);
                }
                return self.ascii_string(b"null").map(Some);
            }
            Tag::BigInt => return Err(self.throw_type_error()),
            _ => {}
        }
        if value.is_object() && !self.is_callable_value(value) {
            if self.is_array(value)? {
                return self.json_array(value, state, indent).map(Some);
            }
            return self.json_object(value, state, indent).map(Some);
        }
        Ok(None)
    }

    /// Enter a value's serialisation, refusing a cycle and a depth beyond
    /// the admitted bound.
    pub(super) fn json_enter(
        &mut self,
        value: Value,
        state: &mut JsonState,
    ) -> Result<(), Completion> {
        let handle = value.as_handle();
        for &seen in state.stack.iter().take(state.depth) {
            if seen == handle {
                return Err(self.throw_type_error());
            }
        }
        if state.depth >= state.stack.len() {
            return Err(self.throw_error_of(ErrorKind::Range));
        }
        state.stack[state.depth] = handle;
        state.depth += 1;
        Ok(())
    }

    pub(super) fn json_join(&mut self, so_far: Value, piece: Value) -> Result<Value, Completion> {
        string::concat(self.heap, so_far.as_handle(), piece.as_handle())
            .map(Value::string)
            .map_err(|_| Completion::HEAP_EXHAUSTED)
    }

    /// A line break followed by the indentation, where a gap is set.
    pub(super) fn json_break(
        &mut self,
        state: &JsonState,
        indent: usize,
    ) -> Result<Value, Completion> {
        if state.gap_length == 0 {
            return self.ascii_string(b"");
        }
        let mut units = [0u16; 512];
        units[0] = 0x0A;
        let mut written = 1usize;
        for _ in 0..indent {
            for &unit in &state.gap[..state.gap_length] {
                if written < units.len() {
                    units[written] = unit;
                    written += 1;
                }
            }
        }
        self.make_string(&units[..written])
    }

    pub(super) fn json_object(
        &mut self,
        value: Value,
        state: &mut JsonState,
        indent: usize,
    ) -> Result<Value, Completion> {
        self.json_enter(value, state)?;
        let mut keys = [Key::Index(0); MAX_OWN_KEYS];
        let mut names = [Value::UNDEFINED; MAX_OWN_KEYS];
        let mut count = 0usize;
        if state.property_list.is_object() {
            let list = state.property_list;
            let length = self.length_of(list)?;
            let mut index = 0u32;
            while index < length && count < names.len() {
                names[count] = self.element(list, index)?;
                count += 1;
                index += 1;
            }
        } else {
            let found = object::own_keys(self.heap, value.as_handle(), &mut keys)
                .map_err(|_| Completion::MALFORMED)?;
            for &key in keys.iter().take(found) {
                if matches!(key, Key::Symbol(_)) || !self.is_enumerable(value, key)? {
                    continue;
                }
                if count < names.len() {
                    let name = self.key_to_value(key)?;
                    names[count] = self.coerce_to_string(name)?;
                    count += 1;
                }
            }
        }
        let mut out = self.ascii_string(b"{")?;
        let mut any = false;
        let separator = if state.gap_length == 0 {
            b":" as &[u8]
        } else {
            b": "
        };
        for &name in names.iter().take(count) {
            let Some(text) = self.json_property(name, value, state, indent + 1)? else {
                continue;
            };
            if any {
                let comma = self.ascii_string(b",")?;
                out = self.json_join(out, comma)?;
            }
            let brk = self.json_break(state, indent + 1)?;
            out = self.json_join(out, brk)?;
            let quoted = self.json_quote(name.as_handle())?;
            out = self.json_join(out, quoted)?;
            let colon = self.ascii_string(separator)?;
            out = self.json_join(out, colon)?;
            out = self.json_join(out, text)?;
            any = true;
        }
        if any {
            let brk = self.json_break(state, indent)?;
            out = self.json_join(out, brk)?;
        }
        let close = self.ascii_string(b"}")?;
        out = self.json_join(out, close)?;
        state.depth -= 1;
        Ok(out)
    }

    pub(super) fn json_array(
        &mut self,
        value: Value,
        state: &mut JsonState,
        indent: usize,
    ) -> Result<Value, Completion> {
        self.json_enter(value, state)?;
        let length = self.length_of(value)?;
        let mut out = self.ascii_string(b"[")?;
        let mut index = 0u32;
        while index < length {
            if index > 0 {
                let comma = self.ascii_string(b",")?;
                out = self.json_join(out, comma)?;
            }
            let brk = self.json_break(state, indent + 1)?;
            out = self.json_join(out, brk)?;
            let name = self.coerce_to_string(Value::number(f64::from(index)))?;
            let text = match self.json_property(name, value, state, indent + 1)? {
                Some(text) => text,
                None => self.ascii_string(b"null")?,
            };
            out = self.json_join(out, text)?;
            index += 1;
        }
        if length > 0 {
            let brk = self.json_break(state, indent)?;
            out = self.json_join(out, brk)?;
        }
        let close = self.ascii_string(b"]")?;
        out = self.json_join(out, close)?;
        state.depth -= 1;
        Ok(out)
    }

    /// QuoteJSONString: the string in quotes, its escapes written, and a
    /// lone surrogate written as an escape so the text is well formed.
    pub(super) fn json_quote(&mut self, text: Handle) -> Result<Value, Completion> {
        let length = string::length(self.heap, text).map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let mut chunk = [0u16; 256];
        let mut filled = 0usize;
        chunk[0] = 0x22;
        filled += 1;
        let mut out: Option<Handle> = None;
        let mut index = 0u32;
        while index <= length {
            let mut piece = [0u16; 8];
            let count;
            if index == length {
                piece[0] = 0x22;
                count = 1;
            } else {
                let unit = string::unit_at(self.heap, text, index)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?
                    .unwrap_or(0);
                let escaped: Option<u8> = match unit {
                    0x08 => Some(b'b'),
                    0x09 => Some(b't'),
                    0x0A => Some(b'n'),
                    0x0C => Some(b'f'),
                    0x0D => Some(b'r'),
                    0x22 => Some(b'"'),
                    0x5C => Some(b'\\'),
                    _ => None,
                };
                if let Some(letter) = escaped {
                    piece[0] = 0x5C;
                    piece[1] = u16::from(letter);
                    count = 2;
                } else {
                    let lone = match unit {
                        0xD800..=0xDBFF => {
                            let next = string::unit_at(self.heap, text, index + 1)
                                .map_err(|_| Completion::HEAP_EXHAUSTED)?
                                .unwrap_or(0);
                            !(0xDC00..=0xDFFF).contains(&next)
                        }
                        0xDC00..=0xDFFF => {
                            let previous = if index == 0 {
                                0
                            } else {
                                string::unit_at(self.heap, text, index - 1)
                                    .map_err(|_| Completion::HEAP_EXHAUSTED)?
                                    .unwrap_or(0)
                            };
                            !(0xD800..=0xDBFF).contains(&previous)
                        }
                        _ => false,
                    };
                    if unit < 0x20 || lone {
                        piece[0] = 0x5C;
                        piece[1] = 0x75;
                        for (place, shift) in [(2usize, 12u32), (3, 8), (4, 4), (5, 0)] {
                            let digit = ((unit >> shift) & 0xF) as u8;
                            piece[place] = u16::from(if digit < 10 {
                                b'0' + digit
                            } else {
                                b'a' + digit - 10
                            });
                        }
                        count = 6;
                    } else {
                        piece[0] = unit;
                        count = 1;
                    }
                }
            }
            if filled + count > chunk.len() {
                let flushed = self.make_string(&chunk[..filled])?.as_handle();
                out = Some(match out {
                    Some(so_far) => string::concat(self.heap, so_far, flushed)
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?,
                    None => flushed,
                });
                filled = 0;
            }
            chunk[filled..filled + count].copy_from_slice(&piece[..count]);
            filled += count;
            index += 1;
        }
        let flushed = self.make_string(&chunk[..filled])?.as_handle();
        let whole = match out {
            Some(so_far) => string::concat(self.heap, so_far, flushed)
                .map_err(|_| Completion::HEAP_EXHAUSTED)?,
            None => flushed,
        };
        Ok(Value::string(whole))
    }
}
