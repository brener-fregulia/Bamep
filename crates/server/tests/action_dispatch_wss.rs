//! Issue #26 "[WP] Dispatch typed actions and complete Attempts": real
//! loopback TCP -> pinned TLS 1.3 -> WebSocket -> Agent Protocol v1
//! integration proving the complete normal connected-session execution path
//! crosses the real WSS/Agent Control Gateway boundary end to end
//! (`m0-simulator-contract-and-validation-strategy.md` "Fidelity boundary"):
//!
//! ```text
//! committed Attempt{Dispatched} (#25)
//!   -> ActionDispatchService transmits ActionDispatch over the real
//!      OutboundSessionDirectory/AgentControlGateway session
//!   -> real WSS delivery to a Simulated Agent (SimulatedActionAgent)
//!   -> ActionAck/ActionProgress/ActionResult back over the same session
//!   -> AgentControlGateway -> ActionEvidenceService -> PostgreSQL
//! ```
//!
//! Combines the required normal-success, normal-failure, dispatch-rejection,
//! and duplicate/delayed-terminal-evidence scenarios across a small number of
//! real WSS fixtures rather than one fixture per scenario
//! (`m0-simulator-contract-and-validation-strategy.md`: "may be efficiently
//! combined").
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

mod support;

use std::net::SocketAddr;
use std::sync::Arc;

use bamep_agent_protocol::{
    decode, encode, AgentProtocolMessage, InventoryReportMessage, ProtocolId,
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
    connect_after_trusted_bootstrap, send_bootstrap_evidence, ScenarioOutcome,
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
/// setup: the still-open client `websocket`, the spawned server session
/// task (join it after the client closes), and the committed identifiers.
#[allow(dead_code)]
struct DispatchedSession {
    websocket: ClientWs,
    server_task: JoinHandle<Result<(), bamep_server::adapters::agent_gateway::AgentGatewayError>>,
    attempt: Attempt,
    /// Not read by every test — kept for tests that assert on the resource
    /// arbiter directly instead.
    reservation: bamep_server::runtime::resource_arbiter::ReservationId,
    job_id: JobId,
    step_id: JobStepId,
    endpoint_id: EndpointId,
}

/// Establishes one real WSS session (trusted bootstrap -> pinned TLS ->
/// Agent Protocol handshake -> `BootstrapEvidence`), builds and commits one
/// destructive `Attempt{Dispatched}` for it (all seven preconditions hold),
/// and transmits `ActionDispatch` for it through the real
/// `OutboundSessionDirectory`/`AgentControlGateway` session — but does not
/// yet read the resulting frame; the caller does that.
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
                label: "action-dispatch-wss-harness".into(),
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

    // `run_authenticated_session` registers Runtime Presence asynchronously;
    // the seven-item destructive gate below consumes it. Wait for it so the
    // commitment is deterministic under heavy parallel test load.
    {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while !gateway.presence().is_present(endpoint_id) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the authenticated session never became present"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

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

async fn terminal_audit_count(pool: &PgPool, attempt_id: uuid::Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_records WHERE attempt_id = $1 AND detail LIKE 'attempt %reached terminal state%'",
    )
    .bind(attempt_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn normal_success_over_real_wss_then_duplicate_terminal_evidence_is_ignored() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let (cert_der, key_der) = generate_test_cert("localhost");
    let issuer = TrustedBootstrapFixtureIssuer::from_seed([0x61; 32]);
    let outbound = Arc::new(OutboundSessionDirectory::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let evidence_arbiter = arbiter();
    let evidence_service = Arc::new(ActionEvidenceService::new(
        Arc::clone(&services.job_repo) as Arc<dyn JobRepository>,
        Arc::clone(&reservations),
        Arc::clone(&evidence_arbiter),
    ));
    let gateway = Arc::new(
        Gateway::new(Arc::clone(&services.enrollment))
            .with_bootstrap_evidence_service(Arc::new(
                bamep_server::application::BootstrapEvidenceService::new(
                    Arc::new(PostgresEndpointRepository::new(db.pool.clone())),
                    bamep_trusted_bootstrap::AcceptedSiteKeys::single(issuer.public_key()),
                ),
            ))
            .with_outbound_session_directory(Arc::clone(&outbound))
            .with_action_evidence_service(Arc::clone(&evidence_service)),
    );

    let mut session = establish_and_dispatch(
        &services,
        &gateway,
        &outbound,
        &reservations,
        &arbiter(),
        &issuer,
        cert_der,
        key_der,
        "127.0.0.1:0".parse().unwrap(),
        "wss-action-success",
    )
    .await;

    let AgentProtocolMessage::ActionDispatch(dispatch) =
        recv_agent_message(&mut session.websocket).await
    else {
        panic!("expected ActionDispatch")
    };
    assert_eq!(
        dispatch.body.action_id,
        ProtocolId::from_uuid(session.attempt.action_id.0).unwrap(),
        "the exact persisted action_id must be reused, never replaced"
    );

    let agent =
        SimulatedActionAgent::new().with_default_scenario(ScenarioOutcome::AcceptThenSucceed);
    for message in agent.handle_dispatch(&dispatch) {
        send_agent_message(&mut session.websocket, message).await;
    }
    let action_id = dispatch.body.action_id;
    let scenario_messages = agent.run_configured_scenario(action_id).unwrap();
    for message in &scenario_messages {
        send_agent_message(&mut session.websocket, message.clone()).await;
    }

    // Delayed/duplicate terminal evidence over the same real connection,
    // sent again with fresh message_ids after the terminal outcome already
    // committed — must never mutate anything further
    // (`m0-job-lifecycle-and-scheduling.md` "Duplicate and delayed
    // evidence").
    let AgentProtocolMessage::ActionResult(original_result) = scenario_messages.last().unwrap()
    else {
        panic!("expected the scenario's last message to be ActionResult")
    };
    let delayed_result = original_result.clone().with_fresh_message_id();
    send_agent_message(
        &mut session.websocket,
        AgentProtocolMessage::ActionResult(delayed_result),
    )
    .await;
    let delayed_ack = bamep_agent_protocol::ActionAckMessage::accepted(action_id);
    send_agent_message(
        &mut session.websocket,
        AgentProtocolMessage::ActionAck(delayed_ack),
    )
    .await;

    // Delayed/duplicate ActionProgress after the terminal outcome already
    // committed — must never create a durable Attempt/JobStep/Job mutation,
    // event, or audit; the terminal outcome remains authoritative (Issue #26
    // correction "Correlate ActionProgress to the authenticated Endpoint").
    let AgentProtocolMessage::ActionProgress(delayed_progress_source) = &scenario_messages[1]
    else {
        panic!("expected the scenario's second message to be ActionProgress")
    };
    let delayed_progress = delayed_progress_source.clone().with_fresh_message_id();
    send_agent_message(
        &mut session.websocket,
        AgentProtocolMessage::ActionProgress(delayed_progress),
    )
    .await;

    // Give the server task's select! loop a chance to process every frame
    // already written to the socket before we start asserting DB state —
    // achieved deterministically below by closing the connection and
    // joining the server task, not by sleeping.
    session.websocket.close(None).await.unwrap();
    session.server_task.await.unwrap().unwrap();

    assert_eq!(
        attempt_state(&db.pool, session.attempt.id.0).await,
        "Succeeded"
    );
    let (step_state, _) = job_step_row(&db.pool, session.step_id).await;
    assert_eq!(step_state, "Succeeded");
    assert_eq!(job_state_text(&db.pool, session.job_id).await, "Succeeded");
    assert_eq!(
        terminal_audit_count(&db.pool, session.attempt.id.0).await,
        1,
        "the delayed duplicate ActionResult, the delayed Ack, and the delayed \
         ActionProgress must not create a second audit"
    );

    // The reservation must have been released exactly once — never twice by
    // the delayed duplicate evidence.
    assert!(evidence_arbiter.acquire(network_claims()).is_ok());

    db.teardown().await;
}

#[tokio::test]
async fn normal_execution_failure_over_real_wss() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let (cert_der, key_der) = generate_test_cert("localhost");
    let issuer = TrustedBootstrapFixtureIssuer::from_seed([0x62; 32]);
    let outbound = Arc::new(OutboundSessionDirectory::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let evidence_service = Arc::new(ActionEvidenceService::new(
        Arc::clone(&services.job_repo) as Arc<dyn JobRepository>,
        Arc::clone(&reservations),
        arbiter(),
    ));
    let gateway = Arc::new(
        Gateway::new(Arc::clone(&services.enrollment))
            .with_bootstrap_evidence_service(Arc::new(
                bamep_server::application::BootstrapEvidenceService::new(
                    Arc::new(PostgresEndpointRepository::new(db.pool.clone())),
                    bamep_trusted_bootstrap::AcceptedSiteKeys::single(issuer.public_key()),
                ),
            ))
            .with_outbound_session_directory(Arc::clone(&outbound))
            .with_action_evidence_service(Arc::clone(&evidence_service)),
    );

    let mut session = establish_and_dispatch(
        &services,
        &gateway,
        &outbound,
        &reservations,
        &arbiter(),
        &issuer,
        cert_der,
        key_der,
        "127.0.0.1:0".parse().unwrap(),
        "wss-action-failure",
    )
    .await;

    let AgentProtocolMessage::ActionDispatch(dispatch) =
        recv_agent_message(&mut session.websocket).await
    else {
        panic!("expected ActionDispatch")
    };
    let agent = SimulatedActionAgent::new().with_default_scenario(ScenarioOutcome::AcceptThenFail);
    for message in agent.handle_dispatch(&dispatch) {
        send_agent_message(&mut session.websocket, message).await;
    }
    for message in agent
        .run_configured_scenario(dispatch.body.action_id)
        .unwrap()
    {
        send_agent_message(&mut session.websocket, message).await;
    }

    session.websocket.close(None).await.unwrap();
    session.server_task.await.unwrap().unwrap();

    assert_eq!(
        attempt_state(&db.pool, session.attempt.id.0).await,
        "Failed"
    );
    let (step_state, failure_reason) = job_step_row(&db.pool, session.step_id).await;
    assert_eq!(step_state, "Failed");
    assert_eq!(failure_reason.as_deref(), Some("ExecutionFailed"));
    assert_eq!(job_state_text(&db.pool, session.job_id).await, "Failed");

    db.teardown().await;
}

#[tokio::test]
async fn dispatch_rejection_over_real_wss() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let (cert_der, key_der) = generate_test_cert("localhost");
    let issuer = TrustedBootstrapFixtureIssuer::from_seed([0x63; 32]);
    let outbound = Arc::new(OutboundSessionDirectory::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let evidence_service = Arc::new(ActionEvidenceService::new(
        Arc::clone(&services.job_repo) as Arc<dyn JobRepository>,
        Arc::clone(&reservations),
        arbiter(),
    ));
    let gateway = Arc::new(
        Gateway::new(Arc::clone(&services.enrollment))
            .with_bootstrap_evidence_service(Arc::new(
                bamep_server::application::BootstrapEvidenceService::new(
                    Arc::new(PostgresEndpointRepository::new(db.pool.clone())),
                    bamep_trusted_bootstrap::AcceptedSiteKeys::single(issuer.public_key()),
                ),
            ))
            .with_outbound_session_directory(Arc::clone(&outbound))
            .with_action_evidence_service(Arc::clone(&evidence_service)),
    );

    let mut session = establish_and_dispatch(
        &services,
        &gateway,
        &outbound,
        &reservations,
        &arbiter(),
        &issuer,
        cert_der,
        key_der,
        "127.0.0.1:0".parse().unwrap(),
        "wss-action-reject",
    )
    .await;

    let AgentProtocolMessage::ActionDispatch(dispatch) =
        recv_agent_message(&mut session.websocket).await
    else {
        panic!("expected ActionDispatch")
    };
    let agent = SimulatedActionAgent::new().with_default_scenario(ScenarioOutcome::Reject);
    for message in agent.handle_dispatch(&dispatch) {
        send_agent_message(&mut session.websocket, message).await;
    }
    assert!(
        agent
            .run_configured_scenario(dispatch.body.action_id)
            .is_none(),
        "a Rejected action is never Active"
    );

    session.websocket.close(None).await.unwrap();
    session.server_task.await.unwrap().unwrap();

    assert_eq!(
        attempt_state(&db.pool, session.attempt.id.0).await,
        "Rejected"
    );
    let (step_state, failure_reason) = job_step_row(&db.pool, session.step_id).await;
    assert_eq!(step_state, "Failed");
    assert_eq!(failure_reason.as_deref(), Some("DispatchRejected"));
    assert_eq!(job_state_text(&db.pool, session.job_id).await, "Failed");

    db.teardown().await;
}

/// Issue #26 correction "Complete the real-WSS duplicate/delayed proof",
/// scenario A: the exact same `ActionDispatch` frame crosses the real wire to
/// the Simulated Agent twice. Simulated by writing it a second time directly
/// through the real `OutboundSessionDirectory` transport Port, bypassing
/// `ActionDispatchService`'s own registration guard entirely — that guard
/// (this same correction, scenario "Prevent a second server-side dispatch
/// attempt") already proves the Server itself never re-sends; this test is
/// deliberately an Agent-side idempotency/contract proof only, exercising the
/// path a resend from outside normal Server scheduling (e.g. a future #28
/// reconciliation resend) would take.
#[tokio::test]
async fn duplicate_action_dispatch_over_real_wss_executes_once_and_creates_no_second_attempt() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let (cert_der, key_der) = generate_test_cert("localhost");
    let issuer = TrustedBootstrapFixtureIssuer::from_seed([0x64; 32]);
    let outbound = Arc::new(OutboundSessionDirectory::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let evidence_service = Arc::new(ActionEvidenceService::new(
        Arc::clone(&services.job_repo) as Arc<dyn JobRepository>,
        Arc::clone(&reservations),
        arbiter(),
    ));
    let gateway = Arc::new(
        Gateway::new(Arc::clone(&services.enrollment))
            .with_bootstrap_evidence_service(Arc::new(
                bamep_server::application::BootstrapEvidenceService::new(
                    Arc::new(PostgresEndpointRepository::new(db.pool.clone())),
                    bamep_trusted_bootstrap::AcceptedSiteKeys::single(issuer.public_key()),
                ),
            ))
            .with_outbound_session_directory(Arc::clone(&outbound))
            .with_action_evidence_service(Arc::clone(&evidence_service)),
    );

    let mut session = establish_and_dispatch(
        &services,
        &gateway,
        &outbound,
        &reservations,
        &arbiter(),
        &issuer,
        cert_der,
        key_der,
        "127.0.0.1:0".parse().unwrap(),
        "wss-action-duplicate-dispatch",
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
    for message in agent
        .run_configured_scenario(dispatch.body.action_id)
        .unwrap()
    {
        send_agent_message(&mut session.websocket, message).await;
    }

    // The exact same frame crosses the wire again, after the Agent already
    // completed it.
    outbound
        .dispatch_action(session.endpoint_id, dispatch.clone())
        .await
        .expect("duplicate frame accepted by the local transport");
    let AgentProtocolMessage::ActionDispatch(duplicate_dispatch) =
        recv_agent_message(&mut session.websocket).await
    else {
        panic!("expected the duplicate ActionDispatch frame")
    };
    assert_eq!(duplicate_dispatch.body.action_id, dispatch.body.action_id);

    // Already Completed: the Agent re-emits the retained ActionResult under
    // a fresh message_id, without re-executing.
    let duplicate_response = agent.handle_dispatch(&duplicate_dispatch);
    let [AgentProtocolMessage::ActionResult(duplicate_result)] = duplicate_response.as_slice()
    else {
        panic!("expected exactly one retained ActionResult to be re-emitted")
    };
    assert_eq!(duplicate_result.body.action_id, dispatch.body.action_id);
    assert_eq!(
        duplicate_result.body.outcome,
        bamep_agent_protocol::ActionResultOutcome::Succeeded
    );
    send_agent_message(
        &mut session.websocket,
        AgentProtocolMessage::ActionResult(duplicate_result.clone()),
    )
    .await;

    session.websocket.close(None).await.unwrap();
    session.server_task.await.unwrap().unwrap();

    assert_eq!(
        attempt_state(&db.pool, session.attempt.id.0).await,
        "Succeeded"
    );
    let attempt_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM attempts WHERE job_step_id = $1")
            .bind(session.step_id.0)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(attempt_count, 1, "no second Server Attempt must exist");
    assert_eq!(
        terminal_audit_count(&db.pool, session.attempt.id.0).await,
        1,
        "the duplicate ActionDispatch's re-emitted ActionResult must not create a second audit"
    );

    db.teardown().await;
}

/// Issue #26 correction "Enforce the action wire contract on untrusted
/// input": malformed/incompatible `ActionResult.detail` for the concrete M1
/// action must never cause a durable terminal transition, even though
/// `detail` decodes as a structurally valid JSON object (the wire-invalid
/// shapes `bamep-agent-protocol`'s codec itself now rejects — `ActionAck`
/// outcome/error mismatches, an all-absent `ActionProgress` — are covered by
/// `crates/agent-protocol/tests/action_contract.rs`; this is the one
/// contract check that is Application-level, not codec-level, since
/// `detail`'s schema is owned by the Specification that owns the concrete
/// `action_type`).
#[tokio::test]
async fn malformed_action_result_detail_never_causes_a_durable_terminal_transition() {
    let db = TestDatabase::setup().await;
    let services = build_services(db.pool.clone());
    let (cert_der, key_der) = generate_test_cert("localhost");
    let issuer = TrustedBootstrapFixtureIssuer::from_seed([0x65; 32]);
    let outbound = Arc::new(OutboundSessionDirectory::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let evidence_service = Arc::new(ActionEvidenceService::new(
        Arc::clone(&services.job_repo) as Arc<dyn JobRepository>,
        Arc::clone(&reservations),
        arbiter(),
    ));
    let gateway = Arc::new(
        Gateway::new(Arc::clone(&services.enrollment))
            .with_bootstrap_evidence_service(Arc::new(
                bamep_server::application::BootstrapEvidenceService::new(
                    Arc::new(PostgresEndpointRepository::new(db.pool.clone())),
                    bamep_trusted_bootstrap::AcceptedSiteKeys::single(issuer.public_key()),
                ),
            ))
            .with_outbound_session_directory(Arc::clone(&outbound))
            .with_action_evidence_service(Arc::clone(&evidence_service)),
    );

    let mut session = establish_and_dispatch(
        &services,
        &gateway,
        &outbound,
        &reservations,
        &arbiter(),
        &issuer,
        cert_der,
        key_der,
        "127.0.0.1:0".parse().unwrap(),
        "wss-action-malformed-detail",
    )
    .await;

    let AgentProtocolMessage::ActionDispatch(dispatch) =
        recv_agent_message(&mut session.websocket).await
    else {
        panic!("expected ActionDispatch")
    };
    let action_id = dispatch.body.action_id;

    let bogus_detail = object(json!({"code": "NOT_THE_NORMATIVE_M1_CODE"}));
    let bogus_result = bamep_agent_protocol::ActionResultMessage::new(
        action_id,
        bamep_agent_protocol::ActionResultOutcome::Succeeded,
        bogus_detail,
    );
    send_agent_message(
        &mut session.websocket,
        AgentProtocolMessage::ActionResult(bogus_result),
    )
    .await;
    let AgentProtocolMessage::ProtocolError(error) =
        recv_agent_message(&mut session.websocket).await
    else {
        panic!("expected a ProtocolError response for the malformed detail")
    };
    assert_eq!(error.body.code, "GENERIC");

    session.websocket.close(None).await.unwrap();
    session.server_task.await.unwrap().unwrap();

    assert_eq!(
        attempt_state(&db.pool, session.attempt.id.0).await,
        "Dispatched",
        "malformed detail must never cause a durable terminal transition"
    );
    let (step_state, _) = job_step_row(&db.pool, session.step_id).await;
    assert_eq!(step_state, "Dispatching");
    assert_eq!(
        terminal_audit_count(&db.pool, session.attempt.id.0).await,
        0
    );

    db.teardown().await;
}
