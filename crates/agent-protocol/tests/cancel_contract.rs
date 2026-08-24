//! Contract tests for the Issue #27 Agent Protocol v1 cancellation message
//! slice: `CancelAction`, `CancelAck` (`docs/specifications/m0-agent-protocol-contract.md`
//! "Message types", "Message envelope"). No WSS/transport is exercised here —
//! encode/decode of the wire shape only.

use bamep_agent_protocol::{
    codec, AgentProtocolMessage, CancelAckMessage, CancelAckOutcome, CancelActionMessage,
    ProtocolId,
};
use serde_json::Value;

// ---------------------------------------------------------------------
// CancelAction
// ---------------------------------------------------------------------

#[test]
fn cancel_action_correlation_id_always_equals_action_id() {
    let action_id = ProtocolId::generate();
    let message = CancelActionMessage::new(action_id);
    assert_eq!(message.envelope.correlation_id, Some(action_id));

    let json = codec::encode(&AgentProtocolMessage::CancelAction(message)).unwrap();
    let value: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["type"], "CancelAction");
    assert_eq!(value["action_id"], action_id.to_string());
    assert_eq!(value["correlation_id"], action_id.to_string());
}

#[test]
fn cancel_action_round_trips() {
    let action_id = ProtocolId::generate();
    let original = AgentProtocolMessage::CancelAction(CancelActionMessage::new(action_id));
    let json = codec::encode(&original).unwrap();
    let decoded = codec::decode(&json).unwrap();
    match decoded {
        AgentProtocolMessage::CancelAction(m) => {
            assert_eq!(m.body.action_id, action_id);
            assert_eq!(m.envelope.correlation_id, Some(action_id));
        }
        other => panic!("wrong variant decoded: {other:?}"),
    }
}

#[test]
fn two_cancel_action_transmissions_use_distinct_message_ids() {
    let action_id = ProtocolId::generate();
    let first = CancelActionMessage::new(action_id);
    let second = CancelActionMessage::new(action_id);
    assert_ne!(first.envelope.message_id, second.envelope.message_id);
    assert_eq!(first.body.action_id, second.body.action_id);
}

// ---------------------------------------------------------------------
// CancelAck
// ---------------------------------------------------------------------

#[test]
fn cancel_ack_correlation_id_always_equals_action_id() {
    let action_id = ProtocolId::generate();
    let message = CancelAckMessage::new(action_id, CancelAckOutcome::Cancelled);
    assert_eq!(message.envelope.correlation_id, Some(action_id));

    let json = codec::encode(&AgentProtocolMessage::CancelAck(message)).unwrap();
    let value: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["type"], "CancelAck");
    assert_eq!(value["action_id"], action_id.to_string());
    assert_eq!(value["correlation_id"], action_id.to_string());
    assert_eq!(value["outcome"], "Cancelled");
}

#[test]
fn cancel_ack_outcome_vocabulary_is_exactly_cancelled_already_completed_cannot_cancel_unknown() {
    for (outcome, wire) in [
        (CancelAckOutcome::Cancelled, "Cancelled"),
        (CancelAckOutcome::AlreadyCompleted, "AlreadyCompleted"),
        (CancelAckOutcome::CannotCancel, "CannotCancel"),
        (CancelAckOutcome::Unknown, "Unknown"),
    ] {
        let action_id = ProtocolId::generate();
        let message = CancelAckMessage::new(action_id, outcome);
        let json = codec::encode(&AgentProtocolMessage::CancelAck(message)).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["outcome"], wire);
    }
}

#[test]
fn cancel_ack_round_trips() {
    let action_id = ProtocolId::generate();
    let original = AgentProtocolMessage::CancelAck(CancelAckMessage::new(
        action_id,
        CancelAckOutcome::Unknown,
    ));
    let json = codec::encode(&original).unwrap();
    let decoded = codec::decode(&json).unwrap();
    match decoded {
        AgentProtocolMessage::CancelAck(m) => {
            assert_eq!(m.body.action_id, action_id);
            assert_eq!(m.body.outcome, CancelAckOutcome::Unknown);
        }
        other => panic!("wrong variant decoded: {other:?}"),
    }
}

#[test]
fn cancel_ack_re_emission_gets_a_fresh_message_id_but_preserves_correlation_and_body() {
    let action_id = ProtocolId::generate();
    let original = CancelAckMessage::new(action_id, CancelAckOutcome::Cancelled);
    let original_message_id = original.envelope.message_id;
    let re_emitted = original.clone().with_fresh_message_id();

    assert_ne!(re_emitted.envelope.message_id, original_message_id);
    assert_eq!(re_emitted.envelope.correlation_id, Some(action_id));
    assert_eq!(re_emitted.body.action_id, action_id);
    assert_eq!(re_emitted.body.outcome, original.body.outcome);
}

#[test]
fn cancel_action_id_serializes_as_lowercase_hyphenated_uuidv4() {
    let action_id = ProtocolId::generate();
    let message = CancelActionMessage::new(action_id);
    let json = codec::encode(&AgentProtocolMessage::CancelAction(message)).unwrap();
    let value: Value = serde_json::from_str(&json).unwrap();
    let raw = value["action_id"].as_str().unwrap();
    assert_eq!(raw, raw.to_lowercase());
    let uuid = uuid::Uuid::parse_str(raw).unwrap();
    assert_eq!(uuid.get_version_num(), 4);
}

#[test]
fn unknown_top_level_cancel_type_typo_is_rejected_explicitly() {
    let envelope = bamep_agent_protocol::Envelope::new();
    let action_id = ProtocolId::generate();
    let json = format!(
        r#"{{"type":"CancelActionX","message_id":"{}","protocol_version":"1","timestamp":"{}","correlation_id":"{}","action_id":"{}"}}"#,
        envelope.message_id,
        envelope.timestamp.as_datetime().to_rfc3339(),
        action_id,
        action_id,
    );
    assert!(codec::decode(&json).is_err());
}
