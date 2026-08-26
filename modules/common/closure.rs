//! The container a linked module closure travels in.
//!
//! A closure is several unit images and the specifiers they name each other by.
//! The container holds them in evaluation order, so an isolate that receives
//! one has everything it needs and nothing it must fetch.
//!
//! Like a unit image, a container carries the format and feature digests of the
//! build that wrote it, and is refused rather than reinterpreted when either
//! differs.

use crate::bytecode::format_digest;
use crate::digest::Digest;
use crate::feature;

/// What a container starts with.
pub const MAGIC: [u8; 4] = *b"PLNK";
/// Bytes before the module table.
pub const HEADER_SIZE: usize = 120;
/// Bytes in one module record.
pub const RECORD_SIZE: usize = 16;

/// Why a container is not a closure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClosureError {
    Magic,
    FormatDigest,
    FeatureDigest,
    Truncated,
    /// The bytes are not the ones the container was written with.
    PayloadDigest,
    /// The entry names a module the container does not hold.
    Entry,
    /// The storage a writer was given ran out.
    Full,
}

/// A view over a container.
pub struct Closure<'a> {
    bytes: &'a [u8],
    count: u32,
    entry: u32,
    table_at: usize,
}

impl<'a> Closure<'a> {
    /// Validate a container and answer a view over it.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, ClosureError> {
        if bytes.len() < HEADER_SIZE {
            return Err(ClosureError::Truncated);
        }
        if bytes.get(..4) != Some(&MAGIC[..]) {
            return Err(ClosureError::Magic);
        }
        if bytes.get(4..36) != Some(&format_digest().0[..]) {
            return Err(ClosureError::FormatDigest);
        }
        if bytes.get(36..68) != Some(&feature::digest().0[..]) {
            return Err(ClosureError::FeatureDigest);
        }
        let count = read_u32(bytes, 68).ok_or(ClosureError::Truncated)?;
        let entry = read_u32(bytes, 72).ok_or(ClosureError::Truncated)?;
        // Everything after the header is covered by a digest, so a container
        // that changed on the way is refused rather than read.
        let payload = bytes.get(HEADER_SIZE..).ok_or(ClosureError::Truncated)?;
        if bytes.get(76..108) != Some(&crate::digest::digest(payload).0[..]) {
            return Err(ClosureError::PayloadDigest);
        }
        let table_at = HEADER_SIZE;
        let end = table_at
            .checked_add(
                (count as usize)
                    .checked_mul(RECORD_SIZE)
                    .ok_or(ClosureError::Truncated)?,
            )
            .ok_or(ClosureError::Truncated)?;
        if end > bytes.len() {
            return Err(ClosureError::Truncated);
        }
        if entry >= count {
            return Err(ClosureError::Entry);
        }
        let closure = Self {
            bytes,
            count,
            entry,
            table_at,
        };
        // Every span must be inside the container before anything reads one.
        let mut index = 0u32;
        while index < count {
            closure.specifier(index).ok_or(ClosureError::Truncated)?;
            closure.image(index).ok_or(ClosureError::Truncated)?;
            index += 1;
        }
        Ok(closure)
    }

    pub const fn count(&self) -> u32 {
        self.count
    }

    /// Which module is the one the closure was linked for.
    pub const fn entry(&self) -> u32 {
        self.entry
    }

    /// The specifier a module is known by, as UTF-8 bytes.
    pub fn specifier(&self, index: u32) -> Option<&'a [u8]> {
        let at = self.table_at + index as usize * RECORD_SIZE;
        let offset = read_u32(self.bytes, at)? as usize;
        let length = read_u32(self.bytes, at + 4)? as usize;
        self.bytes.get(offset..offset.checked_add(length)?)
    }

    /// A module's unit image.
    pub fn image(&self, index: u32) -> Option<&'a [u8]> {
        let at = self.table_at + index as usize * RECORD_SIZE;
        let offset = read_u32(self.bytes, at + 8)? as usize;
        let length = read_u32(self.bytes, at + 12)? as usize;
        self.bytes.get(offset..offset.checked_add(length)?)
    }

    /// The module a specifier names, if the closure holds one.
    pub fn index_of(&self, specifier: &[u8]) -> Option<u32> {
        let mut index = 0u32;
        while index < self.count {
            if self.specifier(index) == Some(specifier) {
                return Some(index);
            }
            index += 1;
        }
        None
    }

    /// The digest of the whole container, which is the closure's identity.
    pub fn digest(&self) -> Digest {
        crate::digest::digest(self.bytes)
    }
}

/// Write a container into `out`, answering its length.
///
/// The modules are written in the order they are given, which is the order they
/// evaluate in: a module comes after everything it imports.
pub fn write(
    out: &mut [u8],
    modules: &[(&[u8], &[u8])],
    entry: u32,
) -> Result<usize, ClosureError> {
    let count = modules.len();
    let table_at = HEADER_SIZE;
    let data_at = table_at
        .checked_add(count.checked_mul(RECORD_SIZE).ok_or(ClosureError::Full)?)
        .ok_or(ClosureError::Full)?;
    if out.len() < data_at {
        return Err(ClosureError::Full);
    }

    put(out, 0, &MAGIC)?;
    put(out, 4, &format_digest().0)?;
    put(out, 36, &feature::digest().0)?;
    put_u32(
        out,
        68,
        u32::try_from(count).map_err(|_| ClosureError::Full)?,
    )?;
    put_u32(out, 72, entry)?;
    put(out, 76, &[0u8; 32])?;
    put_u32(out, 108, 0)?;
    put_u32(out, 112, 0)?;
    put_u32(out, 116, 0)?;

    // The specifiers come first, then the images, each aligned so a reader can
    // hand an image straight to the verifier.
    let mut cursor = data_at;
    let mut index = 0usize;
    while index < count {
        let (specifier, _) = modules[index];
        let record = table_at + index * RECORD_SIZE;
        put_u32(
            out,
            record,
            u32::try_from(cursor).map_err(|_| ClosureError::Full)?,
        )?;
        put_u32(
            out,
            record + 4,
            u32::try_from(specifier.len()).map_err(|_| ClosureError::Full)?,
        )?;
        put(out, cursor, specifier)?;
        cursor += specifier.len();
        index += 1;
    }
    let mut index = 0usize;
    while index < count {
        let (_, image) = modules[index];
        cursor = (cursor + 3) & !3;
        let record = table_at + index * RECORD_SIZE;
        put_u32(
            out,
            record + 8,
            u32::try_from(cursor).map_err(|_| ClosureError::Full)?,
        )?;
        put_u32(
            out,
            record + 12,
            u32::try_from(image.len()).map_err(|_| ClosureError::Full)?,
        )?;
        put(out, cursor, image)?;
        cursor += image.len();
        index += 1;
    }

    // The digest is written last, over everything the container carries.
    let payload = out.get(HEADER_SIZE..cursor).ok_or(ClosureError::Full)?;
    let digest = crate::digest::digest(payload);
    put(out, 76, &digest.0)?;
    Ok(cursor)
}

fn put(out: &mut [u8], at: usize, bytes: &[u8]) -> Result<(), ClosureError> {
    let end = at.checked_add(bytes.len()).ok_or(ClosureError::Full)?;
    let slot = out.get_mut(at..end).ok_or(ClosureError::Full)?;
    slot.copy_from_slice(bytes);
    Ok(())
}

fn put_u32(out: &mut [u8], at: usize, value: u32) -> Result<(), ClosureError> {
    put(out, at, &value.to_le_bytes())
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 4)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}
