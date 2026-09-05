//! The isolate heap: one bounded arena and a handle table.
//!
//! Every cell lives in a caller-provided byte arena and is addressed through a
//! handle table entry, never a pointer. Handles carry a generation that retires
//! rather than wrapping, so a stale handle cannot become valid again when a
//! slot is reused, and an arena can be moved or compacted by updating slots
//! alone.
//!
//! Allocation is a bump through the arena. Collection is mark-compact and runs
//! in slices: each slice does a bounded amount of marking or moving and
//! returns, so a collection can span several module steps and be charged to
//! each of their budgets. The mutator does not run between slices, which is why
//! no write barrier is needed; a variant that did interleave would need one.
//!
//! Compaction is safe because a cell is only ever addressed through its slot:
//! moving a cell rewrites one offset and nothing else, since no reference to it
//! exists anywhere else.

use crate::value::Handle;

/// What a cell holds. The kind is stored in the slot rather than the payload,
/// so a reader knows what it is looking at before it reads anything.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum CellKind {
    Free = 0,
    String = 1,
    Symbol = 2,
    BigInt = 3,
    Object = 4,
    Environment = 5,
    PropertyTable = 6,
    Elements = 7,
}

impl CellKind {
    const fn from_byte(byte: u8) -> Option<Self> {
        let kind = match byte {
            0 => Self::Free,
            1 => Self::String,
            2 => Self::Symbol,
            3 => Self::BigInt,
            4 => Self::Object,
            5 => Self::Environment,
            6 => Self::PropertyTable,
            7 => Self::Elements,
            _ => return None,
        };
        Some(kind)
    }
}

/// Why an allocation did not happen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeapError {
    /// The arena has no room for the cell.
    ArenaFull,
    /// Every handle slot is in use.
    SlotsFull,
    /// The handle does not name a live cell of the expected kind.
    StaleHandle,
    /// A collection is in progress, during which nothing may be allocated.
    Collecting,
}

/// One handle table entry.
#[derive(Clone, Copy, Debug)]
pub struct Slot {
    offset: u32,
    length: u32,
    generation: u32,
    kind: u8,
    /// Collection mark: white until reached, then grey while its children are
    /// pending, then black.
    mark: u8,
}

/// Marks a cell carries during a collection.
const WHITE: u8 = 0;
const GREY: u8 = 1;
const BLACK: u8 = 2;

/// Bytes before a cell's payload: the slot it belongs to and its length, which
/// is what lets the compactor walk the arena without a sorted table.
const CELL_HEADER: usize = 8;

/// Which part of a collection is in progress.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    /// No collection is running.
    Idle,
    /// Reachable cells are being found.
    Marking,
    /// Live cells are being moved down and the rest discarded.
    Compacting,
}

impl Slot {
    pub const EMPTY: Self = Self {
        offset: 0,
        length: 0,
        generation: 1,
        kind: 0,
        mark: 0,
    };
}

/// A bounded heap over caller-provided storage.
/// A heap's own state, apart from the storage it was given.
///
/// A module that rebuilds its machine on every step keeps this in its own
/// storage and hands it back, so allocation, collection, and accounting carry
/// on where they stopped rather than starting again over live cells.
#[derive(Clone, Copy, Debug, Default)]
pub struct HeapSave {
    worklist_length: u32,
    used: u32,
    live: u32,
    phase: u8,
    compact_read: u32,
    compact_write: u32,
    reclaimed_cells: u32,
    reclaimed_bytes: u32,
    free_hint: u32,
}

pub struct Heap<'a> {
    arena: &'a mut [u8],
    slots: &'a mut [Slot],
    /// Grey cells whose children are still to be visited.
    worklist: &'a mut [u32],
    worklist_length: u32,
    used: u32,
    live: u32,
    phase: Phase,
    /// Where the compactor has reached, both reading and writing.
    compact_read: u32,
    compact_write: u32,
    /// Cells reclaimed and bytes recovered by the last completed collection.
    reclaimed_cells: u32,
    reclaimed_bytes: u32,
    /// Where the search for a free slot starts: every slot below it was in
    /// use when last looked at, so an allocation scans from here and wraps,
    /// and a slot freed below it pulls it back.
    free_hint: u32,
}

impl<'a> Heap<'a> {
    /// This heap's state, to be restored onto the same storage later.
    pub const fn save(&self) -> HeapSave {
        HeapSave {
            worklist_length: self.worklist_length,
            used: self.used,
            live: self.live,
            phase: self.phase as u8,
            compact_read: self.compact_read,
            compact_write: self.compact_write,
            reclaimed_cells: self.reclaimed_cells,
            reclaimed_bytes: self.reclaimed_bytes,
            free_hint: self.free_hint,
        }
    }

    /// Carry on from a saved state over the same arena, slots, and worklist.
    pub fn restore(&mut self, save: &HeapSave) {
        self.worklist_length = save.worklist_length;
        self.used = save.used;
        self.live = save.live;
        self.free_hint = save.free_hint;
        self.phase = match save.phase {
            1 => Phase::Marking,
            2 => Phase::Compacting,
            _ => Phase::Idle,
        };
        self.compact_read = save.compact_read;
        self.compact_write = save.compact_write;
        self.reclaimed_cells = save.reclaimed_cells;
        self.reclaimed_bytes = save.reclaimed_bytes;
    }
    /// A heap over `arena` with `slots` handle entries and no collection
    /// storage, which is enough until a collection is asked for.
    pub fn new(arena: &'a mut [u8], slots: &'a mut [Slot]) -> Self {
        Self::with_worklist(arena, slots, &mut [])
    }

    /// A heap that can collect, using `worklist` to hold grey cells.
    pub fn with_worklist(
        arena: &'a mut [u8],
        slots: &'a mut [Slot],
        worklist: &'a mut [u32],
    ) -> Self {
        for slot in slots.iter_mut() {
            *slot = Slot::EMPTY;
        }
        Self {
            arena,
            slots,
            worklist,
            worklist_length: 0,
            used: 0,
            live: 0,
            phase: Phase::Idle,
            compact_read: 0,
            compact_write: 0,
            reclaimed_cells: 0,
            reclaimed_bytes: 0,
            free_hint: 0,
        }
    }

    /// Take up storage a heap was already using, with its saved state.
    ///
    /// The slot table is left exactly as it was found: it is the handle table,
    /// and clearing it would invalidate every handle the module still holds.
    /// This is what lets a module rebuild its heap on a later step.
    pub fn adopt(
        arena: &'a mut [u8],
        slots: &'a mut [Slot],
        worklist: &'a mut [u32],
        save: &HeapSave,
    ) -> Self {
        let mut heap = Self {
            arena,
            slots,
            worklist,
            worklist_length: 0,
            used: 0,
            live: 0,
            phase: Phase::Idle,
            compact_read: 0,
            compact_write: 0,
            reclaimed_cells: 0,
            reclaimed_bytes: 0,
            free_hint: 0,
        };
        heap.restore(save);
        heap
    }

    /// Bytes of arena in use.
    pub const fn used(&self) -> u32 {
        self.used
    }

    /// Handle slots still free.
    ///
    /// A heap runs out of two things, not one: the arena its cells live in, and
    /// the table their handles come from. A caller that collects when one is
    /// low must watch the other too.
    pub fn free_slots(&self) -> u32 {
        // Every slot not holding a live cell is free: the live count moves
        // with each allocation and each release, so no walk of the table
        // is needed — a walk the machine's safe points could not afford.
        u32::try_from(self.slots.len())
            .unwrap_or(u32::MAX)
            .saturating_sub(self.live)
    }

    /// Slots the handle table holds in all.
    pub fn slot_capacity(&self) -> u32 {
        u32::try_from(self.slots.len()).unwrap_or(0)
    }

    /// Bytes of arena still free.
    pub fn free(&self) -> u32 {
        u32::try_from(self.arena.len()).unwrap_or(u32::MAX) - self.used
    }

    /// Live cells.
    pub const fn live(&self) -> u32 {
        self.live
    }

    /// Allocate a zeroed cell of `length` bytes.
    ///
    /// Cells are aligned to eight bytes so a payload can hold a value or a
    /// 64-bit field without an unaligned access.
    pub fn allocate(&mut self, kind: CellKind, length: u32) -> Result<Handle, HeapError> {
        if self.phase != Phase::Idle {
            return Err(HeapError::Collecting);
        }
        let header = (self.used + 7) & !7;
        let start = header + u32::try_from(CELL_HEADER).unwrap_or(8);
        let end = start.checked_add(length).ok_or(HeapError::ArenaFull)?;
        if end as usize > self.arena.len() {
            return Err(HeapError::ArenaFull);
        }

        let index = self.take_slot()?;
        let slot = self
            .slots
            .get_mut(index as usize)
            .ok_or(HeapError::SlotsFull)?;
        slot.offset = start;
        slot.length = length;
        slot.kind = kind as u8;
        slot.mark = WHITE;
        let generation = slot.generation;

        if let Some(cell) = self.arena.get_mut(header as usize..end as usize) {
            for byte in cell.iter_mut() {
                *byte = 0;
            }
        }
        if let Some(bytes) = self
            .arena
            .get_mut(header as usize..header as usize + CELL_HEADER)
        {
            let mut fields = [0u8; CELL_HEADER];
            fields[0..4].copy_from_slice(&index.to_le_bytes());
            fields[4..8].copy_from_slice(&length.to_le_bytes());
            bytes.copy_from_slice(&fields);
        }
        self.used = end;
        self.live += 1;
        Ok(Handle::new(index, generation))
    }

    fn take_slot(&mut self) -> Result<u32, HeapError> {
        // Scan from the hint to the end, then from the start up to it: the
        // slots below the hint were all taken when it was last set.
        let count = self.slots.len();
        let start = (self.free_hint as usize).min(count);
        let mut scanned = 0usize;
        let mut index = start;
        while scanned < count {
            if index >= count {
                index = 0;
            }
            if CellKind::from_byte(self.slots[index].kind) == Some(CellKind::Free) {
                self.free_hint = u32::try_from(index + 1).unwrap_or(u32::MAX);
                return u32::try_from(index).map_err(|_| HeapError::SlotsFull);
            }
            index += 1;
            scanned += 1;
        }
        Err(HeapError::SlotsFull)
    }

    /// A slot below the hint came free: the next search starts there.
    fn note_free(&mut self, index: u32) {
        if index < self.free_hint {
            self.free_hint = index;
        }
    }

    fn slot(&self, handle: Handle) -> Result<&Slot, HeapError> {
        let slot = self
            .slots
            .get(handle.index as usize)
            .ok_or(HeapError::StaleHandle)?;
        if slot.generation != handle.generation
            || CellKind::from_byte(slot.kind) == Some(CellKind::Free)
        {
            return Err(HeapError::StaleHandle);
        }
        Ok(slot)
    }

    /// The kind of the cell a handle names.
    pub fn kind(&self, handle: Handle) -> Result<CellKind, HeapError> {
        let slot = self.slot(handle)?;
        CellKind::from_byte(slot.kind).ok_or(HeapError::StaleHandle)
    }

    /// The bytes of the cell a handle names.
    pub fn cell(&self, handle: Handle) -> Result<&[u8], HeapError> {
        let slot = self.slot(handle)?;
        let start = slot.offset as usize;
        let end = start + slot.length as usize;
        self.arena.get(start..end).ok_or(HeapError::StaleHandle)
    }

    /// The bytes of the cell a handle names, for writing.
    pub fn cell_mut(&mut self, handle: Handle) -> Result<&mut [u8], HeapError> {
        let slot = *self.slot(handle)?;
        let start = slot.offset as usize;
        let end = start + slot.length as usize;
        self.arena.get_mut(start..end).ok_or(HeapError::StaleHandle)
    }

    /// Retire a cell's slot. The arena space is reclaimed by the next
    /// collection.
    pub fn retire(&mut self, handle: Handle) -> Result<(), HeapError> {
        let slot = self
            .slots
            .get_mut(handle.index as usize)
            .ok_or(HeapError::StaleHandle)?;
        if slot.generation != handle.generation {
            return Err(HeapError::StaleHandle);
        }
        slot.kind = CellKind::Free as u8;
        slot.length = 0;
        // The generation retires permanently rather than wrapping, so a stale
        // handle can never name this slot again.
        slot.generation = slot.generation.saturating_add(1);
        self.live = self.live.saturating_sub(1);
        self.note_free(handle.index);
        Ok(())
    }
}

// Collection.

impl Heap<'_> {
    /// Which part of a collection is running.
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// Cells and bytes the last completed collection reclaimed.
    pub const fn reclaimed(&self) -> (u32, u32) {
        (self.reclaimed_cells, self.reclaimed_bytes)
    }

    /// Begin a collection from `roots`.
    ///
    /// Nothing may be allocated until the collection finishes, which is what
    /// makes the mark phase safe without a write barrier.
    pub fn begin_collection(&mut self, roots: &[Handle]) -> Result<(), HeapError> {
        if self.phase != Phase::Idle {
            return Err(HeapError::Collecting);
        }
        for slot in self.slots.iter_mut() {
            slot.mark = WHITE;
        }
        self.worklist_length = 0;
        self.phase = Phase::Marking;
        self.reclaimed_cells = 0;
        self.reclaimed_bytes = 0;
        for &root in roots {
            self.shade(root);
        }
        Ok(())
    }

    /// Mark a cell reachable and remember to visit its children.
    ///
    /// A handle that names nothing live is ignored, so a caller may shade
    /// anything it holds without checking it first.
    pub fn shade(&mut self, handle: Handle) {
        let Some(slot) = self.slots.get_mut(handle.index as usize) else {
            return;
        };
        if slot.generation != handle.generation || slot.kind == CellKind::Free as u8 {
            return;
        }
        if slot.mark != WHITE {
            return;
        }
        slot.mark = GREY;
        if let Some(entry) = self.worklist.get_mut(self.worklist_length as usize) {
            *entry = handle.index;
            self.worklist_length += 1;
        } else {
            // The worklist is full. The cell stays grey and is found again by
            // the sweep over slots, so nothing is lost; only the order changes.
            slot.mark = GREY;
        }
    }

    /// Take the next grey cell to visit, if the worklist holds one.
    pub fn next_grey(&mut self) -> Option<Handle> {
        while self.worklist_length > 0 {
            self.worklist_length -= 1;
            let index = *self.worklist.get(self.worklist_length as usize)?;
            let slot = self.slots.get_mut(index as usize)?;
            if slot.mark == GREY {
                slot.mark = BLACK;
                return Some(Handle::new(index, slot.generation));
            }
        }
        // The worklist may have overflowed, leaving grey cells behind; find one.
        let mut index = 0usize;
        while index < self.slots.len() {
            if let Some(slot) = self.slots.get_mut(index) {
                if slot.mark == GREY {
                    slot.mark = BLACK;
                    return Some(Handle::new(
                        u32::try_from(index).unwrap_or(0),
                        slot.generation,
                    ));
                }
            }
            index += 1;
        }
        None
    }

    /// Move to the compaction phase once marking has finished.
    pub fn begin_compaction(&mut self) {
        if self.phase != Phase::Marking {
            return;
        }
        self.phase = Phase::Compacting;
        self.compact_read = 0;
        self.compact_write = 0;
    }

    /// Move up to `budget` bytes of live cells down, discarding the rest.
    ///
    /// Returns whether the compaction finished. A cell is addressed only
    /// through its slot, so moving it rewrites one offset and nothing else.
    pub fn compact_step(&mut self, budget: u32) -> bool {
        if self.phase != Phase::Compacting {
            return true;
        }
        let mut moved = 0u32;
        while self.compact_read < self.used {
            if moved >= budget {
                return false;
            }
            let header = self.compact_read as usize;
            let Some(bytes) = self.arena.get(header..header + CELL_HEADER) else {
                break;
            };
            let Ok(fields) = <[u8; CELL_HEADER]>::try_from(bytes) else {
                break;
            };
            let index = u32::from_le_bytes([fields[0], fields[1], fields[2], fields[3]]);
            let length = u32::from_le_bytes([fields[4], fields[5], fields[6], fields[7]]);
            let total = u32::try_from(CELL_HEADER).unwrap_or(8) + length;
            let aligned = (total + 7) & !7;

            let live = match self.slots.get(index as usize) {
                Some(slot) => {
                    slot.kind != CellKind::Free as u8
                        && slot.mark == BLACK
                        && slot.offset
                            == self.compact_read + u32::try_from(CELL_HEADER).unwrap_or(8)
                }
                None => false,
            };

            if live {
                let destination = self.compact_write;
                if destination != self.compact_read {
                    // A forward copy, because a cell only ever moves down.
                    let from = self.compact_read as usize;
                    let to = destination as usize;
                    let mut copied = 0usize;
                    while copied < total as usize {
                        let byte = match self.arena.get(from + copied) {
                            Some(&byte) => byte,
                            None => break,
                        };
                        if let Some(slot) = self.arena.get_mut(to + copied) {
                            *slot = byte;
                        }
                        copied += 1;
                    }
                }
                if let Some(slot) = self.slots.get_mut(index as usize) {
                    slot.offset = destination + u32::try_from(CELL_HEADER).unwrap_or(8);
                }
                self.compact_write += aligned;
            } else {
                self.reclaimed_bytes = self.reclaimed_bytes.saturating_add(aligned);
                let mut freed = false;
                if let Some(slot) = self.slots.get_mut(index as usize) {
                    if slot.kind != CellKind::Free as u8 {
                        slot.kind = CellKind::Free as u8;
                        slot.length = 0;
                        slot.generation = slot.generation.saturating_add(1);
                        self.live = self.live.saturating_sub(1);
                        self.reclaimed_cells = self.reclaimed_cells.saturating_add(1);
                        freed = true;
                    }
                }
                if freed {
                    self.note_free(index);
                }
            }

            self.compact_read += aligned;
            moved = moved.saturating_add(aligned);
        }

        self.used = self.compact_write;
        self.phase = Phase::Idle;
        self.worklist_length = 0;
        true
    }
}
