//! Eval-site records: what a direct `eval` call could see.
//!
//! The machine cannot resolve a name at run time — environments hold slots,
//! not names — so the compiler records, for every direct `eval` call site,
//! the bindings that were statically visible there: each name, the slot it
//! lives in, and how many contexts up from the call site's innermost context
//! that slot sits. A host that compiles an eval source resolves the source's
//! free names against this record, and the compiled unit then runs over the
//! caller's own environments.
//!
//! The blob is advisory to the host and never executed: a wrong record can
//! only make eval'd code resolve to a wrong-but-bounded slot, which the
//! environment layer refuses like any other bad index.
//!
//! Layout, all little-endian:
//!
//! ```text
//! count: u32
//! count × site:
//!   function: u32 | pc: u32 | flags: u32 | bindings: u32 | var_depth: u32
//!   bindings × { slot: u32 | depth: u32 | kind: u32 | length: u32 | name bytes }
//! ```
//!
//! `var_depth` is the context depth, from the site, of the variable
//! environment its sloppy eval code declares into — `u32::MAX` when that
//! environment is the global object.

/// The site's code is strict, so the eval source is strict before its own
/// directive says anything.
pub const FLAG_STRICT: u32 = 1 << 0;
/// More was visible than the record could hold. A host treats the site as if
/// it had no record rather than resolving against half a scope.
pub const FLAG_TRUNCATED: u32 = 1 << 1;
/// The site sits in function code, so the eval's variable environment is a
/// function environment, which sloppy eval code may not `var`-declare
/// `arguments` into.
pub const FLAG_FUNCTION: u32 = 1 << 2;
/// The site sits in code with a home object — a method, an accessor, a
/// class constructor, or a field initialiser — so the eval source may
/// reference `super.name`.
pub const FLAG_SUPER_PROPERTY: u32 = 1 << 3;
/// The site sits in a derived class constructor, so the eval source may
/// call `super()`.
pub const FLAG_SUPER_CALL: u32 = 1 << 4;
/// The site sits in function code proper, so the eval source may read
/// `new.target`.
pub const FLAG_NEW_TARGET: u32 = 1 << 5;
/// The site sits in a class field initialiser, whose code — the eval
/// source included — may not reference `arguments`.
pub const FLAG_NO_ARGUMENTS: u32 = 1 << 6;
/// The site can see a private scope, so the eval source may reference
/// private members.
pub const FLAG_PRIVATES: u32 = 1 << 7;
/// The site sits in a parameter initialiser — arrow or not — whose
/// parameter bindings a sloppy eval may not `var`-redeclare.
pub const FLAG_PARAMETERS: u32 = 1 << 8;

/// One binding a host hands back to the compiler.
#[derive(Clone, Copy)]
pub struct EvalBinding<'a> {
    pub name: &'a [u8],
    pub slot: u32,
    pub depth: u32,
    pub kind: u32,
}

/// One decoded site.
#[derive(Clone, Copy)]
pub struct Site<'a> {
    pub flags: u32,
    bindings: &'a [u8],
    pub binding_count: u32,
    /// The context depth of the site's variable environment, from the site.
    pub var_depth: u32,
}

impl<'a> Site<'a> {
    /// The bindings, decoded into the caller's storage; answers how many fit.
    pub fn bindings(&self, out: &mut [EvalBinding<'a>]) -> usize {
        let mut at = 0usize;
        let mut written = 0usize;
        let mut index = 0u32;
        while index < self.binding_count {
            let Some(slot) = read_u32(self.bindings, at) else {
                break;
            };
            let Some(depth) = read_u32(self.bindings, at + 4) else {
                break;
            };
            let Some(kind) = read_u32(self.bindings, at + 8) else {
                break;
            };
            let Some(length) = read_u32(self.bindings, at + 12) else {
                break;
            };
            let Some(next) = advance(self.bindings, at, length) else {
                break;
            };
            let Some(name) = self.bindings.get(at + 16..next) else {
                break;
            };
            if let Some(target) = out.get_mut(written) {
                *target = EvalBinding {
                    name,
                    slot,
                    depth,
                    kind,
                };
                written += 1;
            }
            at = next;
            index += 1;
        }
        written
    }
}

/// Find the site recorded for `(function, pc)`, if the blob holds one.
pub fn find(blob: &[u8], function: u32, pc: u32) -> Option<Site<'_>> {
    let count = read_u32(blob, 0)?;
    let mut at = 4usize;
    let mut index = 0u32;
    while index < count {
        let site_function = read_u32(blob, at)?;
        let site_pc = read_u32(blob, at + 4)?;
        let flags = read_u32(blob, at + 8)?;
        let bindings = read_u32(blob, at + 12)?;
        let var_depth = read_u32(blob, at + 16)?;
        let start = at + 20;
        let mut cursor = start;
        let mut binding = 0u32;
        while binding < bindings {
            let length = read_u32(blob, cursor + 12)?;
            cursor = advance(blob, cursor, length)?;
            binding += 1;
        }
        if site_function == function && site_pc == pc {
            return Some(Site {
                flags,
                bindings: blob.get(start..cursor)?,
                binding_count: bindings,
                var_depth,
            });
        }
        at = cursor;
        index += 1;
    }
    None
}

/// Where the record after a binding of `length` name bytes begins, when the
/// blob is long enough to hold it. A length the blob cannot cover is no
/// record — and because every cursor comes from here, no offset in this
/// module can run past the blob or wrap the address space.
fn advance(blob: &[u8], at: usize, length: u32) -> Option<usize> {
    let next = at
        .checked_add(16)?
        .checked_add(usize::try_from(length).ok()?)?;
    (next <= blob.len()).then_some(next)
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..)?.get(..4)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}
