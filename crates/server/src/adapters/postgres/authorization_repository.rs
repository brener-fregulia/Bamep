//! `TransferAuthorizationRepository` Port implementation against real
//! PostgreSQL (Issue #38).
//!
//! [`PostgresTransferAuthorizationRepository::load_authorization_state`]
//! reads the `transfers`/`artifacts` row pair (via
//! [`super::transfer_repository::load_locked_facts`]), the owning `attempts`
//! row (when bound), and the `endpoints`/credential row, all `FOR UPDATE`
//! inside one transaction, then rolls back without persisting anything — a
//! read-only locking snapshot, mirroring
//! `PostgresTransferRepository::find_transfer_context`'s identical
//! lock-then-rollback shape. Locking all four together in one transaction is
//! what prevents a concurrent mutation (for example, a competing #40 dispatch
//! commit, or an operator credential revocation) from producing a
//! contradictory cross-read snapshot for one authorization decision (Issue
//! #38 "PostgreSQL transaction/repository composition").

use async_trait::async_trait;
use bamep_domain::{Attempt, AttemptId, EndpointAggregate, JobStepId, TransferId};
use sqlx::{PgPool, Postgres, Row, Transaction};

use super::shared::to_backend_err;
use crate::ports::{AuthorizationDurableState, RepositoryError, TransferAuthorizationRepository};

/// Adapter-local representation of the `attempt_state` PostgreSQL ENUM,
/// duplicated from `super::job_repository`'s private equivalent — both bind
/// to the same underlying `attempt_state` database type; Domain
/// (`bamep_domain::AttemptState`) stays free of SQLx derives, per that
/// module's identical rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "attempt_state")]
enum PgAttemptState {
    Dispatched,
    InProgress,
    AwaitingReconciliation,
    Succeeded,
    Failed,
    Cancelled,
    Rejected,
    Indeterminate,
}

impl From<PgAttemptState> for bamep_domain::AttemptState {
    fn from(state: PgAttemptState) -> Self {
        match state {
            PgAttemptState::Dispatched => bamep_domain::AttemptState::Dispatched,
            PgAttemptState::InProgress => bamep_domain::AttemptState::InProgress,
            PgAttemptState::AwaitingReconciliation => {
                bamep_domain::AttemptState::AwaitingReconciliation
            }
            PgAttemptState::Succeeded => bamep_domain::AttemptState::Succeeded,
            PgAttemptState::Failed => bamep_domain::AttemptState::Failed,
            PgAttemptState::Cancelled => bamep_domain::AttemptState::Cancelled,
            PgAttemptState::Rejected => bamep_domain::AttemptState::Rejected,
            PgAttemptState::Indeterminate => bamep_domain::AttemptState::Indeterminate,
        }
    }
}

async fn load_attempt_for_update(
    tx: &mut Transaction<'_, Postgres>,
    attempt_id: AttemptId,
) -> Result<Option<Attempt>, RepositoryError> {
    let row = sqlx::query(
        "SELECT id, job_step_id, action_id, state FROM attempts WHERE id = $1 FOR UPDATE",
    )
    .bind(attempt_id.0)
    .fetch_optional(&mut **tx)
    .await
    .map_err(to_backend_err)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let id: uuid::Uuid = row.try_get("id").map_err(to_backend_err)?;
    let job_step_id: uuid::Uuid = row.try_get("job_step_id").map_err(to_backend_err)?;
    let action_id: uuid::Uuid = row.try_get("action_id").map_err(to_backend_err)?;
    let state: PgAttemptState = row.try_get("state").map_err(to_backend_err)?;
    Ok(Some(Attempt {
        id: AttemptId(id),
        job_step_id: JobStepId(job_step_id),
        action_id: bamep_domain::ActionId(action_id),
        state: state.into(),
    }))
}

pub struct PostgresTransferAuthorizationRepository {
    pool: PgPool,
}

impl PostgresTransferAuthorizationRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TransferAuthorizationRepository for PostgresTransferAuthorizationRepository {
    async fn load_authorization_state(
        &self,
        transfer_id: TransferId,
    ) -> Result<Option<AuthorizationDurableState>, RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(to_backend_err)?;

        let Some(facts) =
            super::transfer_repository::load_locked_facts(&mut tx, transfer_id).await?
        else {
            tx.rollback().await.map_err(to_backend_err)?;
            return Ok(None);
        };

        let attempt = match facts.transfer.attempt_id {
            Some(attempt_id) => {
                let attempt = load_attempt_for_update(&mut tx, attempt_id)
                    .await?
                    .ok_or_else(|| {
                        RepositoryError::Backend(format!(
                            "transfer {:?} durably references missing attempt {attempt_id:?}",
                            transfer_id
                        ))
                    })?;
                Some(attempt)
            }
            None => None,
        };

        let endpoint: Option<EndpointAggregate> =
            super::shared::load_by_id_for_update(&mut tx, facts.transfer.endpoint_id).await?;
        let Some(endpoint) = endpoint else {
            return Err(RepositoryError::Backend(format!(
                "transfer {transfer_id:?} durably references missing endpoint \
                 {:?}",
                facts.transfer.endpoint_id
            )));
        };

        tx.rollback().await.map_err(to_backend_err)?;

        Ok(Some(AuthorizationDurableState {
            transfer: facts.transfer,
            artifact: facts.artifact,
            manifest: facts.manifest,
            held_chunk_indices: facts.held_chunk_indices,
            attempt,
            endpoint,
        }))
    }
}
