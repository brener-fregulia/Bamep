//! Small explicit UTF-8 JSON codec for [`WorkerProtocolMessage`]
//! (`m1-worker-data-plane-control-contract.md` "Transport, framing, and
//! versioning"). No socket or framing types — encoding/decoding only; see
//! [`crate::framing`] for the length-prefix layer.

use crate::messages::WorkerProtocolMessage;

/// `Display`/`Debug` intentionally carry only `serde_json`'s own error
/// description (position, expected-type/variant information) — never the
/// raw input JSON, so a malformed message cannot leak arbitrary received
/// content through this error's textual representation.
#[derive(Debug, thiserror::Error)]
#[error("failed to encode Worker IPC message: {0}")]
pub struct EncodeError(#[source] serde_json::Error);

/// Every top-level `"type"` value this crate's [`WorkerProtocolMessage`]
/// recognizes. Kept in sync with that enum's variants by
/// `unknown_type_is_classified_distinctly_from_other_malformed_json`, which
/// fails if a variant is added here without a matching wire name.
const KNOWN_MESSAGE_TYPES: &[&str] = &[
    "WorkerHello",
    "ServerHello",
    "HandshakeRejected",
    "AuthorizationQuery",
    "AuthorizationDecision",
    "ProtocolError",
];

/// Distinguishes an unrecognized top-level `type` from every other
/// malformed-JSON case (correction audit "Unknown top-level Worker message
/// type"): the approved contract requires a receiver to answer an unknown
/// `type` with a stable `ProtocolError`
/// (`m1-worker-data-plane-control-contract.md`: "Unknown top-level type:
/// rejected with ProtocolError"), which requires the receiver to be able to
/// tell "the frame was parseable enough to see `type`, but `type` itself is
/// unrecognized" apart from "the frame was not even valid JSON" or "`type`
/// was recognized but the rest of the message was malformed" — both of
/// which remain generically `Malformed`.
#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    /// The payload was not valid JSON, or `type` was recognized but some
    /// other structural requirement of the message failed. Carries only
    /// `serde_json`'s own error description — never the raw input JSON.
    #[error("failed to decode Worker IPC message: {0}")]
    Malformed(#[source] serde_json::Error),
    /// The payload was a JSON object with a string `"type"` field that does
    /// not match any message this crate recognizes.
    #[error("unknown top-level message type {0:?}")]
    UnknownType(String),
}

/// Serializes a message to its UTF-8 JSON wire representation (one JSON
/// object per message, matching "a single UTF-8-encoded JSON object").
pub fn encode(message: &WorkerProtocolMessage) -> Result<String, EncodeError> {
    serde_json::to_string(message).map_err(EncodeError)
}

/// Parses a UTF-8 JSON wire value into a known message.
///
/// An unrecognized top-level `type` is classified as
/// [`DecodeError::UnknownType`] rather than folded into the generic
/// [`DecodeError::Malformed`] case, so a caller (the `bamepd`/Worker
/// handshake and post-handshake handlers) can answer it with a stable
/// `ProtocolError` rather than silently closing the connection. It is never
/// silently interpreted as some fallback variant.
pub fn decode(json: &str) -> Result<WorkerProtocolMessage, DecodeError> {
    let value: serde_json::Value = serde_json::from_str(json).map_err(DecodeError::Malformed)?;
    let type_name = value.get("type").and_then(|t| t.as_str());
    if let Some(type_name) = type_name {
        if !KNOWN_MESSAGE_TYPES.contains(&type_name) {
            return Err(DecodeError::UnknownType(type_name.to_string()));
        }
    }
    serde_json::from_value(value).map_err(DecodeError::Malformed)
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::messages::{ProtocolErrorMessage, WorkerHelloMessage};

    #[test]
    fn round_trips_worker_hello() {
        let instance_id = Uuid::new_v4();
        let message = WorkerProtocolMessage::WorkerHello(WorkerHelloMessage::new(instance_id));
        let encoded = encode(&message).expect("encode");
        let decoded = decode(&encoded).expect("decode");
        match decoded {
            WorkerProtocolMessage::WorkerHello(m) => {
                assert_eq!(m.body.worker_instance_id, instance_id);
                assert!(m.body.worker_protocol_version.is_v1());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn optional_fields_are_omitted_not_null() {
        let message =
            WorkerProtocolMessage::ProtocolError(ProtocolErrorMessage::new("malformed_frame"));
        let encoded = encode(&message).expect("encode");
        assert!(!encoded.contains("null"));
        assert!(!encoded.contains("\"message\""));
        assert!(!encoded.contains("\"in_reply_to\""));
    }

    #[test]
    fn unknown_type_is_rejected() {
        let json = r#"{"type":"Bogus","protocol_version":"1","message_id":"9d1c9a3e-5f3e-4a3b-8b0a-6a9b5f3a2b11"}"#;
        assert!(decode(json).is_err());
    }

    /// Correction audit "Unknown top-level Worker message type": an
    /// unrecognized `type` on an otherwise-parseable envelope must be
    /// classified as `DecodeError::UnknownType`, distinct from a
    /// non-JSON/malformed-known-message `DecodeError::Malformed`, so a
    /// caller can answer it with a stable `ProtocolError` instead of a
    /// generic close.
    #[test]
    fn unknown_type_is_classified_distinctly_from_malformed_json() {
        let json = r#"{"type":"Bogus","protocol_version":"1","message_id":"9d1c9a3e-5f3e-4a3b-8b0a-6a9b5f3a2b11"}"#;
        match decode(json) {
            Err(DecodeError::UnknownType(type_name)) => assert_eq!(type_name, "Bogus"),
            other => panic!("expected DecodeError::UnknownType, got {other:?}"),
        }

        match decode("{not json") {
            Err(DecodeError::Malformed(_)) => {}
            other => panic!("expected DecodeError::Malformed, got {other:?}"),
        }

        // A recognized `type` with a malformed body is `Malformed`, not
        // `UnknownType` — the classification is purely about the `type`
        // field itself.
        let known_type_malformed_body =
            r#"{"type":"WorkerHello","protocol_version":"1","message_id":"not-a-uuid"}"#;
        match decode(known_type_malformed_body) {
            Err(DecodeError::Malformed(_)) => {}
            other => panic!("expected DecodeError::Malformed, got {other:?}"),
        }
    }

    /// `KNOWN_MESSAGE_TYPES` must stay in sync with every
    /// `WorkerProtocolMessage` variant: encoding each real variant and
    /// decoding it back must never spuriously classify it as
    /// `DecodeError::UnknownType`.
    #[test]
    fn every_real_message_variant_round_trips_without_being_classified_unknown() {
        use crate::messages::{
            AuthorizationDecisionMessage, AuthorizationOperation, AuthorizationQueryMessage,
            HandshakeRejectedMessage, ServerHelloMessage, WireTransferDirection,
        };

        let variants = [
            WorkerProtocolMessage::WorkerHello(WorkerHelloMessage::new(Uuid::new_v4())),
            WorkerProtocolMessage::ServerHello(ServerHelloMessage::new(Uuid::new_v4())),
            WorkerProtocolMessage::HandshakeRejected(
                HandshakeRejectedMessage::incompatible_version(Uuid::new_v4()),
            ),
            WorkerProtocolMessage::AuthorizationQuery(AuthorizationQueryMessage::new(
                "t",
                AuthorizationOperation::ChunkUpload,
                Uuid::new_v4(),
                Uuid::new_v4(),
                WireTransferDirection::AgentToServer,
                Some(0),
                "p",
                1,
                "s",
            )),
            WorkerProtocolMessage::AuthorizationDecision(AuthorizationDecisionMessage::approved(
                Uuid::new_v4(),
                None,
            )),
            WorkerProtocolMessage::ProtocolError(ProtocolErrorMessage::new("some_code")),
        ];
        for variant in variants {
            let encoded = encode(&variant).expect("encode");
            match decode(&encoded) {
                Ok(_) => {}
                Err(err) => panic!("real variant misclassified: {err}"),
            }
        }
    }

    #[test]
    fn unknown_fields_inside_known_type_are_ignored() {
        let json = r#"{
            "type":"WorkerHello",
            "protocol_version":"1",
            "message_id":"9d1c9a3e-5f3e-4a3b-8b0a-6a9b5f3a2b11",
            "worker_protocol_version":"1",
            "worker_instance_id":"9d1c9a3e-5f3e-4a3b-8b0a-6a9b5f3a2b11",
            "totally_unexpected_future_field":"z"
        }"#;
        assert!(decode(json).is_ok());
    }

    #[test]
    fn malformed_json_is_rejected() {
        assert!(decode("{not json").is_err());
    }
}
