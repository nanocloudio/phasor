//! What an image says it requires of its deployment.
//!
//! A capability import states a requirement: `import { readText } from
//! "phasor:filesystem"` says the program needs that binding, and the
//! deployment decides whether it is granted. The requirement is recorded in
//! the image's own import table, so nothing outside the image can claim one
//! for it, and a deployment that granted less refuses the image whole rather
//! than starting it and answering the call with `undefined`.

use crate::digest::Digest;

use crate::bytecode::Unit;

/// The scheme an image's import uses to name a capability rather than a
/// module. `import { readText } from "phasor:filesystem"` states a
/// requirement; the deployment decides whether it is granted.
pub const SCHEME: &[u8] = b"phasor:";

/// One binding an image says it requires: the interface its import named, and
/// the member it asked for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Requirement {
    /// The interface, as the specifier gave it after the scheme.
    pub interface: [u8; 32],
    pub interface_length: usize,
    /// The member the import asked for, which is the binding's own name.
    pub member: [u8; 32],
    pub member_length: usize,
}

impl Requirement {
    pub const EMPTY: Self = Self {
        interface: [0; 32],
        interface_length: 0,
        member: [0; 32],
        member_length: 0,
    };

    /// The interface as it was written.
    pub fn interface(&self) -> &[u8] {
        self.interface.get(..self.interface_length).unwrap_or(&[])
    }

    /// The member as it was written.
    pub fn member(&self) -> &[u8] {
        self.member.get(..self.member_length).unwrap_or(&[])
    }

    /// The digest of `interface.member`, which is the name a deployment
    /// grants and the only thing both ends agree on.
    pub fn name(&self) -> Digest {
        let mut hasher = crate::digest::Hasher::new();
        hasher.update(self.interface());
        hasher.update(b".");
        hasher.update(self.member());
        hasher.finish()
    }
}

/// Whether a specifier names a capability rather than a module.
pub fn is_capability(specifier: &[u16]) -> bool {
    if specifier.len() < SCHEME.len() {
        return false;
    }
    let mut index = 0usize;
    while index < SCHEME.len() {
        if specifier.get(index).copied() != Some(u16::from(SCHEME[index])) {
            return false;
        }
        index += 1;
    }
    true
}

/// The bindings an image states it requires: every import whose specifier
/// carries the capability scheme, in the order the image records them.
///
/// This is what makes a missing capability an admission failure rather than
/// an `undefined` a program discovers when it calls one. A deployment reads
/// the requirements, checks each against what it granted, and refuses the
/// image whole.
pub fn requirements(unit: &Unit<'_>, out: &mut [Requirement]) -> usize {
    let mut written = 0usize;
    let mut index = 0u32;
    while index < unit.header().import_count {
        let mut specifier = [0u16; 64];
        let mut member = [0u16; 64];
        let Some((specifier_length, member_length, _)) =
            unit.import_at(index, &mut specifier, &mut member)
        else {
            break;
        };
        index += 1;
        let written_specifier = specifier.get(..specifier_length).unwrap_or(&[]);
        if !is_capability(written_specifier) {
            continue;
        }
        let Some(slot) = out.get_mut(written) else {
            break;
        };
        let mut requirement = Requirement::EMPTY;
        // The interface is what follows the scheme; both it and the member
        // are ASCII, because a capability's name is a stable identifier.
        let mut at = SCHEME.len();
        while at < specifier_length && requirement.interface_length < requirement.interface.len() {
            let unit_value = written_specifier.get(at).copied().unwrap_or(0);
            if unit_value > 0x7F {
                break;
            }
            requirement.interface[requirement.interface_length] =
                u8::try_from(unit_value).unwrap_or(b'?');
            requirement.interface_length += 1;
            at += 1;
        }
        let mut at = 0usize;
        while at < member_length && requirement.member_length < requirement.member.len() {
            let unit_value = member.get(at).copied().unwrap_or(0);
            if unit_value > 0x7F {
                break;
            }
            requirement.member[requirement.member_length] =
                u8::try_from(unit_value).unwrap_or(b'?');
            requirement.member_length += 1;
            at += 1;
        }
        *slot = requirement;
        written += 1;
    }
    written
}
