//! The URI codecs.

use super::*;

impl<'a, 'u, 'h, 'atoms> Vm<'a, 'u, 'h, 'atoms> {
    /// The natives of `Object`.
    /// The URI codecs: percent-encoding over UTF-8, with each function's
    /// own set of untouched characters, and a URIError for a malformed
    /// escape or a lone surrogate.
    pub(in crate::vm) fn uri_native(&mut self, id: u32, value: Value) -> Result<Value, Completion> {
        let text = self.coerce_to_string(value)?;
        let handle = text.as_handle();
        let length =
            crate::string::length(self.heap, handle).map_err(|_| Completion::MALFORMED)? as usize;
        let mut units = [0u16; 1024];
        if length > units.len() {
            return Err(Completion::HEAP_EXHAUSTED);
        }
        crate::string::copy_units(self.heap, handle, &mut units[..length])
            .map_err(|_| Completion::MALFORMED)?;
        let mut out = [0u16; 3072];
        let mut written = 0usize;
        let decode = matches!(id, native::DECODE_URI | native::DECODE_URI_COMPONENT);
        let unreserved = |unit: u16, component: bool| -> bool {
            let byte = unit as u8;
            if unit > 0x7F {
                return false;
            }
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
                )
                || (!component
                    && matches!(
                        byte,
                        b';' | b'/' | b'?' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b',' | b'#'
                    ))
        };
        let reserved_kept = |byte: u8, component: bool| -> bool {
            !component
                && matches!(
                    byte,
                    b';' | b'/' | b'?' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b',' | b'#'
                )
        };
        let component = matches!(
            id,
            native::ENCODE_URI_COMPONENT | native::DECODE_URI_COMPONENT
        );
        let uri_error = |vm: &mut Self| -> Completion {
            let reason = match vm.create_error(ErrorKind::Uri, Value::UNDEFINED) {
                Ok(reason) => reason,
                Err(completion) => return completion,
            };
            Completion::Throw(reason)
        };
        let mut push = |slot: u16, written: &mut usize| -> bool {
            if let Some(cell) = out.get_mut(*written) {
                *cell = slot;
                *written += 1;
                true
            } else {
                false
            }
        };
        if decode {
            let hex = |unit: u16| -> Option<u8> {
                match unit {
                    0x30..=0x39 => Some(unit as u8 - b'0'),
                    0x41..=0x46 => Some(unit as u8 - b'A' + 10),
                    0x61..=0x66 => Some(unit as u8 - b'a' + 10),
                    _ => None,
                }
            };
            let mut index = 0usize;
            while index < length {
                let unit = units[index];
                if unit != u16::from(b'%') {
                    if !push(unit, &mut written) {
                        return Err(Completion::HEAP_EXHAUSTED);
                    }
                    index += 1;
                    continue;
                }
                let byte_at = |at: usize| -> Option<u8> {
                    if at + 2 < length && units[at] == u16::from(b'%') {
                        Some((hex(units[at + 1])? << 4) | hex(units[at + 2])?)
                    } else {
                        None
                    }
                };
                let Some(first_byte) = byte_at(index) else {
                    return Err(uri_error(self));
                };
                let count = if first_byte < 0x80 {
                    1
                } else if (0xC2..0xE0).contains(&first_byte) {
                    2
                } else if (0xE0..0xF0).contains(&first_byte) {
                    3
                } else if (0xF0..0xF5).contains(&first_byte) {
                    4
                } else {
                    return Err(uri_error(self));
                };
                if count == 1 {
                    if reserved_kept(first_byte, component) {
                        // decodeURI leaves an escaped reserved character as
                        // its escape.
                        for offset in 0..3 {
                            if !push(units[index + offset], &mut written) {
                                return Err(Completion::HEAP_EXHAUSTED);
                            }
                        }
                    } else if !push(u16::from(first_byte), &mut written) {
                        return Err(Completion::HEAP_EXHAUSTED);
                    }
                    index += 3;
                    continue;
                }
                let mut point = u32::from(first_byte & (0x7F >> count));
                let mut offset = 3usize;
                let mut trailing = 1usize;
                while trailing < count {
                    let Some(byte) = byte_at(index + offset) else {
                        return Err(uri_error(self));
                    };
                    if byte & 0xC0 != 0x80 {
                        return Err(uri_error(self));
                    }
                    point = (point << 6) | u32::from(byte & 0x3F);
                    offset += 3;
                    trailing += 1;
                }
                if point > 0x10FFFF || (0xD800..0xE000).contains(&point) {
                    return Err(uri_error(self));
                }
                if point > 0xFFFF {
                    let bias = point - 0x10000;
                    if !push(0xD800 + (bias >> 10) as u16, &mut written)
                        || !push(0xDC00 + (bias & 0x3FF) as u16, &mut written)
                    {
                        return Err(Completion::HEAP_EXHAUSTED);
                    }
                } else if !push(point as u16, &mut written) {
                    return Err(Completion::HEAP_EXHAUSTED);
                }
                index += 3 * count;
            }
        } else {
            let hex_digit = |value: u8| -> u16 {
                u16::from(if value < 10 {
                    b'0' + value
                } else {
                    b'A' + value - 10
                })
            };
            let mut index = 0usize;
            while index < length {
                let unit = units[index];
                if unreserved(unit, component) {
                    if !push(unit, &mut written) {
                        return Err(Completion::HEAP_EXHAUSTED);
                    }
                    index += 1;
                    continue;
                }
                // The unit — or a pair — becomes UTF-8 percent escapes.
                let point = if (0xD800..0xDC00).contains(&unit) {
                    let Some(&low) = units.get(index + 1) else {
                        return Err(uri_error(self));
                    };
                    if !(0xDC00..0xE000).contains(&low) || index + 1 >= length {
                        return Err(uri_error(self));
                    }
                    index += 2;
                    0x10000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(low) - 0xDC00)
                } else if (0xDC00..0xE000).contains(&unit) {
                    return Err(uri_error(self));
                } else {
                    index += 1;
                    u32::from(unit)
                };
                let mut bytes = [0u8; 4];
                let encoded = char::from_u32(point)
                    .map(|c| c.encode_utf8(&mut bytes).len())
                    .unwrap_or(0);
                for &byte in bytes.get(..encoded).unwrap_or(&[]) {
                    if !push(u16::from(b'%'), &mut written)
                        || !push(hex_digit(byte >> 4), &mut written)
                        || !push(hex_digit(byte & 0xF), &mut written)
                    {
                        return Err(Completion::HEAP_EXHAUSTED);
                    }
                }
            }
        }
        let made = crate::string::create(self.heap, out.get(..written).unwrap_or(&[]))
            .map_err(|_| Completion::HEAP_EXHAUSTED)?;
        Ok(Value::string(made))
    }
}
