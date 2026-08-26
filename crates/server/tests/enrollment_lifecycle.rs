//! Component/Integration tests: `EnrollmentService` and `BootOrchestrationService`
//! against the real `PostgresEndpointRepository`/`PostgresCredentialRedemptionRepository`/
//! `PostgresBootContextRepository` Adapters and a real PostgreSQL instance
//! (ADR-0013), exercising the full ADR-0012/ADR-0014 credential-redemption
//! model through the real Application operations — no direct database row
//! mutation anywhere in this file (Issue #17 "Safety constraints"); direct
//! SQL is used only to *read* and assert on durable state the
//! Application/Domain already committed, and — in exactly one test — to
//! install a test-local trigger that forces a deterministic mid-transaction
//! failure inside this disposable database.
//!
//! Every enrollment credential in this file is issued through
//! `BootOrchestrationService::issue_enrollment_credential` (the real
//! ADR-0014 issuance path, `issue_e1` below) — no transitional HMAC fixture
//! remains anywhere in this crate.
//!
//! The operator-approval calls below stand in for the "control path separate
//! from the Simulated Agent participant" (Issue #17 RF-002): this test file
//! plays that separate harness role, calling `EnrollmentService::approve_enrollment`
//! directly — never through a credential-redemption/Agent-facing method.
//!
//! Every test uses a `ManualClock` (`support::ManualClock`) instead of real
//! wall-clock sleeps, so `EnrollmentService::redeem`'s decision-time clock
//! read (see `application::Clock`) stays under precise, deterministic test
//! control — including while a concurrent request is genuinely blocked on a
//! PostgreSQL lock.
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

mod support;

use std::sync::Arc;

use bamep_domain::credential::CredentialHash;
use bamep_domain::presented_credential::{CredentialKind, PresentedCredential};
use bamep_domain::{Actor, BootNonce, EndpointId, TrustedBootstrapState};
use bamep_server::adapters::postgres::{
    PostgresBootContextRepository, PostgresCredentialRedemptionRepository,
    PostgresEndpointRepository,
};
use bamep_server::application::{
    ApplicationError, BootOrchestrationService, Clock, EnrollmentService, RedeemResult,
};
use bamep_server::ports::EndpointRepository;
use chrono::{Duration, Utc};
use sqlx::PgPool;
use support::{ManualClock, TestDatabase};
use uuid::Uuid;

type Enrollment =
    EnrollmentService<PostgresEndpointRepository, PostgresCredentialRedemptionRepository>;
type BootOrchestration = BootOrchestrationService<PostgresBootContextRepository>;

fn build_services(pool: PgPool) -> (BootOrchestration, Enrollment, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new(Utc::now()));
    let boot_repo = Arc::new(PostgresBootContextRepository::new(pool.clone()));
    let endpoint_repo = Arc::new(PostgresEndpointRepository::new(pool.clone()));
    let redemption_repo = Arc::new(PostgresCredentialRedemptionRepository::new(pool));
    let boot_orchestration = BootOrchestrationService::new(boot_repo, Duration::minutes(5));
    let enrollment = EnrollmentService::with_clock(
        endpoint_repo,
        redemption_repo,
        Arc::clone(&clock) as Arc<dyn Clock>,
    );
    (boot_orchestration, enrollment, clock)
}

/// Issues a real ADR-0014 boot-scoped enrollment credential through
/// `BootOrchestrationService` — the same path Boot Orchestration itself
/// exercises end to end. Generates a fresh, discarded `BootNonce`: most
/// tests in this file don't need to assert on the exact nonce value — see
/// [`issue_e1_with_nonce`] for the ones (current-boot assertions) that do.
async fn issue_e1(
    boot_orchestration: &BootOrchestration,
    inventory_signal: &str,
    now: chrono::DateTime<Utc>,
) -> PresentedCredential {
    let boot_nonce = BootNonce::generate().expect("OS CSPRNG must be available in tests");
    issue_e1_with_nonce(boot_orchestration, inventory_signal, boot_nonce, now).await
}

/// Same as [`issue_e1`], but with caller-controlled `boot_nonce` — for tests
/// that assert on the exact persisted current-boot nonce.
async fn issue_e1_with_nonce(
    boot_orchestration: &BootOrchestration,
    inventory_signal: &str,
    boot_nonce: BootNonce,
    now: chrono::DateTime<Utc>,
) -> PresentedCredential {
    boot_orchestration
        .issue_enrollment_credential(inventory_signal, boot_nonce, now)
        .await
        .expect("issuance must succeed")
}

/// Returns the `index`-th `.`-separated segment of a credential wire value —
/// a test-only way to forge a credential that mixes one credential's lookup
/// id with another's secret (or vice versa), without any access to
/// `presented_credential`'s private fields.
fn wire_segment(wire: &str, index: usize) -> &str {
    wire.split('.').nth(index).unwrap()
}

async fn domain_event_count(pool: &PgPool, endpoint_id: EndpointId) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM domain_events WHERE endpoint_id = $1")
        .bind(endpoint_id.0)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn audit_record_count(pool: &PgPool, endpoint_id: EndpointId) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM audit_records WHERE endpoint_id = $1")
        .bind(endpoint_id.0)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn endpoint_row_count(pool: &PgPool, inventory_signal: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM endpoints WHERE inventory_signal = $1")
        .bind(inventory_signal)
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

async fn identity_state(pool: &PgPool, endpoint_id: EndpointId) -> String {
    // `identity_state` is a native `endpoint_identity_state` PostgreSQL ENUM
    // — cast to `text` so this assertion helper can keep
    // comparing against the plain textual label.
    sqlx::query_scalar("SELECT identity_state::text FROM endpoints WHERE id = $1")
        .bind(endpoint_id.0)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn hardware_confidence(pool: &PgPool, endpoint_id: EndpointId) -> String {
    sqlx::query_scalar("SELECT hardware_confidence::text FROM endpoints WHERE id = $1")
        .bind(endpoint_id.0)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn lookup_id_for_slot(pool: &PgPool, endpoint_id: EndpointId, slot: &str) -> Option<Vec<u8>> {
    sqlx::query_scalar(
        "SELECT lookup_id FROM endpoint_credential_lookups \
         WHERE endpoint_id = $1 AND slot = $2::credential_slot_role",
    )
    .bind(endpoint_id.0)
    .bind(slot)
    .fetch_optional(pool)
    .await
    .unwrap()
}

async fn lookup_row_count(pool: &PgPool, endpoint_id: EndpointId) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM endpoint_credential_lookups WHERE endpoint_id = $1")
        .bind(endpoint_id.0)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn boot_context_resolved_endpoint(pool: &PgPool, boot_context_id: &[u8]) -> Option<Uuid> {
    sqlx::query_scalar("SELECT resolved_endpoint_id FROM boot_contexts WHERE boot_context_id = $1")
        .bind(boot_context_id.to_vec())
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Narrow test-only setup used by credential-lifecycle tests that are not
/// intended to duplicate production `BootstrapEvidence` verification. The
/// dedicated evidence and WSS suites exercise the real transition.
async fn set_trusted_bootstrap_established(pool: &PgPool, endpoint_id: EndpointId) {
    sqlx::query("UPDATE endpoints SET trusted_bootstrap_state = 'Established' WHERE id = $1")
        .bind(endpoint_id.0)
        .execute(pool)
        .await
        .expect("test-only trusted_bootstrap_state override must succeed");
}

async fn current_boot_of(pool: &PgPool, endpoint_id: EndpointId) -> bamep_domain::CurrentBoot {
    PostgresEndpointRepository::new(pool.clone())
        .find_by_id(endpoint_id)
        .await
        .unwrap()
        .expect("endpoint must exist")
        .current_boot
        .expect("current boot must be set")
}

#[tokio::test]
async fn migrations_apply_cleanly_to_a_fresh_database() {
    let db = TestDatabase::setup().await;

    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_name NOT LIKE '\\_sqlx%' \
         ORDER BY table_name",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();

    assert_eq!(
        tables,
        vec![
            "artifacts",
            "attempts",
            "audit_records",
            "boot_contexts",
            "chunk_identities",
            "chunk_manifests",
            "domain_events",
            "endpoint_credential_lookups",
            "endpoint_credentials",
            "endpoints",
            "inventory_revisions",
            "job_steps",
            "jobs",
            "transfers",
        ]
    );

    db.teardown().await;
}

/// Confirms the baseline's closed-vocabulary columns use their intended
/// user-defined PostgreSQL ENUM types, not another non-`TEXT` representation.
#[tokio::test]
async fn closed_vocabulary_columns_use_native_postgres_enum_types() {
    let db = TestDatabase::setup().await;

    async fn udt_name(pool: &PgPool, table_name: &str, column_name: &str) -> String {
        sqlx::query_scalar(
            "SELECT udt_name FROM information_schema.columns \
             WHERE table_name = $1 AND column_name = $2",
        )
        .bind(table_name)
        .bind(column_name)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    assert_eq!(
        udt_name(&db.pool, "endpoints", "identity_state").await,
        "endpoint_identity_state"
    );
    assert_eq!(
        udt_name(&db.pool, "endpoints", "hardware_confidence").await,
        "hardware_confidence_state"
    );
    assert_eq!(
        udt_name(&db.pool, "domain_events", "event_type").await,
        "domain_event_type"
    );
    assert_eq!(
        udt_name(&db.pool, "audit_records", "actor_kind").await,
        "audit_actor_kind"
    );

    db.teardown().await;
}

#[tokio::test]
async fn first_contact_creates_pending_enrollment_and_persists_lookup_pair() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());

    let e1 = issue_e1(&boot_orchestration, "sim-endpoint-01", clock.now()).await;
    let result = enrollment.redeem(&e1.to_wire_value()).await.unwrap();

    let RedeemResult::Established { endpoint_id, .. } = result else {
        panic!("first contact must establish a session");
    };
    assert_eq!(
        identity_state(&db.pool, endpoint_id).await,
        "PendingEnrollment"
    );

    assert_eq!(
        lookup_id_for_slot(&db.pool, endpoint_id, "predecessor").await,
        Some(e1.lookup_id().to_bytes().to_vec()),
        "the promoted predecessor mapping must carry E1's own lookup id"
    );
    assert!(
        lookup_id_for_slot(&db.pool, endpoint_id, "successor")
            .await
            .is_some(),
        "a fresh Runtime successor mapping must exist"
    );
    assert_eq!(lookup_row_count(&db.pool, endpoint_id).await, 2);

    let resolved = boot_context_resolved_endpoint(&db.pool, &e1.lookup_id().to_bytes()).await;
    assert_eq!(resolved, Some(endpoint_id.0));

    db.teardown().await;
}

#[tokio::test]
async fn first_contact_persists_consistent_hardware_confidence_independently_of_approval() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());

    let e1 = issue_e1(&boot_orchestration, "sim-hwconf-01", clock.now()).await;
    let RedeemResult::Established { endpoint_id, .. } =
        enrollment.redeem(&e1.to_wire_value()).await.unwrap()
    else {
        panic!("first contact must establish a session");
    };

    // Established at Endpoint creation, before any operator approval —
    // never coupled to PendingEnrollment -> Enrolled.
    assert_eq!(
        identity_state(&db.pool, endpoint_id).await,
        "PendingEnrollment"
    );
    assert_eq!(
        hardware_confidence(&db.pool, endpoint_id).await,
        "Consistent"
    );

    enrollment
        .approve_enrollment(
            endpoint_id,
            Actor::Operator {
                label: "wp1-harness".into(),
            },
            clock.now(),
        )
        .await
        .unwrap();

    assert_eq!(identity_state(&db.pool, endpoint_id).await, "Enrolled");
    assert_eq!(
        hardware_confidence(&db.pool, endpoint_id).await,
        "Consistent",
        "operator approval must not disturb the independent hardware-confidence dimension"
    );

    db.teardown().await;
}

#[tokio::test]
async fn hardware_confidence_survives_a_fresh_pool_instance() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());

    let e1 = issue_e1(&boot_orchestration, "sim-hwconf-durable-01", clock.now()).await;
    let RedeemResult::Established { endpoint_id, .. } =
        enrollment.redeem(&e1.to_wire_value()).await.unwrap()
    else {
        panic!("first contact must establish a session");
    };

    // A brand-new pool/repository against the same database, mirroring
    // `durable_state_survives_a_fresh_pool_instance` — proves durable
    // persistence rather than an in-process cache.
    let fresh_pool = PgPool::connect(&db.db_url)
        .await
        .expect("reconnect to the same durable database");
    let fresh_repo = Arc::new(PostgresEndpointRepository::new(fresh_pool));
    let fetched = fresh_repo
        .find_by_id(endpoint_id)
        .await
        .unwrap()
        .expect("state committed by the original pool must be visible to a fresh one");
    assert_eq!(
        fetched.hardware_confidence,
        bamep_domain::HardwareConfidence::Consistent
    );

    db.teardown().await;
}

#[tokio::test]
async fn well_formed_but_unknown_credential_is_rejected_with_no_persisted_transition() {
    let db = TestDatabase::setup().await;
    let (_boot, enrollment, _clock) = build_services(db.pool.clone());

    let bogus = PresentedCredential::generate(CredentialKind::Enrollment);
    let result = enrollment.redeem(&bogus.to_wire_value()).await.unwrap();

    assert!(matches!(result, RedeemResult::Rejected));
    assert_eq!(total_endpoint_count(&db.pool).await, 0);

    db.teardown().await;
}

#[tokio::test]
async fn malformed_credential_wire_is_rejected_generically_with_no_persisted_transition() {
    let db = TestDatabase::setup().await;
    let (_boot, enrollment, _clock) = build_services(db.pool.clone());

    let result = enrollment
        .redeem("not-a-valid-credential-wire-value")
        .await
        .unwrap();

    assert!(matches!(result, RedeemResult::Rejected));
    assert_eq!(total_endpoint_count(&db.pool).await, 0);

    db.teardown().await;
}

#[tokio::test]
async fn expired_boot_context_is_rejected() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());

    let e1 = issue_e1(&boot_orchestration, "sim-endpoint-03", clock.now()).await;
    clock.advance(Duration::minutes(10)); // past the 5-minute enrollment TTL

    let result = enrollment.redeem(&e1.to_wire_value()).await.unwrap();
    assert!(matches!(result, RedeemResult::Rejected));

    db.teardown().await;
}

#[tokio::test]
async fn wrong_lookup_id_or_wrong_secret_is_rejected() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());

    let e1 = issue_e1(&boot_orchestration, "sim-endpoint-wrong-01", clock.now()).await;
    let RedeemResult::Established { .. } = enrollment.redeem(&e1.to_wire_value()).await.unwrap()
    else {
        panic!("first contact must establish a session");
    };

    let e1_wire = e1.to_wire_value();
    let other = PresentedCredential::generate(CredentialKind::Enrollment);
    let other_wire = other.to_wire_value();

    // Correct lookup id, wrong secret.
    let wrong_secret = format!(
        "v1.enrollment.{}.{}",
        wire_segment(&e1_wire, 2),
        wire_segment(&other_wire, 3)
    );
    assert!(matches!(
        enrollment.redeem(&wrong_secret).await.unwrap(),
        RedeemResult::Rejected
    ));

    // Wrong lookup id, correct (E1's real) secret bytes.
    let wrong_lookup = format!(
        "v1.enrollment.{}.{}",
        wire_segment(&other_wire, 2),
        wire_segment(&e1_wire, 3)
    );
    assert!(matches!(
        enrollment.redeem(&wrong_lookup).await.unwrap(),
        RedeemResult::Rejected
    ));

    db.teardown().await;
}

#[tokio::test]
async fn runtime_kind_never_falls_through_to_a_colliding_boot_context_lookup_id() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());

    let e1 = issue_e1(&boot_orchestration, "sim-endpoint-collide-01", clock.now()).await;
    let e1_wire = e1.to_wire_value();

    // A Runtime-kinded credential forged to carry E1's exact lookup id (and
    // otherwise-unrelated secret bytes) must never be routed to BootContext
    // (ADR-0014 point 7) — it must be rejected as UnknownCredential, not
    // evaluated against the still-open, still-unresolved BootContext.
    let runtime_wire = format!(
        "v1.runtime.{}.{}",
        wire_segment(&e1_wire, 2),
        wire_segment(&e1_wire, 3)
    );
    let result = enrollment.redeem(&runtime_wire).await.unwrap();
    assert!(matches!(result, RedeemResult::Rejected));

    // The BootContext itself remains untouched — E1 still resolves normally.
    let established = enrollment.redeem(&e1_wire).await.unwrap();
    assert!(matches!(established, RedeemResult::Established { .. }));

    db.teardown().await;
}

#[tokio::test]
async fn runtime_successor_reconnects_by_lookup_id_alone() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());

    let e1 = issue_e1(&boot_orchestration, "sim-endpoint-02", clock.now()).await;
    let RedeemResult::Established {
        endpoint_id,
        runtime_credential: r1,
        ..
    } = enrollment.redeem(&e1.to_wire_value()).await.unwrap()
    else {
        panic!("first contact must establish a session");
    };

    clock.advance(Duration::seconds(5));
    let result = enrollment.redeem(&r1.to_wire_value()).await.unwrap();

    assert!(
        matches!(result, RedeemResult::Established { endpoint_id: id, .. } if id == endpoint_id)
    );

    db.teardown().await;
}

#[tokio::test]
async fn predecessor_retry_after_unconfirmed_successor_supersedes_and_reissues() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());

    let e1 = issue_e1(&boot_orchestration, "sim-endpoint-06", clock.now()).await;
    let RedeemResult::Established {
        endpoint_id,
        runtime_credential: r1,
        ..
    } = enrollment.redeem(&e1.to_wire_value()).await.unwrap()
    else {
        panic!();
    };

    // E1 is presented again before R1 ever authenticated (dropped connection
    // between commit and delivery).
    clock.advance(Duration::seconds(5));
    let RedeemResult::Established {
        runtime_credential: r1_prime,
        ..
    } = enrollment.redeem(&e1.to_wire_value()).await.unwrap()
    else {
        panic!("predecessor must still authenticate");
    };
    assert_ne!(r1.lookup_id(), r1_prime.lookup_id());

    assert_eq!(
        lookup_id_for_slot(&db.pool, endpoint_id, "predecessor").await,
        Some(e1.lookup_id().to_bytes().to_vec()),
        "E1 must remain the predecessor mapping"
    );
    assert_eq!(
        lookup_id_for_slot(&db.pool, endpoint_id, "successor").await,
        Some(r1_prime.lookup_id().to_bytes().to_vec()),
        "the stale R1 mapping must be replaced by R1'"
    );

    // The superseded R1 must now be rejected with a generic outcome.
    clock.advance(Duration::seconds(5));
    let rejected = enrollment.redeem(&r1.to_wire_value()).await.unwrap();
    assert!(matches!(rejected, RedeemResult::Rejected));

    // R1' (the fresh successor) authenticates and confirms.
    let confirmed = enrollment.redeem(&r1_prime.to_wire_value()).await.unwrap();
    assert!(matches!(confirmed, RedeemResult::Established { .. }));

    db.teardown().await;
}

#[tokio::test]
async fn successor_confirmation_rotates_lookup_projection() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());

    let e1 = issue_e1(&boot_orchestration, "sim-endpoint-confirm-01", clock.now()).await;
    let RedeemResult::Established {
        endpoint_id,
        runtime_credential: r1,
        ..
    } = enrollment.redeem(&e1.to_wire_value()).await.unwrap()
    else {
        panic!();
    };

    clock.advance(Duration::seconds(5));
    let RedeemResult::Established {
        runtime_credential: r2,
        ..
    } = enrollment.redeem(&r1.to_wire_value()).await.unwrap()
    else {
        panic!("R1 must confirm");
    };

    assert_eq!(
        lookup_id_for_slot(&db.pool, endpoint_id, "predecessor").await,
        Some(r1.lookup_id().to_bytes().to_vec()),
        "R1 must become the predecessor"
    );
    assert_eq!(
        lookup_id_for_slot(&db.pool, endpoint_id, "successor").await,
        Some(r2.lookup_id().to_bytes().to_vec()),
        "a fresh R2 successor must exist"
    );

    // E1's old lookup mapping is gone: E1 no longer routes anywhere.
    let e1_rejected = enrollment.redeem(&e1.to_wire_value()).await.unwrap();
    assert!(matches!(e1_rejected, RedeemResult::Rejected));

    db.teardown().await;
}

#[tokio::test]
async fn promoted_predecessor_survives_boot_context_expiry() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());
    // A much longer runtime-credential TTL than the 5-minute BootContext TTL
    // `build_services` issues against.
    let enrollment = enrollment.with_credential_ttl(Duration::hours(1));

    let e1 = issue_e1(&boot_orchestration, "sim-endpoint-promoted-01", clock.now()).await;
    let RedeemResult::Established { .. } = enrollment.redeem(&e1.to_wire_value()).await.unwrap()
    else {
        panic!();
    };

    // Past BootContext's own 5-minute TTL, but well within the 1-hour
    // promoted-predecessor credential TTL (ADR-0014 point 6).
    clock.advance(Duration::minutes(30));
    let retry = enrollment.redeem(&e1.to_wire_value()).await.unwrap();
    assert!(matches!(retry, RedeemResult::Established { .. }));

    db.teardown().await;
}

#[tokio::test]
async fn operator_approval_via_separate_control_path_transitions_to_enrolled() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());

    let e1 = issue_e1(&boot_orchestration, "sim-endpoint-04", clock.now()).await;
    let RedeemResult::Established { endpoint_id, .. } =
        enrollment.redeem(&e1.to_wire_value()).await.unwrap()
    else {
        panic!("first contact must establish a session");
    };

    enrollment
        .approve_enrollment(
            endpoint_id,
            Actor::Operator {
                label: "wp1-harness".into(),
            },
            clock.now(),
        )
        .await
        .unwrap();

    db.teardown().await;
}

#[tokio::test]
async fn approve_enrollment_commits_state_event_and_audit_atomically() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());

    let e1 = issue_e1(&boot_orchestration, "sim-endpoint-atomic-01", clock.now()).await;
    let RedeemResult::Established { endpoint_id, .. } =
        enrollment.redeem(&e1.to_wire_value()).await.unwrap()
    else {
        panic!("first contact must establish a session");
    };

    assert_eq!(
        domain_event_count(&db.pool, endpoint_id).await,
        1,
        "first contact emits exactly EndpointPendingEnrollment"
    );
    assert_eq!(audit_record_count(&db.pool, endpoint_id).await, 0);

    enrollment
        .approve_enrollment(
            endpoint_id,
            Actor::Operator {
                label: "wp1-harness".into(),
            },
            clock.now(),
        )
        .await
        .unwrap();

    assert_eq!(
        domain_event_count(&db.pool, endpoint_id).await,
        3,
        "approval commits EndpointEnrolled + OperatorDecisionRecorded alongside the earlier event"
    );
    assert_eq!(
        audit_record_count(&db.pool, endpoint_id).await,
        1,
        "approval commits exactly one immutable audit record in the same transaction"
    );

    assert_eq!(identity_state(&db.pool, endpoint_id).await, "Enrolled");

    db.teardown().await;
}

#[tokio::test]
async fn invalid_transition_rolls_back_without_partial_writes() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());

    let e1 = issue_e1(&boot_orchestration, "sim-endpoint-invalid-01", clock.now()).await;
    let RedeemResult::Established { endpoint_id, .. } =
        enrollment.redeem(&e1.to_wire_value()).await.unwrap()
    else {
        panic!("first contact must establish a session");
    };
    let operator = Actor::Operator {
        label: "wp1-harness".into(),
    };
    enrollment
        .approve_enrollment(endpoint_id, operator.clone(), clock.now())
        .await
        .unwrap();

    let events_before = domain_event_count(&db.pool, endpoint_id).await;
    let audit_before = audit_record_count(&db.pool, endpoint_id).await;

    // Already Enrolled: approving again is an illegal transition. Nothing
    // may be written for a rejected transition, exactly like a rejected
    // AuthRequest.
    let err = enrollment
        .approve_enrollment(endpoint_id, operator, clock.now())
        .await
        .unwrap_err();
    assert!(matches!(err, ApplicationError::InvalidTransition(_)));

    assert_eq!(
        domain_event_count(&db.pool, endpoint_id).await,
        events_before
    );
    assert_eq!(
        audit_record_count(&db.pool, endpoint_id).await,
        audit_before
    );

    db.teardown().await;
}

#[tokio::test]
async fn reconnect_preserves_enrolled_identity_without_repeated_approval() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());

    let e1 = issue_e1(&boot_orchestration, "sim-endpoint-05", clock.now()).await;
    let RedeemResult::Established {
        endpoint_id,
        runtime_credential: r1,
        ..
    } = enrollment.redeem(&e1.to_wire_value()).await.unwrap()
    else {
        panic!("first contact must establish a session");
    };

    enrollment
        .approve_enrollment(
            endpoint_id,
            Actor::Operator {
                label: "wp1-harness".into(),
            },
            clock.now(),
        )
        .await
        .unwrap();

    // Fresh handshake reconnect, redeeming the previously issued runtime
    // credential — no operator approval call happens here.
    clock.advance(Duration::seconds(30));
    let result = enrollment.redeem(&r1.to_wire_value()).await.unwrap();

    assert!(
        matches!(result, RedeemResult::Established { endpoint_id: id, .. } if id == endpoint_id)
    );
    assert_eq!(identity_state(&db.pool, endpoint_id).await, "Enrolled");

    db.teardown().await;
}

#[tokio::test]
async fn genuine_reboot_preserves_identity_and_replaces_lookup_pair() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());

    let e1 = issue_e1(&boot_orchestration, "sim-endpoint-reboot-01", clock.now()).await;
    let RedeemResult::Established {
        endpoint_id,
        runtime_credential: r1,
        ..
    } = enrollment.redeem(&e1.to_wire_value()).await.unwrap()
    else {
        panic!("first contact must establish a session");
    };

    enrollment
        .approve_enrollment(
            endpoint_id,
            Actor::Operator {
                label: "wp1-harness".into(),
            },
            clock.now(),
        )
        .await
        .unwrap();

    let events_before = domain_event_count(&db.pool, endpoint_id).await;
    let audit_before = audit_record_count(&db.pool, endpoint_id).await;

    // Genuine reboot: the Agent restarts, re-engages the Boot Orchestrator,
    // and presents a fresh enrollment credential (E2) for the same
    // inventory_signal rather than anything from its old runtime chain.
    clock.advance(Duration::hours(2));
    let e2 = issue_e1(&boot_orchestration, "sim-endpoint-reboot-01", clock.now()).await;
    let result = enrollment.redeem(&e2.to_wire_value()).await.unwrap();

    let RedeemResult::Established {
        endpoint_id: id_after_reboot,
        runtime_credential: r_after_reboot,
        ..
    } = result
    else {
        panic!(
            "a fresh, valid enrollment credential must re-establish a chain for a known Endpoint"
        )
    };
    assert_eq!(
        id_after_reboot, endpoint_id,
        "Endpoint identity must be preserved across a genuine reboot"
    );
    assert_ne!(
        r_after_reboot.lookup_id(),
        r1.lookup_id(),
        "the post-reboot chain must not reuse any pre-reboot runtime credential"
    );

    // No re-approval: identity stays Enrolled, and no new event/audit was
    // committed for this purely credential-dimension transition.
    assert_eq!(identity_state(&db.pool, endpoint_id).await, "Enrolled");
    assert_eq!(
        domain_event_count(&db.pool, endpoint_id).await,
        events_before
    );
    assert_eq!(
        audit_record_count(&db.pool, endpoint_id).await,
        audit_before
    );

    // The lookup pair was fully replaced: E2 is now predecessor, and the
    // pre-reboot runtime credential no longer routes anywhere.
    assert_eq!(
        lookup_id_for_slot(&db.pool, endpoint_id, "predecessor").await,
        Some(e2.lookup_id().to_bytes().to_vec())
    );
    assert_eq!(lookup_row_count(&db.pool, endpoint_id).await, 2);

    let stale_r1_result = enrollment.redeem(&r1.to_wire_value()).await.unwrap();
    assert!(
        matches!(stale_r1_result, RedeemResult::Rejected),
        "the old chain was fully superseded, not merged"
    );

    let resolved = boot_context_resolved_endpoint(&db.pool, &e2.lookup_id().to_bytes()).await;
    assert_eq!(resolved, Some(endpoint_id.0));

    db.teardown().await;
}

#[tokio::test]
async fn revoked_chain_remains_lookup_resolvable_but_rejects_everything() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());

    let e1 = issue_e1(&boot_orchestration, "sim-endpoint-07", clock.now()).await;
    let RedeemResult::Established {
        endpoint_id,
        runtime_credential: r1,
        ..
    } = enrollment.redeem(&e1.to_wire_value()).await.unwrap()
    else {
        panic!();
    };

    enrollment
        .revoke_credential(endpoint_id, clock.now())
        .await
        .unwrap();

    // Revocation must not disturb the credential lookup projection — a
    // revoked chain remains lookup-resolvable and fails closed in Domain
    // instead (ADR-0014 checkpoint "LOOKUP PROJECTION ON ACCEPTED
    // REDEMPTION").
    assert_eq!(lookup_row_count(&db.pool, endpoint_id).await, 2);

    clock.advance(Duration::seconds(1));
    assert!(matches!(
        enrollment.redeem(&r1.to_wire_value()).await.unwrap(),
        RedeemResult::Rejected
    ));
    // Even the original, still within its own validity window, enrollment
    // credential must not resurrect a revoked chain.
    assert!(matches!(
        enrollment.redeem(&e1.to_wire_value()).await.unwrap(),
        RedeemResult::Rejected
    ));

    // A fresh, independently valid E2 does not clear revocation either
    // (owner-approved policy, ADR-0012 point 8 / "Consequences").
    let e2 = issue_e1(&boot_orchestration, "sim-endpoint-07", clock.now()).await;
    assert!(matches!(
        enrollment.redeem(&e2.to_wire_value()).await.unwrap(),
        RedeemResult::Rejected
    ));

    db.teardown().await;
}

#[tokio::test]
async fn approving_unknown_endpoint_fails_explicitly() {
    let db = TestDatabase::setup().await;
    let (_boot, enrollment, clock) = build_services(db.pool.clone());

    let err = enrollment
        .approve_enrollment(EndpointId::new(), Actor::System, clock.now())
        .await
        .unwrap_err();
    assert!(matches!(err, ApplicationError::EndpointNotFound(_)));

    db.teardown().await;
}

#[tokio::test]
async fn durable_state_survives_a_fresh_pool_instance() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());

    let e1 = issue_e1(&boot_orchestration, "sim-endpoint-durable-01", clock.now()).await;
    let RedeemResult::Established { endpoint_id, .. } =
        enrollment.redeem(&e1.to_wire_value()).await.unwrap()
    else {
        panic!("first contact must establish a session");
    };
    enrollment
        .approve_enrollment(
            endpoint_id,
            Actor::Operator {
                label: "wp1-harness".into(),
            },
            clock.now(),
        )
        .await
        .unwrap();

    // A brand-new pool/repository/Application stack against the same
    // database — a different in-process instance than the one that
    // committed the above, so this proves durable persistence rather than
    // an in-process cache.
    let fresh_pool = PgPool::connect(&db.db_url)
        .await
        .expect("reconnect to the same durable database");
    let fresh_repo = Arc::new(PostgresEndpointRepository::new(fresh_pool));
    let fetched = fresh_repo
        .find_by_id(endpoint_id)
        .await
        .unwrap()
        .expect("state committed by the original pool must be visible to a fresh one");
    assert_eq!(fetched.inventory_signal, "sim-endpoint-durable-01");
    assert_eq!(fetched.identity, bamep_domain::IdentityState::Enrolled);

    db.teardown().await;
}

#[tokio::test]
async fn concurrent_first_redemptions_of_the_same_boot_context_resolve_to_one_endpoint() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());
    let enrollment = Arc::new(enrollment);

    let e1 = issue_e1(&boot_orchestration, "sim-endpoint-race-01", clock.now()).await;
    let wire = e1.to_wire_value();

    // Two concurrent AuthRequests presenting the same boot-scoped enrollment
    // credential for a never-before-seen inventory_signal — e.g. a
    // duplicate/retried boot message. The BootContext row lock must
    // serialize these so exactly one Endpoint identity and one
    // EndpointPendingEnrollment event are ever created, never two.
    let enrollment_a = Arc::clone(&enrollment);
    let wire_a = wire.clone();
    let task_a = tokio::spawn(async move { enrollment_a.redeem(&wire_a).await });

    let enrollment_b = Arc::clone(&enrollment);
    let wire_b = wire.clone();
    let task_b = tokio::spawn(async move { enrollment_b.redeem(&wire_b).await });

    let (result_a, result_b) = tokio::join!(task_a, task_b);
    let result_a = result_a.unwrap().unwrap();
    let result_b = result_b.unwrap().unwrap();

    let RedeemResult::Established {
        endpoint_id: id_a, ..
    } = result_a
    else {
        panic!("A must authenticate: E1 is a valid enrollment credential")
    };
    let RedeemResult::Established {
        endpoint_id: id_b, ..
    } = result_b
    else {
        panic!("B must authenticate: E1 is a valid enrollment credential")
    };
    assert_eq!(
        id_a, id_b,
        "both racing requests must resolve to the same Endpoint identity"
    );

    assert_eq!(
        endpoint_row_count(&db.pool, "sim-endpoint-race-01").await,
        1,
        "exactly one Endpoint row must exist for this inventory_signal"
    );
    assert_eq!(
        domain_event_count(&db.pool, id_a).await,
        1,
        "exactly one EndpointPendingEnrollment event must be committed, never two"
    );

    db.teardown().await;
}

#[tokio::test]
async fn concurrent_different_boot_contexts_sharing_inventory_signal_correlate_to_one_endpoint() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());
    let enrollment = Arc::new(enrollment);

    let signal = "sim-endpoint-race-02";
    let e1 = issue_e1(&boot_orchestration, signal, clock.now()).await;
    let e1_prime = issue_e1(&boot_orchestration, signal, clock.now()).await;
    assert_ne!(
        e1.lookup_id(),
        e1_prime.lookup_id(),
        "two independent BootContexts for the same inventory_signal"
    );

    // The two BootContext rows are different, so their own row locks do not
    // serialize this race — the inventory_signal advisory lock
    // (ADR-0014 point 8's concurrency correction, moved to the unresolved-
    // correlation stage) must instead prevent a duplicate Endpoint.
    let enrollment_a = Arc::clone(&enrollment);
    let wire_a = e1.to_wire_value();
    let task_a = tokio::spawn(async move { enrollment_a.redeem(&wire_a).await });

    let enrollment_b = Arc::clone(&enrollment);
    let wire_b = e1_prime.to_wire_value();
    let task_b = tokio::spawn(async move { enrollment_b.redeem(&wire_b).await });

    let (result_a, result_b) = tokio::join!(task_a, task_b);
    let result_a = result_a.unwrap().unwrap();
    let result_b = result_b.unwrap().unwrap();

    let RedeemResult::Established {
        endpoint_id: id_a, ..
    } = result_a
    else {
        panic!("A must authenticate: e1 is a valid enrollment credential")
    };
    let RedeemResult::Established {
        endpoint_id: id_b, ..
    } = result_b
    else {
        panic!("B must authenticate: e1_prime is a valid enrollment credential")
    };

    assert_eq!(
        id_a, id_b,
        "both racing BootContexts sharing one inventory_signal must correlate to the same Endpoint \
         (one via first_contact, the other via genuine_reboot)"
    );
    assert_eq!(
        endpoint_row_count(&db.pool, signal).await,
        1,
        "the advisory lock must prevent a duplicate Endpoint for the shared inventory_signal"
    );

    db.teardown().await;
}

#[tokio::test]
async fn concurrent_same_predecessor_redemption_only_last_commit_wins() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());

    let e1 = issue_e1(&boot_orchestration, "sim-endpoint-race-03", clock.now()).await;
    let RedeemResult::Established { .. } = enrollment.redeem(&e1.to_wire_value()).await.unwrap()
    else {
        panic!();
    };

    // E1's successor was never confirmed — E1 is still a valid predecessor.
    // Two concurrent AuthRequests both retry E1 (two racing reconnect
    // attempts). ADR-0012 point 7: both must individually authenticate, but
    // only the last *committed* successor may remain valid afterward.
    let enrollment = Arc::new(enrollment);
    clock.advance(Duration::seconds(5));

    let enrollment_a = Arc::clone(&enrollment);
    let wire_a = e1.to_wire_value();
    let task_a = tokio::spawn(async move { enrollment_a.redeem(&wire_a).await });

    let enrollment_b = Arc::clone(&enrollment);
    let wire_b = e1.to_wire_value();
    let task_b = tokio::spawn(async move { enrollment_b.redeem(&wire_b).await });

    let (result_a, result_b) = tokio::join!(task_a, task_b);
    let result_a = result_a.unwrap().unwrap();
    let result_b = result_b.unwrap().unwrap();

    let RedeemResult::Established {
        runtime_credential: issued_a,
        ..
    } = result_a
    else {
        panic!("A must authenticate: E1 is still a valid predecessor")
    };
    let RedeemResult::Established {
        runtime_credential: issued_b,
        ..
    } = result_b
    else {
        panic!("B must authenticate: E1 is still a valid predecessor")
    };
    assert_ne!(
        issued_a.lookup_id(),
        issued_b.lookup_id(),
        "each concurrent redemption must mint its own fresh successor"
    );

    // Exactly one of the two racing successors may still be valid — a
    // credential invalidated by the other's later commit must not mint a
    // further successor from stale state.
    clock.advance(Duration::seconds(1));
    let outcome_a = enrollment.redeem(&issued_a.to_wire_value()).await.unwrap();
    let outcome_b = enrollment.redeem(&issued_b.to_wire_value()).await.unwrap();
    let accepted = [&outcome_a, &outcome_b]
        .into_iter()
        .filter(|r| matches!(r, RedeemResult::Established { .. }))
        .count();
    assert_eq!(
        accepted, 1,
        "exactly one of the two racing successors must remain valid — never both, never neither"
    );

    db.teardown().await;
}

/// Proves the timing-boundary fix, not just ordinary expiry: a confirmed
/// runtime predecessor (R1 — an opaque, non-enrollment credential, so it
/// cannot be rescued by the genuine-reboot fallback) is retried while a
/// *separate* transaction holds the exact PostgreSQL row lock the Adapter
/// itself takes on this Endpoint (`shared::load_by_id_for_update`'s `FOR
/// UPDATE`). Only after the concurrent `redeem` call is provably blocked on
/// that lock does the test advance the shared `ManualClock` past R1's expiry
/// and release the lock. If `redeem` used a timestamp captured before the
/// lock wait, it would still see R1 as valid and incorrectly accept it; with
/// the fix, the decision closure reads the clock only once it actually
/// runs — after the lock — and correctly rejects.
#[tokio::test]
async fn commit_time_credential_validity_is_evaluated_after_lock_acquisition() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());
    let enrollment = Arc::new(enrollment.with_credential_ttl(Duration::milliseconds(300)));

    let signal = "sim-endpoint-clock-race-01";
    let t0 = clock.now();
    let e1 = issue_e1(&boot_orchestration, signal, t0).await;
    let RedeemResult::Established {
        endpoint_id,
        runtime_credential: r1,
        ..
    } = enrollment.redeem(&e1.to_wire_value()).await.unwrap()
    else {
        panic!("first contact must establish a session");
    };

    // Confirm R1: it becomes the predecessor. Confirmation does not refresh
    // its expiry — it keeps the 300ms window from t0.
    clock.set(t0 + Duration::milliseconds(10));
    let RedeemResult::Established { .. } = enrollment.redeem(&r1.to_wire_value()).await.unwrap()
    else {
        panic!("R1 must confirm")
    };

    // Hold the exact row lock the Adapter itself takes on this Endpoint, in
    // a separate transaction, so a concurrent `redeem` retrying R1 (still
    // valid at this instant) is forced to block on it.
    let mut lock_tx = db.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM endpoints WHERE id = $1 FOR UPDATE")
        .bind(endpoint_id.0)
        .fetch_one(&mut *lock_tx)
        .await
        .unwrap();

    let enrollment_task = Arc::clone(&enrollment);
    let r1_wire = r1.to_wire_value();
    let redeem_task = tokio::spawn(async move { enrollment_task.redeem(&r1_wire).await });

    // Best-effort only: gives the spawned task a chance to actually reach
    // and block on the lock before we act. Not load-bearing for
    // correctness — the task structurally cannot proceed past the lock
    // until `lock_tx` below is rolled back, regardless of scheduling.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // While the concurrent request is blocked waiting for our lock, advance
    // the clock past R1's expiry (t0 + 300ms).
    clock.set(t0 + Duration::milliseconds(400));

    lock_tx.rollback().await.unwrap(); // release the lock; no writes made

    let result = redeem_task.await.unwrap().unwrap();

    assert!(
        matches!(result, RedeemResult::Rejected),
        "a predecessor that expired while waiting for the commit-time lock must be \
         rejected — the decision must never use a timestamp captured before the lock"
    );

    db.teardown().await;
}

/// Proves whole-transaction rollback under a genuine mid-transaction
/// PostgreSQL failure — not merely a rejected Domain transition (see
/// `invalid_transition_rolls_back_without_partial_writes` above, which only
/// proves the latter). A test-local trigger, installed only in this
/// disposable per-test database and dropped with it at teardown, forces the
/// `endpoint_credential_lookups` insert — a statement issued *after* the
/// `endpoints`/`endpoint_credentials` inserts and the `domain_events` insert
/// have already been sent — to fail deterministically.
#[tokio::test]
async fn first_contact_rolls_back_entirely_when_lookup_projection_fails() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());

    let e1 = issue_e1(&boot_orchestration, "sim-endpoint-rollback-01", clock.now()).await;

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

    let err = enrollment.redeem(&e1.to_wire_value()).await.unwrap_err();
    assert!(matches!(err, ApplicationError::Repository(_)));

    assert_eq!(
        total_endpoint_count(&db.pool).await,
        0,
        "the endpoint insert must not survive rollback"
    );
    let event_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM domain_events")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        event_count, 0,
        "the domain event inserted earlier in the same failed transaction must not survive rollback"
    );

    let resolved = boot_context_resolved_endpoint(&db.pool, &e1.lookup_id().to_bytes()).await;
    assert_eq!(
        resolved, None,
        "BootContext resolution must not survive rollback of a transaction that failed later"
    );

    // CurrentBoot is written as part of the same `endpoints` INSERT that
    // just proved zero rows survived rollback — but assert it explicitly:
    // no row anywhere may carry a replaced/committed current-boot
    // projection from this failed transaction.
    let current_boot_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM endpoints WHERE current_boot_context_id = $1")
            .bind(e1.lookup_id().to_bytes().to_vec())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        current_boot_rows, 0,
        "no Endpoint's current-boot projection may reference this BootContext after rollback"
    );

    db.teardown().await;
}

#[tokio::test]
async fn first_contact_establishes_current_boot_not_established() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());

    let nonce_a = BootNonce::from_bytes([0x01; 32]);
    let e1 = issue_e1_with_nonce(
        &boot_orchestration,
        "sim-current-boot-first-contact",
        nonce_a,
        clock.now(),
    )
    .await;
    let boot_context_id_a = e1.lookup_id().clone();

    let RedeemResult::Established { endpoint_id, .. } =
        enrollment.redeem(&e1.to_wire_value()).await.unwrap()
    else {
        panic!("first contact must establish a session");
    };

    assert_eq!(
        identity_state(&db.pool, endpoint_id).await,
        "PendingEnrollment"
    );

    let current_boot = current_boot_of(&db.pool, endpoint_id).await;
    assert_eq!(current_boot.boot_context_id(), &boot_context_id_a);
    assert_eq!(current_boot.boot_nonce(), nonce_a);
    assert_eq!(
        current_boot.trusted_bootstrap(),
        TrustedBootstrapState::NotEstablished
    );

    let resolved = boot_context_resolved_endpoint(&db.pool, &boot_context_id_a.to_bytes()).await;
    assert_eq!(
        resolved,
        Some(endpoint_id.0),
        "BootContext A must resolve to the same Endpoint, committed atomically with the rest"
    );

    db.teardown().await;
}

#[tokio::test]
async fn same_boot_reconnect_preserves_established_current_boot() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());

    let nonce_a = BootNonce::from_bytes([0x02; 32]);
    let e1 = issue_e1_with_nonce(
        &boot_orchestration,
        "sim-current-boot-same-boot",
        nonce_a,
        clock.now(),
    )
    .await;
    let boot_context_id_a = e1.lookup_id().clone();
    let RedeemResult::Established {
        endpoint_id,
        runtime_credential: r1,
        ..
    } = enrollment.redeem(&e1.to_wire_value()).await.unwrap()
    else {
        panic!("first contact must establish a session");
    };

    // Test-only: see `set_trusted_bootstrap_established` doc comment.
    set_trusted_bootstrap_established(&db.pool, endpoint_id).await;
    let established_before = current_boot_of(&db.pool, endpoint_id).await;
    assert_eq!(
        established_before.trusted_bootstrap(),
        TrustedBootstrapState::Established
    );

    // Same-boot reconnect: a valid runtime successor redemption.
    clock.advance(Duration::seconds(5));
    let result = enrollment.redeem(&r1.to_wire_value()).await.unwrap();
    assert!(matches!(result, RedeemResult::Established { .. }));

    let current_boot_after = current_boot_of(&db.pool, endpoint_id).await;
    assert_eq!(current_boot_after.boot_context_id(), &boot_context_id_a);
    assert_eq!(current_boot_after.boot_nonce(), nonce_a);
    assert_eq!(
        current_boot_after.trusted_bootstrap(),
        TrustedBootstrapState::Established,
        "same-boot credential reconnect/rotation must preserve CurrentBoot exactly, \
         including an already-Established state"
    );

    db.teardown().await;
}

#[tokio::test]
async fn genuine_reboot_replaces_current_boot_and_resets_trust_from_established() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());

    let signal = "sim-current-boot-reboot";
    let nonce_a = BootNonce::from_bytes([0x03; 32]);
    let e1 = issue_e1_with_nonce(&boot_orchestration, signal, nonce_a, clock.now()).await;
    let boot_context_id_a = e1.lookup_id().clone();
    let RedeemResult::Established { endpoint_id, .. } =
        enrollment.redeem(&e1.to_wire_value()).await.unwrap()
    else {
        panic!("first contact must establish a session");
    };

    // Test-only: see `set_trusted_bootstrap_established` doc comment. The
    // production genuine_reboot Domain transition itself must perform the
    // reset below, independent of this test-only setup.
    set_trusted_bootstrap_established(&db.pool, endpoint_id).await;

    clock.advance(Duration::hours(2));
    let nonce_b = BootNonce::from_bytes([0x04; 32]);
    assert_ne!(nonce_a, nonce_b);
    let e2 = issue_e1_with_nonce(&boot_orchestration, signal, nonce_b, clock.now()).await;
    let boot_context_id_b = e2.lookup_id().clone();
    assert_ne!(boot_context_id_a, boot_context_id_b);

    let result = enrollment.redeem(&e2.to_wire_value()).await.unwrap();
    let RedeemResult::Established {
        endpoint_id: id_after_reboot,
        ..
    } = result
    else {
        panic!(
            "a fresh, valid enrollment credential must re-establish a chain for a known Endpoint"
        )
    };
    assert_eq!(
        id_after_reboot, endpoint_id,
        "EndpointId/identity must be preserved across a genuine reboot"
    );

    let current_boot = current_boot_of(&db.pool, endpoint_id).await;
    assert_eq!(current_boot.boot_context_id(), &boot_context_id_b);
    assert_ne!(current_boot.boot_context_id(), &boot_context_id_a);
    assert_eq!(current_boot.boot_nonce(), nonce_b);
    assert_ne!(current_boot.boot_nonce(), nonce_a);
    assert_eq!(
        current_boot.trusted_bootstrap(),
        TrustedBootstrapState::NotEstablished,
        "genuine reboot must reset trusted bootstrap to NotEstablished even though the \
         previous CurrentBoot was Established"
    );

    let resolved = boot_context_resolved_endpoint(&db.pool, &boot_context_id_b.to_bytes()).await;
    assert_eq!(resolved, Some(endpoint_id.0));

    db.teardown().await;
}

#[tokio::test]
async fn rejected_credential_does_not_change_current_boot() {
    let db = TestDatabase::setup().await;
    let (boot_orchestration, enrollment, clock) = build_services(db.pool.clone());

    let nonce_a = BootNonce::from_bytes([0x06; 32]);
    let e1 = issue_e1_with_nonce(
        &boot_orchestration,
        "sim-current-boot-rejection",
        nonce_a,
        clock.now(),
    )
    .await;
    let RedeemResult::Established { endpoint_id, .. } =
        enrollment.redeem(&e1.to_wire_value()).await.unwrap()
    else {
        panic!("first contact must establish a session");
    };
    set_trusted_bootstrap_established(&db.pool, endpoint_id).await;

    let before = current_boot_of(&db.pool, endpoint_id).await;

    // A well-formed but unrelated credential — never authenticates against
    // this (or any) chain.
    let bogus = PresentedCredential::generate(CredentialKind::Runtime);
    let rejected = enrollment.redeem(&bogus.to_wire_value()).await.unwrap();
    assert!(matches!(rejected, RedeemResult::Rejected));

    let after = current_boot_of(&db.pool, endpoint_id).await;
    assert_eq!(
        after, before,
        "a rejected redemption must change none of current_boot's fields"
    );

    db.teardown().await;
}

#[tokio::test]
async fn historical_boot_context_with_null_nonce_fails_closed_on_redemption() {
    let db = TestDatabase::setup().await;
    let (_boot, enrollment, clock) = build_services(db.pool.clone());

    // Simulates an unknown historical BootContext row: real, schema-valid
    // columns, but NULL boot_nonce. The nullable baseline representation keeps
    // such rows fail-closed without fabricating or backfilling a nonce.
    // Inserted directly against the schema, not through
    // BootOrchestrationService, which always supplies a non-NULL nonce for
    // newly issued BootContexts.
    let credential = PresentedCredential::generate(CredentialKind::Enrollment);
    let verifier = CredentialHash::of_bytes(credential.secret().expose_secret_bytes());
    let now = clock.now();
    sqlx::query(
        "INSERT INTO boot_contexts \
         (boot_context_id, verifier, issued_at, expires_at, inventory_signal, boot_nonce) \
         VALUES ($1, $2, $3, $4, $5, NULL)",
    )
    .bind(credential.lookup_id().to_bytes().to_vec())
    .bind(verifier.to_bytes().to_vec())
    .bind(now)
    .bind(now + Duration::minutes(5))
    .bind("sim-historical-null-nonce")
    .execute(&db.pool)
    .await
    .unwrap();

    let err = enrollment
        .redeem(&credential.to_wire_value())
        .await
        .unwrap_err();
    assert!(
        matches!(err, ApplicationError::Repository(_)),
        "a historical BootContext row with a NULL boot_nonce must fail closed rather than be \
         silently accepted as a valid new-flow BootContext"
    );
    assert_eq!(total_endpoint_count(&db.pool).await, 0);

    db.teardown().await;
}
