//! Host bindings: the typed seam between a program and a capability.
//!
//! A program reaches the outside only through a binding the deployment
//! admitted. A binding names what it is, what schema its payloads follow, and
//! how many calls may be outstanding at once. The engine holds no provider
//! address, no credential, and no transport: it produces a call record and
//! consumes a completion record, and something else moves them.
//!
//! A call is asynchronous by construction. The engine never waits: it records a
//! pending call, hands the program a promise, and settles that promise when the
//! completion arrives. That is what keeps a synchronous language operation from
//! blocking a module step.

use crate::digest::Digest;
use crate::value::Value;

/// One admitted binding.
#[derive(Clone, Copy, Debug)]
pub struct Binding {
    /// The digest of the binding's name, which is what an image states it
    /// requires and what a deployment grants.
    pub name: Digest,
    /// The digest of the payload schema both ends check.
    pub schema: Digest,
    /// Calls that may be outstanding on this binding at once.
    pub in_flight_max: u32,
    /// Calls outstanding now.
    pub in_flight: u32,
}

impl Binding {
    pub const EMPTY: Self = Self {
        name: Digest([0; 32]),
        schema: Digest([0; 32]),
        in_flight_max: 0,
        in_flight: 0,
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
    /// The completion's payload does not match the binding's schema.
    SchemaMismatch,
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
    /// The payload did not match the binding's schema.
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
        }
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
/// correlation identifier, a trace context, and the digest of the payload the
/// caller staged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CallRecord {
    pub request: u64,
    pub binding: u32,
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
            trace: u64::from_le_bytes(<[u8; 8]>::try_from(frame.get(16..24)?).ok()?),
            payload: Digest(payload),
        })
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
    /// The value the provider produced, when it produced a number. A binding
    /// whose payloads are richer than that carries them by digest and a
    /// transfer, which this build does not implement.
    pub value: Option<f64>,
}

impl CompletionRecord {
    pub fn encode(&self) -> [u8; COMPLETION_FRAME] {
        let mut frame = [0u8; COMPLETION_FRAME];
        frame[0..8].copy_from_slice(&self.request.to_le_bytes());
        frame[8] = self.disposition as u8;
        frame[9] = self.cause as u8;
        let (kind, bits) = match self.value {
            Some(value) => (1u32, value.to_bits()),
            None => (0, 0),
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
            value: if kind == 1 {
                Some(f64::from_bits(bits))
            } else {
                None
            },
        })
    }
}
