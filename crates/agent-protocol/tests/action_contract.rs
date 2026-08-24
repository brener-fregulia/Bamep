//! Contract tests for the Issue #26 Agent Protocol v1 typed-action message
//! slice: `ActionDispatch`, `ActionAck`, `ActionProgress`, `ActionResult`
//! (`docs/specifications/m0-agent-protocol-contract.md` "Action field
//! contract", "ActionAck diagnostic shape", "ActionProgress fields"). No
//! WSS/transport is exercised here — encode/decode of the wire shape only.

use bamep_agent_protocol::{
    codec, ActionAckError, ActionAckMessage, ActionAckOutcome, ActionDispatchMessage,
    ActionProgressMessage, ActionResultMessage, ActionResultOutcome, AgentProtocolMessage, Percent,
    ProtocolId,
};
use serde_json::{Map, Value};

fn empty_params() -> Map<String, Value> {
    Map::new()
}

// ---------------------------------------------------------------------
// ActionDispatch
// ---------------------------------------------------------------------

#[test]
fn action_dispatch_correlation_id_always_equals_action_id() {
    let action_id = ProtocolId::generate();
    let message = ActionDispatchMessage::new(
        action_id,
        "bamep.m1.simulated-execution",
        "1",
        empty_params(),
    );
    assert_eq!(message.envelope.correlation_id, Some(action_id));

    let json = codec::encode(&AgentProtocolMessage::ActionDispatch(message)).unwrap();
    let value: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["type"], "ActionDispatch");
    assert_eq!(value["action_id"], action_id.to_string());
    assert_eq!(value["correlation_id"], action_id.to_string());
    assert_eq!(value["action_version"], "1");
    assert_eq!(value["parameters"], serde_json::json!({}));
    assert!(
        value.get("retry_of").is_none(),
        "retry_of must be omitted, never null, when absent"
    );
}

#[test]
fn action_dispatch_round_trips_with_closed_empty_m1_parameters() {
    let action_id = ProtocolId::generate();
    let original = AgentProtocolMessage::ActionDispatch(ActionDispatchMessage::new(
        action_id,
        "bamep.m1.simulated-execution",
        "1",
        empty_params(),
    ));
    let json = codec::encode(&original).unwrap();
    let decoded = codec::decode(&json).unwrap();
    match decoded {
        AgentProtocolMessage::ActionDispatch(m) => {
            assert_eq!(m.body.action_id, action_id);
            assert_eq!(m.body.action_type, "bamep.m1.simulated-execution");
            assert_eq!(m.body.action_version, "1");
            assert!(m.body.parameters.is_empty());
            assert_eq!(m.body.retry_of, None);
        }
        other => panic!("wrong variant decoded: {other:?}"),
    }
}

#[test]
fn action_dispatch_retry_of_is_a_distinct_action_id_when_present() {
    let action_id = ProtocolId::generate();
    let retry_of = ProtocolId::generate();
    assert_ne!(action_id, retry_of);
    let message = ActionDispatchMessage::new(
        action_id,
        "bamep.m1.simulated-execution",
        "1",
        empty_params(),
    )
    .with_retry_of(retry_of);
    let json = codec::encode(&AgentProtocolMessage::ActionDispatch(message)).unwrap();
    let value: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["retry_of"], retry_of.to_string());
}

// ---------------------------------------------------------------------
// ActionAck
// ---------------------------------------------------------------------

#[test]
fn action_ack_accepted_has_no_error_field_and_correlates() {
    let action_id = ProtocolId::generate();
    let message = ActionAckMessage::accepted(action_id);
    assert_eq!(message.envelope.correlation_id, Some(action_id));
    let json = codec::encode(&AgentProtocolMessage::ActionAck(message)).unwrap();
    let value: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["outcome"], "Accepted");
    assert!(
        value.get("error").is_none(),
        "Accepted must never carry error"
    );
}

#[test]
fn action_ack_rejected_carries_the_diagnostic_shape() {
    let action_id = ProtocolId::generate();
    let message = ActionAckMessage::rejected(action_id, ActionAckError::new("UNSUPPORTED_ACTION"));
    let json = codec::encode(&AgentProtocolMessage::ActionAck(message)).unwrap();
    let value: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["outcome"], "Rejected");
    assert_eq!(value["error"]["code"], "UNSUPPORTED_ACTION");
    assert!(value["error"].get("message").is_none());
}

#[test]
fn action_ack_rejected_with_message_round_trips() {
    let action_id = ProtocolId::generate();
    let error =
        ActionAckError::new("INVALID_PARAMETERS").with_message("unexpected key in parameters");
    let original =
        AgentProtocolMessage::ActionAck(ActionAckMessage::rejected(action_id, error.clone()));
    let json = codec::encode(&original).unwrap();
    let decoded = codec::decode(&json).unwrap();
    match decoded {
        AgentProtocolMessage::ActionAck(m) => {
            assert_eq!(m.body.outcome, ActionAckOutcome::Rejected);
            assert_eq!(m.body.error, Some(error));
        }
        other => panic!("wrong variant decoded: {other:?}"),
    }
}

#[test]
fn action_ack_re_emission_gets_a_fresh_message_id_but_preserves_correlation_and_body() {
    let action_id = ProtocolId::generate();
    let original = ActionAckMessage::accepted(action_id);
    let original_message_id = original.envelope.message_id;
    let re_emitted = original.clone().with_fresh_message_id();

    assert_ne!(re_emitted.envelope.message_id, original_message_id);
    assert_eq!(re_emitted.envelope.correlation_id, Some(action_id));
    assert_eq!(re_emitted.body.action_id, action_id);
    assert_eq!(re_emitted.body.outcome, original.body.outcome);
}

// ---------------------------------------------------------------------
// ActionProgress
// ---------------------------------------------------------------------

#[test]
fn action_progress_percent_only_matches_the_m1_deterministic_progression() {
    let action_id = ProtocolId::generate();
    for raw in [0u8, 50, 100] {
        let percent = Percent::new(raw).unwrap();
        let message = ActionProgressMessage::percent(action_id, percent);
        let json = codec::encode(&AgentProtocolMessage::ActionProgress(message)).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["percent"], raw);
        assert!(value.get("bytes_processed").is_none());
        assert!(value.get("eta").is_none());
    }
}

#[test]
fn action_progress_percent_above_100_is_rejected_at_construction() {
    assert!(Percent::new(101).is_err());
    assert!(Percent::new(255).is_err());
    assert!(Percent::new(100).is_ok());
}

#[test]
fn action_progress_percent_above_100_is_rejected_on_the_wire_too() {
    let envelope = bamep_agent_protocol::Envelope::new();
    let json = format!(
        r#"{{"type":"ActionProgress","message_id":"{}","protocol_version":"1","timestamp":"{}","action_id":"{}","percent":101}}"#,
        envelope.message_id,
        envelope.timestamp.as_datetime().to_rfc3339(),
        ProtocolId::generate(),
    );
    assert!(
        codec::decode(&json).is_err(),
        "an out-of-range percent must be rejected explicitly, never clamped"
    );
}

#[test]
fn action_progress_requires_at_least_one_field() {
    let action_id = ProtocolId::generate();
    assert!(ActionProgressMessage::new(action_id, None, None, None).is_err());
    assert!(ActionProgressMessage::new(action_id, None, Some(1024), None).is_ok());
}

#[test]
fn action_progress_is_transient_and_carries_no_terminal_authority_field() {
    // Structural proof that ActionProgress has no outcome/detail field at
    // all — it cannot represent a terminal result, only advisory metadata.
    let action_id = ProtocolId::generate();
    let message = ActionProgressMessage::percent(action_id, Percent::new(50).unwrap());
    let json = codec::encode(&AgentProtocolMessage::ActionProgress(message)).unwrap();
    let value: Value = serde_json::from_str(&json).unwrap();
    assert!(value.get("outcome").is_none());
    assert!(value.get("detail").is_none());
}

// ---------------------------------------------------------------------
// ActionResult
// ---------------------------------------------------------------------

#[test]
fn action_result_succeeded_detail_matches_the_m1_deterministic_schema() {
    let action_id = ProtocolId::generate();
    let detail = serde_json::json!({"code": "SIMULATED_COMPLETION"})
        .as_object()
        .unwrap()
        .clone();
    let message = ActionResultMessage::new(action_id, ActionResultOutcome::Succeeded, detail);
    let json = codec::encode(&AgentProtocolMessage::ActionResult(message)).unwrap();
    let value: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["outcome"], "Succeeded");
    assert_eq!(value["detail"]["code"], "SIMULATED_COMPLETION");
    assert_eq!(value["correlation_id"], action_id.to_string());
}

#[test]
fn action_result_failed_detail_matches_the_m1_deterministic_schema() {
    let action_id = ProtocolId::generate();
    let detail = serde_json::json!({"code": "SIMULATED_FAILURE"})
        .as_object()
        .unwrap()
        .clone();
    let original = AgentProtocolMessage::ActionResult(ActionResultMessage::new(
        action_id,
        ActionResultOutcome::Failed,
        detail.clone(),
    ));
    let json = codec::encode(&original).unwrap();
    let decoded = codec::decode(&json).unwrap();
    match decoded {
        AgentProtocolMessage::ActionResult(m) => {
            assert_eq!(m.body.outcome, ActionResultOutcome::Failed);
            assert_eq!(m.body.detail, detail);
        }
        other => panic!("wrong variant decoded: {other:?}"),
    }
}

#[test]
fn action_result_outcome_vocabulary_is_exactly_succeeded_failed_cancelled() {
    for (outcome, wire) in [
        (ActionResultOutcome::Succeeded, "Succeeded"),
        (ActionResultOutcome::Failed, "Failed"),
        (ActionResultOutcome::Cancelled, "Cancelled"),
    ] {
        let message = ActionResultMessage::new(ProtocolId::generate(), outcome, empty_params());
        let json = codec::encode(&AgentProtocolMessage::ActionResult(message)).unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["outcome"], wire);
    }
}

#[test]
fn action_result_re_emission_gets_a_fresh_message_id_but_preserves_correlation_and_body() {
    let action_id = ProtocolId::generate();
    let detail = serde_json::json!({"code": "SIMULATED_COMPLETION"})
        .as_object()
        .unwrap()
        .clone();
    let original =
        ActionResultMessage::new(action_id, ActionResultOutcome::Succeeded, detail.clone());
    let original_message_id = original.envelope.message_id;
    let re_emitted = original.with_fresh_message_id();

    assert_ne!(re_emitted.envelope.message_id, original_message_id);
    assert_eq!(re_emitted.envelope.correlation_id, Some(action_id));
    assert_eq!(re_emitted.body.detail, detail);
}

// ---------------------------------------------------------------------
// Cross-cutting: action_id/message_id remain lowercase hyphenated UUIDv4
// ---------------------------------------------------------------------

#[test]
fn action_id_serializes_as_lowercase_hyphenated_uuidv4() {
    let action_id = ProtocolId::generate();
    let message = ActionDispatchMessage::new(
        action_id,
        "bamep.m1.simulated-execution",
        "1",
        empty_params(),
    );
    let json = codec::encode(&AgentProtocolMessage::ActionDispatch(message)).unwrap();
    let value: Value = serde_json::from_str(&json).unwrap();
    let raw = value["action_id"].as_str().unwrap();
    assert_eq!(raw, raw.to_lowercase());
    let uuid = uuid::Uuid::parse_str(raw).unwrap();
    assert_eq!(uuid.get_version_num(), 4);
}
