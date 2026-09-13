//! What an image says it requires of its deployment.
//!
//! A capability import states a requirement: `import { connect } from
//! "wasi:sockets/tcp"` says the program needs that binding, and the
//! deployment decides whether it is granted. The requirement is recorded in
//! the image's own import table, so nothing outside the image can claim one
//! for it, and a deployment that granted less refuses the image whole rather
//! than starting it and answering the call with `undefined`.
//!
//! # The name both ends agree on
//!
//! A binding is named by the digest of `<interface>#<member>`, where the
//! interface is the import's specifier **including its scheme**. The scheme
//! is part of the identity and not decoration: `wasi:sockets/tcp` is the
//! published interface and `phasor:sockets/tcp` would be this project's own,
//! and they are not the same capability. `docs/reference/capability-register.md`
//! is the register, and this is the name it records.
//!
//! The separator cannot appear in either part, because the grammar below
//! excludes it. That is what makes the name injective: without it,
//! `("a#b", "c")` and `("a", "b#c")` would be one name for two capabilities.
//!
//! # The grammar
//!
//! ```text
//! interface := scheme ":" path
//! scheme    := [a-z] [a-z0-9-]*
//! path      := [a-z0-9] [a-z0-9/._@-]*
//! member    := [A-Za-z] [A-Za-z0-9]*
//! ```
//!
//! The interface is the whole of that production, scheme included, which is
//! what the digest covers.
//!
//! Anything else is refused. A requirement that cannot be read is not a
//! requirement that is absent: every failure here refuses the image, because
//! the alternative is a shorter list, and a shorter list is always the more
//! permissive one.

use crate::digest::Digest;

use crate::bytecode::Unit;

/// What separates an interface from its member in the name both ends digest.
/// Excluded from both parts by the grammar, so the join is unambiguous.
pub const SEPARATOR: u8 = b'#';

/// The longest interface, member, and import specifier this reads. A name
/// longer than it is refused rather than truncated: two capabilities sharing
/// a prefix must not share an identity.
///
/// Sixty-four rather than thirty-two because a versioned WASI name reaches
/// the smaller figure exactly -- `wasi:http/outgoing-handler@0.2.0` is
/// thirty-two characters -- and a bound a real name sits on is a bound that
/// refuses the next one for no reason anybody chose.
pub const MAX_INTERFACE: usize = 64;
pub const MAX_MEMBER: usize = 32;
/// The longest import specifier or name this reads.
///
/// Generous for a module specifier, which in a closure is a short name or a
/// relative path, and far past any capability name, which `MAX_INTERFACE`
/// bounds at a quarter of it. An import longer than this refuses the image:
/// the reader cannot tell whether it named a capability without reading it,
/// so it cannot be passed over.
///
/// It is deliberately within what the front end will emit, so the refusal is
/// reachable from a program rather than only from a crafted image. A bound
/// no test can arrive at is a bound nobody can check.
const MAX_UNITS: usize = 128;

/// Why an image's stated requirements could not be read.
///
/// Every one refuses the image. A requirement that cannot be read is not
/// absent, and treating it as absent admits exactly the image that could not
/// be checked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    /// An import could not be read at all: its specifier or name is longer
    /// than this reads, or the table itself is malformed.
    Unreadable,
    /// The specifier does not follow the grammar, or is longer than a name
    /// may be.
    Specifier,
    /// The member does not follow the grammar, or is longer than a name may
    /// be.
    Member,
    /// The image states more requirements than the caller left room for.
    TooMany,
}

/// One binding an image says it requires: the interface its import named, and
/// the member it asked for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Requirement {
    /// The interface, scheme included, exactly as the specifier gave it.
    pub interface: [u8; MAX_INTERFACE],
    pub interface_length: usize,
    /// The member the import asked for, which is the binding's own name.
    pub member: [u8; MAX_MEMBER],
    pub member_length: usize,
}

impl Requirement {
    pub const EMPTY: Self = Self {
        interface: [0; MAX_INTERFACE],
        interface_length: 0,
        member: [0; MAX_MEMBER],
        member_length: 0,
    };

    /// The interface as it was written, scheme included.
    pub fn interface(&self) -> &[u8] {
        self.interface.get(..self.interface_length).unwrap_or(&[])
    }

    /// The member as it was written.
    pub fn member(&self) -> &[u8] {
        self.member.get(..self.member_length).unwrap_or(&[])
    }

    /// The digest of `interface#member`, which is the name a deployment
    /// grants and the only thing both ends agree on.
    pub fn name(&self) -> Digest {
        name_of(self.interface(), self.member())
    }
}

/// The name a deployment grants a capability by, from the two parts that make
/// it. One function, so the requiring end and the granting end cannot drift.
pub fn name_of(interface: &[u8], member: &[u8]) -> Digest {
    let mut hasher = crate::digest::Hasher::new();
    hasher.update(interface);
    hasher.update(&[SEPARATOR]);
    hasher.update(member);
    hasher.finish()
}

/// Whether a byte may open a scheme.
const fn scheme_start(byte: u8) -> bool {
    byte.is_ascii_lowercase()
}

/// Whether a byte may continue a scheme.
const fn scheme_body(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
}

/// Whether a byte may appear in an interface, after its first.
const fn interface_body(byte: u8) -> bool {
    byte.is_ascii_lowercase()
        || byte.is_ascii_digit()
        || matches!(byte, b'/' | b'.' | b'_' | b'@' | b'-')
}

/// Whether a specifier names a capability rather than a module.
///
/// A capability specifier carries a scheme: a run of lowercase letters,
/// digits and hyphens, opened by a letter, followed by a colon. That is the
/// shape rather than a list, so `wasi:` and `phasor:` and any scheme a later
/// interface takes are all recognised without one being privileged. A module
/// specifier is a relative path or a bare name and carries no scheme, and
/// this engine resolves no specifier over a network, so a scheme can only
/// mean a capability.
pub fn is_capability(specifier: &[u16]) -> bool {
    let Some(&first) = specifier.first() else {
        return false;
    };
    if first > 0x7F || !scheme_start(first as u8) {
        return false;
    }
    let mut index = 1usize;
    while index < specifier.len() {
        let unit = specifier.get(index).copied().unwrap_or(0);
        if unit == u16::from(b':') {
            // A scheme with nothing after it names no interface.
            return index + 1 < specifier.len();
        }
        if unit > 0x7F || !scheme_body(unit as u8) {
            return false;
        }
        index += 1;
    }
    false
}

/// Copy an ASCII run into a fixed field, refusing anything the grammar does
/// not admit and anything longer than the field.
fn take(
    units: &[u16],
    out: &mut [u8],
    start: usize,
    admits: impl Fn(usize, u8) -> bool,
) -> Option<usize> {
    let mut length = 0usize;
    let mut index = start;
    while index < units.len() {
        let unit = units.get(index).copied().unwrap_or(0);
        if unit > 0x7F {
            return None;
        }
        let byte = unit as u8;
        if !admits(index - start, byte) {
            return None;
        }
        let slot = out.get_mut(length)?;
        *slot = byte;
        length += 1;
        index += 1;
    }
    if length == 0 {
        return None;
    }
    Some(length)
}

/// The bindings an image states it requires: every import whose specifier
/// carries a scheme, in the order the image records them.
///
/// This is what makes a missing capability an admission failure rather than
/// an `undefined` a program discovers when it calls one. A deployment reads
/// the requirements, checks each against what it granted, and refuses the
/// image whole.
///
/// # Errors
///
/// Refuses rather than answering a shorter list. A specifier too long to
/// read, a name too long to hold, a byte the grammar does not admit, or more
/// requirements than the caller left room for are each a reason the image
/// cannot be checked, and an image that cannot be checked is not admitted.
pub fn requirements_of(
    specifier: &[u16],
    member: &[u16],
    out: &mut [Requirement],
) -> Result<usize, Refusal> {
    if !is_capability(specifier) {
        return Ok(0);
    }
    let Some(slot) = out.get_mut(0) else {
        return Err(Refusal::TooMany);
    };
    *slot = read_one(specifier, member)?;
    Ok(1)
}

/// One requirement from one import's two halves, held to the grammar.
fn read_one(specifier: &[u16], member: &[u16]) -> Result<Requirement, Refusal> {
    let mut requirement = Requirement::EMPTY;
    let mut colon = 0usize;
    while colon < specifier.len() && specifier.get(colon).copied().unwrap_or(0) != u16::from(b':') {
        colon += 1;
    }
    // The interface is the whole specifier, scheme included: the scheme says
    // whose interface it is, and two schemes naming the same path are two
    // capabilities.
    let Some(interface_length) = take(specifier, &mut requirement.interface, 0, |at, byte| {
        if at < colon {
            return if at == 0 {
                scheme_start(byte)
            } else {
                scheme_body(byte)
            };
        }
        if at == colon {
            return byte == b':';
        }
        if at == colon + 1 {
            return byte.is_ascii_lowercase() || byte.is_ascii_digit();
        }
        interface_body(byte)
    }) else {
        return Err(Refusal::Specifier);
    };
    requirement.interface_length = interface_length;
    let Some(member_length) = take(member, &mut requirement.member, 0, |at, byte| {
        if at == 0 {
            byte.is_ascii_alphabetic()
        } else {
            byte.is_ascii_alphanumeric()
        }
    }) else {
        return Err(Refusal::Member);
    };
    requirement.member_length = member_length;
    Ok(requirement)
}

pub fn requirements(unit: &Unit<'_>, out: &mut [Requirement]) -> Result<usize, Refusal> {
    let mut written = 0usize;
    let mut index = 0u32;
    while index < unit.header().import_count {
        let mut specifier = [0u16; MAX_UNITS];
        let mut member = [0u16; MAX_UNITS];
        // An import this cannot read is not an import that requires nothing.
        let Some((specifier_length, member_length, _)) =
            unit.import_at(index, &mut specifier, &mut member)
        else {
            return Err(Refusal::Unreadable);
        };
        index += 1;
        let written_specifier = specifier.get(..specifier_length).unwrap_or(&[]);
        if !is_capability(written_specifier) {
            continue;
        }
        let Some(slot) = out.get_mut(written) else {
            return Err(Refusal::TooMany);
        };
        let requirement = read_one(
            written_specifier,
            member.get(..member_length).unwrap_or(&[]),
        )?;
        *slot = requirement;
        written += 1;
    }
    Ok(written)
}
