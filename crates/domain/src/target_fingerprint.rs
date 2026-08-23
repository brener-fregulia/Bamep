//! `TargetFingerprint`: an opaque, stable target-disk identity/evidence value
//! (`docs/specifications/m0-endpoint-identity-lifecycle.md` "5. Target-disk
//! revalidation"; `docs/specifications/m0-simulator-contract-and-validation-strategy.md`
//! "Target-disk revalidation fidelity boundary").
//!
//! Deliberately opaque: Domain never parses, compares component-wise, or
//! infers this value from inventory JSON — it is a narrow evidence handle for
//! destructive-operation precondition 5, not a disk/partition/storage-
//! topology model. Equality is the only operation this checkpoint needs.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TargetFingerprint(pub String);

impl TargetFingerprint {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}
