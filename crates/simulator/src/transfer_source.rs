//! Agent-side disposable capture-source abstraction for the M1
//! `bamep.m1.data-plane-transfer` Agent -> Server participant (Issue #19
//! checkpoint C1).
//!
//! C1 never reads a physical disk and never implements the production backup
//! format (`m1-simulated-vertical-slice-and-baseline-validation.md` RF-005 —
//! disposable source bytes come from Simulator/test-harness configuration, the
//! same fixture boundary trusted-bootstrap material uses). A [`TransferSource`]
//! is only required to be a *deterministic, repeatable* byte stream that can be
//! sliced into fixed-size chunks — exactly what the chunk-manifest /
//! full-Artifact reconstruction contract
//! (`m0-data-plane-and-storage-contracts.md` "Chunk manifest",
//! "Full-Artifact byte reconstruction") assumes of a reproducible source.

/// A deterministic, repeatable capture source the Agent chunks and hashes.
///
/// Every read for the same `(index, chunk_size)` MUST return the same bytes
/// *unless* the source is deliberately mutated between transfer runs (a
/// resumability test hook — see [`InMemoryTransferSource::mutate_chunk`]).
pub trait TransferSource {
    /// The total deterministic byte length of the reproducible source. MUST be
    /// `>= 1` (`m0-data-plane-and-storage-contracts.md` "Full-Artifact byte
    /// reconstruction": the final chunk is `1..=chunk_size` bytes).
    fn total_len(&self) -> u64;

    /// The exact raw bytes for chunk `index`, given the authoritative
    /// `chunk_size` from the authorized data-plane flow. `index` is 0-based;
    /// every chunk except the last is exactly `chunk_size` bytes and the last
    /// is `1..=chunk_size`. Callers only request `index` in
    /// `0..total_len.div_ceil(chunk_size)`.
    fn chunk_bytes(&self, index: u64, chunk_size: u32) -> Vec<u8>;
}

/// The default in-memory [`TransferSource`]: a `Vec<u8>` held entirely in
/// Agent memory, with deterministic construction and narrow test-only
/// mutation hooks.
#[derive(Debug, Clone)]
pub struct InMemoryTransferSource {
    bytes: Vec<u8>,
}

impl InMemoryTransferSource {
    /// Wraps exact bytes. Panics on an empty source — a 0-byte Artifact cannot
    /// be represented by the chunk contract.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        let bytes = bytes.into();
        assert!(
            !bytes.is_empty(),
            "a TransferSource must carry at least one byte"
        );
        Self { bytes }
    }

    /// A deterministic pseudo-random pattern of `len` bytes derived from
    /// `seed` — reproducible across runs and processes, with enough variation
    /// that adjacent chunks differ.
    pub fn pattern(len: usize, seed: u64) -> Self {
        assert!(len >= 1, "a TransferSource must carry at least one byte");
        // A tiny SplitMix64-style generator: deterministic, no crate needed,
        // and good enough to make every chunk's digest distinct.
        let mut state = seed ^ 0x9E37_79B9_7F4A_7C15;
        let mut bytes = Vec::with_capacity(len);
        for _ in 0..len {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            bytes.push((z & 0xFF) as u8);
        }
        Self { bytes }
    }

    /// Test hook: overwrites the byte range that chunk `chunk_index` covers at
    /// `chunk_size` so a later [`TransferSource::chunk_bytes`] read no longer
    /// reproduces whatever chunk identity was previously recorded for that
    /// index. Proves that source mutation fails the transfer closed and never
    /// causes the recorded expected identity to be rewritten
    /// (`m0-data-plane-and-storage-contracts.md` "Chunk transfer and
    /// resumability").
    pub fn mutate_chunk(&mut self, chunk_index: u64, chunk_size: u32) {
        let start = (chunk_index * u64::from(chunk_size)) as usize;
        if start >= self.bytes.len() {
            return;
        }
        let end = (start + chunk_size as usize).min(self.bytes.len());
        for b in &mut self.bytes[start..end] {
            *b = b.wrapping_add(1) ^ 0xA5;
        }
    }

    /// The exact backing bytes (tests use this to compute expected digests).
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl TransferSource for InMemoryTransferSource {
    fn total_len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn chunk_bytes(&self, index: u64, chunk_size: u32) -> Vec<u8> {
        let start = (index * u64::from(chunk_size)) as usize;
        if start >= self.bytes.len() {
            return Vec::new();
        }
        let end = (start + chunk_size as usize).min(self.bytes.len());
        self.bytes[start..end].to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_into_fixed_chunks_with_a_short_final_chunk() {
        let source = InMemoryTransferSource::new(vec![0u8; 10]);
        assert_eq!(source.total_len(), 10);
        assert_eq!(source.chunk_bytes(0, 4).len(), 4);
        assert_eq!(source.chunk_bytes(1, 4).len(), 4);
        assert_eq!(source.chunk_bytes(2, 4).len(), 2); // short final chunk
    }

    #[test]
    fn exact_multiple_makes_the_final_chunk_full_size() {
        let source = InMemoryTransferSource::new(vec![1u8; 8]);
        assert_eq!(source.chunk_bytes(1, 4).len(), 4);
    }

    #[test]
    fn pattern_is_deterministic_and_chunks_differ() {
        let a = InMemoryTransferSource::pattern(4096, 7);
        let b = InMemoryTransferSource::pattern(4096, 7);
        assert_eq!(a.as_bytes(), b.as_bytes());
        assert_ne!(a.chunk_bytes(0, 1024), a.chunk_bytes(1, 1024));
    }

    #[test]
    fn mutate_chunk_changes_only_that_chunk() {
        let mut source = InMemoryTransferSource::pattern(4096, 1);
        let before_0 = source.chunk_bytes(0, 1024);
        let before_2 = source.chunk_bytes(2, 1024);
        source.mutate_chunk(1, 1024);
        assert_eq!(source.chunk_bytes(0, 1024), before_0);
        let pristine_chunk_1 = InMemoryTransferSource::pattern(4096, 1).chunk_bytes(1, 1024);
        assert_ne!(source.chunk_bytes(1, 1024), pristine_chunk_1);
        assert_eq!(source.chunk_bytes(2, 1024), before_2);
    }
}
