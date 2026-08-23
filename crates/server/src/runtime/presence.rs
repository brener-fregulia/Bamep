//! Runtime Presence Registry (`m0-stack-and-boundaries-baseline.md`
//! "Component responsibilities and boundaries" — Runtime Services: Runtime
//! Presence Registry; `m0-endpoint-identity-lifecycle.md` "Destructive-
//! operation authorization preconditions" precondition 2): tracks which
//! Endpoints currently have at least one authenticated Agent Protocol session
//! alive.
//!
//! Memory-only — never persisted to PostgreSQL
//! (`m0-persistence-observability-and-domain-events.md` "Durable versus
//! transient state": "Agent connection/presence" is transient by default). A
//! fresh registry always starts empty; durable `CredentialActive` state after
//! restart is never used to reconstruct/fabricate presence
//! (`m0-endpoint-identity-lifecycle.md` "Credential/session validity": "Agent
//! presence is independent from credential validity").

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use bamep_agent_protocol::ProtocolId;
use bamep_domain::EndpointId;

/// Currently authenticated sessions, keyed by Endpoint. One Endpoint may have
/// multiple simultaneous authenticated `SessionId`s
/// (`bamep_server::adapters::agent_gateway::AuthenticatedSession`); an
/// Endpoint is present while its session set is non-empty and becomes absent
/// only once the *last* registered session is removed — never merely because
/// any single session closed.
#[derive(Default)]
pub struct PresenceRegistry {
    sessions: Mutex<HashMap<EndpointId, HashSet<ProtocolId>>>,
}

impl PresenceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `session_id` as a currently authenticated session for
    /// `endpoint_id`. Idempotent: registering the same session twice has no
    /// additional effect.
    pub fn register(&self, endpoint_id: EndpointId, session_id: ProtocolId) {
        let mut sessions = self
            .sessions
            .lock()
            .expect("presence registry lock poisoned");
        sessions.entry(endpoint_id).or_default().insert(session_id);
    }

    /// Removes exactly `session_id` for `endpoint_id`. Idempotent: removing
    /// an already-absent session, or a session for an Endpoint with no
    /// registered sessions, is a no-op. Only removal of the last registered
    /// session for an Endpoint makes it absent.
    pub fn unregister(&self, endpoint_id: EndpointId, session_id: ProtocolId) {
        let mut sessions = self
            .sessions
            .lock()
            .expect("presence registry lock poisoned");
        if let Entry::Occupied(mut entry) = sessions.entry(endpoint_id) {
            entry.get_mut().remove(&session_id);
            if entry.get().is_empty() {
                entry.remove();
            }
        }
    }

    /// True while at least one authenticated session remains registered for
    /// `endpoint_id`.
    pub fn is_present(&self, endpoint_id: EndpointId) -> bool {
        let sessions = self
            .sessions
            .lock()
            .expect("presence registry lock poisoned");
        sessions
            .get(&endpoint_id)
            .is_some_and(|set| !set.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> ProtocolId {
        ProtocolId::generate()
    }

    #[test]
    fn fresh_registry_reports_every_endpoint_absent() {
        let registry = PresenceRegistry::new();
        assert!(!registry.is_present(EndpointId::new()));
    }

    #[test]
    fn registering_one_session_makes_the_endpoint_present() {
        let registry = PresenceRegistry::new();
        let endpoint_id = EndpointId::new();
        registry.register(endpoint_id, session());
        assert!(registry.is_present(endpoint_id));
    }

    #[test]
    fn removing_the_only_session_makes_the_endpoint_absent() {
        let registry = PresenceRegistry::new();
        let endpoint_id = EndpointId::new();
        let s1 = session();
        registry.register(endpoint_id, s1);
        registry.unregister(endpoint_id, s1);
        assert!(!registry.is_present(endpoint_id));
    }

    #[test]
    fn a_second_session_survives_removal_of_the_first() {
        let registry = PresenceRegistry::new();
        let endpoint_id = EndpointId::new();
        let (s1, s2) = (session(), session());
        registry.register(endpoint_id, s1);
        registry.register(endpoint_id, s2);
        assert!(registry.is_present(endpoint_id));

        registry.unregister(endpoint_id, s1);
        assert!(
            registry.is_present(endpoint_id),
            "S1 closing must not erase S2's presence"
        );

        registry.unregister(endpoint_id, s2);
        assert!(
            !registry.is_present(endpoint_id),
            "only removal of the last session makes the Endpoint absent"
        );
    }

    #[test]
    fn registering_the_same_session_twice_is_idempotent() {
        let registry = PresenceRegistry::new();
        let endpoint_id = EndpointId::new();
        let s1 = session();
        registry.register(endpoint_id, s1);
        registry.register(endpoint_id, s1);

        // A single unregister of the duplicate-registered session must fully
        // clear presence — no phantom second registration remains.
        registry.unregister(endpoint_id, s1);
        assert!(!registry.is_present(endpoint_id));
    }

    #[test]
    fn unregistering_an_unknown_session_or_endpoint_is_a_safe_no_op() {
        let registry = PresenceRegistry::new();
        let endpoint_id = EndpointId::new();
        // No panic, no effect, for an Endpoint that was never registered.
        registry.unregister(endpoint_id, session());
        assert!(!registry.is_present(endpoint_id));

        // Nor for a session that never matched an existing registration.
        registry.register(endpoint_id, session());
        registry.unregister(endpoint_id, session());
        assert!(registry.is_present(endpoint_id));
    }

    #[test]
    fn different_endpoints_remain_isolated() {
        let registry = PresenceRegistry::new();
        let (a, b) = (EndpointId::new(), EndpointId::new());
        registry.register(a, session());

        assert!(registry.is_present(a));
        assert!(!registry.is_present(b));
    }
}
