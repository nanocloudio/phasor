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

/// Every feature the engine has, in the order the digest takes them.
///
/// The order is the layers of the engine: what is scanned, what is parsed, what
/// runs, and the library on top. Adding, removing, or versioning an entry
/// changes the digest, and therefore refuses every image compiled before the
/// change.
pub const FEATURES: [Feature; 53] = [
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
    // Version 3 of objects: method definitions joined the literal.
    feature(b"syntax.object", 3),
    // Version 4 of members: `new` over a tagged template, and `import(...)`
    // with its `source` and `defer` phases.
    feature(b"syntax.member", 4),
    feature(b"syntax.optional-chain", 1),
    feature(b"syntax.operators", 1),
    feature(b"syntax.conditional", 1),
    feature(b"syntax.assignment", 1),
    feature(b"syntax.sequence", 1),
    // Version 2 of statements: `with` in sloppy code, and a `let` that is a
    // name where only a Statement may stand.
    feature(b"syntax.statements", 2),
    // Version 2 of declarations and functions: binding patterns, parameter
    // defaults, and rest parameters joined the admitted surface. Version 3
    // of declarations: `using`, and the legacy octal literals sloppy code
    // admits. Version 4: `await using`.
    feature(b"syntax.declarations", 4),
    feature(b"syntax.functions", 2),
    // Async functions, async arrows, and `await`.
    feature(b"syntax.async", 1),
    // Classes: declarations, expressions, heritage, super, and members.
    // Version 2: static blocks, and writes through `super`.
    feature(b"syntax.class", 2),
    // Generators: function*, yield, and yield*.
    feature(b"syntax.generator", 1),
    feature(b"syntax.iteration", 1),
    feature(b"syntax.regexp", 1),
    // Version 2 of modules: any IdentifierName as an exported or imported
    // name.
    feature(b"syntax.modules", 2),
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
    feature(b"library.json", 1),
    feature(b"library.date", 1),
    feature(b"library.buffers", 1),
    feature(b"runtime.proxy", 1),
];

/// The digest of the feature list.
///
/// Every entry goes in, whatever a given build carries. The list is the
/// language an image was compiled against — the syntax and the semantics of
/// what runs — and that is one language everywhere. A build may leave a
/// library area out (`omit_json`, `omit_regexp`) without touching it: an
/// omitted area is absent from the realm, which a program observes the way it
/// observes any host without that object, and an image whose bytecode needs
/// an omitted engine area is refused when it is admitted. Neither is a
/// different language, so neither is a different digest, and one image is
/// admitted by every build that can run it.
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
