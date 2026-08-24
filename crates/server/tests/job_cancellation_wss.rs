//! Issue #27 "[WP] Execute Job cancellation end to end": real loopback TCP ->
//! pinned TLS 1.3 -> WebSocket -> Agent Protocol v1 integration proving
//! cancellation crosses the real WSS/Agent Control Gateway boundary end to
//! end (`m0-simulator-contract-and-validation-strategy.md` "Fidelity
//! boundary"):
//!
//! ```text
//! committed Attempt{Dispatched} (#25/#26)
//!   -> CancellationService::request durably commits Running -> Cancelling
//!   -> CancelAction{exact existing action_id} transmitted over the real
//!      OutboundSessionDirectory/AgentControlGateway session
//!   -> real WSS delivery to a Simulated Agent (SimulatedActionAgent)
//!   -> CancelAck back over the same session
//!   -> AgentControlGateway -> CancellationService::apply_cancel_ack -> PostgreSQL
//! ```
//!
//! Combines the required successful-cancellation, `CannotCancel`,
//! `AlreadyCompleted`-with-unknown-result, and `Unknown` scenarios across a
//! small number of real WSS fixtures rather than one fixture per scenario
//! (`m0-simulator-contract-and-validation-strategy.md`: "may be efficiently
//! combined"). Mirrors `action_dispatch_wss.rs`'s harness.
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

mod support;

use std::net::SocketAddr;
use std::sync::Arc;

use bamep_agent_protocol::{
    decode, encode, AgentProtocolMessage, CancelAckOutcome, InventoryReportMessage,
};
use bamep_domain::{Actor, Attempt, BootNonce, EndpointId, JobId, JobStepId, TargetFingerprint};
use bamep_server::adapters::agent_gateway::{AgentControlGateway, HandshakeOutcome};
use bamep_server::adapters::agent_transport::AgentTransportAcceptor;
use bamep_server::adapters::postgres::{
    PostgresBootContextRepository, PostgresCredentialRedemptionRepository,
    PostgresEndpointRepository, PostgresInventoryRepository, PostgresJobRepository,
};
use bamep_server::adapters::target_revalidation_fixture::FixtureTargetRevalidation;
use bamep_server::application::{
    ActionDispatchOutcome, ActionDispatchService, ActionEvidenceService, BootOrchestrationService,
    CancelActionSendOutcome, CancellationRequestResult, CancellationService,
    DestructiveIntentService, EnrollmentService, FinalDispatchResult, FinalDispatchService,
    InventoryService, JobSchedulingService, JobService, RedeemResult,
};
use bamep_server::ports::{
    AgentDispatchPort, InventoryRepository, JobRepository, TargetRevalidationPort,
};
use bamep_server::runtime::outbound_sessions::OutboundSessionDirectory;
use bamep_server::runtime::reservation_registry::AttemptReservationRegistry;
use bamep_server::runtime::resource_arbiter::{
    ResourceClaim, ResourceKind, TechnicalResourceArbiter,
};
use bamep_simulator::{
    connect_after_trusted_bootstrap, send_bootstrap_evidence, CancelBehavior, ScenarioOutcome,
    SimulatedActionAgent, SimulatedBootstrapMaterial, SimulatedPairedTrust,
    SimulatorHandshakeOutcome, TrustedBootstrapFixtureIssuer,
};
use bamep_trusted_bootstrap::ServerCertFingerprint;
use chrono::{Duration, Utc};
use futures_util::{SinkExt, StreamExt};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde_json::{json, Map, Value};
use sqlx::{PgPool, Row};
use support::TestDatabase;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_rustls::client::TlsStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

type Enrollment =
    EnrollmentService<PostgresEndpointRepository, PostgresCredentialRedemptionRepository>;
type Gateway =
    AgentControlGateway<PostgresEndpointRepository, PostgresCredentialRedemptionRepository>;
type BootOrchestration = BootOrchestrationService<PostgresBootContextRepository>;
type ClientWs = WebSocketStream<TlsStream<tokio::net::TcpStream>>;

fn object(value: Value) -> Map<String, Value> {
    value.as_object().unwrap().clone()
}

fn network_claims() -> Vec<ResourceClaim> {
    vec![ResourceClaim::new(ResourceKind::new("network"), 1)]
}

fn arbiter() -> Arc<TechnicalResourceArbiter> {
    Arc::new(TechnicalResourceArbiter::new([(
        ResourceKind::new("network"),
        10,
    )]))
}

fn operator() -> Actor {
    Actor::Operator {
        label: "job-cancellation-wss-harness".into(),
    }
}

fn generate_test_cert(subject_alt_name: &str) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec![subject_alt_name.to_string()]).expect("cert generation");
    let cert_der = cert.der().clone();
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    (cert_der, key_der)
}

struct Services {
    boot: BootOrchestration,
    enrollment: Arc<Enrollment>,
    jobs: JobService<PostgresJobRepository>,
    scheduling: JobSchedulingService<PostgresJobRepository>,
    intents: DestructiveIntentService<PostgresJobRepository, PostgresInventoryRepository>,
    inventory: InventoryService,
    job_repo: Arc<PostgresJobRepository>,
    target: Arc<FixtureTargetRevalidation>,
}

fn build_services(pool: PgPool) -> Services {
    let boot_repo = Arc::new(PostgresBootContextRepository::new(pool.clone()));
    let endpoint_repo = Arc::new(PostgresEndpointRepository::new(pool.clone()));
    let redemption_repo = Arc::new(PostgresCredentialRedemptionRepository::new(pool.clone()));
    let job_repo = Arc::new(PostgresJobRepository::new(pool.clone()));
    let inventory_repo = Arc::new(PostgresInventoryRepository::new(pool.clone()));
    let target = Arc::new(FixtureTargetRevalidation::new());

    let boot = BootOrchestrationService::new(boot_repo, Duration::minutes(5));
    let enrollment = Arc::new(EnrollmentService::new(endpoint_repo, redemption_repo));
    let jobs = JobService::new(Arc::clone(&job_repo));
    let scheduling = JobSchedulingService::new(Arc::clone(&job_repo));
    let inventory =
        InventoryService::new(Arc::clone(&inventory_repo) as Arc<dyn InventoryRepository>);
    let intents = DestructiveIntentService::new(
        Arc::clone(&job_repo),
        Arc::clone(&inventory_repo),
        Arc::clone(&target) as Arc<dyn TargetRevalidationPort>,
    );

    Services {
        boot,
        enrollment,
        jobs,
        scheduling,
        intents,
        inventory,
        job_repo,
        target,
    }
}

/// Everything one WSS test needs after the handshake/evidence/dispatch
/// setup, mirroring `action_dispatch_wss.rs::DispatchedSession`.
#[allow(dead_code)]
struct DispatchedSession {
    websocket: ClientWs,
    server_task: JoinHandle<Result<(), bamep_server::adapters::agent_gateway::AgentGatewayError>>,
    attempt: Attempt,
    reservation: bamep_server::runtime::resource_arbiter::ReservationId,
    job_id: JobId,
    step_id: JobStepId,
    endpoint_id: EndpointId,
}

/// Establishes one real WSS session, commits one destructive
/// `Attempt{Dispatched}`, and transmits `ActionDispatch` for it — identical
/// to `action_dispatch_wss.rs::establish_and_dispatch`.
#[allow(clippy::too_many_arguments)]
async fn establish_and_dispatch(
    services: &Services,
    gateway: &Arc<Gateway>,
    outbound: &Arc<OutboundSessionDirectory>,
    reservations: &Arc<AttemptReservationRegistry>,
    dispatch_arbiter: &Arc<TechnicalResourceArbiter>,
    issuer: &TrustedBootstrapFixtureIssuer,
    cert_der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
    addr: SocketAddr,
    inventory_signal: &str,
) -> DispatchedSession {
    let now = Utc::now();
    let boot_nonce = BootNonce::generate().expect("OS CSPRNG must be available in tests");
    let credential = services
        .boot
        .issue_enrollment_credential(inventory_signal, boot_nonce, now)
        .await
        .unwrap();

    let fingerprint = ServerCertFingerprint::from_leaf_der(cert_der.as_ref());
    let listener = TcpListener::bind(addr).await.expect("bind");
    let bound_addr = listener.local_addr().unwrap();
    let acceptor = AgentTransportAcceptor::new(vec![cert_der], key_der).expect("build acceptor");
    let gateway_for_task = Arc::clone(gateway);
    let server_task: JoinHandle<
        Result<(), bamep_server::adapters::agent_gateway::AgentGatewayError>,
    > = tokio::spawn(async move {
        let (tcp_stream, _peer) = listener.accept().await.expect("accept tcp");
        let mut connection = acceptor.accept(tcp_stream).await.expect("tls+ws accept");
        let HandshakeOutcome::Established(session) = gateway_for_task
            .handshake(&mut connection.websocket)
            .await?
        else {
            panic!("handshake must establish for a valid credential")
        };
        gateway_for_task
            .run_authenticated_session(
                &mut connection.websocket,
                session,
                connection.server_fingerprint,
            )
            .await
    });

    let assertion = issuer.issue(boot_nonce, fingerprint);
    let material = SimulatedBootstrapMaterial::from_assertion(&assertion);
    let paired = SimulatedPairedTrust::single(issuer.public_key());
    let mut connection =
        connect_after_trusted_bootstrap(bound_addr, "localhost", &paired, boot_nonce, &material)
            .await
            .expect("local trust then pinned WSS succeeds");

    let SimulatorHandshakeOutcome::Established(_established) =
        bamep_simulator::authenticate(&mut connection.websocket, &credential.to_wire_value())
            .await
            .expect("handshake helper must not error")
    else {
        panic!("credential must establish a session")
    };
    send_bootstrap_evidence(&mut connection.websocket, &connection.established)
        .await
        .unwrap();

    let RedeemResult::Established { endpoint_id, .. } = services
        .enrollment
        .redeem(&credential.to_wire_value())
        .await
        .unwrap()
    else {
        panic!("redeem must reuse the already-established Endpoint")
    };
    services
        .enrollment
        .approve_enrollment(
            endpoint_id,
            Actor::Operator {
                label: "cancellation-wss-harness".into(),
            },
            now,
        )
        .await
        .unwrap();

    services
        .inventory
        .record(
            endpoint_id,
            InventoryReportMessage::new(object(json!({"disk": "a"}))),
        )
        .await
        .unwrap();
    services
        .target
        .set_current_target(endpoint_id, TargetFingerprint::new("disk-a"));

    let job = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();
    let step_id = job.steps[0].id;
    services.intents.authorize(job.id, step_id).await.unwrap();
    services.scheduling.admit(job.id).await.unwrap();
    services
        .scheduling
        .satisfy_current_step_preconditions(job.id, step_id)
        .await
        .unwrap();

    let dispatch_service = FinalDispatchService::new(
        Arc::clone(&services.job_repo),
        gateway.presence(),
        Arc::clone(&services.target) as Arc<dyn TargetRevalidationPort>,
        Arc::clone(dispatch_arbiter),
    );
    let FinalDispatchResult::Committed {
        outcome,
        reservation,
    } = dispatch_service
        .commit_destructive_dispatch(job.id, step_id, network_claims())
        .await
        .unwrap()
    else {
        panic!("expected a successful final-dispatch commitment")
    };

    let action_dispatch_service = ActionDispatchService::new(
        Arc::clone(reservations),
        Arc::clone(outbound) as Arc<dyn AgentDispatchPort>,
    );
    let outcome_send = action_dispatch_service
        .dispatch(endpoint_id, outcome.attempt, reservation)
        .await;
    assert!(
        matches!(outcome_send, ActionDispatchOutcome::Sent),
        "expected the local transport to accept the frame, got {outcome_send:?}"
    );

    DispatchedSession {
        websocket: connection.websocket,
        server_task,
        attempt: outcome.attempt,
        reservation,
        job_id: job.id,
        step_id,
        endpoint_id,
    }
}

async fn recv_agent_message(websocket: &mut ClientWs) -> AgentProtocolMessage {
    let frame = websocket
        .next()
        .await
        .expect("a frame is present")
        .expect("frame read ok");
    let Message::Text(text) = frame else {
        panic!("expected a text frame, got {frame:?}")
    };
    decode(text.as_str()).expect("decode ok")
}

async fn send_agent_message(websocket: &mut ClientWs, message: AgentProtocolMessage) {
    let wire = encode(&message).expect("encode ok");
    websocket.send(Message::text(wire)).await.unwrap();
}

async fn attempt_state(pool: &PgPool, attempt_id: uuid::Uuid) -> String {
    sqlx::query_scalar("SELECT state::text FROM attempts WHERE id = $1")
        .bind(attempt_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn job_step_row(pool: &PgPool, step_id: JobStepId) -> (String, Option<String>) {
    let row = sqlx::query("SELECT state::text, failure_reason::text FROM job_steps WHERE id = $1")
        .bind(step_id.0)
        .fetch_one(pool)
        .await
        .unwrap();
    (row.get(0), row.get(1))
}

async fn job_state_text(pool: &PgPool, job_id: JobId) -> String {
    sqlx::query_scalar("SELECT state::text FROM jobs WHERE id = $1")
        .bind(job_id.0)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn event_count(pool: &PgPool, job_id: JobId, event_type: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM domain_events WHERE job_id = $1 AND event_type = $2::domain_event_type",
    )
    .bind(job_id.0)
    .bind(event_type)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn terminal_audit_count(pool: &PgPool, attempt_id: uuid::Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_records WHERE attempt_id = $1 AND detail LIKE 'attempt %reached terminal state%'",
    )
    .bind(attempt_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn cancellation_audit_count(pool: &PgPool, job_id: JobId) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_records WHERE job_id = $1 AND detail LIKE 'operator cancellation%'",
    )
    .bind(job_id.0)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// One [`Services`] plus every shared component a cancellation-over-WSS test
/// needs (`OutboundSessionDirectory`, reservation registry, arbiter,
/// `Gateway` wired with `ActionEvidenceService`/`CancellationService`).
struct CancellationHarness {
    services: Services,
    gateway: Arc<Gateway>,
    outbound: Arc<OutboundSessionDirectory>,
    reservations: Arc<AttemptReservationRegistry>,
    cancellation: Arc<CancellationService>,
}

fn build_harness(pool: PgPool, issuer: &TrustedBootstrapFixtureIssuer) -> CancellationHarness {
    let services = build_services(pool.clone());
    let outbound = Arc::new(OutboundSessionDirectory::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let shared_arbiter = arbiter();
    let evidence_service = Arc::new(ActionEvidenceService::new(
        Arc::clone(&services.job_repo) as Arc<dyn JobRepository>,
        Arc::clone(&reservations),
        Arc::clone(&shared_arbiter),
    ));
    let cancellation = Arc::new(CancellationService::new(
        Arc::clone(&services.job_repo) as Arc<dyn JobRepository>,
        Arc::clone(&reservations),
        Arc::clone(&shared_arbiter),
        Arc::clone(&outbound) as Arc<dyn AgentDispatchPort>,
    ));
    let gateway = Arc::new(
        Gateway::new(Arc::clone(&services.enrollment))
            .with_bootstrap_evidence_service(Arc::new(
                bamep_server::application::BootstrapEvidenceService::new(
                    Arc::new(PostgresEndpointRepository::new(pool.clone())),
                    bamep_trusted_bootstrap::AcceptedSiteKeys::single(issuer.public_key()),
                ),
            ))
            .with_outbound_session_directory(Arc::clone(&outbound))
            .with_action_evidence_service(Arc::clone(&evidence_service))
            .with_cancellation_service(Arc::clone(&cancellation)),
    );

    CancellationHarness {
        services,
        gateway,
        outbound,
        reservations,
        cancellation,
    }
}

#[tokio::test]
async fn successful_cancellation_over_real_wss() {
    let db = TestDatabase::setup().await;
    let (cert_der, key_der) = generate_test_cert("localhost");
    let issuer = TrustedBootstrapFixtureIssuer::from_seed([0x71; 32]);
    let harness = build_harness(db.pool.clone(), &issuer);

    let mut session = establish_and_dispatch(
        &harness.services,
        &harness.gateway,
        &harness.outbound,
        &harness.reservations,
        &arbiter(),
        &issuer,
        cert_der,
        key_der,
        "127.0.0.1:0".parse().unwrap(),
        "wss-cancel-success",
    )
    .await;

    let AgentProtocolMessage::ActionDispatch(dispatch) =
        recv_agent_message(&mut session.websocket).await
    else {
        panic!("expected ActionDispatch")
    };
    let agent =
        SimulatedActionAgent::new().with_default_scenario(ScenarioOutcome::AcceptThenSucceed);
    for message in agent.handle_dispatch(&dispatch) {
        send_agent_message(&mut session.websocket, message).await;
    }

    // Durable cancellation request commits (Running -> Cancelling) and
    // transmits CancelAction over the real session.
    let cancel_result = harness
        .cancellation
        .request(session.job_id, operator())
        .await
        .unwrap();
    let CancellationRequestResult::EnteredCancelling { send } = cancel_result else {
        panic!("expected EnteredCancelling, got {cancel_result:?}")
    };
    assert_eq!(send, CancelActionSendOutcome::Sent);

    let AgentProtocolMessage::CancelAction(cancel) =
        recv_agent_message(&mut session.websocket).await
    else {
        panic!("expected CancelAction")
    };
    assert_eq!(
        cancel.body.action_id, dispatch.body.action_id,
        "the exact existing action_id must be reused, never replaced"
    );

    let cancel_ack = agent.handle_cancel(&cancel);
    assert_eq!(cancel_ack.body.outcome, CancelAckOutcome::Cancelled);
    send_agent_message(
        &mut session.websocket,
        AgentProtocolMessage::CancelAck(cancel_ack),
    )
    .await;

    session.websocket.close(None).await.unwrap();
    session.server_task.await.unwrap().unwrap();

    assert_eq!(
        attempt_state(&db.pool, session.attempt.id.0).await,
        "Cancelled"
    );
    let (step_state, _) = job_step_row(&db.pool, session.step_id).await;
    assert_eq!(step_state, "Cancelled");
    assert_eq!(job_state_text(&db.pool, session.job_id).await, "Cancelled");
    assert_eq!(
        event_count(&db.pool, session.job_id, "JobCancelled").await,
        1
    );
    assert_eq!(cancellation_audit_count(&db.pool, session.job_id).await, 1);
    assert_eq!(
        terminal_audit_count(&db.pool, session.attempt.id.0).await,
        1
    );
    // Reservation released exactly once.
    assert_eq!(harness.reservations.take(session.attempt.id), None);

    db.teardown().await;
}

#[tokio::test]
async fn cannot_cancel_then_real_terminal_result_still_ends_the_job_cancelled_over_real_wss() {
    let db = TestDatabase::setup().await;
    let (cert_der, key_der) = generate_test_cert("localhost");
    let issuer = TrustedBootstrapFixtureIssuer::from_seed([0x72; 32]);
    let harness = build_harness(db.pool.clone(), &issuer);

    let mut session = establish_and_dispatch(
        &harness.services,
        &harness.gateway,
        &harness.outbound,
        &harness.reservations,
        &arbiter(),
        &issuer,
        cert_der,
        key_der,
        "127.0.0.1:0".parse().unwrap(),
        "wss-cancel-cannot-cancel",
    )
    .await;

    let AgentProtocolMessage::ActionDispatch(dispatch) =
        recv_agent_message(&mut session.websocket).await
    else {
        panic!("expected ActionDispatch")
    };
    let agent =
        SimulatedActionAgent::new().with_default_scenario(ScenarioOutcome::AcceptThenSucceed);
    for message in agent.handle_dispatch(&dispatch) {
        send_agent_message(&mut session.websocket, message).await;
    }

    let cancel_result = harness
        .cancellation
        .request(session.job_id, operator())
        .await
        .unwrap();
    assert!(matches!(
        cancel_result,
        CancellationRequestResult::EnteredCancelling { .. }
    ));

    let AgentProtocolMessage::CancelAction(cancel) =
        recv_agent_message(&mut session.websocket).await
    else {
        panic!("expected CancelAction")
    };
    agent.configure_cancel_behavior(cancel.body.action_id, CancelBehavior::CannotCancel);
    let cancel_ack = agent.handle_cancel(&cancel);
    assert_eq!(cancel_ack.body.outcome, CancelAckOutcome::CannotCancel);
    send_agent_message(
        &mut session.websocket,
        AgentProtocolMessage::CancelAck(cancel_ack),
    )
    .await;

    // The real terminal ActionResult still arrives afterward.
    for message in agent
        .run_configured_scenario(dispatch.body.action_id)
        .unwrap()
    {
        send_agent_message(&mut session.websocket, message).await;
    }

    session.websocket.close(None).await.unwrap();
    session.server_task.await.unwrap().unwrap();

    // Real execution result preserved on Attempt/JobStep...
    assert_eq!(
        attempt_state(&db.pool, session.attempt.id.0).await,
        "Succeeded"
    );
    let (step_state, _) = job_step_row(&db.pool, session.step_id).await;
    assert_eq!(step_state, "Succeeded");
    // ...but the Job ends Cancelled, not Succeeded.
    assert_eq!(job_state_text(&db.pool, session.job_id).await, "Cancelled");
    assert_eq!(
        event_count(&db.pool, session.job_id, "JobCancelled").await,
        1
    );
    assert_eq!(
        event_count(&db.pool, session.job_id, "JobSucceeded").await,
        0
    );

    db.teardown().await;
}

#[tokio::test]
async fn already_completed_with_unknown_result_awaits_reconciliation_over_real_wss() {
    let db = TestDatabase::setup().await;
    let (cert_der, key_der) = generate_test_cert("localhost");
    let issuer = TrustedBootstrapFixtureIssuer::from_seed([0x73; 32]);
    let harness = build_harness(db.pool.clone(), &issuer);

    let mut session = establish_and_dispatch(
        &harness.services,
        &harness.gateway,
        &harness.outbound,
        &harness.reservations,
        &arbiter(),
        &issuer,
        cert_der,
        key_der,
        "127.0.0.1:0".parse().unwrap(),
        "wss-cancel-already-completed",
    )
    .await;

    let AgentProtocolMessage::ActionDispatch(dispatch) =
        recv_agent_message(&mut session.websocket).await
    else {
        panic!("expected ActionDispatch")
    };
    let agent =
        SimulatedActionAgent::new().with_default_scenario(ScenarioOutcome::AcceptThenSucceed);
    for message in agent.handle_dispatch(&dispatch) {
        send_agent_message(&mut session.websocket, message).await;
    }

    // The Agent locally completes the action, but the resulting
    // ActionProgress/ActionResult frames are deliberately never transmitted
    // — the Server still sees an active Attempt.
    let _withheld = agent
        .run_configured_scenario(dispatch.body.action_id)
        .unwrap();

    let cancel_result = harness
        .cancellation
        .request(session.job_id, operator())
        .await
        .unwrap();
    assert!(matches!(
        cancel_result,
        CancellationRequestResult::EnteredCancelling { .. }
    ));

    let AgentProtocolMessage::CancelAction(cancel) =
        recv_agent_message(&mut session.websocket).await
    else {
        panic!("expected CancelAction")
    };
    let cancel_ack = agent.handle_cancel(&cancel);
    assert_eq!(cancel_ack.body.outcome, CancelAckOutcome::AlreadyCompleted);
    send_agent_message(
        &mut session.websocket,
        AgentProtocolMessage::CancelAck(cancel_ack),
    )
    .await;

    session.websocket.close(None).await.unwrap();
    session.server_task.await.unwrap().unwrap();

    assert_eq!(
        attempt_state(&db.pool, session.attempt.id.0).await,
        "AwaitingReconciliation"
    );
    let (step_state, _) = job_step_row(&db.pool, session.step_id).await;
    assert_eq!(step_state, "Dispatching");
    assert_eq!(job_state_text(&db.pool, session.job_id).await, "Cancelling");
    assert_eq!(
        event_count(&db.pool, session.job_id, "JobCancelled").await,
        0
    );

    db.teardown().await;
}

#[tokio::test]
async fn unknown_cancel_ack_awaits_reconciliation_without_fabricating_a_terminal_outcome_over_real_wss(
) {
    let db = TestDatabase::setup().await;
    let (cert_der, key_der) = generate_test_cert("localhost");
    let issuer = TrustedBootstrapFixtureIssuer::from_seed([0x74; 32]);
    let harness = build_harness(db.pool.clone(), &issuer);

    let mut session = establish_and_dispatch(
        &harness.services,
        &harness.gateway,
        &harness.outbound,
        &harness.reservations,
        &arbiter(),
        &issuer,
        cert_der,
        key_der,
        "127.0.0.1:0".parse().unwrap(),
        "wss-cancel-unknown",
    )
    .await;

    // No ActionAck is ever sent — the committed `Dispatched` Attempt is
    // already cancellation-relevant on its own.
    let AgentProtocolMessage::ActionDispatch(dispatch) =
        recv_agent_message(&mut session.websocket).await
    else {
        panic!("expected ActionDispatch")
    };

    let cancel_result = harness
        .cancellation
        .request(session.job_id, operator())
        .await
        .unwrap();
    assert!(matches!(
        cancel_result,
        CancellationRequestResult::EnteredCancelling { .. }
    ));

    let AgentProtocolMessage::CancelAction(cancel) =
        recv_agent_message(&mut session.websocket).await
    else {
        panic!("expected CancelAction")
    };
    assert_eq!(cancel.body.action_id, dispatch.body.action_id);

    // A fresh Agent-local instance stands in for loss of local state (Agent
    // restart) — a deterministic control hook, not a flaky real restart.
    let restarted_agent = SimulatedActionAgent::new();
    let cancel_ack = restarted_agent.handle_cancel(&cancel);
    assert_eq!(cancel_ack.body.outcome, CancelAckOutcome::Unknown);
    send_agent_message(
        &mut session.websocket,
        AgentProtocolMessage::CancelAck(cancel_ack),
    )
    .await;

    session.websocket.close(None).await.unwrap();
    session.server_task.await.unwrap().unwrap();

    assert_eq!(
        attempt_state(&db.pool, session.attempt.id.0).await,
        "AwaitingReconciliation"
    );
    let (step_state, _) = job_step_row(&db.pool, session.step_id).await;
    assert_eq!(step_state, "Dispatching");
    assert_eq!(job_state_text(&db.pool, session.job_id).await, "Cancelling");
    assert_eq!(
        event_count(&db.pool, session.job_id, "JobCancelled").await,
        0,
        "Unknown never fabricates a terminal outcome"
    );

    db.teardown().await;
}
