//! Regular expressions: compiling a pattern, matching it, and the
//! replace and split built on a match.

use super::*;

/// Steps one match may take before the task is out of fuel.
pub(super) const REGEXP_FUEL: u32 = 1_000_000;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    // Regular expressions. A pattern is compiled once into a program of bytes,
    // and the program is held in the object the pattern made.

    /// Give the machine somewhere to match in. Without it, a program that uses
    /// a regular expression is told so rather than matching in storage it was
    /// never given.
    pub fn attach_regexp(
        &mut self,
        choices: &'a mut [crate::regexp::Choice],
        undo: &'a mut [(u8, u32)],
        subject: &'a mut [u16],
    ) {
        self.regexp_choices = Some(choices);
        self.regexp_undo = Some(undo);
        self.regexp_subject = Some(subject);
    }

    /// Build a regular expression from its pattern and flags.
    pub(super) fn create_regexp(
        &mut self,
        pattern: &[u16],
        flags: u8,
    ) -> Result<Value, Completion> {
        let mut code = [0u8; crate::regexp::MAX_PROGRAM];
        let (length, groups) = crate::regexp::compile(pattern, flags, &mut code)
            .map_err(|_| self.throw_error_of(ErrorKind::Syntax))?;
        // The program is held the way a string's units are held: it is bytes,
        // it holds no reference, and the collector already knows how to move
        // one of those.
        let mut units = [0u16; crate::regexp::MAX_PROGRAM + 2];
        units[0] = u16::from(groups);
        let mut index = 0usize;
        while index < length {
            units[index + 1] = u16::from(code[index]);
            index += 1;
        }
        let program = self.make_string(units.get(..length + 1).unwrap_or(&[]))?;
        let source = self.make_string(pattern)?;
        let handle = object::create_regexp(
            self.heap,
            Value::object(self.realm.regexp_prototype),
            program,
            flags,
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let value = Value::object(handle);
        let source_key = self.ascii_key(b"source")?;
        object::define_own_property(self.heap, handle, source_key, Descriptor::data(source, 0))
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let mut flag_units = [0u16; 8];
        let written = crate::regexp::flag_text(flags, &mut flag_units);
        let flag_text = self.make_string(flag_units.get(..written).unwrap_or(&[]))?;
        let flags_key = self.ascii_key(b"flags")?;
        object::define_own_property(self.heap, handle, flags_key, Descriptor::data(flag_text, 0))
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        for (name, bit) in [
            (&b"global"[..], crate::regexp::flag::GLOBAL),
            (&b"ignoreCase"[..], crate::regexp::flag::IGNORE_CASE),
            (&b"multiline"[..], crate::regexp::flag::MULTILINE),
            (&b"dotAll"[..], crate::regexp::flag::DOT_ALL),
            (&b"sticky"[..], crate::regexp::flag::STICKY),
        ] {
            let key = self.ascii_key(name)?;
            object::define_own_property(
                self.heap,
                handle,
                key,
                Descriptor::data(Value::boolean(flags & bit != 0), 0),
            )
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        let last_index_key = self.ascii_key(b"lastIndex")?;
        object::define_own_property(
            self.heap,
            handle,
            last_index_key,
            Descriptor::data(Value::number(0.0), attribute::WRITABLE),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(value)
    }

    /// Match `subject` with a regular expression from `start`, answering the
    /// slots it filled.
    pub(super) fn match_regexp(
        &mut self,
        regexp: Value,
        subject: Handle,
        start: u32,
        sticky: bool,
    ) -> Result<Option<crate::regexp::Slots>, Completion> {
        if !regexp.is_object() {
            return Err(self.throw_type_error());
        }
        let Some((program, flags)) = object::regexp_program(self.heap, regexp.as_handle())
            .map_err(|_| Completion::HEAP_EXHAUSTED)?
        else {
            return Err(self.throw_type_error());
        };
        if !program.is_string() {
            return Err(self.throw_type_error());
        }

        // The program and the subject are copied out of the heap, because the
        // matcher works on units and the heap may move under it.
        let program_length = string::length(self.heap, program.as_handle())
            .map_err(|_| Completion::HEAP_EXHAUSTED)? as usize;
        let mut code = [0u8; crate::regexp::MAX_PROGRAM];
        let mut units = [0u16; crate::regexp::MAX_PROGRAM + 2];
        if program_length > units.len() {
            return Err(self.throw_error_of(ErrorKind::Range));
        }
        string::copy_units(
            self.heap,
            program.as_handle(),
            units.get_mut(..program_length).unwrap_or(&mut []),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let groups = u8::try_from(units.first().copied().unwrap_or(0)).unwrap_or(0);
        let mut index = 1usize;
        while index < program_length && index - 1 < code.len() {
            code[index - 1] = u8::try_from(units[index]).unwrap_or(0);
            index += 1;
        }

        let subject_length =
            string::length(self.heap, subject).map_err(|_| Completion::HEAP_EXHAUSTED)? as usize;
        let Some(buffer) = self.regexp_subject.as_deref_mut() else {
            return Err(Completion::Terminated(Termination::NotImplemented));
        };
        if subject_length > buffer.len() {
            return Err(Completion::QUOTA_EXCEEDED);
        }
        string::copy_units(self.heap, subject, &mut buffer[..subject_length])
            .map_err(|_| Completion::MALFORMED)?;

        let program = crate::regexp::Program {
            code: code.get(..program_length.saturating_sub(1)).unwrap_or(&[]),
            groups,
            flags,
        };
        let (Some(choices), Some(undo), Some(subject_units)) = (
            self.regexp_choices.as_deref_mut(),
            self.regexp_undo.as_deref_mut(),
            self.regexp_subject.as_deref(),
        ) else {
            return Err(Completion::Terminated(Termination::NotImplemented));
        };
        let input = subject_units.get(..subject_length).unwrap_or(&[]);
        let mut matcher = crate::regexp::Matcher {
            choices,
            undo,
            halted: false,
        };
        // The match spends the task's own fuel, so what it may spend is what
        // the task has left, up to the ceiling one match is allowed.
        let budget = u32::try_from(self.fuel.min(u64::from(REGEXP_FUEL))).unwrap_or(REGEXP_FUEL);
        let mut fuel = budget;
        let mut at = start as usize;
        loop {
            if at > input.len() {
                return Ok(None);
            }
            if let Some(slots) = crate::regexp::run(&program, input, at, &mut matcher, &mut fuel) {
                self.fuel = self.fuel.saturating_sub(u64::from(budget - fuel));
                return Ok(Some(slots));
            }
            // The match did not fail: it was never decided. Saying "no match"
            // here would answer a question nobody could answer, and a program
            // reading that answer has no way to tell it from the truth.
            if matcher.halted {
                self.fuel = self.fuel.saturating_sub(u64::from(budget - fuel));
                return Err(Completion::Terminated(Termination::FuelExhausted));
            }
            if sticky {
                self.fuel = self.fuel.saturating_sub(u64::from(budget - fuel));
                return Ok(None);
            }
            // Unicode mode advances by code point: a start inside a
            // surrogate pair is not a start at all.
            if flags & crate::regexp::flag::UNICODE != 0
                && input
                    .get(at)
                    .is_some_and(|&unit| (0xD800..0xDC00).contains(&unit))
                && input
                    .get(at + 1)
                    .is_some_and(|&low| (0xDC00..0xE000).contains(&low))
            {
                at += 2;
            } else {
                at += 1;
            }
            if fuel == 0 {
                // Matching is charged to the task's fuel, so a pathological
                // pattern ends the task rather than the machine.
                self.fuel = 0;
                return Err(Completion::Terminated(Termination::FuelExhausted));
            }
            if at > input.len() {
                self.fuel = self.fuel.saturating_sub(u64::from(budget - fuel));
                return Ok(None);
            }
        }
    }

    /// `replace` with a pattern: every match where it is global, and the first
    /// otherwise. A `$` in the replacement text names part of the match.
    pub(super) fn replace_with_pattern(
        &mut self,
        subject: Handle,
        pattern: Value,
        replacement: Value,
    ) -> Result<Value, Completion> {
        let (global, sticky) = self.regexp_kind(pattern)?;
        let groups = self.regexp_groups(pattern);
        let length = string::length(self.heap, subject).map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let callable = self.is_callable_value(replacement);
        let mut result = self.ascii_string(b"")?.as_handle();
        let mut position = 0u32;
        let mut start = 0u32;
        loop {
            let Some(slots) = self.match_regexp(pattern, subject, start, sticky)? else {
                break;
            };
            let head = string::slice(self.heap, subject, position, slots[0])
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            result =
                string::concat(self.heap, result, head).map_err(|_| Completion::HEAP_EXHAUSTED)?;

            let text = if callable {
                let mut arguments = [Value::UNDEFINED; MAX_ARGUMENTS];
                let matched = string::slice(self.heap, subject, slots[0], slots[1])
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                arguments[0] = Value::string(matched);
                let mut count = 1usize;
                let mut index = 1usize;
                while index <= usize::from(groups) && count + 2 < arguments.len() {
                    let (from, to) = (slots[index * 2], slots[index * 2 + 1]);
                    arguments[count] = if from == u32::MAX || to == u32::MAX {
                        Value::UNDEFINED
                    } else {
                        let piece = string::slice(self.heap, subject, from, to)
                            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                        Value::string(piece)
                    };
                    count += 1;
                    index += 1;
                }
                arguments[count] = Value::number(crate::softfloat::from_u64(u64::from(slots[0])));
                arguments[count + 1] = Value::string(subject);
                count += 2;
                let named = self.named_captures(pattern, subject, &slots)?;
                if named.is_object() && count < arguments.len() {
                    arguments[count] = named;
                    count += 1;
                }
                let outcome = self.call_with(
                    replacement,
                    Value::UNDEFINED,
                    arguments.get(..count).unwrap_or(&[]),
                )?;
                self.string_handle(outcome)?
            } else {
                let text = self.string_handle(replacement)?;
                let named = self.named_captures(pattern, subject, &slots)?;
                self.expand_replacement(subject, text, &slots, groups, named)?
            };
            result =
                string::concat(self.heap, result, text).map_err(|_| Completion::HEAP_EXHAUSTED)?;

            position = slots[1];
            start = if slots[1] > slots[0] {
                slots[1]
            } else {
                slots[1] + 1
            };
            if !global {
                break;
            }
            if start > length {
                break;
            }
        }
        let tail = string::slice(self.heap, subject, position, length)
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        result = string::concat(self.heap, result, tail).map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(Value::string(result))
    }

    /// Expand `$&`, `` $` ``, `$'`, `$$`, and `$1`-`$99` in a replacement.
    pub(super) fn expand_replacement(
        &mut self,
        subject: Handle,
        replacement: Handle,
        slots: &crate::regexp::Slots,
        groups: u8,
        named: Value,
    ) -> Result<Handle, Completion> {
        let length =
            string::length(self.heap, replacement).map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let subject_length =
            string::length(self.heap, subject).map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let mut result = self.ascii_string(b"")?.as_handle();
        let mut index = 0u32;
        let mut plain_start = 0u32;
        while index < length {
            let unit = string::unit_at(self.heap, replacement, index)
                .map_err(|_| Completion::HEAP_EXHAUSTED)?
                .unwrap_or(0);
            if unit != 0x24 || index + 1 >= length {
                index += 1;
                continue;
            }
            let next = string::unit_at(self.heap, replacement, index + 1)
                .map_err(|_| Completion::HEAP_EXHAUSTED)?
                .unwrap_or(0);
            let (piece, width) = match next {
                0x24 => {
                    let text = string::slice(self.heap, replacement, index, index + 1)
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                    (Some(text), 2)
                }
                0x26 => {
                    let text = string::slice(self.heap, subject, slots[0], slots[1])
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                    (Some(text), 2)
                }
                0x60 => {
                    let text = string::slice(self.heap, subject, 0, slots[0])
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                    (Some(text), 2)
                }
                0x27 => {
                    let text = string::slice(self.heap, subject, slots[1], subject_length)
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                    (Some(text), 2)
                }
                0x30..=0x39 => {
                    // One or two digits, whichever names a group there is.
                    let mut number = u32::from(next - 0x30);
                    let mut width = 2u32;
                    if index + 2 < length {
                        let third = string::unit_at(self.heap, replacement, index + 2)
                            .map_err(|_| Completion::HEAP_EXHAUSTED)?
                            .unwrap_or(0);
                        if (0x30..=0x39).contains(&third) {
                            let wider = number * 10 + u32::from(third - 0x30);
                            if wider <= u32::from(groups) && wider != 0 {
                                number = wider;
                                width = 3;
                            }
                        }
                    }
                    if number == 0 || number > u32::from(groups) {
                        (None, 0)
                    } else {
                        let (from, to) =
                            (slots[number as usize * 2], slots[number as usize * 2 + 1]);
                        let text = if from == u32::MAX || to == u32::MAX {
                            self.ascii_string(b"")?.as_handle()
                        } else {
                            string::slice(self.heap, subject, from, to)
                                .map_err(|_| Completion::HEAP_EXHAUSTED)?
                        };
                        (Some(text), width)
                    }
                }
                // `$<name>`: the named capture, or nothing for a name the
                // pattern lacks; literal where the pattern names no group.
                0x3C if named.is_object() => {
                    let mut close = index + 2;
                    while close < length {
                        let unit = string::unit_at(self.heap, replacement, close)
                            .map_err(|_| Completion::HEAP_EXHAUSTED)?
                            .unwrap_or(0);
                        if unit == 0x3E {
                            break;
                        }
                        close += 1;
                    }
                    if close >= length {
                        (None, 0)
                    } else {
                        let name = string::slice(self.heap, replacement, index + 2, close)
                            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                        let key = self.coerce_to_key(Value::string(name))?;
                        let capture = self.get_property(named, key)?;
                        let text = if capture.is_undefined() {
                            self.ascii_string(b"")?.as_handle()
                        } else {
                            self.string_handle(capture)?
                        };
                        (Some(text), close - index + 1)
                    }
                }
                _ => (None, 0),
            };
            let Some(piece) = piece else {
                index += 1;
                continue;
            };
            let plain = string::slice(self.heap, replacement, plain_start, index)
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            result =
                string::concat(self.heap, result, plain).map_err(|_| Completion::HEAP_EXHAUSTED)?;
            result =
                string::concat(self.heap, result, piece).map_err(|_| Completion::HEAP_EXHAUSTED)?;
            index += width;
            plain_start = index;
        }
        let plain = string::slice(self.heap, replacement, plain_start, length)
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        result =
            string::concat(self.heap, result, plain).map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(result)
    }

    /// `split` with a pattern: the pieces between matches, with whatever the
    /// pattern captured between them.
    pub(super) fn split_by_pattern(
        &mut self,
        subject: Handle,
        pattern: Value,
    ) -> Result<Value, Completion> {
        let groups = self.regexp_groups(pattern);
        let length = string::length(self.heap, subject).map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let array = self.new_array()?;
        let mut written = 0u32;
        let mut position = 0u32;
        let mut start = 0u32;
        loop {
            if start > length {
                break;
            }
            let Some(slots) = self.match_regexp(pattern, subject, start, false)? else {
                break;
            };
            if slots[1] == slots[0] && slots[0] >= length {
                break;
            }
            let piece = string::slice(self.heap, subject, position, slots[0])
                .map_err(|_| Completion::HEAP_EXHAUSTED)?;
            self.set_element(array, written, Value::string(piece))?;
            written += 1;
            let mut index = 1usize;
            while index <= usize::from(groups) {
                let (from, to) = (slots[index * 2], slots[index * 2 + 1]);
                let value = if from == u32::MAX || to == u32::MAX {
                    Value::UNDEFINED
                } else {
                    let text = string::slice(self.heap, subject, from, to)
                        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                    Value::string(text)
                };
                self.set_element(array, written, value)?;
                written += 1;
                index += 1;
            }
            position = slots[1];
            start = if slots[1] > slots[0] {
                slots[1]
            } else {
                slots[1] + 1
            };
        }
        let tail = string::slice(self.heap, subject, position, length)
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        self.set_element(array, written, Value::string(tail))?;
        written += 1;
        self.set_length(array, written)?;
        Ok(array)
    }

    /// A value used as a pattern: itself where it is one, and a pattern of its
    /// text where it is not.
    pub(super) fn as_regexp(&mut self, value: Value) -> Result<Value, Completion> {
        if value.is_object()
            && object::regexp_program(self.heap, value.as_handle())
                .map_err(|_| Completion::HEAP_EXHAUSTED)?
                .is_some()
        {
            return Ok(value);
        }
        let text = self.string_handle(value)?;
        let length =
            string::length(self.heap, text).map_err(|_| Completion::HEAP_EXHAUSTED)? as usize;
        let mut units = [0u16; 512];
        let room = length.min(units.len());
        string::copy_units(self.heap, text, units.get_mut(..room).unwrap_or(&mut []))
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        // A string used as a pattern is taken literally, which is what
        // `String.prototype.replace` and `split` do with one.
        let mut quoted = [0u16; 1024];
        let mut written = 0usize;
        for &unit in units.get(..room).unwrap_or(&[]) {
            if matches!(
                unit,
                0x5C | 0x5E
                    | 0x24
                    | 0x2E
                    | 0x2A
                    | 0x2B
                    | 0x3F
                    | 0x28
                    | 0x29
                    | 0x5B
                    | 0x5D
                    | 0x7B
                    | 0x7D
                    | 0x7C
                    | 0x2F
            ) {
                if let Some(slot) = quoted.get_mut(written) {
                    *slot = 0x5C;
                    written += 1;
                }
            }
            if let Some(slot) = quoted.get_mut(written) {
                *slot = unit;
                written += 1;
            }
        }
        self.create_regexp(quoted.get(..written).unwrap_or(&[]), 0)
    }

    /// Whether a pattern is global, and whether it is sticky.
    pub(super) fn regexp_kind(&mut self, regexp: Value) -> Result<(bool, bool), Completion> {
        let Some((_, flags)) = (if regexp.is_object() {
            object::regexp_program(self.heap, regexp.as_handle())
                .map_err(|_| Completion::HEAP_EXHAUSTED)?
        } else {
            None
        }) else {
            return Err(self.throw_type_error());
        };
        Ok((
            flags & crate::regexp::flag::GLOBAL != 0,
            flags & crate::regexp::flag::STICKY != 0,
        ))
    }

    /// The array a match answers: the whole match, then each group, with the
    /// index it was found at and the input it was found in.
    pub(super) fn match_result(
        &mut self,
        subject: Handle,
        slots: &crate::regexp::Slots,
        regexp: Value,
    ) -> Result<Value, Completion> {
        let groups = if regexp.is_object() {
            object::regexp_program(self.heap, regexp.as_handle())
                .map_err(|_| Completion::HEAP_EXHAUSTED)?
                .map_or(0, |_| self.regexp_groups(regexp))
        } else {
            0
        };
        let array = self.new_array()?;
        let text = string::slice(self.heap, subject, slots[0], slots[1])
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        self.set_element(array, 0, Value::string(text))?;
        let mut index = 1u32;
        while index <= u32::from(groups) {
            let start = slots[index as usize * 2];
            let end = slots[index as usize * 2 + 1];
            let value = if start == u32::MAX || end == u32::MAX {
                Value::UNDEFINED
            } else {
                let text = string::slice(self.heap, subject, start, end)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                Value::string(text)
            };
            self.set_element(array, index, value)?;
            index += 1;
        }
        self.set_length(array, index)?;
        let index_key = self.ascii_key(b"index")?;
        let value = Value::number(crate::softfloat::from_u64(u64::from(slots[0])));
        self.define_property(array, index_key, value)?;
        let input_key = self.ascii_key(b"input")?;
        self.define_property(array, input_key, Value::string(subject))?;
        // `groups`: an object of the named captures, or undefined where the
        // pattern names none.
        let groups_value = self.named_captures(regexp, subject, slots)?;
        let groups_key = self.ascii_key(b"groups")?;
        self.define_property(array, groups_key, groups_value)?;
        Ok(array)
    }

    /// The program bytes a regexp holds, copied out for reading its tables.
    pub(super) fn regexp_code(
        &mut self,
        regexp: Value,
        code: &mut [u8; crate::regexp::MAX_PROGRAM],
    ) -> Result<usize, Completion> {
        let Ok(Some((program, _))) = object::regexp_program(self.heap, regexp.as_handle()) else {
            return Ok(0);
        };
        if !program.is_string() {
            return Ok(0);
        }
        let length = string::length(self.heap, program.as_handle())
            .map_err(|_| Completion::HEAP_EXHAUSTED)? as usize;
        let mut units = [0u16; crate::regexp::MAX_PROGRAM + 2];
        if length > units.len() {
            return Ok(0);
        }
        string::copy_units(
            self.heap,
            program.as_handle(),
            units.get_mut(..length).unwrap_or(&mut []),
        )
        .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        let mut index = 1usize;
        while index < length && index - 1 < code.len() {
            code[index - 1] = u8::try_from(units[index]).unwrap_or(0);
            index += 1;
        }
        Ok(length.saturating_sub(1))
    }

    /// The `groups` object of a match: each named group's capture under its
    /// name, on an object with no prototype — or undefined without names.
    pub(super) fn named_captures(
        &mut self,
        regexp: Value,
        subject: Handle,
        slots: &crate::regexp::Slots,
    ) -> Result<Value, Completion> {
        if !regexp.is_object() {
            return Ok(Value::UNDEFINED);
        }
        let mut code = [0u8; crate::regexp::MAX_PROGRAM];
        let length = self.regexp_code(regexp, &mut code)?;
        let mut names = crate::regexp::Names::of(code.get(..length).unwrap_or(&[]));
        if names.count() == 0 {
            return Ok(Value::UNDEFINED);
        }
        let groups =
            object::create(self.heap, Value::NULL).map_err(|_| Completion::HEAP_EXHAUSTED)?;
        while let Some((index, bytes)) = names.next_entry() {
            let mut units = [0u16; crate::regexp::MAX_NAME_UNITS];
            let mut count = 0usize;
            while count * 2 + 1 < bytes.len() && count < units.len() {
                units[count] = u16::from(bytes[count * 2]) | (u16::from(bytes[count * 2 + 1]) << 8);
                count += 1;
            }
            let name = self.make_string(units.get(..count).unwrap_or(&[]))?;
            let key = self.coerce_to_key(name)?;
            let (from, to) = (
                slots
                    .get(usize::from(index) * 2)
                    .copied()
                    .unwrap_or(u32::MAX),
                slots
                    .get(usize::from(index) * 2 + 1)
                    .copied()
                    .unwrap_or(u32::MAX),
            );
            let value = if from == u32::MAX || to == u32::MAX {
                Value::UNDEFINED
            } else {
                let text = string::slice(self.heap, subject, from, to)
                    .map_err(|_| Completion::HEAP_EXHAUSTED)?;
                Value::string(text)
            };
            object::define_own_property(
                self.heap,
                groups,
                key,
                Descriptor::data(
                    value,
                    attribute::WRITABLE | attribute::ENUMERABLE | attribute::CONFIGURABLE,
                ),
            )
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        }
        Ok(Value::object(groups))
    }

    /// How many groups a pattern captures, read from its program.
    pub(super) fn regexp_groups(&mut self, regexp: Value) -> u8 {
        let Ok(Some((program, _))) = object::regexp_program(self.heap, regexp.as_handle()) else {
            return 0;
        };
        if !program.is_string() {
            return 0;
        }
        string::unit_at(self.heap, program.as_handle(), 0)
            .ok()
            .flatten()
            .and_then(|unit| u8::try_from(unit).ok())
            .unwrap_or(0)
    }

    /// A regular expression literal: its program compiled once per evaluation.
    pub(super) fn op_create_regexp(&mut self, operands: &[u32; 3]) -> Step {
        let constant = self
            .unit()
            .constant(operands[0])
            .ok_or(Completion::MALFORMED)?;
        let mut units = [0u16; 513];
        let length = self
            .unit()
            .constant_units(&constant, &mut units)
            .ok_or(Completion::MALFORMED)?;
        let flags = crate::regexp::flags_of_token(units.first().copied().unwrap_or(0))
            .map_err(|_| self.throw_error_of(ErrorKind::Syntax))?;
        let pattern = units.get(1..length).unwrap_or(&[]);
        // A literal makes a new expression every time it is evaluated,
        // because its `lastIndex` is state a program can write.
        let value = self.create_regexp(pattern, flags)?;
        self.accumulator = value;

        Ok(())
    }
}
