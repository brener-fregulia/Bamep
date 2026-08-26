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

#[derive(Debug, thiserror::Error)]
#[error("failed to decode Worker IPC message: {0}")]
pub struct DecodeError(#[source] serde_json::Error);

/// Serializes a message to its UTF-8 JSON wire representation (one JSON
/// object per message, matching "a single UTF-8-encoded JSON object").
pub fn encode(message: &WorkerProtocolMessage) -> Result<String, EncodeError> {
    serde_json::to_string(message).map_err(EncodeError)
}

/// Parses a UTF-8 JSON wire value into a known message.
///
/// An unrecognized top-level `type` (or any other structural violation)
/// fails explicitly via [`DecodeError`] — it is never silently interpreted
/// as some fallback variant.
pub fn decode(json: &str) -> Result<WorkerProtocolMessage, DecodeError> {
    serde_json::from_str(json).map_err(DecodeError)
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
