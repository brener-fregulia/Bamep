//! Component/Integration tests for the `BootContextRepository` Port
//! (`crates/server/src/ports.rs`) against its real `PostgresBootContextRepository`
//! Adapter and a real PostgreSQL instance. This is the ADR-0014 point 11
//! ("Persist-before-deliver issuance ordering") persistence prerequisite:
//! proves durable insertion of a newly issued `BootContext`, exact column
//! mapping, duplicate-`boot_context_id` rejection, and that only the one-way
//! verifier — never a plaintext secret — is stored. Credential
//! redemption/routing has no code path yet and is not exercised here.
//!
//! Requires a real, reachable PostgreSQL instance — see `support::TestDatabase`.

mod support;

use bamep_domain::credential::CredentialHash;
use bamep_domain::presented_credential::{CredentialLookupId, PresentedCredentialSecret};
use bamep_domain::{BootContext, BootNonce, EndpointId};
use bamep_server::adapters::postgres::PostgresBootContextRepository;
use bamep_server::ports::{BootContextRepository, RepositoryError};
use chrono::{Duration, SubsecRound, Utc};
use sqlx::{PgPool, Row};
use support::TestDatabase;
use uuid::Uuid;

/// Inserts a minimal, otherwise-unrelated `endpoints` row so the
/// `resolved_endpoint_id` foreign key has a legitimate row to reference,
/// matching `boot_context_and_credential_lookup_schema.rs`'s helper.
async fn insert_endpoint(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO endpoints (id, inventory_signal, identity_state, hardware_confidence, created_at, updated_at) \
         VALUES ($1, $2, 'PendingEnrollment', 'Consistent', $3, $3)",
    )
    .bind(id)
    .bind(format!("boot-context-repo-test-{id}"))
    .bind(now)
    .execute(pool)
    .await
    .expect("insert baseline endpoint row");
    id
}

/// PostgreSQL `TIMESTAMPTZ` stores microsecond precision; truncating here
/// keeps round-trip comparisons exact without depending on the platform
/// clock's actual sub-microsecond resolution.
fn now_micros() -> chrono::DateTime<Utc> {
    Utc::now().trunc_subsecs(6)
}

fn test_boot_nonce() -> BootNonce {
    BootNonce::from_bytes([0x5A; 32])
}

fn fresh_unresolved_context(secret: &PresentedCredentialSecret) -> BootContext {
    let now = now_micros();
    BootContext::new(
        CredentialLookupId::generate(),
        CredentialHash::of_bytes(secret.expose_secret_bytes()),
        now,
        now + Duration::minutes(5),
        "sim-boot-context-repo".into(),
        test_boot_nonce(),
    )
}

async fn fetch_boot_context_row(
    pool: &PgPool,
    boot_context_id: &[u8],
) -> Option<sqlx::postgres::PgRow> {
    sqlx::query(
        "SELECT boot_context_id, verifier, issued_at, expires_at, inventory_signal, resolved_endpoint_id, boot_nonce \
         FROM boot_contexts WHERE boot_context_id = $1",
    )
    .bind(boot_context_id)
    .fetch_optional(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn inserting_an_unresolved_boot_context_succeeds_and_persists_exact_fields() {
    let db = TestDatabase::setup().await;
    let repo = PostgresBootContextRepository::new(db.pool.clone());

    let secret = PresentedCredentialSecret::generate();
    let context = fresh_unresolved_context(&secret);

    repo.insert_boot_context(&context)
        .await
        .expect("insert of a fresh, unresolved BootContext must succeed");

    let row = fetch_boot_context_row(&db.pool, &context.boot_context_id().to_bytes())
        .await
        .expect("row must exist after a successful insert");

    let persisted_id: Vec<u8> = row.try_get("boot_context_id").unwrap();
    let persisted_verifier: Vec<u8> = row.try_get("verifier").unwrap();
    let persisted_issued_at: chrono::DateTime<Utc> = row.try_get("issued_at").unwrap();
    let persisted_expires_at: chrono::DateTime<Utc> = row.try_get("expires_at").unwrap();
    let persisted_signal: String = row.try_get("inventory_signal").unwrap();
    let persisted_resolved: Option<Uuid> = row.try_get("resolved_endpoint_id").unwrap();
    let persisted_nonce: Option<Vec<u8>> = row.try_get("boot_nonce").unwrap();

    assert_eq!(persisted_id, context.boot_context_id().to_bytes().to_vec());
    assert_eq!(persisted_verifier, context.verifier().to_bytes().to_vec());
    assert_eq!(persisted_issued_at, context.issued_at());
    assert_eq!(persisted_expires_at, context.expires_at());
    assert_eq!(persisted_signal, context.inventory_signal());
    assert_eq!(
        persisted_resolved, None,
        "a newly issued BootContext must persist a NULL resolved_endpoint_id"
    );
    assert_eq!(
        persisted_nonce,
        Some(context.boot_nonce().as_bytes().to_vec()),
        "the exact 32-byte BootNonce must be persisted"
    );

    db.teardown().await;
}

#[tokio::test]
async fn boot_context_boot_nonce_round_trips_exactly_through_a_fresh_connection() {
    let db = TestDatabase::setup().await;
    let repo = PostgresBootContextRepository::new(db.pool.clone());

    let secret = PresentedCredentialSecret::generate();
    let context = fresh_unresolved_context(&secret);

    repo.insert_boot_context(&context)
        .await
        .expect("insert must succeed");

    // A separate pool/connection than the one the repo inserted through —
    // proving durable commit, not merely in-process/same-connection
    // visibility, matching this crate's other durability-proof style.
    let fresh_pool = PgPool::connect(&db.db_url)
        .await
        .expect("reconnect to the same durable database");
    let row = fetch_boot_context_row(&fresh_pool, &context.boot_context_id().to_bytes())
        .await
        .expect("row must be visible to a fresh connection");

    let persisted_nonce_bytes: Vec<u8> = row
        .try_get::<Option<Vec<u8>>, _>("boot_nonce")
        .unwrap()
        .expect("a newly inserted BootContext always has a non-NULL boot_nonce");
    let reconstructed_array: [u8; 32] = persisted_nonce_bytes
        .try_into()
        .expect("persisted boot_nonce must be exactly 32 bytes");
    let reconstructed = BootNonce::from_bytes(reconstructed_array);

    assert_eq!(
        reconstructed,
        context.boot_nonce(),
        "reconstructing BootNonce from its persisted bytes must equal the original exactly"
    );

    db.teardown().await;
}

#[tokio::test]
async fn inserting_a_resolved_boot_context_preserves_the_resolved_endpoint_id() {
    let db = TestDatabase::setup().await;
    let repo = PostgresBootContextRepository::new(db.pool.clone());
    let endpoint_uuid = insert_endpoint(&db.pool).await;

    let secret = PresentedCredentialSecret::generate();
    let now = now_micros();
    let context = BootContext::from_parts(
        CredentialLookupId::generate(),
        CredentialHash::of_bytes(secret.expose_secret_bytes()),
        now,
        now + Duration::minutes(5),
        "sim-boot-context-repo-resolved".into(),
        Some(EndpointId(endpoint_uuid)),
        test_boot_nonce(),
    );

    repo.insert_boot_context(&context)
        .await
        .expect("insert of a resolved BootContext referencing an existing Endpoint must succeed");

    let row = fetch_boot_context_row(&db.pool, &context.boot_context_id().to_bytes())
        .await
        .expect("row must exist after a successful insert");
    let persisted_resolved: Option<Uuid> = row.try_get("resolved_endpoint_id").unwrap();

    assert_eq!(persisted_resolved, Some(endpoint_uuid));

    db.teardown().await;
}

#[tokio::test]
async fn duplicate_boot_context_id_is_rejected_and_does_not_overwrite_the_original_row() {
    let db = TestDatabase::setup().await;
    let repo = PostgresBootContextRepository::new(db.pool.clone());

    let original_secret = PresentedCredentialSecret::generate();
    let original = fresh_unresolved_context(&original_secret);
    repo.insert_boot_context(&original)
        .await
        .expect("first insert establishes the row");

    // Same boot_context_id, but otherwise different content — simulating an
    // (illegitimate) attempt to re-issue against the same identifier.
    let colliding_secret = PresentedCredentialSecret::generate();
    let now = now_micros();
    let colliding = BootContext::from_parts(
        original.boot_context_id().clone(),
        CredentialHash::of_bytes(colliding_secret.expose_secret_bytes()),
        now,
        now + Duration::minutes(30),
        "sim-boot-context-repo-colliding".into(),
        None,
        test_boot_nonce(),
    );

    let result = repo.insert_boot_context(&colliding).await;
    assert!(
        matches!(result, Err(RepositoryError::Backend(_))),
        "a colliding boot_context_id must surface a persistence error, not silently succeed"
    );

    let row = fetch_boot_context_row(&db.pool, &original.boot_context_id().to_bytes())
        .await
        .expect("the original row must still exist");
    let persisted_verifier: Vec<u8> = row.try_get("verifier").unwrap();
    let persisted_signal: String = row.try_get("inventory_signal").unwrap();

    assert_eq!(
        persisted_verifier,
        original.verifier().to_bytes().to_vec(),
        "the original row's verifier must not be overwritten by the rejected duplicate"
    );
    assert_eq!(
        persisted_signal,
        original.inventory_signal(),
        "the original row's inventory_signal must not be overwritten by the rejected duplicate"
    );

    let row_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM boot_contexts WHERE boot_context_id = $1")
            .bind(original.boot_context_id().to_bytes().to_vec())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        row_count, 1,
        "exactly one row must exist for this boot_context_id"
    );

    db.teardown().await;
}

/// No plaintext credential secret is ever passed to this Port at all — its
/// signature accepts only `&BootContext`, which structurally never holds one
/// (`bamep_domain::boot_context::BootContext` stores only a `CredentialHash`
/// verifier). This test additionally proves the persisted `verifier` column
/// is functionally the one-way hash, not the plaintext secret: it is 48
/// bytes (`CredentialHash::to_bytes`), never the 32-byte raw secret, and an
/// independently reconstructed `CredentialHash` from the persisted bytes
/// still verifies the original secret.
#[tokio::test]
async fn no_plaintext_credential_secret_is_stored_by_this_operation() {
    let db = TestDatabase::setup().await;
    let repo = PostgresBootContextRepository::new(db.pool.clone());

    let secret = PresentedCredentialSecret::generate();
    let context = fresh_unresolved_context(&secret);

    repo.insert_boot_context(&context)
        .await
        .expect("insert must succeed");

    let row = fetch_boot_context_row(&db.pool, &context.boot_context_id().to_bytes())
        .await
        .expect("row must exist after a successful insert");
    let persisted_verifier: Vec<u8> = row.try_get("verifier").unwrap();

    assert_eq!(
        persisted_verifier.len(),
        48,
        "the persisted verifier must be the 48-byte CredentialHash, not a raw secret"
    );
    assert_ne!(
        persisted_verifier,
        secret.expose_secret_bytes().to_vec(),
        "the persisted verifier must never equal the plaintext secret bytes"
    );

    let reconstructed =
        CredentialHash::from_bytes(persisted_verifier.try_into().expect("checked length above"));
    assert!(
        reconstructed.verify_bytes(secret.expose_secret_bytes()),
        "the persisted verifier must still functionally verify the original secret \
         through the one-way hash mechanism"
    );

    db.teardown().await;
}
