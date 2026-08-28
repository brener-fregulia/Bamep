//! Issue #38 "Runtime vertical test": proves the narrowest meaningful
//! control-path slice explicitly required by the Work Package —
//!
//! ```text
//! authenticated Agent (real in-memory WSS)
//!     -> TransferAuthorizationRequest
//!     -> bamepd decision (real PostgreSQL durable state)
//!     -> TransferAuthorizationGrant{token}
//!     -> Worker AuthorizationQuery over a real Unix Domain Socket
//!     -> bamepd's authoritative AuthorizationDecision
//! ```
//!
//! using the exact same `TransferAuthorizationService`/`CapabilityStore`
//! instance both boundaries share in production (`bamepd`'s composition
//! root, `crates/server/src/bin/bamepd.rs`), so this test proves the same
//! durable authorization state — and the same in-memory issued capability —
//! actually drives both the Agent-facing grant and the Worker-facing
//! decision. Deliberately does not cross into actual chunk HTTPS (#39):
//! this test carries no bulk bytes and issues no HTTP request at all.
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

#![cfg(unix)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use bamep_agent_protocol::{
    decode, encode, AgentProtocolMessage, AuthRequestMessage, ProtocolId,
    TransferAuthorizationRequestMessage,
};
use bamep_domain::{
    ArtifactId, ChunkSize, DigestAlgorithm, EndpointId, SourceProvenance, TransferDirection,
    TransferId,
};
use bamep_server::adapters::agent_gateway::{AgentControlGateway, HandshakeOutcome};
use bamep_server::adapters::postgres::{
    PostgresBootContextRepository, PostgresCredentialRedemptionRepository,
    PostgresEndpointRepository, PostgresJobRepository, PostgresTransferAuthorizationRepository,
    PostgresTransferRepository,
};
use bamep_server::adapters::worker_control_plane::WorkerControlPlane;
use bamep_server::application::{
    BootOrchestrationService, EnrollmentService, RedeemResult, TransferAuthorizationService,
    TransferDispatchResult, TransferDispatchService, TransferService,
};
use bamep_server::runtime::capability_store::CapabilityStore;
use bamep_server::runtime::replay_cache::ReplayCache;
use bamep_server::runtime::resource_arbiter::{
    ResourceClaim, ResourceKind, TechnicalResourceArbiter,
};
use bamep_server::runtime::worker_authority::WorkerAuthorityRegistry;
use bamep_worker_protocol::{
    receive, send, AuthorizationDecisionOutcome, AuthorizationQueryMessage, ServerHelloMessage,
    WorkerHelloMessage, WorkerProtocolMessage,
};
use chrono::Utc;
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use sqlx::PgPool;
use support::TestDatabase;
use tokio::net::UnixStream;
use tokio::sync::watch;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use uuid::Uuid;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
const DATA_PLANE_BASE_URL: &str = "https://server.example:8443";

type Enrollment =
    EnrollmentService<PostgresEndpointRepository, PostgresCredentialRedemptionRepository>;
type Gateway =
    AgentControlGateway<PostgresEndpointRepository, PostgresCredentialRedemptionRepository>;

struct TempSocketPath(std::path::PathBuf);

impl TempSocketPath {
    fn fresh() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "bamep-transfer-authorization-vertical-{}",
            Uuid::new_v4()
        ));
        Self(dir.join("worker.sock"))
    }
}

impl Drop for TempSocketPath {
    fn drop(&mut self) {
        if let Some(parent) = self.0.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}

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

struct DispatchedTransfer {
    endpoint_id: EndpointId,
    transfer_id: TransferId,
    artifact_id: ArtifactId,
    action_id: ProtocolId,
}

async fn dispatched_transfer_fixture(pool: &PgPool, signal: &str) -> DispatchedTransfer {
    let boot = BootOrchestrationService::new(
        Arc::new(PostgresBootContextRepository::new(pool.clone())),
        chrono::Duration::minutes(5),
    );
    let enrollment: Enrollment = EnrollmentService::new(
        Arc::new(PostgresEndpointRepository::new(pool.clone())),
        Arc::new(PostgresCredentialRedemptionRepository::new(pool.clone())),
    );
    let job_repo = Arc::new(PostgresJobRepository::new(pool.clone()));
    let jobs = bamep_server::application::JobService::new(Arc::clone(&job_repo));
    let scheduling = bamep_server::application::JobSchedulingService::new(Arc::clone(&job_repo));
    let transfers = TransferService::new(Arc::new(PostgresTransferRepository::new(pool.clone())));
    let arbiter = Arc::new(TechnicalResourceArbiter::new([(
        ResourceKind::new("network"),
        10,
    )]));
    let dispatch = TransferDispatchService::new(Arc::clone(&job_repo), Arc::clone(&arbiter));
    let evidence = bamep_server::application::ActionEvidenceService::new(
        Arc::clone(&job_repo) as Arc<dyn bamep_server::ports::JobRepository>,
        Arc::new(bamep_server::runtime::reservation_registry::AttemptReservationRegistry::new()),
        arbiter,
    );

    let now = Utc::now();
    let boot_nonce = bamep_domain::BootNonce::generate().expect("OS CSPRNG must be available");
    let credential = boot
        .issue_enrollment_credential(signal, boot_nonce, now)
        .await
        .expect("issuance must succeed");
    let RedeemResult::Established { endpoint_id, .. } = enrollment
        .redeem(&credential.to_wire_value())
        .await
        .unwrap()
    else {
        panic!("first contact must establish a session");
    };
    enrollment
        .approve_enrollment(
            endpoint_id,
            bamep_domain::Actor::Operator {
                label: "transfer-authorization-vertical-harness".into(),
            },
            now,
        )
        .await
        .unwrap();

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

    // `m0-agent-protocol-contract.md` "Transfer authorization": the request
    // is valid only after `ActionAck{outcome: Accepted}` — process it so the
    // Attempt is durably `InProgress` before the WSS authorization request.
    evidence
        .apply(
            action_id,
            endpoint_id,
            bamep_domain::ActionEvidence::AckAccepted,
        )
        .await
        .expect("ActionAck{Accepted} advances the Attempt to InProgress");

    DispatchedTransfer {
        endpoint_id,
        transfer_id: context.transfer.id,
        artifact_id: context.transfer.artifact_id,
        action_id,
    }
}

/// The whole control path in one test: the Agent WSS side (`AgentControlGateway`
/// + real `AuthRequest`/handshake) issues a capability through the exact same
/// `TransferAuthorizationService`/`CapabilityStore` the Worker UDS side
/// (`WorkerControlPlane`) later consumes it through — exactly as `bamepd`'s
/// own composition root wires them (`crates/server/src/bin/bamepd.rs`).
#[tokio::test]
async fn agent_issued_capability_is_approved_by_a_real_worker_uds_query() {
    let db = TestDatabase::setup().await;
    let fixture = dispatched_transfer_fixture(&db.pool, "vertical-01").await;

    // The one shared service instance both boundaries below consume.
    let transfer_authorization = Arc::new(TransferAuthorizationService::new(
        Arc::new(PostgresTransferAuthorizationRepository::new(
            db.pool.clone(),
        )),
        Arc::new(CapabilityStore::new()),
        Arc::new(ReplayCache::new()),
        DATA_PLANE_BASE_URL,
    ));

    // --- Agent WSS side: real handshake, then TransferAuthorizationRequest ---
    let enrollment = Arc::new(EnrollmentService::new(
        Arc::new(PostgresEndpointRepository::new(db.pool.clone())),
        Arc::new(PostgresCredentialRedemptionRepository::new(db.pool.clone())),
    ));
    let boot = BootOrchestrationService::new(
        Arc::new(PostgresBootContextRepository::new(db.pool.clone())),
        chrono::Duration::minutes(5),
    );
    let signer = bamep_trusted_bootstrap::fixture::FixtureAssertionSigner::from_seed([13; 32]);
    let bootstrap_evidence = Arc::new(bamep_server::application::BootstrapEvidenceService::new(
        Arc::new(PostgresEndpointRepository::new(db.pool.clone())),
        bamep_trusted_bootstrap::AcceptedSiteKeys::single(signer.public_key()),
    ));
    let gateway: Arc<Gateway> = Arc::new(
        Gateway::new(Arc::clone(&enrollment))
            .with_bootstrap_evidence_service(bootstrap_evidence)
            .with_transfer_authorization_service(Arc::clone(&transfer_authorization)),
    );

    // A second, independent credential for the same already-enrolled
    // Endpoint — the fixture already established/approved it; this session
    // only needs to authenticate as that Endpoint to exercise the WSS path.
    let boot_nonce = bamep_domain::BootNonce::generate().unwrap();
    let e1 = boot
        .issue_enrollment_credential("vertical-01", boot_nonce, Utc::now())
        .await
        .unwrap();

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
        panic!("expected Established");
    };
    assert_eq!(session.endpoint_id, fixture.endpoint_id);
    let _ = recv_message(&mut client_ws).await; // SessionEstablished

    let server_gateway = Arc::clone(&gateway);
    let gateway_task = tokio::spawn(async move {
        server_gateway
            .run_authenticated_session(
                &mut server_ws,
                session,
                bamep_trusted_bootstrap::ServerCertFingerprint::from_sha256_digest([7; 32]),
            )
            .await
    });

    let transfer_id_wire =
        ProtocolId::from_uuid(fixture.transfer_id.0).expect("TransferId is always a UUID v4");
    let signing_key = SigningKey::from_bytes(&rand::random());
    let public_key =
        bamep_domain::ProofPublicKey::from_bytes(signing_key.verifying_key().to_bytes()).unwrap();
    let request = TransferAuthorizationRequestMessage::new(
        fixture.action_id,
        transfer_id_wire,
        public_key.to_wire_value(),
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
    let token = grant.body.token.clone();
    assert!(!token.is_empty());

    client_ws.close(None).await.unwrap();
    gateway_task.await.unwrap().unwrap();

    // --- Worker UDS side: the exact granted token is authoritatively approved ---
    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let chunk_acceptance = Arc::new(bamep_server::application::ChunkAcceptanceService::new(
        Arc::new(PostgresTransferRepository::new(db.pool.clone())),
    ));
    let control_plane_task = tokio::spawn(plane.run(
        registry,
        Arc::clone(&transfer_authorization),
        chunk_acceptance,
        shutdown_rx,
    ));

    let mut worker_stream = UnixStream::connect(&socket.0).await.expect("connect");
    let hello = WorkerHelloMessage::new(Uuid::new_v4());
    let sent_id = hello.envelope.message_id;
    send(
        &mut worker_stream,
        &WorkerProtocolMessage::WorkerHello(hello),
    )
    .await
    .expect("send WorkerHello");
    match timeout(TEST_TIMEOUT, receive(&mut worker_stream))
        .await
        .expect("no timeout")
        .expect("receive ServerHello")
    {
        WorkerProtocolMessage::ServerHello(ServerHelloMessage { body, .. }) => {
            assert_eq!(body.in_reply_to, sent_id);
            assert!(body.compatible);
        }
        other => panic!("expected ServerHello, got {other:?}"),
    }

    let capability_id = bamep_domain::CapabilityId::from_token_bytes(token.as_bytes());
    let proof_id = bamep_domain::ProofId::generate();
    let issued_at_millis = Utc::now().timestamp_millis() as u64;
    let transcript_fields = bamep_domain::ProofTranscriptFields {
        operation: bamep_domain::AuthorizationOperation::ChunkUpload,
        transfer_id: fixture.transfer_id,
        artifact_id: fixture.artifact_id,
        direction: TransferDirection::AgentToServer,
        chunk_index: Some(0),
        proof_id,
        issued_at_millis,
    };
    let transcript = bamep_domain::build_proof_transcript(&capability_id, &transcript_fields);
    let signature = signing_key.sign(&transcript);
    let signature = bamep_domain::ProofSignature::from_bytes(signature.to_bytes());

    // `artifact_id`/`direction` are signed into the 137-byte transcript but
    // never carried on the v1 wire message — `bamepd` reconstructs them from
    // the capability binding it granted on the Agent WSS side.
    let query = AuthorizationQueryMessage::new(
        token,
        fixture.transfer_id.0,
        0,
        proof_id.to_wire_value(),
        issued_at_millis,
        signature.to_wire_value(),
    );
    let query_id = query.envelope.message_id;
    send(
        &mut worker_stream,
        &WorkerProtocolMessage::AuthorizationQuery(query),
    )
    .await
    .expect("send AuthorizationQuery");

    match timeout(TEST_TIMEOUT, receive(&mut worker_stream))
        .await
        .expect("no timeout")
        .expect("receive AuthorizationDecision")
    {
        WorkerProtocolMessage::AuthorizationDecision(decision) => {
            assert_eq!(decision.body.in_reply_to, query_id);
            assert_eq!(
                decision.body.decision,
                AuthorizationDecisionOutcome::Approved,
                "the exact capability the Agent WSS side granted must be approved by the \
                 Worker UDS side, driven by the same durable authorization state"
            );
        }
        other => panic!("expected AuthorizationDecision, got {other:?}"),
    }

    drop(worker_stream);
    control_plane_task.abort();
    db.teardown().await;
}
