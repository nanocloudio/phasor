//! The deterministic replay profile.
//!
//! A program that makes no provider call and runs under no deadline is a pure
//! function of its image and its policy: nothing outside the image can change
//! what it produces. This module makes that claim checkable. A run produces a
//! recording, which is the identity of what went in, everything that came from
//! outside, and a digest of what came out. Replaying the same image with the
//! same recording must produce the same outcome digest, and a run that used
//! anything the recording does not name is not deterministic.
//!
//! Collection, slice size, heap size, and the number of module steps are
//! deliberately not recorded, because none of them may change a result. A
//! difference in outcome across those is a defect, and the probe that varies
//! them is how it would be found.

use crate::digest::{Digest, Hasher};
use crate::policy::{Outcome, Policy};

/// Something that reached the isolate from outside during a run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    /// The host reported the current time.
    Time(u64),
    /// The host answered a call the program made.
    Completion { request: u32, payload: Digest },
    /// The host asked the task to stop.
    Cancellation,
}

/// Which profile a run was admitted under.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Profile {
    /// No provider call and no deadline: the outcome depends on the image and
    /// the policy alone.
    Deterministic,
    /// Provider calls or a deadline are admitted, so the outcome depends on
    /// what the host supplied, which the recording names.
    Observed,
}

/// The largest number of events a recording holds.
pub const MAX_EVENTS: usize = 64;

/// One run's identity, inputs, and result.
#[derive(Clone, Copy, Debug)]
pub struct Recording {
    pub profile: Profile,
    pub image: Digest,
    pub policy: Digest,
    events: [Event; MAX_EVENTS],
    count: usize,
    /// Whether more events happened than the recording could hold, which makes
    /// it unusable for a replay rather than quietly incomplete.
    overflowed: bool,
    outcome: Digest,
    finished: bool,
}

impl Recording {
    /// Begin recording a run of `image` under `policy`.
    pub fn new(profile: Profile, image: Digest, policy: &Policy) -> Self {
        Self {
            profile,
            image,
            policy: digest_policy(policy),
            events: [Event::Cancellation; MAX_EVENTS],
            count: 0,
            overflowed: false,
            outcome: Digest([0; 32]),
            finished: false,
        }
    }

    /// Note something that arrived from outside.
    pub fn record(&mut self, event: Event) {
        if self.count < self.events.len() {
            self.events[self.count] = event;
            self.count += 1;
        } else {
            self.overflowed = true;
        }
    }

    /// The events, in the order they arrived.
    pub fn events(&self) -> &[Event] {
        match self.events.get(..self.count) {
            Some(slice) => slice,
            None => &[],
        }
    }

    /// Whether the recording holds every event of its run.
    pub const fn complete(&self) -> bool {
        !self.overflowed
    }

    /// Close the recording with the digest of what the run produced.
    pub fn finish(&mut self, outcome: Digest) {
        self.outcome = outcome;
        self.finished = true;
    }

    pub const fn outcome(&self) -> Digest {
        self.outcome
    }

    /// Whether another run reproduced this one.
    ///
    /// Two runs match when they ran the same image under the same policy, saw
    /// the same events in the same order, and produced the same outcome. A
    /// recording that overflowed matches nothing, because it does not describe
    /// its own run.
    pub fn reproduces(&self, other: &Self) -> bool {
        if !self.complete() || !other.complete() || !self.finished || !other.finished {
            return false;
        }
        self.profile == other.profile
            && self.image == other.image
            && self.policy == other.policy
            && self.outcome == other.outcome
            && self.events() == other.events()
    }

    /// The digest of the whole recording, which identifies the run.
    pub fn digest(&self) -> Digest {
        let mut hasher = Hasher::new();
        hasher.update(&[u8::from(matches!(self.profile, Profile::Observed))]);
        hasher.update(&self.image.0);
        hasher.update(&self.policy.0);
        for event in self.events() {
            match event {
                Event::Time(value) => {
                    hasher.update(&[1]);
                    hasher.update(&value.to_le_bytes());
                }
                Event::Completion { request, payload } => {
                    hasher.update(&[2]);
                    hasher.update(&request.to_le_bytes());
                    hasher.update(&payload.0);
                }
                Event::Cancellation => hasher.update(&[3]),
            }
        }
        hasher.update(&self.outcome.0);
        hasher.finish()
    }
}

/// The digest of a policy, which is part of a run's identity because a
/// different budget can end a run differently.
pub fn digest_policy(policy: &Policy) -> Digest {
    let mut hasher = Hasher::new();
    hasher.update(&policy.heap_bytes.to_le_bytes());
    hasher.update(&policy.heap_cells.to_le_bytes());
    hasher.update(&policy.fuel.to_le_bytes());
    hasher.update(&policy.frames.to_le_bytes());
    hasher.update(&policy.registers.to_le_bytes());
    hasher.update(&policy.jobs.to_le_bytes());
    hasher.update(&policy.pending_calls.to_le_bytes());
    hasher.update(&policy.image_bytes.to_le_bytes());
    hasher.update(&policy.deadline_ms.to_le_bytes());
    hasher.finish()
}

/// The digest of an outcome and the text of the value it produced.
///
/// A value is identified by its printed form rather than by its address, so two
/// runs that built equal values in different cells agree.
pub fn digest_outcome(outcome: Outcome, text: &[u16]) -> Digest {
    let mut hasher = Hasher::new();
    hasher.update(&[outcome as u8]);
    for &unit in text {
        hasher.update(&unit.to_le_bytes());
    }
    hasher.finish()
}
