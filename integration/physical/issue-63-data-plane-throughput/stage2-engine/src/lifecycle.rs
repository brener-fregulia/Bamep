//! Typed per-case lifecycle + the matrix sequencer's fail/contaminate policy.
//! PURE.
//!
//! Per-case state machine (strict linear order):
//!
//! ```text
//! Planned -> ServerReady -> CredentialReady -> RunnerReady -> ClockChecked
//!   -> SourceObserved -> SourceValidated -> TransferStarted -> StreamCompleted
//!   -> SealCompleted -> ArtifactVerified -> Completed
//! ```
//!
//! Terminal off-nominal: `Failed { at, reason }` / `Contaminated { at, reason }`.
//! Both LATCH: no further transition is accepted.
//!
//! FAILURE / CONTAMINATION POLICY (Stage-3 clean matrix, ZERO deliberate fault
//! injection): if ANY case is `Failed` or `Contaminated`, the sequencer STOPS
//! handing out new cases, preserves all evidence, and the matrix fails. There
//! is NO automatic "retry until green" and NO silent repeat of a measured case.

use crate::matrix::{Case, Phase, RunPlan};

/// The ordered nominal states. The discriminant is the linear position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CaseState {
    Planned = 0,
    ServerReady = 1,
    CredentialReady = 2,
    RunnerReady = 3,
    ClockChecked = 4,
    SourceObserved = 5,
    SourceValidated = 6,
    TransferStarted = 7,
    StreamCompleted = 8,
    SealCompleted = 9,
    ArtifactVerified = 10,
    Completed = 11,
}

impl CaseState {
    fn next(self) -> Option<CaseState> {
        use CaseState::*;
        Some(match self {
            Planned => ServerReady,
            ServerReady => CredentialReady,
            CredentialReady => RunnerReady,
            RunnerReady => ClockChecked,
            ClockChecked => SourceObserved,
            SourceObserved => SourceValidated,
            SourceValidated => TransferStarted,
            TransferStarted => StreamCompleted,
            StreamCompleted => SealCompleted,
            SealCompleted => ArtifactVerified,
            ArtifactVerified => Completed,
            Completed => return None,
        })
    }
}

/// Correlation carried on every case event (spec: preserve correlation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Correlation {
    pub run_id: String,
    pub case_id: String,
    pub phase: Phase,
    pub cycle: Option<u8>,
    pub slot: Option<u8>,
    pub chunk_size_bytes: u64,
    pub extent_bytes: u64,
    pub transfer_id: Option<String>,
    pub artifact_id: Option<String>,
}

impl Correlation {
    pub fn from_case(c: &Case) -> Self {
        Self {
            run_id: c.run_id.clone(),
            case_id: c.case_id.clone(),
            phase: c.phase,
            cycle: c.cycle,
            slot: c.slot,
            chunk_size_bytes: c.chunk_size_bytes,
            extent_bytes: c.extent_bytes,
            transfer_id: None,
            artifact_id: None,
        }
    }
}

/// The terminal disposition of a case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaseOutcome {
    Running(CaseState),
    Completed,
    Failed { at: CaseState, reason: String },
    Contaminated { at: CaseState, reason: String },
}

impl CaseOutcome {
    pub fn is_bad(&self) -> bool {
        matches!(self, CaseOutcome::Failed { .. } | CaseOutcome::Contaminated { .. })
    }
    pub fn is_completed(&self) -> bool {
        matches!(self, CaseOutcome::Completed)
    }
    pub fn is_terminal(&self) -> bool {
        !matches!(self, CaseOutcome::Running(_))
    }
}

/// Why a transition was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransitionError {
    /// Requested a state that is not the immediate successor of the current one.
    NotSequential { from: CaseState, requested: CaseState },
    /// The case is already terminal (Completed / Failed / Contaminated).
    AlreadyTerminal,
}

/// One case's state machine.
#[derive(Debug, Clone)]
pub struct CaseMachine {
    outcome: CaseOutcome,
    correlation: Correlation,
}

impl CaseMachine {
    pub fn new(case: &Case) -> Self {
        Self {
            outcome: CaseOutcome::Running(CaseState::Planned),
            correlation: Correlation::from_case(case),
        }
    }

    pub fn outcome(&self) -> &CaseOutcome {
        &self.outcome
    }

    pub fn correlation(&self) -> &Correlation {
        &self.correlation
    }

    pub fn state(&self) -> Option<CaseState> {
        match self.outcome {
            CaseOutcome::Running(s) => Some(s),
            _ => None,
        }
    }

    pub fn set_transfer_id(&mut self, id: impl Into<String>) {
        self.correlation.transfer_id = Some(id.into());
    }
    pub fn set_artifact_id(&mut self, id: impl Into<String>) {
        self.correlation.artifact_id = Some(id.into());
    }

    /// Advance to `to`, which MUST be the immediate successor of the current
    /// state. `Completed` is reached by advancing from `ArtifactVerified`.
    pub fn advance(&mut self, to: CaseState) -> Result<(), TransitionError> {
        let current = match self.outcome {
            CaseOutcome::Running(s) => s,
            _ => return Err(TransitionError::AlreadyTerminal),
        };
        match current.next() {
            Some(expected) if expected == to => {
                self.outcome = if to == CaseState::Completed {
                    CaseOutcome::Completed
                } else {
                    CaseOutcome::Running(to)
                };
                Ok(())
            }
            _ => Err(TransitionError::NotSequential {
                from: current,
                requested: to,
            }),
        }
    }

    /// Latch a FAILURE at the current state. Idempotent-safe: a second fail /
    /// contaminate / advance is ignored (the first terminal reason stands).
    pub fn fail(&mut self, reason: impl Into<String>) {
        if let CaseOutcome::Running(at) = self.outcome {
            self.outcome = CaseOutcome::Failed {
                at,
                reason: reason.into(),
            };
        }
    }

    /// Latch a CONTAMINATION at the current state (an unexpected retry / resume
    /// / re-auth / transport recovery in the clean matrix).
    pub fn contaminate(&mut self, reason: impl Into<String>) {
        if let CaseOutcome::Running(at) = self.outcome {
            self.outcome = CaseOutcome::Contaminated {
                at,
                reason: reason.into(),
            };
        }
    }
}

/// Why the sequencer will not hand out another case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatrixHalt {
    /// A prior case was Failed / Contaminated. The matrix is failed; evidence
    /// is preserved; no new case starts.
    PriorCaseBad { case_id: String, reason: String },
    /// All 36 cases were handed out.
    Exhausted,
}

/// Deterministically sequences the plan's cases and enforces stop-on-first-bad.
pub struct MatrixSequencer {
    plan: RunPlan,
    next_index: usize,
    halt: Option<MatrixHalt>,
    completed: usize,
}

impl MatrixSequencer {
    pub fn new(plan: RunPlan) -> Self {
        Self {
            plan,
            next_index: 0,
            halt: None,
            completed: 0,
        }
    }

    pub fn plan(&self) -> &RunPlan {
        &self.plan
    }

    pub fn completed_count(&self) -> usize {
        self.completed
    }

    pub fn halt_reason(&self) -> Option<&MatrixHalt> {
        self.halt.as_ref()
    }

    /// The next case to run, or a [`MatrixHalt`]. Does NOT advance past a bad
    /// prior case.
    pub fn next_case(&mut self) -> Result<Option<Case>, MatrixHalt> {
        if let Some(h) = &self.halt {
            return Err(h.clone());
        }
        if self.next_index >= self.plan.cases.len() {
            self.halt = Some(MatrixHalt::Exhausted);
            return Ok(None);
        }
        let case = self.plan.cases[self.next_index].clone();
        self.next_index += 1;
        Ok(Some(case))
    }

    /// Record a case's terminal machine. A `Failed` / `Contaminated` outcome
    /// halts the sequencer; a measured/​warm-up `Completed` is counted. This is
    /// NEVER a retry point.
    pub fn record_terminal(&mut self, machine: &CaseMachine) {
        match machine.outcome() {
            CaseOutcome::Completed => self.completed += 1,
            CaseOutcome::Failed { reason, .. } | CaseOutcome::Contaminated { reason, .. } => {
                if self.halt.is_none() {
                    self.halt = Some(MatrixHalt::PriorCaseBad {
                        case_id: machine.correlation().case_id.clone(),
                        reason: reason.clone(),
                    });
                }
            }
            CaseOutcome::Running(_) => { /* not terminal; ignore */ }
        }
    }

    /// True once every planned case completed successfully and nothing halted
    /// the matrix.
    pub fn matrix_succeeded(&self) -> bool {
        self.halt
            .as_ref()
            .map(|h| matches!(h, MatrixHalt::Exhausted))
            .unwrap_or(false)
            && self.completed == self.plan.cases.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::RunPlan;

    fn plan() -> RunPlan {
        RunPlan::build("i63s3-lifecycle-test").unwrap()
    }

    fn drive_to_completed(m: &mut CaseMachine) {
        for to in [
            CaseState::ServerReady,
            CaseState::CredentialReady,
            CaseState::RunnerReady,
            CaseState::ClockChecked,
            CaseState::SourceObserved,
            CaseState::SourceValidated,
            CaseState::TransferStarted,
            CaseState::StreamCompleted,
            CaseState::SealCompleted,
            CaseState::ArtifactVerified,
            CaseState::Completed,
        ] {
            m.advance(to).unwrap();
        }
    }

    #[test]
    fn full_nominal_path_reaches_completed() {
        let p = plan();
        let mut m = CaseMachine::new(&p.cases[0]);
        drive_to_completed(&mut m);
        assert!(m.outcome().is_completed());
        assert_eq!(m.state(), None);
    }

    #[test]
    fn skipping_a_state_is_refused() {
        let p = plan();
        let mut m = CaseMachine::new(&p.cases[0]);
        m.advance(CaseState::ServerReady).unwrap();
        let err = m.advance(CaseState::RunnerReady).unwrap_err();
        assert_eq!(
            err,
            TransitionError::NotSequential {
                from: CaseState::ServerReady,
                requested: CaseState::RunnerReady,
            }
        );
    }

    #[test]
    fn going_backwards_is_refused() {
        let p = plan();
        let mut m = CaseMachine::new(&p.cases[0]);
        m.advance(CaseState::ServerReady).unwrap();
        m.advance(CaseState::CredentialReady).unwrap();
        assert!(matches!(
            m.advance(CaseState::ServerReady),
            Err(TransitionError::NotSequential { .. })
        ));
    }

    #[test]
    fn fail_latches_and_blocks_further_transitions() {
        let p = plan();
        let mut m = CaseMachine::new(&p.cases[4]);
        m.advance(CaseState::ServerReady).unwrap();
        m.advance(CaseState::CredentialReady).unwrap();
        m.fail("worker HTTPS bind failed");
        assert!(matches!(
            m.outcome(),
            CaseOutcome::Failed { at: CaseState::CredentialReady, .. }
        ));
        assert_eq!(
            m.advance(CaseState::RunnerReady),
            Err(TransitionError::AlreadyTerminal)
        );
        // a late fail/contaminate does not overwrite the first terminal reason
        m.contaminate("late");
        assert!(matches!(
            m.outcome(),
            CaseOutcome::Failed { reason, .. } if reason == "worker HTTPS bind failed"
        ));
    }

    #[test]
    fn contaminate_latches_at_current_state() {
        let p = plan();
        let mut m = CaseMachine::new(&p.cases[4]);
        for to in [
            CaseState::ServerReady,
            CaseState::CredentialReady,
            CaseState::RunnerReady,
            CaseState::ClockChecked,
            CaseState::SourceObserved,
            CaseState::SourceValidated,
            CaseState::TransferStarted,
        ] {
            m.advance(to).unwrap();
        }
        m.contaminate("unexpected resume discovery mid-stream");
        assert!(matches!(
            m.outcome(),
            CaseOutcome::Contaminated { at: CaseState::TransferStarted, .. }
        ));
    }

    #[test]
    fn sequencer_hands_out_all_36_then_reports_exhausted() {
        let mut seq = MatrixSequencer::new(plan());
        let mut n = 0;
        while let Ok(Some(case)) = seq.next_case() {
            let mut m = CaseMachine::new(&case);
            drive_to_completed(&mut m);
            seq.record_terminal(&m);
            n += 1;
        }
        assert_eq!(n, 36);
        assert_eq!(seq.completed_count(), 36);
        assert!(matches!(seq.next_case(), Err(MatrixHalt::Exhausted)));
        assert!(seq.matrix_succeeded());
    }

    #[test]
    fn sequencer_stops_after_the_first_failed_case() {
        let mut seq = MatrixSequencer::new(plan());
        // run 3 cases fine
        for _ in 0..3 {
            let case = seq.next_case().unwrap().unwrap();
            let mut m = CaseMachine::new(&case);
            drive_to_completed(&mut m);
            seq.record_terminal(&m);
        }
        // 4th case fails
        let bad = seq.next_case().unwrap().unwrap();
        let mut m = CaseMachine::new(&bad);
        m.advance(CaseState::ServerReady).unwrap();
        m.fail("digest mismatch");
        seq.record_terminal(&m);

        match seq.next_case() {
            Err(MatrixHalt::PriorCaseBad { case_id, reason }) => {
                assert_eq!(case_id, bad.case_id);
                assert_eq!(reason, "digest mismatch");
            }
            other => panic!("expected halt, got {other:?}"),
        }
        assert!(!seq.matrix_succeeded());
        assert_eq!(seq.completed_count(), 3, "no silent repeat, no retry");
    }

    #[test]
    fn sequencer_stops_after_a_contaminated_case_too() {
        let mut seq = MatrixSequencer::new(plan());
        let case = seq.next_case().unwrap().unwrap();
        let mut m = CaseMachine::new(&case);
        m.advance(CaseState::ServerReady).unwrap();
        m.contaminate("unexpected auth re-grant");
        seq.record_terminal(&m);
        assert!(matches!(
            seq.next_case(),
            Err(MatrixHalt::PriorCaseBad { .. })
        ));
    }

    #[test]
    fn correlation_is_carried_from_the_case() {
        let p = plan();
        let measured = p.measured().next().unwrap();
        let mut m = CaseMachine::new(measured);
        m.set_transfer_id("t-123");
        m.set_artifact_id("a-456");
        let c = m.correlation();
        assert_eq!(c.case_id, measured.case_id);
        assert_eq!(c.cycle, measured.cycle);
        assert_eq!(c.slot, measured.slot);
        assert_eq!(c.chunk_size_bytes, measured.chunk_size_bytes);
        assert_eq!(c.transfer_id.as_deref(), Some("t-123"));
        assert_eq!(c.artifact_id.as_deref(), Some("a-456"));
    }
}
