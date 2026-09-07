//! Issue #63 Stage 1 — pure, mode-aware event state machine.
//!
//! Stage 1 proves ONLY the risky new automation primitives:
//!   derived PXE/WinPE runtime -> WinPE auto-starts the Issue-63 runner ->
//!   network ready -> runner reaches the Fedora coordinator -> Server UTC
//!   obtained -> WinPE system clock aligned automatically -> skew re-checked ->
//!   runner reports READY.
//!
//! There is NO bulk disk read, NO Transfer, NO Artifact and NO matrix.
//!
//! ## Evidence integrity (owner-mandated)
//!
//! A run that did NOT cross the physical MiniPC / UEFI PXE / WinPE environment
//! must NEVER be able to reach [`Verdict::PhysicalPass`]. The coordinator is
//! told its [`Mode`] EXPLICITLY (never inferred from a hostname/address):
//!
//!   * `Mode::Physical`   -> may reach `PhysicalPass`, but only when every
//!     integrity check holds: no host-smoke-stamped event, the runner reported
//!     the real Win32 clock backend, and the two bootstrap milestones were
//!     FORWARDED from the bootstrap's on-disk `X:\` evidence file (origin
//!     `bootstrap-forwarded`) — a synthesised/inferred milestone is a hard
//!     integrity failure.
//!   * `Mode::HostSmoke`  -> best case is `HostSmokePass`; `PhysicalPass` is
//!     structurally unreachable. Integrity checks are recorded but NOT enforced
//!     (host smoke is allowed to exercise the degraded paths).

use std::collections::BTreeSet;

/// The ordered Stage-1 milestones. Every one must be observed for a pass
/// (the last, `stage1.ready`, is only emitted after the clock is aligned and
/// re-checked in bound).
pub const EXPECTED: &[&str] = &[
    "winpe.booted",
    "winpe.wpeinit_complete",
    "winpe.network_ready",
    "winpe.runner_ready",
    "winpe.server_utc_received",
    "winpe.clock_alignment_attempted",
    "winpe.clock_aligned",
    "stage1.ready",
];

/// The two milestones that MUST come from the bootstrap's on-disk evidence file.
pub const BOOTSTRAP_MILESTONES: &[&str] = &["winpe.booted", "winpe.wpeinit_complete"];

/// Events that mean the Stage-1 chain failed closed.
pub const FAILURE_EVENTS: &[&str] = &[
    "winpe.clock_alignment_failed",
    "winpe.bootstrap_evidence_missing",
    "stage1.failed",
];

/// The only `origin` value that counts as a genuinely OBSERVED bootstrap
/// milestone (forwarded verbatim by the runner from `X:\bamep-i63-events.ndjson`).
pub const OBSERVED_BOOTSTRAP_ORIGIN: &str = "bootstrap-forwarded";
/// Origins that are synthesised/inferred and must NEVER satisfy a physical gate.
pub const SYNTHESISED_ORIGINS: &[&str] =
    &["runner-inferred", "runner-synthesized", "runner-degraded", "coordinator"];

pub const CLOCK_BACKEND_PHYSICAL: &str = "win32-setsystemtime-utc";
pub const CLOCK_BACKEND_HOST: &str = "host-stub-noop";

pub fn is_expected(event: &str) -> bool {
    EXPECTED.iter().any(|e| *e == event)
}
pub fn is_failure(event: &str) -> bool {
    FAILURE_EVENTS.iter().any(|e| *e == event)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Physical,
    HostSmoke,
}
impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "physical" => Some(Mode::Physical),
            "host-smoke" | "host_smoke" | "simulation" => Some(Mode::HostSmoke),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Physical => "physical",
            Mode::HostSmoke => "host-smoke",
        }
    }
}

/// A parsed view of one ingested event line, for the pure machine.
#[derive(Debug, Default, Clone)]
pub struct Observed {
    pub name: String,
    /// The mode the event line was stamped with (every runner event carries it).
    pub mode: Option<String>,
    /// `bootstrap-forwarded`, `runner`, `runner-inferred`, ...
    pub origin: Option<String>,
    /// Present only on `winpe.runner_start`.
    pub clock_backend: Option<String>,
}
impl Observed {
    pub fn named(name: &str) -> Self {
        Self {
            name: name.to_string(),
            ..Default::default()
        }
    }
    pub fn with_origin(mut self, o: &str) -> Self {
        self.origin = Some(o.to_string());
        self
    }
    pub fn with_mode(mut self, m: &str) -> Self {
        self.mode = Some(m.to_string());
        self
    }
    pub fn with_clock_backend(mut self, c: &str) -> Self {
        self.clock_backend = Some(c.to_string());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Pending { progress: usize },
    HostSmokePass,
    PhysicalPass,
    Fail { reason: String },
}

impl Verdict {
    /// True once the run has reached a terminal state (pass or fail). The
    /// coordinator is *expected* to exit cleanly after this; the launcher must
    /// not treat that exit as "service unexpectedly died".
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Verdict::Pending { .. })
    }
}

/// The short marker string written to the coordinator's `--verdict-file` (and
/// used by the launcher to tell an expected terminal exit from a crash), or
/// `None` for a still-running verdict.
pub fn verdict_marker(v: &Verdict, mode: Mode) -> Option<&'static str> {
    match (v, mode) {
        (Verdict::PhysicalPass, _) => Some("physical_pass"),
        (Verdict::HostSmokePass, _) => Some("host_smoke_pass"),
        (Verdict::Fail { .. }, Mode::Physical) => Some("physical_fail"),
        (Verdict::Fail { .. }, Mode::HostSmoke) => Some("host_smoke_fail"),
        (Verdict::Pending { .. }, _) => None,
    }
}

pub struct Stage1Machine {
    mode: Mode,
    seen: BTreeSet<String>,
    failed: Option<String>,
    integrity: Vec<String>,
}

impl Stage1Machine {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            seen: BTreeSet::new(),
            failed: None,
            integrity: Vec::new(),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Record an integrity violation (deduped). Recorded in every mode; only
    /// ENFORCED (blocks a pass) in `Mode::Physical`.
    fn violate(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        if !self.integrity.contains(&msg) {
            self.integrity.push(msg);
        }
    }

    pub fn integrity_violations(&self) -> &[String] {
        &self.integrity
    }

    /// Feed one observed event; returns the resulting verdict.
    pub fn observe(&mut self, ev: &Observed) -> Verdict {
        if is_failure(&ev.name) && self.failed.is_none() {
            self.failed = Some(ev.name.clone());
        }

        // (a) a mode-stamped event that disagrees with the coordinator's mode
        match ev.mode.as_deref() {
            Some(m) if m != self.mode.as_str() => {
                self.violate(format!(
                    "event {:?} stamped mode={m:?} but coordinator mode={}",
                    ev.name,
                    self.mode.as_str()
                ));
            }
            _ => {}
        }

        // (b) the runner's clock backend, reported once on winpe.runner_start
        if ev.name == "winpe.runner_start" {
            match ev.clock_backend.as_deref() {
                Some(CLOCK_BACKEND_PHYSICAL) => {}
                Some(other) => self.violate(format!(
                    "winpe.runner_start clock_backend={other:?} is not the real Win32 backend"
                )),
                None => self.violate("winpe.runner_start carried no clock_backend"),
            }
        }

        if is_expected(&ev.name) {
            // (c) any milestone from a synthesised/inferred origin
            if let Some(o) = ev.origin.as_deref() {
                if SYNTHESISED_ORIGINS.contains(&o) {
                    self.violate(format!("milestone {:?} has synthesised origin {o:?}", ev.name));
                }
            }
            // (d) the two bootstrap milestones must be FORWARDED from on-disk evidence
            if BOOTSTRAP_MILESTONES.contains(&ev.name.as_str()) {
                let forwarded = ev.origin.as_deref() == Some(OBSERVED_BOOTSTRAP_ORIGIN);
                if !forwarded {
                    self.violate(format!(
                        "bootstrap milestone {:?} not forwarded from on-disk evidence (origin={:?})",
                        ev.name, ev.origin
                    ));
                }
            }
            self.seen.insert(ev.name.clone());
        }

        self.verdict()
    }

    pub fn verdict(&self) -> Verdict {
        if let Some(e) = &self.failed {
            return Verdict::Fail {
                reason: format!("failure_event={e}"),
            };
        }
        // Integrity failures are terminal in physical mode only.
        if self.mode == Mode::Physical && !self.integrity.is_empty() {
            return Verdict::Fail {
                reason: format!("evidence_integrity: {}", self.integrity.join("; ")),
            };
        }
        let progress = EXPECTED
            .iter()
            .take_while(|e| self.seen.contains(**e))
            .count();
        if progress < EXPECTED.len() {
            return Verdict::Pending { progress };
        }
        match self.mode {
            Mode::Physical => Verdict::PhysicalPass,
            Mode::HostSmoke => Verdict::HostSmokePass,
        }
    }

    pub fn missing(&self) -> Vec<&'static str> {
        EXPECTED
            .iter()
            .copied()
            .filter(|e| !self.seen.contains(*e))
            .collect()
    }
    pub fn observed(&self) -> Vec<&'static str> {
        EXPECTED
            .iter()
            .copied()
            .filter(|e| self.seen.contains(*e))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-formed physical event stream: bootstrap milestones forwarded from
    /// on-disk evidence, runner_start carrying the real Win32 clock backend,
    /// every line stamped `mode=physical`.
    fn physical_stream() -> Vec<Observed> {
        let mut v = vec![Observed::named("winpe.runner_start")
            .with_mode("physical")
            .with_clock_backend(CLOCK_BACKEND_PHYSICAL)];
        for name in EXPECTED {
            let mut e = Observed::named(name).with_mode("physical");
            if BOOTSTRAP_MILESTONES.contains(name) {
                e = e.with_origin(OBSERVED_BOOTSTRAP_ORIGIN);
            } else {
                e = e.with_origin("runner");
            }
            v.push(e);
        }
        v
    }

    fn feed(mode: Mode, stream: &[Observed]) -> Verdict {
        let mut m = Stage1Machine::new(mode);
        let mut last = m.verdict();
        for e in stream {
            last = m.observe(e);
        }
        last
    }

    // ---- 1. complete OBSERVED physical chain -> PhysicalPass -------------
    #[test]
    fn complete_observed_physical_chain_is_physical_pass() {
        assert_eq!(feed(Mode::Physical, &physical_stream()), Verdict::PhysicalPass);
    }

    // ---- 2. missing booted -> no PhysicalPass ---------------------------
    #[test]
    fn missing_booted_never_physical_pass() {
        let stream: Vec<Observed> = physical_stream()
            .into_iter()
            .filter(|e| e.name != "winpe.booted")
            .collect();
        match feed(Mode::Physical, &stream) {
            Verdict::PhysicalPass => panic!("must not PhysicalPass without winpe.booted"),
            Verdict::Pending { progress } => assert_eq!(progress, 0),
            other => panic!("unexpected {other:?}"),
        }
    }

    // ---- 3. missing wpeinit_complete -> no PhysicalPass ----------------
    #[test]
    fn missing_wpeinit_complete_never_physical_pass() {
        let stream: Vec<Observed> = physical_stream()
            .into_iter()
            .filter(|e| e.name != "winpe.wpeinit_complete")
            .collect();
        assert!(!matches!(
            feed(Mode::Physical, &stream),
            Verdict::PhysicalPass
        ));
    }

    // ---- 4. synthesised / inferred milestone -> no PhysicalPass -------
    #[test]
    fn synthesised_bootstrap_milestone_blocks_physical_pass() {
        let stream: Vec<Observed> = physical_stream()
            .into_iter()
            .map(|e| {
                if e.name == "winpe.booted" {
                    Observed::named("winpe.booted")
                        .with_mode("physical")
                        .with_origin("runner-inferred")
                } else {
                    e
                }
            })
            .collect();
        match feed(Mode::Physical, &stream) {
            Verdict::Fail { reason } => assert!(reason.contains("evidence_integrity")),
            other => panic!("expected evidence_integrity Fail, got {other:?}"),
        }
    }

    #[test]
    fn bare_origin_bootstrap_milestone_blocks_physical_pass() {
        // origin present but NOT `bootstrap-forwarded` (e.g. the bootstrap's own
        // `origin:"bootstrap"` tag) is still not proof it was forwarded verbatim.
        let stream: Vec<Observed> = physical_stream()
            .into_iter()
            .map(|e| {
                if e.name == "winpe.wpeinit_complete" {
                    Observed::named("winpe.wpeinit_complete")
                        .with_mode("physical")
                        .with_origin("bootstrap")
                } else {
                    e
                }
            })
            .collect();
        assert!(matches!(
            feed(Mode::Physical, &stream),
            Verdict::Fail { .. }
        ));
    }

    // ---- 5. host mode + perfect chain -> HostSmokePass, never Physical
    #[test]
    fn host_mode_perfect_chain_is_host_smoke_pass_only() {
        let mut v = vec![Observed::named("winpe.runner_start")
            .with_mode("host-smoke")
            .with_clock_backend(CLOCK_BACKEND_HOST)];
        for name in EXPECTED {
            let mut e = Observed::named(name).with_mode("host-smoke");
            if BOOTSTRAP_MILESTONES.contains(name) {
                e = e.with_origin(OBSERVED_BOOTSTRAP_ORIGIN);
            }
            v.push(e);
        }
        assert_eq!(feed(Mode::HostSmoke, &v), Verdict::HostSmokePass);
    }

    #[test]
    fn host_smoke_events_cannot_yield_physical_pass_even_via_physical_coordinator() {
        // A host-smoke runner stream fed to a physical coordinator: the mode
        // mismatch AND the host clock backend are both integrity failures.
        let mut v = vec![Observed::named("winpe.runner_start")
            .with_mode("host-smoke")
            .with_clock_backend(CLOCK_BACKEND_HOST)];
        for name in EXPECTED {
            let mut e = Observed::named(name).with_mode("host-smoke");
            if BOOTSTRAP_MILESTONES.contains(name) {
                e = e.with_origin(OBSERVED_BOOTSTRAP_ORIGIN);
            }
            v.push(e);
        }
        match feed(Mode::Physical, &v) {
            Verdict::Fail { reason } => assert!(reason.contains("evidence_integrity")),
            other => panic!("expected Fail, got {other:?}"),
        }
    }

    #[test]
    fn physical_mode_missing_clock_backend_blocks_pass() {
        let stream: Vec<Observed> = physical_stream()
            .into_iter()
            .map(|e| {
                if e.name == "winpe.runner_start" {
                    Observed::named("winpe.runner_start").with_mode("physical")
                } else {
                    e
                }
            })
            .collect();
        assert!(matches!(feed(Mode::Physical, &stream), Verdict::Fail { .. }));
    }

    // ---- failure events latch -----------------------------------------
    #[test]
    fn clock_alignment_failed_is_terminal() {
        let mut m = Stage1Machine::new(Mode::Physical);
        for e in &physical_stream()[..5] {
            m.observe(e);
        }
        let v = m.observe(&Observed::named("winpe.clock_alignment_failed").with_mode("physical"));
        assert!(matches!(v, Verdict::Fail { .. }));
        // a late-arriving milestone must not flip it back
        let v = m.observe(&Observed::named("stage1.ready").with_mode("physical").with_origin("runner"));
        assert!(matches!(v, Verdict::Fail { .. }));
    }

    #[test]
    fn bootstrap_evidence_missing_is_terminal_failure() {
        let mut m = Stage1Machine::new(Mode::Physical);
        m.observe(&Observed::named("winpe.runner_start").with_mode("physical").with_clock_backend(CLOCK_BACKEND_PHYSICAL));
        let v = m.observe(&Observed::named("winpe.bootstrap_evidence_missing").with_mode("physical"));
        assert!(matches!(v, Verdict::Fail { .. }));
    }

    #[test]
    fn empty_is_pending_zero_and_unknown_events_are_ignored() {
        let mut m = Stage1Machine::new(Mode::Physical);
        assert_eq!(m.verdict(), Verdict::Pending { progress: 0 });
        m.observe(&Observed::named("winpe.some.debug").with_mode("physical"));
        assert_eq!(m.verdict(), Verdict::Pending { progress: 0 });
    }

    #[test]
    fn out_of_order_within_cumulative_snapshot_still_resolves_physical() {
        let mut stream = physical_stream();
        stream.reverse();
        assert_eq!(feed(Mode::Physical, &stream), Verdict::PhysicalPass);
    }

    // ---- lifecycle: terminal marker (owner review round 2) --------------

    #[test]
    fn pending_has_no_verdict_marker_and_is_not_terminal() {
        let v = Verdict::Pending { progress: 3 };
        assert!(!v.is_terminal());
        assert_eq!(verdict_marker(&v, Mode::Physical), None);
        assert_eq!(verdict_marker(&v, Mode::HostSmoke), None);
    }

    #[test]
    fn terminal_verdicts_have_stable_markers() {
        assert!(Verdict::PhysicalPass.is_terminal());
        assert!(Verdict::HostSmokePass.is_terminal());
        assert!(Verdict::Fail { reason: "x".into() }.is_terminal());

        assert_eq!(verdict_marker(&Verdict::PhysicalPass, Mode::Physical), Some("physical_pass"));
        assert_eq!(verdict_marker(&Verdict::HostSmokePass, Mode::HostSmoke), Some("host_smoke_pass"));
        assert_eq!(
            verdict_marker(&Verdict::Fail { reason: "e".into() }, Mode::Physical),
            Some("physical_fail")
        );
        assert_eq!(
            verdict_marker(&Verdict::Fail { reason: "e".into() }, Mode::HostSmoke),
            Some("host_smoke_fail")
        );
    }

    #[test]
    fn a_reached_physical_pass_yields_a_terminal_marker() {
        let mut m = Stage1Machine::new(Mode::Physical);
        let mut v = m.verdict();
        for e in physical_stream() {
            v = m.observe(&e);
        }
        assert_eq!(v, Verdict::PhysicalPass);
        assert!(v.is_terminal());
        assert_eq!(verdict_marker(&v, m.mode()), Some("physical_pass"));
    }
}
