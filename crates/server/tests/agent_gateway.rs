//! Gateway-semantics tests (Issue #17 WP1 handshake checkpoint):
//! `AgentControlGateway::handshake` driven directly over an in-memory
//! (no-TLS) established WebSocket, against the real PostgreSQL-backed
//! `EnrollmentService` (`support::TestDatabase`) — the same Application
//! stack `enrollment_lifecycle.rs` already exercises exhaustively at the
//! Domain/Application boundary. This file proves only the Gateway's own
//! wire-level behavior: decode/phase validation, generic `AuthError` policy,
//! protocol-version-before-redemption ordering, correlation, and
//! `SessionEstablished` construction.
//!
//! Real TLS/WSS transport composition (a valid handshake and a rejected
//! credential crossing the full TCP -> TLS 1.3 -> WebSocket -> Agent Protocol
//! boundary, plus the version-no-redeem proof) is covered separately by
//! `tests/agent_gateway_wss.rs`. `BootstrapEvidence` is used here only to
//! prove it is rejected as a wrong-phase message before `AuthRequest` — this
//! file does not process it.

mod support;

use std::sync::Arc;

use bamep_agent_protocol::{
    decode, encode, AgentProtocolMessage, AuthRequestMessage, BootstrapEvidenceMessage, Envelope,
    InventoryReportMessage, ProtocolErrorMessage, ProtocolId, ProtocolVersion,
    TransferAuthorizationRequestMessage,
};
use bamep_domain::presented_credential::{CredentialKind, PresentedCredential};
use bamep_domain::{ChunkSize, DigestAlgorithm, SourceProvenance, TransferDirection};
use bamep_server::adapters::agent_gateway::{
    AgentControlGateway, AgentGatewayError, HandshakeOutcome,
};
use bamep_server::adapters::postgres::{
    PostgresBootContextRepository, PostgresCredentialRedemptionRepository,
    PostgresEndpointRepository, PostgresJobRepository, PostgresTransferAuthorizationRepository,
    PostgresTransferRepository,
};
use bamep_server::application::{
    ApplicationError, BootOrchestrationService, BootstrapEvidenceService, Clock, EnrollmentService,
    RedeemResult, TransferAuthorizationService, TransferDispatchResult, TransferDispatchService,
    TransferService,
};
use bamep_server::runtime::capability_store::CapabilityStore;
use bamep_server::runtime::replay_cache::ReplayCache;
use bamep_server::runtime::resource_arbiter::{
    ResourceClaim, ResourceKind, TechnicalResourceArbiter,
};
use bamep_trusted_bootstrap::ServerCertFingerprint;
use bamep_trusted_bootstrap::{fixture::FixtureAssertionSigner, AcceptedSiteKeys};
use chrono::{DateTime, Duration, TimeZone, Utc};
use ed25519_dalek::SigningKey;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use sqlx::PgPool;
use support::{ManualClock, TestDatabase};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use uuid::Uuid;

type Enrollment =
    EnrollmentService<PostgresEndpointRepository, PostgresCredentialRedemptionRepository>;
type Gateway =
    AgentControlGateway<PostgresEndpointRepository, PostgresCredentialRedemptionRepository>;
type BootOrchestration = BootOrchestrationService<PostgresBootContextRepository>;

/// Truncated to millisecond precision, matching `MessageTimestamp`'s own
/// wire serialization precision (`bamep_agent_protocol::envelope`) — so a
/// full-precision `DateTime<Utc>` computed in the test compares equal to the
/// value that actually round-tripped over the wire.
fn truncate_to_millis(dt: DateTime<Utc>) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(dt.timestamp_millis()).unwrap()
}

fn build_services(
    pool: PgPool,
    clock: Arc<ManualClock>,
    credential_ttl: Duration,
) -> (BootOrchestration, Arc<Enrollment>) {
    let boot_repo = Arc::new(PostgresBootContextRepository::new(pool.clone()));
    let endpoint_repo = Arc::new(PostgresEndpointRepository::new(pool.clone()));
    let redemption_repo = Arc::new(PostgresCredentialRedemptionRepository::new(pool));
    let boot_orchestration = BootOrchestrationService::new(boot_repo, Duration::minutes(5));
    let enrollment =
        EnrollmentService::with_clock(endpoint_repo, redemption_repo, clock as Arc<dyn Clock>)
            .with_credential_ttl(credential_ttl);
    (boot_orchestration, Arc::new(enrollment))
}

async fn issue_e1(
    boot: &BootOrchestration,
    signal: &str,
    now: DateTime<Utc>,
) -> PresentedCredential {
    // This file proves Gateway wire-level semantics, not current-boot
    // persistence (see `enrollment_lifecycle.rs` for that) — a fresh,
    // discarded BootNonce is sufficient here.
    let boot_nonce =
        bamep_domain::BootNonce::generate().expect("OS CSPRNG must be available in tests");
    boot.issue_enrollment_credential(signal, boot_nonce, now)
        .await
        .expect("issuance must succeed")
}

async fn domain_event_count(pool: &PgPool, endpoint_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM domain_events WHERE endpoint_id = $1")
        .bind(endpoint_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn endpoint_id_for_signal(pool: &PgPool, signal: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM endpoints WHERE inventory_signal = $1")
        .bind(signal)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn total_endpoint_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM endpoints")
        .fetch_one(pool)
        .await
        .unwrap()
}

fn object(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    value.as_object().unwrap().clone()
}

/// A connected, no-TLS in-process WebSocket pair: a real WS Upgrade
/// handshake over an in-memory duplex pipe. Sufficient for handshake-phase
/// Gateway tests — TLS/WSS transport is a separate, already-proven boundary
/// (`src/adapters/agent_transport.rs`; `tests/agent_gateway_wss.rs`).
async fn websocket_pair() -> (
    WebSocketStream<tokio::io::DuplexStream>,
    WebSocketStream<tokio::io::DuplexStream>,
) {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let server_task =
        tokio::spawn(async move { tokio_tungstenite::accept_async(server_io).await.unwrap() });
    let (client_ws, _response) =
        tokio_tungstenite::client_async("ws://bamep-agent-test/", client_io)
            .await
            .expect("in-memory WebSocket Upgrade must succeed");
    let server_ws = server_task
        .await
        .expect("server accept task must not panic");
    (client_ws, server_ws)
}

async fn send_text(ws: &mut WebSocketStream<tokio::io::DuplexStream>, wire: String) {
    ws.send(Message::text(wire)).await.expect("send text frame");
}

async fn recv_message(ws: &mut WebSocketStream<tokio::io::DuplexStream>) -> AgentProtocolMessage {
    let frame = ws
        .next()
        .await
        .expect("a frame is present")
        .expect("frame read ok");
    decode(frame.into_text().expect("text frame").as_str()).expect("decode message")
}

#[tokio::test]
async fn authenticated_session_reports_wire_violations_but_keeps_invalid_evidence_silent() {
    let db = TestDatabase::setup().await;
    let clock = Arc::new(ManualClock::new(Utc::now()));
    let (boot, enrollment) =
        build_services(db.pool.clone(), Arc::clone(&clock), Duration::minutes(10));
    let signer = FixtureAssertionSigner::from_seed([9; 32]);
    let evidence = Arc::new(BootstrapEvidenceService::new(
        Arc::new(PostgresEndpointRepository::new(db.pool.clone())),
        AcceptedSiteKeys::single(signer.public_key()),
    ));
    let gateway =
        Arc::new(Gateway::new(Arc::clone(&enrollment)).with_bootstrap_evidence_service(evidence));
    let e1 = issue_e1(&boot, "gw-session-violations", clock.now()).await;
    let (mut client_ws, mut server_ws) = websocket_pair().await;
    let request = AuthRequestMessage::new(e1.to_wire_value());
    send_text(
        &mut client_ws,
        encode(&AgentProtocolMessage::AuthRequest(request)).unwrap(),
    )
    .await;
    let HandshakeOutcome::Established(session) = gateway.handshake(&mut server_ws).await.unwrap()
    else {
        panic!()
    };
    let _ = recv_message(&mut client_ws).await;
    let server_gateway = Arc::clone(&gateway);
    let task = tokio::spawn(async move {
        server_gateway
            .run_authenticated_session(
                &mut server_ws,
                session,
                ServerCertFingerprint::from_sha256_digest([1; 32]),
            )
            .await
    });

    // Wire-invalid ActionAck/ActionProgress shapes (Issue #26 correction
    // "Enforce the action wire contract on untrusted input"): these are
    // rejected by `bamep_agent_protocol`'s own `Deserialize` before the
    // Gateway ever matches on a known variant, exactly like the other
    // malformed/unknown frames this test already proves — a decode failure,
    // never a trustworthy correlation_id.
    let action_id = ProtocolId::generate();
    let envelope1 = Envelope::new();
    let malformed_action_ack = format!(
        r#"{{"type":"ActionAck","message_id":"{}","protocol_version":"1","timestamp":"{}","correlation_id":"{}","action_id":"{}","outcome":"Rejected"}}"#,
        envelope1.message_id,
        envelope1.timestamp.as_datetime().to_rfc3339(),
        action_id,
        action_id,
    );
    let envelope2 = Envelope::new();
    let malformed_action_progress = format!(
        r#"{{"type":"ActionProgress","message_id":"{}","protocol_version":"1","timestamp":"{}","correlation_id":"{}","action_id":"{}"}}"#,
        envelope2.message_id,
        envelope2.timestamp.as_datetime().to_rfc3339(),
        action_id,
        action_id,
    );

    for frame in [
        Message::text("{"),
        Message::text(
            r#"{"type":"Unknown","message_id":"3fa85f64-5717-4562-b3fc-2c963f66afa6","protocol_version":"1","timestamp":"2026-08-14T21:00:00Z"}"#,
        ),
        Message::binary(vec![1, 2, 3]),
        Message::text(
            encode(&AgentProtocolMessage::AuthRequest(AuthRequestMessage::new(
                "again",
            )))
            .unwrap(),
        ),
        Message::text(malformed_action_ack),
        Message::text(malformed_action_progress),
    ] {
        client_ws.send(frame).await.unwrap();
        let AgentProtocolMessage::ProtocolError(error) = recv_message(&mut client_ws).await else {
            panic!()
        };
        assert_eq!(error.body.code, "GENERIC");
        assert_eq!(error.body.message, "protocol violation");
    }

    let invalid = BootstrapEvidenceMessage::new("not-a-nonce", "not-an-assertion");
    send_text(
        &mut client_ws,
        encode(&AgentProtocolMessage::BootstrapEvidence(invalid)).unwrap(),
    )
    .await;
    send_text(&mut client_ws, "{".to_string()).await;
    assert!(matches!(recv_message(&mut client_ws).await, AgentProtocolMessage::ProtocolError(_)),
        "invalid evidence emitted no response and the following violation proves the session stayed alive");

    let inbound = ProtocolErrorMessage::new("GENERIC", "protocol violation");
    send_text(
        &mut client_ws,
        encode(&AgentProtocolMessage::ProtocolError(inbound)).unwrap(),
    )
    .await;
    client_ws.close(None).await.unwrap();
    task.await.unwrap().unwrap();
    db.teardown().await;
}

#[tokio::test]
async fn valid_auth_request_establishes_session() {
    let db = TestDatabase::setup().await;
    let clock = Arc::new(ManualClock::new(Utc::now()));
    let ttl = Duration::minutes(10);
    let (boot, enrollment) = build_services(db.pool.clone(), Arc::clone(&clock), ttl);
    let gateway = Gateway::new(Arc::clone(&enrollment));

    let now = clock.now();
    let expected_expiry = truncate_to_millis(now + ttl);
    let e1 = issue_e1(&boot, "gw-established-01", now).await;

    let (mut client_ws, mut server_ws) = websocket_pair().await;

    let auth_request = AuthRequestMessage::new(e1.to_wire_value());
    let request_message_id = auth_request.envelope.message_id;
    send_text(
        &mut client_ws,
        encode(&AgentProtocolMessage::AuthRequest(auth_request)).unwrap(),
    )
    .await;

    let outcome = gateway.handshake(&mut server_ws).await.unwrap();
    let HandshakeOutcome::Established(session) = outcome else {
        panic!("expected Established");
    };

    let response = recv_message(&mut client_ws).await;
    let AgentProtocolMessage::SessionEstablished(established) = response else {
        panic!("expected SessionEstablished, got {response:?}");
    };

    assert_eq!(
        established.envelope.correlation_id,
        Some(request_message_id),
        "correlation_id must equal the AuthRequest's own message_id"
    );
    assert_eq!(established.body.session_id, session.session_id);
    assert_eq!(
        session.session_id.as_uuid().get_version_num(),
        4,
        "session_id must be a fresh v4 UUID"
    );
    assert_eq!(
        truncate_to_millis(established.body.credential_expires_at.as_datetime()),
        expected_expiry,
        "credential_expires_at must be exactly the Application-returned expiry"
    );

    let endpoint_id = endpoint_id_for_signal(&db.pool, "gw-established-01").await;
    assert_eq!(session.endpoint_id.0, endpoint_id);
    assert_eq!(
        domain_event_count(&db.pool, endpoint_id).await,
        1,
        "exactly one redemption (first contact) must have durably committed"
    );

    // The delivered runtime_credential is exactly the Application-issued
    // value: only that exact credential can go on to authenticate.
    let reconnect_result = enrollment
        .redeem(&established.body.runtime_credential)
        .await
        .unwrap();
    assert!(matches!(reconnect_result, RedeemResult::Established { .. }));

    db.teardown().await;
}

#[tokio::test]
async fn rejected_credential_yields_generic_auth_error() {
    let db = TestDatabase::setup().await;
    let clock = Arc::new(ManualClock::new(Utc::now()));
    let (_boot, enrollment) =
        build_services(db.pool.clone(), Arc::clone(&clock), Duration::minutes(10));
    let gateway = Gateway::new(Arc::clone(&enrollment));

    let bogus = PresentedCredential::generate(CredentialKind::Enrollment);
    let (mut client_ws, mut server_ws) = websocket_pair().await;

    let auth_request = AuthRequestMessage::new(bogus.to_wire_value());
    let request_message_id = auth_request.envelope.message_id;
    send_text(
        &mut client_ws,
        encode(&AgentProtocolMessage::AuthRequest(auth_request)).unwrap(),
    )
    .await;

    let outcome = gateway.handshake(&mut server_ws).await.unwrap();
    assert!(matches!(outcome, HandshakeOutcome::Rejected));

    let response = recv_message(&mut client_ws).await;
    let AgentProtocolMessage::AuthError(error) = response else {
        panic!("expected AuthError, got {response:?}");
    };
    assert_eq!(error.body.reason, "rejected");
    assert_eq!(error.envelope.correlation_id, Some(request_message_id));
    assert_eq!(total_endpoint_count(&db.pool).await, 0);

    db.teardown().await;
}

#[tokio::test]
async fn incompatible_protocol_version_rejects_without_redeeming() {
    let db = TestDatabase::setup().await;
    let clock = Arc::new(ManualClock::new(Utc::now()));
    let (boot, enrollment) =
        build_services(db.pool.clone(), Arc::clone(&clock), Duration::minutes(10));
    let gateway = Gateway::new(Arc::clone(&enrollment));

    let e1 = issue_e1(&boot, "gw-version-01", clock.now()).await;
    let (mut client_ws, mut server_ws) = websocket_pair().await;

    let mut auth_request = AuthRequestMessage::new(e1.to_wire_value());
    auth_request.envelope.protocol_version = ProtocolVersion::new("2");
    let request_message_id = auth_request.envelope.message_id;
    send_text(
        &mut client_ws,
        encode(&AgentProtocolMessage::AuthRequest(auth_request)).unwrap(),
    )
    .await;

    let outcome = gateway.handshake(&mut server_ws).await.unwrap();
    assert!(matches!(outcome, HandshakeOutcome::Rejected));

    let response = recv_message(&mut client_ws).await;
    let AgentProtocolMessage::AuthError(error) = response else {
        panic!("expected AuthError, got {response:?}");
    };
    assert_eq!(error.body.reason, "rejected");
    assert_eq!(error.envelope.correlation_id, Some(request_message_id));

    // No credential transition: no Endpoint was ever created for this
    // inventory_signal.
    assert_eq!(total_endpoint_count(&db.pool).await, 0);

    // Strong proof: the SAME E1 still successfully establishes with v1 — an
    // incompatible-version attempt never consumed/rotated it.
    let (mut client_ws2, mut server_ws2) = websocket_pair().await;
    let retry_request = AuthRequestMessage::new(e1.to_wire_value());
    send_text(
        &mut client_ws2,
        encode(&AgentProtocolMessage::AuthRequest(retry_request)).unwrap(),
    )
    .await;
    let retry_outcome = gateway.handshake(&mut server_ws2).await.unwrap();
    assert!(
        matches!(retry_outcome, HandshakeOutcome::Established(_)),
        "E1 must still be fully valid for a genuine v1 AuthRequest"
    );

    db.teardown().await;
}

#[tokio::test]
async fn malformed_json_rejects_generically_without_correlation() {
    let db = TestDatabase::setup().await;
    let clock = Arc::new(ManualClock::new(Utc::now()));
    let (_boot, enrollment) =
        build_services(db.pool.clone(), Arc::clone(&clock), Duration::minutes(10));
    let gateway = Gateway::new(Arc::clone(&enrollment));

    let (mut client_ws, mut server_ws) = websocket_pair().await;
    send_text(&mut client_ws, "{ this is not valid JSON".to_string()).await;

    let outcome = gateway.handshake(&mut server_ws).await.unwrap();
    assert!(matches!(outcome, HandshakeOutcome::Rejected));

    let response = recv_message(&mut client_ws).await;
    let AgentProtocolMessage::AuthError(error) = response else {
        panic!("expected AuthError, got {response:?}");
    };
    assert_eq!(error.body.reason, "rejected");
    assert_eq!(
        error.envelope.correlation_id, None,
        "no trustworthy message_id can be recovered from malformed JSON"
    );
    assert_eq!(total_endpoint_count(&db.pool).await, 0);

    db.teardown().await;
}

#[tokio::test]
async fn unknown_message_type_rejects_generically() {
    let db = TestDatabase::setup().await;
    let clock = Arc::new(ManualClock::new(Utc::now()));
    let (_boot, enrollment) =
        build_services(db.pool.clone(), Arc::clone(&clock), Duration::minutes(10));
    let gateway = Gateway::new(Arc::clone(&enrollment));

    let (mut client_ws, mut server_ws) = websocket_pair().await;
    let envelope = Envelope::new();
    let wire = format!(
        r#"{{"type":"TotallyUnknownMessage","message_id":"{}","protocol_version":"1","timestamp":"{}"}}"#,
        envelope.message_id,
        envelope.timestamp.as_datetime().to_rfc3339(),
    );
    send_text(&mut client_ws, wire).await;

    let outcome = gateway.handshake(&mut server_ws).await.unwrap();
    assert!(matches!(outcome, HandshakeOutcome::Rejected));

    let response = recv_message(&mut client_ws).await;
    let AgentProtocolMessage::AuthError(error) = response else {
        panic!("expected AuthError, got {response:?}");
    };
    assert_eq!(error.body.reason, "rejected");
    assert_eq!(
        error.envelope.correlation_id, None,
        "an unknown type is a decode failure and never yields a trustworthy message_id"
    );
    assert_eq!(total_endpoint_count(&db.pool).await, 0);

    db.teardown().await;
}

#[tokio::test]
async fn bootstrap_evidence_before_auth_request_rejects_with_correlation() {
    let db = TestDatabase::setup().await;
    let clock = Arc::new(ManualClock::new(Utc::now()));
    let (_boot, enrollment) =
        build_services(db.pool.clone(), Arc::clone(&clock), Duration::minutes(10));
    let gateway = Gateway::new(Arc::clone(&enrollment));

    let (mut client_ws, mut server_ws) = websocket_pair().await;
    let evidence = BootstrapEvidenceMessage::new("boot-nonce-01", "opaque-assertion-bytes");
    let evidence_message_id = evidence.envelope.message_id;
    send_text(
        &mut client_ws,
        encode(&AgentProtocolMessage::BootstrapEvidence(evidence)).unwrap(),
    )
    .await;

    let outcome = gateway.handshake(&mut server_ws).await.unwrap();
    assert!(matches!(outcome, HandshakeOutcome::Rejected));

    let response = recv_message(&mut client_ws).await;
    let AgentProtocolMessage::AuthError(error) = response else {
        panic!("expected AuthError, got {response:?}");
    };
    assert_eq!(error.body.reason, "rejected");
    assert_eq!(error.envelope.correlation_id, Some(evidence_message_id));
    assert_eq!(total_endpoint_count(&db.pool).await, 0);

    db.teardown().await;
}

#[tokio::test]
async fn binary_frame_during_handshake_rejects() {
    let db = TestDatabase::setup().await;
    let clock = Arc::new(ManualClock::new(Utc::now()));
    let (_boot, enrollment) =
        build_services(db.pool.clone(), Arc::clone(&clock), Duration::minutes(10));
    let gateway = Gateway::new(Arc::clone(&enrollment));

    let (mut client_ws, mut server_ws) = websocket_pair().await;
    client_ws
        .send(Message::binary(vec![1u8, 2, 3]))
        .await
        .expect("send binary frame");

    let outcome = gateway.handshake(&mut server_ws).await.unwrap();
    assert!(matches!(outcome, HandshakeOutcome::Rejected));

    let response = recv_message(&mut client_ws).await;
    let AgentProtocolMessage::AuthError(error) = response else {
        panic!("expected AuthError, got {response:?}");
    };
    assert_eq!(error.body.reason, "rejected");
    assert_eq!(error.envelope.correlation_id, None);
    assert_eq!(total_endpoint_count(&db.pool).await, 0);

    db.teardown().await;
}

/// Proves whole-transaction rollback surfaces as a genuine `AgentGatewayError`
/// rather than being reinterpreted as a credential rejection — the same
/// test-local trigger technique `enrollment_lifecycle.rs`'s
/// `first_contact_rolls_back_entirely_when_lookup_projection_fails` uses,
/// applied here at the Gateway boundary.
#[tokio::test]
async fn repository_failure_surfaces_as_gateway_error_not_auth_error() {
    let db = TestDatabase::setup().await;
    let clock = Arc::new(ManualClock::new(Utc::now()));
    let (boot, enrollment) =
        build_services(db.pool.clone(), Arc::clone(&clock), Duration::minutes(10));
    let gateway = Gateway::new(Arc::clone(&enrollment));

    let e1 = issue_e1(&boot, "gw-repo-failure-01", clock.now()).await;

    sqlx::query(
        "CREATE FUNCTION fail_lookup_insert() RETURNS trigger AS $$
         BEGIN
             RAISE EXCEPTION 'test-induced failure: lookup insert must never become durable';
         END;
         $$ LANGUAGE plpgsql",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_lookup_insert_trigger \
         BEFORE INSERT ON endpoint_credential_lookups \
         FOR EACH ROW EXECUTE FUNCTION fail_lookup_insert()",
    )
    .execute(&db.pool)
    .await
    .unwrap();

    let (mut client_ws, mut server_ws) = websocket_pair().await;
    let auth_request = AuthRequestMessage::new(e1.to_wire_value());
    send_text(
        &mut client_ws,
        encode(&AgentProtocolMessage::AuthRequest(auth_request)).unwrap(),
    )
    .await;

    let result = gateway.handshake(&mut server_ws).await;
    assert!(
        matches!(
            result,
            Err(AgentGatewayError::Application(
                ApplicationError::Repository(_)
            ))
        ),
        "expected an AgentGatewayError wrapping a repository failure, got {result:?}"
    );
    assert_eq!(
        total_endpoint_count(&db.pool).await,
        0,
        "a failed transaction must not leave a partially committed Endpoint"
    );

    // Neither SessionEstablished nor AuthError was ever sent for this
    // attempt.
    let no_response =
        tokio::time::timeout(std::time::Duration::from_millis(200), client_ws.next()).await;
    assert!(
        no_response.is_err(),
        "no handshake response frame must be sent when redeem fails with a repository error"
    );

    db.teardown().await;
}

/// Runtime Presence Registry integration (Issue #30): the real
/// `AgentControlGateway` registers presence only once its authenticated
/// session loop is active, and reliably removes it on close — using the same
/// in-memory duplex harness as the rest of this file, since presence
/// semantics do not depend on real TLS/WSS transport.
#[tokio::test]
async fn authenticated_session_registers_presence_while_active_and_unregisters_on_close() {
    let db = TestDatabase::setup().await;
    let clock = Arc::new(ManualClock::new(Utc::now()));
    let (boot, enrollment) =
        build_services(db.pool.clone(), Arc::clone(&clock), Duration::minutes(10));
    let signer = FixtureAssertionSigner::from_seed([31; 32]);
    let evidence = Arc::new(BootstrapEvidenceService::new(
        Arc::new(PostgresEndpointRepository::new(db.pool.clone())),
        AcceptedSiteKeys::single(signer.public_key()),
    ));
    let gateway =
        Arc::new(Gateway::new(Arc::clone(&enrollment)).with_bootstrap_evidence_service(evidence));

    let e1 = issue_e1(&boot, "presence-basic-01", clock.now()).await;
    let (mut client_ws, mut server_ws) = websocket_pair().await;
    send_text(
        &mut client_ws,
        encode(&AgentProtocolMessage::AuthRequest(AuthRequestMessage::new(
            e1.to_wire_value(),
        )))
        .unwrap(),
    )
    .await;
    let HandshakeOutcome::Established(session) = gateway.handshake(&mut server_ws).await.unwrap()
    else {
        panic!("expected a valid AuthRequest to establish")
    };
    let endpoint_id = session.endpoint_id;
    let _ = recv_message(&mut client_ws).await; // consume SessionEstablished

    assert!(
        !gateway.presence().is_present(endpoint_id),
        "presence must not register merely from a completed handshake"
    );

    let gw = Arc::clone(&gateway);
    let task = tokio::spawn(async move {
        gw.run_authenticated_session(
            &mut server_ws,
            session,
            ServerCertFingerprint::from_sha256_digest([1; 32]),
        )
        .await
    });

    // Synchronize on a real response so presence is observed only once the
    // session loop has provably started (registration happens-before the
    // loop that can produce this response).
    send_text(&mut client_ws, "{".to_string()).await;
    assert!(matches!(
        recv_message(&mut client_ws).await,
        AgentProtocolMessage::ProtocolError(_)
    ));
    assert!(
        gateway.presence().is_present(endpoint_id),
        "the Endpoint must be present while its authenticated session loop is active"
    );

    client_ws.close(None).await.unwrap();
    task.await.unwrap().unwrap();
    assert!(
        !gateway.presence().is_present(endpoint_id),
        "closing the only session must remove presence"
    );

    db.teardown().await;
}

/// One Endpoint may have multiple simultaneous authenticated sessions
/// (obtained here the same way `enrollment_lifecycle.rs`'s
/// `predecessor_retry_after_unconfirmed_successor_supersedes_and_reissues`
/// proves is legal: E1 remains a valid, unconfirmed predecessor and may be
/// redeemed more than once, each redemption minting its own fresh successor
/// and — through the Gateway — its own fresh `SessionId`). Registering S2
/// must not erase S1, and the Endpoint becomes absent only once both close.
#[tokio::test]
async fn presence_tracks_multiple_concurrent_sessions_for_one_endpoint() {
    let db = TestDatabase::setup().await;
    let clock = Arc::new(ManualClock::new(Utc::now()));
    let (boot, enrollment) =
        build_services(db.pool.clone(), Arc::clone(&clock), Duration::minutes(10));
    let signer = FixtureAssertionSigner::from_seed([32; 32]);
    let evidence = Arc::new(BootstrapEvidenceService::new(
        Arc::new(PostgresEndpointRepository::new(db.pool.clone())),
        AcceptedSiteKeys::single(signer.public_key()),
    ));
    let gateway =
        Arc::new(Gateway::new(Arc::clone(&enrollment)).with_bootstrap_evidence_service(evidence));

    let e1 = issue_e1(&boot, "presence-multi-01", clock.now()).await;

    async fn establish(
        gateway: &Gateway,
        wire: &str,
    ) -> (
        WebSocketStream<tokio::io::DuplexStream>,
        WebSocketStream<tokio::io::DuplexStream>,
        bamep_server::adapters::agent_gateway::AuthenticatedSession,
    ) {
        let (mut client_ws, mut server_ws) = websocket_pair().await;
        send_text(
            &mut client_ws,
            encode(&AgentProtocolMessage::AuthRequest(AuthRequestMessage::new(
                wire,
            )))
            .unwrap(),
        )
        .await;
        let HandshakeOutcome::Established(session) =
            gateway.handshake(&mut server_ws).await.unwrap()
        else {
            panic!("expected a valid AuthRequest to establish")
        };
        let _ = recv_message(&mut client_ws).await;
        (client_ws, server_ws, session)
    }

    let (mut client1, server1, session1) = establish(&gateway, &e1.to_wire_value()).await;
    let (mut client2, server2, session2) = establish(&gateway, &e1.to_wire_value()).await;
    assert_eq!(session1.endpoint_id, session2.endpoint_id);
    assert_ne!(session1.session_id, session2.session_id);
    let endpoint_id = session1.endpoint_id;

    let gw1 = Arc::clone(&gateway);
    let mut server1 = server1;
    let task1 = tokio::spawn(async move {
        gw1.run_authenticated_session(
            &mut server1,
            session1,
            ServerCertFingerprint::from_sha256_digest([2; 32]),
        )
        .await
    });
    send_text(&mut client1, "{".to_string()).await;
    assert!(matches!(
        recv_message(&mut client1).await,
        AgentProtocolMessage::ProtocolError(_)
    ));

    let gw2 = Arc::clone(&gateway);
    let mut server2 = server2;
    let task2 = tokio::spawn(async move {
        gw2.run_authenticated_session(
            &mut server2,
            session2,
            ServerCertFingerprint::from_sha256_digest([3; 32]),
        )
        .await
    });
    send_text(&mut client2, "{".to_string()).await;
    assert!(matches!(
        recv_message(&mut client2).await,
        AgentProtocolMessage::ProtocolError(_)
    ));

    assert!(gateway.presence().is_present(endpoint_id));

    client1.close(None).await.unwrap();
    task1.await.unwrap().unwrap();
    assert!(
        gateway.presence().is_present(endpoint_id),
        "S2 remains active — S1 closing must not erase it"
    );

    client2.close(None).await.unwrap();
    task2.await.unwrap().unwrap();
    assert!(
        !gateway.presence().is_present(endpoint_id),
        "only removal of the last session makes the Endpoint absent"
    );

    db.teardown().await;
}

/// A rejected `AuthRequest` must never register presence — proven against a
/// known, already-existing Endpoint whose own session loop is never run in
/// this test, isolating the assertion to the effect of the rejected attempt
/// alone.
#[tokio::test]
async fn rejected_auth_request_never_registers_presence() {
    let db = TestDatabase::setup().await;
    let clock = Arc::new(ManualClock::new(Utc::now()));
    let (boot, enrollment) =
        build_services(db.pool.clone(), Arc::clone(&clock), Duration::minutes(10));
    let gateway = Arc::new(Gateway::new(Arc::clone(&enrollment)));

    let e1 = issue_e1(&boot, "presence-rejected-01", clock.now()).await;
    let (mut setup_client, mut setup_server) = websocket_pair().await;
    send_text(
        &mut setup_client,
        encode(&AgentProtocolMessage::AuthRequest(AuthRequestMessage::new(
            e1.to_wire_value(),
        )))
        .unwrap(),
    )
    .await;
    let HandshakeOutcome::Established(session) =
        gateway.handshake(&mut setup_server).await.unwrap()
    else {
        panic!("expected a valid AuthRequest to establish")
    };
    let endpoint_id = session.endpoint_id;

    let (mut client_ws, mut server_ws) = websocket_pair().await;
    send_text(
        &mut client_ws,
        encode(&AgentProtocolMessage::AuthRequest(AuthRequestMessage::new(
            "not-a-valid-credential-wire-value",
        )))
        .unwrap(),
    )
    .await;
    let outcome = gateway.handshake(&mut server_ws).await.unwrap();
    assert!(matches!(outcome, HandshakeOutcome::Rejected));

    assert!(
        !gateway.presence().is_present(endpoint_id),
        "a rejected AuthRequest must never register presence for any Endpoint"
    );

    db.teardown().await;
}

/// Cleanup must be reliable on an error exit path too, not only the normal
/// close path: forces `AgentGatewayError::InventoryServiceNotConfigured` by
/// establishing a session on a Gateway with no `InventoryService` configured,
/// then sending a well-formed `InventoryReport`.
#[tokio::test]
async fn session_loop_error_path_still_removes_presence() {
    let db = TestDatabase::setup().await;
    let clock = Arc::new(ManualClock::new(Utc::now()));
    let (boot, enrollment) =
        build_services(db.pool.clone(), Arc::clone(&clock), Duration::minutes(10));
    let signer = FixtureAssertionSigner::from_seed([33; 32]);
    let evidence = Arc::new(BootstrapEvidenceService::new(
        Arc::new(PostgresEndpointRepository::new(db.pool.clone())),
        AcceptedSiteKeys::single(signer.public_key()),
    ));
    // Deliberately no `.with_inventory_service(...)`.
    let gateway =
        Arc::new(Gateway::new(Arc::clone(&enrollment)).with_bootstrap_evidence_service(evidence));

    let e1 = issue_e1(&boot, "presence-error-path-01", clock.now()).await;
    let (mut client_ws, mut server_ws) = websocket_pair().await;
    send_text(
        &mut client_ws,
        encode(&AgentProtocolMessage::AuthRequest(AuthRequestMessage::new(
            e1.to_wire_value(),
        )))
        .unwrap(),
    )
    .await;
    let HandshakeOutcome::Established(session) = gateway.handshake(&mut server_ws).await.unwrap()
    else {
        panic!("expected a valid AuthRequest to establish")
    };
    let endpoint_id = session.endpoint_id;
    let _ = recv_message(&mut client_ws).await;

    let gw = Arc::clone(&gateway);
    let task = tokio::spawn(async move {
        gw.run_authenticated_session(
            &mut server_ws,
            session,
            ServerCertFingerprint::from_sha256_digest([4; 32]),
        )
        .await
    });

    let report = InventoryReportMessage::new(object(json!({"cpu": "sim"})));
    send_text(
        &mut client_ws,
        encode(&AgentProtocolMessage::InventoryReport(report)).unwrap(),
    )
    .await;

    let result = task.await.unwrap();
    assert!(matches!(
        result,
        Err(AgentGatewayError::InventoryServiceNotConfigured)
    ));
    assert!(
        !gateway.presence().is_present(endpoint_id),
        "cleanup on an error exit path must still remove presence"
    );

    db.teardown().await;
}

// ---------------------------------------------------------------------
// TransferAuthorizationRequest (Issue #38 "Agent WSS integration")
// ---------------------------------------------------------------------

const DATA_PLANE_BASE_URL: &str = "https://server.example:8443";

struct DispatchedTransfer {
    transfer_id: ProtocolId,
    action_id: ProtocolId,
}

/// Enrolled+approved Endpoint (via the already-authenticated `endpoint_id`),
/// `Running` Job, `PreconditionsSatisfied` JobStep, pre-dispatch Transfer,
/// then a committed `Attempt{Dispatched}` bound to it — mirroring
/// `transfer_authorization_service.rs`'s identical fixture, reused here to
/// prove the same Application boundary through the real Agent WSS Gateway.
async fn dispatched_transfer_fixture(
    pool: PgPool,
    endpoint_id: bamep_domain::EndpointId,
) -> DispatchedTransfer {
    let job_repo = Arc::new(PostgresJobRepository::new(pool.clone()));
    let jobs = bamep_server::application::JobService::new(Arc::clone(&job_repo));
    let scheduling = bamep_server::application::JobSchedulingService::new(Arc::clone(&job_repo));
    let transfers = TransferService::new(Arc::new(PostgresTransferRepository::new(pool.clone())));
    let arbiter = Arc::new(TechnicalResourceArbiter::new([(
        ResourceKind::new("network"),
        10,
    )]));
    let dispatch = TransferDispatchService::new(Arc::clone(&job_repo), arbiter);

    let job = jobs.create_workflow(endpoint_id, 1).await.unwrap();
    let step_id = job.steps[0].id;
    scheduling.admit(job.id).await.unwrap();
    scheduling
        .satisfy_current_step_preconditions(job.id, step_id)
        .await
        .unwrap();
    let context = transfers
        .create_transfer_context(
            endpoint_id,
            job.id,
            step_id,
            TransferDirection::AgentToServer,
            DigestAlgorithm::Sha256,
            ChunkSize::new(4096).unwrap(),
            SourceProvenance::new("disk-0"),
        )
        .await
        .unwrap();
    let result = dispatch
        .commit_transfer_dispatch(
            job.id,
            step_id,
            context.transfer.id,
            vec![ResourceClaim::new(ResourceKind::new("network"), 1)],
        )
        .await
        .unwrap();
    let TransferDispatchResult::Committed { outcome, .. } = result else {
        panic!("expected a successful dispatch commitment");
    };
    let action_id = ProtocolId::from_uuid(outcome.attempt.action_id.0)
        .expect("a Domain ActionId is always a valid UUID v4");
    let transfer_id =
        ProtocolId::from_uuid(context.transfer.id.0).expect("TransferId is always a UUID v4");

    DispatchedTransfer {
        transfer_id,
        action_id,
    }
}

fn generate_proof_public_key_wire() -> String {
    let signing_key = SigningKey::from_bytes(&rand::random());
    bamep_domain::ProofPublicKey::from_bytes(signing_key.verifying_key().to_bytes())
        .unwrap()
        .to_wire_value()
}

fn build_transfer_authorization_service(pool: PgPool) -> Arc<TransferAuthorizationService> {
    Arc::new(TransferAuthorizationService::new(
        Arc::new(PostgresTransferAuthorizationRepository::new(pool)),
        Arc::new(CapabilityStore::new()),
        Arc::new(ReplayCache::new()),
        DATA_PLANE_BASE_URL,
    ))
}

/// Establishes a real authenticated session (real handshake over the
/// in-memory WSS pair) and returns it alongside the client/server socket
/// halves and the durable `endpoint_id`, ready for `run_authenticated_session`.
async fn established_session(
    enrollment: &Arc<Enrollment>,
    boot: &BootOrchestration,
    gateway: &Gateway,
    signal: &str,
    now: DateTime<Utc>,
) -> (
    WebSocketStream<tokio::io::DuplexStream>,
    WebSocketStream<tokio::io::DuplexStream>,
    bamep_server::adapters::agent_gateway::AuthenticatedSession,
) {
    let e1 = issue_e1(boot, signal, now).await;
    let (mut client_ws, mut server_ws) = websocket_pair().await;
    let auth_request = AuthRequestMessage::new(e1.to_wire_value());
    send_text(
        &mut client_ws,
        encode(&AgentProtocolMessage::AuthRequest(auth_request)).unwrap(),
    )
    .await;
    let HandshakeOutcome::Established(session) = gateway.handshake(&mut server_ws).await.unwrap()
    else {
        panic!("expected Established");
    };
    let _ = recv_message(&mut client_ws).await; // SessionEstablished
    enrollment
        .approve_enrollment(
            session.endpoint_id,
            bamep_domain::Actor::Operator {
                label: "agent-gateway-transfer-authorization-harness".into(),
            },
            now,
        )
        .await
        .unwrap();
    (client_ws, server_ws, session)
}

#[tokio::test]
async fn transfer_authorization_request_over_authenticated_session_grants_a_capability() {
    let db = TestDatabase::setup().await;
    let clock = Arc::new(ManualClock::new(Utc::now()));
    let (boot, enrollment) =
        build_services(db.pool.clone(), Arc::clone(&clock), Duration::minutes(10));
    let transfer_authorization = build_transfer_authorization_service(db.pool.clone());
    let signer = FixtureAssertionSigner::from_seed([11; 32]);
    let bootstrap_evidence = Arc::new(BootstrapEvidenceService::new(
        Arc::new(PostgresEndpointRepository::new(db.pool.clone())),
        AcceptedSiteKeys::single(signer.public_key()),
    ));
    let gateway = Arc::new(
        Gateway::new(Arc::clone(&enrollment))
            .with_bootstrap_evidence_service(bootstrap_evidence)
            .with_transfer_authorization_service(Arc::clone(&transfer_authorization)),
    );

    let (mut client_ws, mut server_ws, session) = established_session(
        &enrollment,
        &boot,
        &gateway,
        "gw-transfer-auth-01",
        clock.now(),
    )
    .await;
    let fixture = dispatched_transfer_fixture(db.pool.clone(), session.endpoint_id).await;

    let server_gateway = Arc::clone(&gateway);
    let task = tokio::spawn(async move {
        server_gateway
            .run_authenticated_session(
                &mut server_ws,
                session,
                ServerCertFingerprint::from_sha256_digest([1; 32]),
            )
            .await
    });

    let request = TransferAuthorizationRequestMessage::new(
        fixture.action_id,
        fixture.transfer_id,
        generate_proof_public_key_wire(),
    );
    send_text(
        &mut client_ws,
        encode(&AgentProtocolMessage::TransferAuthorizationRequest(request)).unwrap(),
    )
    .await;

    let response = recv_message(&mut client_ws).await;
    let AgentProtocolMessage::TransferAuthorizationGrant(grant) = response else {
        panic!("expected TransferAuthorizationGrant, got {response:?}");
    };
    assert_eq!(grant.envelope.correlation_id, Some(fixture.action_id));
    assert_eq!(grant.body.transfer_id, fixture.transfer_id);
    assert_eq!(grant.body.data_plane_base_url, DATA_PLANE_BASE_URL);
    assert!(!grant.body.token.is_empty());

    client_ws.close(None).await.unwrap();
    task.await.unwrap().unwrap();
    db.teardown().await;
}

#[tokio::test]
async fn transfer_authorization_request_with_wrong_correlation_is_generically_denied() {
    let db = TestDatabase::setup().await;
    let clock = Arc::new(ManualClock::new(Utc::now()));
    let (boot, enrollment) =
        build_services(db.pool.clone(), Arc::clone(&clock), Duration::minutes(10));
    let transfer_authorization = build_transfer_authorization_service(db.pool.clone());
    let signer = FixtureAssertionSigner::from_seed([12; 32]);
    let bootstrap_evidence = Arc::new(BootstrapEvidenceService::new(
        Arc::new(PostgresEndpointRepository::new(db.pool.clone())),
        AcceptedSiteKeys::single(signer.public_key()),
    ));
    let gateway = Arc::new(
        Gateway::new(Arc::clone(&enrollment))
            .with_bootstrap_evidence_service(bootstrap_evidence)
            .with_transfer_authorization_service(Arc::clone(&transfer_authorization)),
    );

    let (mut client_ws, mut server_ws, session) = established_session(
        &enrollment,
        &boot,
        &gateway,
        "gw-transfer-auth-wrong-01",
        clock.now(),
    )
    .await;
    let fixture = dispatched_transfer_fixture(db.pool.clone(), session.endpoint_id).await;

    let server_gateway = Arc::clone(&gateway);
    let task = tokio::spawn(async move {
        server_gateway
            .run_authenticated_session(
                &mut server_ws,
                session,
                ServerCertFingerprint::from_sha256_digest([1; 32]),
            )
            .await
    });

    // Wrong correlation_id: a syntactically valid but unrelated action_id.
    let request = TransferAuthorizationRequestMessage::new(
        ProtocolId::generate(),
        fixture.transfer_id,
        generate_proof_public_key_wire(),
    );
    send_text(
        &mut client_ws,
        encode(&AgentProtocolMessage::TransferAuthorizationRequest(request)).unwrap(),
    )
    .await;

    let response = recv_message(&mut client_ws).await;
    let AgentProtocolMessage::TransferAuthorizationDenied(denied) = response else {
        panic!("expected TransferAuthorizationDenied, got {response:?}");
    };
    assert_eq!(denied.body.transfer_id, fixture.transfer_id);
    assert_eq!(
        denied.body.reason, "denied",
        "the single closed generic V1 denial reason"
    );

    client_ws.close(None).await.unwrap();
    task.await.unwrap().unwrap();
    db.teardown().await;
}
