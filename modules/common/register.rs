//! The capability register, as a table both ends of the seam read.
//!
//! `docs/reference/capability-register.md` names every interface a
//! deployment may grant and every member of each. This file is that page as
//! code: for an interface, its members, each with the name a program calls
//! it by, how it is served, and the method number the adapter that serves
//! the interface knows it as. The shell reads it to build the namespaces a
//! grant admits; the router reads it to turn the binding an isolate called
//! into the method an adapter answers to. Two readers of one table cannot
//! disagree about what a member is called.
//!
//! Names are held inline rather than borrowed: a module image takes no
//! relocations, so a table of static references would not survive loading.
//!
//! Nothing here is borrowed from elsewhere in the tree: this file names no
//! other, so a module mounts it on its own. A member says whether it is a
//! snapshot rather than naming `binding::Class`, and the one reader that
//! builds bindings makes that into a class. The register is a page of facts,
//! and a page of facts should not drag a machine behind it.

/// What separates an interface from its member in the name both ends digest.
/// Excluded from both parts by the grammar `capability.rs` holds, so the
/// join is unambiguous and one entry of a grant list splits exactly once.
pub const SEPARATOR: u8 = b'#';

/// The longest interface and member a name may be built from. A name longer
/// than either is refused rather than truncated: two capabilities sharing a
/// prefix must not share an identity.
///
/// Sixty-four rather than thirty-two because a versioned WASI name reaches
/// the smaller figure exactly -- `wasi:http/outgoing-handler@0.2.0` is
/// thirty-two characters -- and a bound a real name sits on is a bound that
/// refuses the next one for no reason anybody chose.
pub const MAX_INTERFACE: usize = 64;
pub const MAX_MEMBER: usize = 32;

/// Members one interface may have.
pub const MAX_MEMBERS: usize = 8;

/// The longest grant list a deployment can state.
///
/// A module parameter's length is one byte on the wire, so this is every
/// byte that can reach a module however much room it makes for them, and a
/// longer list does not arrive cut short -- the length wraps, and what lands
/// is not what was written. The bound is stated at the true figure so that
/// the isolate and the router hold the same text, which is the whole basis
/// on which one numbers a binding and the other names it.
pub const GRANTS_BYTES: usize = 255;

/// One member of an interface: the name a program calls it by, how it is
/// served, and the method number the adapter knows it as.
#[derive(Clone, Copy)]
pub struct Member {
    pub name: [u8; 8],
    pub name_length: usize,
    /// Whether the host supplies this member's value at the task boundary
    /// rather than answering a call for it. A snapshot is read with no call
    /// and no channel round trip; everything else is a call.
    pub snapshot: bool,
    pub method: u32,
}

impl Member {
    pub const EMPTY: Self = Self {
        name: [0; 8],
        name_length: 0,
        snapshot: false,
        method: 0,
    };

    const fn new(name: [u8; 8], name_length: usize, snapshot: bool, method: u32) -> Self {
        Self {
            name,
            name_length,
            snapshot,
            method,
        }
    }

    pub fn name(&self) -> &[u8] {
        self.name.get(..self.name_length).unwrap_or(&[])
    }
}

/// The members of an interface, by the identifier the register names it by.
/// An interface the register does not hold has none.
///
/// A clock is a fact supplied at a task boundary, so its `now` is a snapshot
/// and a program reads it with no call. Everything else is a call.
pub fn members(interface: &[u8]) -> ([Member; MAX_MEMBERS], usize) {
    let mut out = [Member::EMPTY; MAX_MEMBERS];
    let count = match interface {
        b"wasi:clocks/wall-clock" => {
            out[0] = Member::new(*b"now\0\0\0\0\0", 3, true, 0);
            // Waiting is a call: the adapter holds it until the delay has
            // passed, and the promise it answers is what a timer is built on.
            out[1] = Member::new(*b"sleep\0\0\0", 5, false, 1);
            2
        }
        b"wasi:random/random" => {
            out[0] = Member::new(*b"random\0\0", 6, false, 0);
            1
        }
        b"wasi:http/outgoing-handler" => {
            out[0] = Member::new(*b"send\0\0\0\0", 4, false, 0);
            out[1] = Member::new(*b"status\0\0", 6, false, 1);
            out[2] = Member::new(*b"headers\0", 7, false, 2);
            out[3] = Member::new(*b"read\0\0\0\0", 4, false, 3);
            out[4] = Member::new(*b"close\0\0\0", 5, false, 4);
            out[5] = Member::new(*b"origins\0", 7, false, 5);
            6
        }
        b"phasor:net/websocket" => {
            out[0] = Member::new(*b"open\0\0\0\0", 4, false, 0);
            out[1] = Member::new(*b"send\0\0\0\0", 4, false, 1);
            out[2] = Member::new(*b"receive\0", 7, false, 2);
            out[3] = Member::new(*b"close\0\0\0", 5, false, 3);
            out[4] = Member::new(*b"origin\0\0", 6, false, 4);
            5
        }
        b"wasi:sockets/tcp" => {
            out[0] = Member::new(*b"connect\0", 7, false, 0);
            out[1] = Member::new(*b"send\0\0\0\0", 4, false, 1);
            out[2] = Member::new(*b"receive\0", 7, false, 2);
            out[3] = Member::new(*b"close\0\0\0", 5, false, 3);
            out[4] = Member::new(*b"endpoint", 8, false, 4);
            5
        }
        b"wasi:filesystem/types" => {
            out[0] = Member::new(*b"read\0\0\0\0", 4, false, 0);
            out[1] = Member::new(*b"write\0\0\0", 5, false, 1);
            out[2] = Member::new(*b"open\0\0\0\0", 4, false, 4);
            out[3] = Member::new(*b"readAt\0\0", 6, false, 5);
            out[4] = Member::new(*b"close\0\0\0", 5, false, 6);
            out[5] = Member::new(*b"size\0\0\0\0", 4, false, 7);
            6
        }
        b"phasor:store/keyvalue" => {
            out[0] = Member::new(*b"read\0\0\0\0", 4, false, 0);
            out[1] = Member::new(*b"write\0\0\0", 5, false, 1);
            out[2] = Member::new(*b"list\0\0\0\0", 4, false, 2);
            out[3] = Member::new(*b"delete\0\0", 6, false, 3);
            out[4] = Member::new(*b"open\0\0\0\0", 4, false, 4);
            out[5] = Member::new(*b"readAt\0\0", 6, false, 5);
            6
        }
        _ => 0,
    };
    (out, count)
}

/// The member an interface has under `name`, or nothing when the register
/// holds no such interface or the interface has no such member.
pub fn member_of(interface: &[u8], name: &[u8]) -> Option<Member> {
    let (table, count) = members(interface);
    table
        .get(..count)?
        .iter()
        .copied()
        .find(|member| member.name() == name)
}

/// The entries of a grant list, in order.
///
/// A deployment states what it grants as text: `<interface>#<member>`
/// entries separated by commas, with whatever space it likes around them.
/// The isolate admits a binding for each and the router maps each back to a
/// method, so both walk the text with this one function: an entry one of
/// them could read and the other could not would be a grant that was
/// admitted and could never be served, or the reverse.
///
/// `visit` is called with the interface and the member of every entry that
/// has both. An entry without the separator is not a grant and is passed
/// over; nothing here checks the grammar, which is the digest's business.
pub fn grants(names: &[u8], mut visit: impl FnMut(&[u8], &[u8])) {
    let mut at = 0usize;
    while at < names.len() {
        let mut end = at;
        while end < names.len() && names.get(end).copied().unwrap_or(b',') != b',' {
            end += 1;
        }
        let entry = trimmed(names.get(at..end).unwrap_or(&[]));
        if !entry.is_empty() {
            let mut cut = 0usize;
            while cut < entry.len() && entry.get(cut).copied().unwrap_or(0) != SEPARATOR {
                cut += 1;
            }
            if cut < entry.len() {
                visit(
                    entry.get(..cut).unwrap_or(&[]),
                    entry.get(cut + 1..).unwrap_or(&[]),
                );
            }
        }
        at = end + 1;
    }
}

/// `entry` without the spaces and tabs around it.
fn trimmed(entry: &[u8]) -> &[u8] {
    let mut start = 0usize;
    while start < entry.len() && matches!(entry.get(start).copied(), Some(b' ') | Some(b'\t')) {
        start += 1;
    }
    let mut end = entry.len();
    while end > start && matches!(entry.get(end - 1).copied(), Some(b' ') | Some(b'\t')) {
        end -= 1;
    }
    entry.get(start..end).unwrap_or(&[])
}
