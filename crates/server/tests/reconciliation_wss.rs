//! Issue #28 "[WP] Reconcile interrupted Attempts safely": real loopback TCP
//! -> pinned TLS 1.3 -> WebSocket -> Agent Protocol v1 integration proving
//! reconciliation crosses the real WSS/Agent Control Gateway boundary end to
//! end (`m0-simulator-contract-and-validation-strategy.md` "Fidelity
//! boundary"):
//!
//! ```text
//! committed Attempt{Dispatched} (#25/#26)
//!   -> real WSS session closes (connection loss)
//!   -> AgentControlGateway -> ReconciliationService::mark_endpoint_uncertain
//!      durably commits Dispatched -> AwaitingReconciliation
//!   -> a fresh real WSS session (re)establishes for the same Endpoint
//!   -> AgentControlGateway -> ReconciliationService::reconcile_on_session_start
//!      transmits StatusQuery{exact existing action_id} over that session
//!   -> real WSS delivery to a Simulated Agent (SimulatedActionAgent)
//!   -> StatusReport back over the same session
//!   -> AgentControlGateway -> ReconciliationService::apply_status_report -> PostgreSQL
//! ```
//!
//! Combines the required disconnect/reconnect-terminal-reconciliation, Agent-
//! restart-`Unknown`, and Server-restart scenarios across a small number of
//! real WSS fixtures rather than one fixture per scenario
//! (`m0-simulator-contract-and-validation-strategy.md`: "may be efficiently
//! combined"). Mirrors `job_cancellation_wss.rs`'s harness.
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

mod support;

use std::net::SocketAddr;
use std::sync::Arc;

use bamep_agent_protocol::{decode, encode, AgentProtocolMessage, KnownActionState};
use bamep_domain::{Attempt, BootNonce, EndpointId, JobId, JobStepId, TargetFingerprint};
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
    InventoryService, JobSchedulingService, JobService, ReconciliationService, RedeemResult,
};
use bamep_server::ports::{
    AgentDispatchPort, CloseIndeterminateResult, InventoryRepository, JobRepository,
    TargetRevalidationPort,
};
use bamep_server::runtime::outbound_sessions::OutboundSessionDirectory;
use bamep_server::runtime::presence::PresenceRegistry;
use bamep_server::runtime::reservation_registry::AttemptReservationRegistry;
use bamep_server::runtime::resource_arbiter::{
    ResourceClaim, ResourceKind, TechnicalResourceArbiter,
};
use bamep_simulator::{
    connect_after_trusted_bootstrap, send_bootstrap_evidence, SimulatedActionAgent,
    SimulatedBootstrapMaterial, SimulatedPairedTrust, SimulatorHandshakeOutcome,
    TrustedBootstrapFixtureIssuer,
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

/// A running Gateway plus every shared Runtime Service component one WSS
/// session needs (`OutboundSessionDirectory`, `PresenceRegistry`,
/// reservation registry, `ReconciliationService`).
struct Harness {
    gateway: Arc<Gateway>,
    outbound: Arc<OutboundSessionDirectory>,
    presence: Arc<PresenceRegistry>,
    reservations: Arc<AttemptReservationRegistry>,
    reconciliation: Arc<ReconciliationService>,
    dispatch_arbiter: Arc<TechnicalResourceArbiter>,
}

fn build_harness(
    pool: PgPool,
    services: &Services,
    issuer: &TrustedBootstrapFixtureIssuer,
) -> Harness {
    let outbound = Arc::new(OutboundSessionDirectory::new());
    let reservations = Arc::new(AttemptReservationRegistry::new());
    let shared_arbiter = arbiter();
    let evidence_service = Arc::new(ActionEvidenceService::new(
        Arc::clone(&services.job_repo) as Arc<dyn JobRepository>,
        Arc::clone(&reservations),
        Arc::clone(&shared_arbiter),
    ));
    let reconciliation = Arc::new(ReconciliationService::new(
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
            .with_reconciliation_service(Arc::clone(&reconciliation)),
    );
    let presence = gateway.presence();

    Harness {
        gateway,
        outbound,
        presence,
        reservations,
        reconciliation,
        dispatch_arbiter: shared_arbiter,
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

/// Polls `attempt_id`'s durable state until it reaches `expected`, bounded to
/// avoid ever hanging a test — the terminal-evidence frames a test sends over
/// a still-open (not closed) session are processed by that session's own
/// independent Gateway task, so there is no other synchronization point a
/// test can await directly (unlike closing a session and awaiting its
/// `server_task`, which this helper exists specifically to avoid needing when
/// a test must keep that session open afterward). Panics with the
/// last-observed state on timeout — never silently proceeds with a wrong
/// precondition.
async fn wait_for_attempt_state(pool: &PgPool, attempt_id: uuid::Uuid, expected: &str) {
    for _ in 0..200 {
        let observed = attempt_state(pool, attempt_id).await;
        if observed == expected {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!(
        "attempt {attempt_id} never reached {expected:?}, last observed {:?}",
        attempt_state(pool, attempt_id).await
    );
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

/// End-to-end fixture context threaded through one scenario: everything
/// needed to establish the first WSS session, dispatch one destructive
/// Attempt, then later establish a *second* WSS session for the same
/// Endpoint using the same enrolled credential chain (ADR-0012 reconnect).
#[allow(dead_code)]
struct Fixture {
    boot_nonce: BootNonce,
    fingerprint: ServerCertFingerprint,
    job_id: JobId,
    step_id: JobStepId,
    endpoint_id: EndpointId,
    attempt: Attempt,
    /// The rotated `runtime_credential` wire value from this session's own
    /// `SessionEstablished` — every successful `AuthRequest`, including
    /// reconnect, returns a fresh one (`m0-agent-protocol-contract.md`
    /// "Runtime credential wire behavior"), so this is exactly what a
    /// subsequent [`reconnect`] call must present.
    runtime_credential: String,
}

/// The Job/Endpoint/session context [`connect_and_prepare`] establishes,
/// before any destructive dispatch has been committed — split out from
/// [`Fixture`] so a test can connect an additional overlapping session
/// (Issue #28 corrective pass "Session-loss reconciliation with overlapping
/// sessions") before deciding when dispatch should actually happen (and
/// therefore which currently-live session it routes through).
#[allow(dead_code)]
struct PreparedSession {
    boot_nonce: BootNonce,
    fingerprint: ServerCertFingerprint,
    job_id: JobId,
    step_id: JobStepId,
    endpoint_id: EndpointId,
    runtime_credential: String,
}

/// Establishes one real WSS session for a freshly enrolled, fully-trusted
/// Endpoint with one `Running` Job of `step_count` ordered steps, whose FIRST
/// step already holds `PreconditionsSatisfied` — everything short of the
/// actual destructive dispatch commitment, which [`commit_and_dispatch`]
/// performs separately. `step_count > 1` lets a test drive a second Attempt
/// through the same Job once the first reaches a terminal state (Issue #28
/// second corrective pass "cross-Attempt stale session correlation").
#[allow(clippy::too_many_arguments)]
async fn connect_and_prepare(
    services: &Services,
    harness: &Harness,
    issuer: &TrustedBootstrapFixtureIssuer,
    cert_der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
    addr: SocketAddr,
    inventory_signal: &str,
    step_count: usize,
) -> (
    ClientWs,
    JoinHandle<Result<(), bamep_server::adapters::agent_gateway::AgentGatewayError>>,
    PreparedSession,
) {
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
    let gateway_for_task = Arc::clone(&harness.gateway);
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

    // This second, direct `redeem` call (the same pattern
    // `job_cancellation_wss.rs::establish_and_dispatch` uses) is itself
    // another successful redemption of the same first-contact identity —
    // its own freshly issued `runtime_credential` is the one that is
    // actually currently active afterward, superseding whatever the real
    // WSS handshake's `SessionEstablished` returned moments earlier. A
    // reconnect must present *this* wire value, not the WSS handshake's now
    // superseded one.
    let RedeemResult::Established {
        endpoint_id,
        runtime_credential: active_credential,
        ..
    } = services
        .enrollment
        .redeem(&credential.to_wire_value())
        .await
        .unwrap()
    else {
        panic!("redeem must reuse the already-established Endpoint")
    };
    let runtime_credential = active_credential.to_wire_value();
    services
        .enrollment
        .approve_enrollment(
            endpoint_id,
            bamep_domain::Actor::Operator {
                label: "reconciliation-wss-harness".into(),
            },
            now,
        )
        .await
        .unwrap();

    services
        .inventory
        .record(
            endpoint_id,
            bamep_agent_protocol::InventoryReportMessage::new(object(json!({"disk": "a"}))),
        )
        .await
        .unwrap();
    services
        .target
        .set_current_target(endpoint_id, TargetFingerprint::new("disk-a"));

    let job = services
        .jobs
        .create_workflow(endpoint_id, step_count)
        .await
        .unwrap();
    let step_id = job.steps[0].id;
    services.intents.authorize(job.id, step_id).await.unwrap();
    services.scheduling.admit(job.id).await.unwrap();
    services
        .scheduling
        .satisfy_current_step_preconditions(job.id, step_id)
        .await
        .unwrap();

    (
        connection.websocket,
        server_task,
        PreparedSession {
            boot_nonce,
            fingerprint,
            job_id: job.id,
            step_id,
            endpoint_id,
            runtime_credential,
        },
    )
}

/// Commits one destructive `Attempt{Dispatched}` for `job_id`/`step_id` and
/// transmits `ActionDispatch` for it — the local transport routes it through
/// whichever session is *currently* selected for `endpoint_id` at the exact
/// moment this is called, which is the point of keeping this separate from
/// [`connect_and_prepare`]: a test can connect a second overlapping session
/// first, so dispatch provably routes through the newer one.
async fn commit_and_dispatch(
    services: &Services,
    harness: &Harness,
    job_id: JobId,
    step_id: JobStepId,
    endpoint_id: EndpointId,
) -> Attempt {
    let dispatch_service = FinalDispatchService::new(
        Arc::clone(&services.job_repo),
        Arc::clone(&harness.presence),
        Arc::clone(&services.target) as Arc<dyn TargetRevalidationPort>,
        Arc::clone(&harness.dispatch_arbiter),
    );
    let FinalDispatchResult::Committed {
        outcome,
        reservation,
    } = dispatch_service
        .commit_destructive_dispatch(job_id, step_id, network_claims())
        .await
        .unwrap()
    else {
        panic!("expected a successful final-dispatch commitment")
    };

    let action_dispatch_service = ActionDispatchService::new(
        Arc::clone(&harness.reservations),
        Arc::clone(&harness.outbound) as Arc<dyn AgentDispatchPort>,
    );
    let outcome_send = action_dispatch_service
        .dispatch(endpoint_id, outcome.attempt, reservation)
        .await;
    assert!(
        matches!(outcome_send, ActionDispatchOutcome::Sent),
        "expected the local transport to accept the frame, got {outcome_send:?}"
    );

    outcome.attempt
}

/// Establishes the first real WSS session, commits one destructive
/// `Attempt{Dispatched}`, transmits `ActionDispatch` for it, and returns both
/// the live `websocket`/`server_task` and the [`Fixture`] context needed to
/// reconnect later — mirrors `job_cancellation_wss.rs::establish_and_dispatch`.
/// Composes [`connect_and_prepare`] + [`commit_and_dispatch`] for the common
/// single-session case; a test needing overlapping sessions calls those two
/// helpers directly instead.
#[allow(clippy::too_many_arguments)]
async fn establish_and_dispatch(
    services: &Services,
    harness: &Harness,
    issuer: &TrustedBootstrapFixtureIssuer,
    cert_der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
    addr: SocketAddr,
    inventory_signal: &str,
) -> (
    ClientWs,
    JoinHandle<Result<(), bamep_server::adapters::agent_gateway::AgentGatewayError>>,
    Fixture,
) {
    let (websocket, server_task, prepared) = connect_and_prepare(
        services,
        harness,
        issuer,
        cert_der,
        key_der,
        addr,
        inventory_signal,
        1,
    )
    .await;
    let attempt = commit_and_dispatch(
        services,
        harness,
        prepared.job_id,
        prepared.step_id,
        prepared.endpoint_id,
    )
    .await;

    (
        websocket,
        server_task,
        Fixture {
            boot_nonce: prepared.boot_nonce,
            fingerprint: prepared.fingerprint,
            job_id: prepared.job_id,
            step_id: prepared.step_id,
            endpoint_id: prepared.endpoint_id,
            attempt,
            runtime_credential: prepared.runtime_credential,
        },
    )
}

/// Re-establishes a *new* real WSS session for the already-enrolled Endpoint
/// this `credential_wire` currently authenticates (ADR-0012 reconnect: the
/// rotated `runtime_credential` from the prior `SessionEstablished`), against
/// a freshly bound listener served by `harness.gateway` (same-boot, no fresh
/// `BootstrapEvidence` required — `m0-agent-protocol-contract.md`: "same-boot
/// reconnect does not require evidence to be resent").
#[allow(clippy::too_many_arguments)]
async fn reconnect(
    harness: &Harness,
    issuer: &TrustedBootstrapFixtureIssuer,
    cert_der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
    addr: SocketAddr,
    boot_nonce: BootNonce,
    fingerprint: ServerCertFingerprint,
    credential_wire: &str,
) -> (
    ClientWs,
    JoinHandle<Result<(), bamep_server::adapters::agent_gateway::AgentGatewayError>>,
    String,
) {
    let listener = TcpListener::bind(addr).await.expect("bind");
    let bound_addr = listener.local_addr().unwrap();
    let acceptor = AgentTransportAcceptor::new(vec![cert_der], key_der).expect("build acceptor");
    let gateway_for_task = Arc::clone(&harness.gateway);
    let server_task: JoinHandle<
        Result<(), bamep_server::adapters::agent_gateway::AgentGatewayError>,
    > = tokio::spawn(async move {
        let (tcp_stream, _peer) = listener.accept().await.expect("accept tcp");
        let mut connection = acceptor.accept(tcp_stream).await.expect("tls+ws accept");
        let HandshakeOutcome::Established(session) = gateway_for_task
            .handshake(&mut connection.websocket)
            .await?
        else {
            panic!("handshake must establish for a still-valid rotated credential")
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

    let SimulatorHandshakeOutcome::Established(established) =
        bamep_simulator::authenticate(&mut connection.websocket, credential_wire)
            .await
            .expect("handshake helper must not error")
    else {
        panic!("rotated credential must re-establish a session")
    };
    let runtime_credential = established.body.runtime_credential.clone();

    (connection.websocket, server_task, runtime_credential)
}

// ---------------------------------------------------------------------
// Disconnect / reconnect: terminal reconciliation
// ---------------------------------------------------------------------

#[tokio::test]
async fn disconnect_then_reconnect_status_query_and_report_reach_terminal_over_real_wss() {
    let db = TestDatabase::setup().await;
    let (cert_der, key_der) = generate_test_cert("localhost");
    let cert_der_2 = cert_der.clone();
    let key_der_2 = key_der.clone_key();
    let issuer = TrustedBootstrapFixtureIssuer::from_seed([0x81; 32]);
    let services = build_services(db.pool.clone());
    let harness = build_harness(db.pool.clone(), &services, &issuer);

    let (mut websocket, server_task, fixture) = establish_and_dispatch(
        &services,
        &harness,
        &issuer,
        cert_der,
        key_der,
        "127.0.0.1:0".parse().unwrap(),
        "wss-reconcile-disconnect-reconnect",
    )
    .await;

    let AgentProtocolMessage::ActionDispatch(dispatch) = recv_agent_message(&mut websocket).await
    else {
        panic!("expected ActionDispatch")
    };
    let agent = SimulatedActionAgent::new()
        .with_default_scenario(bamep_simulator::ScenarioOutcome::AcceptThenSucceed);
    for message in agent.handle_dispatch(&dispatch) {
        send_agent_message(&mut websocket, message).await;
    }
    // Execute locally but never transmit the terminal ActionResult — the
    // Server still sees an active, now-disconnecting Attempt.
    let _withheld = agent
        .run_configured_scenario(dispatch.body.action_id)
        .unwrap();

    // Connection loss: a clean Close still triggers the Gateway's
    // connection-loss reconciliation hook — the disconnect trigger governs
    // "no longer connected while active", not close-frame cleanliness.
    websocket.close(None).await.unwrap();
    server_task.await.unwrap().unwrap();

    assert_eq!(
        attempt_state(&db.pool, fixture.attempt.id.0).await,
        "AwaitingReconciliation"
    );

    // Reconnect using the rotated runtime_credential from the first session
    // (ADR-0012 reconnect) — a fresh real WSS session for the same Endpoint.
    let (mut websocket2, server_task2, _rotated_again) = reconnect(
        &harness,
        &issuer,
        cert_der_2,
        key_der_2,
        "127.0.0.1:0".parse().unwrap(),
        fixture.boot_nonce,
        fixture.fingerprint,
        &fixture.runtime_credential,
    )
    .await;

    // Session establishment must have triggered StatusQuery for the exact
    // existing action_id — never a fresh ActionDispatch, never a replacement
    // identity.
    let AgentProtocolMessage::StatusQuery(query) = recv_agent_message(&mut websocket2).await else {
        panic!("expected StatusQuery")
    };
    assert_eq!(query.body.action_id, dispatch.body.action_id);

    // The same Agent instance still retains its local action state (this is
    // an ordinary reconnect, not an Agent restart) and reports the real
    // completed outcome.
    let report = agent.handle_status_query(&query);
    assert_eq!(report.body.known_state, KnownActionState::Succeeded);
    send_agent_message(&mut websocket2, AgentProtocolMessage::StatusReport(report)).await;

    websocket2.close(None).await.unwrap();
    server_task2.await.unwrap().unwrap();

    assert_eq!(
        attempt_state(&db.pool, fixture.attempt.id.0).await,
        "Succeeded"
    );
    let (step_state, _) = job_step_row(&db.pool, fixture.step_id).await;
    assert_eq!(step_state, "Succeeded");
    assert_eq!(job_state_text(&db.pool, fixture.job_id).await, "Succeeded");
    assert_eq!(harness.reservations.take(fixture.attempt.id), None);

    db.teardown().await;
}

// ---------------------------------------------------------------------
// Agent restart: lost local state -> Unknown, then explicit Indeterminate
// ---------------------------------------------------------------------

#[tokio::test]
async fn agent_restart_status_query_returns_unknown_then_explicit_indeterminate_closes_it_over_real_wss(
) {
    let db = TestDatabase::setup().await;
    let (cert_der, key_der) = generate_test_cert("localhost");
    let cert_der_2 = cert_der.clone();
    let key_der_2 = key_der.clone_key();
    let issuer = TrustedBootstrapFixtureIssuer::from_seed([0x82; 32]);
    let services = build_services(db.pool.clone());
    let harness = build_harness(db.pool.clone(), &services, &issuer);

    let (mut websocket, server_task, fixture) = establish_and_dispatch(
        &services,
        &harness,
        &issuer,
        cert_der,
        key_der,
        "127.0.0.1:0".parse().unwrap(),
        "wss-reconcile-agent-restart",
    )
    .await;

    let AgentProtocolMessage::ActionDispatch(dispatch) = recv_agent_message(&mut websocket).await
    else {
        panic!("expected ActionDispatch")
    };
    // No ActionAck is ever sent — the committed `Dispatched` Attempt is
    // already reconciliation-relevant on its own.
    let _ = &dispatch;

    websocket.close(None).await.unwrap();
    server_task.await.unwrap().unwrap();
    assert_eq!(
        attempt_state(&db.pool, fixture.attempt.id.0).await,
        "AwaitingReconciliation"
    );

    let (mut websocket2, server_task2, _rotated) = reconnect(
        &harness,
        &issuer,
        cert_der_2,
        key_der_2,
        "127.0.0.1:0".parse().unwrap(),
        fixture.boot_nonce,
        fixture.fingerprint,
        &fixture.runtime_credential,
    )
    .await;

    let AgentProtocolMessage::StatusQuery(query) = recv_agent_message(&mut websocket2).await else {
        panic!("expected StatusQuery")
    };
    assert_eq!(query.body.action_id, dispatch.body.action_id);

    // A fresh Agent-local instance stands in for loss of local state (Agent
    // restart) — a deterministic control hook, mirroring the same pattern
    // already used for CancelAck (Issue #27's `job_cancellation_wss.rs`).
    let restarted_agent = SimulatedActionAgent::new();
    let report = restarted_agent.handle_status_query(&query);
    assert_eq!(report.body.known_state, KnownActionState::Unknown);
    send_agent_message(&mut websocket2, AgentProtocolMessage::StatusReport(report)).await;

    websocket2.close(None).await.unwrap();
    server_task2.await.unwrap().unwrap();

    // Unknown never proves non-execution and never fabricates Indeterminate
    // on its own.
    assert_eq!(
        attempt_state(&db.pool, fixture.attempt.id.0).await,
        "AwaitingReconciliation"
    );

    // Only the explicit, structurally separate operator control path may
    // close it Indeterminate.
    let close_result = harness
        .reconciliation
        .close_indeterminate(
            fixture.job_id,
            bamep_domain::Actor::Operator {
                label: "reconciliation-wss-operator".into(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(close_result, CloseIndeterminateResult::Applied(_)));

    assert_eq!(
        attempt_state(&db.pool, fixture.attempt.id.0).await,
        "Indeterminate"
    );
    let (step_state, reason) = job_step_row(&db.pool, fixture.step_id).await;
    assert_eq!(step_state, "Failed");
    assert_eq!(reason.as_deref(), Some("ReconciliationIndeterminate"));
    assert_eq!(job_state_text(&db.pool, fixture.job_id).await, "Failed");
    assert_eq!(harness.reservations.take(fixture.attempt.id), None);

    db.teardown().await;
}

// ---------------------------------------------------------------------
// Server restart: persisted uncertain Attempts reconcile, never redispatch
// ---------------------------------------------------------------------

#[tokio::test]
async fn server_restart_then_reconnect_issues_status_query_and_reaches_terminal_over_real_wss() {
    let db = TestDatabase::setup().await;
    let (cert_der, key_der) = generate_test_cert("localhost");
    let cert_der_2 = cert_der.clone();
    let key_der_2 = key_der.clone_key();
    let issuer = TrustedBootstrapFixtureIssuer::from_seed([0x83; 32]);
    let services = build_services(db.pool.clone());
    let harness = build_harness(db.pool.clone(), &services, &issuer);

    let (mut websocket, server_task, fixture) = establish_and_dispatch(
        &services,
        &harness,
        &issuer,
        cert_der,
        key_der,
        "127.0.0.1:0".parse().unwrap(),
        "wss-reconcile-server-restart",
    )
    .await;

    let AgentProtocolMessage::ActionDispatch(dispatch) = recv_agent_message(&mut websocket).await
    else {
        panic!("expected ActionDispatch")
    };
    let agent = SimulatedActionAgent::new()
        .with_default_scenario(bamep_simulator::ScenarioOutcome::AcceptThenFail);
    for message in agent.handle_dispatch(&dispatch) {
        send_agent_message(&mut websocket, message).await;
    }
    let _withheld = agent
        .run_configured_scenario(dispatch.body.action_id)
        .unwrap();

    // The connection is still nominally open when the Server "restarts":
    // every in-memory Runtime Service is discarded and rebuilt from
    // scratch, sharing only the same durable PostgreSQL pool — proving
    // restart recovery never depends on the old session/task still running.
    // The still-Dispatched Attempt (no connection-loss trigger ever fired
    // for it) is exactly the case the restart sweep must catch.
    assert_eq!(
        attempt_state(&db.pool, fixture.attempt.id.0).await,
        "Dispatched"
    );
    let restarted_harness = build_harness(db.pool.clone(), &services, &issuer);
    let reconciled = restarted_harness
        .reconciliation
        .reconcile_on_startup()
        .await
        .unwrap();
    assert_eq!(reconciled, vec![fixture.attempt.id]);
    assert_eq!(
        attempt_state(&db.pool, fixture.attempt.id.0).await,
        "AwaitingReconciliation"
    );
    // No second Attempt, no ActionDispatch resend — only a state read/write.
    server_task.abort();

    // The Endpoint reconnects against the restarted harness's own Gateway —
    // "Do NOT require an Agent to already be connected at Server startup".
    let (mut websocket2, server_task2, _rotated) = reconnect(
        &restarted_harness,
        &issuer,
        cert_der_2,
        key_der_2,
        "127.0.0.1:0".parse().unwrap(),
        fixture.boot_nonce,
        fixture.fingerprint,
        &fixture.runtime_credential,
    )
    .await;

    let AgentProtocolMessage::StatusQuery(query) = recv_agent_message(&mut websocket2).await else {
        panic!("expected StatusQuery")
    };
    assert_eq!(query.body.action_id, dispatch.body.action_id);

    let report = agent.handle_status_query(&query);
    assert_eq!(report.body.known_state, KnownActionState::Failed);
    send_agent_message(&mut websocket2, AgentProtocolMessage::StatusReport(report)).await;

    websocket2.close(None).await.unwrap();
    server_task2.await.unwrap().unwrap();

    assert_eq!(
        attempt_state(&db.pool, fixture.attempt.id.0).await,
        "Failed"
    );
    let (step_state, reason) = job_step_row(&db.pool, fixture.step_id).await;
    assert_eq!(step_state, "Failed");
    assert_eq!(reason.as_deref(), Some("ExecutionFailed"));
    assert_eq!(job_state_text(&db.pool, fixture.job_id).await, "Failed");
    // The stale in-memory reservation mapping from the pre-restart harness
    // is gone; the restarted harness's own (fresh) registry never held one
    // either — absence must never corrupt the durable Attempt lifecycle.
    assert_eq!(
        restarted_harness.reservations.take(fixture.attempt.id),
        None
    );

    db.teardown().await;
}

// ---------------------------------------------------------------------
// Session-loss reconciliation with overlapping sessions (Issue #28
// corrective pass)
// ---------------------------------------------------------------------

#[tokio::test]
async fn older_superseded_session_disconnecting_does_not_disturb_the_newer_dispatch_relevant_session(
) {
    let db = TestDatabase::setup().await;
    let (cert_der, key_der) = generate_test_cert("localhost");
    let cert_der_2 = cert_der.clone();
    let key_der_2 = key_der.clone_key();
    let issuer = TrustedBootstrapFixtureIssuer::from_seed([0x84; 32]);
    let services = build_services(db.pool.clone());
    let harness = build_harness(db.pool.clone(), &services, &issuer);

    // Session A connects first...
    let (mut websocket_a, server_task_a, prepared) = connect_and_prepare(
        &services,
        &harness,
        &issuer,
        cert_der,
        key_der,
        "127.0.0.1:0".parse().unwrap(),
        "wss-reconcile-older-superseded",
        1,
    )
    .await;

    // ...then session B connects for the SAME Endpoint before any dispatch
    // has happened, becoming the newer/currently-selected session.
    let (mut websocket_b, server_task_b, _rotated) = reconnect(
        &harness,
        &issuer,
        cert_der_2,
        key_der_2,
        "127.0.0.1:0".parse().unwrap(),
        prepared.boot_nonce,
        prepared.fingerprint,
        &prepared.runtime_credential,
    )
    .await;

    // Dispatch happens only now — it must route through B, the currently
    // selected session, never through A.
    let attempt = commit_and_dispatch(
        &services,
        &harness,
        prepared.job_id,
        prepared.step_id,
        prepared.endpoint_id,
    )
    .await;

    let AgentProtocolMessage::ActionDispatch(dispatch) = recv_agent_message(&mut websocket_b).await
    else {
        panic!("expected ActionDispatch on session B")
    };
    let expected_action_id =
        bamep_agent_protocol::ProtocolId::from_uuid(attempt.action_id.0).unwrap();
    assert_eq!(dispatch.body.action_id, expected_action_id);

    // Session A — older, superseded, never carried this Attempt's traffic —
    // disconnects. This must NOT move the active Attempt to
    // AwaitingReconciliation.
    websocket_a.close(None).await.unwrap();
    server_task_a.await.unwrap().unwrap();

    assert_eq!(
        attempt_state(&db.pool, attempt.id.0).await,
        "Dispatched",
        "an unrelated older session's disconnect must never move the active Attempt"
    );

    // Normal terminal evidence from B — the real dispatch-relevant session —
    // still completes the Attempt normally.
    let agent = SimulatedActionAgent::new()
        .with_default_scenario(bamep_simulator::ScenarioOutcome::AcceptThenSucceed);
    for message in agent.handle_dispatch(&dispatch) {
        send_agent_message(&mut websocket_b, message).await;
    }
    for message in agent
        .run_configured_scenario(dispatch.body.action_id)
        .unwrap()
    {
        send_agent_message(&mut websocket_b, message).await;
    }

    websocket_b.close(None).await.unwrap();
    server_task_b.await.unwrap().unwrap();

    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "Succeeded");
    let (step_state, _) = job_step_row(&db.pool, prepared.step_id).await;
    assert_eq!(step_state, "Succeeded");
    assert_eq!(job_state_text(&db.pool, prepared.job_id).await, "Succeeded");

    db.teardown().await;
}

#[tokio::test]
async fn dispatch_relevant_session_disconnecting_while_another_session_remains_live_triggers_reconciliation_and_reuses_it(
) {
    let db = TestDatabase::setup().await;
    let (cert_der, key_der) = generate_test_cert("localhost");
    let cert_der_2 = cert_der.clone();
    let key_der_2 = key_der.clone_key();
    let issuer = TrustedBootstrapFixtureIssuer::from_seed([0x85; 32]);
    let services = build_services(db.pool.clone());
    let harness = build_harness(db.pool.clone(), &services, &issuer);

    // Session A connects, and dispatch happens through it — A is the only
    // live session at that point.
    let (mut websocket_a, server_task_a, prepared) = connect_and_prepare(
        &services,
        &harness,
        &issuer,
        cert_der,
        key_der,
        "127.0.0.1:0".parse().unwrap(),
        "wss-reconcile-current-session-loss",
        1,
    )
    .await;
    let attempt = commit_and_dispatch(
        &services,
        &harness,
        prepared.job_id,
        prepared.step_id,
        prepared.endpoint_id,
    )
    .await;

    let AgentProtocolMessage::ActionDispatch(dispatch) = recv_agent_message(&mut websocket_a).await
    else {
        panic!("expected ActionDispatch on session A")
    };
    let agent = SimulatedActionAgent::new()
        .with_default_scenario(bamep_simulator::ScenarioOutcome::AcceptThenSucceed);
    for message in agent.handle_dispatch(&dispatch) {
        send_agent_message(&mut websocket_a, message).await;
    }
    // Executed locally but withheld — the Server still sees an active
    // Attempt when A disconnects.
    let _withheld = agent
        .run_configured_scenario(dispatch.body.action_id)
        .unwrap();

    // Session B connects for the same Endpoint while A is still live — an
    // ordinary second authenticated session; no traffic has been sent
    // through it yet.
    let (mut websocket_b, server_task_b, _rotated) = reconnect(
        &harness,
        &issuer,
        cert_der_2,
        key_der_2,
        "127.0.0.1:0".parse().unwrap(),
        prepared.boot_nonce,
        prepared.fingerprint,
        &prepared.runtime_credential,
    )
    .await;

    // Session A — the actual dispatch-relevant session — disconnects.
    websocket_a.close(None).await.unwrap();
    server_task_a.await.unwrap().unwrap();

    assert_eq!(
        attempt_state(&db.pool, attempt.id.0).await,
        "AwaitingReconciliation"
    );

    // Session B, still live, is reused for StatusQuery of the exact
    // existing action_id — never a fresh ActionDispatch/replacement
    // identity.
    let AgentProtocolMessage::StatusQuery(query) = recv_agent_message(&mut websocket_b).await
    else {
        panic!("expected StatusQuery on the remaining live session B")
    };
    assert_eq!(query.body.action_id, dispatch.body.action_id);

    let report = agent.handle_status_query(&query);
    assert_eq!(report.body.known_state, KnownActionState::Succeeded);
    send_agent_message(&mut websocket_b, AgentProtocolMessage::StatusReport(report)).await;

    websocket_b.close(None).await.unwrap();
    server_task_b.await.unwrap().unwrap();

    assert_eq!(attempt_state(&db.pool, attempt.id.0).await, "Succeeded");
    let (step_state, _) = job_step_row(&db.pool, prepared.step_id).await;
    assert_eq!(step_state, "Succeeded");
    assert_eq!(job_state_text(&db.pool, prepared.job_id).await, "Succeeded");

    db.teardown().await;
}

#[tokio::test]
async fn last_live_session_ending_clears_presence_and_outbound_readiness_over_real_wss() {
    let db = TestDatabase::setup().await;
    let (cert_der, key_der) = generate_test_cert("localhost");
    let issuer = TrustedBootstrapFixtureIssuer::from_seed([0x86; 32]);
    let services = build_services(db.pool.clone());
    let harness = build_harness(db.pool.clone(), &services, &issuer);

    let (mut websocket, server_task, fixture) = establish_and_dispatch(
        &services,
        &harness,
        &issuer,
        cert_der,
        key_der,
        "127.0.0.1:0".parse().unwrap(),
        "wss-reconcile-last-session-ends",
    )
    .await;

    let AgentProtocolMessage::ActionDispatch(_dispatch) = recv_agent_message(&mut websocket).await
    else {
        panic!("expected ActionDispatch")
    };
    assert!(harness.presence.is_present(fixture.endpoint_id));

    // The only live session for this Endpoint ends.
    websocket.close(None).await.unwrap();
    server_task.await.unwrap().unwrap();

    // Presence/outbound readiness are gone — the exact facts
    // `FinalDispatchService`'s destructive gate (precondition 2, transient
    // half; already exhaustively proven by
    // `crates/domain/src/final_dispatch.rs`'s own
    // `missing_agent_presence_fails_while_credential_remains_active`) and
    // `ReconciliationService::reconcile_on_session_start`'s `StatusQuery`
    // send both depend on. `run_authenticated_session` unregisters both
    // synchronously, with no `.await` in between, strictly before it ever
    // awaits `ReconciliationService` — so there is no stale-ready window a
    // concurrent final-dispatch attempt could observe, unlike before this
    // corrective pass.
    assert!(!harness.presence.is_present(fixture.endpoint_id));
    let status_result = harness
        .outbound
        .status_query(
            fixture.endpoint_id,
            bamep_agent_protocol::StatusQueryMessage::new(
                bamep_agent_protocol::ProtocolId::generate(),
            ),
        )
        .await;
    assert_eq!(
        status_result,
        Err(bamep_server::ports::AgentDispatchError::NoSession)
    );

    assert_eq!(
        attempt_state(&db.pool, fixture.attempt.id.0).await,
        "AwaitingReconciliation"
    );

    db.teardown().await;
}

#[tokio::test]
async fn cross_attempt_stale_dispatch_correlation_from_a_terminal_prior_attempt_never_disturbs_the_next_one(
) {
    // Issue #28 second corrective pass "Attempt-scoped session correlation":
    // Session A dispatches and completes Attempt 1 entirely on its own.
    // Session B then connects and stays live. The next JobStep's Attempt 2
    // is committed and dispatched through B — the currently selected live
    // session — while A is STILL live (not yet disconnecting). Only once
    // Attempt 2 is already Dispatched through B does A disconnect. A's own
    // disconnect-reconciliation trigger must observe that it only ever
    // carried Attempt 1's (now terminal) action_id, never Attempt 2's, and
    // must therefore never move Attempt 2 to AwaitingReconciliation — the
    // exact cross-Attempt race the first corrective pass's Endpoint-scoped
    // `last_sent_session` left open.
    let db = TestDatabase::setup().await;
    let (cert_der, key_der) = generate_test_cert("localhost");
    let cert_der_2 = cert_der.clone();
    let key_der_2 = key_der.clone_key();
    let issuer = TrustedBootstrapFixtureIssuer::from_seed([0x87; 32]);
    let services = build_services(db.pool.clone());
    let harness = build_harness(db.pool.clone(), &services, &issuer);

    // Session A connects and prepares a two-step Job.
    let (mut websocket_a, server_task_a, prepared) = connect_and_prepare(
        &services,
        &harness,
        &issuer,
        cert_der,
        key_der,
        "127.0.0.1:0".parse().unwrap(),
        "wss-reconcile-cross-attempt-stale-correlation",
        2,
    )
    .await;

    // Attempt 1 dispatches and completes entirely through session A.
    let attempt_1 = commit_and_dispatch(
        &services,
        &harness,
        prepared.job_id,
        prepared.step_id,
        prepared.endpoint_id,
    )
    .await;
    let AgentProtocolMessage::ActionDispatch(dispatch_1) =
        recv_agent_message(&mut websocket_a).await
    else {
        panic!("expected ActionDispatch for Attempt 1 on session A")
    };
    let agent = SimulatedActionAgent::new()
        .with_default_scenario(bamep_simulator::ScenarioOutcome::AcceptThenSucceed);
    for message in agent.handle_dispatch(&dispatch_1) {
        send_agent_message(&mut websocket_a, message).await;
    }
    for message in agent
        .run_configured_scenario(dispatch_1.body.action_id)
        .unwrap()
    {
        send_agent_message(&mut websocket_a, message).await;
    }
    // Session A stays open/live past this point (it must still be the exact
    // session whose disconnect is tested below), so there is no
    // `server_task_a.await` synchronization point yet to confirm the Gateway
    // has actually applied this terminal evidence — poll instead.
    wait_for_attempt_state(&db.pool, attempt_1.id.0, "Succeeded").await;

    // Session B connects for the same Endpoint while A remains live.
    let (mut websocket_b, server_task_b, _rotated) = reconnect(
        &harness,
        &issuer,
        cert_der_2,
        key_der_2,
        "127.0.0.1:0".parse().unwrap(),
        prepared.boot_nonce,
        prepared.fingerprint,
        &prepared.runtime_credential,
    )
    .await;

    // The next JobStep becomes eligible.
    let job = services
        .job_repo
        .find_job(prepared.job_id)
        .await
        .unwrap()
        .expect("job must exist");
    let step_2 = job.steps[1].id;
    services
        .intents
        .authorize(prepared.job_id, step_2)
        .await
        .unwrap();
    services
        .scheduling
        .satisfy_current_step_preconditions(prepared.job_id, step_2)
        .await
        .unwrap();

    // The actual race this test exists to close, made deterministic rather
    // than left to scheduler luck: Attempt 2 is committed DURABLY to
    // PostgreSQL first — via `FinalDispatchService` directly, deliberately
    // WITHOUT yet calling `ActionDispatchService::dispatch` — so the Runtime
    // `OutboundSessionDirectory` correlation map still says "(session A,
    // Attempt 1's action_id)" at this exact point (nothing has dispatched
    // Attempt 2 through it yet), while PostgreSQL's current active Attempt
    // for this Endpoint is now genuinely Attempt 2. This is exactly the
    // window the real production race can land in: `OutboundSessionDirectory`
    // only records a new dispatch correlation once `ActionDispatchService`
    // actually calls it, which always happens strictly AFTER the durable
    // commit, never before or atomically with it.
    let dispatch_service = FinalDispatchService::new(
        Arc::clone(&services.job_repo),
        Arc::clone(&harness.presence),
        Arc::clone(&services.target) as Arc<dyn TargetRevalidationPort>,
        Arc::clone(&harness.dispatch_arbiter),
    );
    let FinalDispatchResult::Committed {
        outcome,
        reservation,
    } = dispatch_service
        .commit_destructive_dispatch(prepared.job_id, step_2, network_claims())
        .await
        .unwrap()
    else {
        panic!("expected a successful final-dispatch commitment for Attempt 2")
    };
    let attempt_2 = outcome.attempt;
    let action_id_2 = bamep_agent_protocol::ProtocolId::from_uuid(attempt_2.action_id.0).unwrap();
    assert_ne!(dispatch_1.body.action_id, action_id_2);

    // Session A — which only ever carried Attempt 1's now-terminal
    // action_id in the still-stale Runtime correlation map — disconnects
    // now, exactly inside that window. This must NOT move the already-
    // durably-committed Attempt 2 to AwaitingReconciliation: the decide-
    // closure `mark_endpoint_uncertain` threads through the Adapter's lock
    // locks Attempt 2 (PostgreSQL's genuine current candidate) and finds its
    // `action_id` does not match the `action_id` A's disconnect captured, so
    // it safely no-ops.
    websocket_a.close(None).await.unwrap();
    server_task_a.await.unwrap().unwrap();

    assert_eq!(
        attempt_state(&db.pool, attempt_2.id.0).await,
        "Dispatched",
        "an older session's disconnect must never disturb a later Attempt it never carried, \
         even when that Attempt was already durably committed before the disconnect ran"
    );

    // Only now does `ActionDispatch` for Attempt 2 actually transmit through
    // B, the currently selected live session — never through A, and never a
    // second Attempt/commitment.
    let action_dispatch_service = ActionDispatchService::new(
        Arc::clone(&harness.reservations),
        Arc::clone(&harness.outbound) as Arc<dyn AgentDispatchPort>,
    );
    let send_outcome = action_dispatch_service
        .dispatch(prepared.endpoint_id, attempt_2, reservation)
        .await;
    assert!(
        matches!(send_outcome, ActionDispatchOutcome::Sent),
        "expected the local transport to accept the frame, got {send_outcome:?}"
    );

    let AgentProtocolMessage::ActionDispatch(dispatch_2) =
        recv_agent_message(&mut websocket_b).await
    else {
        panic!("expected ActionDispatch for Attempt 2 on session B")
    };
    assert_eq!(dispatch_2.body.action_id, action_id_2);

    // Normal evidence from B — the real dispatch-relevant session for
    // Attempt 2 — completes it normally; B receives no unsolicited
    // StatusQuery in between (the very next frame it observes is exactly the
    // evidence this test drives).
    for message in agent.handle_dispatch(&dispatch_2) {
        send_agent_message(&mut websocket_b, message).await;
    }
    for message in agent
        .run_configured_scenario(dispatch_2.body.action_id)
        .unwrap()
    {
        send_agent_message(&mut websocket_b, message).await;
    }

    websocket_b.close(None).await.unwrap();
    server_task_b.await.unwrap().unwrap();

    assert_eq!(attempt_state(&db.pool, attempt_2.id.0).await, "Succeeded");
    let (step_state, _) = job_step_row(&db.pool, step_2).await;
    assert_eq!(step_state, "Succeeded");
    assert_eq!(job_state_text(&db.pool, prepared.job_id).await, "Succeeded");
    // No second Attempt/ActionDispatch was ever created for either JobStep.
    assert_eq!(
        harness.reconciliation.reconcile_on_startup().await.unwrap(),
        Vec::new(),
        "no Attempt was ever left Dispatched/InProgress for a restart sweep to find"
    );

    db.teardown().await;
}
