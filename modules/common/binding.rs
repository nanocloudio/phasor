//! Host bindings: the typed seam between a program and a capability.
//!
//! A program reaches the outside only through a binding the deployment
//! admitted. A binding names what it is, how it is served, and how many calls
//! may be outstanding at once. The engine holds no provider
//! address, no credential, and no transport: it produces a call record and
//! consumes a completion record, and something else moves them.
//!
//! A call is asynchronous by construction. The engine never waits: it records a
//! pending call, hands the program a promise, and settles that promise when the
//! completion arrives. That is what keeps a synchronous language operation from
//! blocking a module step. The one exception is a snapshot binding, whose
//! value a host supplies before the task runs, so a program reads it with no
//! call at all — which is what lets a clock be a capability and still answer
//! synchronously.
//!
//! What crosses the boundary with a call is a payload: bytes, with their
//! digest in the record, staged behind the fixed frame and invisible until the
//! digest checks. A provider that answers with a resource rather than a value
//! answers with a handle: an index and a generation, meaningful only to the
//! binding that issued it, and stale for good once released.

use crate::digest::Digest;
use crate::value::Value;

/// How a binding is served, which decides what a call to it does.
///
/// The classes are the ones a capability can honestly offer. A provider that
/// answers over a channel is asynchronous and says so; presenting it as
/// synchronous would mean blocking a module step, which no binding may do. A
/// snapshot is the other way round: the fact is supplied before the task runs,
/// so reading it is local and needs no call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Class {
    /// The call returns a promise and completes through a later job.
    Async = 0,
    /// The host supplies the value before the task runs; a read is local and
    /// answers at once.
    Snapshot = 1,
}

impl Class {
    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Async),
            1 => Some(Self::Snapshot),
            _ => None,
        }
    }
}

/// One admitted binding.
#[derive(Clone, Copy, Debug)]
pub struct Binding {
    /// The digest of the binding's name, which is what an image states it
    /// requires and what a deployment grants.
    pub name: Digest,
    /// Calls that may be outstanding on this binding at once.
    pub in_flight_max: u32,
    /// Calls outstanding now.
    pub in_flight: u32,
    /// How the binding is served.
    pub class: Class,
    /// A snapshot binding's value, supplied before the task runs.
    pub snapshot: f64,
    /// The capability this binding belongs to. Handles are shared within it
    /// and nowhere else.
    ///
    /// A resource is opened by one member of an interface and used by
    /// others: `http#send` answers a response that `http#status`,
    /// `http#headers`, `http#read` and `http#close` all read. Scoping a
    /// handle to the MEMBER that issued it made those calls unresolvable,
    /// and the raw packed handle crossed to the provider instead — which
    /// only looked like it worked while the first handle of a run packed to
    /// zero and a provider's "is this my slot 0" check passed by accident.
    ///
    /// The capability is still the boundary: a handle from one grant is as
    /// stale as a released one when presented to another.
    pub scope: u32,
}

impl Binding {
    pub const EMPTY: Self = Self {
        name: Digest([0; 32]),
        in_flight_max: 0,
        in_flight: 0,
        class: Class::Async,
        snapshot: 0.0,
        scope: 0,
    };
}

/// Why a call was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallError {
    /// The binding is not one this isolate was granted.
    NotAdmitted,
    /// The binding already has as many calls outstanding as it admits.
    TooManyInFlight,
    /// The pending table is full.
    PendingFull,
    /// No pending call has that request identifier, or its generation is stale.
    UnknownRequest,
    /// The binding is not served the way the caller is treating it: a value
    /// supplied at the task boundary belongs to a snapshot, and a binding
    /// answered over a channel is not one.
    ClassMismatch,
    /// The handle names no live resource of this binding, or its generation
    /// is stale.
    StaleHandle,
    /// The handle table is full.
    HandlesFull,
}

/// One call waiting for its completion.
#[derive(Clone, Copy, Debug)]
pub struct Pending {
    /// The correlation identifier the call record carries.
    pub request: u64,
    /// The trace the call belongs to.
    pub trace: u64,
    /// The binding the call was made on.
    pub binding: u32,
    /// The promise that settles when the completion arrives.
    pub promise: Value,
    /// Whether the slot is in use.
    pub live: bool,
}

impl Pending {
    pub const EMPTY: Self = Self {
        request: 0,
        trace: 0,
        binding: 0,
        promise: Value::UNDEFINED,
        live: false,
    };
}

/// How a completion turned out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Disposition {
    /// The provider answered.
    Fulfilled = 0,
    /// The provider refused or failed, with a typed cause.
    Rejected = 1,
}

impl Disposition {
    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::Fulfilled),
            1 => Some(Self::Rejected),
            _ => None,
        }
    }
}

/// Why a provider refused or failed.
///
/// The cause is typed rather than a message, because a program and a policy
/// both have to act on it, and neither can act on prose. Retrying is the
/// caller's decision: the engine never retries a call on its own, because a
/// second call is a second use of a capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Cause {
    /// The call succeeded, so there is no cause.
    None = 0,
    /// The deployment refused this call.
    Denied = 1,
    /// The provider is not there.
    Unavailable = 2,
    /// The provider took too long.
    Timeout = 3,
    /// The call did not have the fields the member takes, or a field did not
    /// hold what it must. A provider checks the shape of what it is given
    /// rather than reading it as though it were right.
    Malformed = 4,
    /// The provider failed for its own reasons.
    Internal = 5,
    /// The provider was busy and the same call may be made again.
    Busy = 6,
}

impl Cause {
    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Self::None),
            1 => Some(Self::Denied),
            2 => Some(Self::Unavailable),
            3 => Some(Self::Timeout),
            4 => Some(Self::Malformed),
            5 => Some(Self::Internal),
            6 => Some(Self::Busy),
            _ => None,
        }
    }

    /// Whether making the same call again could succeed.
    ///
    /// This says what the cause means, not what the caller should do: the
    /// decision to retry, and any delay before it, belongs to the caller.
    /// The cause's name, held inline: a module image takes no relocations, so a
    /// table of static references would not survive loading.
    pub const fn name(self) -> ([u8; 11], usize) {
        match self {
            Self::None => (*b"None\0\0\0\0\0\0\0", 4),
            Self::Denied => (*b"Denied\0\0\0\0\0", 6),
            Self::Unavailable => (*b"Unavailable", 11),
            Self::Timeout => (*b"Timeout\0\0\0\0", 7),
            Self::Malformed => (*b"Malformed\0\0", 9),
            Self::Internal => (*b"Internal\0\0\0", 8),
            Self::Busy => (*b"Busy\0\0\0\0\0\0\0", 4),
        }
    }

    pub const fn retryable(self) -> bool {
        matches!(self, Self::Busy | Self::Timeout | Self::Unavailable)
    }
}

/// One resource a provider opened, as the program sees it.
///
/// A handle is an index and a generation, and nothing else: no descriptor, no
/// path, no address. The generation advances when the slot is released and
/// never wraps, so a handle kept past its resource's life names nothing rather
/// than naming whatever took the slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Handle {
    pub index: u32,
    pub generation: u32,
}

impl Handle {
    /// The handle as one number, which is how it crosses the boundary and how
    /// a program holds it. Both halves fit exactly, so nothing is lost.
    pub const fn pack(self) -> u64 {
        ((self.index as u64) << 32) | self.generation as u64
    }

    pub const fn unpack(packed: u64) -> Self {
        Self {
            index: (packed >> 32) as u32,
            generation: packed as u32,
        }
    }
}

/// One slot of the handle table: which capability issued the handle, what
/// the provider calls it, and whether it is live.
#[derive(Clone, Copy, Debug)]
pub struct Resource {
    /// The capability that issued the handle — not the member. Every member
    /// of that capability may use it, and no other capability may.
    pub scope: u32,
    /// The provider's own identifier for the resource, opaque here.
    pub token: u64,
    pub generation: u32,
    pub live: bool,
}

impl Resource {
    pub const EMPTY: Self = Self {
        scope: 0,
        token: 0,
        generation: 0,
        live: false,
    };
}

/// The admitted bindings and the calls outstanding on them.
///
/// Both tables are caller-provided: a deployment admits a fixed number of
/// bindings and a fixed number of outstanding calls, and neither grows.
/// The table's own state, apart from the storage it was given.
#[derive(Clone, Copy, Debug, Default)]
pub struct BindingsSave {
    admitted: usize,
    next_request: u64,
}

pub struct Bindings<'a> {
    bindings: &'a mut [Binding],
    admitted: usize,
    pending: &'a mut [Pending],
    /// The next correlation identifier, which never repeats within a run.
    next_request: u64,
    /// Resources providers opened, addressed by handle. Empty where a host
    /// attached none, which is a host whose bindings return values only.
    resources: Option<&'a mut [Resource]>,
}

impl<'a> Bindings<'a> {
    /// The table's state, to be restored onto the same storage later.
    pub const fn save(&self) -> BindingsSave {
        BindingsSave {
            admitted: self.admitted,
            next_request: self.next_request,
        }
    }

    /// Carry on from a saved state over the same descriptors and pending slots.
    pub fn restore(&mut self, save: &BindingsSave) {
        self.admitted = save.admitted;
        self.next_request = save.next_request;
    }
    pub fn new(bindings: &'a mut [Binding], pending: &'a mut [Pending]) -> Self {
        for slot in pending.iter_mut() {
            *slot = Pending::EMPTY;
        }
        Self {
            bindings,
            admitted: 0,
            pending,
            next_request: 1,
            resources: None,
        }
    }

    /// Attach the table a provider's handles live in. A host that admits a
    /// binding answering with resources attaches one; a host whose bindings
    /// answer with values needs none.
    pub fn attach_resources(&mut self, resources: &'a mut [Resource]) {
        for slot in resources.iter_mut() {
            *slot = Resource::EMPTY;
        }
        self.resources = Some(resources);
    }

    /// Take up a resource table a previous step left, with its handles intact:
    /// a file open across a pause stays open.
    pub fn adopt_resources(&mut self, resources: &'a mut [Resource]) {
        self.resources = Some(resources);
    }

    /// Record a resource a provider opened on `binding`, answering the handle
    /// the program holds it by.
    pub fn open(&mut self, binding: u32, token: u64) -> Result<Handle, CallError> {
        let scope = self.scope_of(binding).ok_or(CallError::NotAdmitted)?;
        let Some(resources) = self.resources.as_deref_mut() else {
            return Err(CallError::HandlesFull);
        };
        let mut scan = 0usize;
        while scan < resources.len() {
            let slot = &mut resources[scan];
            if !slot.live {
                slot.scope = scope;
                slot.token = token;
                slot.live = true;
                return Ok(Handle {
                    index: u32::try_from(scan).map_err(|_| CallError::HandlesFull)?,
                    generation: slot.generation,
                });
            }
            scan += 1;
        }
        Err(CallError::HandlesFull)
    }

    /// The capability a binding belongs to, or `None` when it is not one
    /// this isolate holds.
    fn scope_of(&self, binding: u32) -> Option<u32> {
        self.binding(binding).map(|descriptor| descriptor.scope)
    }

    /// The provider's own identifier for a live handle, presented on any
    /// member of the capability that opened it.
    ///
    /// A handle from another CAPABILITY is as stale as a released one: a
    /// resource is reachable only through the capability that opened it.
    /// Within one capability every member may use it, which is what lets a
    /// response opened by `send` be read by `status`.
    pub fn resolve(&self, binding: u32, handle: Handle) -> Result<u64, CallError> {
        let scope = self.scope_of(binding).ok_or(CallError::StaleHandle)?;
        let resources = self.resources.as_deref().ok_or(CallError::StaleHandle)?;
        let slot = resources
            .get(handle.index as usize)
            .ok_or(CallError::StaleHandle)?;
        if !slot.live || slot.generation != handle.generation || slot.scope != scope {
            return Err(CallError::StaleHandle);
        }
        Ok(slot.token)
    }

    /// Release a handle. The slot's generation advances, so the handle names
    /// nothing ever again.
    pub fn release(&mut self, binding: u32, handle: Handle) -> Result<u64, CallError> {
        let token = self.resolve(binding, handle)?;
        let resources = self
            .resources
            .as_deref_mut()
            .ok_or(CallError::StaleHandle)?;
        if let Some(slot) = resources.get_mut(handle.index as usize) {
            slot.live = false;
            slot.generation = slot.generation.wrapping_add(1);
        }
        Ok(token)
    }

    /// Handles live on a binding, which a host closes when a task ends.
    pub fn open_handles(&self, out: &mut [Handle]) -> usize {
        let Some(resources) = self.resources.as_deref() else {
            return 0;
        };
        let mut written = 0usize;
        for (index, slot) in resources.iter().enumerate() {
            if slot.live {
                if let Some(entry) = out.get_mut(written) {
                    *entry = Handle {
                        index: u32::try_from(index).unwrap_or(0),
                        generation: slot.generation,
                    };
                    written += 1;
                }
            }
        }
        written
    }

    /// Take up storage a table was already using, with its saved state. The
    /// pending slots are left as they were found: a call that is outstanding
    /// stays outstanding across a rebuild.
    pub fn adopt(
        bindings: &'a mut [Binding],
        pending: &'a mut [Pending],
        save: &BindingsSave,
    ) -> Self {
        Self {
            bindings,
            admitted: save.admitted,
            pending,
            next_request: save.next_request,
            resources: None,
        }
    }

    /// Admit a binding, returning the index a call names it by.
    pub fn admit(&mut self, binding: Binding) -> Result<u32, CallError> {
        let slot = self
            .bindings
            .get_mut(self.admitted)
            .ok_or(CallError::NotAdmitted)?;
        *slot = binding;
        let index = u32::try_from(self.admitted).map_err(|_| CallError::NotAdmitted)?;
        self.admitted += 1;
        Ok(index)
    }

    pub const fn admitted(&self) -> usize {
        self.admitted
    }

    pub fn binding(&self, index: u32) -> Option<&Binding> {
        if index as usize >= self.admitted {
            return None;
        }
        self.bindings.get(index as usize)
    }

    /// The index of an admitted binding, by the digest of its name. This is
    /// how an image's stated requirement meets what a deployment granted:
    /// the name is the only thing both ends agree on.
    pub fn index_of(&self, name: &Digest) -> Option<u32> {
        let mut index = 0usize;
        while index < self.admitted {
            if self.bindings.get(index).is_some_and(|b| &b.name == name) {
                return u32::try_from(index).ok();
            }
            index += 1;
        }
        None
    }

    /// Supply a snapshot binding's value for the task about to run.
    pub fn set_snapshot(&mut self, index: u32, value: f64) -> Result<(), CallError> {
        if index as usize >= self.admitted {
            return Err(CallError::NotAdmitted);
        }
        let slot = self
            .bindings
            .get_mut(index as usize)
            .ok_or(CallError::NotAdmitted)?;
        if slot.class != Class::Snapshot {
            return Err(CallError::ClassMismatch);
        }
        slot.snapshot = value;
        Ok(())
    }

    /// Calls outstanding across every binding.
    pub fn in_flight(&self) -> u32 {
        let mut total = 0u32;
        for slot in self.pending.iter() {
            if slot.live {
                total += 1;
            }
        }
        total
    }

    /// Begin a call on `binding`, recording the promise that will settle and
    /// the trace it belongs to.
    ///
    /// Returns the correlation identifier the call record carries.
    pub fn begin(&mut self, binding: u32, promise: Value, trace: u64) -> Result<u64, CallError> {
        let index = binding as usize;
        if index >= self.admitted {
            return Err(CallError::NotAdmitted);
        }
        {
            let descriptor = self.bindings.get(index).ok_or(CallError::NotAdmitted)?;
            if descriptor.in_flight >= descriptor.in_flight_max {
                return Err(CallError::TooManyInFlight);
            }
        }

        let mut slot_index = None;
        let mut scan = 0usize;
        while scan < self.pending.len() {
            if !self.pending[scan].live {
                slot_index = Some(scan);
                break;
            }
            scan += 1;
        }
        let Some(slot_index) = slot_index else {
            return Err(CallError::PendingFull);
        };

        let request = self.next_request;
        self.next_request = self.next_request.saturating_add(1);
        if let Some(slot) = self.pending.get_mut(slot_index) {
            *slot = Pending {
                request,
                trace,
                binding,
                promise,
                live: true,
            };
        }
        if let Some(descriptor) = self.bindings.get_mut(index) {
            descriptor.in_flight += 1;
        }
        Ok(request)
    }

    /// Take the pending call a completion answers.
    ///
    /// A request identifier is answered once: a repeated or unknown completion
    /// is refused rather than settling something twice.
    pub fn complete(&mut self, request: u64) -> Result<Pending, CallError> {
        let mut scan = 0usize;
        while scan < self.pending.len() {
            let slot = self.pending[scan];
            if slot.live && slot.request == request {
                if let Some(entry) = self.pending.get_mut(scan) {
                    entry.live = false;
                }
                if let Some(descriptor) = self.bindings.get_mut(slot.binding as usize) {
                    descriptor.in_flight = descriptor.in_flight.saturating_sub(1);
                }
                return Ok(slot);
            }
            scan += 1;
        }
        Err(CallError::UnknownRequest)
    }

    /// The identifiers of every call still outstanding.
    ///
    /// A host that has waited long enough for an answer needs to know what it
    /// is still waiting on, so it can time the calls out itself rather than
    /// waiting for a provider that may never answer.
    pub fn outstanding(&self, out: &mut [u64]) -> usize {
        let mut written = 0usize;
        for slot in self.pending.iter() {
            if slot.live {
                if let Some(entry) = out.get_mut(written) {
                    *entry = slot.request;
                    written += 1;
                }
            }
        }
        written
    }

    /// The binding an outstanding call was made on, which is what a
    /// resource it opens is recorded against.
    pub fn binding_of(&self, request: u64) -> Option<u32> {
        for slot in self.pending.iter() {
            if slot.live && slot.request == request {
                return Some(slot.binding);
            }
        }
        None
    }

    /// The trace an outstanding call belongs to.
    pub fn trace_of(&self, request: u64) -> Option<u64> {
        for slot in self.pending.iter() {
            if slot.live && slot.request == request {
                return Some(slot.trace);
            }
        }
        None
    }

    /// The promises of every outstanding call, which a collection must treat as
    /// roots: nothing else refers to them until their completion arrives.
    pub fn roots(&self, out: &mut [crate::value::Handle]) -> usize {
        let mut written = 0usize;
        for slot in self.pending.iter() {
            if slot.live && slot.promise.is_object() {
                if let Some(entry) = out.get_mut(written) {
                    *entry = slot.promise.as_handle();
                    written += 1;
                }
            }
        }
        written
    }
}

/// The record a call produces, which something else carries to a provider.
///
/// It holds no address, no credential, and no pointer: a binding index, a
/// correlation identifier, a trace context, and the digest and length of the
/// payload the caller staged. The payload's bytes follow the frame on the same
/// port; the digest is what makes them the caller's bytes rather than whatever
/// arrived.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CallRecord {
    pub request: u64,
    pub binding: u32,
    /// Bytes of payload following this frame. Zero for a call that carries
    /// nothing but its arguments' digest.
    pub payload_length: u32,
    /// The trace this call belongs to, which a completion carries back so a
    /// deployment can correlate the two without inspecting either payload.
    pub trace: u64,
    pub payload: Digest,
}

/// Bytes in an encoded call record.
pub const CALL_FRAME: usize = 56;
/// Bytes in an encoded completion record.
pub const COMPLETION_FRAME: usize = 32;

impl CallRecord {
    /// Encode the record as one fixed-width frame: little-endian integers, no
    /// padding beyond what is named, and no pointer.
    pub fn encode(&self) -> [u8; CALL_FRAME] {
        let mut frame = [0u8; CALL_FRAME];
        frame[0..8].copy_from_slice(&self.request.to_le_bytes());
        frame[8..12].copy_from_slice(&self.binding.to_le_bytes());
        frame[12..16].copy_from_slice(&self.payload_length.to_le_bytes());
        frame[16..24].copy_from_slice(&self.trace.to_le_bytes());
        frame[24..56].copy_from_slice(&self.payload.0);
        frame
    }

    /// Decode a frame, or nothing when it is not one.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let frame = <[u8; CALL_FRAME]>::try_from(bytes.get(..CALL_FRAME)?).ok()?;
        let mut payload = [0u8; 32];
        payload.copy_from_slice(frame.get(24..56)?);
        Some(Self {
            request: u64::from_le_bytes(<[u8; 8]>::try_from(frame.get(0..8)?).ok()?),
            binding: u32::from_le_bytes(<[u8; 4]>::try_from(frame.get(8..12)?).ok()?),
            payload_length: u32::from_le_bytes(<[u8; 4]>::try_from(frame.get(12..16)?).ok()?),
            trace: u64::from_le_bytes(<[u8; 8]>::try_from(frame.get(16..24)?).ok()?),
            payload: Digest(payload),
        })
    }
}

/// What a provider answered with: nothing, a number, bytes, or a resource.
///
/// Bytes travel behind the frame the way a call's payload does. A resource is
/// a handle the binding issued, which the program can pass back and nothing
/// else can forge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Answer {
    /// The call succeeded and produced no value.
    None,
    /// A number, which is the whole answer.
    Number(f64),
    /// Bytes, of the given length, following the frame.
    Payload(u32),
    /// A resource the provider opened, as the packed handle.
    Resource(u64),
}

impl Answer {
    /// Bytes of payload following the frame, which is none for every answer
    /// that is not one.
    pub const fn payload_length(self) -> u32 {
        match self {
            Self::Payload(length) => length,
            _ => 0,
        }
    }
}

/// What a host says about a call it answered.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompletionRecord {
    pub request: u64,
    pub disposition: Disposition,
    pub cause: Cause,
    /// The trace the call carried, returned unchanged.
    pub trace: u64,
    /// What the provider produced.
    pub answer: Answer,
}

impl CompletionRecord {
    pub fn encode(&self) -> [u8; COMPLETION_FRAME] {
        let mut frame = [0u8; COMPLETION_FRAME];
        frame[0..8].copy_from_slice(&self.request.to_le_bytes());
        frame[8] = self.disposition as u8;
        frame[9] = self.cause as u8;
        let (kind, bits) = match self.answer {
            Answer::None => (0u32, 0u64),
            Answer::Number(value) => (1, value.to_bits()),
            Answer::Payload(length) => (2, u64::from(length)),
            Answer::Resource(handle) => (3, handle),
        };
        frame[12..16].copy_from_slice(&kind.to_le_bytes());
        frame[16..24].copy_from_slice(&bits.to_le_bytes());
        frame[24..32].copy_from_slice(&self.trace.to_le_bytes());
        frame
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let frame = <[u8; COMPLETION_FRAME]>::try_from(bytes.get(..COMPLETION_FRAME)?).ok()?;
        let kind = u32::from_le_bytes(<[u8; 4]>::try_from(frame.get(12..16)?).ok()?);
        let bits = u64::from_le_bytes(<[u8; 8]>::try_from(frame.get(16..24)?).ok()?);
        Some(Self {
            request: u64::from_le_bytes(<[u8; 8]>::try_from(frame.get(0..8)?).ok()?),
            disposition: Disposition::from_byte(frame[8])?,
            cause: Cause::from_byte(frame[9])?,
            trace: u64::from_le_bytes(<[u8; 8]>::try_from(frame.get(24..32)?).ok()?),
            answer: match kind {
                1 => Answer::Number(f64::from_bits(bits)),
                2 => Answer::Payload(u32::try_from(bits).ok()?),
                3 => Answer::Resource(bits),
                _ => Answer::None,
            },
        })
    }
}
