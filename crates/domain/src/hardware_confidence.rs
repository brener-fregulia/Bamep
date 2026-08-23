//! `HardwareConfidence`: the Endpoint's durable hardware-confidence dimension
//! (`docs/specifications/m0-endpoint-identity-lifecycle.md` "3. Hardware
//! confidence").
//!
//! This is the third independent aggregate dimension the Specification
//! already defines, alongside identity ([`crate::identity::IdentityState`])
//! and credential/session lifecycle ([`crate::credential::CredentialChain`]):
//! it is never inferred from identity approval, credential validity, Agent
//! presence, or trusted current bootstrap, and none of those may be inferred
//! from it either.
//!
//! This module is pure, like every other Domain module: it never touches the
//! clock, randomness, or persistence directly, and it implements no
//! comparison/threshold/escalation policy — those remain implementation-time
//! and out of scope for this checkpoint.

use serde::{Deserialize, Serialize};

/// The authoritative three-state hardware-confidence vocabulary
/// (`m0-endpoint-identity-lifecycle.md` "3. Hardware confidence"). No
/// transition function is defined by this checkpoint: the only value ever
/// produced here is the `Consistent` initial baseline
/// ([`HardwareConfidence::initial`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HardwareConfidence {
    Consistent,
    LoweredConfidence,
    Conflict,
}

impl HardwareConfidence {
    /// The initial hardware-confidence baseline for a newly created Endpoint
    /// (`m0-endpoint-identity-lifecycle.md` "3. Hardware confidence":
    /// "a newly created Endpoint begins at `Consistent`. This is the initial
    /// hardware baseline only, established at Endpoint creation
    /// independently of identity approval"). Does not imply `Enrolled`
    /// identity, `CredentialActive` credential state, current Agent
    /// presence, or trusted current bootstrap — each remains its own
    /// independent dimension/precondition.
    pub fn initial() -> Self {
        HardwareConfidence::Consistent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_baseline_is_consistent() {
        assert_eq!(
            HardwareConfidence::initial(),
            HardwareConfidence::Consistent
        );
    }
}
