//! Mechanical SHA-256 chunk digest (Issue #39 Phase D1). The Worker computes
//! this itself as a pure transport/storage mechanism — it is **not**
//! `bamep_domain::Digest` and carries no business authority (ADR-0018
//! "Worker responsibility (mechanism, not authority)"; the Worker crate has
//! no `bamep-domain` dependency).
//!
//! The raw 32 bytes are the internal representation. The canonical textual
//! form is RFC 4648 base64url **without padding** — exactly 43 ASCII
//! characters for a 32-byte SHA-256 digest — matching the encoding
//! `docs/specifications/m0-data-plane-and-storage-contracts.md` ("Chunk
//! manifest") already fixes for chunk/Artifact digest text and the
//! convention `bamep-trusted-bootstrap` already uses. Phase D1 exposes the
//! encoder only; no wire boundary consumes it yet (Phase E owns that).

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

/// A raw SHA-256 digest over some chunk's exact bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    /// Wraps the 32 raw digest bytes produced by a SHA-256 computation.
    pub const fn from_raw(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw 32-byte digest.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Canonical RFC 4648 base64url encoding, no padding: exactly 43 ASCII
    /// characters, drawn only from `A-Z a-z 0-9 - _`. Deterministic.
    pub fn to_base64url_no_pad(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }
}

/// Never renders the raw bytes as an opaque array; the canonical base64url
/// form is both shorter and the representation every other component speaks.
impl std::fmt::Debug for Sha256Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Sha256Digest({})", self.to_base64url_no_pad())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published SHA-256 of `b"abc"`, as raw bytes and as this module's
    /// canonical text. Proves the encoder is exactly RFC 4648 base64url with
    /// no padding and no alternate alphabet.
    #[test]
    fn known_sha256_abc_vector_encodes_canonically() {
        let raw: [u8; 32] = [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ];
        let digest = Sha256Digest::from_raw(raw);
        let text = digest.to_base64url_no_pad();

        assert_eq!(text, "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0");
        assert_eq!(text.len(), 43);
        assert!(!text.contains('='), "no padding");
        assert!(
            !text.contains('+') && !text.contains('/'),
            "url-safe alphabet only"
        );
    }

    #[test]
    fn raw_bytes_round_trip() {
        let raw = [7u8; 32];
        assert_eq!(Sha256Digest::from_raw(raw).as_bytes(), &raw);
    }

    #[test]
    fn equality_is_over_the_raw_bytes() {
        assert_eq!(
            Sha256Digest::from_raw([1; 32]),
            Sha256Digest::from_raw([1; 32])
        );
        assert_ne!(
            Sha256Digest::from_raw([1; 32]),
            Sha256Digest::from_raw([2; 32])
        );
    }
}
