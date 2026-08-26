//! Common Worker IPC v1 message envelope fields
//! (`m1-worker-data-plane-control-contract.md` "Transport, framing, and
//! versioning"): every message carries `protocol_version`, `message_id`,
//! and `type` at the same flat JSON level — there is no nested `"envelope"`
//! object on the wire. [`Envelope`] exists only as a Rust-side grouping,
//! flattened into each concrete message struct via `#[serde(flatten)]`,
//! mirroring `bamep-agent-protocol`'s identical convention.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Wire textual value of the currently supported Worker IPC protocol
/// version.
pub const PROTOCOL_VERSION_V1: &str = "1";

/// `protocol_version` as carried on the wire: a string, not a closed enum.
/// An incompatible received value (e.g. `"2"`) must deserialize
/// successfully so the handshake can explicitly reject it with
/// `HandshakeRejected` rather than fail parsing outright.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolVersion(String);

impl ProtocolVersion {
    /// The currently supported Worker IPC v1 textual value.
    pub fn v1() -> Self {
        Self(PROTOCOL_VERSION_V1.to_string())
    }

    /// Wraps an arbitrary received textual value without validating it.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this value equals the currently supported v1 textual value.
    pub fn is_v1(&self) -> bool {
        self.0 == PROTOCOL_VERSION_V1
    }
}

impl Default for ProtocolVersion {
    fn default() -> Self {
        Self::v1()
    }
}

/// Fields common to every Worker IPC v1 message, flattened into each
/// concrete message struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub protocol_version: ProtocolVersion,
    pub message_id: Uuid,
}

impl Envelope {
    /// A fresh v1 envelope stamped with a new v4 `message_id`.
    pub fn new() -> Self {
        Self {
            protocol_version: ProtocolVersion::v1(),
            message_id: Uuid::new_v4(),
        }
    }

    /// Whether this envelope satisfies the normative wire requirements every
    /// received message must carry (`m1-worker-data-plane-control-contract.md`
    /// "Transport, framing, and versioning": every message carries
    /// `protocol_version == "1"` and `message_id == UUID v4`). A receiver
    /// must validate these on every inbound message, never rely only on the
    /// sender's constructors having produced valid local values.
    pub fn is_valid(&self) -> bool {
        self.protocol_version.is_v1() && is_uuid_v4(&self.message_id)
    }
}

impl Default for Envelope {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether `id` is an ordinary UUID v4, the normative identity shape
/// required for `message_id` and `worker_instance_id`
/// (`m1-worker-data-plane-control-contract.md` "Handshake"). Requires both:
///
/// - the version nibble equals 4;
/// - the variant is the RFC4122/RFC9562 standard variant every ordinary
///   `Uuid::new_v4()` value carries.
///
/// Checking the version nibble alone (correction audit "UUID v4 variant
/// validation") would also accept a value whose top two variant bits encode
/// the reserved NCS, Microsoft, or future variants while merely *reusing*
/// the version-4 bit pattern by coincidence — a version-4-shaped value that
/// no real `Uuid::new_v4()` implementation would ever produce. Rejecting
/// those closes that gap without changing the wire representation: a real
/// v4 UUID from any conforming generator still satisfies this exactly as
/// before.
pub fn is_uuid_v4(id: &Uuid) -> bool {
    id.get_version_num() == 4 && id.get_variant() == uuid::Variant::RFC4122
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_envelope_is_valid() {
        assert!(Envelope::new().is_valid());
    }

    #[test]
    fn wrong_protocol_version_is_invalid() {
        let mut envelope = Envelope::new();
        envelope.protocol_version = ProtocolVersion::new("2");
        assert!(!envelope.is_valid());
    }

    #[test]
    fn non_v4_message_id_is_invalid() {
        let mut envelope = Envelope::new();
        // A nil UUID has version nibble 0, not 4.
        envelope.message_id = Uuid::nil();
        assert!(!envelope.is_valid());
    }

    #[test]
    fn is_uuid_v4_accepts_a_real_v4_value() {
        assert!(is_uuid_v4(&Uuid::new_v4()));
    }

    /// Sets the version nibble (bits 4-7 of byte 6) to `version`, keeping
    /// the low nibble untouched.
    fn with_version(mut bytes: [u8; 16], version: u8) -> [u8; 16] {
        bytes[6] = (bytes[6] & 0x0f) | (version << 4);
        bytes
    }

    /// Sets byte 8's top bits to encode `variant_top_bits` (already shifted
    /// into position), keeping the low 6 bits untouched.
    fn with_variant_top_bits(mut bytes: [u8; 16], variant_top_bits: u8) -> [u8; 16] {
        bytes[8] = (bytes[8] & 0b0011_1111) | variant_top_bits;
        bytes
    }

    #[test]
    fn is_uuid_v4_rejects_a_nil_uuid() {
        assert!(!is_uuid_v4(&Uuid::nil()));
    }

    #[test]
    fn is_uuid_v4_rejects_version_nibble_4_with_the_ncs_variant() {
        // NCS variant: top bit of byte 8 is 0 (0b0xxx_xxxx).
        let bytes = with_variant_top_bits(with_version(*Uuid::new_v4().as_bytes(), 4), 0b0000_0000);
        assert!(!is_uuid_v4(&Uuid::from_bytes(bytes)));
    }

    #[test]
    fn is_uuid_v4_rejects_version_nibble_4_with_the_microsoft_variant() {
        // Microsoft variant: top 3 bits of byte 8 are 110.
        let bytes = with_variant_top_bits(with_version(*Uuid::new_v4().as_bytes(), 4), 0b1100_0000);
        assert!(!is_uuid_v4(&Uuid::from_bytes(bytes)));
    }

    #[test]
    fn is_uuid_v4_rejects_version_nibble_4_with_the_future_variant() {
        // "Future" reserved variant: top 3 bits of byte 8 are 111.
        let bytes = with_variant_top_bits(with_version(*Uuid::new_v4().as_bytes(), 4), 0b1110_0000);
        assert!(!is_uuid_v4(&Uuid::from_bytes(bytes)));
    }

    #[test]
    fn is_uuid_v4_accepts_version_nibble_4_with_the_rfc4122_variant() {
        // RFC4122/RFC9562 standard variant: top 2 bits of byte 8 are 10 —
        // exactly what every real `Uuid::new_v4()` value carries.
        let bytes = with_variant_top_bits(with_version(*Uuid::new_v4().as_bytes(), 4), 0b1000_0000);
        assert!(is_uuid_v4(&Uuid::from_bytes(bytes)));
    }

    #[test]
    fn is_uuid_v4_rejects_a_value_with_a_different_version_nibble() {
        // A UUID whose version nibble is set to 1 rather than 4.
        let mut bytes = *Uuid::new_v4().as_bytes();
        bytes[6] = (bytes[6] & 0x0f) | 0x10;
        let not_v4 = Uuid::from_bytes(bytes);
        assert!(!is_uuid_v4(&not_v4));
    }
}
