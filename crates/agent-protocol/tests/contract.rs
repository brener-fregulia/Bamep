//! Contract tests for the WP1 Agent Protocol v1 handshake/evidence message
//! slice (`docs/specifications/m0-agent-protocol-contract.md` "Validation
//! expectations (contract tests)"; `docs/development/testing.md` "Contract
//! tests"). No WSS/transport is exercised here — encode/decode of the wire
//! shape only.

use bamep_agent_protocol::{
    codec, AgentProtocolMessage, AuthErrorMessage, AuthRequestMessage, BootstrapEvidenceMessage,
    Envelope, InventoryReportMessage, LocalBootTrust, ProtocolErrorMessage, ProtocolId,
    SessionEstablishedMessage,
};
use chrono::Utc;
use serde_json::Value;
use uuid::Uuid;

fn assert_rfc3339_utc_string(value: &Value, field: &str) {
    let raw = value
        .get(field)
        .unwrap_or_else(|| panic!("missing field {field}"));
    let s = raw
        .as_str()
        .unwrap_or_else(|| panic!("field {field} is not a JSON string: {raw:?}"));
    chrono::DateTime::parse_from_rfc3339(s)
        .unwrap_or_else(|e| panic!("field {field} is not RFC3339: {e}"));
    assert!(
        s.ends_with('Z'),
        "field {field} does not use the UTC 'Z' designator: {s}"
    );
}

fn assert_uuid_v4_string(value: &Value, field: &str) {
    let raw = value
        .get(field)
        .unwrap_or_else(|| panic!("missing field {field}"));
    let s = raw
        .as_str()
        .unwrap_or_else(|| panic!("field {field} is not a JSON string: {raw:?}"));
    assert_eq!(s, s.to_lowercase(), "field {field} is not lowercase");
    let uuid = Uuid::parse_str(s).unwrap_or_else(|e| panic!("field {field} is not a UUID: {e}"));
    assert_eq!(uuid.get_version_num(), 4, "field {field} is not UUIDv4");
}

// ---------------------------------------------------------------------
// 1. AuthRequest wire shape
// ---------------------------------------------------------------------

#[test]
fn auth_request_serializes_flat_with_required_envelope_fields() {
    let message = AuthRequestMessage::new("opaque-credential-wire-value");
    let json = codec::encode(&AgentProtocolMessage::AuthRequest(message)).expect("encodes");
    let value: Value = serde_json::from_str(&json).expect("valid json");

    assert_eq!(value["type"], "AuthRequest");
    assert_eq!(value["protocol_version"], "1");
    assert_eq!(value["credential"], "opaque-credential-wire-value");
    assert_uuid_v4_string(&value, "message_id");
    assert_rfc3339_utc_string(&value, "timestamp");
    assert!(
        value.get("correlation_id").is_none(),
        "correlation_id must be omitted when absent, got {value}"
    );
}

// ---------------------------------------------------------------------
// 2. SessionEstablished wire shape
// ---------------------------------------------------------------------

#[test]
fn session_established_carries_required_fields() {
    let session_id = ProtocolId::generate();
    let expires_at = Utc::now().into();
    let message =
        SessionEstablishedMessage::new(session_id, "opaque-runtime-credential", expires_at);
    let json = codec::encode(&AgentProtocolMessage::SessionEstablished(message)).expect("encodes");
    let value: Value = serde_json::from_str(&json).expect("valid json");

    assert_eq!(value["type"], "SessionEstablished");
    assert_eq!(value["runtime_credential"], "opaque-runtime-credential");
    assert_uuid_v4_string(&value, "session_id");
    assert_rfc3339_utc_string(&value, "credential_expires_at");
}

// ---------------------------------------------------------------------
// 3. AuthError wire shape
// ---------------------------------------------------------------------

#[test]
fn auth_error_carries_only_reason_no_invented_detail_fields() {
    let message = AuthErrorMessage::new("rejected");
    let json = codec::encode(&AgentProtocolMessage::AuthError(message)).expect("encodes");
    let value: Value = serde_json::from_str(&json).expect("valid json");

    assert_eq!(value["type"], "AuthError");
    assert_eq!(value["reason"], "rejected");

    let object = value.as_object().expect("object");
    let allowed = [
        "type",
        "message_id",
        "protocol_version",
        "timestamp",
        "correlation_id",
        "reason",
    ];
    for key in object.keys() {
        assert!(
            allowed.contains(&key.as_str()),
            "unexpected field on AuthError: {key}"
        );
    }
}

// ---------------------------------------------------------------------
// 4. BootstrapEvidence wire shape
// ---------------------------------------------------------------------

#[test]
fn bootstrap_evidence_carries_required_fields_and_closed_trust_value() {
    let message = BootstrapEvidenceMessage::new("nonce-value", "assertion-value");
    let json = codec::encode(&AgentProtocolMessage::BootstrapEvidence(message)).expect("encodes");
    let value: Value = serde_json::from_str(&json).expect("valid json");

    assert_eq!(value["type"], "BootstrapEvidence");
    assert_eq!(value["boot_nonce"], "nonce-value");
    assert_eq!(value["bootstrap_assertion"], "assertion-value");
    assert_eq!(value["local_boot_trust"], "Established");
}

// ---------------------------------------------------------------------
// 5. Round-trip encode/decode for every implemented message
// ---------------------------------------------------------------------

#[test]
fn auth_request_round_trips() {
    let original = AgentProtocolMessage::AuthRequest(AuthRequestMessage::new("cred-1"));
    let json = codec::encode(&original).expect("encodes");
    let decoded = codec::decode(&json).expect("decodes");
    match decoded {
        AgentProtocolMessage::AuthRequest(m) => assert_eq!(m.body.credential, "cred-1"),
        other => panic!("wrong variant decoded: {other:?}"),
    }
}

#[test]
fn session_established_round_trips() {
    let session_id = ProtocolId::generate();
    let expires_at = Utc::now().into();
    let original = AgentProtocolMessage::SessionEstablished(SessionEstablishedMessage::new(
        session_id,
        "runtime-cred-1",
        expires_at,
    ));
    let json = codec::encode(&original).expect("encodes");
    let decoded = codec::decode(&json).expect("decodes");
    match decoded {
        AgentProtocolMessage::SessionEstablished(m) => {
            assert_eq!(m.body.session_id, session_id);
            assert_eq!(m.body.runtime_credential, "runtime-cred-1");
        }
        other => panic!("wrong variant decoded: {other:?}"),
    }
}

#[test]
fn auth_error_round_trips() {
    let original = AgentProtocolMessage::AuthError(AuthErrorMessage::new("rejected"));
    let json = codec::encode(&original).expect("encodes");
    let decoded = codec::decode(&json).expect("decodes");
    match decoded {
        AgentProtocolMessage::AuthError(m) => assert_eq!(m.body.reason, "rejected"),
        other => panic!("wrong variant decoded: {other:?}"),
    }
}

#[test]
fn bootstrap_evidence_round_trips() {
    let original = AgentProtocolMessage::BootstrapEvidence(BootstrapEvidenceMessage::new(
        "nonce-1",
        "assertion-1",
    ));
    let json = codec::encode(&original).expect("encodes");
    let decoded = codec::decode(&json).expect("decodes");
    match decoded {
        AgentProtocolMessage::BootstrapEvidence(m) => {
            assert_eq!(m.body.boot_nonce, "nonce-1");
            assert_eq!(m.body.bootstrap_assertion, "assertion-1");
            assert_eq!(m.body.local_boot_trust, LocalBootTrust::Established);
        }
        other => panic!("wrong variant decoded: {other:?}"),
    }
}

// ---------------------------------------------------------------------
// 6. protocol_version is textual and preserves unsupported values
// ---------------------------------------------------------------------

#[test]
fn protocol_version_is_textual_and_unsupported_value_is_preserved_not_rejected() {
    let mut message = AuthRequestMessage::new("cred");
    message.envelope.protocol_version = bamep_agent_protocol::ProtocolVersion::new("2");
    let json = codec::encode(&AgentProtocolMessage::AuthRequest(message)).expect("encodes");
    let value: Value = serde_json::from_str(&json).expect("valid json");
    assert_eq!(value["protocol_version"], "2");

    let decoded = codec::decode(&json).expect("decodes despite unsupported version");
    match decoded {
        AgentProtocolMessage::AuthRequest(m) => {
            assert_eq!(m.envelope.protocol_version.as_str(), "2");
            assert!(!m.envelope.protocol_version.is_v1());
        }
        other => panic!("wrong variant decoded: {other:?}"),
    }
}

// ---------------------------------------------------------------------
// 7 & 8. UUIDv4 identifier serialization and non-v4 rejection
// ---------------------------------------------------------------------

#[test]
fn message_id_and_session_id_are_lowercase_hyphenated_uuidv4() {
    let session_id = ProtocolId::generate();
    let message = SessionEstablishedMessage::new(session_id, "cred", Utc::now().into());
    let json = codec::encode(&AgentProtocolMessage::SessionEstablished(message)).expect("encodes");
    let value: Value = serde_json::from_str(&json).expect("valid json");

    assert_uuid_v4_string(&value, "message_id");
    assert_uuid_v4_string(&value, "session_id");
}

#[test]
fn non_v4_uuid_in_protocol_id_field_is_rejected() {
    // A syntactically valid UUID (v1, time-based / namespace-based v5-style
    // shape doesn't matter — any non-v4 version byte) must be rejected.
    let non_v4 = "6ba7b810-9dad-11d1-80b4-00c04fd430c8"; // classic v1 example UUID
    let uuid = Uuid::parse_str(non_v4).unwrap();
    assert_ne!(uuid.get_version_num(), 4, "fixture must actually be non-v4");

    let json = format!(
        r#"{{"type":"AuthRequest","message_id":"{non_v4}","protocol_version":"1","timestamp":"2026-08-14T21:00:00Z","credential":"c"}}"#
    );
    let result = codec::decode(&json);
    assert!(result.is_err(), "non-v4 UUID message_id must be rejected");
}

// ---------------------------------------------------------------------
// ProtocolId input hardening: canonical lowercase-hyphenated UUIDv4 only.
// ---------------------------------------------------------------------

fn auth_request_json_with_message_id(message_id: &str) -> String {
    format!(
        r#"{{"type":"AuthRequest","message_id":"{message_id}","protocol_version":"1","timestamp":"2026-08-14T21:00:00Z","credential":"c"}}"#
    )
}

#[test]
fn canonical_lowercase_hyphenated_uuidv4_still_decodes() {
    let id = ProtocolId::generate();
    let canonical = id.to_string();
    assert_eq!(canonical, canonical.to_lowercase());
    assert!(canonical.contains('-'));

    let json = auth_request_json_with_message_id(&canonical);
    let decoded = codec::decode(&json).expect("canonical UUIDv4 must decode");
    match decoded {
        AgentProtocolMessage::AuthRequest(m) => assert_eq!(m.envelope.message_id, id),
        other => panic!("wrong variant decoded: {other:?}"),
    }
}

#[test]
fn uppercase_representation_of_a_uuidv4_is_rejected() {
    let id = ProtocolId::generate();
    let uppercase = id.to_string().to_uppercase();
    // Sanity: this fixture must actually differ from the canonical form,
    // otherwise the test would not exercise the rejection path.
    assert_ne!(uppercase, id.to_string());

    let json = auth_request_json_with_message_id(&uppercase);
    let result = codec::decode(&json);
    assert!(
        result.is_err(),
        "uppercase UUIDv4 representation must be rejected, not silently normalized"
    );
}

#[test]
fn non_hyphenated_simple_representation_of_a_uuidv4_is_rejected() {
    let id = ProtocolId::generate();
    let simple = id.as_uuid().simple().to_string();
    assert!(!simple.contains('-'));

    let json = auth_request_json_with_message_id(&simple);
    let result = codec::decode(&json);
    assert!(
        result.is_err(),
        "non-hyphenated/simple UUIDv4 representation must be rejected"
    );
}

#[test]
fn canonical_non_v4_uuid_remains_rejected() {
    // Already-canonical (lowercase, hyphenated) but version 1, so only the
    // version check — not the canonical-form check — should be the reason
    // for rejection.
    let non_v4 = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";
    assert_eq!(non_v4, non_v4.to_lowercase());

    let json = auth_request_json_with_message_id(non_v4);
    let result = codec::decode(&json);
    assert!(
        result.is_err(),
        "a canonical but non-v4 UUID must still be rejected"
    );
}

#[test]
fn generated_ids_still_serialize_as_lowercase_hyphenated_uuidv4() {
    let message = AuthRequestMessage::new("cred").with_correlation_id(ProtocolId::generate());
    let json = codec::encode(&AgentProtocolMessage::AuthRequest(message)).expect("encodes");
    let value: Value = serde_json::from_str(&json).expect("valid json");

    assert_uuid_v4_string(&value, "message_id");
    assert_uuid_v4_string(&value, "correlation_id");
}

// ---------------------------------------------------------------------
// 9. Optional correlation_id
// ---------------------------------------------------------------------

#[test]
fn correlation_id_absent_is_omitted_present_is_exact_uuidv4() {
    let without = AuthRequestMessage::new("cred");
    let json_without = codec::encode(&AgentProtocolMessage::AuthRequest(without)).expect("encodes");
    let value_without: Value = serde_json::from_str(&json_without).expect("valid json");
    assert!(value_without.get("correlation_id").is_none());

    let correlation_id = ProtocolId::generate();
    let with = AuthRequestMessage::new("cred").with_correlation_id(correlation_id);
    let json_with = codec::encode(&AgentProtocolMessage::AuthRequest(with)).expect("encodes");
    let value_with: Value = serde_json::from_str(&json_with).expect("valid json");
    assert_uuid_v4_string(&value_with, "correlation_id");
    assert_eq!(value_with["correlation_id"], correlation_id.to_string());
}

// ---------------------------------------------------------------------
// 10. Unknown field inside a known message type is ignored
// ---------------------------------------------------------------------

#[test]
fn unknown_extra_field_inside_auth_request_is_ignored() {
    let envelope = Envelope::new();
    let json = format!(
        r#"{{"type":"AuthRequest","message_id":"{}","protocol_version":"1","timestamp":"{}","credential":"c","some_future_field":42}}"#,
        envelope.message_id,
        envelope.timestamp.as_datetime().to_rfc3339(),
    );
    let decoded = codec::decode(&json).expect("unknown field must not break decoding");
    match decoded {
        AgentProtocolMessage::AuthRequest(m) => assert_eq!(m.body.credential, "c"),
        other => panic!("wrong variant decoded: {other:?}"),
    }
}

// ---------------------------------------------------------------------
// 11. Unknown top-level type is rejected, not silently coerced
// ---------------------------------------------------------------------

#[test]
fn unknown_top_level_message_type_is_rejected() {
    let json = r#"{"type":"SomeFutureMessage","message_id":"3fa85f64-5717-4562-b3fc-2c963f66afa6","protocol_version":"1","timestamp":"2026-08-14T21:00:00Z"}"#;
    let result = codec::decode(json);
    assert!(
        result.is_err(),
        "an unknown top-level type must fail explicitly, not be silently interpreted"
    );
}

// ---------------------------------------------------------------------
// 12. Redacted Debug output for credential-bearing fields
// ---------------------------------------------------------------------

#[test]
fn auth_request_debug_output_does_not_leak_credential() {
    let message = AuthRequestMessage::new("super-secret-credential-value");
    let debug = format!("{message:?}");
    assert!(!debug.contains("super-secret-credential-value"));
    assert!(debug.contains("REDACTED"));
}

#[test]
fn session_established_debug_output_does_not_leak_runtime_credential() {
    let message = SessionEstablishedMessage::new(
        ProtocolId::generate(),
        "super-secret-runtime",
        Utc::now().into(),
    );
    let debug = format!("{message:?}");
    assert!(!debug.contains("super-secret-runtime"));
    assert!(debug.contains("REDACTED"));
}

#[test]
fn bootstrap_evidence_debug_output_does_not_leak_assertion() {
    let message = BootstrapEvidenceMessage::new("nonce", "super-secret-assertion-bytes");
    let debug = format!("{message:?}");
    assert!(!debug.contains("super-secret-assertion-bytes"));
    assert!(debug.contains("REDACTED"));
}

// ---------------------------------------------------------------------
// 13. Timestamps are JSON strings, never numeric epoch values
// ---------------------------------------------------------------------

#[test]
fn timestamps_are_json_strings_not_numbers() {
    let message = AuthRequestMessage::new("cred");
    let json = codec::encode(&AgentProtocolMessage::AuthRequest(message)).expect("encodes");
    let value: Value = serde_json::from_str(&json).expect("valid json");
    assert!(
        value["timestamp"].is_string(),
        "timestamp must be a string, got {value}"
    );

    let session = SessionEstablishedMessage::new(ProtocolId::generate(), "cred", Utc::now().into());
    let json = codec::encode(&AgentProtocolMessage::SessionEstablished(session)).expect("encodes");
    let value: Value = serde_json::from_str(&json).expect("valid json");
    assert!(
        value["credential_expires_at"].is_string(),
        "credential_expires_at must be a string, got {value}"
    );
}

#[test]
fn protocol_error_has_normative_flat_wire_shape_and_optional_correlation() {
    let correlation = ProtocolId::generate();
    let message =
        ProtocolErrorMessage::new("GENERIC", "protocol violation").with_correlation_id(correlation);
    let json = codec::encode(&AgentProtocolMessage::ProtocolError(message)).unwrap();
    let value: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["type"], "ProtocolError");
    assert_eq!(value["code"], "GENERIC");
    assert_eq!(value["message"], "protocol violation");
    assert_eq!(value["correlation_id"], correlation.to_string());
    assert!(matches!(
        codec::decode(&json).unwrap(),
        AgentProtocolMessage::ProtocolError(_)
    ));
}

#[test]
fn inventory_report_round_trips_as_an_opaque_json_object() {
    let inventory = serde_json::json!({
        "cpu": {"model": "simulated"},
        "disks": [{"bytes": 1024}],
    })
    .as_object()
    .unwrap()
    .clone();
    let wire = codec::encode(&AgentProtocolMessage::InventoryReport(
        InventoryReportMessage::new(inventory.clone()),
    ))
    .unwrap();
    let value: Value = serde_json::from_str(&wire).unwrap();
    assert_eq!(value["type"], "InventoryReport");
    assert_eq!(value["inventory"], Value::Object(inventory.clone()));

    let AgentProtocolMessage::InventoryReport(decoded) = codec::decode(&wire).unwrap() else {
        panic!("expected InventoryReport")
    };
    assert_eq!(decoded.body.inventory, inventory);
}

#[test]
fn inventory_report_ignores_unknown_inner_message_fields() {
    let envelope = Envelope::new();
    let json = format!(
        r#"{{"type":"InventoryReport","message_id":"{}","protocol_version":"1","timestamp":"{}","inventory":{{"cpu":"sim"}},"future_metadata":{{"sequence":7}}}}"#,
        envelope.message_id,
        envelope.timestamp.as_datetime().to_rfc3339(),
    );
    let AgentProtocolMessage::InventoryReport(decoded) = codec::decode(&json).unwrap() else {
        panic!("expected InventoryReport")
    };
    assert_eq!(decoded.body.inventory["cpu"], "sim");
}

#[test]
fn inventory_report_rejects_missing_or_non_object_inventory() {
    let envelope = Envelope::new();
    for body in ["", r#","inventory":[1,2,3]"#, r#","inventory":"opaque""#] {
        let json = format!(
            r#"{{"type":"InventoryReport","message_id":"{}","protocol_version":"1","timestamp":"{}"{body}}}"#,
            envelope.message_id,
            envelope.timestamp.as_datetime().to_rfc3339(),
        );
        assert!(codec::decode(&json).is_err(), "must reject: {json}");
    }
}
