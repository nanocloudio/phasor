//! Strings, property keys, and `Number::toString`.
//!
//! A string cell stores its length in UTF-16 code units and its units either as
//! one byte each, where every unit fits, or as two bytes each. The storage is
//! an implementation choice with no observable effect: indexing, length, and
//! comparison are always in UTF-16 code units, which is what ECMAScript string
//! semantics expose.
//!
//! Property keys are either an array index, an interned string, or a symbol.
//! Interning makes key comparison a handle comparison, which is what the object
//! model needs.

use crate::dtoa;
use crate::heap::{CellKind, Heap, HeapError};
use crate::value::Handle;

/// The largest string this build admits, in UTF-16 code units.
pub const MAX_UNITS: u32 = 1 << 24;

/// Bytes before a string's units: a flags word and the length.
const HEADER: usize = 8;
/// One byte per unit is possible.
const LATIN1: u8 = 1;

/// Create a string from UTF-16 code units.
pub fn create(heap: &mut Heap<'_>, units: &[u16]) -> Result<Handle, HeapError> {
    let length = u32::try_from(units.len()).map_err(|_| HeapError::ArenaFull)?;
    if length > MAX_UNITS {
        return Err(HeapError::ArenaFull);
    }
    let latin1 = units.iter().all(|&unit| unit < 0x100);
    let payload = if latin1 { units.len() } else { units.len() * 2 };
    let size = u32::try_from(HEADER + payload).map_err(|_| HeapError::ArenaFull)?;
    let handle = heap.allocate(CellKind::String, size)?;

    let cell = heap.cell_mut(handle)?;
    cell[0] = if latin1 { LATIN1 } else { 0 };
    cell[4..8].copy_from_slice(&length.to_le_bytes());
    if latin1 {
        for (index, &unit) in units.iter().enumerate() {
            cell[HEADER + index] = u8::try_from(unit).unwrap_or(0);
        }
    } else {
        for (index, &unit) in units.iter().enumerate() {
            let bytes = unit.to_le_bytes();
            cell[HEADER + index * 2] = bytes[0];
            cell[HEADER + index * 2 + 1] = bytes[1];
        }
    }
    Ok(handle)
}

/// Create a string from UTF-16 code units already laid out little-endian,
/// two bytes each, as a unit image stores a constant: no code-unit buffer
/// stands between the image and the heap, so a constant may be as long as
/// the image holds.
pub fn create_from_le_bytes(heap: &mut Heap<'_>, bytes: &[u8]) -> Result<Handle, HeapError> {
    let count = bytes.len() / 2;
    let length = u32::try_from(count).map_err(|_| HeapError::ArenaFull)?;
    if length > MAX_UNITS {
        return Err(HeapError::ArenaFull);
    }
    let mut latin1 = true;
    let mut index = 0usize;
    while index < count {
        if bytes[index * 2 + 1] != 0 {
            latin1 = false;
            break;
        }
        index += 1;
    }
    let payload = if latin1 { count } else { count * 2 };
    let size = u32::try_from(HEADER + payload).map_err(|_| HeapError::ArenaFull)?;
    let handle = heap.allocate(CellKind::String, size)?;
    let cell = heap.cell_mut(handle)?;
    cell[0] = if latin1 { LATIN1 } else { 0 };
    cell[4..8].copy_from_slice(&length.to_le_bytes());
    if latin1 {
        let mut index = 0usize;
        while index < count {
            cell[HEADER + index] = bytes[index * 2];
            index += 1;
        }
    } else {
        cell[HEADER..HEADER + count * 2].copy_from_slice(&bytes[..count * 2]);
    }
    Ok(handle)
}

/// Create a symbol: a value whose identity is itself.
///
/// The description is held the way a string's units are held, and it is only
/// ever read: two symbols with the same description are different values, which
/// is the whole point of one.
pub fn create_symbol(heap: &mut Heap<'_>, units: &[u16]) -> Result<Handle, HeapError> {
    let length = u32::try_from(units.len()).map_err(|_| HeapError::ArenaFull)?;
    if length > MAX_UNITS {
        return Err(HeapError::ArenaFull);
    }
    let size = u32::try_from(HEADER + units.len() * 2).map_err(|_| HeapError::ArenaFull)?;
    let handle = heap.allocate(CellKind::Symbol, size)?;
    let cell = heap.cell_mut(handle)?;
    cell[0] = 0;
    cell[4..8].copy_from_slice(&length.to_le_bytes());
    for (index, &unit) in units.iter().enumerate() {
        let bytes = unit.to_le_bytes();
        cell[HEADER + index * 2] = bytes[0];
        cell[HEADER + index * 2 + 1] = bytes[1];
    }
    Ok(handle)
}

/// Create a symbol whose description is ASCII text.
pub fn create_symbol_ascii(heap: &mut Heap<'_>, text: &[u8]) -> Result<Handle, HeapError> {
    let mut units = [0u16; 64];
    let length = text.len().min(units.len());
    for (index, &byte) in text.iter().take(length).enumerate() {
        units[index] = u16::from(byte);
    }
    create_symbol(heap, units.get(..length).unwrap_or(&[]))
}

/// Create a string from ASCII bytes.
pub fn create_ascii(heap: &mut Heap<'_>, text: &[u8]) -> Result<Handle, HeapError> {
    let mut units = [0u16; 64];
    if text.len() <= units.len() {
        for (index, &byte) in text.iter().enumerate() {
            units[index] = u16::from(byte);
        }
        return create(heap, units.get(..text.len()).unwrap_or(&[]));
    }
    // Longer ASCII text is written directly, since it is already Latin-1.
    let length = u32::try_from(text.len()).map_err(|_| HeapError::ArenaFull)?;
    let size = u32::try_from(HEADER + text.len()).map_err(|_| HeapError::ArenaFull)?;
    let handle = heap.allocate(CellKind::String, size)?;
    let cell = heap.cell_mut(handle)?;
    cell[0] = LATIN1;
    cell[4..8].copy_from_slice(&length.to_le_bytes());
    cell[HEADER..HEADER + text.len()].copy_from_slice(text);
    Ok(handle)
}

/// A copy of `[start, end)` of a string, as a new string.
///
/// The bounds are clamped, so a caller that computed them from a program's
/// arguments cannot ask for anything outside the string.
pub fn slice(
    heap: &mut Heap<'_>,
    handle: Handle,
    start: u32,
    end: u32,
) -> Result<Handle, HeapError> {
    let length = length(heap, handle)?;
    let start = start.min(length);
    let end = end.clamp(start, length);
    let count = end - start;
    let target = allocate_units(heap, count)?;
    let mut index = 0u32;
    while index < count {
        let unit = unit_at(heap, handle, start + index)?.unwrap_or(0);
        write_wide_unit(heap, target, index, unit)?;
        index += 1;
    }
    Ok(target)
}

/// Allocate a string of `count` units, every one of them zero.
fn allocate_units(heap: &mut Heap<'_>, count: u32) -> Result<Handle, HeapError> {
    if count > MAX_UNITS {
        return Err(HeapError::ArenaFull);
    }
    let size = u32::try_from(HEADER + count as usize * 2).map_err(|_| HeapError::ArenaFull)?;
    let handle = heap.allocate(CellKind::String, size)?;
    let cell = heap.cell_mut(handle)?;
    cell[0] = 0;
    cell[4..8].copy_from_slice(&count.to_le_bytes());
    Ok(handle)
}

/// Write one unit of a string that was allocated wide.
fn write_wide_unit(
    heap: &mut Heap<'_>,
    handle: Handle,
    index: u32,
    unit: u16,
) -> Result<(), HeapError> {
    let cell = heap.cell_mut(handle)?;
    let at = HEADER + index as usize * 2;
    let bytes = unit.to_le_bytes();
    match cell.get_mut(at..at + 2) {
        Some(slot) => {
            slot[0] = bytes[0];
            slot[1] = bytes[1];
            Ok(())
        }
        None => Err(HeapError::StaleHandle),
    }
}

/// Where `needle` first occurs in `haystack` at or after `from`.
pub fn index_of(
    heap: &Heap<'_>,
    haystack: Handle,
    needle: Handle,
    from: u32,
) -> Result<Option<u32>, HeapError> {
    let outer = length(heap, haystack)?;
    let inner = length(heap, needle)?;
    if inner > outer {
        return Ok(None);
    }
    let mut start = from.min(outer);
    while start + inner <= outer {
        let mut index = 0u32;
        let mut same = true;
        while index < inner {
            if unit_at(heap, haystack, start + index)? != unit_at(heap, needle, index)? {
                same = false;
                break;
            }
            index += 1;
        }
        if same {
            return Ok(Some(start));
        }
        start += 1;
    }
    Ok(None)
}

/// Where `needle` last occurs in `haystack`.
pub fn last_index_of(
    heap: &Heap<'_>,
    haystack: Handle,
    needle: Handle,
) -> Result<Option<u32>, HeapError> {
    let outer = length(heap, haystack)?;
    let inner = length(heap, needle)?;
    if inner > outer {
        return Ok(None);
    }
    let mut start = outer - inner;
    loop {
        let mut index = 0u32;
        let mut same = true;
        while index < inner {
            if unit_at(heap, haystack, start + index)? != unit_at(heap, needle, index)? {
                same = false;
                break;
            }
            index += 1;
        }
        if same {
            return Ok(Some(start));
        }
        if start == 0 {
            return Ok(None);
        }
        start -= 1;
    }
}

/// Whether `haystack` has `needle` at `at`.
pub fn matches_at(
    heap: &Heap<'_>,
    haystack: Handle,
    needle: Handle,
    at: u32,
) -> Result<bool, HeapError> {
    let outer = length(heap, haystack)?;
    let inner = length(heap, needle)?;
    if at.saturating_add(inner) > outer {
        return Ok(false);
    }
    let mut index = 0u32;
    while index < inner {
        if unit_at(heap, haystack, at + index)? != unit_at(heap, needle, index)? {
            return Ok(false);
        }
        index += 1;
    }
    Ok(true)
}

/// The same string with its ASCII letters in one case.
///
/// Only the ASCII range and the Latin-1 letters with a simple one-to-one
/// mapping are converted, which is what this build admits; anything else is
/// left as it is rather than being changed wrongly.
pub fn convert_case(heap: &mut Heap<'_>, handle: Handle, upper: bool) -> Result<Handle, HeapError> {
    let count = length(heap, handle)?;
    let target = allocate_units(heap, count)?;
    let mut index = 0u32;
    while index < count {
        let unit = unit_at(heap, handle, index)?.unwrap_or(0);
        let converted = if upper {
            match unit {
                0x61..=0x7A => unit - 32,
                0xE0..=0xFE if unit != 0xF7 => unit - 32,
                _ => unit,
            }
        } else {
            match unit {
                0x41..=0x5A => unit + 32,
                0xC0..=0xDE if unit != 0xD7 => unit + 32,
                _ => unit,
            }
        };
        write_wide_unit(heap, target, index, converted)?;
        index += 1;
    }
    Ok(target)
}

/// UTF-8 bytes (one per code unit, as a network payload arrives) decoded
/// into text, and text encoded back the same way.
///
/// These exist because doing it in JavaScript cannot be made cheap: a string
/// built with `out += ch` reallocates the whole result per character, so
/// decoding n bytes churns O(n^2) and a page-sized body exhausts the arena
/// long before it is read. Segmenting the appends only lowers the constant.
/// Here the answer is sized first and then filled in place, so one string is
/// allocated however long the input is.
///
/// The source is a byte string: each unit is one byte of the payload, and a
/// unit above 0xFF is not a byte and makes the input malformed at that
/// position, which decodes to U+FFFD exactly as a bad sequence does.
pub fn decode_utf8(heap: &mut Heap<'_>, handle: Handle) -> Result<Handle, HeapError> {
    let count = length(heap, handle)?;
    let units = utf8_scan(heap, handle, count, None)?;
    let target = allocate_units(heap, units)?;
    utf8_scan(heap, handle, count, Some(target))?;
    Ok(target)
}

/// Count the units a decode produces, or write them into `out`. One walk
/// serves both so the two can never disagree about the length.
fn utf8_scan(
    heap: &mut Heap<'_>,
    handle: Handle,
    count: u32,
    out: Option<Handle>,
) -> Result<u32, HeapError> {
    let mut written = 0u32;
    let mut at = 0u32;
    while at < count {
        let first = unit_at(heap, handle, at)?.unwrap_or(0);
        let width = match first {
            0x00..=0x7F => 1u32,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            // A continuation byte with nothing to continue, or a unit that
            // is not a byte at all.
            _ => 0,
        };
        if width == 0 {
            // An invalid lead: one replacement, and carry on at the next
            // byte, which may well start a valid sequence.
            if let Some(target) = out {
                write_wide_unit(heap, target, written, 0xFFFD)?;
            }
            written += 1;
            at += 1;
            continue;
        }
        if at + width > count {
            // A sequence cut off by the end of the input is ONE replacement
            // for the whole remainder, not one per leftover byte: the bytes
            // that are there are a prefix of a single code point.
            if let Some(target) = out {
                write_wide_unit(heap, target, written, 0xFFFD)?;
            }
            written += 1;
            break;
        }
        let mut point = u32::from(match width {
            1 => first,
            2 => first & 0x1F,
            3 => first & 0x0F,
            _ => first & 0x07,
        });
        let mut index = 1u32;
        while index < width {
            let continuation = unit_at(heap, handle, at + index)?.unwrap_or(0);
            point = (point << 6) | u32::from(continuation & 0x3F);
            index += 1;
        }
        at += width;
        if point > 0xFFFF {
            let adjusted = point - 0x10000;
            if let Some(target) = out {
                let high = 0xD800 + u16::try_from(adjusted >> 10).unwrap_or(0);
                let low = 0xDC00 + u16::try_from(adjusted & 0x3FF).unwrap_or(0);
                write_wide_unit(heap, target, written, high)?;
                write_wide_unit(heap, target, written + 1, low)?;
            }
            written += 2;
        } else {
            if let Some(target) = out {
                write_wide_unit(
                    heap,
                    target,
                    written,
                    u16::try_from(point).unwrap_or(0xFFFD),
                )?;
            }
            written += 1;
        }
    }
    Ok(written)
}

/// Text encoded as UTF-8, one byte per unit of the answer.
pub fn encode_utf8(heap: &mut Heap<'_>, handle: Handle) -> Result<Handle, HeapError> {
    let count = length(heap, handle)?;
    let bytes = utf8_emit(heap, handle, count, None)?;
    let target = allocate_units(heap, bytes)?;
    utf8_emit(heap, handle, count, Some(target))?;
    Ok(target)
}

fn utf8_emit(
    heap: &mut Heap<'_>,
    handle: Handle,
    count: u32,
    out: Option<Handle>,
) -> Result<u32, HeapError> {
    let mut written = 0u32;
    let mut at = 0u32;
    while at < count {
        let unit = unit_at(heap, handle, at)?.unwrap_or(0);
        let mut point = u32::from(unit);
        at += 1;
        // A surrogate pair is one code point; a lone surrogate is left as it
        // is and encodes to three bytes, which is what the JavaScript
        // implementation this replaces did.
        if (0xD800..0xDC00).contains(&unit) && at < count {
            let low = unit_at(heap, handle, at)?.unwrap_or(0);
            if (0xDC00..0xE000).contains(&low) {
                point = 0x10000 + ((point - 0xD800) << 10) + (u32::from(low) - 0xDC00);
                at += 1;
            }
        }
        let mut encoded = [0u16; 4];
        let width = if point < 0x80 {
            encoded[0] = u16::try_from(point).unwrap_or(0);
            1
        } else if point < 0x800 {
            encoded[0] = u16::try_from(0xC0 | (point >> 6)).unwrap_or(0);
            encoded[1] = u16::try_from(0x80 | (point & 0x3F)).unwrap_or(0);
            2
        } else if point < 0x10000 {
            encoded[0] = u16::try_from(0xE0 | (point >> 12)).unwrap_or(0);
            encoded[1] = u16::try_from(0x80 | ((point >> 6) & 0x3F)).unwrap_or(0);
            encoded[2] = u16::try_from(0x80 | (point & 0x3F)).unwrap_or(0);
            3
        } else {
            encoded[0] = u16::try_from(0xF0 | (point >> 18)).unwrap_or(0);
            encoded[1] = u16::try_from(0x80 | ((point >> 12) & 0x3F)).unwrap_or(0);
            encoded[2] = u16::try_from(0x80 | ((point >> 6) & 0x3F)).unwrap_or(0);
            encoded[3] = u16::try_from(0x80 | (point & 0x3F)).unwrap_or(0);
            4
        };
        if let Some(target) = out {
            let mut index = 0usize;
            while index < width {
                write_wide_unit(
                    heap,
                    target,
                    written + u32::try_from(index).unwrap_or(0),
                    encoded[index],
                )?;
                index += 1;
            }
        }
        written += u32::try_from(width).unwrap_or(0);
    }
    Ok(written)
}

/// Whether a code unit is white space or a line terminator, which is what
/// trimming removes.
pub const fn is_trimmable(unit: u16) -> bool {
    matches!(
        unit,
        0x09 | 0x0A | 0x0B | 0x0C | 0x0D | 0x20 | 0xA0 | 0x1680 | 0x2000
            ..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000 | 0xFEFF
    )
}

/// The same string with white space removed from one or both ends.
pub fn trim(
    heap: &mut Heap<'_>,
    handle: Handle,
    start: bool,
    end: bool,
) -> Result<Handle, HeapError> {
    let count = length(heap, handle)?;
    let mut first = 0u32;
    if start {
        while first < count {
            match unit_at(heap, handle, first)? {
                Some(unit) if is_trimmable(unit) => first += 1,
                _ => break,
            }
        }
    }
    let mut last = count;
    if end {
        while last > first {
            match unit_at(heap, handle, last - 1)? {
                Some(unit) if is_trimmable(unit) => last -= 1,
                _ => break,
            }
        }
    }
    slice(heap, handle, first, last)
}

/// The same string repeated `count` times.
pub fn repeat(heap: &mut Heap<'_>, handle: Handle, count: u32) -> Result<Handle, HeapError> {
    let unit_count = length(heap, handle)?;
    let total = unit_count.checked_mul(count).ok_or(HeapError::ArenaFull)?;
    let target = allocate_units(heap, total)?;
    let mut written = 0u32;
    let mut turn = 0u32;
    while turn < count {
        let mut index = 0u32;
        while index < unit_count {
            let unit = unit_at(heap, handle, index)?.unwrap_or(0);
            write_wide_unit(heap, target, written, unit)?;
            written += 1;
            index += 1;
        }
        turn += 1;
    }
    Ok(target)
}

/// The same string padded to `target` units with copies of `filler`.
pub fn pad(
    heap: &mut Heap<'_>,
    handle: Handle,
    target_length: u32,
    filler: Handle,
    at_start: bool,
) -> Result<Handle, HeapError> {
    let count = length(heap, handle)?;
    let filler_length = length(heap, filler)?;
    if target_length <= count || filler_length == 0 {
        return slice(heap, handle, 0, count);
    }
    let target = allocate_units(heap, target_length)?;
    let padding = target_length - count;
    let mut index = 0u32;
    while index < target_length {
        let unit = if at_start {
            if index < padding {
                unit_at(heap, filler, index % filler_length)?.unwrap_or(0)
            } else {
                unit_at(heap, handle, index - padding)?.unwrap_or(0)
            }
        } else if index < count {
            unit_at(heap, handle, index)?.unwrap_or(0)
        } else {
            unit_at(heap, filler, (index - count) % filler_length)?.unwrap_or(0)
        };
        write_wide_unit(heap, target, index, unit)?;
        index += 1;
    }
    Ok(target)
}

/// The code point at `index`, and how many units it took.
pub fn code_point_at(
    heap: &Heap<'_>,
    handle: Handle,
    index: u32,
) -> Result<Option<(u32, u32)>, HeapError> {
    let Some(first) = unit_at(heap, handle, index)? else {
        return Ok(None);
    };
    if (0xD800..0xDC00).contains(&first) {
        if let Some(second) = unit_at(heap, handle, index + 1)? {
            if (0xDC00..0xE000).contains(&second) {
                let point =
                    0x1_0000 + ((u32::from(first) - 0xD800) << 10) + (u32::from(second) - 0xDC00);
                return Ok(Some((point, 2)));
            }
        }
    }
    Ok(Some((u32::from(first), 1)))
}

/// The length of a string in UTF-16 code units.
pub fn length(heap: &Heap<'_>, handle: Handle) -> Result<u32, HeapError> {
    let cell = string_cell(heap, handle)?;
    Ok(u32::from_le_bytes([cell[4], cell[5], cell[6], cell[7]]))
}

/// The code unit at `index`.
pub fn unit_at(heap: &Heap<'_>, handle: Handle, index: u32) -> Result<Option<u16>, HeapError> {
    let cell = string_cell(heap, handle)?;
    let count = u32::from_le_bytes([cell[4], cell[5], cell[6], cell[7]]);
    if index >= count {
        return Ok(None);
    }
    let at = index as usize;
    if cell[0] & LATIN1 != 0 {
        Ok(cell.get(HEADER + at).map(|&byte| u16::from(byte)))
    } else {
        let low = cell.get(HEADER + at * 2).copied();
        let high = cell.get(HEADER + at * 2 + 1).copied();
        Ok(match (low, high) {
            (Some(low), Some(high)) => Some(u16::from(low) | (u16::from(high) << 8)),
            _ => None,
        })
    }
}

/// Copy a string's units into `out`, returning how many were written.
pub fn copy_units(heap: &Heap<'_>, handle: Handle, out: &mut [u16]) -> Result<usize, HeapError> {
    let count = length(heap, handle)? as usize;
    if out.len() < count {
        return Err(HeapError::ArenaFull);
    }
    let mut index = 0usize;
    while index < count {
        out[index] = unit_at(heap, handle, u32::try_from(index).unwrap_or(0))?.unwrap_or(0);
        index += 1;
    }
    Ok(count)
}

/// Whether two strings hold the same units.
pub fn equals(heap: &Heap<'_>, left: Handle, right: Handle) -> Result<bool, HeapError> {
    if left == right {
        return Ok(true);
    }
    let count = length(heap, left)?;
    if count != length(heap, right)? {
        return Ok(false);
    }
    let mut index = 0u32;
    while index < count {
        if unit_at(heap, left, index)? != unit_at(heap, right, index)? {
            return Ok(false);
        }
        index += 1;
    }
    Ok(true)
}

/// Compare two strings by code unit, which is what the relational operators
/// use.
pub fn compare(
    heap: &Heap<'_>,
    left: Handle,
    right: Handle,
) -> Result<core::cmp::Ordering, HeapError> {
    let left_length = length(heap, left)?;
    let right_length = length(heap, right)?;
    let shared = left_length.min(right_length);
    let mut index = 0u32;
    while index < shared {
        let a = unit_at(heap, left, index)?.unwrap_or(0);
        let b = unit_at(heap, right, index)?.unwrap_or(0);
        if a != b {
            return Ok(a.cmp(&b));
        }
        index += 1;
    }
    Ok(left_length.cmp(&right_length))
}

/// Join two strings.
pub fn concat(heap: &mut Heap<'_>, left: Handle, right: Handle) -> Result<Handle, HeapError> {
    let left_length = length(heap, left)?;
    let right_length = length(heap, right)?;
    let total = left_length
        .checked_add(right_length)
        .ok_or(HeapError::ArenaFull)?;
    if total > MAX_UNITS {
        return Err(HeapError::ArenaFull);
    }

    // Both operands are Latin-1 exactly when every unit of both fits a byte.
    let latin1 = is_latin1(heap, left)? && is_latin1(heap, right)?;
    let payload = if latin1 {
        total as usize
    } else {
        total as usize * 2
    };
    let handle = heap.allocate(
        CellKind::String,
        u32::try_from(HEADER + payload).map_err(|_| HeapError::ArenaFull)?,
    )?;

    let mut index = 0u32;
    while index < total {
        let unit = if index < left_length {
            unit_at(heap, left, index)?.unwrap_or(0)
        } else {
            unit_at(heap, right, index - left_length)?.unwrap_or(0)
        };
        write_unit(heap, handle, index, unit, latin1)?;
        index += 1;
    }
    let cell = heap.cell_mut(handle)?;
    cell[0] = if latin1 { LATIN1 } else { 0 };
    cell[4..8].copy_from_slice(&total.to_le_bytes());
    Ok(handle)
}

fn write_unit(
    heap: &mut Heap<'_>,
    handle: Handle,
    index: u32,
    unit: u16,
    latin1: bool,
) -> Result<(), HeapError> {
    let cell = heap.cell_mut(handle)?;
    let at = index as usize;
    if latin1 {
        if let Some(slot) = cell.get_mut(HEADER + at) {
            *slot = u8::try_from(unit).unwrap_or(0);
        }
    } else {
        let bytes = unit.to_le_bytes();
        if let Some(slot) = cell.get_mut(HEADER + at * 2) {
            *slot = bytes[0];
        }
        if let Some(slot) = cell.get_mut(HEADER + at * 2 + 1) {
            *slot = bytes[1];
        }
    }
    Ok(())
}

fn is_latin1(heap: &Heap<'_>, handle: Handle) -> Result<bool, HeapError> {
    let cell = string_cell(heap, handle)?;
    Ok(cell[0] & LATIN1 != 0)
}

fn string_cell<'h>(heap: &'h Heap<'_>, handle: Handle) -> Result<&'h [u8], HeapError> {
    // A symbol holds its description exactly as a string holds its units, so
    // the readers are the same and a description needs no copy of its own.
    if !matches!(heap.kind(handle)?, CellKind::String | CellKind::Symbol) {
        return Err(HeapError::StaleHandle);
    }
    let cell = heap.cell(handle)?;
    if cell.len() < HEADER {
        return Err(HeapError::StaleHandle);
    }
    Ok(cell)
}

/// A property key.
///
/// An array index is held as a number rather than as text, because that is how
/// an indexed element is addressed and how the specification distinguishes it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    Index(u32),
    Name(Handle),
    Symbol(Handle),
}

/// The largest array index, which is one below the largest array length.
pub const MAX_ARRAY_INDEX: u32 = u32::MAX - 1;

/// The array index a string denotes, if it is a canonical one.
///
/// A canonical index is `0` or a digit sequence with no leading zero whose
/// value is below the maximum, which is exactly the set of strings that address
/// an element rather than a named property.
pub fn array_index(units: &[u16]) -> Option<u32> {
    if units.is_empty() || units.len() > 10 {
        return None;
    }
    if units[0] == u16::from(b'0') {
        return if units.len() == 1 { Some(0) } else { None };
    }
    let mut value = 0u64;
    for &unit in units {
        if !(0x30..=0x39).contains(&unit) {
            return None;
        }
        value = value * 10 + u64::from(unit - 0x30);
        if value > u64::from(MAX_ARRAY_INDEX) {
            return None;
        }
    }
    u32::try_from(value).ok()
}

/// An open-addressed table that gives one handle per distinct string.
///
/// Interning is what makes a property-key comparison a handle comparison. The
/// table is caller-provided and never grows: a full table is an ordinary
/// failure.
/// The atom table's own state, apart from its storage.
#[derive(Clone, Copy, Debug, Default)]
pub struct AtomsSave {
    count: u32,
}

pub struct Atoms<'a> {
    entries: &'a mut [u32],
    handles: &'a mut [Handle],
    count: u32,
}

const EMPTY_ENTRY: u32 = u32::MAX;

impl<'a> Atoms<'a> {
    /// The table's state, to be restored onto the same storage later.
    pub const fn save(&self) -> AtomsSave {
        AtomsSave { count: self.count }
    }

    /// Carry on from a saved state over the same entries and handles.
    pub fn restore(&mut self, save: &AtomsSave) {
        self.count = save.count;
    }
    /// A table over `entries` slots, which must be a power of two, holding
    /// indices into `handles`.
    pub fn new(entries: &'a mut [u32], handles: &'a mut [Handle]) -> Self {
        for entry in entries.iter_mut() {
            *entry = EMPTY_ENTRY;
        }
        Self {
            entries,
            handles,
            count: 0,
        }
    }

    pub const fn count(&self) -> u32 {
        self.count
    }

    /// The interned handles, which a collection must treat as roots.
    /// Take up storage an atom table was already using, with its saved state.
    ///
    /// The entries are left as they were found: they are the table, and an
    /// interned name must stay interned across a rebuild.
    pub fn adopt(entries: &'a mut [u32], handles: &'a mut [Handle], save: &AtomsSave) -> Self {
        Self {
            entries,
            handles,
            count: save.count,
        }
    }

    pub fn handles(&self) -> &[Handle] {
        match self.handles.get(..self.count as usize) {
            Some(slice) => slice,
            None => &[],
        }
    }

    /// The handle for `units`, creating and recording the string when the table
    /// does not hold it yet.
    pub fn intern(&mut self, heap: &mut Heap<'_>, units: &[u16]) -> Result<Handle, HeapError> {
        let mask = self.entries.len().wrapping_sub(1);
        if self.entries.is_empty() || self.entries.len() & mask != 0 {
            return Err(HeapError::SlotsFull);
        }
        let hash = hash_units(units) as usize;
        let mut probe = hash & mask;
        let mut steps = 0usize;
        while steps <= mask {
            let entry = self.entries[probe];
            if entry == EMPTY_ENTRY {
                let handle = create(heap, units)?;
                let index = self.count as usize;
                let slot = self.handles.get_mut(index).ok_or(HeapError::SlotsFull)?;
                *slot = handle;
                self.entries[probe] = u32::try_from(index).map_err(|_| HeapError::SlotsFull)?;
                self.count += 1;
                return Ok(handle);
            }
            let existing = *self
                .handles
                .get(entry as usize)
                .ok_or(HeapError::StaleHandle)?;
            if units_equal(heap, existing, units)? {
                return Ok(existing);
            }
            probe = (probe + 1) & mask;
            steps += 1;
        }
        Err(HeapError::SlotsFull)
    }

    /// Intern a string already on the heap, by its own units: the handle
    /// itself becomes the atom when no equal one is held, so a name of any
    /// length is a key without a unit buffer between.
    pub fn intern_string(&mut self, heap: &Heap<'_>, string: Handle) -> Result<Handle, HeapError> {
        let mask = self.entries.len().wrapping_sub(1);
        if self.entries.is_empty() || self.entries.len() & mask != 0 {
            return Err(HeapError::SlotsFull);
        }
        let count = length(heap, string)?;
        let mut hash = 0x811C_9DC5u32;
        let mut index = 0u32;
        while index < count {
            let unit = unit_at(heap, string, index)?.unwrap_or(0);
            hash ^= u32::from(unit);
            hash = hash.wrapping_mul(0x0100_0193);
            index += 1;
        }
        let mut probe = hash as usize & mask;
        let mut steps = 0usize;
        while steps <= mask {
            let entry = self.entries[probe];
            if entry == EMPTY_ENTRY {
                let index = self.count as usize;
                let slot = self.handles.get_mut(index).ok_or(HeapError::SlotsFull)?;
                *slot = string;
                self.entries[probe] = u32::try_from(index).map_err(|_| HeapError::SlotsFull)?;
                self.count += 1;
                return Ok(string);
            }
            let existing = *self
                .handles
                .get(entry as usize)
                .ok_or(HeapError::StaleHandle)?;
            if equals(heap, existing, string)? {
                return Ok(existing);
            }
            probe = (probe + 1) & mask;
            steps += 1;
        }
        Err(HeapError::SlotsFull)
    }

    /// The interned handle for `units`, without creating one.
    pub fn lookup(&self, heap: &Heap<'_>, units: &[u16]) -> Result<Option<Handle>, HeapError> {
        let mask = self.entries.len().wrapping_sub(1);
        if self.entries.is_empty() {
            return Ok(None);
        }
        let mut probe = hash_units(units) as usize & mask;
        let mut steps = 0usize;
        while steps <= mask {
            let entry = self.entries[probe];
            if entry == EMPTY_ENTRY {
                return Ok(None);
            }
            let existing = *self
                .handles
                .get(entry as usize)
                .ok_or(HeapError::StaleHandle)?;
            if units_equal(heap, existing, units)? {
                return Ok(Some(existing));
            }
            probe = (probe + 1) & mask;
            steps += 1;
        }
        Ok(None)
    }
}

fn units_equal(heap: &Heap<'_>, handle: Handle, units: &[u16]) -> Result<bool, HeapError> {
    let count = length(heap, handle)?;
    if count as usize != units.len() {
        return Ok(false);
    }
    let mut index = 0u32;
    while (index as usize) < units.len() {
        if unit_at(heap, handle, index)? != Some(units[index as usize]) {
            return Ok(false);
        }
        index += 1;
    }
    Ok(true)
}

/// A stable hash over code units.
fn hash_units(units: &[u16]) -> u32 {
    let mut hash = 0x811C_9DC5u32;
    for &unit in units {
        hash ^= u32::from(unit);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// `Number::toString` in base ten, writing UTF-16 code units into `out`.
///
/// The digits are the shortest that read back as the same double, and the
/// layout follows the specification: an integer form below twenty-one digits,
/// a fixed form down to a millionth, and an exponential form outside those.
pub fn number_to_string(value: f64, out: &mut [u16]) -> usize {
    dtoa::shortest_text(value, out)
}
