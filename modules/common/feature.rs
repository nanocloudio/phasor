//! The feature list this build admits, and the digest that binds it.
//!
//! A version number on the engine says nothing useful about what a program may
//! contain. A list of features does: each entry names one part of the admitted
//! language and the version of that part. The ordered digest of the list goes
//! into every unit image, so an image compiled against a different list is
//! refused rather than run against semantics it was not compiled for.
//!
//! The names are held inline rather than as references: a module image takes no
//! data relocations, so a table of pointers would not survive loading.

use crate::digest::{Digest, Hasher};

/// Bytes a feature name may take.
pub const NAME_MAX: usize = 24;

/// One admitted feature: what it is, and which version of it this build has.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Feature {
    pub name: [u8; NAME_MAX],
    pub length: u8,
    pub version: u16,
}

impl Feature {
    /// The name as text.
    pub fn name(&self) -> &[u8] {
        self.name.get(..self.length as usize).unwrap_or(&[])
    }
}

/// Build one entry, padding the name to a fixed width.
const fn feature(name: &[u8], version: u16) -> Feature {
    let mut bytes = [0u8; NAME_MAX];
    let mut index = 0usize;
    while index < name.len() && index < NAME_MAX {
        bytes[index] = name[index];
        index += 1;
    }
    Feature {
        name: bytes,
        length: index as u8,
        version,
    }
}

/// Every feature this build admits, in the order the digest takes them.
///
/// The order is the layers of the engine: what is scanned, what is parsed, and
/// what runs. Adding, removing, or versioning an entry changes the digest, and
/// therefore refuses every image compiled before the change.
pub const FEATURES: [Feature; 46] = [
    feature(b"lex.source-text", 1),
    feature(b"lex.comment", 1),
    feature(b"lex.identifier", 1),
    feature(b"lex.punctuator", 1),
    feature(b"lex.numeric", 1),
    feature(b"lex.bigint", 1),
    feature(b"lex.string", 1),
    feature(b"lex.template", 1),
    feature(b"lex.regexp-literal", 1),
    // Version 3 of the three syntax features whose surface grew: a BigInt
    // literal, a spread element, and a spread argument are all admitted now,
    // which the versions say so that an older image is refused.
    feature(b"syntax.primary", 3),
    feature(b"syntax.array", 3),
    feature(b"syntax.object", 2),
    feature(b"syntax.member", 3),
    feature(b"syntax.optional-chain", 1),
    feature(b"syntax.operators", 1),
    feature(b"syntax.conditional", 1),
    feature(b"syntax.assignment", 1),
    feature(b"syntax.sequence", 1),
    feature(b"syntax.statements", 1),
    feature(b"syntax.declarations", 1),
    feature(b"syntax.functions", 1),
    feature(b"syntax.iteration", 1),
    feature(b"syntax.regexp", 1),
    feature(b"syntax.modules", 1),
    feature(b"runtime.values", 1),
    feature(b"runtime.strings", 1),
    feature(b"runtime.objects", 1),
    feature(b"runtime.functions", 1),
    feature(b"runtime.errors", 1),
    feature(b"runtime.jobs", 1),
    feature(b"runtime.promises", 1),
    feature(b"runtime.bindings", 1),
    feature(b"runtime.collection", 1),
    feature(b"runtime.symbols", 1),
    feature(b"runtime.iteration", 1),
    feature(b"runtime.bigint", 1),
    feature(b"runtime.regexp", 1),
    feature(b"runtime.modules", 1),
    feature(b"library.object", 1),
    feature(b"library.array", 1),
    feature(b"library.string", 1),
    feature(b"library.number", 1),
    feature(b"library.math", 1),
    feature(b"library.function", 1),
    feature(b"library.promise", 1),
    feature(b"library.regexp", 1),
];

/// The digest of the admitted feature list.
pub fn digest() -> Digest {
    let mut hasher = Hasher::new();
    hasher.update(b"phasor.features.1");
    for entry in &FEATURES {
        hasher.update(&[entry.length]);
        hasher.update(entry.name());
        hasher.update(&entry.version.to_le_bytes());
    }
    hasher.finish()
}
