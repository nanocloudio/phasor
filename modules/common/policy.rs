//! The isolate's resource policy and lifecycle contract.
//!
//! An isolate is admitted against a policy: a set of limits, each with a
//! compiled-in ceiling that a deployment may lower and can never raise. Every
//! limit has one stable outcome when it is reached, and those outcomes are the
//! whole vocabulary a caller has to reason about: nothing here can panic, and
//! nothing can quietly take more than it was admitted for.

/// What an isolate is doing.
///
/// The states are observable: a caller can see which one an isolate is in, and
/// which transitions are possible, without inspecting its contents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum State {
    /// Created, with storage reserved and no image loaded.
    Empty = 0,
    /// An image is loaded and verified; no task is running.
    Ready = 1,
    /// A task is running.
    Running = 2,
    /// A task is suspended at a safe point, waiting for a host completion.
    Suspended = 3,
    /// The isolate finished a task and is ready for the next.
    Idle = 4,
    /// The isolate stopped for a reason that admits no further task.
    Stopped = 5,
}

impl State {
    /// Whether a transition is one the lifecycle admits.
    pub const fn admits(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Empty, Self::Ready)
                | (Self::Ready, Self::Running)
                | (Self::Running, Self::Suspended)
                | (Self::Running, Self::Idle)
                | (Self::Running, Self::Stopped)
                | (Self::Suspended, Self::Running)
                | (Self::Suspended, Self::Stopped)
                | (Self::Idle, Self::Running)
                | (Self::Idle, Self::Ready)
                | (Self::Idle, Self::Stopped)
                | (Self::Ready, Self::Stopped)
                | (Self::Empty, Self::Stopped)
        )
    }
}

/// How a task ended.
///
/// A completion is either the program's own result, which it could have
/// produced or thrown, or an administrative outcome, which it cannot catch and
/// cannot prevent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Outcome {
    /// The program returned a value.
    Returned = 0,
    /// The program threw a value it did not catch.
    Threw = 1,
    /// The instruction budget ran out.
    FuelExhausted = 2,
    /// The wall-clock deadline passed.
    DeadlineReached = 3,
    /// The host asked for the task to stop.
    Cancelled = 4,
    /// The heap could not satisfy an allocation inside its quota.
    HeapExhausted = 5,
    /// The call stack reached its admitted depth.
    StackOverflow = 6,
    /// A bounded table, queue, or buffer was full.
    QuotaExceeded = 7,
    /// The image was rejected before anything ran.
    ImageRejected = 8,
}

impl Outcome {
    /// Whether the program could observe and handle this outcome.
    ///
    /// Administrative outcomes are uncatchable on purpose: a script must not be
    /// able to defeat a policy by catching it and continuing.
    pub const fn catchable(self) -> bool {
        matches!(self, Self::Returned | Self::Threw)
    }

    /// Whether the isolate may run another task afterwards.
    pub const fn resumable(self) -> bool {
        matches!(
            self,
            Self::Returned | Self::Threw | Self::FuelExhausted | Self::DeadlineReached
        )
    }
}

/// The limits an isolate is admitted against.
///
/// Every field is a hard maximum for one task, not an average or a target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Policy {
    /// Bytes of heap arena.
    pub heap_bytes: u32,
    /// Handle table entries, which bounds live cells.
    pub heap_cells: u32,
    /// Instructions one task may run.
    pub fuel: u64,
    /// Call frames, which bounds call depth.
    pub frames: u32,
    /// Registers across all frames.
    pub registers: u32,
    /// Jobs the queue may hold.
    pub jobs: u32,
    /// Host calls that may be outstanding at once.
    pub pending_calls: u32,
    /// Bytes an image may occupy.
    pub image_bytes: u32,
    /// Milliseconds of wall clock one task may take, or zero for no deadline.
    pub deadline_ms: u32,
    /// Cells a collection slice may visit before yielding.
    pub collection_slice: u32,
}

impl Policy {
    /// The compiled-in ceiling. A deployment may lower any field and can never
    /// raise one.
    pub const CEILING: Self = Self {
        heap_bytes: 64 * 1024 * 1024,
        heap_cells: 1 << 20,
        fuel: 1 << 40,
        frames: 4096,
        registers: 1 << 20,
        jobs: 65_536,
        pending_calls: 4096,
        image_bytes: 16 * 1024 * 1024,
        deadline_ms: 3_600_000,
        collection_slice: 65_536,
    };

    /// A policy small enough for a constrained deployment, which is also what a
    /// caller gets when it asks for nothing in particular.
    pub const MODEST: Self = Self {
        heap_bytes: 256 * 1024,
        heap_cells: 4096,
        fuel: 10_000_000,
        frames: 64,
        registers: 4096,
        jobs: 256,
        pending_calls: 32,
        image_bytes: 256 * 1024,
        deadline_ms: 1000,
        collection_slice: 1024,
    };

    /// Clamp every field to the ceiling, so an admitted policy can never widen
    /// it.
    #[must_use]
    pub const fn clamped(self) -> Self {
        Self {
            heap_bytes: min_u32(self.heap_bytes, Self::CEILING.heap_bytes),
            heap_cells: min_u32(self.heap_cells, Self::CEILING.heap_cells),
            fuel: min_u64(self.fuel, Self::CEILING.fuel),
            frames: min_u32(self.frames, Self::CEILING.frames),
            registers: min_u32(self.registers, Self::CEILING.registers),
            jobs: min_u32(self.jobs, Self::CEILING.jobs),
            pending_calls: min_u32(self.pending_calls, Self::CEILING.pending_calls),
            image_bytes: min_u32(self.image_bytes, Self::CEILING.image_bytes),
            deadline_ms: min_u32(self.deadline_ms, Self::CEILING.deadline_ms),
            collection_slice: min_u32(self.collection_slice, Self::CEILING.collection_slice),
        }
    }

    /// Whether storage of these sizes satisfies the policy.
    ///
    /// Admission proves the inequality once, so no later step has to check that
    /// its storage is large enough.
    pub const fn admits(
        &self,
        heap_bytes: u32,
        heap_cells: u32,
        frames: u32,
        registers: u32,
    ) -> bool {
        heap_bytes >= self.heap_bytes
            && heap_cells >= self.heap_cells
            && frames >= self.frames
            && registers >= self.registers
    }
}

const fn min_u32(left: u32, right: u32) -> u32 {
    if left < right {
        left
    } else {
        right
    }
}

const fn min_u64(left: u64, right: u64) -> u64 {
    if left < right {
        left
    } else {
        right
    }
}

/// Why an isolate would not accept an image or a task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rejection {
    /// The storage handed to the isolate is smaller than the policy requires.
    InsufficientStorage,
    /// The image is larger than the policy admits.
    ImageTooLarge,
    /// The image did not verify.
    ImageRejected,
    /// The lifecycle does not admit this operation in this state.
    WrongState,
}
