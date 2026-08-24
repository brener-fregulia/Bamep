//! Contract tests for the Issue #28 Agent Protocol v1 reconciliation message
//! slice: `StatusQuery`, `StatusReport` (`docs/specifications/m0-agent-protocol-contract.md`
//! "Message types", "Message envelope", "Agent-action state vocabulary"). No
//! WSS/transport is exercised here — encode/decode of the wire shape only.

use bamep_agent_protocol::{
    codec, AgentProtocolMessage, KnownActionState, ProtocolId, StatusQueryMessage,
    StatusReportMessage,
};
use serde_json::Value;

// ---------------------------------------------------------------------
// StatusQuery
// ---------------------------------------------------------------------

#[test]
fn status_query_correlation_id_always_equals_action_id() {
    let action_id = ProtocolId::generate();
    let message = StatusQueryMessage::new(action_id);
    assert_eq!(message.envelope.correlation_id, Some(action_id));

    let json = codec::encode(&AgentProtocolMessage::StatusQuery(message)).unwrap();
    let value: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["type"], "StatusQuery");
    assert_eq!(value["action_id"], action_id.to_string());
    assert_eq!(value["correlation_id"], action_id.to_string());
}

#[test]
fn status_query_round_trips() {
    let action_id = ProtocolId::generate();
    let original = AgentProtocolMessage::StatusQuery(StatusQueryMessage::new(action_id));
    let json = codec::encode(&original).unwrap();
    let decoded = codec::decode(&json).unwrap();
    match decoded {
        AgentProtocolMessage::StatusQuery(m) => {
            assert_eq!(m.body.action_id, action_id);
            assert_eq!(m.envelope.correlation_id, Some(action_id));
        }
        other => panic!("wrong variant decoded: {other:?}"),
    }
}

#[test]
fn two_status_query_transmissions_use_distinct_message_ids() {
    let action_id = ProtocolId::generate();
    let first = StatusQueryMessage::new(action_id);
    let second = StatusQueryMessage::new(action_id);
    assert_ne!(first.envelope.message_id, second.envelope.message_id);
    assert_eq!(first.body.action_id, second.body.action_id);
}

#[test]
fn status_query_action_id_serializes_as_lowercase_hyphenated_uuidv4() {
    let action_id = ProtocolId::generate();
    let message = StatusQueryMessage::new(action_id);
    let json = codec::encode(&AgentProtocolMessage::StatusQuery(message)).unwrap();
    let value: Value = serde_json::from_str(&json).unwrap();
    let raw = value["action_id"].as_str().unwrap();
    assert_eq!(raw, raw.to_lowercase());
    let uuid = uuid::Uuid::parse_str(raw).unwrap();
    assert_eq!(uuid.get_version_num(), 4);
}

// ---------------------------------------------------------------------
// StatusReport
// ---------------------------------------------------------------------

#[test]
fn status_report_correlation_id_always_equals_action_id() {
    let action_id = ProtocolId::generate();
    let message = StatusReportMessage::new(action_id, KnownActionState::Running);
    assert_eq!(message.envelope.correlation_id, Some(action_id));

    let json = codec::encode(&AgentProtocolMessage::StatusReport(message)).unwrap();
    let value: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["type"], "StatusReport");
    assert_eq!(value["action_id"], action_id.to_string());
    assert_eq!(value["correlation_id"], action_id.to_string());
    assert_eq!(value["known_state"], "Running");
}

#[test]
fn known_state_vocabulary_is_exactly_accepted_running_succeeded_failed_cancelled_unknown() {
    for (state, wire) in [
        (KnownActionState::Accepted, "Accepted"),
        (KnownActionState::Running, "Running"),
        (KnownActionState::Succeeded, "Succeeded"),
        (KnownActionState::Failed, "Failed"),
        (KnownActionState::Cancelled, "Cancelled"),
        (KnownActionState::Unknown, "Unknown"),
    ] {
        let action_id = ProtocolId::generate();
        let message = StatusReportMessage::new(action_id, state);
        let json = codec::encode(&AgentProtocolMessage::StatusReport(message)).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["known_state"], wire);
    }
}

#[test]
fn status_report_round_trips() {
    let action_id = ProtocolId::generate();
    let original = AgentProtocolMessage::StatusReport(StatusReportMessage::new(
        action_id,
        KnownActionState::Unknown,
    ));
    let json = codec::encode(&original).unwrap();
    let decoded = codec::decode(&json).unwrap();
    match decoded {
        AgentProtocolMessage::StatusReport(m) => {
            assert_eq!(m.body.action_id, action_id);
            assert_eq!(m.body.known_state, KnownActionState::Unknown);
        }
        other => panic!("wrong variant decoded: {other:?}"),
    }
}

#[test]
fn status_report_re_emission_gets_a_fresh_message_id_but_preserves_correlation_and_body() {
    let action_id = ProtocolId::generate();
    let original = StatusReportMessage::new(action_id, KnownActionState::Succeeded);
    let original_message_id = original.envelope.message_id;
    let re_emitted = original.clone().with_fresh_message_id();

    assert_ne!(re_emitted.envelope.message_id, original_message_id);
    assert_eq!(re_emitted.envelope.correlation_id, Some(action_id));
    assert_eq!(re_emitted.body.action_id, action_id);
    assert_eq!(re_emitted.body.known_state, original.body.known_state);
}

#[test]
fn status_report_action_id_serializes_as_lowercase_hyphenated_uuidv4() {
    let action_id = ProtocolId::generate();
    let message = StatusReportMessage::new(action_id, KnownActionState::Failed);
    let json = codec::encode(&AgentProtocolMessage::StatusReport(message)).unwrap();
    let value: Value = serde_json::from_str(&json).unwrap();
    let raw = value["action_id"].as_str().unwrap();
    assert_eq!(raw, raw.to_lowercase());
    let uuid = uuid::Uuid::parse_str(raw).unwrap();
    assert_eq!(uuid.get_version_num(), 4);
}

#[test]
fn unknown_top_level_status_type_typo_is_rejected_explicitly() {
    let envelope = bamep_agent_protocol::Envelope::new();
    let action_id = ProtocolId::generate();
    let json = format!(
        r#"{{"type":"StatusQueryX","message_id":"{}","protocol_version":"1","timestamp":"{}","correlation_id":"{}","action_id":"{}"}}"#,
        envelope.message_id,
        envelope.timestamp.as_datetime().to_rfc3339(),
        action_id,
        action_id,
    );
    assert!(codec::decode(&json).is_err());
}

#[test]
fn unknown_known_state_value_is_rejected_explicitly() {
    let action_id = ProtocolId::generate();
    let envelope = bamep_agent_protocol::Envelope::new();
    let json = format!(
        r#"{{"type":"StatusReport","message_id":"{}","protocol_version":"1","timestamp":"{}","correlation_id":"{}","action_id":"{}","known_state":"NotARealState"}}"#,
        envelope.message_id,
        envelope.timestamp.as_datetime().to_rfc3339(),
        action_id,
        action_id,
    );
    assert!(codec::decode(&json).is_err());
}
