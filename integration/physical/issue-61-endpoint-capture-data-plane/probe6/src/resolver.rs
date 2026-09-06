//! Issue #61 CP4 — Agent-local source-observation-epoch resolver.
//!
//! Pure, host-testable. The resolver takes ONLY the opaque cross-boundary
//! tuple `(source_observation_id, agent_source_id)` and resolves it against
//! the mapping the probe built during read-only enumeration of THIS boot.
//!
//! It NEVER accepts `\\.\PhysicalDriveN`, a model string, a serial, a bus
//! type, or an enumeration ordinal as a selection input, and it NEVER falls
//! back to "the first source" / `PhysicalDrive0` / discovery order. A tuple
//! that does not resolve exactly once fails closed.
//!
//! This is probe-local Spike evidence. It is NOT the production
//! `bamep.m2.endpoint-capture-transfer` action and NOT a product
//! `SOURCE_REFERENCE_STALE` implementation — no product component yet resolves
//! an authoritative `SourceReference` (Issue #60 CP7; Issue #61 CP0/CP3).

/// One entry of the current epoch's mapping. `agent_source_id` is the only
/// cross-boundary value; `local_locator` is Agent-local lab evidence that the
/// resolver *returns* but never *selects by*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochEntry {
    pub agent_source_id: String,
    pub local_locator: String,
}

/// The current source-mapping continuity epoch (RF-3): the Agent's live
/// `(source_observation_id, agent_source_id) -> exact local source` mapping
/// for THIS enumeration.
#[derive(Debug, Clone)]
pub struct CurrentEpoch {
    observation_id: String,
    entries: Vec<EpochEntry>,
    duplicate_agent_source_ids: Vec<String>,
}

/// Why a resolution failed closed. Every variant means "no source is
/// selected"; none carries a usable locator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// The current epoch's projection contains a duplicate `agent_source_id`
    /// (RF-4): the whole projection is ambiguous and NO tuple resolves while
    /// the duplication persists.
    AmbiguousEpoch { duplicate_agent_source_ids: Vec<String> },
    /// `source_observation_id` is not this boot's current epoch — a stale or
    /// unrecognised source-observation epoch (RF-3). The Agent must never
    /// resolve against a mapping from a different epoch.
    StaleObservationEpoch { presented: String, current: String },
    /// The epoch matches but no entry has this `agent_source_id`.
    UnknownAgentSourceId { agent_source_id: String },
    /// The epoch matches and more than one entry has this `agent_source_id`
    /// (defence in depth against a mapping built with a duplicate).
    AmbiguousAgentSourceId { agent_source_id: String, count: usize },
}

/// A successfully resolved source. `local_locator` is evidence-only — it is
/// the *consequence* of resolving the tuple, never an input to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub agent_source_id: String,
    pub local_locator: String,
}

impl CurrentEpoch {
    pub fn new(observation_id: impl Into<String>, entries: Vec<EpochEntry>) -> Self {
        let observation_id = observation_id.into();
        let mut seen = std::collections::BTreeSet::new();
        let mut dups = std::collections::BTreeSet::new();
        for e in &entries {
            if !seen.insert(e.agent_source_id.clone()) {
                dups.insert(e.agent_source_id.clone());
            }
        }
        Self {
            observation_id,
            entries,
            duplicate_agent_source_ids: dups.into_iter().collect(),
        }
    }

    pub fn observation_id(&self) -> &str {
        &self.observation_id
    }

    pub fn has_duplicate_agent_source_ids(&self) -> bool {
        !self.duplicate_agent_source_ids.is_empty()
    }

    /// Resolve STRICTLY by the opaque tuple. Fails closed on any doubt.
    /// There is no code path that returns a source for a non-matching epoch
    /// or a non-matching id.
    pub fn resolve(
        &self,
        source_observation_id: &str,
        agent_source_id: &str,
    ) -> Result<Resolved, ResolveError> {
        if !self.duplicate_agent_source_ids.is_empty() {
            return Err(ResolveError::AmbiguousEpoch {
                duplicate_agent_source_ids: self.duplicate_agent_source_ids.clone(),
            });
        }
        if source_observation_id != self.observation_id {
            return Err(ResolveError::StaleObservationEpoch {
                presented: source_observation_id.to_string(),
                current: self.observation_id.clone(),
            });
        }
        let mut hits = self
            .entries
            .iter()
            .filter(|e| e.agent_source_id == agent_source_id);
        match (hits.next(), hits.next()) {
            (Some(e), None) => Ok(Resolved {
                agent_source_id: e.agent_source_id.clone(),
                local_locator: e.local_locator.clone(),
            }),
            (None, _) => Err(ResolveError::UnknownAgentSourceId {
                agent_source_id: agent_source_id.to_string(),
            }),
            (Some(_), Some(_)) => Err(ResolveError::AmbiguousAgentSourceId {
                agent_source_id: agent_source_id.to_string(),
                count: self
                    .entries
                    .iter()
                    .filter(|e| e.agent_source_id == agent_source_id)
                    .count(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(asid: &str, loc: &str) -> EpochEntry {
        EpochEntry {
            agent_source_id: asid.into(),
            local_locator: loc.into(),
        }
    }

    /// Two distinct sources: [0] the "HDD", [1] the "SSD" — deliberately with
    /// the SSD SECOND so a fallback-to-first bug is visible.
    fn epoch() -> CurrentEpoch {
        CurrentEpoch::new(
            "OBS-current-epoch-0000000000000000000000000",
            vec![
                entry("asid-hdd", r"\\.\PhysicalDrive1"),
                entry("asid-ssd", r"\\.\PhysicalDrive0"),
            ],
        )
    }

    // A — current epoch + known agent_source_id -> resolves exactly one source
    #[test]
    fn a_current_epoch_known_id_resolves_exactly_one() {
        let e = epoch();
        let r = e
            .resolve(e.observation_id(), "asid-ssd")
            .expect("must resolve");
        assert_eq!(r.agent_source_id, "asid-ssd");
        assert_eq!(r.local_locator, r"\\.\PhysicalDrive0");
    }

    // B — current epoch + unknown agent_source_id -> fail closed
    #[test]
    fn b_current_epoch_unknown_id_fails_closed() {
        let e = epoch();
        assert_eq!(
            e.resolve(e.observation_id(), "asid-not-in-this-epoch"),
            Err(ResolveError::UnknownAgentSourceId {
                agent_source_id: "asid-not-in-this-epoch".into()
            })
        );
    }

    // C — stale source_observation_id + otherwise known id -> fail closed
    #[test]
    fn c_stale_observation_epoch_fails_closed_even_for_a_known_id_string() {
        let e = epoch();
        match e.resolve("OBS-a-different-superseded-epoch-000000000000", "asid-ssd") {
            Err(ResolveError::StaleObservationEpoch { presented, current }) => {
                assert_eq!(presented, "OBS-a-different-superseded-epoch-000000000000");
                assert_eq!(current, e.observation_id());
            }
            other => panic!("expected StaleObservationEpoch, got {other:?}"),
        }
    }

    // D — duplicate / ambiguous agent_source_id mapping -> fail closed for ANY resolve
    #[test]
    fn d_duplicate_agent_source_id_epoch_is_fail_closed_for_every_tuple() {
        let e = CurrentEpoch::new(
            "OBS-dup",
            vec![
                entry("dup", r"\\.\PhysicalDrive0"),
                entry("dup", r"\\.\PhysicalDrive1"),
                entry("unique", r"\\.\PhysicalDrive2"),
            ],
        );
        assert!(e.has_duplicate_agent_source_ids());
        // even the non-duplicated id cannot be resolved while the projection
        // is ambiguous
        assert_eq!(
            e.resolve(e.observation_id(), "unique"),
            Err(ResolveError::AmbiguousEpoch {
                duplicate_agent_source_ids: vec!["dup".into()]
            })
        );
        assert_eq!(
            e.resolve(e.observation_id(), "dup"),
            Err(ResolveError::AmbiguousEpoch {
                duplicate_agent_source_ids: vec!["dup".into()]
            })
        );
    }

    // E — no fallback to first source / PhysicalDrive0 / enumeration order
    #[test]
    fn e_no_fallback_to_first_or_ordinal_or_path() {
        let e = epoch(); // entries[0] is the HDD
        // an unknown id must NOT silently return entries[0]
        assert!(e.resolve(e.observation_id(), "totally-unknown").is_err());
        // a stale epoch must NOT silently return entries[0]
        assert!(e.resolve("nope", "asid-ssd").is_err());
        // and a known id resolves to ITS entry, not the first one
        let r = e.resolve(e.observation_id(), "asid-hdd").unwrap();
        assert_eq!(r.local_locator, r"\\.\PhysicalDrive1");
        let r = e.resolve(e.observation_id(), "asid-ssd").unwrap();
        assert_eq!(r.local_locator, r"\\.\PhysicalDrive0");
    }

    // the CP3 prior-epoch tuples are both stale relative to a fresh epoch
    #[test]
    fn cp3_prior_epoch_tuples_are_rejected_as_stale() {
        let e = epoch();
        for (obs, asid) in [
            (
                "fzSWUDJdAIdbvKkHa5UzXWp8ssDdr-blMbFHcUzUEVM",
                "7bGA10ahvEZcXtM5W7O0CtXc",
            ),
            (
                "L9mQpz0PIeoDXIsBibiDXmtSsvecsG1qdBG1GRoMa20",
                "z9CY-nubpHT9tTnNjAPCbwPj",
            ),
        ] {
            assert!(matches!(
                e.resolve(obs, asid),
                Err(ResolveError::StaleObservationEpoch { .. })
            ));
        }
    }
}
