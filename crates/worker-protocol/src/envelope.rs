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
}

impl Default for Envelope {
    fn default() -> Self {
        Self::new()
    }
}
