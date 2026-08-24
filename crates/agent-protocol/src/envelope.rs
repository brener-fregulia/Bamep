//! Common Agent Protocol v1 message envelope fields and their wire-encoding
//! primitives (`docs/specifications/m0-agent-protocol-contract.md` "Message
//! envelope", "Wire encoding").
//!
//! Every Agent Protocol message is one flat JSON object carrying these
//! envelope fields alongside its message-specific fields at the same level —
//! there is no nested "envelope" or "payload" object on the wire. The
//! [`Envelope`] struct exists only as a Rust-side grouping, flattened into
//! each concrete message struct via `#[serde(flatten)]`.

use std::fmt;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

/// Wire textual value of the currently supported Agent Protocol version.
pub const PROTOCOL_VERSION_V1: &str = "1";

/// `protocol_version` as carried on the wire: a string, not a closed enum.
///
/// The Specification requires a future Server handshake to be able to
/// *receive* an incompatible value (e.g. `"2"`) and explicitly reject it with
/// `AuthError`, rather than have deserialization itself fail. This type
/// therefore preserves whatever textual value was received; comparing
/// against [`ProtocolVersion::v1`] (or [`ProtocolVersion::is_v1`]) is a
/// caller-level decision, not a parse-time one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolVersion(String);

impl ProtocolVersion {
    /// The currently supported Agent Protocol v1 value.
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

impl Serialize for ProtocolVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ProtocolVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self)
    }
}

/// Rejects a wire value that fails the Agent Protocol identifier invariant
/// enforced by [`ProtocolId`]'s `Deserialize` implementation
/// (`docs/specifications/m0-agent-protocol-contract.md` "Wire encoding":
/// "UUID (version 4) represented as a lowercase hyphenated string").
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolIdError {
    /// Syntactically valid UUID, but not version 4.
    #[error("protocol identifier is not a version-4 UUID")]
    NotUuidV4,
    /// A valid version-4 UUID, but its textual form is not exactly the
    /// canonical lowercase-hyphenated representation (e.g. uppercase hex, a
    /// non-hyphenated/simple form, or a braced/urn form).
    #[error("protocol identifier is not the canonical lowercase-hyphenated UUID representation")]
    NonCanonicalRepresentation,
}

/// A `message_id` / `session_id` / `correlation_id` (and, in future
/// checkpoints, `action_id`) wire identifier: UUID version 4, encoded as the
/// canonical lowercase hyphenated string
/// (`docs/specifications/m0-agent-protocol-contract.md` "Wire encoding").
///
/// Generation always produces a v4 UUID, serialized in canonical form.
/// Deserialization enforces the full wire invariant on *input*, not only on
/// output: the value must (1) be a syntactically valid UUID, (2) be version
/// 4, and (3) equal — byte-for-byte — the canonical lowercase-hyphenated
/// textual representation of that UUID. A syntactically valid UUID accepted
/// by a permissive parser but expressed in a non-canonical form (uppercase
/// hex, a non-hyphenated/simple representation, a braced/urn form, etc.) is
/// rejected explicitly rather than silently normalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProtocolId(Uuid);

impl ProtocolId {
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wraps an already-known UUID, rejecting anything that is not version 4.
    /// This accepts a parsed [`Uuid`], so it has no textual form to compare
    /// against — the canonical-representation check only applies to
    /// [`Deserialize`], which starts from wire text.
    pub fn from_uuid(uuid: Uuid) -> Result<Self, ProtocolIdError> {
        if uuid.get_version_num() != 4 {
            return Err(ProtocolIdError::NotUuidV4);
        }
        Ok(Self(uuid))
    }

    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl fmt::Display for ProtocolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for ProtocolId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for ProtocolId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        let uuid = Uuid::parse_str(&raw).map_err(D::Error::custom)?;
        let id = ProtocolId::from_uuid(uuid).map_err(D::Error::custom)?;
        // `Uuid`'s `Display` always renders the canonical lowercase
        // hyphenated form, so a straight string comparison against the
        // original wire text is exactly the "canonical representation"
        // check — anything a permissive parser accepted but that isn't
        // already in that exact form differs here and is rejected.
        if raw != id.0.to_string() {
            return Err(D::Error::custom(
                ProtocolIdError::NonCanonicalRepresentation,
            ));
        }
        Ok(id)
    }
}

/// Wire timestamp: RFC 3339 / ISO 8601, always UTC, always a JSON string —
/// never an epoch integer
/// (`docs/specifications/m0-agent-protocol-contract.md` "Wire encoding").
///
/// Serializes with a `Z` UTC designator (e.g. `2026-08-14T21:00:00Z`), not
/// `+00:00` — the Specification's own example uses `Z`. Millisecond
/// sub-second precision is this implementation's own formatting choice, not
/// itself a Specification requirement: the Specification requires an RFC
/// 3339 UTC string and does not mandate (or forbid) any particular
/// sub-second precision. Deserialization accepts any valid RFC 3339 string
/// and normalizes it to UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageTimestamp(DateTime<Utc>);

impl MessageTimestamp {
    pub fn now() -> Self {
        Self(Utc::now())
    }

    pub fn from_datetime(value: DateTime<Utc>) -> Self {
        Self(value)
    }

    pub fn as_datetime(&self) -> DateTime<Utc> {
        self.0
    }
}

impl From<DateTime<Utc>> for MessageTimestamp {
    fn from(value: DateTime<Utc>) -> Self {
        Self(value)
    }
}

impl Serialize for MessageTimestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_rfc3339_opts(SecondsFormat::Millis, true))
    }
}

impl<'de> Deserialize<'de> for MessageTimestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        let parsed = DateTime::parse_from_rfc3339(&raw).map_err(D::Error::custom)?;
        Ok(Self(parsed.with_timezone(&Utc)))
    }
}

/// Rejects a value outside `0..=100`
/// (`docs/specifications/m0-agent-protocol-contract.md` "ActionProgress
/// fields": "`percent` is an integer in `0..=100`").
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("percent must be in 0..=100")]
pub struct PercentOutOfRange;

/// `ActionProgress.percent`: an integer in `0..=100`, enforced on
/// construction and on wire input alike — deserializing an out-of-range value
/// fails explicitly rather than silently clamping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Percent(u8);

impl Percent {
    pub fn new(value: u8) -> Result<Self, PercentOutOfRange> {
        if value > 100 {
            return Err(PercentOutOfRange);
        }
        Ok(Self(value))
    }

    pub fn value(self) -> u8 {
        self.0
    }
}

impl Serialize for Percent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(self.0)
    }
}

impl<'de> Deserialize<'de> for Percent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = u8::deserialize(deserializer)?;
        Percent::new(raw).map_err(D::Error::custom)
    }
}

/// Fields common to every Agent Protocol v1 message, flattened into each
/// concrete message struct rather than nested under a `"envelope"` key on
/// the wire (`m0-agent-protocol-contract.md` "Message envelope").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub message_id: ProtocolId,
    pub protocol_version: ProtocolVersion,
    pub timestamp: MessageTimestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<ProtocolId>,
}

impl Envelope {
    /// A fresh v1 envelope stamped with a new `message_id` and the current
    /// time, no `correlation_id`.
    pub fn new() -> Self {
        Self {
            message_id: ProtocolId::generate(),
            protocol_version: ProtocolVersion::v1(),
            timestamp: MessageTimestamp::now(),
            correlation_id: None,
        }
    }

    pub fn with_correlation_id(mut self, correlation_id: ProtocolId) -> Self {
        self.correlation_id = Some(correlation_id);
        self
    }
}

impl Default for Envelope {
    fn default() -> Self {
        Self::new()
    }
}
