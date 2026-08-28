//! Issue #39 Phase C1 — `ResumeDiscoveryQuery` / `ResumeDiscoveryContinue`
//! durable-state retrieval and pagination.
//!
//! Covers: the full IPC vertical (real frame -> `WorkerControlPlane` ->
//! sender-constrained authorization -> PostgreSQL -> consistent snapshot ->
//! `ResumeDiscoveryPage`) with at least one forced continuation (item 44);
//! authorization-time snapshot consistency (item 37); strict ascending
//! pagination with no gap/duplicate/omission and first-page-only metadata
//! (item 38); single-use cursors (item 40); and cursor invalidation across a
//! connection generation (item 41).
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

#![cfg(unix)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use bamep_domain::{Digest, DigestAlgorithm};
use bamep_server::adapters::postgres::PostgresTransferRepository;
use bamep_server::adapters::worker_control_plane::WorkerControlPlane;
use bamep_server::application::ChunkAcceptanceService;
use bamep_server::runtime::worker_authority::WorkerAuthorityRegistry;
use bamep_worker_protocol::{
    receive, send, ResumeDiscoveryContinueMessage, ResumeDiscoveryDecision,
    ResumeDiscoveryPageBody, ResumeDiscoveryQueryMessage, WireDigestAlgorithm,
    WorkerProtocolMessage,
};
use ed25519_dalek::SigningKey;
use sqlx::PgPool;
use tokio::sync::watch;
use tokio::time::timeout;

use support::{
    build_authorization_service, build_chunk_acceptance_service, dispatched_transfer_fixture,
    handshake, issue_capability, sign_proof, DispatchedTransfer, TempSocketPath, TestDatabase,
    IPC_TEST_TIMEOUT as TEST_TIMEOUT,
};

fn digest_wire(byte: u8) -> String {
    Digest::new(DigestAlgorithm::Sha256, vec![byte; 32])
        .unwrap()
        .to_wire_value()
}

/// Durably holds `chunk_index` with a deterministic digest, straight through
/// the real `ChunkAcceptanceService` (no wire).
async fn hold_chunk(svc: &ChunkAcceptanceService, fixture: &DispatchedTransfer, index: u64) {
    let outcome = svc
        .commit_chunk_acceptance(fixture.transfer_id, index, digest_wire(index as u8), 4096)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        bamep_server::ports::ChunkAcceptanceCommit::Committed,
        "held chunk {index} must commit"
    );
}

async fn resume_query(
    stream: &mut tokio::net::UnixStream,
    signing_key: &SigningKey,
    token: &str,
    fixture: &DispatchedTransfer,
) -> ResumeDiscoveryPageBody {
    let (proof_id, issued_at, signature) = sign_proof(
        signing_key,
        token,
        fixture,
        bamep_domain::AuthorizationOperation::ResumeDiscovery,
        None,
    );
    let q = ResumeDiscoveryQueryMessage::new(
        token,
        fixture.transfer_id.0,
        proof_id,
        issued_at,
        signature,
    );
    let sent = q.envelope.message_id;
    send(stream, &WorkerProtocolMessage::ResumeDiscoveryQuery(q))
        .await
        .unwrap();
    let page = expect_page(stream).await;
    assert_eq!(page.in_reply_to, sent);
    page
}

async fn resume_continue(
    stream: &mut tokio::net::UnixStream,
    cursor: &str,
) -> ResumeDiscoveryPageBody {
    let c = ResumeDiscoveryContinueMessage::new(cursor);
    let sent = c.envelope.message_id;
    send(stream, &WorkerProtocolMessage::ResumeDiscoveryContinue(c))
        .await
        .unwrap();
    let page = expect_page(stream).await;
    assert_eq!(page.in_reply_to, sent);
    page
}

async fn expect_page(stream: &mut tokio::net::UnixStream) -> ResumeDiscoveryPageBody {
    match timeout(TEST_TIMEOUT, receive(stream))
        .await
        .expect("no timeout")
        .expect("receive")
    {
        WorkerProtocolMessage::ResumeDiscoveryPage(p) => p.body,
        other => panic!("expected ResumeDiscoveryPage, got {other:?}"),
    }
}

fn indices(page: &ResumeDiscoveryPageBody) -> Vec<u64> {
    page.held_chunks
        .as_ref()
        .expect("approved page carries held_chunks")
        .iter()
        .map(|h| h.chunk_index)
        .collect()
}

struct Harness {
    db: TestDatabase,
    fixture: DispatchedTransfer,
    signing_key: SigningKey,
    token: String,
    registry: Arc<WorkerAuthorityRegistry>,
    _socket: TempSocketPath,
    socket_path: std::path::PathBuf,
    run_task: tokio::task::JoinHandle<
        Result<(), bamep_server::adapters::worker_control_plane::WorkerControlPlaneError>,
    >,
    _shutdown_tx: watch::Sender<bool>,
}

impl Harness {
    async fn start(signal: &str, page_size: usize) -> Self {
        let db = TestDatabase::setup().await;
        let fixture = dispatched_transfer_fixture(&db.pool, signal).await;
        let authorization = build_authorization_service(db.pool.clone());
        let signing_key = SigningKey::from_bytes(&rand::random());
        let token = issue_capability(&authorization, &fixture, &signing_key).await;

        let socket = TempSocketPath::fresh();
        let socket_path = socket.0.clone();
        let plane = WorkerControlPlane::bind(&socket.0)
            .expect("bind")
            .with_resume_page_size(page_size);
        let registry = Arc::new(WorkerAuthorityRegistry::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let run_task = tokio::spawn(plane.run(
            Arc::clone(&registry),
            authorization,
            build_chunk_acceptance_service(db.pool.clone()),
            shutdown_rx,
        ));
        Self {
            db,
            fixture,
            signing_key,
            token,
            registry,
            _socket: socket,
            socket_path,
            run_task,
            _shutdown_tx: shutdown_tx,
        }
    }

    fn acceptance(&self) -> ChunkAcceptanceService {
        ChunkAcceptanceService::new(Arc::new(PostgresTransferRepository::new(
            self.db.pool.clone(),
        )))
    }

    async fn finish(self) {
        self.run_task.abort();
        self.db.teardown().await;
    }
}

#[tokio::test]
async fn a_small_held_set_ships_in_one_page_with_no_cursor() {
    let h = Harness::start("c1-resume-onepage", 8192).await;
    let svc = h.acceptance();
    hold_chunk(&svc, &h.fixture, 0).await;
    hold_chunk(&svc, &h.fixture, 2).await;

    let mut stream = handshake(&h.socket_path).await;
    let page = resume_query(&mut stream, &h.signing_key, &h.token, &h.fixture).await;

    assert_eq!(page.decision, ResumeDiscoveryDecision::Approved);
    assert_eq!(page.transfer_id, Some(h.fixture.transfer_id.0));
    assert_eq!(page.sealed, Some(false));
    assert_eq!(page.digest_algorithm, Some(WireDigestAlgorithm::Sha256));
    assert_eq!(page.chunk_size, Some(4096));
    assert_eq!(page.expected_chunk_count, None); // omitted before sealing
    assert_eq!(indices(&page), vec![0, 2]);
    assert!(page.resume_cursor.is_none());
    // Only durably held chunks, with their recorded digests.
    let hc = page.held_chunks.unwrap();
    assert_eq!(hc[0].digest, digest_wire(0));
    assert_eq!(hc[1].digest, digest_wire(2));

    drop(stream);
    h.finish().await;
}

#[tokio::test]
async fn pagination_is_strictly_ascending_with_no_gap_or_repeat_and_first_page_only_metadata() {
    // Issue #39 Phase C1 item 38 + 44.
    let h = Harness::start("c1-resume-paginate", 2).await;
    let svc = h.acceptance();
    for i in 0..7u64 {
        hold_chunk(&svc, &h.fixture, i).await;
    }

    let mut stream = handshake(&h.socket_path).await;
    let mut page = resume_query(&mut stream, &h.signing_key, &h.token, &h.fixture).await;

    // First page: metadata present, first slice.
    assert!(page.transfer_id.is_some() && page.sealed.is_some() && page.chunk_size.is_some());
    let mut seen: Vec<u64> = indices(&page);
    assert_eq!(seen, vec![0, 1]);

    let mut pages = 1;
    while let Some(cursor) = page.resume_cursor.clone() {
        page = resume_continue(&mut stream, &cursor).await;
        pages += 1;
        // Continuation pages carry NO manifest-level field.
        assert!(page.transfer_id.is_none());
        assert!(page.sealed.is_none());
        assert!(page.digest_algorithm.is_none());
        assert!(page.chunk_size.is_none());
        assert!(page.expected_chunk_count.is_none());
        seen.extend(indices(&page));
    }
    assert_eq!(pages, 4, "7 chunks at page size 2 -> 4 pages");
    assert_eq!(
        seen,
        vec![0, 1, 2, 3, 4, 5, 6],
        "ascending, no gap, no repeat"
    );

    // The snapshot is released once pagination completes.
    let store = h.registry.current_operations().expect("store");
    assert_eq!(store.live_resume_snapshot_count(), 0);

    drop(stream);
    h.finish().await;
}

#[tokio::test]
async fn continuation_reflects_only_the_authorization_time_snapshot() {
    // Issue #39 Phase C1 item 37.
    let h = Harness::start("c1-resume-consistency", 1).await;
    let svc = h.acceptance();
    hold_chunk(&svc, &h.fixture, 0).await;
    hold_chunk(&svc, &h.fixture, 2).await;

    let mut stream = handshake(&h.socket_path).await;
    let first = resume_query(&mut stream, &h.signing_key, &h.token, &h.fixture).await;
    assert_eq!(indices(&first), vec![0]);
    let cursor = first.resume_cursor.clone().expect("more pages remain");

    // After the snapshot was captured, chunk 1 becomes durably held.
    hold_chunk(&svc, &h.fixture, 1).await;

    // The original sequence must NOT see chunk 1 — only its snapshot.
    let second = resume_continue(&mut stream, &cursor).await;
    assert_eq!(indices(&second), vec![2]);
    assert!(second.resume_cursor.is_none());

    // A fresh ResumeDiscoveryQuery afterwards DOES include chunk 1 (page size
    // 1, so it arrives across pages: {0} then {1} then {2}).
    let fresh = resume_query(&mut stream, &h.signing_key, &h.token, &h.fixture).await;
    assert_eq!(indices(&fresh), vec![0]);
    let mut all = indices(&fresh);
    let mut cur = fresh.resume_cursor.clone();
    while let Some(c) = cur {
        let p = resume_continue(&mut stream, &c).await;
        all.extend(indices(&p));
        cur = p.resume_cursor.clone();
    }
    assert_eq!(
        all,
        vec![0, 1, 2],
        "a fresh snapshot includes the later chunk"
    );

    drop(stream);
    h.finish().await;
}

#[tokio::test]
async fn a_reused_or_unknown_cursor_is_denied() {
    // Issue #39 Phase C1 item 40 + 33.
    let h = Harness::start("c1-resume-cursor-reuse", 1).await;
    let svc = h.acceptance();
    hold_chunk(&svc, &h.fixture, 0).await;
    hold_chunk(&svc, &h.fixture, 1).await;

    let mut stream = handshake(&h.socket_path).await;
    let first = resume_query(&mut stream, &h.signing_key, &h.token, &h.fixture).await;
    let cursor = first.resume_cursor.clone().unwrap();

    let second = resume_continue(&mut stream, &cursor).await;
    assert_eq!(indices(&second), vec![1]);

    // Reusing the just-consumed cursor is denied, not the same page again.
    let reused = resume_continue(&mut stream, &cursor).await;
    assert_eq!(reused.decision, ResumeDiscoveryDecision::Denied);
    assert!(reused.held_chunks.is_none());

    // A fabricated cursor is denied too.
    let unknown = resume_continue(&mut stream, "res_00000000000000000000000000000000").await;
    assert_eq!(unknown.decision, ResumeDiscoveryDecision::Denied);

    drop(stream);
    h.finish().await;
}

#[tokio::test]
async fn a_cursor_from_a_prior_connection_generation_is_denied() {
    // Issue #39 Phase C1 item 41.
    let h = Harness::start("c1-resume-generation", 1).await;
    let svc = h.acceptance();
    hold_chunk(&svc, &h.fixture, 0).await;
    hold_chunk(&svc, &h.fixture, 1).await;

    // Generation A: obtain a cursor, then drop the connection.
    let mut stream_a = handshake(&h.socket_path).await;
    let first = resume_query(&mut stream_a, &h.signing_key, &h.token, &h.fixture).await;
    let stale_cursor = first.resume_cursor.clone().unwrap();
    drop(stream_a);

    // Wait for generation A to end.
    let mut waited = 0;
    while h.registry.current_operations().is_some() && waited < 100 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += 1;
    }

    // Generation B: a fresh connection. The prior-generation cursor is denied
    // and exposes no snapshot data.
    let mut stream_b = handshake(&h.socket_path).await;
    let page = resume_continue(&mut stream_b, &stale_cursor).await;
    assert_eq!(page.decision, ResumeDiscoveryDecision::Denied);
    assert!(page.held_chunks.is_none() && page.transfer_id.is_none());

    // Generation B can still start its own resume normally.
    let fresh = resume_query(&mut stream_b, &h.signing_key, &h.token, &h.fixture).await;
    assert_eq!(fresh.decision, ResumeDiscoveryDecision::Approved);
    assert_eq!(indices(&fresh), vec![0]);

    drop(stream_b);
    h.finish().await;
}

#[tokio::test]
async fn a_denied_authorization_returns_a_bare_denied_page() {
    let h = Harness::start("c1-resume-denied", 8192).await;
    let mut stream = handshake(&h.socket_path).await;

    // Wrong signing key -> signature verification fails -> generic denial.
    let wrong_key = SigningKey::from_bytes(&rand::random());
    let (proof_id, issued_at, signature) = sign_proof(
        &wrong_key,
        &h.token,
        &h.fixture,
        bamep_domain::AuthorizationOperation::ResumeDiscovery,
        None,
    );
    let q = ResumeDiscoveryQueryMessage::new(
        &h.token,
        h.fixture.transfer_id.0,
        proof_id,
        issued_at,
        signature,
    );
    send(&mut stream, &WorkerProtocolMessage::ResumeDiscoveryQuery(q))
        .await
        .unwrap();
    let page = expect_page(&mut stream).await;
    assert_eq!(page.decision, ResumeDiscoveryDecision::Denied);
    assert!(page.transfer_id.is_none());
    assert!(page.sealed.is_none());
    assert!(page.held_chunks.is_none());
    assert!(page.resume_cursor.is_none());

    let _pool: &PgPool = &h.db.pool;
    drop(stream);
    h.finish().await;
}
