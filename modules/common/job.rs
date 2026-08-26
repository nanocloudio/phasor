//! The job queue.
//!
//! ECMAScript jobs run one at a time, to completion, in the order they were
//! enqueued. The queue here is a bounded ring in caller-provided storage: a
//! full queue is an ordinary failure, never a growth, because the number of
//! jobs a task may create is part of what a deployment admits.

use crate::value::Value;

/// What a job does when it runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum JobKind {
    /// Call a handler with one argument, which is what a promise reaction is.
    Reaction = 0,
    /// Settle a promise with a value, which is what a resolution that had no
    /// handler does.
    Settle = 1,
    /// Follow a thenable: call its `then` with functions that settle the
    /// promise that adopted it. A promise resolved with another promise waits
    /// for that one rather than holding it as a value.
    Adopt = 2,
}

/// One queued job.
#[derive(Clone, Copy, Debug)]
pub struct Job {
    pub kind: JobKind,
    /// The function to call, or the promise to settle.
    pub target: Value,
    /// The argument to pass, or the value to settle with.
    pub argument: Value,
    /// The promise that receives the handler's result, if any.
    pub derived: Value,
}

impl Job {
    pub const EMPTY: Self = Self {
        kind: JobKind::Reaction,
        target: Value::UNDEFINED,
        argument: Value::UNDEFINED,
        derived: Value::UNDEFINED,
    };
}

/// Why a job was not enqueued.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueError {
    /// The queue is at its admitted size.
    Full,
}

/// A bounded first-in, first-out job queue.
/// The queue's own state, apart from the slots it was given.
#[derive(Clone, Copy, Debug, Default)]
pub struct QueueSave {
    head: usize,
    length: usize,
    enqueued: u64,
}

pub struct Queue<'a> {
    slots: &'a mut [Job],
    head: usize,
    length: usize,
    /// Jobs enqueued over the queue's whole life, which is what a caller
    /// reports rather than inferring from the current length.
    enqueued: u64,
}

impl<'a> Queue<'a> {
    /// The queue's state, to be restored onto the same slots later.
    pub const fn save(&self) -> QueueSave {
        QueueSave {
            head: self.head,
            length: self.length,
            enqueued: self.enqueued,
        }
    }

    /// Carry on from a saved state over the same slots.
    pub fn restore(&mut self, save: &QueueSave) {
        self.head = save.head;
        self.length = save.length;
        self.enqueued = save.enqueued;
    }
    pub fn new(slots: &'a mut [Job]) -> Self {
        Self {
            slots,
            head: 0,
            length: 0,
            enqueued: 0,
        }
    }

    pub const fn len(&self) -> usize {
        self.length
    }

    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub const fn enqueued(&self) -> u64 {
        self.enqueued
    }

    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Add a job to the end of the queue.
    pub fn push(&mut self, job: Job) -> Result<(), QueueError> {
        if self.slots.is_empty() || self.length == self.slots.len() {
            return Err(QueueError::Full);
        }
        let at = (self.head + self.length) % self.slots.len();
        let slot = self.slots.get_mut(at).ok_or(QueueError::Full)?;
        *slot = job;
        self.length += 1;
        self.enqueued = self.enqueued.saturating_add(1);
        Ok(())
    }

    /// Take the job at the front of the queue.
    /// The handles every queued job holds, which a collection must treat as
    /// roots: a job is scheduled work, and the work has not run yet.
    pub fn roots(&self, out: &mut [crate::value::Handle]) -> usize {
        let mut written = 0usize;
        let mut index = 0usize;
        while index < self.length {
            let at = (self.head + index) % self.slots.len().max(1);
            if let Some(job) = self.slots.get(at) {
                for value in [job.target, job.argument, job.derived] {
                    if matches!(
                        value.tag(),
                        crate::value::Tag::String
                            | crate::value::Tag::Symbol
                            | crate::value::Tag::BigInt
                            | crate::value::Tag::Object
                    ) {
                        if let Some(slot) = out.get_mut(written) {
                            *slot = value.as_handle();
                            written += 1;
                        }
                    }
                }
            }
            index += 1;
        }
        written
    }

    pub fn pop(&mut self) -> Option<Job> {
        if self.length == 0 {
            return None;
        }
        let job = *self.slots.get(self.head)?;
        self.head = (self.head + 1) % self.slots.len().max(1);
        self.length -= 1;
        Some(job)
    }
}
