//! A deterministic random bit generator over ChaCha20 (RFC 8439's block
//! function), seeded by a host from an entropy capability.
//!
//! A program asks for random values synchronously — `crypto.getRandomValues`
//! answers in the call — while the only source of unpredictability a program
//! may have is a granted capability, which answers later. So the host draws a
//! seed from the capability before the program runs, and the program's
//! values come from this generator: 256 bits of key, a block counter, and
//! the unused remainder of the last block. Everything is plain words, so it
//! is saved with the machine between steps like the rest of its state.
//!
//! Unseeded, it produces nothing: a host with no entropy grant installs no
//! `crypto`, and `Math.random` keeps its fixed, replayable sequence.

/// The generator's state.
#[derive(Clone, Copy, Debug)]
pub struct Drbg {
    key: [u32; 8],
    counter: u32,
    block: [u32; 16],
    used: u8,
    seeded: bool,
}

impl Drbg {
    /// A generator nothing has seeded.
    pub const UNSEEDED: Self = Self {
        key: [0; 8],
        counter: 0,
        block: [0; 16],
        used: 16,
        seeded: false,
    };

    /// A generator keyed by `seed`.
    pub fn seeded(seed: &[u32; 8]) -> Self {
        Self {
            key: *seed,
            counter: 0,
            block: [0; 16],
            used: 16,
            seeded: true,
        }
    }

    /// Whether a host seeded it.
    pub const fn is_seeded(&self) -> bool {
        self.seeded
    }

    /// The next 32 random bits.
    pub fn next_u32(&mut self) -> u32 {
        if self.used >= 16 {
            self.block = block(&self.key, self.counter);
            self.counter = self.counter.wrapping_add(1);
            // Past 2^32 blocks the stream would repeat; rekey from it first.
            if self.counter == 0 {
                let mut key = [0u32; 8];
                key.copy_from_slice(&self.block[..8]);
                self.key = key;
                self.block = block(&self.key, 0);
                self.counter = 1;
            }
            self.used = 0;
        }
        let word = self.block[usize::from(self.used)];
        self.used += 1;
        word
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        (u64::from(self.next_u32()) << 32) | u64::from(self.next_u32())
    }
}

/// One ChaCha20 quarter round over four words of the state.
#[inline(always)]
fn quarter(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(12);
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(7);
}

/// The ChaCha20 block for `key` at `counter`, with a zero nonce: one key is
/// one stream here, so the nonce has nothing to distinguish.
pub fn block(key: &[u32; 8], counter: u32) -> [u32; 16] {
    block_with(key, counter, &[0; 3])
}

/// RFC 8439's block function: key, block counter, and nonce.
pub fn block_with(key: &[u32; 8], counter: u32, nonce: &[u32; 3]) -> [u32; 16] {
    let mut input = [0u32; 16];
    input[0] = 0x6170_7865;
    input[1] = 0x3320_646e;
    input[2] = 0x7962_2d32;
    input[3] = 0x6b20_6574;
    input[4..12].copy_from_slice(key);
    input[12] = counter;
    input[13..16].copy_from_slice(nonce);
    let mut state = input;
    let mut round = 0;
    while round < 10 {
        quarter(&mut state, 0, 4, 8, 12);
        quarter(&mut state, 1, 5, 9, 13);
        quarter(&mut state, 2, 6, 10, 14);
        quarter(&mut state, 3, 7, 11, 15);
        quarter(&mut state, 0, 5, 10, 15);
        quarter(&mut state, 1, 6, 11, 12);
        quarter(&mut state, 2, 7, 8, 13);
        quarter(&mut state, 3, 4, 9, 14);
        round += 1;
    }
    let mut out = [0u32; 16];
    for (index, word) in out.iter_mut().enumerate() {
        *word = state[index].wrapping_add(input[index]);
    }
    out
}
