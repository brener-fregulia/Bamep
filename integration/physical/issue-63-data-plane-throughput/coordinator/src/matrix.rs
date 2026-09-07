//! Issue #63 Stage 2 — typed matrix coordinator. THROWAWAY Spike.
//!
//! Deterministically sequences the 36-case plan, drives a per-case typed
//! lifecycle, STOPS handing out cases after the first FAILED / CONTAMINATED
//! case, and aggregates the per-case results. All logic is composed over
//! `bamep_i63_stage2_engine`; this module adds only the lab-operation glue.
//!
//! The coordinator exposes ONLY these typed Issue-63 lab operations — never a
//! generic remote-exec facility:
//!
//!   next_case        -> the next planned [`Case`], or a halt
//!   case_ready       -> harness + runner + clock are up for the current case
//!   case_started     -> source observed + validated, transfer stream begun
//!   case_completed   -> stream + seal + Artifact::Verified; carries the result
//!   case_failed      -> the current case failed / was contaminated
//!   matrix_completed -> implicit once every planned case has completed
//!
//! Nothing here opens a socket, runs a transfer, or reads a device. The Stage-3
//! networked wiring (real per-case harness + WinPE probe) is NOT built here and
//! NOT armed.

use bamep_i63_stage2_engine::analysis::{analyse, Analysis};
use bamep_i63_stage2_engine::budget::{evaluate_budget, BudgetInputs, BudgetVerdict};
use bamep_i63_stage2_engine::lifecycle::{
    CaseMachine, CaseState, MatrixHalt, MatrixSequencer,
};
use bamep_i63_stage2_engine::matrix::{ArithmeticError, Case, RunPlan};
use bamep_i63_stage2_engine::result::CaseResult;

/// A typed lab request from the lab runner.
#[derive(Debug, Clone)]
pub enum LabRequest {
    NextCase,
    CaseReady { case_id: String },
    CaseStarted { case_id: String },
    CaseCompleted { case_id: String, result: Box<CaseResult> },
    CaseFailed { case_id: String, reason: String, contaminated: bool },
}

/// The coordinator's typed response.
#[derive(Debug)]
pub enum LabResponse {
    Case(Box<Case>),
    Ack,
    MatrixCompleted { completed: usize, analysis: Box<Analysis> },
    MatrixHalted(MatrixHalt),
    /// The request did not match the current case / lifecycle position.
    Rejected(String),
}

/// Why the coordinator refused to start.
#[derive(Debug)]
pub enum MatrixStartError {
    Plan(ArithmeticError),
    Budget(BudgetVerdict),
}

pub struct MatrixCoordinator {
    seq: MatrixSequencer,
    current: Option<CaseMachine>,
    results: Vec<CaseResult>,
    budget: BudgetVerdict,
}

impl MatrixCoordinator {
    /// Build the coordinator for `run_id`. Fails closed if the plan arithmetic
    /// is invalid or the observed free space under the Worker storage root is
    /// below the Stage-3 gate.
    pub fn new(run_id: &str, observed_free_bytes: u64) -> Result<Self, MatrixStartError> {
        let plan = RunPlan::build(run_id).map_err(MatrixStartError::Plan)?;
        let budget = evaluate_budget(&BudgetInputs::stage3(observed_free_bytes));
        if !budget.is_ok() {
            return Err(MatrixStartError::Budget(budget));
        }
        Ok(Self {
            seq: MatrixSequencer::new(plan),
            current: None,
            results: Vec::new(),
            budget,
        })
    }

    pub fn budget(&self) -> &BudgetVerdict {
        &self.budget
    }

    pub fn results(&self) -> &[CaseResult] {
        &self.results
    }

    pub fn plan_len(&self) -> usize {
        self.seq.plan().cases.len()
    }

    /// The full deterministic 36-case plan, in run order (for the Stage-3 run
    /// directory's `matrix-plan.json`).
    pub fn plan_cases(&self) -> &[Case] {
        &self.seq.plan().cases
    }

    fn current_case_id(&self) -> Option<&str> {
        self.current.as_ref().map(|m| m.correlation().case_id.as_str())
    }

    /// Advance the current [`CaseMachine`] through a run of nominal states,
    /// stopping at `target` (inclusive).
    fn advance_to(m: &mut CaseMachine, targets: &[CaseState]) -> Result<(), String> {
        for &t in targets {
            m.advance(t).map_err(|e| format!("{e:?}"))?;
        }
        Ok(())
    }

    pub fn handle(&mut self, req: LabRequest) -> LabResponse {
        match req {
            LabRequest::NextCase => {
                if self.current.is_some() {
                    return LabResponse::Rejected(
                        "a case is already in flight; complete or fail it first".into(),
                    );
                }
                match self.seq.next_case() {
                    Ok(Some(case)) => {
                        self.current = Some(CaseMachine::new(&case));
                        LabResponse::Case(Box::new(case))
                    }
                    Ok(None) => {
                        // Exhausted: matrix complete iff every case completed.
                        let analysis = analyse(&self.results);
                        LabResponse::MatrixCompleted {
                            completed: self.seq.completed_count(),
                            analysis: Box::new(analysis),
                        }
                    }
                    Err(halt) => LabResponse::MatrixHalted(halt),
                }
            }

            LabRequest::CaseReady { case_id } => self.on_case(&case_id, |m| {
                Self::advance_to(
                    m,
                    &[
                        CaseState::ServerReady,
                        CaseState::CredentialReady,
                        CaseState::RunnerReady,
                        CaseState::ClockChecked,
                    ],
                )
            }),

            LabRequest::CaseStarted { case_id } => self.on_case(&case_id, |m| {
                Self::advance_to(
                    m,
                    &[
                        CaseState::SourceObserved,
                        CaseState::SourceValidated,
                        CaseState::TransferStarted,
                    ],
                )
            }),

            LabRequest::CaseCompleted { case_id, result } => {
                if self.current_case_id() != Some(case_id.as_str()) {
                    return LabResponse::Rejected(format!("no in-flight case {case_id}"));
                }
                if result.case_id != case_id {
                    return LabResponse::Rejected("result.case_id mismatch".into());
                }
                if !result.is_completed_and_verified() {
                    // A "completed" that is not Verified is a failure, not success.
                    return self.fail_current(
                        &case_id,
                        &format!(
                            "case_completed but not Verified: status={} artifact={}",
                            result.case_status, result.final_artifact_status
                        ),
                        false,
                    );
                }
                let mut m = self.current.take().unwrap();
                if let Some(tid) = &result.transfer_id {
                    m.set_transfer_id(tid.clone());
                }
                if let Some(aid) = &result.artifact_id {
                    m.set_artifact_id(aid.clone());
                }
                if let Err(e) = Self::advance_to(
                    &mut m,
                    &[
                        CaseState::StreamCompleted,
                        CaseState::SealCompleted,
                        CaseState::ArtifactVerified,
                        CaseState::Completed,
                    ],
                ) {
                    m.fail(format!("lifecycle: {e}"));
                    self.seq.record_terminal(&m);
                    return LabResponse::MatrixHalted(
                        self.seq.halt_reason().cloned().unwrap_or(MatrixHalt::Exhausted),
                    );
                }
                self.seq.record_terminal(&m);
                self.results.push(*result);
                LabResponse::Ack
            }

            LabRequest::CaseFailed {
                case_id,
                reason,
                contaminated,
            } => self.fail_current(&case_id, &reason, contaminated),
        }
    }

    fn on_case(
        &mut self,
        case_id: &str,
        f: impl FnOnce(&mut CaseMachine) -> Result<(), String>,
    ) -> LabResponse {
        if self.current_case_id() != Some(case_id) {
            return LabResponse::Rejected(format!("no in-flight case {case_id}"));
        }
        let m = self.current.as_mut().unwrap();
        match f(m) {
            Ok(()) => LabResponse::Ack,
            Err(e) => {
                m.fail(format!("lifecycle transition rejected: {e}"));
                let m = self.current.take().unwrap();
                self.seq.record_terminal(&m);
                LabResponse::MatrixHalted(
                    self.seq.halt_reason().cloned().unwrap_or(MatrixHalt::Exhausted),
                )
            }
        }
    }

    fn fail_current(&mut self, case_id: &str, reason: &str, contaminated: bool) -> LabResponse {
        if self.current_case_id() != Some(case_id) {
            return LabResponse::Rejected(format!("no in-flight case {case_id}"));
        }
        let mut m = self.current.take().unwrap();
        if contaminated {
            m.contaminate(reason.to_string());
        } else {
            m.fail(reason.to_string());
        }
        self.seq.record_terminal(&m);
        LabResponse::MatrixHalted(
            self.seq
                .halt_reason()
                .cloned()
                .unwrap_or(MatrixHalt::Exhausted),
        )
    }

    /// Did every planned case complete successfully?
    pub fn matrix_succeeded(&self) -> bool {
        self.seq.matrix_succeeded()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bamep_i63_stage2_engine::budget::PAYLOAD_BYTES;
    use bamep_i63_stage2_engine::matrix::EXTENT_BYTES;
    use bamep_i63_stage2_engine::result::ConnectionCount;
    use bamep_i63_stage2_engine::GIB;

    fn ok_result(case: &Case) -> CaseResult {
        let (b_mib, b_mb) = CaseResult::rates(case.extent_bytes, 60_000.0);
        let (v_mib, v_mb) = CaseResult::rates(case.extent_bytes, 66_000.0);
        CaseResult {
            run_id: case.run_id.clone(),
            case_id: case.case_id.clone(),
            phase: case.phase,
            cycle: case.cycle,
            slot: case.slot,
            chunk_size_bytes: case.chunk_size_bytes,
            extent_bytes: case.extent_bytes,
            chunk_count: case.expected_chunk_count,
            transfer_id: Some("t".into()),
            artifact_id: Some("a".into()),
            source_safety_verdict: "accept".into(),
            clock_skew_verdict: "in_bound".into(),
            bulk_stream_wall_ms: 60_000.0,
            bulk_stream_mib_s: b_mib,
            bulk_stream_mb_s: b_mb,
            verified_transfer_wall_ms: 66_000.0,
            verified_transfer_mib_s: v_mib,
            verified_transfer_mb_s: v_mb,
            resume_ms: 3.0,
            seal_d2_ms: 5_000.0,
            read_ms: 1.0,
            chunk_sha_ms: 1.0,
            rolling_sha_ms: 1.0,
            proof_ms: 1.0,
            put_ack_ms: 1.0,
            connection_count: ConnectionCount::by_construction(case.expected_chunk_count),
            final_artifact_status: "Verified".into(),
            case_status: "completed".into(),
        }
    }

    fn run_case(c: &mut MatrixCoordinator, case: &Case) {
        assert!(matches!(
            c.handle(LabRequest::CaseReady { case_id: case.case_id.clone() }),
            LabResponse::Ack
        ));
        assert!(matches!(
            c.handle(LabRequest::CaseStarted { case_id: case.case_id.clone() }),
            LabResponse::Ack
        ));
        assert!(matches!(
            c.handle(LabRequest::CaseCompleted {
                case_id: case.case_id.clone(),
                result: Box::new(ok_result(case)),
            }),
            LabResponse::Ack
        ));
    }

    #[test]
    fn budget_below_gate_fails_closed_at_construction() {
        match MatrixCoordinator::new("r", 50 * GIB) {
            Err(MatrixStartError::Budget(_)) => {}
            _ => panic!("expected a fail-closed budget rejection"),
        }
    }

    #[test]
    fn full_36_case_walkthrough_reaches_matrix_completed_with_analysis() {
        let mut c = MatrixCoordinator::new("i63s3-x", 120 * GIB).unwrap();
        assert!(c.budget().is_ok());
        let mut n = 0;
        loop {
            match c.handle(LabRequest::NextCase) {
                LabResponse::Case(case) => {
                    run_case(&mut c, &case);
                    n += 1;
                }
                LabResponse::MatrixCompleted { completed, analysis } => {
                    assert_eq!(completed, 36);
                    // 32 measured cases across 4 sizes, warm-ups excluded.
                    assert_eq!(analysis.sizes.iter().map(|s| s.n).sum::<usize>(), 32);
                    assert!(analysis.excluded_unverified.is_empty());
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(n, 36);
        assert!(c.matrix_succeeded());
        assert_eq!(PAYLOAD_BYTES, 36 * EXTENT_BYTES);
    }

    #[test]
    fn a_failed_case_halts_the_matrix_and_no_more_cases_are_handed_out() {
        let mut c = MatrixCoordinator::new("i63s3-y", 100 * GIB).unwrap();
        // 3 good cases
        for _ in 0..3 {
            let LabResponse::Case(case) = c.handle(LabRequest::NextCase) else {
                panic!()
            };
            run_case(&mut c, &case);
        }
        // 4th fails
        let LabResponse::Case(bad) = c.handle(LabRequest::NextCase) else {
            panic!()
        };
        c.handle(LabRequest::CaseReady { case_id: bad.case_id.clone() });
        let resp = c.handle(LabRequest::CaseFailed {
            case_id: bad.case_id.clone(),
            reason: "digest mismatch".into(),
            contaminated: false,
        });
        assert!(matches!(resp, LabResponse::MatrixHalted(MatrixHalt::PriorCaseBad { .. })));
        // no more cases
        assert!(matches!(
            c.handle(LabRequest::NextCase),
            LabResponse::MatrixHalted(MatrixHalt::PriorCaseBad { .. })
        ));
        assert!(!c.matrix_succeeded());
    }

    #[test]
    fn a_contaminated_case_also_halts_the_matrix() {
        let mut c = MatrixCoordinator::new("i63s3-z", 100 * GIB).unwrap();
        let LabResponse::Case(case) = c.handle(LabRequest::NextCase) else {
            panic!()
        };
        c.handle(LabRequest::CaseReady { case_id: case.case_id.clone() });
        c.handle(LabRequest::CaseStarted { case_id: case.case_id.clone() });
        let resp = c.handle(LabRequest::CaseFailed {
            case_id: case.case_id.clone(),
            reason: "unexpected resume mid-stream".into(),
            contaminated: true,
        });
        assert!(matches!(resp, LabResponse::MatrixHalted(_)));
        assert!(matches!(
            c.handle(LabRequest::NextCase),
            LabResponse::MatrixHalted(_)
        ));
    }

    #[test]
    fn case_completed_but_not_verified_is_treated_as_a_failure() {
        let mut c = MatrixCoordinator::new("i63s3-nv", 100 * GIB).unwrap();
        let LabResponse::Case(case) = c.handle(LabRequest::NextCase) else {
            panic!()
        };
        c.handle(LabRequest::CaseReady { case_id: case.case_id.clone() });
        c.handle(LabRequest::CaseStarted { case_id: case.case_id.clone() });
        let mut bad = ok_result(&case);
        bad.final_artifact_status = "Failed".into();
        let resp = c.handle(LabRequest::CaseCompleted {
            case_id: case.case_id.clone(),
            result: Box::new(bad),
        });
        assert!(matches!(resp, LabResponse::MatrixHalted(_)));
    }

    #[test]
    fn next_case_is_rejected_while_a_case_is_in_flight() {
        let mut c = MatrixCoordinator::new("i63s3-q", 100 * GIB).unwrap();
        let LabResponse::Case(_) = c.handle(LabRequest::NextCase) else {
            panic!()
        };
        assert!(matches!(
            c.handle(LabRequest::NextCase),
            LabResponse::Rejected(_)
        ));
    }

    #[test]
    fn ordering_is_deterministic_across_two_coordinators() {
        let mut a = MatrixCoordinator::new("same", 100 * GIB).unwrap();
        let mut b = MatrixCoordinator::new("same", 100 * GIB).unwrap();
        for _ in 0..36 {
            let (LabResponse::Case(ca), LabResponse::Case(cb)) =
                (a.handle(LabRequest::NextCase), b.handle(LabRequest::NextCase))
            else {
                panic!()
            };
            assert_eq!(ca.case_id, cb.case_id);
            run_case(&mut a, &ca);
            run_case(&mut b, &cb);
        }
    }
}
