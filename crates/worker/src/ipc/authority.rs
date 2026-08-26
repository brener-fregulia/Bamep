//! Fail-closed authoritative-control state (Issue #37 "Fail-closed
//! authority state"; ADR-0018 "IPC loss is fail-closed"): whether Worker
//! currently has a live, current-generation, handshaken `bamepd` control
//! connection.
//!
//! [`AuthoritySnapshot::is_available`] is the single invariant #38/#39 must
//! consume rather than reinventing: `true` only for a current-generation
//! successful handshake, `false` the instant that connection is lost —
//! never cached, inferred, or fabricated locally.

use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorityPhase {
    Disconnected,
    Connecting,
    Handshaking,
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthoritySnapshot {
    pub phase: AuthorityPhase,
    /// `0` before this Worker process ever completed a handshake; increments
    /// by exactly one on every successful handshake for the process's
    /// lifetime (`m1-worker-data-plane-control-contract.md` "Connection
    /// generations and correlation"). Stable across the process lifetime —
    /// only a fresh process start resets it, mirroring `worker_instance_id`.
    pub generation: u64,
}

impl AuthoritySnapshot {
    pub fn is_available(&self) -> bool {
        self.phase == AuthorityPhase::Ready
    }
}

/// Shared handle updated by the reconnect loop and observed by callers
/// (later #38/#39 request paths; today, tests and `bamep-worker`'s own
/// startup log). `tokio::sync::watch` gives observers the current snapshot
/// plus notification of every subsequent change, without polling.
#[derive(Clone)]
pub struct AuthorityTracker {
    tx: watch::Sender<AuthoritySnapshot>,
}

impl AuthorityTracker {
    pub fn new() -> (Self, watch::Receiver<AuthoritySnapshot>) {
        let (tx, rx) = watch::channel(AuthoritySnapshot {
            phase: AuthorityPhase::Disconnected,
            generation: 0,
        });
        (Self { tx }, rx)
    }

    pub fn current(&self) -> AuthoritySnapshot {
        *self.tx.borrow()
    }

    fn set_phase(&self, phase: AuthorityPhase) {
        self.tx.send_modify(|snapshot| snapshot.phase = phase);
    }

    pub fn set_disconnected(&self) {
        self.set_phase(AuthorityPhase::Disconnected);
    }

    pub fn set_connecting(&self) {
        self.set_phase(AuthorityPhase::Connecting);
    }

    pub fn set_handshaking(&self) {
        self.set_phase(AuthorityPhase::Handshaking);
    }

    /// A successful handshake starts a new connection generation. Returns
    /// the new generation number.
    pub fn set_ready(&self) -> u64 {
        let mut generation = 0;
        self.tx.send_modify(|snapshot| {
            snapshot.generation += 1;
            snapshot.phase = AuthorityPhase::Ready;
            generation = snapshot.generation;
        });
        generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_disconnected_and_unavailable() {
        let (tracker, _rx) = AuthorityTracker::new();
        let snapshot = tracker.current();
        assert_eq!(snapshot.phase, AuthorityPhase::Disconnected);
        assert_eq!(snapshot.generation, 0);
        assert!(!snapshot.is_available());
    }

    #[test]
    fn only_ready_is_available() {
        let (tracker, _rx) = AuthorityTracker::new();
        for phase_setter in [
            AuthorityTracker::set_connecting as fn(&AuthorityTracker),
            AuthorityTracker::set_handshaking as fn(&AuthorityTracker),
            AuthorityTracker::set_disconnected as fn(&AuthorityTracker),
        ] {
            phase_setter(&tracker);
            assert!(!tracker.current().is_available());
        }
        tracker.set_ready();
        assert!(tracker.current().is_available());
    }

    #[test]
    fn each_successful_handshake_advances_the_generation() {
        let (tracker, _rx) = AuthorityTracker::new();
        assert_eq!(tracker.set_ready(), 1);
        tracker.set_disconnected();
        assert_eq!(tracker.set_ready(), 2);
    }

    #[test]
    fn disconnect_immediately_clears_availability() {
        let (tracker, _rx) = AuthorityTracker::new();
        tracker.set_ready();
        assert!(tracker.current().is_available());
        tracker.set_disconnected();
        assert!(!tracker.current().is_available());
    }
}
