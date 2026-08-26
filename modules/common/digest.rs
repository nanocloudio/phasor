//! SHA-256, the content-identity function for every Phasor artefact.
//!
//! Identity is a digest over canonical bytes, so the same source produces the
//! same unit on every target and a mismatched artefact fails closed before it
//! is used. The implementation is allocation-free, holds no pointer in static
//! data, and takes its input incrementally so a bounded step can hash part of a
//! transfer and resume.

#[rustfmt::skip]
static ROUND_CONSTANTS: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// A 256-bit content identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Digest(pub [u8; 32]);

impl Digest {
    /// The first eight bytes, for a compact identity in a fixed-width record.
    pub const fn prefix(&self) -> u64 {
        let bytes = self.0;
        u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])
    }
}

/// An incremental SHA-256 state.
pub struct Hasher {
    state: [u32; 8],
    block: [u8; 64],
    block_length: usize,
    length: u64,
}

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl Hasher {
    pub const fn new() -> Self {
        Self {
            state: [
                0x6a09_e667,
                0xbb67_ae85,
                0x3c6e_f372,
                0xa54f_f53a,
                0x510e_527f,
                0x9b05_688c,
                0x1f83_d9ab,
                0x5be0_cd19,
            ],
            block: [0; 64],
            block_length: 0,
            length: 0,
        }
    }

    /// Absorb `bytes`. Any number of calls produce the same digest as one call
    /// over the concatenation.
    pub fn update(&mut self, bytes: &[u8]) {
        self.length = self.length.wrapping_add(bytes.len() as u64);
        let mut offset = 0usize;
        while offset < bytes.len() {
            let space = 64 - self.block_length;
            let take = if bytes.len() - offset < space {
                bytes.len() - offset
            } else {
                space
            };
            let Some(source) = bytes.get(offset..offset + take) else {
                return;
            };
            let Some(target) = self
                .block
                .get_mut(self.block_length..self.block_length + take)
            else {
                return;
            };
            target.copy_from_slice(source);
            self.block_length += take;
            offset += take;
            if self.block_length == 64 {
                let block = self.block;
                self.compress(&block);
                self.block_length = 0;
            }
        }
    }

    /// Finish and return the digest.
    #[must_use]
    pub fn finish(mut self) -> Digest {
        let bit_length = self.length.wrapping_mul(8);
        self.update_raw(&[0x80]);
        while self.block_length != 56 {
            self.update_raw(&[0x00]);
        }
        self.update_raw(&bit_length.to_be_bytes());

        let mut output = [0u8; 32];
        let mut index = 0usize;
        while index < 8 {
            let word = self.state[index].to_be_bytes();
            let base = index * 4;
            output[base] = word[0];
            output[base + 1] = word[1];
            output[base + 2] = word[2];
            output[base + 3] = word[3];
            index += 1;
        }
        Digest(output)
    }

    /// Absorb padding bytes without counting them in the message length.
    fn update_raw(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.block[self.block_length] = byte;
            self.block_length += 1;
            if self.block_length == 64 {
                let block = self.block;
                self.compress(&block);
                self.block_length = 0;
            }
        }
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut schedule = [0u32; 64];
        let mut index = 0usize;
        while index < 16 {
            let base = index * 4;
            schedule[index] = u32::from_be_bytes([
                block[base],
                block[base + 1],
                block[base + 2],
                block[base + 3],
            ]);
            index += 1;
        }
        while index < 64 {
            let previous = schedule[index - 15];
            let ahead = schedule[index - 2];
            let s0 = previous.rotate_right(7) ^ previous.rotate_right(18) ^ (previous >> 3);
            let s1 = ahead.rotate_right(17) ^ ahead.rotate_right(19) ^ (ahead >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(s1);
            index += 1;
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        let mut round = 0usize;
        while round < 64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choose)
                .wrapping_add(ROUND_CONSTANTS[round])
                .wrapping_add(schedule[round]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
            round += 1;
        }

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }
}

/// The digest of one contiguous input.
pub fn digest(bytes: &[u8]) -> Digest {
    let mut hasher = Hasher::new();
    hasher.update(bytes);
    hasher.finish()
}
