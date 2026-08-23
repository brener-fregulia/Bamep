//! RF-003 Simulator acceptance through real TCP -> TLS 1.3 pin -> WSS ->
//! Agent Protocol session and real PostgreSQL persistence.

mod support;

use std::sync::Arc;

use bamep_agent_protocol::{encode, AgentProtocolMessage, InventoryReportMessage};
use bamep_domain::{BootNonce, InventorySnapshot};
use bamep_server::adapters::agent_gateway::{AgentControlGateway, HandshakeOutcome};
use bamep_server::adapters::agent_transport::AgentTransportAcceptor;
use bamep_server::adapters::postgres::{
    PostgresBootContextRepository, PostgresCredentialRedemptionRepository,
    PostgresEndpointRepository, PostgresInventoryRepository,
};
use bamep_server::application::{
    BootOrchestrationService, BootstrapEvidenceService, EnrollmentService, InventoryService,
    RedeemResult,
};
use bamep_server::ports::InventoryRepository;
use bamep_simulator::{
    authenticate, connect_pinned_wss, send_inventory_report, SimulatorHandshakeOutcome,
};
use bamep_trusted_bootstrap::{AcceptedSiteKeys, ServerCertFingerprint};
use chrono::{Duration, Utc};
use futures_util::{SinkExt, StreamExt};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde_json::{json, Map, Value};
use support::TestDatabase;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

type Gateway =
    AgentControlGateway<PostgresEndpointRepository, PostgresCredentialRedemptionRepository>;

fn object(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

/// The Endpoint dimensions that inventory reporting must never implicitly
/// change: identity/enrollment, credential state/chain, durable hardware
/// confidence, the authoritative current boot, and trusted-bootstrap state.
async fn endpoint_dimensions(
    pool: &sqlx::PgPool,
    endpoint_id: uuid::Uuid,
) -> (String, bool, String, Vec<u8>, String) {
    sqlx::query_as(
        "SELECT identity_state::text, c.revoked, e.hardware_confidence::text, e.current_boot_nonce, e.trusted_bootstrap_state::text \
         FROM endpoints e JOIN endpoint_credentials c ON c.endpoint_id = e.id WHERE e.id = $1",
    )
    .bind(endpoint_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn inventory_revision_current_pointer_and_event_roll_back_together() {
    let db = TestDatabase::setup().await;
    let boot = BootOrchestrationService::new(
        Arc::new(PostgresBootContextRepository::new(db.pool.clone())),
        Duration::minutes(5),
    );
    let enrollment = EnrollmentService::new(
        Arc::new(PostgresEndpointRepository::new(db.pool.clone())),
        Arc::new(PostgresCredentialRedemptionRepository::new(db.pool.clone())),
    );
    let credential = boot
        .issue_enrollment_credential(
            "inventory-atomicity",
            BootNonce::from_bytes([0x19; 32]),
            Utc::now(),
        )
        .await
        .unwrap();
    let RedeemResult::Established { endpoint_id, .. } = enrollment
        .redeem(&credential.to_wire_value())
        .await
        .unwrap()
    else {
        panic!("endpoint must establish")
    };

    sqlx::query(
        "CREATE FUNCTION reject_inventory_event() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN IF NEW.event_type = 'InventoryRevisionRecorded' THEN RAISE EXCEPTION 'forced event failure'; END IF; RETURN NEW; END $$"
    ).execute(&db.pool).await.unwrap();
    sqlx::query(
        "CREATE TRIGGER reject_inventory_event BEFORE INSERT ON domain_events \
         FOR EACH ROW EXECUTE FUNCTION reject_inventory_event()",
    )
    .execute(&db.pool)
    .await
    .unwrap();

    let repo = PostgresInventoryRepository::new(db.pool.clone());
    let result = repo
        .record_inventory(
            endpoint_id,
            InventorySnapshot(object(json!({"cpu":"sim"}))),
            Utc::now(),
        )
        .await;
    assert!(result.is_err());
    let revisions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM inventory_revisions WHERE endpoint_id=$1")
            .bind(endpoint_id.0)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let current: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT current_inventory_revision_id FROM endpoints WHERE id=$1")
            .bind(endpoint_id.0)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(revisions, 0);
    assert!(current.is_none());
    db.teardown().await;
}

fn certificate() -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    (
        cert.der().clone(),
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der())),
    )
}

#[tokio::test]
async fn inventory_on_change_and_phase_rejection_cross_real_wss_and_survive_reload() {
    let db = TestDatabase::setup().await;
    let endpoint_repo = Arc::new(PostgresEndpointRepository::new(db.pool.clone()));
    let redemption_repo = Arc::new(PostgresCredentialRedemptionRepository::new(db.pool.clone()));
    let boot = BootOrchestrationService::new(
        Arc::new(PostgresBootContextRepository::new(db.pool.clone())),
        Duration::minutes(5),
    );
    let enrollment = Arc::new(EnrollmentService::new(
        endpoint_repo.clone(),
        redemption_repo,
    ));
    let inventory_repo = Arc::new(PostgresInventoryRepository::new(db.pool.clone()));
    let inventory_service = Arc::new(InventoryService::new(inventory_repo.clone()));
    let fixture_key = bamep_simulator::TrustedBootstrapFixtureIssuer::from_seed([0x18; 32]);
    let evidence = Arc::new(BootstrapEvidenceService::new(
        endpoint_repo,
        AcceptedSiteKeys::single(fixture_key.public_key()),
    ));
    let gateway = Arc::new(
        Gateway::new(enrollment)
            .with_bootstrap_evidence_service(evidence)
            .with_inventory_service(inventory_service),
    );

    let credential = boot
        .issue_enrollment_credential(
            "inventory-wss-endpoint",
            BootNonce::from_bytes([0x18; 32]),
            Utc::now(),
        )
        .await
        .unwrap();
    let (cert_der, key_der) = certificate();
    let fingerprint = ServerCertFingerprint::from_leaf_der(cert_der.as_ref());
    let acceptor = Arc::new(AgentTransportAcceptor::new(vec![cert_der], key_der).unwrap());
    let listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let addr = listener.local_addr().unwrap();

    let server = {
        let gateway = gateway.clone();
        let acceptor = acceptor.clone();
        let listener = listener.clone();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut connection = acceptor.accept(tcp).await.unwrap();
            let HandshakeOutcome::Established(session) =
                gateway.handshake(&mut connection.websocket).await.unwrap()
            else {
                panic!("session must establish")
            };
            gateway
                .run_authenticated_session(
                    &mut connection.websocket,
                    session,
                    connection.server_fingerprint,
                )
                .await
                .unwrap();
            session
        })
    };
    let mut client = connect_pinned_wss(addr, "localhost", fingerprint)
        .await
        .unwrap();
    let SimulatorHandshakeOutcome::Established(_) =
        authenticate(&mut client, &credential.to_wire_value())
            .await
            .unwrap()
    else {
        panic!("session must establish")
    };

    // Only one Endpoint exists in this database at this point.
    let endpoint_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM endpoints")
        .fetch_one(&db.pool)
        .await
        .unwrap();

    // Captured after session establishment but strictly before the first
    // InventoryReport: the baseline that inventory reporting itself must
    // never implicitly change.
    let dimensions_before = endpoint_dimensions(&db.pool, endpoint_id).await;
    assert_eq!(
        dimensions_before.2, "Consistent",
        "the newly created Endpoint must already carry its Consistent hardware-confidence baseline"
    );

    const MALFORMED_INVENTORY_REPORT: &str = r#"{"type":"InventoryReport","inventory":[]}"#;

    client
        .send(Message::text(MALFORMED_INVENTORY_REPORT))
        .await
        .unwrap();
    let malformed_response = client.next().await.unwrap().unwrap().into_text().unwrap();
    assert!(malformed_response.contains("ProtocolError"));
    assert_eq!(
        endpoint_dimensions(&db.pool, endpoint_id).await,
        dimensions_before
    );

    // Each valid report below is followed by a malformed report used purely
    // as a synchronization barrier: `run_authenticated_session` processes
    // frames strictly in order on this one connection, so the barrier's
    // ProtocolError response can only arrive once the Server has finished
    // processing the preceding report — making the dimensions query that
    // follows guaranteed to observe post-report durable state.
    send_inventory_report(&mut client, object(json!({"cpu":"sim", "memory_bytes":8})))
        .await
        .unwrap();
    client
        .send(Message::text(MALFORMED_INVENTORY_REPORT))
        .await
        .unwrap();
    assert!(client
        .next()
        .await
        .unwrap()
        .unwrap()
        .into_text()
        .unwrap()
        .contains("ProtocolError"));
    assert_eq!(
        endpoint_dimensions(&db.pool, endpoint_id).await,
        dimensions_before,
        "the first observed InventoryReport must not change identity/credential/hardware-confidence/boot/trusted-bootstrap state"
    );

    send_inventory_report(&mut client, object(json!({"memory_bytes":8, "cpu":"sim"})))
        .await
        .unwrap();
    client
        .send(Message::text(MALFORMED_INVENTORY_REPORT))
        .await
        .unwrap();
    assert!(client
        .next()
        .await
        .unwrap()
        .unwrap()
        .into_text()
        .unwrap()
        .contains("ProtocolError"));
    assert_eq!(
        endpoint_dimensions(&db.pool, endpoint_id).await,
        dimensions_before,
        "an unchanged re-report must not change identity/credential/hardware-confidence/boot/trusted-bootstrap state"
    );

    send_inventory_report(&mut client, object(json!({"cpu":"sim", "memory_bytes":16})))
        .await
        .unwrap();
    client
        .send(Message::text(MALFORMED_INVENTORY_REPORT))
        .await
        .unwrap();
    assert!(client
        .next()
        .await
        .unwrap()
        .unwrap()
        .into_text()
        .unwrap()
        .contains("ProtocolError"));
    assert_eq!(
        endpoint_dimensions(&db.pool, endpoint_id).await,
        dimensions_before,
        "a meaningfully changed inventory report must not change identity/credential/hardware-confidence/boot/trusted-bootstrap state"
    );

    client.close(None).await.unwrap();
    let session = server.await.unwrap();
    assert_eq!(session.endpoint_id.0, endpoint_id);

    let revision_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM inventory_revisions WHERE endpoint_id = $1")
            .bind(session.endpoint_id.0)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let event_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM domain_events WHERE endpoint_id = $1 AND event_type = 'InventoryRevisionRecorded'")
        .bind(session.endpoint_id.0).fetch_one(&db.pool).await.unwrap();
    assert_eq!((revision_count, event_count), (2, 2));

    // A known InventoryReport in the handshake phase is rejected as AuthError
    // and cannot reach the inventory service.
    let phase_server = {
        let gateway = gateway.clone();
        let acceptor = acceptor.clone();
        let listener = listener.clone();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut connection = acceptor.accept(tcp).await.unwrap();
            gateway.handshake(&mut connection.websocket).await.unwrap()
        })
    };
    let mut phase_client = connect_pinned_wss(addr, "localhost", fingerprint)
        .await
        .unwrap();
    let report = InventoryReportMessage::new(object(json!({"phase":"invalid"})));
    phase_client
        .send(Message::text(
            encode(&AgentProtocolMessage::InventoryReport(report)).unwrap(),
        ))
        .await
        .unwrap();
    assert!(phase_client.next().await.unwrap().unwrap().is_text());
    assert!(matches!(
        phase_server.await.unwrap(),
        HandshakeOutcome::Rejected
    ));
    let after_invalid: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM inventory_revisions WHERE endpoint_id = $1")
            .bind(session.endpoint_id.0)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(after_invalid, 2);

    db.pool.close().await;
    let reloaded_pool = bamep_server::adapters::postgres::connect(&db.db_url)
        .await
        .unwrap();
    let reloaded_repo = PostgresInventoryRepository::new(reloaded_pool.clone());
    let current = reloaded_repo
        .find_current_inventory(session.endpoint_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.snapshot.0["memory_bytes"], 16);
    let dimensions_after_reload = endpoint_dimensions(&reloaded_pool, endpoint_id).await;
    assert_eq!(
        dimensions_after_reload, dimensions_before,
        "PostgreSQL reload must preserve identity/credential/hardware-confidence/boot/trusted-bootstrap state independent from inventory persistence"
    );
    reloaded_pool.close().await;
    db.teardown().await;
}
