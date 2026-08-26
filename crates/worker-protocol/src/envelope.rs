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

/// Whether `id` is a UUID v4 (the version nibble equals 4), the normative
/// identity shape required for `message_id` and `worker_instance_id`
/// (`m1-worker-data-plane-control-contract.md` "Handshake"). Checks the
/// version nibble directly rather than `Uuid::get_version()` so a value that
/// is version-4-shaped but carries a non-RFC4122 variant is still accepted —
/// this is a wire-format shape check, not a generator-provenance proof.
pub fn is_uuid_v4(id: &Uuid) -> bool {
    id.get_version_num() == 4
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

    #[test]
    fn is_uuid_v4_rejects_a_value_with_a_different_version_nibble() {
        // A UUID whose version nibble is set to 1 rather than 4.
        let mut bytes = *Uuid::new_v4().as_bytes();
        bytes[6] = (bytes[6] & 0x0f) | 0x10;
        let not_v4 = Uuid::from_bytes(bytes);
        assert!(!is_uuid_v4(&not_v4));
    }
}
