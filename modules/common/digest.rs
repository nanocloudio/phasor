//! SHA-256, the content-identity function for every Phasor artefact.
//!
//! Identity is a digest over canonical bytes, so the same source produces the
//! same unit on every target and a mismatched artefact fails closed before it
//! is used. The hash is the Fluxor SDK's SHA-256, the one core every module
//! on the platform shares; this file is the identity layer over it: the
//! `Digest` a record carries and the `Hasher` a bounded step can resume.

#[allow(
    dead_code,
    reason = "the SDK core carries a one-shot helper and a caller-buffer \
              finaliser this layer does not use"
)]
mod sdk {
    include!("../../target/fluxor/fluxor-abi/sdk/crypto/sha256.rs");
}

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

/// An incremental SHA-256 state. Allocation-free and resumable, so a bounded
/// step can hash part of a transfer and continue on the next.
pub struct Hasher(sdk::Sha256);

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl Hasher {
    pub const fn new() -> Self {
        Self(sdk::Sha256::new())
    }

    /// Absorb `bytes`. Any number of calls produce the same digest as one call
    /// over the concatenation.
    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    /// Finish and return the digest.
    #[must_use]
    pub fn finish(self) -> Digest {
        Digest(self.0.finalize())
    }
}

/// The digest of one contiguous input.
pub fn digest(bytes: &[u8]) -> Digest {
    let mut hasher = Hasher::new();
    hasher.update(bytes);
    hasher.finish()
}
