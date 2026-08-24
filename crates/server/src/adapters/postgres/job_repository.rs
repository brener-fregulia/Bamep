//! `JobRepository` Port implementation against real PostgreSQL
//! (`m0-job-lifecycle-and-scheduling.md` "Domain model"; Issue #24 "durable
//! workflow creation" boundary).
//!
//! `create_workflow` locks the target `endpoints` row, verifies it is
//! `Enrolled`, and inserts the `jobs` row plus every `job_steps` row in the
//! same transaction — atomic Job + ordered JobStep creation, with no partial
//! workflow possible on any failure path.

use async_trait::async_trait;
use bamep_domain::{
    ActionId, Attempt, AttemptId, AttemptState, DestructiveIntent, EndpointId, InventoryRevisionId,
    Job, JobAdmissionError, JobId, JobState, JobStep, JobStepEligibilityError,
    JobStepFailureReason, JobStepId, JobStepState, TargetFingerprint,
};
use sqlx::{PgPool, Row};

use super::shared::{
    actor_label, event_payload, to_backend_err, PgAuditActorKind, PgDomainEventType,
    PgIdentityState,
};
use crate::ports::{
    ActionEvidenceLockedFacts, AdmitJobDecision, AdmitJobError, ApplyActionEvidenceDecision,
    ApplyActionEvidenceDecisionOutcome, ApplyActionEvidenceError, ApplyActionEvidenceResult,
    ApplyCancelAckDecision, ApplyCancelAckDecisionOutcome, ApplyCancelAckResult,
    AuthorizeDestructiveIntentDecision, AuthorizeDestructiveIntentError,
    CancellationRequestDecided, CommitDestructiveDispatchError, CreateWorkflowError,
    FinalDispatchDecision, FinalDispatchLockedFacts, JobRepository, RepositoryError,
    RequestCancellationDecision, RequestCancellationError, RequestCancellationLockedFacts,
    RequestCancellationResult, SatisfyStepPreconditionsDecision, SatisfyStepPreconditionsError,
};

/// Adapter-local representation of the `job_state` PostgreSQL ENUM
/// (`docs/development/persistence.md` "Closed categorical values"). Domain
/// (`bamep_domain::JobState`) stays free of SQLx/PostgreSQL derives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "job_state")]
enum PgJobState {
    Pending,
    Running,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
}

impl From<JobState> for PgJobState {
    fn from(state: JobState) -> Self {
        match state {
            JobState::Pending => PgJobState::Pending,
            JobState::Running => PgJobState::Running,
            JobState::Cancelling => PgJobState::Cancelling,
            JobState::Succeeded => PgJobState::Succeeded,
            JobState::Failed => PgJobState::Failed,
            JobState::Cancelled => PgJobState::Cancelled,
        }
    }
}

impl From<PgJobState> for JobState {
    fn from(state: PgJobState) -> Self {
        match state {
            PgJobState::Pending => JobState::Pending,
            PgJobState::Running => JobState::Running,
            PgJobState::Cancelling => JobState::Cancelling,
            PgJobState::Succeeded => JobState::Succeeded,
            PgJobState::Failed => JobState::Failed,
            PgJobState::Cancelled => JobState::Cancelled,
        }
    }
}

/// Adapter-local representation of the `job_step_state` PostgreSQL ENUM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "job_step_state")]
enum PgJobStepState {
    Pending,
    PreconditionsSatisfied,
    Dispatching,
    Succeeded,
    Failed,
    Cancelled,
}

impl From<JobStepState> for PgJobStepState {
    fn from(state: JobStepState) -> Self {
        match state {
            JobStepState::Pending => PgJobStepState::Pending,
            JobStepState::PreconditionsSatisfied => PgJobStepState::PreconditionsSatisfied,
            JobStepState::Dispatching => PgJobStepState::Dispatching,
            JobStepState::Succeeded => PgJobStepState::Succeeded,
            JobStepState::Failed => PgJobStepState::Failed,
            JobStepState::Cancelled => PgJobStepState::Cancelled,
        }
    }
}

impl From<PgJobStepState> for JobStepState {
    fn from(state: PgJobStepState) -> Self {
        match state {
            PgJobStepState::Pending => JobStepState::Pending,
            PgJobStepState::PreconditionsSatisfied => JobStepState::PreconditionsSatisfied,
            PgJobStepState::Dispatching => JobStepState::Dispatching,
            PgJobStepState::Succeeded => JobStepState::Succeeded,
            PgJobStepState::Failed => JobStepState::Failed,
            PgJobStepState::Cancelled => JobStepState::Cancelled,
        }
    }
}

/// Adapter-local representation of the `job_step_failure_reason` PostgreSQL
/// ENUM (Issue #26). Domain (`bamep_domain::JobStepFailureReason`) stays free
/// of SQLx/PostgreSQL derives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "job_step_failure_reason")]
enum PgJobStepFailureReason {
    DispatchRejected,
    ExecutionFailed,
}

impl From<JobStepFailureReason> for PgJobStepFailureReason {
    fn from(reason: JobStepFailureReason) -> Self {
        match reason {
            JobStepFailureReason::DispatchRejected => PgJobStepFailureReason::DispatchRejected,
            JobStepFailureReason::ExecutionFailed => PgJobStepFailureReason::ExecutionFailed,
        }
    }
}

impl From<PgJobStepFailureReason> for JobStepFailureReason {
    fn from(reason: PgJobStepFailureReason) -> Self {
        match reason {
            PgJobStepFailureReason::DispatchRejected => JobStepFailureReason::DispatchRejected,
            PgJobStepFailureReason::ExecutionFailed => JobStepFailureReason::ExecutionFailed,
        }
    }
}

/// Adapter-local representation of the `attempt_state` PostgreSQL ENUM
/// (`docs/development/persistence.md` "Closed categorical values"). Domain
/// (`bamep_domain::AttemptState`) stays free of SQLx/PostgreSQL derives. Only
/// `Dispatched` is ever persisted by this Work Package (#25); the remaining
/// variants are mapped so later Work Packages do not need a schema/Adapter
/// type change.
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

impl From<AttemptState> for PgAttemptState {
    fn from(state: AttemptState) -> Self {
        match state {
            AttemptState::Dispatched => PgAttemptState::Dispatched,
            AttemptState::InProgress => PgAttemptState::InProgress,
            AttemptState::AwaitingReconciliation => PgAttemptState::AwaitingReconciliation,
            AttemptState::Succeeded => PgAttemptState::Succeeded,
            AttemptState::Failed => PgAttemptState::Failed,
            AttemptState::Cancelled => PgAttemptState::Cancelled,
            AttemptState::Rejected => PgAttemptState::Rejected,
            AttemptState::Indeterminate => PgAttemptState::Indeterminate,
        }
    }
}

impl From<PgAttemptState> for AttemptState {
    fn from(state: PgAttemptState) -> Self {
        match state {
            PgAttemptState::Dispatched => AttemptState::Dispatched,
            PgAttemptState::InProgress => AttemptState::InProgress,
            PgAttemptState::AwaitingReconciliation => AttemptState::AwaitingReconciliation,
            PgAttemptState::Succeeded => AttemptState::Succeeded,
            PgAttemptState::Failed => AttemptState::Failed,
            PgAttemptState::Cancelled => AttemptState::Cancelled,
            PgAttemptState::Rejected => AttemptState::Rejected,
            PgAttemptState::Indeterminate => AttemptState::Indeterminate,
        }
    }
}

pub struct PostgresJobRepository {
    pool: PgPool,
}

impl PostgresJobRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl JobRepository for PostgresJobRepository {
    async fn create_workflow(&self, job: &Job) -> Result<(), CreateWorkflowError> {
        let mut tx = self.pool.begin().await.map_err(to_backend_err)?;

        // Lock the target Endpoint for the duration of this transaction so a
        // concurrent identity transition (e.g. retirement) cannot race
        // between this check and the inserts below.
        let identity_state: Option<PgIdentityState> =
            sqlx::query_scalar("SELECT identity_state FROM endpoints WHERE id = $1 FOR UPDATE")
                .bind(job.endpoint_id.0)
                .fetch_optional(&mut *tx)
                .await
                .map_err(to_backend_err)?;

        let Some(identity_state) = identity_state else {
            tx.rollback().await.map_err(to_backend_err)?;
            return Err(CreateWorkflowError::EndpointNotFound(job.endpoint_id));
        };
        if identity_state != PgIdentityState::Enrolled {
            tx.rollback().await.map_err(to_backend_err)?;
            return Err(CreateWorkflowError::EndpointNotEnrolled(job.endpoint_id));
        }

        sqlx::query("INSERT INTO jobs (id, endpoint_id, state) VALUES ($1, $2, $3)")
            .bind(job.id.0)
            .bind(job.endpoint_id.0)
            .bind(PgJobState::from(job.state))
            .execute(&mut *tx)
            .await
            .map_err(to_backend_err)?;

        for step in &job.steps {
            sqlx::query(
                "INSERT INTO job_steps (id, job_id, step_order, state) VALUES ($1, $2, $3, $4)",
            )
            .bind(step.id.0)
            .bind(step.job_id.0)
            .bind(step.order)
            .bind(PgJobStepState::from(step.state))
            .execute(&mut *tx)
            .await
            .map_err(to_backend_err)?;
        }

        tx.commit().await.map_err(to_backend_err)?;
        Ok(())
    }

    async fn find_job(&self, id: JobId) -> Result<Option<Job>, RepositoryError> {
        let job_row = sqlx::query("SELECT endpoint_id, state FROM jobs WHERE id = $1")
            .bind(id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(to_backend_err)?;
        let Some(job_row) = job_row else {
            return Ok(None);
        };
        let endpoint_id: uuid::Uuid = job_row.try_get("endpoint_id").map_err(to_backend_err)?;
        let state: PgJobState = job_row.try_get("state").map_err(to_backend_err)?;

        let step_rows = sqlx::query(
            "SELECT id, job_id, step_order, state, \
                    authorized_inventory_revision_id, authorized_target_fingerprint, failure_reason \
             FROM job_steps WHERE job_id = $1 ORDER BY step_order ASC",
        )
        .bind(id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(to_backend_err)?;

        let mut steps = Vec::with_capacity(step_rows.len());
        for row in &step_rows {
            steps.push(row_to_job_step(row)?);
        }

        Ok(Some(Job {
            id,
            endpoint_id: EndpointId(endpoint_id),
            state: state.into(),
            steps,
        }))
    }

    async fn authorize_destructive_intent(
        &self,
        job_id: JobId,
        step_id: JobStepId,
        decide: AuthorizeDestructiveIntentDecision,
    ) -> Result<DestructiveIntent, AuthorizeDestructiveIntentError> {
        let mut tx = self.pool.begin().await.map_err(to_backend_err)?;

        let row = sqlx::query(
            "SELECT id, job_id, step_order, state, \
                    authorized_inventory_revision_id, authorized_target_fingerprint, failure_reason \
             FROM job_steps WHERE id = $1 AND job_id = $2 FOR UPDATE",
        )
        .bind(step_id.0)
        .bind(job_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(to_backend_err)?;

        let Some(row) = row else {
            tx.rollback().await.map_err(to_backend_err)?;
            return Err(AuthorizeDestructiveIntentError::JobStepNotFound(
                step_id, job_id,
            ));
        };
        let step = row_to_job_step(&row).map_err(AuthorizeDestructiveIntentError::Repository)?;

        let intent = match decide(&step) {
            Ok(intent) => intent,
            Err(bamep_domain::DestructiveIntentError::WrongJob) => {
                tx.rollback().await.map_err(to_backend_err)?;
                return Err(AuthorizeDestructiveIntentError::JobStepNotFound(
                    step_id, job_id,
                ));
            }
            Err(bamep_domain::DestructiveIntentError::NotEligible) => {
                tx.rollback().await.map_err(to_backend_err)?;
                return Err(AuthorizeDestructiveIntentError::NotEligible(step_id));
            }
            Err(bamep_domain::DestructiveIntentError::AlreadyAuthorized) => {
                tx.rollback().await.map_err(to_backend_err)?;
                return Err(AuthorizeDestructiveIntentError::AlreadyAuthorized(step_id));
            }
        };

        sqlx::query(
            "UPDATE job_steps SET authorized_inventory_revision_id = $1, \
             authorized_target_fingerprint = $2 WHERE id = $3",
        )
        .bind(intent.authorized_inventory_revision_id.0)
        .bind(intent.authorized_target_fingerprint.as_str())
        .bind(step_id.0)
        .execute(&mut *tx)
        .await
        .map_err(to_backend_err)?;

        tx.commit().await.map_err(to_backend_err)?;
        Ok(intent)
    }

    async fn admit_job(
        &self,
        job_id: JobId,
        decide: AdmitJobDecision,
    ) -> Result<Job, AdmitJobError> {
        let mut tx = self.pool.begin().await.map_err(to_backend_err)?;

        let job_row = sqlx::query("SELECT endpoint_id, state FROM jobs WHERE id = $1 FOR UPDATE")
            .bind(job_id.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(to_backend_err)?;
        let Some(job_row) = job_row else {
            tx.rollback().await.map_err(to_backend_err)?;
            return Err(AdmitJobError::JobNotFound(job_id));
        };
        let endpoint_id: uuid::Uuid = job_row.try_get("endpoint_id").map_err(to_backend_err)?;
        let state: PgJobState = job_row.try_get("state").map_err(to_backend_err)?;

        let steps = fetch_job_steps(&mut tx, job_id, false)
            .await
            .map_err(AdmitJobError::Repository)?;

        let job = Job {
            id: job_id,
            endpoint_id: EndpointId(endpoint_id),
            state: state.into(),
            steps,
        };

        let outcome = match decide(&job) {
            Ok(outcome) => outcome,
            Err(JobAdmissionError::NotPending) => {
                tx.rollback().await.map_err(to_backend_err)?;
                return Err(AdmitJobError::NotEligible(job_id));
            }
        };

        // The `WHERE state = 'Pending'` guard is defensive: holding this
        // exact job row's lock since the SELECT above already guarantees no
        // concurrent writer could have changed it. A same-Endpoint admission
        // race is decided below by the active-Job uniqueness constraint
        // instead, since that race is between two *different* job rows.
        let update_result =
            sqlx::query("UPDATE jobs SET state = $1 WHERE id = $2 AND state = 'Pending'")
                .bind(PgJobState::from(outcome.job.state))
                .bind(job_id.0)
                .execute(&mut *tx)
                .await;

        match update_result {
            Ok(result) if result.rows_affected() == 1 => {}
            Ok(_) => {
                tx.rollback().await.map_err(to_backend_err)?;
                return Err(AdmitJobError::NotEligible(job_id));
            }
            Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => {
                tx.rollback().await.map_err(to_backend_err)?;
                return Err(AdmitJobError::EndpointNotAvailable);
            }
            Err(e) => return Err(AdmitJobError::Repository(to_backend_err(e))),
        }

        let event = &outcome.event;
        sqlx::query(
            "INSERT INTO domain_events \
             (event_id, event_type, event_version, endpoint_id, job_id, occurred_at, payload) \
             VALUES ($1, $2, 1, $3, $4, $5, $6)",
        )
        .bind(event.event_id())
        .bind(PgDomainEventType::from(event))
        .bind(event.endpoint_id().0)
        .bind(job_id.0)
        .bind(event.occurred_at())
        .bind(event_payload(event))
        .execute(&mut *tx)
        .await
        .map_err(to_backend_err)?;

        tx.commit().await.map_err(to_backend_err)?;
        Ok(outcome.job)
    }

    async fn satisfy_current_step_preconditions(
        &self,
        job_id: JobId,
        step_id: JobStepId,
        decide: SatisfyStepPreconditionsDecision,
    ) -> Result<JobStep, SatisfyStepPreconditionsError> {
        let mut tx = self.pool.begin().await.map_err(to_backend_err)?;

        let job_row = sqlx::query("SELECT endpoint_id, state FROM jobs WHERE id = $1 FOR UPDATE")
            .bind(job_id.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(to_backend_err)?;
        let Some(job_row) = job_row else {
            tx.rollback().await.map_err(to_backend_err)?;
            return Err(SatisfyStepPreconditionsError::JobNotFound(job_id));
        };
        let endpoint_id: uuid::Uuid = job_row.try_get("endpoint_id").map_err(to_backend_err)?;
        let state: PgJobState = job_row.try_get("state").map_err(to_backend_err)?;

        let steps = fetch_job_steps(&mut tx, job_id, true)
            .await
            .map_err(SatisfyStepPreconditionsError::Repository)?;

        let job = Job {
            id: job_id,
            endpoint_id: EndpointId(endpoint_id),
            state: state.into(),
            steps,
        };

        let updated_step = match decide(&job) {
            Ok(step) => step,
            Err(JobStepEligibilityError::JobNotRunning) => {
                tx.rollback().await.map_err(to_backend_err)?;
                return Err(SatisfyStepPreconditionsError::JobNotRunning(job_id));
            }
            Err(JobStepEligibilityError::StepNotFound) => {
                tx.rollback().await.map_err(to_backend_err)?;
                return Err(SatisfyStepPreconditionsError::JobStepNotFound(
                    step_id, job_id,
                ));
            }
            Err(JobStepEligibilityError::NotCurrent) => {
                tx.rollback().await.map_err(to_backend_err)?;
                return Err(SatisfyStepPreconditionsError::NotCurrent(step_id));
            }
        };

        sqlx::query("UPDATE job_steps SET state = $1 WHERE id = $2")
            .bind(PgJobStepState::from(updated_step.state))
            .bind(step_id.0)
            .execute(&mut *tx)
            .await
            .map_err(to_backend_err)?;

        tx.commit().await.map_err(to_backend_err)?;
        Ok(updated_step)
    }

    async fn commit_destructive_dispatch(
        &self,
        job_id: JobId,
        step_id: JobStepId,
        decide: FinalDispatchDecision,
    ) -> Result<bamep_domain::FinalDispatchOutcome, CommitDestructiveDispatchError> {
        let mut tx = self.pool.begin().await.map_err(to_backend_err)?;

        let job_row = sqlx::query("SELECT endpoint_id, state FROM jobs WHERE id = $1 FOR UPDATE")
            .bind(job_id.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(to_backend_err)?;
        let Some(job_row) = job_row else {
            tx.rollback().await.map_err(to_backend_err)?;
            return Err(CommitDestructiveDispatchError::JobNotFound(job_id));
        };
        let endpoint_id: uuid::Uuid = job_row.try_get("endpoint_id").map_err(to_backend_err)?;
        let job_state: PgJobState = job_row.try_get("state").map_err(to_backend_err)?;

        let steps = fetch_job_steps(&mut tx, job_id, true)
            .await
            .map_err(CommitDestructiveDispatchError::Repository)?;
        let job = Job {
            id: job_id,
            endpoint_id: EndpointId(endpoint_id),
            state: job_state.into(),
            steps,
        };

        let endpoint = super::shared::load_by_id_for_update(&mut tx, EndpointId(endpoint_id))
            .await
            .map_err(CommitDestructiveDispatchError::Repository)?
            .ok_or_else(|| {
                CommitDestructiveDispatchError::Repository(RepositoryError::Backend(
                    "job references an endpoint that no longer exists".to_string(),
                ))
            })?;

        let current_inventory_revision_id: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT current_inventory_revision_id FROM endpoints WHERE id = $1")
                .bind(endpoint_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(to_backend_err)?;
        let current_inventory_revision_id = current_inventory_revision_id.map(InventoryRevisionId);

        let existing_active_attempt: bool = sqlx::query_scalar(
            "SELECT EXISTS( \
                SELECT 1 FROM attempts \
                WHERE job_step_id = $1 \
                  AND state IN ('Dispatched', 'InProgress', 'AwaitingReconciliation') \
                FOR UPDATE \
             )",
        )
        .bind(step_id.0)
        .fetch_one(&mut *tx)
        .await
        .map_err(to_backend_err)?;

        let facts = FinalDispatchLockedFacts {
            job,
            endpoint,
            existing_active_attempt,
            current_inventory_revision_id,
        };

        let commit = match decide(facts) {
            Ok(commit) => commit,
            Err(denial) => {
                // The Domain decision itself already decided the exact
                // durable result, if any — this Adapter persists exactly
                // `denial.pending_job_step` and never independently encodes
                // the lifecycle rule "revalidation failure means Pending".
                if let Some(pending_step) = &denial.pending_job_step {
                    sqlx::query("UPDATE job_steps SET state = $1 WHERE id = $2")
                        .bind(PgJobStepState::from(pending_step.state))
                        .bind(step_id.0)
                        .execute(&mut *tx)
                        .await
                        .map_err(to_backend_err)?;
                    tx.commit().await.map_err(to_backend_err)?;
                } else {
                    tx.rollback().await.map_err(to_backend_err)?;
                }
                return Err(CommitDestructiveDispatchError::Rejected(denial.rejection));
            }
        };

        sqlx::query("UPDATE job_steps SET state = $1 WHERE id = $2")
            .bind(PgJobStepState::from(commit.outcome.job_step.state))
            .bind(step_id.0)
            .execute(&mut *tx)
            .await
            .map_err(to_backend_err)?;

        sqlx::query(
            "INSERT INTO attempts (id, job_step_id, action_id, state) VALUES ($1, $2, $3, $4)",
        )
        .bind(commit.outcome.attempt.id.0)
        .bind(step_id.0)
        .bind(commit.outcome.attempt.action_id.0)
        .bind(PgAttemptState::from(commit.outcome.attempt.state))
        .execute(&mut *tx)
        .await
        .map_err(to_backend_err)?;

        let audit = &commit.audit;
        sqlx::query(
            "INSERT INTO audit_records \
             (audit_id, endpoint_id, actor_kind, actor_label, occurred_at, detail, \
              job_id, job_step_id, attempt_id, action_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(audit.audit_id)
        .bind(audit.endpoint_id.0)
        .bind(PgAuditActorKind::from(&audit.actor))
        .bind(actor_label(&audit.actor))
        .bind(audit.occurred_at)
        .bind(&audit.detail)
        .bind(audit.job_id.map(|id| id.0))
        .bind(audit.job_step_id.map(|id| id.0))
        .bind(audit.attempt_id.map(|id| id.0))
        .bind(audit.action_id.map(|id| id.0))
        .execute(&mut *tx)
        .await
        .map_err(to_backend_err)?;

        tx.commit().await.map_err(to_backend_err)?;
        Ok(commit.outcome)
    }

    async fn find_attempt(
        &self,
        attempt_id: AttemptId,
    ) -> Result<Option<Attempt>, RepositoryError> {
        let row =
            sqlx::query("SELECT id, job_step_id, action_id, state FROM attempts WHERE id = $1")
                .bind(attempt_id.0)
                .fetch_optional(&self.pool)
                .await
                .map_err(to_backend_err)?;
        row.as_ref().map(row_to_attempt).transpose()
    }

    /// See the Port doc for the full lock/decide/persist contract. Lock order
    /// is exactly Attempt -> JobStep -> Job, matching the Port's stated
    /// contract; `find_job`/`fetch_job_steps` are not reused here because
    /// this method must lock the owning `job_steps` row *before* the `jobs`
    /// row, whereas every other method in this file only ever needs to lock
    /// `jobs` first.
    async fn apply_action_evidence(
        &self,
        action_id: ActionId,
        authenticated_endpoint_id: EndpointId,
        decide: ApplyActionEvidenceDecision,
    ) -> Result<ApplyActionEvidenceResult, ApplyActionEvidenceError> {
        let mut tx = self.pool.begin().await.map_err(to_backend_err)?;

        let attempt_row = sqlx::query(
            "SELECT id, job_step_id, action_id, state FROM attempts WHERE action_id = $1 FOR UPDATE",
        )
        .bind(action_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(to_backend_err)?;
        let Some(attempt_row) = attempt_row else {
            tx.rollback().await.map_err(to_backend_err)?;
            return Err(ApplyActionEvidenceError::UnknownAction);
        };
        let attempt = row_to_attempt(&attempt_row)?;

        let step_row = sqlx::query(
            "SELECT id, job_id, step_order, state, \
                    authorized_inventory_revision_id, authorized_target_fingerprint, failure_reason \
             FROM job_steps WHERE id = $1 FOR UPDATE",
        )
        .bind(attempt.job_step_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(to_backend_err)?;
        let Some(step_row) = step_row else {
            return Err(ApplyActionEvidenceError::Repository(
                RepositoryError::Backend(
                    "attempt references a job_step that no longer exists".to_string(),
                ),
            ));
        };
        let job_step = row_to_job_step(&step_row)?;

        let job_row = sqlx::query("SELECT endpoint_id, state FROM jobs WHERE id = $1 FOR UPDATE")
            .bind(job_step.job_id.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(to_backend_err)?;
        let Some(job_row) = job_row else {
            return Err(ApplyActionEvidenceError::Repository(
                RepositoryError::Backend(
                    "job_step references a job that no longer exists".to_string(),
                ),
            ));
        };
        let job_endpoint_id: uuid::Uuid = job_row.try_get("endpoint_id").map_err(to_backend_err)?;
        let job_state: PgJobState = job_row.try_get("state").map_err(to_backend_err)?;
        let steps = fetch_job_steps(&mut tx, job_step.job_id, true)
            .await
            .map_err(ApplyActionEvidenceError::Repository)?;
        let job = Job {
            id: job_step.job_id,
            endpoint_id: EndpointId(job_endpoint_id),
            state: job_state.into(),
            steps,
        };

        // Never distinguish "unknown action_id" from "known action_id
        // belonging to another Endpoint" — both roll back identically and
        // return the same generic error (`m0-agent-protocol-contract.md`;
        // Issue #26 "Authenticated Endpoint correlation").
        if job.endpoint_id != authenticated_endpoint_id {
            tx.rollback().await.map_err(to_backend_err)?;
            return Err(ApplyActionEvidenceError::UnknownAction);
        }

        let facts = ActionEvidenceLockedFacts {
            job,
            job_step,
            attempt,
        };

        match decide(facts) {
            ApplyActionEvidenceDecisionOutcome::NoOp => {
                tx.rollback().await.map_err(to_backend_err)?;
                Ok(ApplyActionEvidenceResult::NoOp)
            }
            ApplyActionEvidenceDecisionOutcome::Conflict => {
                tx.rollback().await.map_err(to_backend_err)?;
                Ok(ApplyActionEvidenceResult::Conflict)
            }
            ApplyActionEvidenceDecisionOutcome::Applied(commit) => {
                let applied = commit.outcome;

                sqlx::query("UPDATE attempts SET state = $1 WHERE id = $2")
                    .bind(PgAttemptState::from(applied.attempt.state))
                    .bind(applied.attempt.id.0)
                    .execute(&mut *tx)
                    .await
                    .map_err(to_backend_err)?;

                sqlx::query("UPDATE job_steps SET state = $1, failure_reason = $2 WHERE id = $3")
                    .bind(PgJobStepState::from(applied.job_step.state))
                    .bind(
                        applied
                            .job_step
                            .failure_reason
                            .map(PgJobStepFailureReason::from),
                    )
                    .bind(applied.job_step.id.0)
                    .execute(&mut *tx)
                    .await
                    .map_err(to_backend_err)?;

                sqlx::query("UPDATE jobs SET state = $1 WHERE id = $2")
                    .bind(PgJobState::from(applied.job.state))
                    .bind(applied.job.id.0)
                    .execute(&mut *tx)
                    .await
                    .map_err(to_backend_err)?;

                for event in &applied.events {
                    sqlx::query(
                        "INSERT INTO domain_events \
                         (event_id, event_type, event_version, endpoint_id, job_id, job_step_id, occurred_at, payload) \
                         VALUES ($1, $2, 1, $3, $4, $5, $6, $7)",
                    )
                    .bind(event.event_id())
                    .bind(PgDomainEventType::from(event))
                    .bind(event.endpoint_id().0)
                    .bind(event.job_id().map(|id| id.0))
                    .bind(event.job_step_id().map(|id| id.0))
                    .bind(event.occurred_at())
                    .bind(event_payload(event))
                    .execute(&mut *tx)
                    .await
                    .map_err(to_backend_err)?;
                }

                if let Some(audit) = &commit.audit {
                    sqlx::query(
                        "INSERT INTO audit_records \
                         (audit_id, endpoint_id, actor_kind, actor_label, occurred_at, detail, \
                          job_id, job_step_id, attempt_id, action_id) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
                    )
                    .bind(audit.audit_id)
                    .bind(audit.endpoint_id.0)
                    .bind(PgAuditActorKind::from(&audit.actor))
                    .bind(actor_label(&audit.actor))
                    .bind(audit.occurred_at)
                    .bind(&audit.detail)
                    .bind(audit.job_id.map(|id| id.0))
                    .bind(audit.job_step_id.map(|id| id.0))
                    .bind(audit.attempt_id.map(|id| id.0))
                    .bind(audit.action_id.map(|id| id.0))
                    .execute(&mut *tx)
                    .await
                    .map_err(to_backend_err)?;
                }

                tx.commit().await.map_err(to_backend_err)?;
                Ok(ApplyActionEvidenceResult::Applied(applied))
            }
        }
    }

    /// A plain, unlocked read — no transaction, no `FOR UPDATE`, nothing
    /// persisted. See the Port doc for the non-enumeration contract.
    async fn action_targets_endpoint(
        &self,
        action_id: ActionId,
        authenticated_endpoint_id: EndpointId,
    ) -> Result<bool, RepositoryError> {
        let targets: bool = sqlx::query_scalar(
            "SELECT EXISTS( \
                SELECT 1 FROM attempts \
                JOIN job_steps ON job_steps.id = attempts.job_step_id \
                JOIN jobs ON jobs.id = job_steps.job_id \
                WHERE attempts.action_id = $1 AND jobs.endpoint_id = $2 \
             )",
        )
        .bind(action_id.0)
        .bind(authenticated_endpoint_id.0)
        .fetch_one(&self.pool)
        .await
        .map_err(to_backend_err)?;
        Ok(targets)
    }

    /// See the Port doc for the full lock/decide/persist contract. Lock
    /// order preserves Attempt -> JobStep -> Job (Issue #27 "Lock order /
    /// concurrency"): an unlocked pre-scan identifies at most one candidate
    /// active/uncertain Attempt (more than one is an invariant/backend
    /// error), then locks are acquired Attempt -> JobStep -> Job, and the
    /// candidate's freshly-locked state is re-verified before `decide` runs
    /// — never trusting the pre-lock guess.
    async fn request_cancellation(
        &self,
        job_id: JobId,
        decide: RequestCancellationDecision,
    ) -> Result<RequestCancellationResult, RequestCancellationError> {
        // Bounds the missed-candidate retry below: a retry only fires when
        // the post-Job-lock scan observes an Attempt that
        // `commit_destructive_dispatch` committed between this pass's
        // unlocked pre-scan and this pass's Job-lock acquisition (Issue #27
        // follow-up "Close the no-candidate race with final dispatch"). A
        // workflow admits only one active Attempt at a time, so in practice
        // this resolves within a single retry; the bound exists only to
        // fail loudly instead of spinning forever if that invariant is ever
        // violated.
        const MAX_PASSES: u8 = 8;

        let mut passes_remaining = MAX_PASSES;
        let (mut tx, facts) = loop {
            passes_remaining -= 1;
            if passes_remaining == 0 {
                return Err(RequestCancellationError::Repository(
                    RepositoryError::Backend(
                        "request_cancellation: exceeded retry budget resolving the active \
                         Attempt race against final dispatch"
                            .to_string(),
                    ),
                ));
            }

            let candidate_rows = active_attempt_candidate_rows(&self.pool, job_id).await?;
            let candidate_attempt_id: Option<uuid::Uuid> = match candidate_rows.first() {
                Some(row) => Some(row.try_get("attempt_id").map_err(to_backend_err)?),
                None => None,
            };

            let mut tx = self.pool.begin().await.map_err(to_backend_err)?;

            let active_attempt = if let Some(attempt_id) = candidate_attempt_id {
                let attempt_row = sqlx::query(
                    "SELECT id, job_step_id, action_id, state FROM attempts WHERE id = $1 \
                     FOR UPDATE",
                )
                .bind(attempt_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(to_backend_err)?;
                let Some(attempt_row) = attempt_row else {
                    return Err(RequestCancellationError::Repository(
                        RepositoryError::Backend(
                            "candidate active attempt disappeared under lock".to_string(),
                        ),
                    ));
                };
                let attempt =
                    row_to_attempt(&attempt_row).map_err(RequestCancellationError::Repository)?;

                sqlx::query("SELECT id FROM job_steps WHERE id = $1 FOR UPDATE")
                    .bind(attempt.job_step_id.0)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(to_backend_err)?;

                // Re-verify under lock — never trust the pre-lock guess: the
                // Attempt may have independently reached a terminal state
                // between the unlocked scan and this lock acquisition (Issue
                // #27 "Lock order / concurrency", case A).
                matches!(
                    attempt.state,
                    AttemptState::Dispatched
                        | AttemptState::InProgress
                        | AttemptState::AwaitingReconciliation
                )
                .then_some(attempt)
            } else {
                None
            };

            let job_row =
                sqlx::query("SELECT endpoint_id, state FROM jobs WHERE id = $1 FOR UPDATE")
                    .bind(job_id.0)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(to_backend_err)?;
            let Some(job_row) = job_row else {
                tx.rollback().await.map_err(to_backend_err)?;
                return Err(RequestCancellationError::JobNotFound(job_id));
            };
            let endpoint_id: uuid::Uuid = job_row.try_get("endpoint_id").map_err(to_backend_err)?;
            let job_state: PgJobState = job_row.try_get("state").map_err(to_backend_err)?;

            if candidate_attempt_id.is_none() {
                // We hold the Job lock now, which serializes us against
                // `commit_destructive_dispatch` (it locks the same Job row
                // before creating an Attempt). Re-scan within the
                // transaction: PostgreSQL READ COMMITTED gives this
                // statement a fresh snapshot, so it can observe an Attempt
                // that final dispatch committed while this pass waited for
                // the Job lock — one the unlocked pre-scan above could not
                // see.
                let post_lock_rows = active_attempt_candidate_rows(&mut *tx, job_id).await?;
                if !post_lock_rows.is_empty() {
                    // An Attempt just became visible. Do not lock it while
                    // already holding the Job row — that would invert the
                    // established Attempt -> JobStep -> Job lock order and
                    // risk a deadlock cycle against
                    // `commit_destructive_dispatch`. Roll back and retry:
                    // the next pass's unlocked pre-scan will see this
                    // now-committed Attempt and take the candidate-found
                    // branch above, which locks Attempt -> JobStep -> Job
                    // correctly.
                    tx.rollback().await.map_err(to_backend_err)?;
                    continue;
                }
            }

            let steps = fetch_job_steps(&mut tx, job_id, true)
                .await
                .map_err(RequestCancellationError::Repository)?;
            let job = Job {
                id: job_id,
                endpoint_id: EndpointId(endpoint_id),
                state: job_state.into(),
                steps,
            };

            break (
                tx,
                RequestCancellationLockedFacts {
                    job,
                    active_attempt,
                },
            );
        };

        let decided = match decide(facts) {
            Ok(decided) => decided,
            Err(bamep_domain::CancellationRequestError::NotEligible) => {
                tx.rollback().await.map_err(to_backend_err)?;
                return Err(RequestCancellationError::NotEligible(job_id));
            }
        };

        match decided {
            CancellationRequestDecided::EnteredCancelling {
                job,
                attempt_id,
                action_id,
                audit,
            } => {
                sqlx::query("UPDATE jobs SET state = $1 WHERE id = $2")
                    .bind(PgJobState::from(job.state))
                    .bind(job_id.0)
                    .execute(&mut *tx)
                    .await
                    .map_err(to_backend_err)?;
                insert_audit_record(&mut tx, &audit).await?;
                tx.commit().await.map_err(to_backend_err)?;
                Ok(RequestCancellationResult::EnteredCancelling {
                    attempt_id,
                    action_id,
                    endpoint_id: job.endpoint_id,
                })
            }
            CancellationRequestDecided::CompletedImmediately { job, event, audit } => {
                sqlx::query("UPDATE jobs SET state = $1 WHERE id = $2")
                    .bind(PgJobState::from(job.state))
                    .bind(job_id.0)
                    .execute(&mut *tx)
                    .await
                    .map_err(to_backend_err)?;
                sqlx::query(
                    "INSERT INTO domain_events \
                     (event_id, event_type, event_version, endpoint_id, job_id, occurred_at, payload) \
                     VALUES ($1, $2, 1, $3, $4, $5, $6)",
                )
                .bind(event.event_id())
                .bind(PgDomainEventType::from(&event))
                .bind(event.endpoint_id().0)
                .bind(job_id.0)
                .bind(event.occurred_at())
                .bind(event_payload(&event))
                .execute(&mut *tx)
                .await
                .map_err(to_backend_err)?;
                insert_audit_record(&mut tx, &audit).await?;
                tx.commit().await.map_err(to_backend_err)?;
                Ok(RequestCancellationResult::CompletedImmediately)
            }
            CancellationRequestDecided::AlreadyCancelling => {
                tx.rollback().await.map_err(to_backend_err)?;
                Ok(RequestCancellationResult::AlreadyCancelling)
            }
            CancellationRequestDecided::AlreadyTerminal => {
                tx.rollback().await.map_err(to_backend_err)?;
                Ok(RequestCancellationResult::AlreadyTerminal)
            }
        }
    }

    /// See the Port doc. Lock order and correlation mirror
    /// `apply_action_evidence` exactly: Attempt -> JobStep -> Job by
    /// `action_id`, verifying the owning Job targets
    /// `authenticated_endpoint_id`.
    async fn apply_cancel_ack(
        &self,
        action_id: ActionId,
        authenticated_endpoint_id: EndpointId,
        decide: ApplyCancelAckDecision,
    ) -> Result<ApplyCancelAckResult, ApplyActionEvidenceError> {
        let mut tx = self.pool.begin().await.map_err(to_backend_err)?;

        let attempt_row = sqlx::query(
            "SELECT id, job_step_id, action_id, state FROM attempts WHERE action_id = $1 FOR UPDATE",
        )
        .bind(action_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(to_backend_err)?;
        let Some(attempt_row) = attempt_row else {
            tx.rollback().await.map_err(to_backend_err)?;
            return Err(ApplyActionEvidenceError::UnknownAction);
        };
        let attempt = row_to_attempt(&attempt_row)?;

        let step_row = sqlx::query(
            "SELECT id, job_id, step_order, state, \
                    authorized_inventory_revision_id, authorized_target_fingerprint, failure_reason \
             FROM job_steps WHERE id = $1 FOR UPDATE",
        )
        .bind(attempt.job_step_id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(to_backend_err)?;
        let Some(step_row) = step_row else {
            return Err(ApplyActionEvidenceError::Repository(
                RepositoryError::Backend(
                    "attempt references a job_step that no longer exists".to_string(),
                ),
            ));
        };
        let job_step = row_to_job_step(&step_row)?;

        let job_row = sqlx::query("SELECT endpoint_id, state FROM jobs WHERE id = $1 FOR UPDATE")
            .bind(job_step.job_id.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(to_backend_err)?;
        let Some(job_row) = job_row else {
            return Err(ApplyActionEvidenceError::Repository(
                RepositoryError::Backend(
                    "job_step references a job that no longer exists".to_string(),
                ),
            ));
        };
        let job_endpoint_id: uuid::Uuid = job_row.try_get("endpoint_id").map_err(to_backend_err)?;
        let job_state: PgJobState = job_row.try_get("state").map_err(to_backend_err)?;
        let steps = fetch_job_steps(&mut tx, job_step.job_id, true)
            .await
            .map_err(ApplyActionEvidenceError::Repository)?;
        let job = Job {
            id: job_step.job_id,
            endpoint_id: EndpointId(job_endpoint_id),
            state: job_state.into(),
            steps,
        };

        if job.endpoint_id != authenticated_endpoint_id {
            tx.rollback().await.map_err(to_backend_err)?;
            return Err(ApplyActionEvidenceError::UnknownAction);
        }

        let facts = ActionEvidenceLockedFacts {
            job,
            job_step,
            attempt,
        };

        match decide(facts) {
            ApplyCancelAckDecisionOutcome::NoOp => {
                tx.rollback().await.map_err(to_backend_err)?;
                Ok(ApplyCancelAckResult::NoOp)
            }
            ApplyCancelAckDecisionOutcome::Applied(commit) => {
                let applied = commit.outcome;

                sqlx::query("UPDATE attempts SET state = $1 WHERE id = $2")
                    .bind(PgAttemptState::from(applied.attempt.state))
                    .bind(applied.attempt.id.0)
                    .execute(&mut *tx)
                    .await
                    .map_err(to_backend_err)?;

                sqlx::query("UPDATE job_steps SET state = $1, failure_reason = $2 WHERE id = $3")
                    .bind(PgJobStepState::from(applied.job_step.state))
                    .bind(
                        applied
                            .job_step
                            .failure_reason
                            .map(PgJobStepFailureReason::from),
                    )
                    .bind(applied.job_step.id.0)
                    .execute(&mut *tx)
                    .await
                    .map_err(to_backend_err)?;

                sqlx::query("UPDATE jobs SET state = $1 WHERE id = $2")
                    .bind(PgJobState::from(applied.job.state))
                    .bind(applied.job.id.0)
                    .execute(&mut *tx)
                    .await
                    .map_err(to_backend_err)?;

                for event in &applied.events {
                    sqlx::query(
                        "INSERT INTO domain_events \
                         (event_id, event_type, event_version, endpoint_id, job_id, job_step_id, occurred_at, payload) \
                         VALUES ($1, $2, 1, $3, $4, $5, $6, $7)",
                    )
                    .bind(event.event_id())
                    .bind(PgDomainEventType::from(event))
                    .bind(event.endpoint_id().0)
                    .bind(event.job_id().map(|id| id.0))
                    .bind(event.job_step_id().map(|id| id.0))
                    .bind(event.occurred_at())
                    .bind(event_payload(event))
                    .execute(&mut *tx)
                    .await
                    .map_err(to_backend_err)?;
                }

                if let Some(audit) = &commit.audit {
                    insert_audit_record(&mut tx, audit)
                        .await
                        .map_err(ApplyActionEvidenceError::Repository)?;
                }

                tx.commit().await.map_err(to_backend_err)?;
                Ok(ApplyCancelAckResult::Applied(applied))
            }
        }
    }
}

/// Scans for the JobStep-current Attempt(s) in `Dispatched`/`InProgress`/
/// `AwaitingReconciliation` for `job_id`, used by
/// [`PostgresJobRepository::request_cancellation`] both unlocked (the
/// initial pre-scan, over `&self.pool`) and, when that pre-scan found no
/// candidate, again inside the transaction after the Job row lock is held
/// (over `&mut *tx`) — generic so both callers share one query and one
/// invariant check. More than one simultaneously active/uncertain Attempt
/// for a Job is never guessed at — it is an invariant/backend error rather
/// than an arbitrarily selected candidate (Issue #27 "Active Attempt
/// selection").
async fn active_attempt_candidate_rows<'e, E>(
    executor: E,
    job_id: JobId,
) -> Result<Vec<sqlx::postgres::PgRow>, RequestCancellationError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let rows = sqlx::query(
        "SELECT a.id AS attempt_id \
         FROM attempts a \
         JOIN job_steps js ON js.id = a.job_step_id \
         WHERE js.job_id = $1 AND js.state = 'Dispatching' \
           AND a.state IN ('Dispatched', 'InProgress', 'AwaitingReconciliation')",
    )
    .bind(job_id.0)
    .fetch_all(executor)
    .await
    .map_err(to_backend_err)?;
    if rows.len() > 1 {
        return Err(RequestCancellationError::Repository(
            RepositoryError::Backend(
                "invariant violation: more than one simultaneously active/uncertain \
                 attempt for job"
                    .to_string(),
            ),
        ));
    }
    Ok(rows)
}

/// Shared `audit_records` insert used by [`PostgresJobRepository::request_cancellation`]
/// and [`PostgresJobRepository::apply_cancel_ack`].
async fn insert_audit_record(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    audit: &bamep_domain::AuditRecord,
) -> Result<(), RepositoryError> {
    sqlx::query(
        "INSERT INTO audit_records \
         (audit_id, endpoint_id, actor_kind, actor_label, occurred_at, detail, \
          job_id, job_step_id, attempt_id, action_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(audit.audit_id)
    .bind(audit.endpoint_id.0)
    .bind(PgAuditActorKind::from(&audit.actor))
    .bind(actor_label(&audit.actor))
    .bind(audit.occurred_at)
    .bind(&audit.detail)
    .bind(audit.job_id.map(|id| id.0))
    .bind(audit.job_step_id.map(|id| id.0))
    .bind(audit.attempt_id.map(|id| id.0))
    .bind(audit.action_id.map(|id| id.0))
    .execute(&mut **tx)
    .await
    .map_err(to_backend_err)?;
    Ok(())
}

fn row_to_attempt(row: &sqlx::postgres::PgRow) -> Result<Attempt, RepositoryError> {
    let id: uuid::Uuid = row.try_get("id").map_err(to_backend_err)?;
    let job_step_id: uuid::Uuid = row.try_get("job_step_id").map_err(to_backend_err)?;
    let action_id: uuid::Uuid = row.try_get("action_id").map_err(to_backend_err)?;
    let state: PgAttemptState = row.try_get("state").map_err(to_backend_err)?;
    Ok(Attempt {
        id: AttemptId(id),
        job_step_id: JobStepId(job_step_id),
        action_id: ActionId(action_id),
        state: state.into(),
    })
}

/// Reads every `JobStep` for `job_id`, ordered, within `tx`. `lock` rows
/// `FOR UPDATE` when the caller is about to decide and persist a step
/// transition (`satisfy_current_step_preconditions`); admission does not
/// mutate `job_steps` at all, so it reads them unlocked.
async fn fetch_job_steps(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    job_id: JobId,
    lock: bool,
) -> Result<Vec<JobStep>, RepositoryError> {
    const UNLOCKED: &str = "SELECT id, job_id, step_order, state, \
         authorized_inventory_revision_id, authorized_target_fingerprint, failure_reason \
         FROM job_steps WHERE job_id = $1 ORDER BY step_order ASC";
    const LOCKED: &str = "SELECT id, job_id, step_order, state, \
         authorized_inventory_revision_id, authorized_target_fingerprint, failure_reason \
         FROM job_steps WHERE job_id = $1 ORDER BY step_order ASC FOR UPDATE";

    let step_rows = sqlx::query(if lock { LOCKED } else { UNLOCKED })
        .bind(job_id.0)
        .fetch_all(&mut **tx)
        .await
        .map_err(to_backend_err)?;

    let mut steps = Vec::with_capacity(step_rows.len());
    for row in &step_rows {
        steps.push(row_to_job_step(row)?);
    }
    Ok(steps)
}

fn row_to_job_step(row: &sqlx::postgres::PgRow) -> Result<JobStep, RepositoryError> {
    let step_id: uuid::Uuid = row.try_get("id").map_err(to_backend_err)?;
    let step_job_id: uuid::Uuid = row.try_get("job_id").map_err(to_backend_err)?;
    let order: i32 = row.try_get("step_order").map_err(to_backend_err)?;
    let step_state: PgJobStepState = row.try_get("state").map_err(to_backend_err)?;
    let revision_id: Option<uuid::Uuid> = row
        .try_get("authorized_inventory_revision_id")
        .map_err(to_backend_err)?;
    let fingerprint: Option<String> = row
        .try_get("authorized_target_fingerprint")
        .map_err(to_backend_err)?;
    let destructive_intent = match (revision_id, fingerprint) {
        (Some(revision_id), Some(fingerprint)) => Some(DestructiveIntent {
            authorized_inventory_revision_id: InventoryRevisionId(revision_id),
            authorized_target_fingerprint: TargetFingerprint::new(fingerprint),
        }),
        (None, None) => None,
        _ => {
            return Err(RepositoryError::Backend(
                "persisted job_steps destructive intent is partially populated".to_string(),
            ))
        }
    };
    let failure_reason: Option<PgJobStepFailureReason> =
        row.try_get("failure_reason").map_err(to_backend_err)?;

    Ok(JobStep {
        id: JobStepId(step_id),
        job_id: JobId(step_job_id),
        order,
        state: step_state.into(),
        destructive_intent,
        failure_reason: failure_reason.map(Into::into),
    })
}
