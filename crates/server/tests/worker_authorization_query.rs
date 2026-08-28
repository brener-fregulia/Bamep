//! Issue #38 "Worker UDS" validation: `AuthorizationQuery`/
//! `AuthorizationDecision` over a *real* Unix Domain Socket against the real
//! `WorkerControlPlane` (`bamepd`-side) and a real, PostgreSQL-backed
//! `TransferAuthorizationService` — the same real external framing/message
//! shapes a genuine Worker process uses, not an in-process shortcut
//! (`docs/development/testing.md`; `m1-worker-data-plane-control-contract.md`
//! "Validation": "Contract tests exercise the real framing/message shapes").
//!
//! `worker_control_plane.rs` already proves #37's handshake/generation/socket
//! semantics with a fake always-denying authorization repository; this file
//! is the complementary #38 proof that a real signed proof against real
//! durable state is correctly approved/denied end to end over the wire.
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

#![cfg(unix)]

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bamep_agent_protocol::ProtocolId;
use bamep_domain::{
    ArtifactId, ChunkSize, DigestAlgorithm, EndpointId, SourceProvenance, TransferDirection,
    TransferId,
};
use bamep_server::adapters::postgres::{
    PostgresBootContextRepository, PostgresCredentialRedemptionRepository,
    PostgresEndpointRepository, PostgresJobRepository, PostgresTransferAuthorizationRepository,
    PostgresTransferRepository,
};
use bamep_server::adapters::worker_control_plane::WorkerControlPlane;
use bamep_server::application::{
    BootOrchestrationService, EnrollmentService, RedeemResult, TransferAuthorizationOutcome,
    TransferAuthorizationService, TransferDispatchResult, TransferDispatchService, TransferService,
};
use bamep_server::runtime::capability_store::CapabilityStore;
use bamep_server::runtime::replay_cache::ReplayCache;
use bamep_server::runtime::resource_arbiter::{
    ResourceClaim, ResourceKind, TechnicalResourceArbiter,
};
use bamep_server::runtime::transient_worker_operations::TransientOperationError;
use bamep_server::runtime::worker_authority::WorkerAuthorityRegistry;
use bamep_worker_protocol::{
    receive, send, AuthorizationDecisionOutcome, AuthorizationQueryMessage, ServerHelloMessage,
    WireDigestAlgorithm, WorkerHelloMessage, WorkerProtocolMessage,
};
use chrono::Utc;
use ed25519_dalek::{Signer, SigningKey};
use sqlx::PgPool;
use support::TestDatabase;
use tokio::net::UnixStream;
use tokio::sync::watch;
use tokio::time::timeout;
use uuid::Uuid;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
const DATA_PLANE_BASE_URL: &str = "https://server.example:8443";

struct TempSocketPath(std::path::PathBuf);

impl TempSocketPath {
    fn fresh() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "bamep-worker-authorization-query-tests-{}",
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
    let enrollment = EnrollmentService::new(
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
                label: "worker-authorization-query-harness".into(),
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

    // `m0-agent-protocol-contract.md` "Transfer authorization": valid only
    // after `ActionAck{outcome: Accepted}` — advance the Attempt to
    // `InProgress`.
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

fn build_authorization_service(pool: PgPool) -> Arc<TransferAuthorizationService> {
    Arc::new(TransferAuthorizationService::new(
        Arc::new(PostgresTransferAuthorizationRepository::new(pool)),
        Arc::new(CapabilityStore::new()),
        Arc::new(ReplayCache::new()),
        DATA_PLANE_BASE_URL,
    ))
}

async fn issue_capability(
    authorization: &TransferAuthorizationService,
    fixture: &DispatchedTransfer,
    signing_key: &SigningKey,
) -> String {
    let public =
        bamep_domain::ProofPublicKey::from_bytes(signing_key.verifying_key().to_bytes()).unwrap();
    let outcome = authorization
        .issue(
            fixture.endpoint_id,
            fixture.action_id,
            fixture.transfer_id,
            &public.to_wire_value(),
        )
        .await
        .unwrap();
    let TransferAuthorizationOutcome::Granted { token, .. } = outcome else {
        panic!("expected Granted");
    };
    token
}

/// A real, byte-identical `chunk_upload` `AuthorizationQuery` for
/// `chunk_index 0`. `artifact_id`/`direction` are signed into the 137-byte
/// transcript (that layout is unchanged) but never carried on the v1 wire
/// message — `bamepd` reconstructs them from the capability binding. Returns
/// the message plus the exact `proof_id` wire value it signed, so a test can
/// assert the transient acceptance binding `bamepd` records carries it.
fn signed_authorization_query(
    signing_key: &SigningKey,
    token: &str,
    fixture: &DispatchedTransfer,
) -> (AuthorizationQueryMessage, String) {
    let capability_id = bamep_domain::CapabilityId::from_token_bytes(token.as_bytes());
    let proof_id = bamep_domain::ProofId::generate();
    let issued_at_millis = Utc::now().timestamp_millis() as u64;
    let fields = bamep_domain::ProofTranscriptFields {
        operation: bamep_domain::AuthorizationOperation::ChunkUpload,
        transfer_id: fixture.transfer_id,
        artifact_id: fixture.artifact_id,
        direction: TransferDirection::AgentToServer,
        chunk_index: Some(0),
        proof_id,
        issued_at_millis,
    };
    let transcript = bamep_domain::build_proof_transcript(&capability_id, &fields);
    let signature = signing_key.sign(&transcript);
    let signature = bamep_domain::ProofSignature::from_bytes(signature.to_bytes());

    let proof_id_wire = proof_id.to_wire_value();
    let message = AuthorizationQueryMessage::new(
        token,
        fixture.transfer_id.0,
        0,
        proof_id_wire.clone(),
        issued_at_millis,
        signature.to_wire_value(),
    );
    (message, proof_id_wire)
}

async fn handshake(path: &Path) -> UnixStream {
    let mut stream = UnixStream::connect(path).await.expect("connect");
    let hello = WorkerHelloMessage::new(Uuid::new_v4());
    let sent_id = hello.envelope.message_id;
    send(&mut stream, &WorkerProtocolMessage::WorkerHello(hello))
        .await
        .expect("send WorkerHello");
    match timeout(TEST_TIMEOUT, receive(&mut stream))
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
    stream
}

#[tokio::test]
async fn a_real_signed_query_over_a_real_uds_socket_is_approved() {
    let db = TestDatabase::setup().await;
    let fixture = dispatched_transfer_fixture(&db.pool, "wds-approved-01").await;
    let authorization = build_authorization_service(db.pool.clone());
    let signing_key = SigningKey::from_bytes(&rand::random());
    let token = issue_capability(&authorization, &fixture, &signing_key).await;

    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let run_task = tokio::spawn(plane.run(
        Arc::clone(&registry),
        Arc::clone(&authorization),
        shutdown_rx,
    ));

    let mut stream = handshake(&socket.0).await;
    let (query, proof_id_wire) = signed_authorization_query(&signing_key, &token, &fixture);
    let sent_id = query.envelope.message_id;
    send(
        &mut stream,
        &WorkerProtocolMessage::AuthorizationQuery(query),
    )
    .await
    .expect("send AuthorizationQuery");

    let acceptance_handle = match timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive AuthorizationDecision")
    {
        WorkerProtocolMessage::AuthorizationDecision(decision) => {
            assert_eq!(decision.body.in_reply_to, sent_id);
            assert_eq!(
                decision.body.decision,
                AuthorizationDecisionOutcome::Approved
            );
            assert_eq!(
                decision.body.digest_algorithm,
                Some(WireDigestAlgorithm::Sha256)
            );
            assert_eq!(decision.body.chunk_size, Some(4096));
            decision
                .body
                .acceptance_handle
                .expect("an approved decision must carry an acceptance_handle")
        }
        other => panic!("expected AuthorizationDecision, got {other:?}"),
    };

    // The handle exists in *this* generation's transient store, bound to the
    // exact operation identity (`m1-worker-data-plane-control-contract.md`
    // "Transient operation handles").
    let operations = registry
        .current_operations()
        .expect("the current generation must have published its transient store");
    assert!(acceptance_handle.starts_with("acc_"));
    let binding = operations
        .acceptance_binding(&acceptance_handle)
        .expect("the returned handle must be a live acceptance binding");
    assert_eq!(binding.transfer_id, fixture.transfer_id);
    assert_eq!(binding.chunk_index, 0);
    assert_eq!(binding.proof_id, proof_id_wire);
    assert_eq!(operations.live_count(), 1);

    drop(stream);
    run_task.abort();
    db.teardown().await;
}

#[tokio::test]
async fn a_query_signed_by_the_wrong_key_over_a_real_uds_socket_is_denied() {
    let db = TestDatabase::setup().await;
    let fixture = dispatched_transfer_fixture(&db.pool, "wds-denied-01").await;
    let authorization = build_authorization_service(db.pool.clone());
    let signing_key = SigningKey::from_bytes(&rand::random());
    let token = issue_capability(&authorization, &fixture, &signing_key).await;
    let unrelated_key = SigningKey::from_bytes(&rand::random());

    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let run_task = tokio::spawn(plane.run(
        Arc::clone(&registry),
        Arc::clone(&authorization),
        shutdown_rx,
    ));

    let mut stream = handshake(&socket.0).await;
    let (query, _proof_id_wire) = signed_authorization_query(&unrelated_key, &token, &fixture);
    let sent_id = query.envelope.message_id;
    send(
        &mut stream,
        &WorkerProtocolMessage::AuthorizationQuery(query),
    )
    .await
    .expect("send AuthorizationQuery");

    match timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive AuthorizationDecision")
    {
        WorkerProtocolMessage::AuthorizationDecision(decision) => {
            assert_eq!(decision.body.in_reply_to, sent_id);
            assert_eq!(decision.body.decision, AuthorizationDecisionOutcome::Denied);
            assert!(
                decision.body.acceptance_handle.is_none()
                    && decision.body.expected_chunk_digest.is_none()
                    && decision.body.digest_algorithm.is_none(),
                "a denial must never carry any further detail"
            );
        }
        other => panic!("expected AuthorizationDecision, got {other:?}"),
    }

    // A denied authorization never mints a transient binding.
    if let Some(operations) = registry.current_operations() {
        assert_eq!(operations.live_count(), 0);
    }

    drop(stream);
    run_task.abort();
    db.teardown().await;
}

/// Issue #38 "Disconnect/reconnect behavior": a fresh connection generation
/// after an earlier one disconnected mid-query must still correctly serve a
/// brand-new query — proving the per-connection request/response loop
/// (`WorkerControlPlane::handle_connection`) leaves no state behind that
/// could corrupt a later generation.
#[tokio::test]
async fn a_new_connection_after_a_prior_ones_mid_query_disconnect_still_works() {
    let db = TestDatabase::setup().await;
    let fixture = dispatched_transfer_fixture(&db.pool, "wds-reconnect-01").await;
    let authorization = build_authorization_service(db.pool.clone());
    let signing_key = SigningKey::from_bytes(&rand::random());
    let token = issue_capability(&authorization, &fixture, &signing_key).await;

    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let run_task = tokio::spawn(plane.run(registry, Arc::clone(&authorization), shutdown_rx));

    // First generation: send a query, then disconnect immediately without
    // waiting for the reply.
    {
        let mut stream = handshake(&socket.0).await;
        let (query, _) = signed_authorization_query(&signing_key, &token, &fixture);
        send(
            &mut stream,
            &WorkerProtocolMessage::AuthorizationQuery(query),
        )
        .await
        .expect("send AuthorizationQuery");
        drop(stream);
    }

    // Second, independent generation: a fresh query must still be answered
    // correctly.
    let mut stream = handshake(&socket.0).await;
    let (query, _) = signed_authorization_query(&signing_key, &token, &fixture);
    let sent_id = query.envelope.message_id;
    send(
        &mut stream,
        &WorkerProtocolMessage::AuthorizationQuery(query),
    )
    .await
    .expect("send AuthorizationQuery");
    match timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive AuthorizationDecision")
    {
        WorkerProtocolMessage::AuthorizationDecision(decision) => {
            assert_eq!(decision.body.in_reply_to, sent_id);
            assert_eq!(
                decision.body.decision,
                AuthorizationDecisionOutcome::Approved
            );
        }
        other => panic!("expected AuthorizationDecision, got {other:?}"),
    }

    drop(stream);
    run_task.abort();
    db.teardown().await;
}

/// Issue #39 Phase B (`m1-worker-data-plane-control-contract.md` "Transient
/// operation handles" / "Failure semantics"): an `acceptance_handle` returned
/// on one generation carries no authority once that connection drops — the
/// store is gone from the registry, and a task still holding its `Arc` sees
/// every operation fail closed. A fresh generation starts with an empty
/// transient authority set.
#[tokio::test]
async fn an_acceptance_handle_loses_all_authority_when_its_generation_ends() {
    let db = TestDatabase::setup().await;
    let fixture = dispatched_transfer_fixture(&db.pool, "wds-phaseb-disc-01").await;
    let authorization = build_authorization_service(db.pool.clone());
    let signing_key = SigningKey::from_bytes(&rand::random());
    let token = issue_capability(&authorization, &fixture, &signing_key).await;

    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let run_task = tokio::spawn(plane.run(
        Arc::clone(&registry),
        Arc::clone(&authorization),
        shutdown_rx,
    ));

    let mut stream = handshake(&socket.0).await;
    let (query, _) = signed_authorization_query(&signing_key, &token, &fixture);
    send(
        &mut stream,
        &WorkerProtocolMessage::AuthorizationQuery(query),
    )
    .await
    .expect("send AuthorizationQuery");
    let handle = match timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive AuthorizationDecision")
    {
        WorkerProtocolMessage::AuthorizationDecision(decision) => {
            decision.body.acceptance_handle.expect("approved handle")
        }
        other => panic!("expected AuthorizationDecision, got {other:?}"),
    };
    // Keep a strong reference to generation A's store, then drop the socket.
    let store_a = registry.current_operations().expect("generation A store");
    drop(stream);

    // Generation A ends; its store is no longer reachable through the
    // registry and every operation on the retained `Arc` fails closed.
    let mut waited = 0;
    while registry.current_operations().is_some() && waited < 50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += 1;
    }
    assert!(registry.current_operations().is_none());
    assert_eq!(
        store_a.consume_acceptance(&handle, fixture.transfer_id, 0),
        Err(TransientOperationError::StaleGeneration)
    );
    assert!(store_a.acceptance_binding(&handle).is_none());

    // A fresh generation starts empty; the old handle is unknown to it.
    let _stream_b = handshake(&socket.0).await;
    let mut waited = 0;
    while registry.current_operations().is_none() && waited < 50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += 1;
    }
    let store_b = registry.current_operations().expect("generation B store");
    assert!(store_b.acceptance_binding(&handle).is_none());
    assert_eq!(store_b.live_count(), 0);

    run_task.abort();
    db.teardown().await;
}

/// Issue #39 Phase B (`m1-worker-data-plane-control-contract.md` "Transient
/// operation handles"): when the generation's transient store is at capacity,
/// a fully-authorized `AuthorizationQuery` still fails closed — the Worker
/// receives the same generic denial, never an approved decision carrying an
/// unusable handle, and the already-live binding is never overwritten. The
/// saturation cause never appears on the wire.
#[tokio::test]
async fn a_saturated_transient_store_denies_an_otherwise_authorized_query() {
    let db = TestDatabase::setup().await;
    let fixture = dispatched_transfer_fixture(&db.pool, "wds-phaseb-sat-01").await;
    let authorization = build_authorization_service(db.pool.clone());
    let signing_key = SigningKey::from_bytes(&rand::random());
    let token = issue_capability(&authorization, &fixture, &signing_key).await;

    let socket = TempSocketPath::fresh();
    let plane = WorkerControlPlane::bind(&socket.0).expect("bind");
    // Capacity 1: the first approved query fills the store.
    let registry = Arc::new(WorkerAuthorityRegistry::with_operations_capacity(1));
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let run_task = tokio::spawn(plane.run(
        Arc::clone(&registry),
        Arc::clone(&authorization),
        shutdown_rx,
    ));

    let mut stream = handshake(&socket.0).await;

    let (query1, _) = signed_authorization_query(&signing_key, &token, &fixture);
    send(
        &mut stream,
        &WorkerProtocolMessage::AuthorizationQuery(query1),
    )
    .await
    .expect("send first AuthorizationQuery");
    let first_handle = match timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive AuthorizationDecision")
    {
        WorkerProtocolMessage::AuthorizationDecision(d) => {
            assert_eq!(d.body.decision, AuthorizationDecisionOutcome::Approved);
            d.body.acceptance_handle.expect("first handle")
        }
        other => panic!("expected AuthorizationDecision, got {other:?}"),
    };

    // A second, independently valid query (fresh proof) — the store is now
    // full, so minting its acceptance binding fails closed.
    let (query2, _) = signed_authorization_query(&signing_key, &token, &fixture);
    send(
        &mut stream,
        &WorkerProtocolMessage::AuthorizationQuery(query2),
    )
    .await
    .expect("send second AuthorizationQuery");
    match timeout(TEST_TIMEOUT, receive(&mut stream))
        .await
        .expect("no timeout")
        .expect("receive AuthorizationDecision")
    {
        WorkerProtocolMessage::AuthorizationDecision(d) => {
            assert_eq!(
                d.body.decision,
                AuthorizationDecisionOutcome::Denied,
                "a saturated transient store fails the query closed"
            );
            assert!(d.body.acceptance_handle.is_none());
        }
        other => panic!("expected AuthorizationDecision, got {other:?}"),
    }

    // The first live binding was never overwritten.
    let operations = registry.current_operations().expect("current store");
    assert!(operations.acceptance_binding(&first_handle).is_some());
    assert_eq!(operations.live_count(), 1);

    drop(stream);
    run_task.abort();
    db.teardown().await;
}
