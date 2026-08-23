//! Assembles the pure state-machine modules (`identity`, `credential`,
//! `boot_context`) into the concrete durable transitions WP1 requires, each
//! producing a [`RedeemOutcome`]/[`TransitionOutcome`] ready for atomic
//! persistence (ADR-0013 "PostgreSQL persistence backend baseline", carrying
//! forward ADR-0007's "Transactional consistency between domain state,
//! domain events, and audit records").
//!
//! Every [`TransitionOutcome`] returned here is constructed exclusively by
//! this module, so an Application-layer caller can never hand-assemble a
//! domain aggregate into an invariant-violating shape.

use bamep_trusted_bootstrap::BootNonce;
use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

use crate::boot_context::{BootContext, BootContextResolveError};
use crate::credential::{self, AuthOutcome, CredentialChain};
use crate::current_boot::{CurrentBoot, TrustedBootstrapState};
use crate::endpoint::{EndpointAggregate, EndpointId};
use crate::events::{Actor, AuditRecord, DomainEvent, TransitionOutcome};
use crate::hardware_confidence::HardwareConfidence;
use crate::identity::{self, IdentityState, InvalidIdentityTransition};
use crate::presented_credential::{CredentialKind, PresentedCredential};

/// Result of redeeming a credential in a fresh `AuthRequest`, whether for a
/// first-seen Endpoint, a genuine reboot, or a known one.
///
/// `Established` intentionally carries `TransitionOutcome` by value rather
/// than boxed: this type crosses one `AuthRequest` at a time, never a hot
/// per-message path (`ActionProgress`-style traffic stays out of this type
/// entirely), so the size difference clippy flags is not worth the
/// indirection at WP1's scale.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum RedeemOutcome {
    Established {
        outcome: TransitionOutcome,
        /// The fresh Runtime credential the caller generated and this
        /// redemption installed as the new successor — the caller already
        /// holds this value; it is echoed back so a single match on
        /// `RedeemOutcome` gives the Application everything it needs to
        /// answer the `AuthRequest`.
        issued: PresentedCredential,
        issued_expires_at: DateTime<Utc>,
        /// True only when this redemption is the Endpoint's very first
        /// (i.e. this outcome also creates `PendingEnrollment`).
        first_contact: bool,
        successor_confirmed: bool,
        /// The `BootContext` this redemption resolved, when it resolved one
        /// (first contact or genuine reboot) — `None` for an ordinary
        /// persisted-chain rotation (ADR-0014 point 5). The Adapter persists
        /// `resolved_endpoint_id` atomically with the rest of this outcome.
        resolved_boot_context: Option<BootContext>,
    },
    Rejected,
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum TrustedBootstrapOutcome {
    Established(TransitionOutcome),
    Rejected,
}

/// Establishes trust only for the freshly-read current boot. Rejection and
/// idempotence are pure Domain outcomes; no event or audit record exists for
/// this WP1 transition.
pub fn establish_trusted_bootstrap(
    aggregate: &EndpointAggregate,
    verified_current_nonce: BootNonce,
) -> TrustedBootstrapOutcome {
    let Some(current_boot) = aggregate.current_boot.as_ref() else {
        return TrustedBootstrapOutcome::Rejected;
    };
    if current_boot.boot_nonce() != verified_current_nonce {
        return TrustedBootstrapOutcome::Rejected;
    }
    TrustedBootstrapOutcome::Established(TransitionOutcome {
        endpoint: EndpointAggregate {
            current_boot: Some(current_boot.establish_trusted_bootstrap()),
            ..aggregate.clone()
        },
        events: vec![],
        audit: None,
    })
}

/// Verifies `presented` against `context` the way both `first_contact` and
/// `genuine_reboot` require (ADR-0014 point 7): correct kind, correct
/// lookup id, the `BootContext` still admits a first redemption at `now`,
/// and the secret verifies against the stored one-way verifier.
fn boot_context_credential_is_valid(
    context: &BootContext,
    presented: &PresentedCredential,
    now: DateTime<Utc>,
) -> bool {
    presented.kind() == CredentialKind::Enrollment
        && presented.lookup_id() == context.boot_context_id()
        && context.valid_at(now)
        && context.verify_secret(presented.secret())
}

/// `(no record) -> PendingEnrollment`, atomic with
/// `NoActiveCredential -> CredentialActive`
/// (`m0-endpoint-identity-lifecycle.md` "Transitions";
/// `docs/decisions/0012-...md` point 1; ADR-0014 point 5).
///
/// `context` must be unresolved — callers only reach this function through
/// `RedemptionTarget::UnresolvedBootContext { candidate_endpoint: None, .. }`
/// routing, but this function still verifies it explicitly rather than
/// trusting the caller. Every other credential check (kind, lookup id,
/// expiry, secret) is performed here too — this function does not trust an
/// already-verified `presented` the way the pre-ADR-0014 version did.
///
/// Returns `Err` only if [`BootContext::resolve`] itself reports a conflict
/// (a different Endpoint already resolved this `BootContext`) — a Domain
/// invariant failure the Adapter must roll back on, never collapsed into a
/// generic `Rejected` (ADR-0014 checkpoint "BootContext resolution error").
pub fn first_contact(
    context: &BootContext,
    presented: &PresentedCredential,
    fresh_successor: &PresentedCredential,
    now: DateTime<Utc>,
    ttl: Duration,
) -> Result<RedeemOutcome, BootContextResolveError> {
    if context.resolved_endpoint_id().is_some() {
        return Ok(RedeemOutcome::Rejected);
    }
    if !boot_context_credential_is_valid(context, presented, now) {
        return Ok(RedeemOutcome::Rejected);
    }

    let Some(chain) = CredentialChain::establish(
        presented.lookup_id().clone(),
        context.verifier().clone(),
        fresh_successor,
        now,
        ttl,
    ) else {
        return Ok(RedeemOutcome::Rejected);
    };

    let id = EndpointId::new();
    let aggregate = EndpointAggregate {
        id,
        inventory_signal: context.inventory_signal().to_string(),
        identity: IdentityState::PendingEnrollment,
        credential: chain,
        hardware_confidence: HardwareConfidence::initial(),
        current_boot: Some(CurrentBoot::new(
            context.boot_context_id().clone(),
            context.boot_nonce(),
            TrustedBootstrapState::NotEstablished,
        )),
        created_at: now,
        updated_at: now,
    };

    let event = DomainEvent::EndpointPendingEnrollment {
        event_id: Uuid::new_v4(),
        endpoint_id: id,
        occurred_at: now,
    };

    let resolved_context = context.resolve(id)?;

    Ok(RedeemOutcome::Established {
        outcome: TransitionOutcome {
            endpoint: aggregate,
            events: vec![event],
            audit: None,
        },
        issued: fresh_successor.clone(),
        issued_expires_at: now + ttl,
        first_contact: true,
        successor_confirmed: false,
        resolved_boot_context: Some(resolved_context),
    })
}

/// Redeems a credential against a known Endpoint's existing chain. Routine
/// rotation while remaining `CredentialActive` is durable bookkeeping, not a
/// new domain event (ADR-0012 point 9) — the returned [`TransitionOutcome`]
/// therefore carries no events on `Accepted`.
///
/// A rejected `AuthRequest` is explicitly not a domain-state transition
/// requiring persistence (`m0-endpoint-identity-lifecycle.md`; WP1 acceptance
/// criteria) — callers must not persist anything for [`RedeemOutcome::Rejected`].
pub fn redeem_known(
    aggregate: &EndpointAggregate,
    presented: &PresentedCredential,
    fresh_successor: &PresentedCredential,
    now: DateTime<Utc>,
    ttl: Duration,
) -> RedeemOutcome {
    match credential::authenticate(&aggregate.credential, presented, fresh_successor, now, ttl) {
        AuthOutcome::Accepted {
            chain,
            issued_expires_at,
            successor_confirmed,
        } => RedeemOutcome::Established {
            outcome: TransitionOutcome {
                endpoint: EndpointAggregate {
                    credential: chain,
                    updated_at: now,
                    ..aggregate.clone()
                },
                events: vec![],
                audit: None,
            },
            issued: fresh_successor.clone(),
            issued_expires_at,
            first_contact: false,
            successor_confirmed,
            resolved_boot_context: None,
        },
        AuthOutcome::Rejected => RedeemOutcome::Rejected,
    }
}

/// Genuine reboot of an already-known Endpoint (ADR-0012 point 1):
///
/// ```text
/// same boot:      E1 -> R1 -> R2 -> ...
/// genuine reboot: E2 (new enrollment credential) -> fresh runtime credential -> ...
/// ```
///
/// A fresh, valid boot-scoped enrollment credential, correlated by the
/// Adapter to `candidate` via `BootContext.inventory_signal`, establishes a
/// brand-new runtime-credential chain for the Endpoint's existing durable
/// identity — the old chain is superseded in full, exactly as
/// `CredentialChain::establish` already does for a first-seen Endpoint,
/// since "a runtime credential does not need to survive a genuine Agent
/// reboot" (ADR-0012 point 1). Endpoint identity/lifecycle state
/// (`PendingEnrollment`/`Enrolled`/`Retired`) is preserved unchanged: this is
/// a credential-dimension transition only, never a re-run of operator
/// approval (ADR-0004 "Reconnect handling"; `m0-endpoint-identity-lifecycle.md`
/// "Reconnect / credential renewal handling"). No domain event is emitted,
/// for the same reason routine rotation emits none (ADR-0012 point 9).
///
/// `context` must be unresolved, exactly like [`first_contact`]: an
/// already-resolved `BootContext` is no longer eligible for this transition
/// (ADR-0014) and is rejected before any credential validation runs — the
/// normal retry path for an already-resolved `BootContext` is Adapter
/// routing to the persisted Endpoint and [`redeem_known`], not this
/// function. `BootContext::resolve`'s idempotence for the same Endpoint does
/// not substitute for this check: without it, a retried, already-resolved
/// context would still re-establish (and re-persist) a fresh chain on every
/// call.
///
/// Rejects outright — without attempting to establish anything — when
/// `candidate`'s current chain is `CredentialRevoked`: `CredentialRevoked` is
/// durable Endpoint-level state that survives a genuine reboot, and a fresh
/// `E2` does not clear it (owner-approved policy, ADR-0012 point 8 /
/// "Consequences"; restoring `CredentialActive` requires a separate,
/// explicit, authorized reactivation operation, not designed here).
///
/// Returns `Err` only if [`BootContext::resolve`] itself reports a conflict,
/// exactly like [`first_contact`].
pub fn genuine_reboot(
    context: &BootContext,
    candidate: &EndpointAggregate,
    presented: &PresentedCredential,
    fresh_successor: &PresentedCredential,
    now: DateTime<Utc>,
    ttl: Duration,
) -> Result<RedeemOutcome, BootContextResolveError> {
    if context.resolved_endpoint_id().is_some() {
        // Not a resolution conflict: an already-resolved BootContext is
        // simply no longer eligible for this transition (ADR-0014). Retry
        // authenticates against the persisted promoted chain instead —
        // `BootContext::resolve`'s idempotence for the same Endpoint is
        // insufficient by itself to enforce that here, since it would
        // silently re-accept and re-establish a fresh chain on every retry.
        return Ok(RedeemOutcome::Rejected);
    }
    if !boot_context_credential_is_valid(context, presented, now) {
        return Ok(RedeemOutcome::Rejected);
    }
    if candidate.credential.is_revoked() {
        return Ok(RedeemOutcome::Rejected);
    }

    let Some(chain) = CredentialChain::establish(
        presented.lookup_id().clone(),
        context.verifier().clone(),
        fresh_successor,
        now,
        ttl,
    ) else {
        return Ok(RedeemOutcome::Rejected);
    };

    let resolved_context = context.resolve(candidate.id)?;

    Ok(RedeemOutcome::Established {
        outcome: TransitionOutcome {
            endpoint: EndpointAggregate {
                credential: chain,
                // Genuine reboot always replaces CurrentBoot with the newly
                // redeemed BootContext's exact identity/nonce and resets
                // trusted bootstrap to NotEstablished — mandatory even if the
                // previous CurrentBoot was Established
                // (`m0-trusted-bootstrap-and-server-fingerprint-contract.md`
                // "Authoritative current boot and durable Server state").
                current_boot: Some(CurrentBoot::new(
                    context.boot_context_id().clone(),
                    context.boot_nonce(),
                    TrustedBootstrapState::NotEstablished,
                )),
                updated_at: now,
                ..candidate.clone()
            },
            events: vec![],
            audit: None,
        },
        issued: fresh_successor.clone(),
        issued_expires_at: now + ttl,
        first_contact: false,
        successor_confirmed: false,
        resolved_boot_context: Some(resolved_context),
    })
}

/// `PendingEnrollment -> Enrolled`, atomic with `EndpointEnrolled` and
/// `OperatorDecisionRecorded`, plus the required immutable audit record
/// (`m0-persistence-observability-and-domain-events.md` "Auditability";
/// ADR-0004 "Decision: operator-approval-gated first enrollment").
pub fn approve_enrollment(
    aggregate: &EndpointAggregate,
    operator: Actor,
    now: DateTime<Utc>,
) -> Result<TransitionOutcome, InvalidIdentityTransition> {
    let new_identity = identity::approve(aggregate.identity)?;

    let enrolled_event = DomainEvent::EndpointEnrolled {
        event_id: Uuid::new_v4(),
        endpoint_id: aggregate.id,
        occurred_at: now,
    };
    let decision_event = DomainEvent::OperatorDecisionRecorded {
        event_id: Uuid::new_v4(),
        endpoint_id: aggregate.id,
        decision: "EnrollmentApproved".to_string(),
        actor: operator.clone(),
        occurred_at: now,
    };
    let audit = AuditRecord {
        audit_id: Uuid::new_v4(),
        endpoint_id: aggregate.id,
        actor: operator,
        occurred_at: now,
        detail: "EnrollmentApproved".to_string(),
        job_id: None,
        job_step_id: None,
        attempt_id: None,
        action_id: None,
    };

    Ok(TransitionOutcome {
        endpoint: EndpointAggregate {
            identity: new_identity,
            updated_at: now,
            ..aggregate.clone()
        },
        events: vec![enrolled_event, decision_event],
        audit: Some(audit),
    })
}

/// Explicit `CredentialRevoked`: invalidates every credential still valid in
/// the chain (ADR-0012 point 8). WP1 exercises this at the domain/persistence
/// layer directly (Issue #17 "Safety constraints"), with no operator-facing
/// revocation control path introduced — accordingly no domain event or audit
/// record is attributed here; a future Work Package that introduces a real
/// revocation control path owns deciding whether one is required.
pub fn revoke_credential(aggregate: &EndpointAggregate, now: DateTime<Utc>) -> TransitionOutcome {
    TransitionOutcome {
        endpoint: EndpointAggregate {
            credential: aggregate.credential.revoke(),
            updated_at: now,
            ..aggregate.clone()
        },
        events: vec![],
        audit: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::{CredentialHash, DEFAULT_CREDENTIAL_TTL};

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    fn fresh_runtime() -> PresentedCredential {
        PresentedCredential::generate(CredentialKind::Runtime)
    }

    /// Fixed test-only `BootNonce` for tests that don't need to distinguish
    /// one boot's nonce from another's — see [`issue_boot_context_with_nonce`]
    /// for tests that do (e.g. a genuine-reboot old-nonce/new-nonce
    /// assertion).
    fn default_boot_nonce() -> bamep_trusted_bootstrap::BootNonce {
        bamep_trusted_bootstrap::BootNonce::from_bytes([0xAA; 32])
    }

    /// Builds an unresolved `BootContext` plus the matching enrollment
    /// `PresentedCredential` that verifies against it — the pure-Domain
    /// stand-in for what `BootOrchestrationService::issue_enrollment_credential`
    /// produces end to end.
    fn issue_boot_context(
        inventory_signal: &str,
        now: DateTime<Utc>,
        ttl: Duration,
    ) -> (BootContext, PresentedCredential) {
        issue_boot_context_with_nonce(inventory_signal, default_boot_nonce(), now, ttl)
    }

    fn issue_boot_context_with_nonce(
        inventory_signal: &str,
        boot_nonce: bamep_trusted_bootstrap::BootNonce,
        now: DateTime<Utc>,
        ttl: Duration,
    ) -> (BootContext, PresentedCredential) {
        let credential = PresentedCredential::generate(CredentialKind::Enrollment);
        let verifier = CredentialHash::of_bytes(credential.secret().expose_secret_bytes());
        let context = BootContext::new(
            credential.lookup_id().clone(),
            verifier,
            now,
            now + ttl,
            inventory_signal.to_string(),
            boot_nonce,
        );
        (context, credential)
    }

    #[test]
    fn first_contact_creates_pending_enrollment_with_one_event() {
        let (context, e1) = issue_boot_context("mac:AA:BB", now(), Duration::minutes(5));
        let outcome = first_contact(
            &context,
            &e1,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .expect("fresh BootContext resolves cleanly");
        match outcome {
            RedeemOutcome::Established {
                outcome,
                first_contact,
                resolved_boot_context,
                ..
            } => {
                assert!(first_contact);
                assert_eq!(outcome.endpoint.identity, IdentityState::PendingEnrollment);
                assert_eq!(outcome.events.len(), 1);
                assert_eq!(outcome.events[0].event_type(), "EndpointPendingEnrollment");
                assert!(outcome.audit.is_none());
                assert_eq!(
                    resolved_boot_context.unwrap().resolved_endpoint_id(),
                    Some(outcome.endpoint.id)
                );
            }
            RedeemOutcome::Rejected => panic!("first contact must be established"),
        }
    }

    #[test]
    fn first_contact_initializes_consistent_hardware_confidence_independently_of_other_dimensions()
    {
        let (context, e1) = issue_boot_context("mac:AA:BB", now(), Duration::minutes(5));
        let RedeemOutcome::Established { outcome, .. } = first_contact(
            &context,
            &e1,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .unwrap() else {
            panic!("first contact must be established")
        };

        assert_eq!(
            outcome.endpoint.hardware_confidence,
            crate::HardwareConfidence::Consistent,
            "a newly created Endpoint must begin at Consistent"
        );
        // The Consistent baseline must not imply Enrolled identity or an
        // established current boot — each remains independent.
        assert_eq!(outcome.endpoint.identity, IdentityState::PendingEnrollment);
        assert_eq!(
            outcome
                .endpoint
                .current_boot
                .as_ref()
                .unwrap()
                .trusted_bootstrap(),
            TrustedBootstrapState::NotEstablished
        );
    }

    #[test]
    fn first_contact_rejects_wrong_kind_presented_credential() {
        let (context, e1) = issue_boot_context("mac:AA:BB", now(), Duration::minutes(5));
        // Same lookup id and secret, but wrong kind on the wire.
        let wrong_kind_wire = e1.to_wire_value().replacen(".enrollment.", ".runtime.", 1);
        let wrong_kind = PresentedCredential::parse(&wrong_kind_wire).unwrap();
        let outcome = first_contact(
            &context,
            &wrong_kind,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .unwrap();
        assert!(matches!(outcome, RedeemOutcome::Rejected));
    }

    #[test]
    fn first_contact_rejects_wrong_secret() {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

        let (context, e1) = issue_boot_context("mac:AA:BB", now(), Duration::minutes(5));
        // Different secret, but forged to carry E1's real lookup id.
        let other = PresentedCredential::generate(CredentialKind::Enrollment);
        let wire = format!(
            "v1.enrollment.{}.{}",
            URL_SAFE_NO_PAD.encode(e1.lookup_id().to_bytes()),
            URL_SAFE_NO_PAD.encode(other.secret().expose_secret_bytes())
        );
        let forged = PresentedCredential::parse(&wire).unwrap();
        let outcome = first_contact(
            &context,
            &forged,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .unwrap();
        assert!(matches!(outcome, RedeemOutcome::Rejected));
    }

    #[test]
    fn first_contact_rejects_an_expired_boot_context() {
        let past = now() - Duration::minutes(10);
        let (context, e1) = issue_boot_context("mac:AA:BB", past, Duration::minutes(5));
        let outcome = first_contact(
            &context,
            &e1,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .unwrap();
        assert!(matches!(outcome, RedeemOutcome::Rejected));
    }

    #[test]
    fn first_contact_rejects_an_already_resolved_boot_context() {
        let (context, e1) = issue_boot_context("mac:AA:BB", now(), Duration::minutes(5));
        let resolved = context.resolve(EndpointId::new()).unwrap();
        let outcome = first_contact(
            &resolved,
            &e1,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .unwrap();
        assert!(matches!(outcome, RedeemOutcome::Rejected));
    }

    #[test]
    fn redeem_known_rotation_produces_no_events() {
        let (context, e1) = issue_boot_context("mac:AA:BB", now(), Duration::minutes(5));
        let r1 = fresh_runtime();
        let RedeemOutcome::Established { outcome: first, .. } =
            first_contact(&context, &e1, &r1, now(), DEFAULT_CREDENTIAL_TTL).unwrap()
        else {
            panic!()
        };

        let outcome = redeem_known(
            &first.endpoint,
            &r1,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        );
        match outcome {
            RedeemOutcome::Established {
                outcome,
                successor_confirmed,
                resolved_boot_context,
                ..
            } => {
                assert!(successor_confirmed);
                assert!(
                    outcome.events.is_empty(),
                    "routine rotation must not emit a domain event"
                );
                assert!(outcome.audit.is_none());
                assert!(resolved_boot_context.is_none());
            }
            RedeemOutcome::Rejected => panic!("R1 must authenticate"),
        }
    }

    #[test]
    fn genuine_reboot_reestablishes_chain_preserving_identity_and_id() {
        let (context1, e1) = issue_boot_context("mac:AA:BB", now(), Duration::minutes(5));
        let RedeemOutcome::Established { outcome: first, .. } = first_contact(
            &context1,
            &e1,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .unwrap() else {
            panic!()
        };
        let original_id = first.endpoint.id;

        let (context2, e2) = issue_boot_context("mac:AA:BB", now(), Duration::minutes(5));
        let r_after_reboot = fresh_runtime();
        let RedeemOutcome::Established {
            outcome: rebooted, ..
        } = genuine_reboot(
            &context2,
            &first.endpoint,
            &e2,
            &r_after_reboot,
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .unwrap()
        else {
            panic!("genuine reboot must establish a fresh chain")
        };

        assert_eq!(
            rebooted.endpoint.id, original_id,
            "identity must be preserved across reboot"
        );
        assert_eq!(
            rebooted.endpoint.identity, first.endpoint.identity,
            "identity lifecycle state must not change on genuine reboot"
        );
        assert!(
            rebooted.events.is_empty(),
            "genuine reboot must not emit a domain event"
        );
        assert!(rebooted.audit.is_none());

        // The fresh successor from the reboot authenticates against the new
        // chain...
        assert!(matches!(
            credential::authenticate(
                &rebooted.endpoint.credential,
                &r_after_reboot,
                &fresh_runtime(),
                now(),
                DEFAULT_CREDENTIAL_TTL
            ),
            AuthOutcome::Accepted { .. }
        ));
        // ...but nothing from the pre-reboot chain does: it was fully
        // superseded, not merged.
        assert_eq!(
            credential::authenticate(
                &rebooted.endpoint.credential,
                &e1,
                &fresh_runtime(),
                now(),
                DEFAULT_CREDENTIAL_TTL
            ),
            AuthOutcome::Rejected
        );
    }

    #[test]
    fn genuine_reboot_rejects_an_already_resolved_boot_context() {
        let (context1, e1) = issue_boot_context("mac:AA:BB", now(), Duration::minutes(5));
        let RedeemOutcome::Established { outcome: first, .. } = first_contact(
            &context1,
            &e1,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .unwrap() else {
            panic!()
        };
        let candidate = first.endpoint.clone();
        let pre_attempt_chain = candidate.credential.clone();

        // A second BootContext for a genuine reboot, but already resolved to
        // the candidate Endpoint — e.g. a retried AuthRequest for an E2 that
        // already completed a genuine reboot once. This must no longer be
        // eligible for this transition: the normal retry path is Adapter
        // routing to the persisted Endpoint and `redeem_known`, not another
        // call to `genuine_reboot`.
        let (context2, e2) = issue_boot_context("mac:AA:BB", now(), Duration::minutes(5));
        let already_resolved = context2.resolve(candidate.id).unwrap();

        let outcome = genuine_reboot(
            &already_resolved,
            &candidate,
            &e2,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .expect("an already-resolved BootContext is a rejection, never a resolve conflict");

        assert!(matches!(outcome, RedeemOutcome::Rejected));

        // No fresh chain was established: the candidate's credential chain
        // (from the original first contact) is untouched by this rejected
        // attempt.
        assert_eq!(candidate.credential, pre_attempt_chain);
        assert_eq!(
            credential::authenticate(
                &candidate.credential,
                &e2,
                &fresh_runtime(),
                now(),
                DEFAULT_CREDENTIAL_TTL
            ),
            AuthOutcome::Rejected,
            "the rejected E2 must not have become a valid predecessor"
        );
    }

    #[test]
    fn genuine_reboot_rejects_when_current_chain_is_revoked() {
        let (context1, e1) = issue_boot_context("mac:AA:BB", now(), Duration::minutes(5));
        let RedeemOutcome::Established { outcome: first, .. } = first_contact(
            &context1,
            &e1,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .unwrap() else {
            panic!()
        };
        let revoked = revoke_credential(&first.endpoint, now());

        let (context2, e2) = issue_boot_context("mac:AA:BB", now(), Duration::minutes(5));
        let outcome = genuine_reboot(
            &context2,
            &revoked.endpoint,
            &e2,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .unwrap();
        assert!(matches!(outcome, RedeemOutcome::Rejected));
    }

    #[test]
    fn rejected_redemption_yields_no_transition_to_persist() {
        let (context, e1) = issue_boot_context("mac:AA:BB", now(), Duration::minutes(5));
        let RedeemOutcome::Established { outcome: first, .. } = first_contact(
            &context,
            &e1,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .unwrap() else {
            panic!()
        };

        let bogus = PresentedCredential::generate(CredentialKind::Runtime);
        assert!(matches!(
            redeem_known(
                &first.endpoint,
                &bogus,
                &fresh_runtime(),
                now(),
                DEFAULT_CREDENTIAL_TTL
            ),
            RedeemOutcome::Rejected
        ));
    }

    #[test]
    fn approve_enrollment_emits_enrolled_and_operator_decision_with_audit() {
        let (context, e1) = issue_boot_context("mac:AA:BB", now(), Duration::minutes(5));
        let RedeemOutcome::Established { outcome: first, .. } = first_contact(
            &context,
            &e1,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .unwrap() else {
            panic!()
        };

        let operator = Actor::Operator {
            label: "wp1-harness".into(),
        };
        let outcome =
            approve_enrollment(&first.endpoint, operator, now()).expect("valid transition");

        assert_eq!(outcome.endpoint.identity, IdentityState::Enrolled);
        assert_eq!(outcome.events.len(), 2);
        assert!(outcome
            .events
            .iter()
            .any(|e| e.event_type() == "EndpointEnrolled"));
        assert!(outcome
            .events
            .iter()
            .any(|e| e.event_type() == "OperatorDecisionRecorded"));
        assert!(outcome.audit.is_some());
    }

    #[test]
    fn approve_enrollment_rejects_already_enrolled() {
        let (context, e1) = issue_boot_context("mac:AA:BB", now(), Duration::minutes(5));
        let RedeemOutcome::Established { outcome: first, .. } = first_contact(
            &context,
            &e1,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .unwrap() else {
            panic!()
        };
        let operator = Actor::Operator {
            label: "wp1-harness".into(),
        };
        let enrolled = approve_enrollment(&first.endpoint, operator.clone(), now()).unwrap();

        assert!(approve_enrollment(&enrolled.endpoint, operator, now()).is_err());
    }

    #[test]
    fn revoke_invalidates_every_credential_in_chain_with_no_new_event() {
        let (context, e1) = issue_boot_context("mac:AA:BB", now(), Duration::minutes(5));
        let r1 = fresh_runtime();
        let RedeemOutcome::Established { outcome: first, .. } =
            first_contact(&context, &e1, &r1, now(), DEFAULT_CREDENTIAL_TTL).unwrap()
        else {
            panic!()
        };

        let revoked = revoke_credential(&first.endpoint, now());
        assert!(revoked.events.is_empty());

        assert!(matches!(
            redeem_known(
                &revoked.endpoint,
                &r1,
                &fresh_runtime(),
                now(),
                DEFAULT_CREDENTIAL_TTL
            ),
            RedeemOutcome::Rejected
        ));
    }

    #[test]
    fn promoted_predecessor_survives_boot_context_expiry() {
        // ADR-0014 point 6: after promotion, the predecessor is governed by
        // its own CredentialSlot expiry, not by BootContext.expires_at.
        let short_ttl = Duration::minutes(1);
        let (context, e1) = issue_boot_context("mac:AA:BB", now(), short_ttl);
        let long_credential_ttl = Duration::hours(1);
        let RedeemOutcome::Established { outcome: first, .. } =
            first_contact(&context, &e1, &fresh_runtime(), now(), long_credential_ttl).unwrap()
        else {
            panic!()
        };

        // Well past the BootContext's own short TTL, but within the
        // promoted predecessor's much longer credential TTL.
        let later = now() + Duration::minutes(30);
        let retry_successor = fresh_runtime();
        let outcome = redeem_known(
            &first.endpoint,
            &e1,
            &retry_successor,
            later,
            long_credential_ttl,
        );
        assert!(matches!(outcome, RedeemOutcome::Established { .. }));
    }

    #[test]
    fn first_contact_creates_current_boot_not_established() {
        let nonce = bamep_trusted_bootstrap::BootNonce::from_bytes([0x11; 32]);
        let (context, e1) =
            issue_boot_context_with_nonce("mac:AA:BB", nonce, now(), Duration::minutes(5));
        let RedeemOutcome::Established { outcome, .. } = first_contact(
            &context,
            &e1,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .unwrap() else {
            panic!("first contact must establish")
        };

        let current_boot = outcome
            .endpoint
            .current_boot
            .expect("first contact must set CurrentBoot");
        assert_eq!(current_boot.boot_context_id(), context.boot_context_id());
        assert_eq!(current_boot.boot_nonce(), nonce);
        assert_eq!(
            current_boot.trusted_bootstrap(),
            TrustedBootstrapState::NotEstablished
        );
    }

    #[test]
    fn genuine_reboot_replaces_current_boot_and_resets_trust_from_established() {
        let nonce_a = bamep_trusted_bootstrap::BootNonce::from_bytes([0x22; 32]);
        let (context1, e1) =
            issue_boot_context_with_nonce("mac:AA:BB", nonce_a, now(), Duration::minutes(5));
        let RedeemOutcome::Established { outcome: first, .. } = first_contact(
            &context1,
            &e1,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .unwrap() else {
            panic!()
        };

        // Test-only: simulate a previously Established server-side trusted-
        // bootstrap fact for the pre-reboot current boot. This is not the
        // production evidence-acceptance transition (not implemented yet in
        // this checkpoint) — it is a direct, narrowly scoped in-memory
        // construction, exercising only the reset behavior genuine_reboot
        // itself must perform.
        let established_before_reboot = EndpointAggregate {
            current_boot: Some(CurrentBoot::new(
                context1.boot_context_id().clone(),
                nonce_a,
                TrustedBootstrapState::Established,
            )),
            ..first.endpoint.clone()
        };

        let nonce_b = bamep_trusted_bootstrap::BootNonce::from_bytes([0x33; 32]);
        assert_ne!(nonce_a, nonce_b);
        let (context2, e2) =
            issue_boot_context_with_nonce("mac:AA:BB", nonce_b, now(), Duration::minutes(5));
        let RedeemOutcome::Established {
            outcome: rebooted, ..
        } = genuine_reboot(
            &context2,
            &established_before_reboot,
            &e2,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .unwrap()
        else {
            panic!("genuine reboot must establish a fresh chain")
        };

        let current_boot = rebooted
            .endpoint
            .current_boot
            .expect("genuine reboot must set CurrentBoot");
        assert_eq!(current_boot.boot_context_id(), context2.boot_context_id());
        assert_ne!(current_boot.boot_context_id(), context1.boot_context_id());
        assert_eq!(current_boot.boot_nonce(), nonce_b);
        assert_ne!(current_boot.boot_nonce(), nonce_a);
        assert_eq!(
            current_boot.trusted_bootstrap(),
            TrustedBootstrapState::NotEstablished,
            "genuine reboot must reset trusted bootstrap to NotEstablished even when the \
             previous CurrentBoot was Established"
        );
    }

    #[test]
    fn redeem_known_preserves_current_boot_exactly() {
        let (context, e1) = issue_boot_context("mac:AA:BB", now(), Duration::minutes(5));
        let r1 = fresh_runtime();
        let RedeemOutcome::Established { outcome: first, .. } =
            first_contact(&context, &e1, &r1, now(), DEFAULT_CREDENTIAL_TTL).unwrap()
        else {
            panic!()
        };
        let current_boot_before = first.endpoint.current_boot.clone();

        let RedeemOutcome::Established { outcome, .. } = redeem_known(
            &first.endpoint,
            &r1,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        ) else {
            panic!("R1 must authenticate")
        };

        assert_eq!(
            outcome.endpoint.current_boot, current_boot_before,
            "routine rotation must not reset or replace CurrentBoot"
        );
        assert_eq!(
            outcome
                .endpoint
                .current_boot
                .as_ref()
                .unwrap()
                .trusted_bootstrap(),
            TrustedBootstrapState::NotEstablished,
            "ordinary redemption must never infer Established from a successful reconnect"
        );
    }

    #[test]
    fn approve_enrollment_preserves_current_boot() {
        let (context, e1) = issue_boot_context("mac:AA:BB", now(), Duration::minutes(5));
        let RedeemOutcome::Established { outcome: first, .. } = first_contact(
            &context,
            &e1,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .unwrap() else {
            panic!()
        };
        let current_boot_before = first.endpoint.current_boot.clone();

        let operator = Actor::Operator {
            label: "wp1-harness".into(),
        };
        let approved = approve_enrollment(&first.endpoint, operator, now()).unwrap();

        assert_eq!(
            approved.endpoint.current_boot, current_boot_before,
            "operator approval must not disturb CurrentBoot"
        );
    }

    #[test]
    fn approve_enrollment_and_revoke_credential_preserve_hardware_confidence() {
        let (context, e1) = issue_boot_context("mac:AA:BB", now(), Duration::minutes(5));
        let RedeemOutcome::Established { outcome: first, .. } = first_contact(
            &context,
            &e1,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(
            first.endpoint.hardware_confidence,
            crate::HardwareConfidence::Consistent
        );

        let operator = Actor::Operator {
            label: "wp1-harness".into(),
        };
        let approved = approve_enrollment(&first.endpoint, operator, now()).unwrap();
        assert_eq!(
            approved.endpoint.hardware_confidence,
            crate::HardwareConfidence::Consistent,
            "operator approval must not disturb hardware confidence"
        );

        let revoked = revoke_credential(&approved.endpoint, now());
        assert_eq!(
            revoked.endpoint.hardware_confidence,
            crate::HardwareConfidence::Consistent,
            "credential revocation must not disturb hardware confidence"
        );
    }

    #[test]
    fn revoke_credential_preserves_current_boot() {
        let (context, e1) = issue_boot_context("mac:AA:BB", now(), Duration::minutes(5));
        let RedeemOutcome::Established { outcome: first, .. } = first_contact(
            &context,
            &e1,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .unwrap() else {
            panic!()
        };
        let current_boot_before = first.endpoint.current_boot.clone();

        let revoked = revoke_credential(&first.endpoint, now());

        assert_eq!(
            revoked.endpoint.current_boot, current_boot_before,
            "credential revocation must not disturb CurrentBoot"
        );
    }

    #[test]
    fn matching_current_boot_establishes_trust_and_is_idempotent() {
        let nonce = BootNonce::from_bytes([0x41; 32]);
        let (context, e1) =
            issue_boot_context_with_nonce("mac:trust", nonce, now(), Duration::minutes(5));
        let RedeemOutcome::Established { outcome: first, .. } = first_contact(
            &context,
            &e1,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .unwrap() else {
            panic!()
        };
        let TrustedBootstrapOutcome::Established(established) =
            establish_trusted_bootstrap(&first.endpoint, nonce)
        else {
            panic!()
        };
        assert_eq!(
            established
                .endpoint
                .current_boot
                .as_ref()
                .unwrap()
                .trusted_bootstrap(),
            TrustedBootstrapState::Established
        );
        let TrustedBootstrapOutcome::Established(repeated) =
            establish_trusted_bootstrap(&established.endpoint, nonce)
        else {
            panic!()
        };
        assert_eq!(repeated.endpoint, established.endpoint);
        assert!(repeated.events.is_empty());
        assert!(repeated.audit.is_none());
    }

    #[test]
    fn historical_nonce_is_rejected_without_mutation() {
        let nonce = BootNonce::from_bytes([0x42; 32]);
        let (context, e1) =
            issue_boot_context_with_nonce("mac:historical", nonce, now(), Duration::minutes(5));
        let RedeemOutcome::Established { outcome, .. } = first_contact(
            &context,
            &e1,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .unwrap() else {
            panic!()
        };
        assert!(matches!(
            establish_trusted_bootstrap(&outcome.endpoint, BootNonce::from_bytes([0x43; 32])),
            TrustedBootstrapOutcome::Rejected
        ));
        assert_eq!(
            outcome
                .endpoint
                .current_boot
                .as_ref()
                .unwrap()
                .trusted_bootstrap(),
            TrustedBootstrapState::NotEstablished
        );
    }

    #[test]
    fn missing_current_boot_is_rejected() {
        let nonce = BootNonce::from_bytes([0x44; 32]);
        let (context, e1) =
            issue_boot_context_with_nonce("mac:none", nonce, now(), Duration::minutes(5));
        let RedeemOutcome::Established { outcome, .. } = first_contact(
            &context,
            &e1,
            &fresh_runtime(),
            now(),
            DEFAULT_CREDENTIAL_TTL,
        )
        .unwrap() else {
            panic!()
        };
        let aggregate = EndpointAggregate {
            current_boot: None,
            ..outcome.endpoint
        };
        assert!(matches!(
            establish_trusted_bootstrap(&aggregate, nonce),
            TrustedBootstrapOutcome::Rejected
        ));
    }
}
