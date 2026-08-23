//! `JobRepository` Port implementation against real PostgreSQL
//! (`m0-job-lifecycle-and-scheduling.md` "Domain model"; Issue #24 "durable
//! workflow creation" boundary).
//!
//! `create_workflow` locks the target `endpoints` row, verifies it is
//! `Enrolled`, and inserts the `jobs` row plus every `job_steps` row in the
//! same transaction — atomic Job + ordered JobStep creation, with no partial
//! workflow possible on any failure path.

use async_trait::async_trait;
use bamep_domain::{EndpointId, Job, JobId, JobState, JobStep, JobStepId, JobStepState};
use sqlx::{PgPool, Row};

use super::shared::{to_backend_err, PgIdentityState};
use crate::ports::{CreateWorkflowError, JobRepository, RepositoryError};

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
            "SELECT id, job_id, step_order, state FROM job_steps \
             WHERE job_id = $1 ORDER BY step_order ASC",
        )
        .bind(id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(to_backend_err)?;

        let mut steps = Vec::with_capacity(step_rows.len());
        for row in &step_rows {
            let step_id: uuid::Uuid = row.try_get("id").map_err(to_backend_err)?;
            let step_job_id: uuid::Uuid = row.try_get("job_id").map_err(to_backend_err)?;
            let order: i32 = row.try_get("step_order").map_err(to_backend_err)?;
            let step_state: PgJobStepState = row.try_get("state").map_err(to_backend_err)?;
            steps.push(JobStep {
                id: JobStepId(step_id),
                job_id: JobId(step_job_id),
                order,
                state: step_state.into(),
            });
        }

        Ok(Some(Job {
            id,
            endpoint_id: EndpointId(endpoint_id),
            state: state.into(),
            steps,
        }))
    }
}
