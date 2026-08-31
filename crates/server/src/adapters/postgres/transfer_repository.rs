//! `TransferRepository` Port implementation against real PostgreSQL (Issue
//! #36 "Persist Transfer, Artifact, and ChunkManifest lifecycle";
//! `m0-data-plane-and-storage-contracts.md`).
//!
//! Every mutating method locks exactly the `transfers` row identified by
//! `transfer_id` with `FOR UPDATE` before reading its correlated
//! `artifacts`/`chunk_manifests`/`chunk_identities` rows and invoking the
//! caller's `decide` closure — mirroring `super::job_repository`'s lock/
//! decide/persist discipline. Because every mutating operation against one
//! Transfer starts by locking that same `transfers` row, this single lock
//! also serializes access to its correlated Artifact/manifest/chunk rows
//! without a separate `FOR UPDATE` on each of them.
//!
//! ## Composability for a future atomic dispatch commitment (#40)
//!
//! [`load_locked_facts`] and [`persist_attempt_binding`] are the
//! lower-level primitives [`PostgresTransferRepository::bind_attempt`]
//! composes internally. They are `pub(crate)` — reachable from elsewhere in
//! `bamep_server`, but never exposed through `crate::ports` or Domain — so a
//! later Work Package (#40) that must commit the Transfer -> Attempt binding
//! atomically alongside its own JobStep `Dispatching` transition and new
//! `attempts` row can call them directly against its own already-open
//! `Transaction` (lock the Transfer's facts, decide with
//! `bamep_domain::bind_attempt`, then persist the binding), instead of being
//! forced to call [`PostgresTransferRepository::bind_attempt`]'s
//! self-contained transaction before or after its own. This repository
//! never opens a transaction that outlives one method call, so it cannot
//! itself make that future composition impossible.

use std::collections::BTreeSet;

use async_trait::async_trait;
use bamep_domain::{
    Artifact, ArtifactId, ArtifactState, CaptureConsistency, ChunkIndex, ChunkManifest,
    ChunkRecordOutcome, DigestAlgorithm, EndpointId, JobId, JobStepId, SourceProvenance, Transfer,
    TransferContext, TransferDirection, TransferId,
};
use sqlx::{PgPool, Postgres, Row, Transaction};

use super::authorization_repository::load_attempt_for_update;
use super::shared::{load_by_id_for_update, to_backend_err};
use crate::ports::{
    AcceptChunkDecision, AcceptChunkError, AcceptChunkOutcome, ArtifactTransitionDecision,
    ArtifactTransitionRepoError, ArtifactVerificationCommit, ArtifactVerificationDecided,
    AuthorizationDurableState, BindAttemptDecision, BindAttemptError, ChunkAcceptanceCommit,
    ChunkAcceptanceDecided, CommitArtifactVerificationDecision, CommitArtifactVerificationError,
    CommitChunkAcceptanceDecision, CommitChunkAcceptanceError, CommitManifestSealDecision,
    CommitManifestSealError, CreateTransferError, ManifestSealCommit, ManifestSealDecided,
    RecordChunkDecision, RecordChunkError, RepositoryError, SealManifestDecision,
    SealManifestError, SealedManifestDurableFacts, TransferLockedFacts, TransferRepository,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "transfer_direction", rename_all = "snake_case")]
enum PgTransferDirection {
    AgentToServer,
}

impl From<TransferDirection> for PgTransferDirection {
    fn from(direction: TransferDirection) -> Self {
        match direction {
            TransferDirection::AgentToServer => PgTransferDirection::AgentToServer,
        }
    }
}

impl From<PgTransferDirection> for TransferDirection {
    fn from(direction: PgTransferDirection) -> Self {
        match direction {
            PgTransferDirection::AgentToServer => TransferDirection::AgentToServer,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "digest_algorithm", rename_all = "snake_case")]
enum PgDigestAlgorithm {
    Sha256,
}

impl From<DigestAlgorithm> for PgDigestAlgorithm {
    fn from(algorithm: DigestAlgorithm) -> Self {
        match algorithm {
            DigestAlgorithm::Sha256 => PgDigestAlgorithm::Sha256,
        }
    }
}

impl From<PgDigestAlgorithm> for DigestAlgorithm {
    fn from(algorithm: PgDigestAlgorithm) -> Self {
        match algorithm {
            PgDigestAlgorithm::Sha256 => DigestAlgorithm::Sha256,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "artifact_state")]
enum PgArtifactState {
    Incomplete,
    PendingVerification,
    Verified,
    Failed,
}

impl From<ArtifactState> for PgArtifactState {
    fn from(state: ArtifactState) -> Self {
        match state {
            ArtifactState::Incomplete => PgArtifactState::Incomplete,
            ArtifactState::PendingVerification => PgArtifactState::PendingVerification,
            ArtifactState::Verified => PgArtifactState::Verified,
            ArtifactState::Failed => PgArtifactState::Failed,
        }
    }
}

impl From<PgArtifactState> for ArtifactState {
    fn from(state: PgArtifactState) -> Self {
        match state {
            PgArtifactState::Incomplete => ArtifactState::Incomplete,
            PgArtifactState::PendingVerification => ArtifactState::PendingVerification,
            PgArtifactState::Verified => ArtifactState::Verified,
            PgArtifactState::Failed => ArtifactState::Failed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "capture_consistency_state")]
enum PgCaptureConsistency {
    NotApplicable,
    NotEstablished,
    Established,
}

impl From<CaptureConsistency> for PgCaptureConsistency {
    fn from(value: CaptureConsistency) -> Self {
        match value {
            CaptureConsistency::NotApplicable => PgCaptureConsistency::NotApplicable,
            CaptureConsistency::NotEstablished => PgCaptureConsistency::NotEstablished,
            CaptureConsistency::Established => PgCaptureConsistency::Established,
        }
    }
}

impl From<PgCaptureConsistency> for CaptureConsistency {
    fn from(value: PgCaptureConsistency) -> Self {
        match value {
            PgCaptureConsistency::NotApplicable => CaptureConsistency::NotApplicable,
            PgCaptureConsistency::NotEstablished => CaptureConsistency::NotEstablished,
            PgCaptureConsistency::Established => CaptureConsistency::Established,
        }
    }
}

pub struct PostgresTransferRepository {
    pool: PgPool,
}

impl PostgresTransferRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// Locks the `transfers` row identified by `transfer_id` and reconstructs
/// its full [`TransferLockedFacts`], or `None` when no such Transfer exists.
/// The `transfers` row lock is the sole serialization anchor for every
/// mutating operation against this Transfer — see module docs. `pub(crate)`
/// so a future #40 composition can lock/read these facts directly inside
/// its own transaction — see module docs "Composability".
pub(crate) async fn load_locked_facts(
    tx: &mut Transaction<'_, Postgres>,
    transfer_id: TransferId,
) -> Result<Option<TransferLockedFacts>, RepositoryError> {
    let Some(row) = sqlx::query(
        r#"
        SELECT id, endpoint_id, job_id, job_step_id, artifact_id, direction, digest_algorithm,
               chunk_size, source_provenance, attempt_id
        FROM transfers
        WHERE id = $1
        FOR UPDATE
        "#,
    )
    .bind(transfer_id.0)
    .fetch_optional(&mut **tx)
    .await
    .map_err(to_backend_err)?
    else {
        return Ok(None);
    };

    let artifact_id: uuid::Uuid = row.try_get("artifact_id").map_err(to_backend_err)?;
    let direction: PgTransferDirection = row.try_get("direction").map_err(to_backend_err)?;
    let digest_algorithm: PgDigestAlgorithm =
        row.try_get("digest_algorithm").map_err(to_backend_err)?;
    let chunk_size: i32 = row.try_get("chunk_size").map_err(to_backend_err)?;
    let source_provenance: String = row.try_get("source_provenance").map_err(to_backend_err)?;
    let attempt_id: Option<uuid::Uuid> = row.try_get("attempt_id").map_err(to_backend_err)?;

    let transfer = Transfer {
        id: transfer_id,
        endpoint_id: EndpointId(row.try_get("endpoint_id").map_err(to_backend_err)?),
        job_id: JobId(row.try_get("job_id").map_err(to_backend_err)?),
        job_step_id: JobStepId(row.try_get("job_step_id").map_err(to_backend_err)?),
        artifact_id: ArtifactId(artifact_id),
        direction: direction.into(),
        digest_algorithm: digest_algorithm.into(),
        chunk_size: bamep_domain::ChunkSize::new(chunk_size as u32).map_err(|_| {
            RepositoryError::Backend("persisted transfer has a non-positive chunk_size".into())
        })?,
        source_provenance: SourceProvenance::new(source_provenance),
        attempt_id: attempt_id.map(bamep_domain::AttemptId),
    };

    let artifact_row =
        sqlx::query("SELECT state, capture_consistency FROM artifacts WHERE id = $1 FOR UPDATE")
            .bind(artifact_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(to_backend_err)?;
    let state: PgArtifactState = artifact_row.try_get("state").map_err(to_backend_err)?;
    let capture_consistency: PgCaptureConsistency = artifact_row
        .try_get("capture_consistency")
        .map_err(to_backend_err)?;
    let artifact = Artifact {
        id: ArtifactId(artifact_id),
        state: state.into(),
        capture_consistency: capture_consistency.into(),
    };

    let manifest_row = sqlx::query(
        "SELECT sealed, chunk_count, artifact_digest FROM chunk_manifests WHERE artifact_id = $1 FOR UPDATE",
    )
    .bind(artifact_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(to_backend_err)?;
    let sealed: bool = manifest_row.try_get("sealed").map_err(to_backend_err)?;
    let chunk_count: Option<i32> = manifest_row
        .try_get("chunk_count")
        .map_err(to_backend_err)?;
    let artifact_digest: Option<Vec<u8>> = manifest_row
        .try_get("artifact_digest")
        .map_err(to_backend_err)?;

    let mut manifest = ChunkManifest::new(
        ArtifactId(artifact_id),
        transfer.digest_algorithm,
        transfer.chunk_size,
    );

    let chunk_rows = sqlx::query(
        "SELECT chunk_index, size, digest, held FROM chunk_identities WHERE artifact_id = $1 ORDER BY chunk_index ASC",
    )
    .bind(artifact_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(to_backend_err)?;

    let mut held_chunk_indices = BTreeSet::new();
    for chunk_row in &chunk_rows {
        let index: i32 = chunk_row.try_get("chunk_index").map_err(to_backend_err)?;
        let size: i32 = chunk_row.try_get("size").map_err(to_backend_err)?;
        let digest: Vec<u8> = chunk_row.try_get("digest").map_err(to_backend_err)?;
        let held: bool = chunk_row.try_get("held").map_err(to_backend_err)?;

        manifest =
            match manifest.record_expected_chunk(ChunkIndex(index as u32), size as u32, digest) {
                Ok(bamep_domain::ChunkRecordOutcome::Added(m)) => m,
                _ => {
                    return Err(RepositoryError::Backend(
                        "persisted chunk identities could not be reconstructed into a manifest"
                            .into(),
                    ))
                }
            };
        if held {
            held_chunk_indices.insert(ChunkIndex(index as u32));
        }
    }

    // Reapply sealing last: `record_expected_chunk` above rejects a new
    // index on an already-sealed manifest, so every recorded row must be
    // replayed against the still-unsealed manifest before sealing is
    // reapplied here.
    if sealed {
        let chunk_count = chunk_count.ok_or_else(|| {
            RepositoryError::Backend("sealed manifest is missing its durable chunk_count".into())
        })?;
        let artifact_digest = artifact_digest.ok_or_else(|| {
            RepositoryError::Backend(
                "sealed manifest is missing its durable artifact_digest".into(),
            )
        })?;
        manifest = match manifest.seal(chunk_count as u32, artifact_digest) {
            Ok(bamep_domain::SealOutcome::Sealed(m)) => m,
            _ => {
                return Err(RepositoryError::Backend(
                    "persisted sealed manifest facts could not be reconstructed".into(),
                ))
            }
        };
    }

    Ok(Some(TransferLockedFacts {
        transfer,
        artifact,
        manifest,
        held_chunk_indices,
    }))
}

/// Persists `attempt_id` onto `transfers.attempt_id` for `transfer_id`,
/// within the caller's already-open `tx`. The caller is responsible for
/// having already locked the row (e.g. via [`load_locked_facts`]) and for
/// having already decided the binding is legal
/// (`bamep_domain::bind_attempt`) — this function performs no decision of
/// its own, only the write, so a future #40 composition can issue it
/// alongside its own JobStep/Attempt inserts in one transaction.
pub(crate) async fn persist_attempt_binding(
    tx: &mut Transaction<'_, Postgres>,
    transfer_id: TransferId,
    attempt_id: bamep_domain::AttemptId,
) -> Result<(), RepositoryError> {
    sqlx::query("UPDATE transfers SET attempt_id = $1 WHERE id = $2")
        .bind(attempt_id.0)
        .bind(transfer_id.0)
        .execute(&mut **tx)
        .await
        .map_err(to_backend_err)?;
    Ok(())
}

/// Locks the `transfers` row for `transfer_id` and its correlated `artifacts`
/// row (both `FOR UPDATE`) and returns just the reconstructed [`Transfer`] and
/// [`Artifact`] — no manifest, no chunk-identity load. The lock prefix is
/// identical to [`load_locked_facts`]'s (`transfers` then `artifacts`), so the
/// same `transfers`-row serialization anchor and lock ordering hold; this
/// primitive only skips the potentially large chunk-identity read the terminal
/// -`ActionResult` path (Issue #19 checkpoint C2) never needs. `pub(crate)` so
/// `super::job_repository`'s atomic terminal-evidence composition can lock/read
/// these facts directly inside its own transaction (Issue #19 §12
/// "Transaction design for CASE C").
pub(crate) async fn load_locked_transfer_and_artifact(
    tx: &mut Transaction<'_, Postgres>,
    transfer_id: TransferId,
) -> Result<Option<(Transfer, Artifact)>, RepositoryError> {
    let Some(row) = sqlx::query(
        r#"
        SELECT id, endpoint_id, job_id, job_step_id, artifact_id, direction, digest_algorithm,
               chunk_size, source_provenance, attempt_id
        FROM transfers
        WHERE id = $1
        FOR UPDATE
        "#,
    )
    .bind(transfer_id.0)
    .fetch_optional(&mut **tx)
    .await
    .map_err(to_backend_err)?
    else {
        return Ok(None);
    };

    let artifact_id: uuid::Uuid = row.try_get("artifact_id").map_err(to_backend_err)?;
    let direction: PgTransferDirection = row.try_get("direction").map_err(to_backend_err)?;
    let digest_algorithm: PgDigestAlgorithm =
        row.try_get("digest_algorithm").map_err(to_backend_err)?;
    let chunk_size: i32 = row.try_get("chunk_size").map_err(to_backend_err)?;
    let source_provenance: String = row.try_get("source_provenance").map_err(to_backend_err)?;
    let attempt_id: Option<uuid::Uuid> = row.try_get("attempt_id").map_err(to_backend_err)?;

    let transfer = Transfer {
        id: transfer_id,
        endpoint_id: EndpointId(row.try_get("endpoint_id").map_err(to_backend_err)?),
        job_id: JobId(row.try_get("job_id").map_err(to_backend_err)?),
        job_step_id: JobStepId(row.try_get("job_step_id").map_err(to_backend_err)?),
        artifact_id: ArtifactId(artifact_id),
        direction: direction.into(),
        digest_algorithm: digest_algorithm.into(),
        chunk_size: bamep_domain::ChunkSize::new(chunk_size as u32).map_err(|_| {
            RepositoryError::Backend("persisted transfer has a non-positive chunk_size".into())
        })?,
        source_provenance: SourceProvenance::new(source_provenance),
        attempt_id: attempt_id.map(bamep_domain::AttemptId),
    };

    let artifact_row =
        sqlx::query("SELECT state, capture_consistency FROM artifacts WHERE id = $1 FOR UPDATE")
            .bind(artifact_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(to_backend_err)?;
    let state: PgArtifactState = artifact_row.try_get("state").map_err(to_backend_err)?;
    let capture_consistency: PgCaptureConsistency = artifact_row
        .try_get("capture_consistency")
        .map_err(to_backend_err)?;
    let artifact = Artifact {
        id: ArtifactId(artifact_id),
        state: state.into(),
        capture_consistency: capture_consistency.into(),
    };

    Ok(Some((transfer, artifact)))
}

/// Persists `Artifact Incomplete -> Failed` for `artifact_id` within the
/// caller's already-open `tx`, returning `true` iff exactly one row moved from
/// `Incomplete` to `Failed`. The `state = 'Incomplete'` guard makes this a
/// no-op (returns `false`) against any already-non-`Incomplete` Artifact —
/// terminal-Artifact immutability (`m0-data-plane-and-storage-contracts.md`
/// "Artifact lifecycle"). The caller must have already locked the `artifacts`
/// row (e.g. via [`load_locked_transfer_and_artifact`]) and decided the
/// transition is legal (`bamep_domain::fail_incomplete`).
pub(crate) async fn persist_incomplete_artifact_failed(
    tx: &mut Transaction<'_, Postgres>,
    artifact_id: ArtifactId,
) -> Result<bool, RepositoryError> {
    let result =
        sqlx::query("UPDATE artifacts SET state = 'Failed' WHERE id = $1 AND state = 'Incomplete'")
            .bind(artifact_id.0)
            .execute(&mut **tx)
            .await
            .map_err(to_backend_err)?;
    Ok(result.rows_affected() == 1)
}

#[async_trait]
impl TransferRepository for PostgresTransferRepository {
    async fn create_transfer_context(
        &self,
        context: &TransferContext,
    ) -> Result<(), CreateTransferError> {
        let transfer = &context.transfer;
        let mut tx = self.pool.begin().await.map_err(to_backend_err)?;

        let endpoint_exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM endpoints WHERE id = $1)")
                .bind(transfer.endpoint_id.0)
                .fetch_one(&mut *tx)
                .await
                .map_err(to_backend_err)?;
        if !endpoint_exists {
            tx.rollback().await.map_err(to_backend_err)?;
            return Err(CreateTransferError::EndpointNotFound(transfer.endpoint_id));
        }

        let job_endpoint_id: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT endpoint_id FROM jobs WHERE id = $1")
                .bind(transfer.job_id.0)
                .fetch_optional(&mut *tx)
                .await
                .map_err(to_backend_err)?;
        let Some(job_endpoint_id) = job_endpoint_id else {
            tx.rollback().await.map_err(to_backend_err)?;
            return Err(CreateTransferError::JobNotFound(transfer.job_id));
        };
        if job_endpoint_id != transfer.endpoint_id.0 {
            tx.rollback().await.map_err(to_backend_err)?;
            return Err(CreateTransferError::JobEndpointMismatch(
                transfer.job_id,
                transfer.endpoint_id,
            ));
        }

        let step_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM job_steps WHERE id = $1 AND job_id = $2)",
        )
        .bind(transfer.job_step_id.0)
        .bind(transfer.job_id.0)
        .fetch_one(&mut *tx)
        .await
        .map_err(to_backend_err)?;
        if !step_exists {
            tx.rollback().await.map_err(to_backend_err)?;
            return Err(CreateTransferError::JobStepNotFound(
                transfer.job_step_id,
                transfer.job_id,
            ));
        }

        sqlx::query("INSERT INTO artifacts (id, state, capture_consistency) VALUES ($1, $2, $3)")
            .bind(context.artifact.id.0)
            .bind(PgArtifactState::from(context.artifact.state))
            .bind(PgCaptureConsistency::from(
                context.artifact.capture_consistency,
            ))
            .execute(&mut *tx)
            .await
            .map_err(to_backend_err)?;

        sqlx::query(
            r#"
            INSERT INTO transfers
                (id, endpoint_id, job_id, job_step_id, artifact_id, direction, digest_algorithm,
                 chunk_size, source_provenance, attempt_id)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            "#,
        )
        .bind(transfer.id.0)
        .bind(transfer.endpoint_id.0)
        .bind(transfer.job_id.0)
        .bind(transfer.job_step_id.0)
        .bind(transfer.artifact_id.0)
        .bind(PgTransferDirection::from(transfer.direction))
        .bind(PgDigestAlgorithm::from(transfer.digest_algorithm))
        .bind(transfer.chunk_size.get() as i32)
        .bind(transfer.source_provenance.as_str())
        .bind(transfer.attempt_id.map(|id| id.0))
        .execute(&mut *tx)
        .await
        .map_err(to_backend_err)?;

        sqlx::query("INSERT INTO chunk_manifests (artifact_id, sealed) VALUES ($1, FALSE)")
            .bind(context.artifact.id.0)
            .execute(&mut *tx)
            .await
            .map_err(to_backend_err)?;

        tx.commit().await.map_err(to_backend_err)?;
        Ok(())
    }

    async fn find_transfer_context(
        &self,
        transfer_id: TransferId,
    ) -> Result<Option<(TransferContext, BTreeSet<ChunkIndex>)>, RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(to_backend_err)?;
        let facts = load_locked_facts(&mut tx, transfer_id).await?;
        tx.rollback().await.map_err(to_backend_err)?;

        Ok(facts.map(|facts| {
            (
                TransferContext {
                    transfer: facts.transfer,
                    artifact: facts.artifact,
                    manifest: facts.manifest,
                },
                facts.held_chunk_indices,
            )
        }))
    }

    async fn record_expected_chunk(
        &self,
        transfer_id: TransferId,
        decide: RecordChunkDecision,
    ) -> Result<bamep_domain::ChunkRecordOutcome, RecordChunkError> {
        let mut tx = self.pool.begin().await.map_err(to_backend_err)?;
        let Some(facts) = load_locked_facts(&mut tx, transfer_id).await? else {
            tx.rollback().await.map_err(to_backend_err)?;
            return Err(RecordChunkError::TransferNotFound(transfer_id));
        };

        let outcome = decide(&facts)?;
        if let bamep_domain::ChunkRecordOutcome::Added(ref manifest) = outcome {
            // The newly added identity is exactly the one not already
            // present in `facts.manifest` — recover it from the decided
            // manifest by its highest-numbered new entry relative to the
            // locked facts.
            let new_index = manifest
                .chunk_indices()
                .find(|index| facts.manifest.expected_chunk(*index).is_none())
                .ok_or_else(|| {
                    RepositoryError::Backend(
                        "decided manifest added no chunk relative to locked facts".into(),
                    )
                })?;
            let expected = manifest.expected_chunk(new_index).ok_or_else(|| {
                RepositoryError::Backend("decided manifest is missing its own new chunk".into())
            })?;
            sqlx::query(
                "INSERT INTO chunk_identities (artifact_id, chunk_index, size, digest, held) \
                 VALUES ($1, $2, $3, $4, FALSE)",
            )
            .bind(facts.artifact.id.0)
            .bind(new_index.0 as i32)
            .bind(expected.size as i32)
            .bind(expected.digest.as_bytes())
            .execute(&mut *tx)
            .await
            .map_err(to_backend_err)?;
        }

        tx.commit().await.map_err(to_backend_err)?;
        Ok(outcome)
    }

    async fn accept_verified_chunk(
        &self,
        transfer_id: TransferId,
        index: ChunkIndex,
        decide: AcceptChunkDecision,
    ) -> Result<AcceptChunkOutcome, AcceptChunkError> {
        let mut tx = self.pool.begin().await.map_err(to_backend_err)?;
        let Some(facts) = load_locked_facts(&mut tx, transfer_id).await? else {
            tx.rollback().await.map_err(to_backend_err)?;
            return Err(AcceptChunkError::TransferNotFound(transfer_id));
        };

        let already_held = facts.held_chunk_indices.contains(&index);
        decide(&facts)?;

        if already_held {
            tx.rollback().await.map_err(to_backend_err)?;
            return Ok(AcceptChunkOutcome::AlreadyHeld);
        }

        sqlx::query(
            "UPDATE chunk_identities SET held = TRUE WHERE artifact_id = $1 AND chunk_index = $2",
        )
        .bind(facts.artifact.id.0)
        .bind(index.0 as i32)
        .execute(&mut *tx)
        .await
        .map_err(to_backend_err)?;

        tx.commit().await.map_err(to_backend_err)?;
        Ok(AcceptChunkOutcome::Accepted)
    }

    async fn commit_chunk_acceptance(
        &self,
        transfer_id: TransferId,
        index: ChunkIndex,
        decide: CommitChunkAcceptanceDecision,
    ) -> Result<ChunkAcceptanceCommit, CommitChunkAcceptanceError> {
        let mut tx = self.pool.begin().await.map_err(to_backend_err)?;
        let Some(facts) = load_locked_facts(&mut tx, transfer_id).await? else {
            tx.rollback().await.map_err(to_backend_err)?;
            return Err(CommitChunkAcceptanceError::TransferNotFound(transfer_id));
        };

        // Lock the owning Attempt in this same transaction so a concurrent
        // reconciliation cannot move it terminal between this read and the
        // commit (Issue #39 Phase C1 item 16; Issue #38 "PostgreSQL
        // transaction/repository composition").
        let attempt = match facts.transfer.attempt_id {
            Some(attempt_id) => {
                super::authorization_repository::load_attempt_for_update(&mut tx, attempt_id)
                    .await?
            }
            None => None,
        };

        let already_held = facts.held_chunk_indices.contains(&index);

        let record = match decide(&facts, attempt.as_ref()) {
            ChunkAcceptanceDecided::RejectConflict => {
                tx.rollback().await.map_err(to_backend_err)?;
                return Ok(ChunkAcceptanceCommit::RejectedConflict);
            }
            ChunkAcceptanceDecided::RejectNotContinuable => {
                tx.rollback().await.map_err(to_backend_err)?;
                return Ok(ChunkAcceptanceCommit::RejectedNotContinuable);
            }
            ChunkAcceptanceDecided::FailClosed => {
                tx.rollback().await.map_err(to_backend_err)?;
                return Ok(ChunkAcceptanceCommit::FailClosed);
            }
            ChunkAcceptanceDecided::Commit(record) => record,
        };

        match record {
            ChunkRecordOutcome::Added(ref manifest) => {
                // First-writer for a new `chunk_index`: record the immutable
                // expected identity *and* mark it durably held in one
                // transaction. The `transfers` row lock this method holds is
                // the sole serialization anchor, so a concurrent same-index
                // acceptance either waits and then sees this identity
                // (`AlreadyRecorded`/`Conflict`) or lost the race outright —
                // an already-recorded digest is never rewritten.
                let expected = manifest.expected_chunk(index).ok_or_else(|| {
                    RepositoryError::Backend(
                        "decided manifest is missing the accepted chunk identity".into(),
                    )
                })?;
                sqlx::query(
                    "INSERT INTO chunk_identities (artifact_id, chunk_index, size, digest, held) \
                     VALUES ($1, $2, $3, $4, TRUE)",
                )
                .bind(facts.artifact.id.0)
                .bind(index.0 as i32)
                .bind(expected.size as i32)
                .bind(expected.digest.as_bytes())
                .execute(&mut *tx)
                .await
                .map_err(to_backend_err)?;
                tx.commit().await.map_err(to_backend_err)?;
                Ok(ChunkAcceptanceCommit::Committed)
            }
            ChunkRecordOutcome::AlreadyRecorded => {
                if already_held {
                    // Identical `(transfer_id, chunk_index, digest)` already
                    // durably held — no second semantic commit.
                    tx.rollback().await.map_err(to_backend_err)?;
                    Ok(ChunkAcceptanceCommit::AlreadyCommitted)
                } else {
                    // The expected identity is durable but not yet held (only
                    // reachable via a non-C1 `record_expected_chunk` path):
                    // the Worker has now verified matching bytes, so this
                    // acceptance is what makes it durably held.
                    sqlx::query(
                        "UPDATE chunk_identities SET held = TRUE \
                         WHERE artifact_id = $1 AND chunk_index = $2",
                    )
                    .bind(facts.artifact.id.0)
                    .bind(index.0 as i32)
                    .execute(&mut *tx)
                    .await
                    .map_err(to_backend_err)?;
                    tx.commit().await.map_err(to_backend_err)?;
                    Ok(ChunkAcceptanceCommit::Committed)
                }
            }
        }
    }

    async fn commit_manifest_seal(
        &self,
        transfer_id: TransferId,
        decide: CommitManifestSealDecision,
    ) -> Result<ManifestSealCommit, CommitManifestSealError> {
        let mut tx = self.pool.begin().await.map_err(to_backend_err)?;

        // Lock the Transfer/Artifact/manifest/held-chunk facts, the owning
        // Attempt, and the Endpoint/credential row all `FOR UPDATE` in this
        // one transaction — the same consistent locked snapshot Issue #38's
        // `load_authorization_state` composes, but kept *open* so the seal
        // mutation commits against exactly the state the authorization
        // decision was made over (Issue #39 Phase C2: no authorization ->
        // mutation TOCTOU). The `transfers` row lock is the serialization
        // anchor, exactly as for `commit_chunk_acceptance`.
        let Some(facts) = load_locked_facts(&mut tx, transfer_id).await? else {
            tx.rollback().await.map_err(to_backend_err)?;
            return Err(CommitManifestSealError::TransferNotFound(transfer_id));
        };
        let attempt = match facts.transfer.attempt_id {
            Some(attempt_id) => load_attempt_for_update(&mut tx, attempt_id).await?,
            None => None,
        };
        let Some(endpoint) = load_by_id_for_update(&mut tx, facts.transfer.endpoint_id).await?
        else {
            return Err(CommitManifestSealError::Repository(
                RepositoryError::Backend(format!(
                    "transfer {transfer_id:?} durably references missing endpoint {:?}",
                    facts.transfer.endpoint_id
                )),
            ));
        };

        let artifact_id = facts.artifact.id;
        let digest_algorithm = facts.manifest.digest_algorithm;
        let chunk_size = facts.manifest.chunk_size.get();

        let snapshot = AuthorizationDurableState {
            transfer: facts.transfer,
            artifact: facts.artifact,
            manifest: facts.manifest,
            held_chunk_indices: facts.held_chunk_indices,
            attempt,
            endpoint,
        };

        let sealed_facts = |digest_wire: String, chunk_count: u64| SealedManifestDurableFacts {
            artifact_id,
            digest_algorithm,
            chunk_size,
            chunk_count,
            expected_artifact_digest: digest_wire,
        };

        match decide(&snapshot) {
            ManifestSealDecided::Denied => {
                tx.rollback().await.map_err(to_backend_err)?;
                Ok(ManifestSealCommit::Denied)
            }
            ManifestSealDecided::FailClosed => {
                tx.rollback().await.map_err(to_backend_err)?;
                Ok(ManifestSealCommit::FailClosed)
            }
            ManifestSealDecided::RejectIncomplete => {
                tx.rollback().await.map_err(to_backend_err)?;
                Ok(ManifestSealCommit::RejectedIncomplete)
            }
            ManifestSealDecided::RejectAlreadySealed => {
                tx.rollback().await.map_err(to_backend_err)?;
                Ok(ManifestSealCommit::RejectedAlreadySealed)
            }
            ManifestSealDecided::AlreadyPending => {
                // No mutation — the durable sealed tuple is authoritative
                // (`m1-worker-data-plane-control-contract.md`: the durable
                // sealed values, not the request body, are authoritative on
                // an idempotent retry).
                let chunk_count = snapshot.manifest.chunk_count.ok_or_else(|| {
                    RepositoryError::Backend(
                        "already-PendingVerification artifact has an unsealed durable manifest"
                            .into(),
                    )
                })?;
                let artifact_digest =
                    snapshot.manifest.artifact_digest.as_ref().ok_or_else(|| {
                        RepositoryError::Backend(
                            "already-sealed durable manifest is missing its artifact_digest".into(),
                        )
                    })?;
                let out = sealed_facts(artifact_digest.to_wire_value(), u64::from(chunk_count));
                tx.rollback().await.map_err(to_backend_err)?;
                Ok(ManifestSealCommit::AlreadyPending(out))
            }
            ManifestSealDecided::Seal {
                chunk_count,
                artifact_digest,
            } => {
                // First valid seal: the sealed manifest facts and
                // `Incomplete -> PendingVerification` commit atomically, in
                // this one transaction — never a sealed manifest with an
                // Incomplete Artifact observable in between (Issue #39 Phase
                // C2).
                sqlx::query(
                    "UPDATE chunk_manifests SET sealed = TRUE, chunk_count = $1, \
                     artifact_digest = $2 WHERE artifact_id = $3",
                )
                .bind(chunk_count as i32)
                .bind(artifact_digest.as_bytes())
                .bind(artifact_id.0)
                .execute(&mut *tx)
                .await
                .map_err(to_backend_err)?;

                sqlx::query("UPDATE artifacts SET state = $1 WHERE id = $2")
                    .bind(PgArtifactState::PendingVerification)
                    .bind(artifact_id.0)
                    .execute(&mut *tx)
                    .await
                    .map_err(to_backend_err)?;

                tx.commit().await.map_err(to_backend_err)?;
                Ok(ManifestSealCommit::Sealed(sealed_facts(
                    artifact_digest.to_wire_value(),
                    u64::from(chunk_count),
                )))
            }
        }
    }

    async fn commit_artifact_verification(
        &self,
        transfer_id: TransferId,
        decide: CommitArtifactVerificationDecision,
    ) -> Result<ArtifactVerificationCommit, CommitArtifactVerificationError> {
        let mut tx = self.pool.begin().await.map_err(to_backend_err)?;
        let Some(facts) = load_locked_facts(&mut tx, transfer_id).await? else {
            tx.rollback().await.map_err(to_backend_err)?;
            return Err(CommitArtifactVerificationError::TransferNotFound(
                transfer_id,
            ));
        };

        match decide(&facts) {
            ArtifactVerificationDecided::FailClosed => {
                tx.rollback().await.map_err(to_backend_err)?;
                Ok(ArtifactVerificationCommit::FailClosed)
            }
            ArtifactVerificationDecided::Commit(updated) => {
                sqlx::query("UPDATE artifacts SET state = $1 WHERE id = $2")
                    .bind(PgArtifactState::from(updated.state))
                    .bind(updated.id.0)
                    .execute(&mut *tx)
                    .await
                    .map_err(to_backend_err)?;
                tx.commit().await.map_err(to_backend_err)?;
                Ok(ArtifactVerificationCommit::Committed {
                    verified: updated.state == ArtifactState::Verified,
                })
            }
        }
    }

    async fn seal_manifest(
        &self,
        transfer_id: TransferId,
        decide: SealManifestDecision,
    ) -> Result<bamep_domain::SealOutcome, SealManifestError> {
        let mut tx = self.pool.begin().await.map_err(to_backend_err)?;
        let Some(facts) = load_locked_facts(&mut tx, transfer_id).await? else {
            tx.rollback().await.map_err(to_backend_err)?;
            return Err(SealManifestError::TransferNotFound(transfer_id));
        };

        let outcome = decide(&facts)?;
        if let bamep_domain::SealOutcome::Sealed(ref manifest) = outcome {
            let chunk_count = manifest.chunk_count.ok_or_else(|| {
                RepositoryError::Backend("sealed outcome is missing chunk_count".into())
            })?;
            let artifact_digest = manifest.artifact_digest.as_ref().ok_or_else(|| {
                RepositoryError::Backend("sealed outcome is missing artifact_digest".into())
            })?;
            sqlx::query(
                "UPDATE chunk_manifests SET sealed = TRUE, chunk_count = $1, artifact_digest = $2 \
                 WHERE artifact_id = $3",
            )
            .bind(chunk_count as i32)
            .bind(artifact_digest.as_bytes())
            .bind(facts.artifact.id.0)
            .execute(&mut *tx)
            .await
            .map_err(to_backend_err)?;
        }

        tx.commit().await.map_err(to_backend_err)?;
        Ok(outcome)
    }

    async fn begin_artifact_verification(
        &self,
        transfer_id: TransferId,
        decide: ArtifactTransitionDecision,
    ) -> Result<Artifact, ArtifactTransitionRepoError> {
        self.commit_artifact_transition(transfer_id, decide).await
    }

    async fn complete_artifact_verification(
        &self,
        transfer_id: TransferId,
        decide: ArtifactTransitionDecision,
    ) -> Result<Artifact, ArtifactTransitionRepoError> {
        self.commit_artifact_transition(transfer_id, decide).await
    }

    async fn fail_incomplete_artifact(
        &self,
        transfer_id: TransferId,
        decide: ArtifactTransitionDecision,
    ) -> Result<Artifact, ArtifactTransitionRepoError> {
        self.commit_artifact_transition(transfer_id, decide).await
    }

    async fn bind_attempt(
        &self,
        transfer_id: TransferId,
        decide: BindAttemptDecision,
    ) -> Result<Transfer, BindAttemptError> {
        let mut tx = self.pool.begin().await.map_err(to_backend_err)?;
        let Some(facts) = load_locked_facts(&mut tx, transfer_id).await? else {
            tx.rollback().await.map_err(to_backend_err)?;
            return Err(BindAttemptError::TransferNotFound(transfer_id));
        };

        let bound = decide(&facts)?;
        if let Some(attempt_id) = bound.attempt_id {
            persist_attempt_binding(&mut tx, transfer_id, attempt_id).await?;
        }

        tx.commit().await.map_err(to_backend_err)?;
        Ok(bound)
    }
}

impl PostgresTransferRepository {
    async fn commit_artifact_transition(
        &self,
        transfer_id: TransferId,
        decide: ArtifactTransitionDecision,
    ) -> Result<Artifact, ArtifactTransitionRepoError> {
        let mut tx = self.pool.begin().await.map_err(to_backend_err)?;
        let Some(facts) = load_locked_facts(&mut tx, transfer_id).await? else {
            tx.rollback().await.map_err(to_backend_err)?;
            return Err(ArtifactTransitionRepoError::TransferNotFound(transfer_id));
        };

        let updated = decide(&facts)?;

        sqlx::query("UPDATE artifacts SET state = $1 WHERE id = $2")
            .bind(PgArtifactState::from(updated.state))
            .bind(updated.id.0)
            .execute(&mut *tx)
            .await
            .map_err(to_backend_err)?;

        tx.commit().await.map_err(to_backend_err)?;
        Ok(updated)
    }
}
