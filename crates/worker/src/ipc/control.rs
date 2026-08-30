//! The Worker-side concurrent Worker Protocol v1 control client (Issue #39
//! Phase E1): one reconnecting UDS connection, many concurrent outstanding
//! requests, exact `message_id`/`in_reply_to` correlation, generation-scoped
//! follow-up tickets, and fail-closed loss of authority
//! (`docs/specifications/m1-worker-data-plane-control-contract.md`;
//! ADR-0018).
//!
//! This is **not** a general RPC framework — it exposes exactly the five
//! authorizing/follow-up operation pairs the v1 catalog defines
//! ([`WorkerControlHandle::authorize_chunk`] + [`WorkerControlHandle::commit_chunk`],
//! [`WorkerControlHandle::discover_resume`], [`WorkerControlHandle::seal_manifest`] +
//! [`WorkerControlHandle::report_artifact_verification`]) and nothing more.
//! The wire stays `bamep-worker-protocol`; no arbitrary method names, no
//! reflection.
//!
//! Composition (Phase E2 will drive it alongside the HTTPS server):
//!
//! ```text
//! let (handle, driver) = worker_control(uds_path, reconnect_delay, request_timeout, worker_instance_id);
//! //   ^ cloneable request handle for HTTP request handlers
//! tokio::join!(driver.run(shutdown), run_https(handle, storage, reconstructor, tls));
//! ```
//!
//! [`ControlDriver`] owns the whole connect/handshake/read/write/reconnect
//! lifecycle in one task; [`WorkerControlHandle`] is a cheap clone that
//! submits requests against whatever connection generation is currently live.
//! E2 never touches a `UnixStream`.
//!
//! # No business authority
//!
//! Every operation reports only what `bamepd` decided. The Worker never
//! fabricates an `AuthorizationDecision`, `ChunkAcceptanceDecision`,
//! `ResumeDiscoveryPage`, `ManifestSealDecision`, or `ArtifactVerificationAck`
//! locally, never decides `Verified`/`Failed`, and never replays a business
//! request or a transient handle across a reconnect
//! (`m1-worker-data-plane-control-contract.md` "Authority", "Failure
//! semantics", "Transient operation handles").
//!
//! Unix Domain Sockets are Unix-only; the connection lifecycle lives behind
//! `#[cfg(unix)]` and never becomes available elsewhere — no TCP/localhost
//! substitute is introduced. On other platforms the driver never connects,
//! so the plumbing only the Unix connection task consumes is unused there.

#![cfg_attr(not(unix), allow(dead_code))]

use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use bamep_worker_protocol::{
    ArtifactVerificationAckMessage, ArtifactVerificationReportMessage,
    AuthorizationDecisionMessage, AuthorizationDecisionOutcome, AuthorizationQueryMessage,
    ChunkAcceptanceDecisionMessage, ChunkAcceptanceOutcome, ChunkAcceptanceRejectionReason,
    ChunkAcceptanceRequestMessage, HeldChunk, ManifestSealDecisionMessage, ManifestSealOutcome,
    ManifestSealRejectionReason, ManifestSealRequestMessage, ResumeDiscoveryContinueMessage,
    ResumeDiscoveryDecision, ResumeDiscoveryPageMessage, ResumeDiscoveryQueryMessage,
    WireArtifactStatus, WireDigestAlgorithm, WorkerProtocolMessage,
};
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

use super::authority::{AuthoritySnapshot, AuthorityTracker};

/// Fixed diagnostic placeholder for any secret/handle field in this module's
/// `Debug` impls (`m1-worker-data-plane-control-contract.md` "Security and
/// logging").
const REDACTED: &str = "REDACTED";

/// Conservative bound on how many control requests may be outstanding on one
/// connection generation at once. `bamepd`'s per-connection loop processes
/// requests sequentially, so this only needs to cover a pipelined burst plus
/// resume pagination; a new request past this fails closed
/// ([`ControlError::Saturated`]) rather than evicting a live waiter.
const DEFAULT_PENDING_CAPACITY: usize = 64;

// =====================================================================
// Local connection generation
// =====================================================================

/// A Worker-local connection generation. Monotonic and process-local; **never
/// on the wire**. A fresh value is minted on every successful handshake, and
/// every transient follow-up ticket carries the generation it was minted on
/// so a follow-up after reconnect is rejected locally before anything is sent
/// (`m1-worker-data-plane-control-contract.md` "Connection generations and
/// correlation").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LocalGeneration(u64);

impl LocalGeneration {
    pub fn get(self) -> u64 {
        self.0
    }
}

// =====================================================================
// Errors
// =====================================================================

/// Why a control operation could not produce an authoritative `bamepd`
/// answer. Every variant is fail-closed: the caller (Phase E2) maps all of
/// them to the same generic `401`-shaped HTTP response
/// (`m0-data-plane-and-storage-contracts.md`). Never a fabricated decision.
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    /// No live, handshaken `bamepd` connection is currently available.
    #[error("no live bamepd control connection is currently available")]
    NotConnected,
    /// The connection was lost (or its task ended) while this request was
    /// outstanding, or between the pages of a resume discovery. The outcome
    /// is uncertain and must never be treated as success
    /// (`m1-worker-data-plane-control-contract.md` "Disconnect with a request
    /// in flight").
    #[error("the bamepd control connection was lost while a control request was outstanding")]
    ConnectionLost,
    /// A generation-scoped follow-up ticket (`acceptance_handle` /
    /// `verification_handle` / a resume cursor) was presented after the
    /// connection generation changed. **Nothing was sent.** The caller must
    /// re-authorize with a fresh proof.
    #[error("the connection generation changed before this follow-up could be sent")]
    GenerationChanged,
    /// `bamepd` did not answer within the bounded control-request timeout.
    /// The pending request is dropped; a later answer for it is discarded as
    /// stale.
    #[error("bamepd did not answer this control request within {timeout_ms} ms")]
    Timeout { timeout_ms: u64 },
    /// Too many control requests are already outstanding on this connection
    /// generation. No live waiter is evicted.
    #[error("too many control requests are already outstanding")]
    Saturated,
    /// `bamepd`'s response could not be trusted as this request's answer — a
    /// valid `in_reply_to` carrying the wrong response type, an unexpected
    /// message type, or a malformed envelope. The connection generation is
    /// recycled.
    #[error("bamepd's response violated request/response correlation")]
    CorrelationViolation,
    /// `bamepd` returned a `ProtocolError`. Connection-level; the generation
    /// is recycled.
    #[error("bamepd returned a protocol error (code {code})")]
    ProtocolError { code: String },
    /// A resume-discovery continuation page could not be obtained (`bamepd`
    /// denied the cursor). Any partial held-chunk aggregate is discarded
    /// (`m1-worker-data-plane-control-contract.md` "Resume-discovery
    /// pagination"; "Resume-manifest pagination").
    #[error("a resume-discovery continuation page was unavailable")]
    ResumePageUnavailable,
}

// =====================================================================
// Operation inputs (already-parsed mechanical values Phase E2 lifts from HTTP)
// =====================================================================

/// Mechanical inputs for [`WorkerControlHandle::authorize_chunk`]. The Worker
/// forwards `token`/`proof_id`/`issued_at`/`signature` verbatim and never
/// parses the `token` (`m1-worker-data-plane-control-contract.md`
/// "Chunk-upload authorization").
#[derive(Clone)]
pub struct AuthorizeChunkInput {
    pub token: String,
    pub transfer_id: Uuid,
    pub chunk_index: u64,
    pub proof_id: String,
    pub issued_at: u64,
    pub signature: String,
}

/// Mechanical inputs for [`WorkerControlHandle::discover_resume`].
#[derive(Clone)]
pub struct ResumeDiscoveryInput {
    pub token: String,
    pub transfer_id: Uuid,
    pub proof_id: String,
    pub issued_at: u64,
    pub signature: String,
}

/// Mechanical inputs for [`WorkerControlHandle::seal_manifest`].
/// `chunk_count`/`artifact_digest` are Agent-declared from the HTTP body.
#[derive(Clone)]
pub struct ManifestSealInput {
    pub token: String,
    pub transfer_id: Uuid,
    pub proof_id: String,
    pub issued_at: u64,
    pub signature: String,
    pub chunk_count: u64,
    pub artifact_digest: String,
}

impl fmt::Debug for AuthorizeChunkInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizeChunkInput")
            .field("token", &REDACTED)
            .field("transfer_id", &self.transfer_id)
            .field("chunk_index", &self.chunk_index)
            .field("proof_id", &REDACTED)
            .field("issued_at", &REDACTED)
            .field("signature", &REDACTED)
            .finish()
    }
}

impl fmt::Debug for ResumeDiscoveryInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResumeDiscoveryInput")
            .field("token", &REDACTED)
            .field("transfer_id", &self.transfer_id)
            .field("proof_id", &REDACTED)
            .field("issued_at", &REDACTED)
            .field("signature", &REDACTED)
            .finish()
    }
}

impl fmt::Debug for ManifestSealInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManifestSealInput")
            .field("token", &REDACTED)
            .field("transfer_id", &self.transfer_id)
            .field("proof_id", &REDACTED)
            .field("issued_at", &REDACTED)
            .field("signature", &REDACTED)
            .field("chunk_count", &self.chunk_count)
            .field("artifact_digest", &self.artifact_digest)
            .finish()
    }
}

// =====================================================================
// Operation results
// =====================================================================

/// The outcome of [`WorkerControlHandle::authorize_chunk`].
#[derive(Debug)]
pub enum ChunkAuthorization {
    Approved(ChunkAuthorizationApproved),
    Denied,
}

/// An approved `chunk_upload` authorization. `digest_algorithm`/`chunk_size`
/// are the authoritative durable manifest facts the Worker MUST use, never a
/// local constant.
#[derive(Debug)]
pub struct ChunkAuthorizationApproved {
    pub digest_algorithm: WireDigestAlgorithm,
    pub chunk_size: u32,
    /// Present only when `chunk_index` is already durable: its recorded
    /// expected digest (canonical base64url-no-pad). An integrity identity,
    /// not a secret.
    pub expected_chunk_digest: Option<String>,
    /// The generation-scoped follow-up ticket. Pass the whole value into
    /// [`WorkerControlHandle::commit_chunk`]; it carries the opaque
    /// `acceptance_handle`, the bound `(transfer_id, chunk_index)`, and the
    /// local generation.
    pub acceptance_ticket: AcceptanceTicket,
}

/// Opaque generation-scoped ticket bound to one authorized `chunk_upload`.
/// The caller never inspects it; it echoes back to `bamepd` via
/// [`WorkerControlHandle::commit_chunk`].
pub struct AcceptanceTicket {
    generation: LocalGeneration,
    handle: String,
    transfer_id: Uuid,
    chunk_index: u64,
}

impl AcceptanceTicket {
    /// The connection generation this ticket is bound to (diagnostics only).
    pub fn generation(&self) -> LocalGeneration {
        self.generation
    }
}

impl fmt::Debug for AcceptanceTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcceptanceTicket")
            .field("generation", &self.generation)
            .field("handle", &REDACTED)
            .field("transfer_id", &self.transfer_id)
            .field("chunk_index", &self.chunk_index)
            .finish()
    }
}

/// The outcome of [`WorkerControlHandle::commit_chunk`] — `bamepd`'s
/// authoritative durable acceptance decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkAcceptance {
    Committed,
    AlreadyCommitted,
    Rejected(ChunkAcceptanceRejectionReason),
}

/// The outcome of [`WorkerControlHandle::discover_resume`].
#[derive(Debug)]
pub enum ResumeDiscovery {
    Approved(ResumeAggregate),
    Denied,
}

/// The complete, in-order aggregate of every `ResumeDiscoveryPage` for one
/// authorized resume query. No cursor escapes this value
/// (`m1-worker-data-plane-control-contract.md` "Resume-manifest
/// pagination"). `expected_chunk_count` is `Some` iff `sealed` (guaranteed by
/// the protocol's own first-page shape validation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeAggregate {
    pub transfer_id: Uuid,
    pub sealed: bool,
    pub digest_algorithm: WireDigestAlgorithm,
    pub chunk_size: u32,
    pub expected_chunk_count: Option<u64>,
    pub held_chunks: Vec<HeldChunk>,
}

/// The outcome of [`WorkerControlHandle::seal_manifest`].
#[derive(Debug)]
pub enum ManifestSeal {
    Sealed(SealSuccess),
    AlreadyPendingVerification(SealSuccess),
    Rejected(ManifestSealRejectionReason),
    Denied,
}

/// A committed seal (`sealed` or `already_pending_verification`). The
/// authoritative durable sealed values `bamepd` returned — the Worker
/// verifies against `expected_artifact_digest`/`chunk_count`, never against
/// what it sent. `expected_artifact_digest` is **not** compared locally.
#[derive(Debug)]
pub struct SealSuccess {
    pub artifact_id: Uuid,
    pub digest_algorithm: WireDigestAlgorithm,
    pub chunk_size: u32,
    pub chunk_count: u64,
    pub expected_artifact_digest: String,
    /// Generation-scoped follow-up ticket for
    /// [`WorkerControlHandle::report_artifact_verification`].
    pub verification_ticket: VerificationTicket,
}

/// Opaque generation-scoped ticket bound to one just-committed
/// `Incomplete -> PendingVerification`.
pub struct VerificationTicket {
    generation: LocalGeneration,
    handle: String,
}

impl VerificationTicket {
    /// The connection generation this ticket is bound to (diagnostics only).
    pub fn generation(&self) -> LocalGeneration {
        self.generation
    }
}

impl fmt::Debug for VerificationTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VerificationTicket")
            .field("generation", &self.generation)
            .field("handle", &REDACTED)
            .finish()
    }
}

/// The outcome of [`WorkerControlHandle::report_artifact_verification`] — the
/// authoritative committed `artifact_status` from `bamepd`'s own comparison
/// against its durable expected digest. The Worker never establishes this by
/// assertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactVerification {
    Verified,
    Failed,
}

// =====================================================================
// Internal request/response plumbing
// =====================================================================

/// Which response variant is the only valid answer for a given request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpectedResponse {
    Authorization,
    ChunkAcceptance,
    ResumePage,
    ManifestSeal,
    ArtifactVerification,
}

/// A correlated, type-checked response the dispatcher hands back to a waiter.
enum ResponsePayload {
    Authorization(AuthorizationDecisionMessage),
    ChunkAcceptance(ChunkAcceptanceDecisionMessage),
    ResumePage(ResumeDiscoveryPageMessage),
    ManifestSeal(ManifestSealDecisionMessage),
    ArtifactVerification(ArtifactVerificationAckMessage),
}

impl ResponsePayload {
    fn matches(&self, expected: ExpectedResponse) -> bool {
        matches!(
            (self, expected),
            (
                ResponsePayload::Authorization(_),
                ExpectedResponse::Authorization
            ) | (
                ResponsePayload::ChunkAcceptance(_),
                ExpectedResponse::ChunkAcceptance
            ) | (ResponsePayload::ResumePage(_), ExpectedResponse::ResumePage)
                | (
                    ResponsePayload::ManifestSeal(_),
                    ExpectedResponse::ManifestSeal
                )
                | (
                    ResponsePayload::ArtifactVerification(_),
                    ExpectedResponse::ArtifactVerification
                )
        )
    }
}

/// One request the dispatcher must send and correlate a reply to.
struct OutboundRequest {
    message: WorkerProtocolMessage,
    message_id: Uuid,
    expected: ExpectedResponse,
    reply: oneshot::Sender<Result<ResponsePayload, ControlError>>,
}

/// Published per generation: the mpsc into the dispatcher plus the generation
/// it belongs to. A caller compares a follow-up ticket's generation against
/// `generation` before enqueueing.
#[derive(Clone)]
struct GenerationChannel {
    generation: LocalGeneration,
    requests: mpsc::Sender<OutboundRequest>,
}

// =====================================================================
// Public handle
// =====================================================================

/// Cheap-to-clone handle Phase E2's HTTP request handlers use to run Worker
/// Protocol v1 control operations against whatever connection generation is
/// currently live. Every clone observes the same currently-published
/// generation; when the connection is lost every clone fails closed.
#[derive(Clone)]
pub struct WorkerControlHandle {
    current: watch::Receiver<Option<GenerationChannel>>,
    authority: watch::Receiver<AuthoritySnapshot>,
}

impl WorkerControlHandle {
    /// Transport health only — **not** authorization. `true` iff a
    /// current-generation handshake has completed and the connection has not
    /// since been lost.
    pub fn is_ready(&self) -> bool {
        self.authority.borrow().is_available()
    }

    /// The current connection generation, or `None` when not connected.
    pub fn current_generation(&self) -> Option<LocalGeneration> {
        let snapshot = *self.authority.borrow();
        snapshot
            .is_available()
            .then_some(LocalGeneration(snapshot.generation))
    }

    /// A watch receiver for transport-health changes (future E2 readiness
    /// gating / diagnostics). Never a source of authorization.
    pub fn authority(&self) -> watch::Receiver<AuthoritySnapshot> {
        self.authority.clone()
    }

    // ---- authorizing requests ----

    /// `AuthorizationQuery` -> `AuthorizationDecision` for `PUT .../chunks/{n}`.
    pub async fn authorize_chunk(
        &self,
        input: AuthorizeChunkInput,
    ) -> Result<ChunkAuthorization, ControlError> {
        let transfer_id = input.transfer_id;
        let chunk_index = input.chunk_index;
        let message = AuthorizationQueryMessage::new(
            input.token,
            transfer_id,
            chunk_index,
            input.proof_id,
            input.issued_at,
            input.signature,
        );
        let message_id = message.envelope.message_id;

        let (generation, payload) = self
            .send_request(message.into(), message_id, ExpectedResponse::Authorization)
            .await?;
        let ResponsePayload::Authorization(decision) = payload else {
            return Err(ControlError::CorrelationViolation);
        };

        match decision.body.decision {
            AuthorizationDecisionOutcome::Denied => Ok(ChunkAuthorization::Denied),
            AuthorizationDecisionOutcome::Approved => {
                let digest_algorithm = decision
                    .body
                    .digest_algorithm
                    .ok_or(ControlError::CorrelationViolation)?;
                let chunk_size = decision
                    .body
                    .chunk_size
                    .ok_or(ControlError::CorrelationViolation)?;
                let handle = decision
                    .body
                    .acceptance_handle
                    .ok_or(ControlError::CorrelationViolation)?;
                Ok(ChunkAuthorization::Approved(ChunkAuthorizationApproved {
                    digest_algorithm,
                    chunk_size,
                    expected_chunk_digest: decision.body.expected_chunk_digest,
                    acceptance_ticket: AcceptanceTicket {
                        generation,
                        handle,
                        transfer_id,
                        chunk_index,
                    },
                }))
            }
        }
    }

    /// `ResumeDiscoveryQuery` + zero or more `ResumeDiscoveryContinue` for
    /// `GET .../chunks`. Owns all UDS pagination: E2 receives one normalized
    /// aggregate and never sees a cursor. Any page that cannot be obtained
    /// (`denied` continuation, timeout, disconnect, generation change,
    /// correlation violation) discards the partial aggregate and fails closed.
    pub async fn discover_resume(
        &self,
        input: ResumeDiscoveryInput,
    ) -> Result<ResumeDiscovery, ControlError> {
        let query = ResumeDiscoveryQueryMessage::new(
            input.token,
            input.transfer_id,
            input.proof_id,
            input.issued_at,
            input.signature,
        );
        let query_id = query.envelope.message_id;

        let (generation, payload) = self
            .send_request(query.into(), query_id, ExpectedResponse::ResumePage)
            .await?;
        let ResponsePayload::ResumePage(first) = payload else {
            return Err(ControlError::CorrelationViolation);
        };
        if first.body.decision == ResumeDiscoveryDecision::Denied {
            return Ok(ResumeDiscovery::Denied);
        }
        let first = first
            .approved_first_page()
            .map_err(|_| ControlError::CorrelationViolation)?;

        let mut held_chunks = first.held_chunks;
        let mut cursor = first.resume_cursor;
        while let Some(next_cursor) = cursor.take() {
            let cont = ResumeDiscoveryContinueMessage::new(next_cursor);
            let cont_id = cont.envelope.message_id;
            // The continuation is bound to the one authorized query on this
            // generation — reject locally (nothing sent) if it changed.
            let (_generation, payload) = self
                .send_scoped_request(
                    generation,
                    cont.into(),
                    cont_id,
                    ExpectedResponse::ResumePage,
                )
                .await?;
            let ResponsePayload::ResumePage(page) = payload else {
                return Err(ControlError::CorrelationViolation);
            };
            if page.body.decision == ResumeDiscoveryDecision::Denied {
                return Err(ControlError::ResumePageUnavailable);
            }
            let page = page
                .approved_continuation_page()
                .map_err(|_| ControlError::CorrelationViolation)?;
            held_chunks.extend(page.held_chunks);
            cursor = page.resume_cursor;
        }

        Ok(ResumeDiscovery::Approved(ResumeAggregate {
            transfer_id: first.transfer_id,
            sealed: first.sealed,
            digest_algorithm: first.digest_algorithm,
            chunk_size: first.chunk_size,
            expected_chunk_count: first.expected_chunk_count,
            held_chunks,
        }))
    }

    /// `ManifestSealRequest` -> `ManifestSealDecision` for `POST .../seal`.
    pub async fn seal_manifest(
        &self,
        input: ManifestSealInput,
    ) -> Result<ManifestSeal, ControlError> {
        let request = ManifestSealRequestMessage::new(
            input.token,
            input.transfer_id,
            input.proof_id,
            input.issued_at,
            input.signature,
            input.chunk_count,
            input.artifact_digest,
        );
        let request_id = request.envelope.message_id;

        let (generation, payload) = self
            .send_request(request.into(), request_id, ExpectedResponse::ManifestSeal)
            .await?;
        let ResponsePayload::ManifestSeal(decision) = payload else {
            return Err(ControlError::CorrelationViolation);
        };

        match decision.body.outcome {
            ManifestSealOutcome::Denied => Ok(ManifestSeal::Denied),
            ManifestSealOutcome::Rejected => Ok(ManifestSeal::Rejected(
                decision
                    .body
                    .reason
                    .ok_or(ControlError::CorrelationViolation)?,
            )),
            ManifestSealOutcome::Sealed | ManifestSealOutcome::AlreadyPendingVerification => {
                let success = SealSuccess {
                    artifact_id: decision
                        .body
                        .artifact_id
                        .ok_or(ControlError::CorrelationViolation)?,
                    digest_algorithm: decision
                        .body
                        .digest_algorithm
                        .ok_or(ControlError::CorrelationViolation)?,
                    chunk_size: decision
                        .body
                        .chunk_size
                        .ok_or(ControlError::CorrelationViolation)?,
                    chunk_count: decision
                        .body
                        .chunk_count
                        .ok_or(ControlError::CorrelationViolation)?,
                    expected_artifact_digest: decision
                        .body
                        .expected_artifact_digest
                        .clone()
                        .ok_or(ControlError::CorrelationViolation)?,
                    verification_ticket: VerificationTicket {
                        generation,
                        handle: decision
                            .body
                            .verification_handle
                            .clone()
                            .ok_or(ControlError::CorrelationViolation)?,
                    },
                };
                Ok(match decision.body.outcome {
                    ManifestSealOutcome::Sealed => ManifestSeal::Sealed(success),
                    _ => ManifestSeal::AlreadyPendingVerification(success),
                })
            }
        }
    }

    // ---- generation-scoped follow-ups ----

    /// `ChunkAcceptanceRequest` -> `ChunkAcceptanceDecision`, the durable
    /// acceptance step. Rejected locally with
    /// [`ControlError::GenerationChanged`] — **nothing sent** — if the
    /// generation changed since the ticket was minted. `bamepd` still
    /// independently enforces generation scope. `digest`/`size` are the
    /// Worker-verified actual chunk digest and exact received byte count.
    pub async fn commit_chunk(
        &self,
        ticket: AcceptanceTicket,
        digest: String,
        size: u32,
    ) -> Result<ChunkAcceptance, ControlError> {
        let request = ChunkAcceptanceRequestMessage::new(
            ticket.handle,
            ticket.transfer_id,
            ticket.chunk_index,
            digest,
            size,
        );
        let request_id = request.envelope.message_id;

        let (_generation, payload) = self
            .send_scoped_request(
                ticket.generation,
                request.into(),
                request_id,
                ExpectedResponse::ChunkAcceptance,
            )
            .await?;
        let ResponsePayload::ChunkAcceptance(decision) = payload else {
            return Err(ControlError::CorrelationViolation);
        };

        Ok(match decision.body.outcome {
            ChunkAcceptanceOutcome::Committed => ChunkAcceptance::Committed,
            ChunkAcceptanceOutcome::AlreadyCommitted => ChunkAcceptance::AlreadyCommitted,
            ChunkAcceptanceOutcome::Rejected => ChunkAcceptance::Rejected(
                decision
                    .body
                    .reason
                    .ok_or(ControlError::CorrelationViolation)?,
            ),
        })
    }

    /// `ArtifactVerificationReport` -> `ArtifactVerificationAck`. Sends only
    /// `verification_handle` + `computed_artifact_digest` (the wire message
    /// carries nothing else — no `transfer_id`, `artifact_id`, or verdict).
    /// Rejected locally with [`ControlError::GenerationChanged`] — nothing
    /// sent — if the generation changed. Returns `bamepd`'s authoritative
    /// committed status; the Worker never decides it and never turns a
    /// different computed digest into `Failed` locally.
    pub async fn report_artifact_verification(
        &self,
        ticket: VerificationTicket,
        computed_artifact_digest: String,
    ) -> Result<ArtifactVerification, ControlError> {
        let report =
            ArtifactVerificationReportMessage::new(ticket.handle, computed_artifact_digest);
        let report_id = report.envelope.message_id;

        let (_generation, payload) = self
            .send_scoped_request(
                ticket.generation,
                report.into(),
                report_id,
                ExpectedResponse::ArtifactVerification,
            )
            .await?;
        let ResponsePayload::ArtifactVerification(ack) = payload else {
            return Err(ControlError::CorrelationViolation);
        };

        Ok(match ack.body.artifact_status {
            WireArtifactStatus::Verified => ArtifactVerification::Verified,
            WireArtifactStatus::Failed => ArtifactVerification::Failed,
        })
    }

    // ---- internals ----

    async fn send_request(
        &self,
        message: WorkerProtocolMessage,
        message_id: Uuid,
        expected: ExpectedResponse,
    ) -> Result<(LocalGeneration, ResponsePayload), ControlError> {
        let channel = self
            .current
            .borrow()
            .clone()
            .ok_or(ControlError::NotConnected)?;
        dispatch(channel, message, message_id, expected).await
    }

    async fn send_scoped_request(
        &self,
        ticket_generation: LocalGeneration,
        message: WorkerProtocolMessage,
        message_id: Uuid,
        expected: ExpectedResponse,
    ) -> Result<(LocalGeneration, ResponsePayload), ControlError> {
        let channel = self
            .current
            .borrow()
            .clone()
            .ok_or(ControlError::NotConnected)?;
        // The channel object *is* the generation: comparing against it, then
        // sending on it, is one linearized step — a generation flip after the
        // `borrow()` leaves `channel` pointing at a dead dispatcher whose
        // `send` fails, never at a fresh connection carrying a stale ticket
        // (`m1-worker-data-plane-control-contract.md` "Connection generations
        // and correlation").
        if channel.generation != ticket_generation {
            return Err(ControlError::GenerationChanged);
        }
        dispatch(channel, message, message_id, expected).await
    }
}

async fn dispatch(
    channel: GenerationChannel,
    message: WorkerProtocolMessage,
    message_id: Uuid,
    expected: ExpectedResponse,
) -> Result<(LocalGeneration, ResponsePayload), ControlError> {
    let generation = channel.generation;
    let (reply_tx, reply_rx) = oneshot::channel();
    channel
        .requests
        .send(OutboundRequest {
            message,
            message_id,
            expected,
            reply: reply_tx,
        })
        .await
        .map_err(|_| ControlError::NotConnected)?;
    let payload = reply_rx
        .await
        .unwrap_or(Err(ControlError::ConnectionLost))?;
    Ok((generation, payload))
}

// =====================================================================
// Driver
// =====================================================================

/// Owns the connect / handshake / dispatch / reconnect lifecycle in one task.
/// The composition root [`ControlDriver::run`]s it, concurrently with the
/// future HTTPS server.
pub struct ControlDriver {
    uds_path: PathBuf,
    reconnect_delay: Duration,
    request_timeout: Duration,
    worker_instance_id: Uuid,
    pending_capacity: usize,
    tracker: AuthorityTracker,
    publisher: watch::Sender<Option<GenerationChannel>>,
}

impl ControlDriver {
    /// Override the pending-request capacity (tests only need this; production
    /// uses the module default).
    pub fn with_pending_capacity(mut self, capacity: usize) -> Self {
        self.pending_capacity = capacity.max(1);
        self
    }

    /// Runs forever — connect, handshake, dispatch, wait for disconnect,
    /// sleep the bounded reconnect delay, repeat — until `shutdown` resolves.
    /// On shutdown (or on being dropped) every pending waiter fails closed,
    /// no reconnect is attempted, and no task is left detached. Never replays
    /// a business request across a reconnect.
    pub async fn run(self, shutdown: impl Future<Output = ()>) {
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                _ = self.run_one_connection() => {}
            }
            tokio::select! {
                _ = &mut shutdown => break,
                _ = tokio::time::sleep(self.reconnect_delay) => {}
            }
        }
        let _ = self.publisher.send(None);
        self.tracker.set_disconnected();
    }
}

#[cfg(unix)]
mod imp {
    use std::collections::HashMap;

    use bamep_worker_protocol::{receive, send, WorkerHelloMessage};
    use tokio::net::UnixStream;
    use tokio::time::Instant as TokioInstant;

    use super::*;

    #[derive(Debug, thiserror::Error)]
    enum HandshakeError {
        #[error("transport error sending WorkerHello")]
        Send(#[from] bamep_worker_protocol::SendError),
        #[error("transport error awaiting bamepd's handshake response")]
        Receive(#[from] bamep_worker_protocol::ReceiveError),
        #[error("bamepd rejected the handshake as incompatible")]
        Rejected,
        #[error("bamepd's ServerHello failed normative envelope/field/correlation validation")]
        InvalidServerHello,
        #[error("bamepd's HandshakeRejected failed normative envelope/correlation validation")]
        InvalidHandshakeRejected,
        #[error("bamepd sent an unexpected message before the handshake completed")]
        UnexpectedMessage,
    }

    /// `WorkerHello` -> `ServerHello{compatible: true}` before any business
    /// request may be sent (`m1-worker-data-plane-control-contract.md`
    /// "Handshake"). Every normative field of the response is validated;
    /// Worker never proceeds on a malformed or uncorrelated reply.
    /// `worker_instance_id` is the caller's process-lifetime identity,
    /// unchanged across reconnects — only the local connection generation
    /// advances.
    async fn perform_handshake(
        mut stream: UnixStream,
        worker_instance_id: Uuid,
    ) -> Result<UnixStream, HandshakeError> {
        let hello = WorkerHelloMessage::new(worker_instance_id);
        let sent_id = hello.envelope.message_id;
        send(&mut stream, &WorkerProtocolMessage::WorkerHello(hello)).await?;

        match receive(&mut stream).await? {
            WorkerProtocolMessage::ServerHello(response) => {
                if response.is_valid_reply_to(sent_id) {
                    Ok(stream)
                } else {
                    Err(HandshakeError::InvalidServerHello)
                }
            }
            WorkerProtocolMessage::HandshakeRejected(response) => {
                if response.is_valid_reply_to(sent_id) {
                    Err(HandshakeError::Rejected)
                } else {
                    Err(HandshakeError::InvalidHandshakeRejected)
                }
            }
            _ => Err(HandshakeError::UnexpectedMessage),
        }
    }

    enum Inbound {
        Response {
            in_reply_to: Uuid,
            payload: ResponsePayload,
        },
        ProtocolError {
            in_reply_to: Option<Uuid>,
            code: String,
        },
        /// A message type `bamepd` must not send on this boundary after the
        /// handshake, or one with a malformed envelope — a protocol violation
        /// that recycles the connection generation.
        Unexpected,
    }

    fn classify_inbound(message: WorkerProtocolMessage) -> Inbound {
        use WorkerProtocolMessage as M;

        // A response whose envelope is not normatively valid is a malformed
        // message, never this generation's answer.
        let envelope_valid = message.envelope().is_valid();

        match message {
            M::AuthorizationDecision(m) if envelope_valid => Inbound::Response {
                in_reply_to: m.body.in_reply_to,
                payload: ResponsePayload::Authorization(m),
            },
            M::ChunkAcceptanceDecision(m) if envelope_valid => Inbound::Response {
                in_reply_to: m.body.in_reply_to,
                payload: ResponsePayload::ChunkAcceptance(m),
            },
            M::ResumeDiscoveryPage(m) if envelope_valid => Inbound::Response {
                in_reply_to: m.body.in_reply_to,
                payload: ResponsePayload::ResumePage(m),
            },
            M::ManifestSealDecision(m) if envelope_valid => Inbound::Response {
                in_reply_to: m.body.in_reply_to,
                payload: ResponsePayload::ManifestSeal(m),
            },
            M::ArtifactVerificationAck(m) if envelope_valid => Inbound::Response {
                in_reply_to: m.body.in_reply_to,
                payload: ResponsePayload::ArtifactVerification(m),
            },
            M::ProtocolError(m) => Inbound::ProtocolError {
                in_reply_to: m.body.in_reply_to,
                code: m.body.code,
            },
            _ => Inbound::Unexpected,
        }
    }

    /// Aborts its wrapped task on drop, so a connection's reader task never
    /// outlives that connection ("no detached immortal tasks").
    struct AbortOnDrop(tokio::task::JoinHandle<()>);

    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    async fn sleep_until_opt(deadline: Option<TokioInstant>) {
        match deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending::<()>().await,
        }
    }

    /// A pending entry in the current generation's correlation table.
    struct PendingEntry {
        expected: ExpectedResponse,
        reply: oneshot::Sender<Result<ResponsePayload, ControlError>>,
        deadline: TokioInstant,
    }

    impl ControlDriver {
        /// One connection lifetime. Returns when the connection ends for any
        /// reason (connect failure, rejected/malformed handshake, disconnect,
        /// correlation violation, protocol error).
        pub(super) async fn run_one_connection(&self) {
            self.tracker.set_connecting();
            let stream = match UnixStream::connect(&self.uds_path).await {
                Ok(stream) => stream,
                Err(_) => {
                    self.tracker.set_disconnected();
                    return;
                }
            };

            self.tracker.set_handshaking();
            let stream = match perform_handshake(stream, self.worker_instance_id).await {
                Ok(stream) => stream,
                Err(_) => {
                    self.tracker.set_disconnected();
                    return;
                }
            };

            let generation = LocalGeneration(self.tracker.set_ready());
            let (mut read_half, mut write_half) = stream.into_split();

            let (request_tx, mut request_rx) =
                mpsc::channel::<OutboundRequest>(self.pending_capacity);
            let (inbound_tx, mut inbound_rx) =
                mpsc::channel::<WorkerProtocolMessage>(self.pending_capacity);

            // Exactly one framed reader for this connection: a dedicated task
            // that loops `receive` and forwards decoded messages. Aborted on
            // every exit path via `_reader`'s `Drop` — never detached.
            let _reader = AbortOnDrop(tokio::spawn(async move {
                while let Ok(message) = receive(&mut read_half).await {
                    if inbound_tx.send(message).await.is_err() {
                        break;
                    }
                }
            }));

            let _ = self.publisher.send(Some(GenerationChannel {
                generation,
                requests: request_tx,
            }));

            // Exactly one framed writer for this connection: `send` is only
            // ever called here, and only from the single request branch.
            let mut pending: HashMap<Uuid, PendingEntry> = HashMap::new();
            let timeout_ms = self.request_timeout.as_millis() as u64;

            'dispatch: loop {
                let next_deadline = pending.values().map(|entry| entry.deadline).min();
                tokio::select! {
                    maybe_request = request_rx.recv() => {
                        let Some(request) = maybe_request else { break 'dispatch; };
                        if pending.len() >= self.pending_capacity {
                            let _ = request.reply.send(Err(ControlError::Saturated));
                            continue;
                        }
                        if send(&mut write_half, &request.message).await.is_err() {
                            let _ = request.reply.send(Err(ControlError::ConnectionLost));
                            break 'dispatch;
                        }
                        pending.insert(
                            request.message_id,
                            PendingEntry {
                                expected: request.expected,
                                reply: request.reply,
                                deadline: TokioInstant::now() + self.request_timeout,
                            },
                        );
                    }
                    maybe_message = inbound_rx.recv() => {
                        let Some(message) = maybe_message else { break 'dispatch; };
                        match classify_inbound(message) {
                            Inbound::Response { in_reply_to, payload } => {
                                // Unknown / stale `in_reply_to` (including a
                                // response for a prior generation) matches no
                                // entry — discarded, never applied to any state.
                                if let Some(entry) = pending.remove(&in_reply_to) {
                                    if payload.matches(entry.expected) {
                                        let _ = entry.reply.send(Ok(payload));
                                    } else {
                                        let _ = entry
                                            .reply
                                            .send(Err(ControlError::CorrelationViolation));
                                        break 'dispatch;
                                    }
                                }
                            }
                            Inbound::ProtocolError { in_reply_to, code } => {
                                if let Some(entry) = in_reply_to.and_then(|id| pending.remove(&id)) {
                                    let _ = entry.reply.send(Err(ControlError::ProtocolError { code }));
                                }
                                break 'dispatch;
                            }
                            Inbound::Unexpected => break 'dispatch,
                        }
                    }
                    _ = sleep_until_opt(next_deadline) => {
                        let now = TokioInstant::now();
                        let expired: Vec<Uuid> = pending
                            .iter()
                            .filter(|(_, entry)| entry.deadline <= now)
                            .map(|(id, _)| *id)
                            .collect();
                        for id in expired {
                            if let Some(entry) = pending.remove(&id) {
                                let _ = entry.reply.send(Err(ControlError::Timeout { timeout_ms }));
                            }
                        }
                    }
                }
            }

            // Connection ended: stop admitting, fail every remaining waiter,
            // drop the reader (via `_reader`'s `Drop`), never carry pending
            // into the next generation.
            let _ = self.publisher.send(None);
            drop(request_rx);
            drop(inbound_rx);
            for (_, entry) in pending.drain() {
                let _ = entry.reply.send(Err(ControlError::ConnectionLost));
            }
            self.tracker.set_disconnected();
        }
    }
}

#[cfg(not(unix))]
impl ControlDriver {
    /// Non-Unix portability stub: no UDS, so no connection is ever
    /// established. Linux is the Worker reference/production environment
    /// (`docs/development/testing.md`).
    async fn run_one_connection(&self) {
        self.tracker.set_disconnected();
        let _ = self.publisher.send(None);
        std::future::pending::<()>().await
    }
}

// =====================================================================
// Constructor
// =====================================================================

/// Builds the control client. Spawns nothing itself — the caller runs
/// [`ControlDriver::run`] (concurrently with the future HTTPS server) and
/// hands [`WorkerControlHandle`] clones to request handlers.
pub fn worker_control(
    uds_path: PathBuf,
    reconnect_delay: Duration,
    request_timeout: Duration,
    worker_instance_id: Uuid,
) -> (WorkerControlHandle, ControlDriver) {
    let (tracker, authority_rx) = AuthorityTracker::new();
    let (publisher, current_rx) = watch::channel::<Option<GenerationChannel>>(None);

    let handle = WorkerControlHandle {
        current: current_rx,
        authority: authority_rx,
    };
    let driver = ControlDriver {
        uds_path,
        reconnect_delay,
        request_timeout,
        worker_instance_id,
        pending_capacity: DEFAULT_PENDING_CAPACITY,
        tracker,
        publisher,
    };
    (handle, driver)
}

#[cfg(all(test, unix))]
mod tests;
