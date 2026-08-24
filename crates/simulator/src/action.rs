//! Simulated Agent-local typed-action state machine for the single M1
//! concrete action, `bamep.m1.simulated-execution`
//! (`docs/specifications/m1-simulated-vertical-slice-and-baseline-validation.md`
//! RF-004; Issue #26 "Simulator Agent action state").
//!
//! Pure/synchronous decision logic only: [`SimulatedActionAgent::handle_dispatch`]
//! never performs I/O — it returns the ordered list of Agent Protocol
//! messages to send for one `ActionDispatch`, and the caller (a real WSS
//! session loop, e.g. `run_action_dispatch_loop`) is responsible for actually
//! writing them to the socket. This keeps the scenario logic itself
//! deterministic and independently unit-testable, while still requiring the
//! Simulator-level fidelity boundary
//! (`m0-simulator-contract-and-validation-strategy.md`) for any claim that
//! crosses the real transport.
//!
//! Agent restart / loss of this local state belongs to Issue #28; this
//! module never persists it.

use std::collections::HashMap;
use std::sync::Mutex;

use bamep_agent_protocol::{
    ActionAckError, ActionAckMessage, ActionDispatchMessage, ActionProgressMessage,
    ActionResultMessage, ActionResultOutcome, AgentProtocolMessage, Percent, ProtocolId,
};
use serde_json::{Map, Value};

/// The single M1 Simulator-only concrete typed action
/// (`m1-simulated-vertical-slice-and-baseline-validation.md` RF-004). Kept in
/// sync with `bamep_server::application::M1_SIMULATED_EXECUTION_ACTION_TYPE`
/// by the wire contract itself (ADR-0003 "wire-contract independence"), not
/// by shared Rust code — the Simulator crate never depends on `bamep-server`.
pub const M1_ACTION_TYPE: &str = "bamep.m1.simulated-execution";
pub const M1_ACTION_VERSION: &str = "1";

/// Deterministic Simulator scenario configuration controlling the M1
/// action's outcome. Configured per `action_id` before the corresponding
/// `ActionDispatch` is expected, or falls back to a per-agent default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScenarioOutcome {
    AcceptThenSucceed,
    AcceptThenFail,
    Reject,
}

/// The exact dispatch content a recorded `action_id` was first bound to —
/// compared against a duplicate's content to distinguish a genuine retained-
/// evidence duplicate from conflicting dispatch content
/// (`m1-simulated-vertical-slice-and-baseline-validation.md`; Issue #26 "same
/// `action_id` WITH CONFLICTING dispatch content").
#[derive(Debug, Clone, PartialEq, Eq)]
struct DispatchContent {
    action_type: String,
    action_version: String,
    parameters: Map<String, Value>,
}

impl DispatchContent {
    fn from_dispatch(dispatch: &ActionDispatchMessage) -> Self {
        Self {
            action_type: dispatch.body.action_type.clone(),
            action_version: dispatch.body.action_version.clone(),
            parameters: dispatch.body.parameters.clone(),
        }
    }
}

/// One Agent-local record of a known `action_id`
/// (`m0-agent-protocol-contract.md` "Idempotency, retry, and uncertain
/// delivery"). Retains exactly the evidence a duplicate dispatch re-emits,
/// under a fresh `message_id`, without re-executing.
#[derive(Debug, Clone)]
enum LocalActionState {
    Active {
        content: DispatchContent,
        ack: ActionAckMessage,
    },
    Completed {
        content: DispatchContent,
        result: ActionResultMessage,
    },
    Rejected {
        content: DispatchContent,
        ack: ActionAckMessage,
    },
}

impl LocalActionState {
    fn content(&self) -> &DispatchContent {
        match self {
            LocalActionState::Active { content, .. }
            | LocalActionState::Completed { content, .. }
            | LocalActionState::Rejected { content, .. } => content,
        }
    }
}

/// The Simulated Agent's own typed-action participant: local `action_id`
/// state plus deterministic scenario configuration. `Send + Sync` so one
/// instance can be shared (`Arc`) with a WSS session-driving task.
#[derive(Default)]
pub struct SimulatedActionAgent {
    scenarios: Mutex<HashMap<ProtocolId, ScenarioOutcome>>,
    default_scenario: Mutex<Option<ScenarioOutcome>>,
    state: Mutex<HashMap<ProtocolId, LocalActionState>>,
}

impl SimulatedActionAgent {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the scenario used for any `action_id` not individually
    /// configured via [`Self::configure`].
    pub fn with_default_scenario(self, scenario: ScenarioOutcome) -> Self {
        *self.default_scenario.lock().unwrap() = Some(scenario);
        self
    }

    /// Configures the deterministic scenario for one specific `action_id`,
    /// overriding the default for it alone.
    pub fn configure(&self, action_id: ProtocolId, scenario: ScenarioOutcome) {
        self.scenarios.lock().unwrap().insert(action_id, scenario);
    }

    /// Decides the response to one `ActionDispatch`: exactly the
    /// `ActionAck` — `Accepted`, or a terminal `Rejected`. Records the
    /// action's local state *before* returning — synchronously, in this
    /// same call, and strictly before [`Self::run_configured_scenario`] is
    /// ever invoked for it — so a duplicate dispatch arriving before
    /// execution completes always observes the `Active` (or `Rejected`)
    /// record rather than racing it
    /// (`m1-simulated-vertical-slice-and-baseline-validation.md`; Issue #26
    /// "Simulator Agent action state": "record action locally BEFORE
    /// starting simulated execution").
    ///
    /// A duplicate `action_id` never re-executes: an `Active` or `Rejected`
    /// duplicate re-emits the retained `ActionAck` (fresh `message_id`); a
    /// `Completed` duplicate instead re-emits the retained `ActionResult`
    /// (also handled here, since a duplicate dispatch never distinguishes
    /// "still active" from "already completed" from the caller's
    /// perspective — both are answered from this one method). Conflicting
    /// content for an already-known `action_id` is rejected and never
    /// replaces the original record.
    pub fn handle_dispatch(&self, dispatch: &ActionDispatchMessage) -> Vec<AgentProtocolMessage> {
        let action_id = dispatch.body.action_id;
        let content = DispatchContent::from_dispatch(dispatch);
        let mut state = self.state.lock().unwrap();

        if let Some(existing) = state.get(&action_id) {
            if existing.content() != &content {
                // Same action_id, conflicting content: reject, never
                // execute, never replace the existing record.
                let error = ActionAckError::new("INVALID_PARAMETERS")
                    .with_message("action_id already bound to different dispatch content");
                return vec![AgentProtocolMessage::ActionAck(ActionAckMessage::rejected(
                    action_id, error,
                ))];
            }
            return match existing {
                LocalActionState::Active { ack, .. } | LocalActionState::Rejected { ack, .. } => {
                    vec![AgentProtocolMessage::ActionAck(
                        ack.clone().with_fresh_message_id(),
                    )]
                }
                LocalActionState::Completed { result, .. } => {
                    vec![AgentProtocolMessage::ActionResult(
                        result.clone().with_fresh_message_id(),
                    )]
                }
            };
        }

        // First valid dispatch for this action_id: validate action_type/
        // version/closed-empty-parameters before recording or executing
        // anything (`m1-...md` "ActionAck{Rejected}.error.code").
        if content.action_type != M1_ACTION_TYPE {
            return self.reject_and_record(&mut state, action_id, content, "UNSUPPORTED_ACTION");
        }
        if content.action_version != M1_ACTION_VERSION {
            return self.reject_and_record(
                &mut state,
                action_id,
                content,
                "UNSUPPORTED_ACTION_VERSION",
            );
        }
        if !content.parameters.is_empty() {
            return self.reject_and_record(&mut state, action_id, content, "INVALID_PARAMETERS");
        }

        let scenario = self
            .scenarios
            .lock()
            .unwrap()
            .get(&action_id)
            .copied()
            .or(*self.default_scenario.lock().unwrap())
            .expect("a scenario must be configured (per-action or default) before dispatch");

        if scenario == ScenarioOutcome::Reject {
            return self.reject_and_record(&mut state, action_id, content, "ACTION_NOT_AVAILABLE");
        }

        let ack = ActionAckMessage::accepted(action_id);
        state.insert(
            action_id,
            LocalActionState::Active {
                content,
                ack: ack.clone(),
            },
        );
        vec![AgentProtocolMessage::ActionAck(ack)]
    }

    /// Executes the configured scenario for an `action_id` currently
    /// `Active` (i.e. `handle_dispatch` already returned `ActionAck{Accepted}`
    /// for it): the deterministic `ActionProgress` ticks followed by the
    /// terminal `ActionResult`, and transitions the local record to
    /// `Completed`, retaining that exact `ActionResult` for any later
    /// duplicate dispatch. Returns `None` for an unknown `action_id` or one
    /// not currently `Active` (already completed, rejected, or never
    /// dispatched) — the caller (the real WSS session driver) is expected to
    /// call this exactly once, immediately after observing `Accepted` from
    /// `handle_dispatch`.
    pub fn run_configured_scenario(
        &self,
        action_id: ProtocolId,
    ) -> Option<Vec<AgentProtocolMessage>> {
        let content = {
            let state = self.state.lock().unwrap();
            match state.get(&action_id) {
                Some(LocalActionState::Active { content, .. }) => content.clone(),
                _ => return None,
            }
        };
        let scenario = self
            .scenarios
            .lock()
            .unwrap()
            .get(&action_id)
            .copied()
            .or(*self.default_scenario.lock().unwrap())
            .expect("Active implies a scenario was already resolved once");

        let (percents, outcome, code): (&[u8], ActionResultOutcome, &str) = match scenario {
            ScenarioOutcome::AcceptThenSucceed => (
                &[0, 50, 100],
                ActionResultOutcome::Succeeded,
                "SIMULATED_COMPLETION",
            ),
            ScenarioOutcome::AcceptThenFail => {
                (&[0, 50], ActionResultOutcome::Failed, "SIMULATED_FAILURE")
            }
            ScenarioOutcome::Reject => unreachable!("Active is never reached for Reject"),
        };

        let mut messages = Vec::with_capacity(percents.len() + 1);
        for &percent in percents {
            messages.push(AgentProtocolMessage::ActionProgress(
                ActionProgressMessage::percent(action_id, Percent::new(percent).unwrap()),
            ));
        }
        let detail = serde_json::json!({"code": code})
            .as_object()
            .unwrap()
            .clone();
        let result = ActionResultMessage::new(action_id, outcome, detail);
        self.state.lock().unwrap().insert(
            action_id,
            LocalActionState::Completed {
                content,
                result: result.clone(),
            },
        );
        messages.push(AgentProtocolMessage::ActionResult(result));
        Some(messages)
    }

    fn reject_and_record(
        &self,
        state: &mut HashMap<ProtocolId, LocalActionState>,
        action_id: ProtocolId,
        content: DispatchContent,
        code: &'static str,
    ) -> Vec<AgentProtocolMessage> {
        let ack = ActionAckMessage::rejected(action_id, ActionAckError::new(code));
        state.insert(
            action_id,
            LocalActionState::Rejected {
                content,
                ack: ack.clone(),
            },
        );
        vec![AgentProtocolMessage::ActionAck(ack)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatch(action_id: ProtocolId) -> ActionDispatchMessage {
        ActionDispatchMessage::new(action_id, M1_ACTION_TYPE, M1_ACTION_VERSION, Map::new())
    }

    fn outcomes(messages: &[AgentProtocolMessage]) -> Vec<&'static str> {
        messages
            .iter()
            .map(|m| match m {
                AgentProtocolMessage::ActionAck(a) => match a.body.outcome {
                    bamep_agent_protocol::ActionAckOutcome::Accepted => "Ack:Accepted",
                    bamep_agent_protocol::ActionAckOutcome::Rejected => "Ack:Rejected",
                },
                AgentProtocolMessage::ActionProgress(_) => "Progress",
                AgentProtocolMessage::ActionResult(r) => match r.body.outcome {
                    ActionResultOutcome::Succeeded => "Result:Succeeded",
                    ActionResultOutcome::Failed => "Result:Failed",
                    ActionResultOutcome::Cancelled => "Result:Cancelled",
                },
                _ => "Other",
            })
            .collect()
    }

    #[test]
    fn accept_then_succeed_progresses_0_50_100_then_succeeded() {
        let agent =
            SimulatedActionAgent::new().with_default_scenario(ScenarioOutcome::AcceptThenSucceed);
        let action_id = ProtocolId::generate();
        let ack = agent.handle_dispatch(&dispatch(action_id));
        assert_eq!(outcomes(&ack), vec!["Ack:Accepted"]);

        let rest = agent.run_configured_scenario(action_id).unwrap();
        assert_eq!(
            outcomes(&rest),
            vec!["Progress", "Progress", "Progress", "Result:Succeeded"]
        );
    }

    #[test]
    fn accept_then_fail_progresses_0_50_then_failed() {
        let agent =
            SimulatedActionAgent::new().with_default_scenario(ScenarioOutcome::AcceptThenFail);
        let action_id = ProtocolId::generate();
        let ack = agent.handle_dispatch(&dispatch(action_id));
        assert_eq!(outcomes(&ack), vec!["Ack:Accepted"]);

        let rest = agent.run_configured_scenario(action_id).unwrap();
        assert_eq!(
            outcomes(&rest),
            vec!["Progress", "Progress", "Result:Failed"]
        );
    }

    #[test]
    fn reject_scenario_never_executes() {
        let agent = SimulatedActionAgent::new().with_default_scenario(ScenarioOutcome::Reject);
        let action_id = ProtocolId::generate();
        let messages = agent.handle_dispatch(&dispatch(action_id));
        assert_eq!(outcomes(&messages), vec!["Ack:Rejected"]);
        assert!(
            agent.run_configured_scenario(action_id).is_none(),
            "a Rejected action_id is never Active, so there is nothing to execute"
        );
    }

    #[test]
    fn per_action_configuration_overrides_the_default() {
        let agent =
            SimulatedActionAgent::new().with_default_scenario(ScenarioOutcome::AcceptThenSucceed);
        let action_id = ProtocolId::generate();
        agent.configure(action_id, ScenarioOutcome::Reject);
        let messages = agent.handle_dispatch(&dispatch(action_id));
        assert_eq!(outcomes(&messages), vec!["Ack:Rejected"]);
    }

    #[test]
    fn duplicate_active_dispatch_never_executes_again_and_reuses_message_id() {
        let agent =
            SimulatedActionAgent::new().with_default_scenario(ScenarioOutcome::AcceptThenSucceed);
        let action_id = ProtocolId::generate();

        let first_ack = agent.handle_dispatch(&dispatch(action_id));
        assert_eq!(outcomes(&first_ack), vec!["Ack:Accepted"]);

        // A duplicate dispatch arrives while still Active — before
        // `run_configured_scenario` executes anything.
        let duplicate_ack = agent.handle_dispatch(&dispatch(action_id));
        assert_eq!(outcomes(&duplicate_ack), vec!["Ack:Accepted"]);
        let AgentProtocolMessage::ActionAck(first) = &first_ack[0] else {
            unreachable!()
        };
        let AgentProtocolMessage::ActionAck(duplicate) = &duplicate_ack[0] else {
            unreachable!()
        };
        assert_ne!(
            first.envelope.message_id, duplicate.envelope.message_id,
            "a re-emitted duplicate must get a fresh message_id"
        );

        // Execution still happens exactly once.
        let rest = agent.run_configured_scenario(action_id).unwrap();
        assert_eq!(
            outcomes(&rest),
            vec!["Progress", "Progress", "Progress", "Result:Succeeded"]
        );
        assert!(
            agent.run_configured_scenario(action_id).is_none(),
            "the action is no longer Active after completion, so a second run is a no-op"
        );
    }

    #[test]
    fn duplicate_completed_dispatch_re_emits_the_retained_result_without_re_executing() {
        let agent =
            SimulatedActionAgent::new().with_default_scenario(ScenarioOutcome::AcceptThenSucceed);
        let action_id = ProtocolId::generate();
        agent.handle_dispatch(&dispatch(action_id));
        let first_result = agent.run_configured_scenario(action_id).unwrap();
        let AgentProtocolMessage::ActionResult(first_result) = first_result.last().unwrap() else {
            unreachable!()
        };

        let duplicate = agent.handle_dispatch(&dispatch(action_id));
        assert_eq!(outcomes(&duplicate), vec!["Result:Succeeded"]);
        let AgentProtocolMessage::ActionResult(second_result) = &duplicate[0] else {
            unreachable!()
        };
        assert_ne!(
            first_result.envelope.message_id, second_result.envelope.message_id,
            "a re-emitted duplicate must get a fresh message_id"
        );
        assert_eq!(first_result.body.detail, second_result.body.detail);
    }

    #[test]
    fn duplicate_rejected_dispatch_re_emits_the_retained_rejection() {
        let agent = SimulatedActionAgent::new().with_default_scenario(ScenarioOutcome::Reject);
        let action_id = ProtocolId::generate();

        let first = agent.handle_dispatch(&dispatch(action_id));
        let second = agent.handle_dispatch(&dispatch(action_id));
        assert_eq!(outcomes(&first), vec!["Ack:Rejected"]);
        assert_eq!(outcomes(&second), vec!["Ack:Rejected"]);

        let AgentProtocolMessage::ActionAck(first_ack) = &first[0] else {
            unreachable!()
        };
        let AgentProtocolMessage::ActionAck(second_ack) = &second[0] else {
            unreachable!()
        };
        assert_ne!(
            first_ack.envelope.message_id,
            second_ack.envelope.message_id
        );
        assert_eq!(first_ack.body.error, second_ack.body.error);
    }

    #[test]
    fn same_action_id_with_conflicting_content_is_rejected_and_never_replaces_the_original() {
        let agent =
            SimulatedActionAgent::new().with_default_scenario(ScenarioOutcome::AcceptThenSucceed);
        let action_id = ProtocolId::generate();

        agent.handle_dispatch(&dispatch(action_id));
        agent.run_configured_scenario(action_id).unwrap();

        let conflicting =
            ActionDispatchMessage::new(action_id, "bamep.m1.some-other-action", "1", Map::new());
        let second = agent.handle_dispatch(&conflicting);
        assert_eq!(outcomes(&second), vec!["Ack:Rejected"]);

        // The original retained Completed evidence must be unaffected: a
        // matching duplicate of the ORIGINAL content still re-emits it.
        let third = agent.handle_dispatch(&dispatch(action_id));
        assert_eq!(outcomes(&third), vec!["Result:Succeeded"]);
    }

    #[test]
    fn unsupported_action_type_is_rejected_with_the_closed_diagnostic_code() {
        let agent =
            SimulatedActionAgent::new().with_default_scenario(ScenarioOutcome::AcceptThenSucceed);
        let action_id = ProtocolId::generate();
        let bad = ActionDispatchMessage::new(action_id, "bamep.m1.other", "1", Map::new());
        let messages = agent.handle_dispatch(&bad);
        let AgentProtocolMessage::ActionAck(ack) = &messages[0] else {
            unreachable!()
        };
        assert_eq!(ack.body.error.as_ref().unwrap().code, "UNSUPPORTED_ACTION");
    }

    #[test]
    fn unsupported_action_version_is_rejected_with_the_closed_diagnostic_code() {
        let agent =
            SimulatedActionAgent::new().with_default_scenario(ScenarioOutcome::AcceptThenSucceed);
        let action_id = ProtocolId::generate();
        let bad = ActionDispatchMessage::new(action_id, M1_ACTION_TYPE, "2", Map::new());
        let messages = agent.handle_dispatch(&bad);
        let AgentProtocolMessage::ActionAck(ack) = &messages[0] else {
            unreachable!()
        };
        assert_eq!(
            ack.body.error.as_ref().unwrap().code,
            "UNSUPPORTED_ACTION_VERSION"
        );
    }

    #[test]
    fn non_empty_parameters_are_rejected_with_the_closed_diagnostic_code() {
        let agent =
            SimulatedActionAgent::new().with_default_scenario(ScenarioOutcome::AcceptThenSucceed);
        let action_id = ProtocolId::generate();
        let mut params = Map::new();
        params.insert("unexpected".to_string(), Value::Bool(true));
        let bad = ActionDispatchMessage::new(action_id, M1_ACTION_TYPE, M1_ACTION_VERSION, params);
        let messages = agent.handle_dispatch(&bad);
        let AgentProtocolMessage::ActionAck(ack) = &messages[0] else {
            unreachable!()
        };
        assert_eq!(ack.body.error.as_ref().unwrap().code, "INVALID_PARAMETERS");
    }
}
