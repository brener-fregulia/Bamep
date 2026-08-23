//! Component/Integration tests for the final BootContext and credential-lookup
//! baseline schema (ADR-0014): `boot_contexts`,
//! `endpoint_credential_lookups`, the `credential_slot_role` enum, and their
//! constraints.
//!
//! These tests assert PostgreSQL schema invariants directly via
//! `information_schema`/`pg_catalog` and raw SQL, independently of
//! Domain/Application credential behavior.
//!
//! Requires a real, reachable PostgreSQL instance — see
//! `support::TestDatabase`.

mod support;

use chrono::{Duration, Utc};
use sqlx::PgPool;
use support::TestDatabase;
use uuid::Uuid;

/// Inserts a minimal, otherwise-unrelated `endpoints` row so FK-bearing
/// tables in this file have a legitimate row to reference.
async fn insert_endpoint(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO endpoints (id, inventory_signal, identity_state, hardware_confidence, created_at, updated_at) \
         VALUES ($1, $2, 'PendingEnrollment', 'Consistent', $3, $3)",
    )
    .bind(id)
    .bind(format!("schema-test-{id}"))
    .bind(now)
    .execute(pool)
    .await
    .expect("insert baseline endpoint row");
    id
}

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

fn valid_boot_context_id() -> Vec<u8> {
    vec![0xAB; 16]
}

fn valid_verifier() -> Vec<u8> {
    vec![0xCD; 48]
}

fn valid_lookup_id() -> Vec<u8> {
    vec![0xEF; 16]
}

#[tokio::test]
async fn boot_contexts_table_and_columns_match_schema() {
    let db = TestDatabase::setup().await;

    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_name = 'boot_contexts')",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(exists, "boot_contexts table must exist");

    assert_eq!(
        udt_name(&db.pool, "boot_contexts", "boot_context_id").await,
        "bytea"
    );
    assert_eq!(
        udt_name(&db.pool, "boot_contexts", "verifier").await,
        "bytea"
    );
    assert_eq!(
        udt_name(&db.pool, "boot_contexts", "issued_at").await,
        "timestamptz"
    );
    assert_eq!(
        udt_name(&db.pool, "boot_contexts", "expires_at").await,
        "timestamptz"
    );
    assert_eq!(
        udt_name(&db.pool, "boot_contexts", "inventory_signal").await,
        "text"
    );
    assert_eq!(
        udt_name(&db.pool, "boot_contexts", "resolved_endpoint_id").await,
        "uuid"
    );

    db.teardown().await;
}

#[tokio::test]
async fn credential_slot_role_is_native_postgres_enum_with_expected_labels() {
    let db = TestDatabase::setup().await;

    let type_kind: String = sqlx::query_scalar(
        "SELECT typtype::text FROM pg_type WHERE typname = 'credential_slot_role'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(type_kind, "e", "credential_slot_role must be a native enum");

    let labels: Vec<String> = sqlx::query_scalar(
        "SELECT enumlabel FROM pg_enum \
         JOIN pg_type ON pg_enum.enumtypid = pg_type.oid \
         WHERE pg_type.typname = 'credential_slot_role' \
         ORDER BY enumsortorder",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(labels, vec!["predecessor", "successor"]);

    db.teardown().await;
}

#[tokio::test]
async fn endpoint_credential_lookups_table_and_columns_match_schema() {
    let db = TestDatabase::setup().await;

    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_name = 'endpoint_credential_lookups')",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(exists, "endpoint_credential_lookups table must exist");

    assert_eq!(
        udt_name(&db.pool, "endpoint_credential_lookups", "endpoint_id").await,
        "uuid"
    );
    assert_eq!(
        udt_name(&db.pool, "endpoint_credential_lookups", "slot").await,
        "credential_slot_role"
    );
    assert_eq!(
        udt_name(&db.pool, "endpoint_credential_lookups", "lookup_id").await,
        "bytea"
    );

    db.teardown().await;
}

#[tokio::test]
async fn boot_context_id_rejects_a_value_not_exactly_16_bytes() {
    let db = TestDatabase::setup().await;
    let now = Utc::now();

    let result = sqlx::query(
        "INSERT INTO boot_contexts (boot_context_id, verifier, issued_at, expires_at, inventory_signal) \
         VALUES ($1, $2, $3, $4, 'sim-e1')",
    )
    .bind(vec![0u8; 15])
    .bind(valid_verifier())
    .bind(now)
    .bind(now + Duration::minutes(5))
    .execute(&db.pool)
    .await;

    assert!(
        result.is_err(),
        "a 15-byte boot_context_id must be rejected"
    );

    db.teardown().await;
}

#[tokio::test]
async fn verifier_rejects_a_value_not_exactly_48_bytes() {
    let db = TestDatabase::setup().await;
    let now = Utc::now();

    let result = sqlx::query(
        "INSERT INTO boot_contexts (boot_context_id, verifier, issued_at, expires_at, inventory_signal) \
         VALUES ($1, $2, $3, $4, 'sim-e1')",
    )
    .bind(valid_boot_context_id())
    .bind(vec![0u8; 47])
    .bind(now)
    .bind(now + Duration::minutes(5))
    .execute(&db.pool)
    .await;

    assert!(result.is_err(), "a 47-byte verifier must be rejected");

    db.teardown().await;
}

#[tokio::test]
async fn lookup_id_rejects_a_value_not_exactly_16_bytes() {
    let db = TestDatabase::setup().await;
    let endpoint_id = insert_endpoint(&db.pool).await;

    let result = sqlx::query(
        "INSERT INTO endpoint_credential_lookups (endpoint_id, slot, lookup_id) \
         VALUES ($1, 'predecessor', $2)",
    )
    .bind(endpoint_id)
    .bind(vec![0u8; 17])
    .execute(&db.pool)
    .await;

    assert!(result.is_err(), "a 17-byte lookup_id must be rejected");

    db.teardown().await;
}

#[tokio::test]
async fn expires_at_not_after_issued_at_is_rejected() {
    let db = TestDatabase::setup().await;
    let now = Utc::now();

    let result = sqlx::query(
        "INSERT INTO boot_contexts (boot_context_id, verifier, issued_at, expires_at, inventory_signal) \
         VALUES ($1, $2, $3, $3, 'sim-e1')",
    )
    .bind(valid_boot_context_id())
    .bind(valid_verifier())
    .bind(now)
    .execute(&db.pool)
    .await;

    assert!(
        result.is_err(),
        "expires_at equal to issued_at must be rejected (no unresolved-enrollment grace period)"
    );

    db.teardown().await;
}

#[tokio::test]
async fn duplicate_lookup_id_across_different_endpoints_is_rejected() {
    let db = TestDatabase::setup().await;
    let endpoint_a = insert_endpoint(&db.pool).await;
    let endpoint_b = insert_endpoint(&db.pool).await;
    let shared_lookup_id = valid_lookup_id();

    sqlx::query(
        "INSERT INTO endpoint_credential_lookups (endpoint_id, slot, lookup_id) \
         VALUES ($1, 'predecessor', $2)",
    )
    .bind(endpoint_a)
    .bind(shared_lookup_id.clone())
    .execute(&db.pool)
    .await
    .expect("first insert establishes the lookup_id");

    // Same lookup_id, different Endpoint and different slot (successor) —
    // the global UNIQUE(lookup_id) must still reject it, proving routing
    // ambiguity is structurally impossible rather than merely improbable.
    let result = sqlx::query(
        "INSERT INTO endpoint_credential_lookups (endpoint_id, slot, lookup_id) \
         VALUES ($1, 'successor', $2)",
    )
    .bind(endpoint_b)
    .bind(shared_lookup_id)
    .execute(&db.pool)
    .await;

    assert!(
        result.is_err(),
        "a lookup_id already used by another Endpoint/slot must be rejected"
    );

    db.teardown().await;
}

#[tokio::test]
async fn duplicate_slot_for_the_same_endpoint_is_rejected() {
    let db = TestDatabase::setup().await;
    let endpoint_id = insert_endpoint(&db.pool).await;

    sqlx::query(
        "INSERT INTO endpoint_credential_lookups (endpoint_id, slot, lookup_id) \
         VALUES ($1, 'predecessor', $2)",
    )
    .bind(endpoint_id)
    .bind(vec![0x01u8; 16])
    .execute(&db.pool)
    .await
    .expect("first predecessor row for this endpoint");

    let result = sqlx::query(
        "INSERT INTO endpoint_credential_lookups (endpoint_id, slot, lookup_id) \
         VALUES ($1, 'predecessor', $2)",
    )
    .bind(endpoint_id)
    .bind(vec![0x02u8; 16])
    .execute(&db.pool)
    .await;

    assert!(
        result.is_err(),
        "a second predecessor row for the same endpoint must be rejected by the primary key"
    );

    db.teardown().await;
}

#[tokio::test]
async fn predecessor_and_successor_rows_for_the_same_endpoint_are_accepted() {
    let db = TestDatabase::setup().await;
    let endpoint_id = insert_endpoint(&db.pool).await;

    sqlx::query(
        "INSERT INTO endpoint_credential_lookups (endpoint_id, slot, lookup_id) \
         VALUES ($1, 'predecessor', $2)",
    )
    .bind(endpoint_id)
    .bind(vec![0x01u8; 16])
    .execute(&db.pool)
    .await
    .expect("predecessor row");

    sqlx::query(
        "INSERT INTO endpoint_credential_lookups (endpoint_id, slot, lookup_id) \
         VALUES ($1, 'successor', $2)",
    )
    .bind(endpoint_id)
    .bind(vec![0x02u8; 16])
    .execute(&db.pool)
    .await
    .expect("successor row for the same endpoint must be accepted alongside its predecessor");

    let row_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM endpoint_credential_lookups WHERE endpoint_id = $1",
    )
    .bind(endpoint_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(row_count, 2);

    db.teardown().await;
}

#[tokio::test]
async fn multiple_boot_contexts_may_share_the_same_inventory_signal() {
    let db = TestDatabase::setup().await;
    let now = Utc::now();

    for suffix in [0x01u8, 0x02u8] {
        let mut boot_context_id = valid_boot_context_id();
        boot_context_id[0] = suffix;
        sqlx::query(
            "INSERT INTO boot_contexts (boot_context_id, verifier, issued_at, expires_at, inventory_signal) \
             VALUES ($1, $2, $3, $4, 'sim-shared-signal')",
        )
        .bind(boot_context_id)
        .bind(valid_verifier())
        .bind(now)
        .bind(now + Duration::minutes(5))
        .execute(&db.pool)
        .await
        .expect("a second BootContext with the same inventory_signal must be accepted");
    }

    let row_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM boot_contexts WHERE inventory_signal = 'sim-shared-signal'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(row_count, 2);

    db.teardown().await;
}

#[tokio::test]
async fn resolved_endpoint_id_is_nullable_and_respects_the_endpoint_foreign_key() {
    let db = TestDatabase::setup().await;
    let now = Utc::now();

    // NULL (unresolved) is accepted.
    let mut unresolved_id = valid_boot_context_id();
    unresolved_id[0] = 0x01;
    sqlx::query(
        "INSERT INTO boot_contexts (boot_context_id, verifier, issued_at, expires_at, inventory_signal, resolved_endpoint_id) \
         VALUES ($1, $2, $3, $4, 'sim-e1', NULL)",
    )
    .bind(unresolved_id)
    .bind(valid_verifier())
    .bind(now)
    .bind(now + Duration::minutes(5))
    .execute(&db.pool)
    .await
    .expect("unresolved BootContext (NULL resolved_endpoint_id) must be accepted");

    // A real Endpoint id is accepted.
    let endpoint_id = insert_endpoint(&db.pool).await;
    let mut resolved_id = valid_boot_context_id();
    resolved_id[0] = 0x02;
    sqlx::query(
        "INSERT INTO boot_contexts (boot_context_id, verifier, issued_at, expires_at, inventory_signal, resolved_endpoint_id) \
         VALUES ($1, $2, $3, $4, 'sim-e1', $5)",
    )
    .bind(resolved_id)
    .bind(valid_verifier())
    .bind(now)
    .bind(now + Duration::minutes(5))
    .bind(endpoint_id)
    .execute(&db.pool)
    .await
    .expect("resolved_endpoint_id pointing at a real Endpoint must be accepted");

    // A non-existent Endpoint id must be rejected by the foreign key.
    let mut dangling_id = valid_boot_context_id();
    dangling_id[0] = 0x03;
    let result = sqlx::query(
        "INSERT INTO boot_contexts (boot_context_id, verifier, issued_at, expires_at, inventory_signal, resolved_endpoint_id) \
         VALUES ($1, $2, $3, $4, 'sim-e1', $5)",
    )
    .bind(dangling_id)
    .bind(valid_verifier())
    .bind(now)
    .bind(now + Duration::minutes(5))
    .bind(Uuid::new_v4())
    .execute(&db.pool)
    .await;
    assert!(
        result.is_err(),
        "resolved_endpoint_id must respect the endpoints foreign key"
    );

    db.teardown().await;
}
