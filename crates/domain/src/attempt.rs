//! Bamep Domain: `Attempt` — one concrete execution of a `JobStep`
//! (`docs/specifications/m0-job-lifecycle-and-scheduling.md` "Domain model";
//! `docs/decisions/0006-job-jobstep-attempt-state-model-and-scheduling.md`).
//!
//! Issue #25 introduces only the minimum durable Attempt identity/state model
//! needed at the final destructive-dispatch boundary: [`AttemptId`] (Server
//! Domain identity), the owning `JobStepId` (carried on [`Attempt`], not
//! here), [`ActionId`] (Agent Protocol wire-identity correlation), and
//! [`AttemptState`]. `attempt_id` and `action_id` remain distinct identities
//! even though correlated 1:1
//! (`docs/specifications/m0-persistence-observability-and-domain-events.md`
//! "Correlation") — a JobStep may accumulate more than one `Attempt` over its
//! lifetime once retry policy exists, so neither identity is ever reused.
//!
//! [`ActionId`] is deliberately a narrow Domain identity backed by a UUID v4,
//! not a re-export of `bamep_agent_protocol::ProtocolId` — Domain must not
//! depend on the Agent Protocol wire crate merely to reuse that type. Every
//! [`ActionId`] this module produces is a valid UUID v4, so a later Work
//! Package (#26) can convert the exact committed UUID into `ProtocolId`
//! without generating a replacement identity.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::job::JobStepId;

/// Server Domain identity for one [`Attempt`]
/// (`m0-job-lifecycle-and-scheduling.md` "Domain model": "Attempt — one
/// concrete execution of a JobStep"). Distinct from [`ActionId`] even though
/// correlated 1:1 with it on every `Attempt` this module constructs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AttemptId(pub Uuid);

impl AttemptId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for AttemptId {
    fn default() -> Self {
        Self::new()
    }
}

/// The Agent Protocol wire action identity correlated 1:1 with one
/// [`Attempt`] (`m0-persistence-observability-and-domain-events.md`
/// "Correlation": "`attempt_id` ... and `action_id` ... remain distinct even
/// when related 1:1"). Always a UUID v4 — the Agent Protocol contract
/// requires `action_id` to be a UUID v4 — so #26 can convert the committed
/// value exactly into `bamep_agent_protocol::ProtocolId::from_uuid` without
/// generating a replacement identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActionId(pub Uuid);

impl ActionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ActionId {
    fn default() -> Self {
        Self::new()
    }
}

/// The full authoritative Attempt state vocabulary
/// (`m0-job-lifecycle-and-scheduling.md` "Attempt lifecycle"). Issue #25
/// creates only `Dispatched`; every later transition
/// (`InProgress`/`AwaitingReconciliation`/terminal states) belongs to #26 and
/// beyond. The full vocabulary is represented here so those later Work
/// Packages do not need a Domain type change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttemptState {
    Dispatched,
    InProgress,
    AwaitingReconciliation,
    Succeeded,
    Failed,
    Cancelled,
    Rejected,
    Indeterminate,
}

/// One concrete execution of a `JobStep`, corresponding 1:1 to one Agent
/// Protocol `action_id` lifecycle (`m0-job-lifecycle-and-scheduling.md`
/// "Domain model"). No concrete `action_type`/`action_version`/`parameters`
/// are represented — those belong to #26, where `ActionDispatch`
/// transmission is actually introduced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub id: AttemptId,
    pub job_step_id: JobStepId,
    pub action_id: ActionId,
    pub state: AttemptState,
}
