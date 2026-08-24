//! Row-mapping and persistence helpers shared between
//! [`super::repository::PostgresEndpointRepository`] and
//! [`super::credential_redemption_repository::PostgresCredentialRedemptionRepository`],
//! so neither Adapter duplicates complex row mapping or event/audit
//! persistence (ADR-0014 checkpoint "ENDPOINTREPOSITORY CLEANUP": "Reuse/
//! extract internal PostgreSQL helpers ... Keep such extraction private to
//! the Adapter layer"). Nothing here is exported outside `adapters::postgres`.

use bamep_domain::credential::{CredentialChain, CredentialHash, CredentialSlot};
use bamep_domain::presented_credential::CredentialLookupId;
use bamep_domain::{
    Actor, BootContext, BootNonce, CurrentBoot, DomainEvent, EndpointAggregate, EndpointId,
    HardwareConfidence, TransitionOutcome, TrustedBootstrapState,
};
use sqlx::{Postgres, Row, Transaction};

use crate::ports::RepositoryError;

pub(super) fn to_backend_err(e: sqlx::Error) -> RepositoryError {
    RepositoryError::Backend(e.to_string())
}

/// Adapter-local representation of the `endpoint_identity_state` PostgreSQL
/// ENUM (`docs/development/persistence.md`, "Closed categorical values"). Domain
/// (`bamep_domain::IdentityState`) stays free of SQLx/PostgreSQL derives —
/// this type exists only to give SQLx a Postgres-typed value to bind/decode,
/// mapped explicitly to/from Domain below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "endpoint_identity_state")]
pub(super) enum PgIdentityState {
    PendingEnrollment,
    Enrolled,
    Retired,
}

impl From<bamep_domain::IdentityState> for PgIdentityState {
    fn from(state: bamep_domain::IdentityState) -> Self {
        match state {
            bamep_domain::IdentityState::PendingEnrollment => PgIdentityState::PendingEnrollment,
            bamep_domain::IdentityState::Enrolled => PgIdentityState::Enrolled,
            bamep_domain::IdentityState::Retired => PgIdentityState::Retired,
        }
    }
}

impl From<PgIdentityState> for bamep_domain::IdentityState {
    fn from(state: PgIdentityState) -> Self {
        match state {
            PgIdentityState::PendingEnrollment => bamep_domain::IdentityState::PendingEnrollment,
            PgIdentityState::Enrolled => bamep_domain::IdentityState::Enrolled,
            PgIdentityState::Retired => bamep_domain::IdentityState::Retired,
        }
    }
}

/// Adapter-local representation of the `hardware_confidence_state` PostgreSQL
/// ENUM (`docs/development/persistence.md`, "Closed categorical values").
/// Domain (`bamep_domain::HardwareConfidence`) stays free of SQLx/PostgreSQL
/// derives — mapped explicitly to/from Domain below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "hardware_confidence_state")]
pub(super) enum PgHardwareConfidence {
    Consistent,
    LoweredConfidence,
    Conflict,
}

impl From<HardwareConfidence> for PgHardwareConfidence {
    fn from(state: HardwareConfidence) -> Self {
        match state {
            HardwareConfidence::Consistent => PgHardwareConfidence::Consistent,
            HardwareConfidence::LoweredConfidence => PgHardwareConfidence::LoweredConfidence,
            HardwareConfidence::Conflict => PgHardwareConfidence::Conflict,
        }
    }
}

impl From<PgHardwareConfidence> for HardwareConfidence {
    fn from(state: PgHardwareConfidence) -> Self {
        match state {
            PgHardwareConfidence::Consistent => HardwareConfidence::Consistent,
            PgHardwareConfidence::LoweredConfidence => HardwareConfidence::LoweredConfidence,
            PgHardwareConfidence::Conflict => HardwareConfidence::Conflict,
        }
    }
}

/// Adapter-local representation of the `domain_event_type` PostgreSQL ENUM.
/// Only a write direction is needed today: nothing currently reads
/// `domain_events.event_type` back into a Domain type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "domain_event_type")]
pub(super) enum PgDomainEventType {
    EndpointPendingEnrollment,
    EndpointEnrolled,
    OperatorDecisionRecorded,
    InventoryRevisionRecorded,
    JobStarted,
    JobSucceeded,
    JobFailed,
    JobStepFailed,
    JobCancelled,
    AttemptIndeterminate,
}

impl From<&DomainEvent> for PgDomainEventType {
    fn from(event: &DomainEvent) -> Self {
        match event {
            DomainEvent::EndpointPendingEnrollment { .. } => {
                PgDomainEventType::EndpointPendingEnrollment
            }
            DomainEvent::EndpointEnrolled { .. } => PgDomainEventType::EndpointEnrolled,
            DomainEvent::OperatorDecisionRecorded { .. } => {
                PgDomainEventType::OperatorDecisionRecorded
            }
            DomainEvent::InventoryRevisionRecorded { .. } => {
                PgDomainEventType::InventoryRevisionRecorded
            }
            DomainEvent::JobStarted { .. } => PgDomainEventType::JobStarted,
            DomainEvent::JobSucceeded { .. } => PgDomainEventType::JobSucceeded,
            DomainEvent::JobFailed { .. } => PgDomainEventType::JobFailed,
            DomainEvent::JobStepFailed { .. } => PgDomainEventType::JobStepFailed,
            DomainEvent::JobCancelled { .. } => PgDomainEventType::JobCancelled,
            DomainEvent::AttemptIndeterminate { .. } => PgDomainEventType::AttemptIndeterminate,
        }
    }
}

/// Adapter-local representation of the `audit_actor_kind` PostgreSQL ENUM.
/// Only a write direction is needed today: nothing currently reads
/// `audit_records.actor_kind` back into a Domain type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "audit_actor_kind", rename_all = "lowercase")]
pub(super) enum PgAuditActorKind {
    Operator,
    System,
}

impl From<&Actor> for PgAuditActorKind {
    fn from(actor: &Actor) -> Self {
        match actor {
            Actor::Operator { .. } => PgAuditActorKind::Operator,
            Actor::System => PgAuditActorKind::System,
        }
    }
}

/// Adapter-local representation of the `credential_slot_role` PostgreSQL
/// ENUM (`0001_initial_schema.sql`). Not a
/// Domain type (`docs/development/persistence.md` "Closed categorical
/// values"): `bamep_domain::credential::CredentialChain` only knows
/// "predecessor" and "successor" as its two struct-level slots, never as a
/// tagged enum value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "credential_slot_role", rename_all = "lowercase")]
pub(super) enum PgCredentialSlotRole {
    Predecessor,
    Successor,
}

/// Adapter-local representation of the `trusted_bootstrap_state` PostgreSQL
/// ENUM (`0001_initial_schema.sql`).
/// Domain (`bamep_domain::TrustedBootstrapState`) stays free of SQLx/
/// PostgreSQL derives — mapped explicitly to/from Domain below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "trusted_bootstrap_state")]
pub(super) enum PgTrustedBootstrapState {
    NotEstablished,
    Established,
}

impl From<TrustedBootstrapState> for PgTrustedBootstrapState {
    fn from(state: TrustedBootstrapState) -> Self {
        match state {
            TrustedBootstrapState::NotEstablished => PgTrustedBootstrapState::NotEstablished,
            TrustedBootstrapState::Established => PgTrustedBootstrapState::Established,
        }
    }
}

impl From<PgTrustedBootstrapState> for TrustedBootstrapState {
    fn from(state: PgTrustedBootstrapState) -> Self {
        match state {
            PgTrustedBootstrapState::NotEstablished => TrustedBootstrapState::NotEstablished,
            PgTrustedBootstrapState::Established => TrustedBootstrapState::Established,
        }
    }
}

pub(super) fn actor_label(actor: &Actor) -> Option<&str> {
    match actor {
        Actor::Operator { label } => Some(label.as_str()),
        Actor::System => None,
    }
}

/// Text representation used only for the JSONB event payload
/// (`event_payload` below) — unrelated to the `audit_actor_kind` PostgreSQL
/// ENUM, which governs the `audit_records.actor_kind` column instead.
fn actor_columns(actor: &Actor) -> (&'static str, Option<&str>) {
    match actor {
        Actor::Operator { .. } => ("operator", actor_label(actor)),
        Actor::System => ("system", None),
    }
}

/// Event-type-specific remainder only — envelope fields (`event_id`,
/// `event_type`, `endpoint_id`, `occurred_at`) are already real columns
/// (ADR-0013 Sec.6: JSONB is for genuinely variable payload, not for hiding
/// queryable lifecycle/state).
pub(super) fn event_payload(event: &DomainEvent) -> serde_json::Value {
    match event {
        DomainEvent::EndpointPendingEnrollment { .. } | DomainEvent::EndpointEnrolled { .. } => {
            serde_json::json!({})
        }
        DomainEvent::OperatorDecisionRecorded {
            decision, actor, ..
        } => {
            let (actor_kind, actor_label) = actor_columns(actor);
            serde_json::json!({
                "decision": decision,
                "actor_kind": actor_kind,
                "actor_label": actor_label,
            })
        }
        DomainEvent::InventoryRevisionRecorded {
            inventory_revision_id,
            ..
        } => serde_json::json!({"inventory_revision_id": inventory_revision_id.0}),
        DomainEvent::JobStarted { .. }
        | DomainEvent::JobSucceeded { .. }
        | DomainEvent::JobFailed { .. }
        | DomainEvent::JobCancelled { .. } => serde_json::json!({}),
        // job_step_id itself is a queryable `domain_events.job_step_id`
        // column (`event_payload`'s caller binds it separately), so the
        // payload here carries no additional event-specific remainder.
        DomainEvent::JobStepFailed { .. } => serde_json::json!({}),
        // attempt_id itself is a queryable `domain_events.attempt_id` column
        // (`event_payload`'s caller binds it separately, mirroring
        // job_step_id above), so the payload here carries no additional
        // event-specific remainder.
        DomainEvent::AttemptIndeterminate { .. } => serde_json::json!({}),
    }
}

pub(super) fn verifier_from_bytes(bytes: Vec<u8>) -> Result<CredentialHash, RepositoryError> {
    let array: [u8; 48] = bytes.try_into().map_err(|bytes: Vec<u8>| {
        RepositoryError::Backend(format!(
            "credential verifier has unexpected length {} (expected 48)",
            bytes.len()
        ))
    })?;
    Ok(CredentialHash::from_bytes(array))
}

pub(super) fn lookup_id_from_bytes(bytes: Vec<u8>) -> Result<CredentialLookupId, RepositoryError> {
    let array: [u8; 16] = bytes.try_into().map_err(|bytes: Vec<u8>| {
        RepositoryError::Backend(format!(
            "credential lookup id has unexpected length {} (expected 16)",
            bytes.len()
        ))
    })?;
    Ok(CredentialLookupId::from_bytes(array))
}

/// Shared `SELECT` column/join list for reconstructing an
/// [`EndpointAggregate`] including its two credential-chain slots' lookup ids
/// (ADR-0014: `CredentialSlot` owns `lookup_id`). The `LEFT JOIN`s tolerate a
/// missing mapping at the SQL level; [`row_to_aggregate`] below turns a
/// missing *predecessor* mapping into a `RepositoryError::Backend` invariant
/// failure, since every persisted chain must have one.
///
/// Duplicated as a literal `&'static str` in each query below rather than
/// built via `format!`: sqlx 0.9's `SqlSafeStr` bound rejects a
/// runtime-constructed SQL string outright (an anti-injection guard), and
/// every caller here needs a different trailing `WHERE`/lock clause anyway.
macro_rules! endpoint_select {
    () => {
        r#"
        SELECT e.id, e.inventory_signal, e.identity_state, e.hardware_confidence, e.created_at, e.updated_at,
               e.current_boot_context_id, e.current_boot_nonce, e.trusted_bootstrap_state,
               c.predecessor_verifier, c.predecessor_issued_at, c.predecessor_expires_at,
               c.successor_verifier, c.successor_issued_at, c.successor_expires_at, c.revoked,
               lp.lookup_id AS predecessor_lookup_id,
               ls.lookup_id AS successor_lookup_id
        FROM endpoints e
        JOIN endpoint_credentials c ON c.endpoint_id = e.id
        LEFT JOIN endpoint_credential_lookups lp ON lp.endpoint_id = e.id AND lp.slot = 'predecessor'
        LEFT JOIN endpoint_credential_lookups ls ON ls.endpoint_id = e.id AND ls.slot = 'successor'
        "#
    };
}

/// Reconstructs the Endpoint current-boot projection
/// (`bamep_domain::CurrentBoot`) from its three SQL columns. PostgreSQL's own
/// all-or-none `CHECK` constraint in the initial schema should already
/// prevent a partially-populated row, but this Adapter mapping fails closed
/// with `RepositoryError` on an inconsistent partial projection rather than
/// trusting that constraint alone.
fn row_to_current_boot(
    row: &sqlx::postgres::PgRow,
) -> Result<Option<CurrentBoot>, RepositoryError> {
    let context_id: Option<Vec<u8>> = row
        .try_get("current_boot_context_id")
        .map_err(to_backend_err)?;
    let nonce_bytes: Option<Vec<u8>> = row.try_get("current_boot_nonce").map_err(to_backend_err)?;
    let trusted_bootstrap_state: Option<PgTrustedBootstrapState> = row
        .try_get("trusted_bootstrap_state")
        .map_err(to_backend_err)?;

    match (context_id, nonce_bytes, trusted_bootstrap_state) {
        (None, None, None) => Ok(None),
        (Some(id_bytes), Some(nonce_bytes), Some(state)) => {
            let boot_context_id = lookup_id_from_bytes(id_bytes)?;
            let nonce_array: [u8; 32] = nonce_bytes.try_into().map_err(|bytes: Vec<u8>| {
                RepositoryError::Backend(format!(
                    "current_boot_nonce has unexpected length {} (expected 32)",
                    bytes.len()
                ))
            })?;
            Ok(Some(CurrentBoot::new(
                boot_context_id,
                BootNonce::from_bytes(nonce_array),
                state.into(),
            )))
        }
        _ => Err(RepositoryError::Backend(
            "persisted endpoint current-boot projection is partially populated".to_string(),
        )),
    }
}

fn row_to_aggregate(row: &sqlx::postgres::PgRow) -> Result<EndpointAggregate, RepositoryError> {
    let id: uuid::Uuid = row.try_get("id").map_err(to_backend_err)?;
    let inventory_signal: String = row.try_get("inventory_signal").map_err(to_backend_err)?;
    let identity_state: PgIdentityState = row.try_get("identity_state").map_err(to_backend_err)?;
    let hardware_confidence: PgHardwareConfidence =
        row.try_get("hardware_confidence").map_err(to_backend_err)?;
    let created_at = row.try_get("created_at").map_err(to_backend_err)?;
    let updated_at = row.try_get("updated_at").map_err(to_backend_err)?;
    let current_boot = row_to_current_boot(row)?;

    let predecessor_verifier: Vec<u8> = row
        .try_get("predecessor_verifier")
        .map_err(to_backend_err)?;
    let predecessor_lookup_id: Option<Vec<u8>> = row
        .try_get("predecessor_lookup_id")
        .map_err(to_backend_err)?;
    let predecessor_lookup_id = predecessor_lookup_id.ok_or_else(|| {
        RepositoryError::Backend(
            "persisted predecessor credential is missing its lookup mapping".to_string(),
        )
    })?;
    let predecessor = CredentialSlot {
        lookup_id: lookup_id_from_bytes(predecessor_lookup_id)?,
        hash: verifier_from_bytes(predecessor_verifier)?,
        issued_at: row
            .try_get("predecessor_issued_at")
            .map_err(to_backend_err)?,
        expires_at: row
            .try_get("predecessor_expires_at")
            .map_err(to_backend_err)?,
    };

    let successor_verifier: Option<Vec<u8>> =
        row.try_get("successor_verifier").map_err(to_backend_err)?;
    let successor_lookup_id: Option<Vec<u8>> =
        row.try_get("successor_lookup_id").map_err(to_backend_err)?;
    let successor = match (successor_verifier, successor_lookup_id) {
        (Some(verifier_bytes), Some(lookup_bytes)) => Some(CredentialSlot {
            lookup_id: lookup_id_from_bytes(lookup_bytes)?,
            hash: verifier_from_bytes(verifier_bytes)?,
            issued_at: row.try_get("successor_issued_at").map_err(to_backend_err)?,
            expires_at: row
                .try_get("successor_expires_at")
                .map_err(to_backend_err)?,
        }),
        (None, None) => None,
        _ => {
            return Err(RepositoryError::Backend(
                "persisted successor verifier and lookup mapping disagree on presence".to_string(),
            ))
        }
    };
    let revoked: bool = row.try_get("revoked").map_err(to_backend_err)?;

    Ok(EndpointAggregate {
        id: EndpointId(id),
        inventory_signal,
        identity: identity_state.into(),
        credential: CredentialChain::from_parts(predecessor, successor, revoked),
        hardware_confidence: hardware_confidence.into(),
        current_boot,
        created_at,
        updated_at,
    })
}

pub(super) async fn load_by_id_for_update(
    tx: &mut Transaction<'_, Postgres>,
    id: EndpointId,
) -> Result<Option<EndpointAggregate>, RepositoryError> {
    let row = sqlx::query(concat!(
        endpoint_select!(),
        " WHERE e.id = $1 FOR UPDATE OF e, c"
    ))
    .bind(id.0)
    .fetch_optional(&mut **tx)
    .await
    .map_err(to_backend_err)?;

    row.as_ref().map(row_to_aggregate).transpose()
}

pub(super) async fn load_by_signal_for_update(
    tx: &mut Transaction<'_, Postgres>,
    signal: &str,
) -> Result<Option<EndpointAggregate>, RepositoryError> {
    let row = sqlx::query(concat!(
        endpoint_select!(),
        " WHERE e.inventory_signal = $1 FOR UPDATE OF e, c"
    ))
    .bind(signal)
    .fetch_optional(&mut **tx)
    .await
    .map_err(to_backend_err)?;

    row.as_ref().map(row_to_aggregate).transpose()
}

pub(super) async fn find_by_id(
    pool: &sqlx::PgPool,
    id: EndpointId,
) -> Result<Option<EndpointAggregate>, RepositoryError> {
    let row = sqlx::query(concat!(endpoint_select!(), " WHERE e.id = $1"))
        .bind(id.0)
        .fetch_optional(pool)
        .await
        .map_err(to_backend_err)?;

    row.as_ref().map(row_to_aggregate).transpose()
}

/// Persists a [`TransitionOutcome`] (state + domain events + audit record)
/// within `tx`, without committing it — the caller decides commit/rollback.
/// Never touches `endpoint_credential_lookups`: normal `update_endpoint`
/// operations (approval, revocation) must not disturb the credential lookup
/// projection (ADR-0014 checkpoint "LOOKUP PROJECTION ON ACCEPTED
/// REDEMPTION": "Do NOT delete lookup mappings on normal
/// update_endpoint/revocation operations"). Credential redemption calls
/// [`persist_lookup_projection`] separately, in the same transaction.
pub(super) async fn persist_transition(
    tx: &mut Transaction<'_, Postgres>,
    outcome: &TransitionOutcome,
) -> Result<(), RepositoryError> {
    let endpoint = &outcome.endpoint;

    let (current_boot_context_id, current_boot_nonce, trusted_bootstrap_state): (
        Option<Vec<u8>>,
        Option<Vec<u8>>,
        Option<PgTrustedBootstrapState>,
    ) = match &endpoint.current_boot {
        Some(current_boot) => (
            Some(current_boot.boot_context_id().to_bytes().to_vec()),
            Some(current_boot.boot_nonce().as_bytes().to_vec()),
            Some(PgTrustedBootstrapState::from(
                current_boot.trusted_bootstrap(),
            )),
        ),
        None => (None, None, None),
    };

    sqlx::query(
        r#"
        INSERT INTO endpoints (
            id, inventory_signal, identity_state, hardware_confidence, created_at, updated_at,
            current_boot_context_id, current_boot_nonce, trusted_bootstrap_state
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        ON CONFLICT (id) DO UPDATE SET
            identity_state = EXCLUDED.identity_state,
            hardware_confidence = EXCLUDED.hardware_confidence,
            updated_at = EXCLUDED.updated_at,
            current_boot_context_id = EXCLUDED.current_boot_context_id,
            current_boot_nonce = EXCLUDED.current_boot_nonce,
            trusted_bootstrap_state = EXCLUDED.trusted_bootstrap_state
        "#,
    )
    .bind(endpoint.id.0)
    .bind(&endpoint.inventory_signal)
    .bind(PgIdentityState::from(endpoint.identity))
    .bind(PgHardwareConfidence::from(endpoint.hardware_confidence))
    .bind(endpoint.created_at)
    .bind(endpoint.updated_at)
    .bind(current_boot_context_id)
    .bind(current_boot_nonce)
    .bind(trusted_bootstrap_state)
    .execute(&mut **tx)
    .await
    .map_err(to_backend_err)?;

    let predecessor = endpoint.credential.predecessor();
    let successor = endpoint.credential.successor();

    sqlx::query(
        r#"
        INSERT INTO endpoint_credentials (
            endpoint_id, predecessor_verifier, predecessor_issued_at, predecessor_expires_at,
            successor_verifier, successor_issued_at, successor_expires_at, revoked
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        ON CONFLICT (endpoint_id) DO UPDATE SET
            predecessor_verifier = EXCLUDED.predecessor_verifier,
            predecessor_issued_at = EXCLUDED.predecessor_issued_at,
            predecessor_expires_at = EXCLUDED.predecessor_expires_at,
            successor_verifier = EXCLUDED.successor_verifier,
            successor_issued_at = EXCLUDED.successor_issued_at,
            successor_expires_at = EXCLUDED.successor_expires_at,
            revoked = EXCLUDED.revoked
        "#,
    )
    .bind(endpoint.id.0)
    .bind(predecessor.hash.to_bytes().to_vec())
    .bind(predecessor.issued_at)
    .bind(predecessor.expires_at)
    .bind(successor.as_ref().map(|s| s.hash.to_bytes().to_vec()))
    .bind(successor.as_ref().map(|s| s.issued_at))
    .bind(successor.as_ref().map(|s| s.expires_at))
    .bind(endpoint.credential.is_revoked())
    .execute(&mut **tx)
    .await
    .map_err(to_backend_err)?;

    for event in &outcome.events {
        sqlx::query(
            r#"
            INSERT INTO domain_events (event_id, event_type, event_version, endpoint_id, occurred_at, payload)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
        )
        .bind(event.event_id())
        .bind(PgDomainEventType::from(event))
        .bind(1i32)
        .bind(event.endpoint_id().0)
        .bind(event.occurred_at())
        .bind(event_payload(event))
        .execute(&mut **tx)
        .await
        .map_err(to_backend_err)?;
    }

    if let Some(audit) = &outcome.audit {
        sqlx::query(
            r#"
            INSERT INTO audit_records (audit_id, endpoint_id, actor_kind, actor_label, occurred_at, detail)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
        )
        .bind(audit.audit_id)
        .bind(audit.endpoint_id.0)
        .bind(PgAuditActorKind::from(&audit.actor))
        .bind(actor_label(&audit.actor))
        .bind(audit.occurred_at)
        .bind(&audit.detail)
        .execute(&mut **tx)
        .await
        .map_err(to_backend_err)?;
    }

    Ok(())
}

/// Replaces `endpoint_id`'s entire `endpoint_credential_lookups` projection
/// with the final accepted chain's mapping, within `tx` (ADR-0014 checkpoint
/// "LOOKUP PROJECTION ON ACCEPTED REDEMPTION"). Delete-then-insert rather
/// than transition-specific upsert ordering: the Adapter does not need to
/// know whether this was a predecessor retry, a successor confirmation,
/// first contact, or a genuine reboot, and this ordering avoids the
/// temporary predecessor/successor `UNIQUE(lookup_id)` collision independent
/// upserts could otherwise hit mid-transaction.
pub(super) async fn persist_lookup_projection(
    tx: &mut Transaction<'_, Postgres>,
    endpoint_id: EndpointId,
    chain: &CredentialChain,
) -> Result<(), RepositoryError> {
    sqlx::query("DELETE FROM endpoint_credential_lookups WHERE endpoint_id = $1")
        .bind(endpoint_id.0)
        .execute(&mut **tx)
        .await
        .map_err(to_backend_err)?;

    sqlx::query(
        "INSERT INTO endpoint_credential_lookups (endpoint_id, slot, lookup_id) VALUES ($1, $2, $3)",
    )
    .bind(endpoint_id.0)
    .bind(PgCredentialSlotRole::Predecessor)
    .bind(chain.predecessor().lookup_id.to_bytes().to_vec())
    .execute(&mut **tx)
    .await
    .map_err(to_backend_err)?;

    if let Some(successor) = chain.successor() {
        sqlx::query(
            "INSERT INTO endpoint_credential_lookups (endpoint_id, slot, lookup_id) VALUES ($1, $2, $3)",
        )
        .bind(endpoint_id.0)
        .bind(PgCredentialSlotRole::Successor)
        .bind(successor.lookup_id.to_bytes().to_vec())
        .execute(&mut **tx)
        .await
        .map_err(to_backend_err)?;
    }

    Ok(())
}

/// Locks and reconstructs a `BootContext` by its durable `boot_context_id`
/// bytes, within `tx`. Used only by credential redemption routing — no other
/// Adapter path reads `boot_contexts`.
///
/// Fails closed with `RepositoryError` when the row's `boot_nonce` is `NULL`
/// (an unknown historical row represented by the nullable baseline column):
/// such a row is never fabricated or backfilled a nonce, and is
/// therefore never accepted as a valid new-flow `BootContext` — it cannot be
/// redeemed through the current credential-redemption flow.
pub(super) async fn load_boot_context_for_update(
    tx: &mut Transaction<'_, Postgres>,
    boot_context_id: &[u8],
) -> Result<Option<BootContext>, RepositoryError> {
    let row = sqlx::query(
        r#"
        SELECT boot_context_id, verifier, issued_at, expires_at, inventory_signal, resolved_endpoint_id, boot_nonce
        FROM boot_contexts
        WHERE boot_context_id = $1
        FOR UPDATE
        "#,
    )
    .bind(boot_context_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(to_backend_err)?;

    let Some(row) = row else {
        return Ok(None);
    };

    let id_bytes: Vec<u8> = row.try_get("boot_context_id").map_err(to_backend_err)?;
    let verifier_bytes: Vec<u8> = row.try_get("verifier").map_err(to_backend_err)?;
    let issued_at = row.try_get("issued_at").map_err(to_backend_err)?;
    let expires_at = row.try_get("expires_at").map_err(to_backend_err)?;
    let inventory_signal: String = row.try_get("inventory_signal").map_err(to_backend_err)?;
    let resolved_endpoint_id: Option<uuid::Uuid> = row
        .try_get("resolved_endpoint_id")
        .map_err(to_backend_err)?;
    let boot_nonce_bytes: Option<Vec<u8>> = row.try_get("boot_nonce").map_err(to_backend_err)?;
    let boot_nonce_bytes = boot_nonce_bytes.ok_or_else(|| {
        RepositoryError::Backend(
            "boot context has no boot_nonce (a historical, pre-trusted-bootstrap row); it \
             cannot be redeemed through the current credential-redemption flow"
                .to_string(),
        )
    })?;
    let boot_nonce_array: [u8; 32] = boot_nonce_bytes.try_into().map_err(|bytes: Vec<u8>| {
        RepositoryError::Backend(format!(
            "boot context boot_nonce has unexpected length {} (expected 32)",
            bytes.len()
        ))
    })?;

    Ok(Some(BootContext::from_parts(
        lookup_id_from_bytes(id_bytes)?,
        verifier_from_bytes(verifier_bytes)?,
        issued_at,
        expires_at,
        inventory_signal,
        resolved_endpoint_id.map(EndpointId),
        BootNonce::from_bytes(boot_nonce_array),
    )))
}

/// Persists `context.resolved_endpoint_id()` for `context.boot_context_id()`,
/// within `tx`. Used only when a redemption's `RedeemOutcome::Established`
/// carries a `resolved_boot_context` (first contact or genuine reboot).
pub(super) async fn persist_boot_context_resolution(
    tx: &mut Transaction<'_, Postgres>,
    context: &BootContext,
) -> Result<(), RepositoryError> {
    sqlx::query("UPDATE boot_contexts SET resolved_endpoint_id = $1 WHERE boot_context_id = $2")
        .bind(context.resolved_endpoint_id().map(|id| id.0))
        .bind(context.boot_context_id().to_bytes().to_vec())
        .execute(&mut **tx)
        .await
        .map_err(to_backend_err)?;
    Ok(())
}
