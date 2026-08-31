//! Agent-side `bamep.m1.data-plane-transfer` participant
//! (`docs/specifications/m1-simulated-vertical-slice-and-baseline-validation.md`
//! RF-005; Issue #19 checkpoint C1).
//!
//! This is a **distinct** participant from the RF-004
//! `bamep.m1.simulated-execution` state machine
//! ([`crate::action::SimulatedActionAgent`]) — that action explicitly has no
//! data-plane transfer, and C1 deliberately does not thread transfer behaviour
//! through it with conditionals.
//!
//! C1 owns, mechanically and without any network control-plane composition:
//!
//! ```text
//! ActionDispatch parameters  ->  parse + ActionAck
//!   ->  (caller supplies an already-obtained AgentTransferAuthorization)
//!   ->  real HTTPS resume-discovery / chunk upload / seal
//!         (crate::data_plane::DataPlaneClient)
//!   ->  Agent-side ActionProgress (bytes_processed = durably-held bytes)
//!   ->  Agent-side terminal ActionResult construction
//! ```
//!
//! C1 does **not**: open a WSS session; send/receive `TransferAuthorizationRequest`
//! / `TransferAuthorizationGrant`; transmit any Agent Protocol message on the
//! network; mutate Attempt / JobStep / Job state; decide that `bamepd` accepted
//! the `ActionResult`. `ActionResult` construction here is a local artefact the
//! future C3 WSS integration will actually send, and C2 will consume.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use bamep_agent_protocol::{
    ActionAckError, ActionAckMessage, ActionDispatchMessage, ActionProgressMessage,
    ActionResultMessage, ActionResultOutcome, ProtocolId,
};
use bamep_trusted_bootstrap::ServerCertFingerprint;

use crate::data_plane::{
    DataPlaneClient, DataPlaneClientError, PutChunkOutcome, ResumeOutcome, SealArtifactStatus,
    SealOutcome,
};
use crate::transfer_authorization::{
    AgentTransferAuthorization, ProofError, TransferDirection, TransferOperation,
};
use crate::transfer_source::TransferSource;

/// The `action_type` / `action_version` this participant serves
/// (`m1-simulated-vertical-slice-and-baseline-validation.md` RF-005). Kept in
/// sync with `bamep_server::application::M1_DATA_PLANE_TRANSFER_ACTION_TYPE`
/// by the wire contract itself (ADR-0003), not shared Rust code.
pub const M1_DATA_PLANE_TRANSFER_ACTION_TYPE: &str = "bamep.m1.data-plane-transfer";
pub const M1_DATA_PLANE_TRANSFER_ACTION_VERSION: &str = "1";

/// The closed `digest_algorithm` vocabulary — exactly one M1 v1 value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDigestAlgorithm {
    Sha256,
}

/// The exact RF-005 `bamep.m1.data-plane-transfer` v1 `parameters` object,
/// parsed from an `ActionDispatch` — the single channel through which the
/// Agent learns `transfer_id`/`artifact_id`
/// (`m1-simulated-vertical-slice-and-baseline-validation.md` RF-005).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataPlaneTransferParams {
    pub transfer_id: Uuid,
    pub artifact_id: Uuid,
    pub direction: TransferDirection,
    pub digest_algorithm: TransferDigestAlgorithm,
    pub chunk_size: u32,
}

/// Why a `bamep.m1.data-plane-transfer` `ActionDispatch` was rejected — mapped
/// to exactly one closed RF-005 `ActionAck{Rejected}.error.code`
/// (`m1-simulated-vertical-slice-and-baseline-validation.md` RF-005). No
/// transfer-specific codes are invented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferDispatchRejection {
    /// `action_type` is not `bamep.m1.data-plane-transfer`.
    UnsupportedAction,
    /// `action_version` is not `"1"`.
    UnsupportedActionVersion,
    /// The `parameters` object is structurally invalid — a missing/mistyped
    /// field, an unknown `direction`/`digest_algorithm` enum value, a
    /// non-positive `chunk_size`, or an unexpected extra key (the schema is
    /// closed).
    InvalidParameters(&'static str),
}

impl TransferDispatchRejection {
    /// The closed RF-005 diagnostic code.
    pub fn code(&self) -> &'static str {
        match self {
            TransferDispatchRejection::UnsupportedAction => "UNSUPPORTED_ACTION",
            TransferDispatchRejection::UnsupportedActionVersion => "UNSUPPORTED_ACTION_VERSION",
            TransferDispatchRejection::InvalidParameters(_) => "INVALID_PARAMETERS",
        }
    }

    fn into_ack_error(self) -> ActionAckError {
        match self {
            TransferDispatchRejection::InvalidParameters(reason) => {
                ActionAckError::new("INVALID_PARAMETERS").with_message(reason)
            }
            other => ActionAckError::new(other.code()),
        }
    }
}

/// Parses and validates a `bamep.m1.data-plane-transfer` `ActionDispatch`'s
/// exact RF-005 `parameters` shape. Never silently defaults a missing
/// identifier.
pub fn parse_transfer_dispatch_parameters(
    dispatch: &ActionDispatchMessage,
) -> Result<DataPlaneTransferParams, TransferDispatchRejection> {
    if dispatch.body.action_type != M1_DATA_PLANE_TRANSFER_ACTION_TYPE {
        return Err(TransferDispatchRejection::UnsupportedAction);
    }
    if dispatch.body.action_version != M1_DATA_PLANE_TRANSFER_ACTION_VERSION {
        return Err(TransferDispatchRejection::UnsupportedActionVersion);
    }
    parse_transfer_parameters(&dispatch.body.parameters)
}

fn parse_transfer_parameters(
    parameters: &Map<String, Value>,
) -> Result<DataPlaneTransferParams, TransferDispatchRejection> {
    use TransferDispatchRejection::InvalidParameters;

    const EXPECTED_KEYS: [&str; 5] = [
        "transfer_id",
        "artifact_id",
        "direction",
        "digest_algorithm",
        "chunk_size",
    ];
    for key in parameters.keys() {
        if !EXPECTED_KEYS.contains(&key.as_str()) {
            return Err(InvalidParameters(
                "parameters carries an unexpected key (the v1 schema is closed)",
            ));
        }
    }

    let transfer_id = parse_uuid_field(parameters, "transfer_id")?;
    let artifact_id = parse_uuid_field(parameters, "artifact_id")?;

    let direction = match parameters.get("direction").and_then(Value::as_str) {
        Some("agent_to_server") => TransferDirection::AgentToServer,
        Some(_) => return Err(InvalidParameters("direction is an unknown enum value")),
        None => return Err(InvalidParameters("direction is missing or not a string")),
    };
    let digest_algorithm = match parameters.get("digest_algorithm").and_then(Value::as_str) {
        Some("sha256") => TransferDigestAlgorithm::Sha256,
        Some(_) => {
            return Err(InvalidParameters(
                "digest_algorithm is an unknown enum value",
            ))
        }
        None => {
            return Err(InvalidParameters(
                "digest_algorithm is missing or not a string",
            ))
        }
    };
    let chunk_size = match parameters.get("chunk_size").and_then(Value::as_u64) {
        Some(value) if value >= 1 => u32::try_from(value)
            .map_err(|_| InvalidParameters("chunk_size does not fit in a u32"))?,
        Some(_) => return Err(InvalidParameters("chunk_size is not a positive integer")),
        None => {
            return Err(InvalidParameters(
                "chunk_size is missing or not a non-negative integer",
            ))
        }
    };

    Ok(DataPlaneTransferParams {
        transfer_id,
        artifact_id,
        direction,
        digest_algorithm,
        chunk_size,
    })
}

fn parse_uuid_field(
    parameters: &Map<String, Value>,
    key: &'static str,
) -> Result<Uuid, TransferDispatchRejection> {
    match parameters.get(key).and_then(Value::as_str) {
        Some(text) => {
            let parsed = Uuid::parse_str(text).map_err(|_| {
                TransferDispatchRejection::InvalidParameters("a UUID field is not a valid UUID")
            })?;
            // The wire form is the canonical lowercase-hyphenated UUID v4.
            if parsed.hyphenated().to_string() != text {
                return Err(TransferDispatchRejection::InvalidParameters(
                    "a UUID field is not in canonical lowercase-hyphenated form",
                ));
            }
            Ok(parsed)
        }
        None => Err(TransferDispatchRejection::InvalidParameters(
            "a required UUID field is missing or not a string",
        )),
    }
}

/// One accepted transfer action, carrying the identity facts that stay fixed
/// for the life of the action (`m1-...md` RF-005; Issue #19 C1 §14). Passed
/// back into [`DataPlaneTransferAgent::run`].
#[derive(Debug, Clone)]
pub struct AcceptedTransfer {
    action_id: ProtocolId,
    params: DataPlaneTransferParams,
}

impl AcceptedTransfer {
    pub fn action_id(&self) -> ProtocolId {
        self.action_id
    }

    pub fn params(&self) -> &DataPlaneTransferParams {
        &self.params
    }

    pub fn transfer_id(&self) -> Uuid {
        self.params.transfer_id
    }

    pub fn artifact_id(&self) -> Uuid {
        self.params.artifact_id
    }
}

/// The response to one `accept` call: always an [`ActionAckMessage`] to send,
/// plus the [`AcceptedTransfer`] handle iff the dispatch was accepted.
#[derive(Debug, Clone)]
pub struct TransferDispatchResponse {
    pub ack: ActionAckMessage,
    pub accepted: Option<AcceptedTransfer>,
}

/// One Agent-side cumulative progress observation for the transfer action.
/// `bytes_processed` is the cumulative bytes of chunks `bamepd` has durably
/// accepted for this Transfer so far — never bytes merely read locally or
/// written to a socket (`m1-...md` RF-005; `m0-agent-protocol-contract.md`
/// "ActionProgress fields").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferProgress {
    pub bytes_processed: u64,
}

impl TransferProgress {
    /// Builds the Agent Protocol `ActionProgress` for this observation
    /// (`bytes_processed` only; the transfer action never reports `percent`).
    pub fn into_action_progress(self, action_id: ProtocolId) -> ActionProgressMessage {
        ActionProgressMessage::new(action_id, None, Some(self.bytes_processed), None)
            .expect("bytes_processed is always present")
    }
}

/// Deterministic test hooks for [`DataPlaneTransferAgent::run`]. All default
/// to inert; production callers use `TransferRunOptions::default()`.
#[derive(Debug, Clone, Default)]
pub struct TransferRunOptions {
    /// After this many chunks reach durable-held state **during this run**,
    /// stop and return [`TransferRunOutcome::Suspended`] with
    /// [`SuspendReason::InterruptionHookFired`] — a resumable, non-terminal
    /// state for the same action identity. `None` runs to completion.
    pub interrupt_after_newly_held_chunks: Option<u32>,
    /// When uploading this chunk index, transmit corrupted bytes while still
    /// declaring the digest of the *true* source bytes — proves the Worker's
    /// independent hash rejects it (`DIGEST_MISMATCH`), distinct from source
    /// mutation. `None` transmits the true bytes.
    pub corrupt_transmitted_bytes_of_chunk: Option<u64>,
}

/// The typed terminal outcome of one transfer action run. Each maps to exactly
/// one Agent Protocol `ActionResult`
/// (`m1-simulated-vertical-slice-and-baseline-validation.md` RF-005
/// "`ActionResult.detail`"). C1 constructs the message; **C2** owns whether and
/// when `bamepd` consumes it and the durable Artifact/Attempt/JobStep/Job
/// transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferActionResult {
    /// Seal returned a durably `Verified` Artifact.
    /// `Succeeded` / `{ "code": "TRANSFER_VERIFIED", "artifact_id" }`.
    Verified { artifact_id: Uuid },
    /// Seal returned a durably `Failed` Artifact (`PendingVerification ->
    /// Failed`). `Failed` / `{ "code": "ARTIFACT_VERIFICATION_FAILED",
    /// "artifact_id" }`.
    ArtifactVerificationFailed { artifact_id: Uuid },
    /// A required chunk could not be reproduced/verified from the source
    /// (source mutation, transmission corruption rejected by the Worker's
    /// independent hash, or a recorded identity conflict). `Failed` /
    /// `{ "code": "CHUNK_VERIFICATION_FAILED", "artifact_id" }`.
    ChunkVerificationFailed { artifact_id: Uuid },
    /// A non-specific terminal inability to continue the data-plane action.
    /// `Failed` / `{ "code": "TRANSFER_ABANDONED", "artifact_id" }`. Never used
    /// where a specific Artifact-verification or chunk-reproducibility failure
    /// is known.
    Abandoned { artifact_id: Uuid },
}

impl TransferActionResult {
    pub fn artifact_id(&self) -> Uuid {
        match self {
            TransferActionResult::Verified { artifact_id }
            | TransferActionResult::ArtifactVerificationFailed { artifact_id }
            | TransferActionResult::ChunkVerificationFailed { artifact_id }
            | TransferActionResult::Abandoned { artifact_id } => *artifact_id,
        }
    }

    /// Builds the exact RF-005 Agent Protocol `ActionResult` message for this
    /// outcome. This is a local construction — C1 never transmits it.
    pub fn into_action_result(&self, action_id: ProtocolId) -> ActionResultMessage {
        let (outcome, code) = match self {
            TransferActionResult::Verified { .. } => {
                (ActionResultOutcome::Succeeded, "TRANSFER_VERIFIED")
            }
            TransferActionResult::ArtifactVerificationFailed { .. } => {
                (ActionResultOutcome::Failed, "ARTIFACT_VERIFICATION_FAILED")
            }
            TransferActionResult::ChunkVerificationFailed { .. } => {
                (ActionResultOutcome::Failed, "CHUNK_VERIFICATION_FAILED")
            }
            TransferActionResult::Abandoned { .. } => {
                (ActionResultOutcome::Failed, "TRANSFER_ABANDONED")
            }
        };
        let mut detail = Map::new();
        detail.insert("code".to_string(), Value::String(code.to_string()));
        detail.insert(
            "artifact_id".to_string(),
            Value::String(self.artifact_id().to_string()),
        );
        ActionResultMessage::new(action_id, outcome, detail)
    }
}

/// Why a run stopped without a terminal result. Every variant is resumable
/// with the *same* action/transfer/artifact identity (Issue #19 C1 §14/§26);
/// none of them fabricates an `ActionResult`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuspendReason {
    /// The deterministic [`TransferRunOptions::interrupt_after_newly_held_chunks`]
    /// hook fired.
    InterruptionHookFired,
    /// The Worker HTTPS surface returned the single fixed generic
    /// `401 AUTHORIZATION_DENIED`. The Agent cannot infer *why*
    /// (`m0-data-plane-and-storage-contracts.md` "Per-request verification" —
    /// one non-enumerable denial). The caller may retry with a fresh
    /// [`AgentTransferAuthorization`] (renewal is owned by C3's real WSS path).
    AuthorizationUnavailable,
    /// A transport-level failure reaching the data-plane origin (connect, TLS,
    /// timeout). Never proof of any durable state.
    DataPlaneUnreachable,
}

/// A non-terminal, resumable transfer state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SuspendedTransfer {
    pub reason: SuspendReason,
    pub action_id: ProtocolId,
    pub transfer_id: Uuid,
    pub artifact_id: Uuid,
    /// Cumulative durably-held bytes observed before the run stopped.
    pub durably_held_bytes: u64,
}

/// The result of [`DataPlaneTransferAgent::run`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferRunOutcome {
    /// A terminal outcome — construct the corresponding `ActionResult`.
    Completed(TransferActionResult),
    /// A non-terminal, resumable stop — no `ActionResult`.
    Suspended(SuspendedTransfer),
}

/// A caller-misuse error (never a protocol outcome).
#[derive(Debug, thiserror::Error)]
pub enum TransferRunError {
    #[error(
        "the supplied AgentTransferAuthorization is not bound to the accepted transfer/artifact/direction"
    )]
    AuthorizationMismatch,
    #[error("the accepted parameters declare an unsupported direction or digest algorithm")]
    UnsupportedParameters,
    #[error("the TransferSource returned a wrongly sized chunk at index {chunk_index}")]
    SourceContractViolation { chunk_index: u64 },
    #[error("the TransferSource is empty (0 bytes)")]
    EmptySource,
    #[error("per-request proof construction failed")]
    Proof(#[source] ProofError),
    #[error("the data_plane_base_url from the grant could not be used")]
    Client(#[source] DataPlaneClientError),
}

/// One local record per known `action_id`, so a duplicate `ActionDispatch`
/// re-emits the retained `ActionAck` under a fresh `message_id` without
/// re-accepting or re-executing (`m0-agent-protocol-contract.md` "Idempotency,
/// retry, and uncertain delivery").
#[derive(Debug, Clone)]
enum LocalRecord {
    Accepted {
        content: DispatchContent,
        accepted: AcceptedTransfer,
        ack: ActionAckMessage,
    },
    Rejected {
        content: DispatchContent,
        ack: ActionAckMessage,
    },
}

impl LocalRecord {
    fn content(&self) -> &DispatchContent {
        match self {
            LocalRecord::Accepted { content, .. } | LocalRecord::Rejected { content, .. } => {
                content
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DispatchContent {
    action_type: String,
    action_version: String,
    parameters: Map<String, Value>,
}

impl DispatchContent {
    fn of(dispatch: &ActionDispatchMessage) -> Self {
        Self {
            action_type: dispatch.body.action_type.clone(),
            action_version: dispatch.body.action_version.clone(),
            parameters: dispatch.body.parameters.clone(),
        }
    }
}

/// The Agent-side `bamep.m1.data-plane-transfer` participant. Holds the
/// trusted-bootstrap `ServerCertFingerprint` the Agent already authenticated —
/// the exact leaf pin every data-plane HTTPS connection reuses (ADR-0018) —
/// plus minimal per-`action_id` accept state.
pub struct DataPlaneTransferAgent {
    server_fingerprint: ServerCertFingerprint,
    records: Mutex<HashMap<ProtocolId, LocalRecord>>,
}

impl DataPlaneTransferAgent {
    pub fn new(server_fingerprint: ServerCertFingerprint) -> Self {
        Self {
            server_fingerprint,
            records: Mutex::new(HashMap::new()),
        }
    }

    /// Decides the `ActionAck` for one `bamep.m1.data-plane-transfer`
    /// `ActionDispatch` and, on acceptance, returns the [`AcceptedTransfer`]
    /// handle. Records the action locally *before* returning, so a duplicate
    /// dispatch re-emits the retained ack (fresh `message_id`) rather than
    /// re-accepting. Conflicting content for a known `action_id` is rejected
    /// and never replaces the original record.
    pub fn accept(&self, dispatch: &ActionDispatchMessage) -> TransferDispatchResponse {
        let action_id = dispatch.body.action_id;
        let content = DispatchContent::of(dispatch);
        let mut records = self.records.lock().unwrap();

        if let Some(existing) = records.get(&action_id) {
            if existing.content() != &content {
                let ack = ActionAckMessage::rejected(
                    action_id,
                    ActionAckError::new("INVALID_PARAMETERS")
                        .with_message("action_id already bound to different dispatch content"),
                );
                return TransferDispatchResponse {
                    ack,
                    accepted: None,
                };
            }
            return match existing {
                LocalRecord::Accepted { accepted, ack, .. } => TransferDispatchResponse {
                    ack: ack.clone().with_fresh_message_id(),
                    accepted: Some(accepted.clone()),
                },
                LocalRecord::Rejected { ack, .. } => TransferDispatchResponse {
                    ack: ack.clone().with_fresh_message_id(),
                    accepted: None,
                },
            };
        }

        match parse_transfer_dispatch_parameters(dispatch) {
            Ok(params) => {
                let ack = ActionAckMessage::accepted(action_id);
                let accepted = AcceptedTransfer { action_id, params };
                records.insert(
                    action_id,
                    LocalRecord::Accepted {
                        content,
                        accepted: accepted.clone(),
                        ack: ack.clone(),
                    },
                );
                TransferDispatchResponse {
                    ack,
                    accepted: Some(accepted),
                }
            }
            Err(rejection) => {
                let ack = ActionAckMessage::rejected(action_id, rejection.into_ack_error());
                records.insert(
                    action_id,
                    LocalRecord::Rejected {
                        content,
                        ack: ack.clone(),
                    },
                );
                TransferDispatchResponse {
                    ack,
                    accepted: None,
                }
            }
        }
    }

    /// Executes one transfer run against the real Worker HTTPS `/api/data/v1/`
    /// surface: resume-discovery, then upload only the chunks `bamepd` does not
    /// already durably hold, then seal.
    ///
    /// The same algorithm serves a fresh transfer and a resumed one — a fresh
    /// run simply finds an empty held set. To **resume** after a
    /// [`TransferRunOutcome::Suspended`], call `run` again with the *same*
    /// [`AcceptedTransfer`] and a *fresh* [`AgentTransferAuthorization`] for
    /// the same `transfer_id` (Issue #19 C1 §14/§18/§26).
    ///
    /// `progress` is invoked once with the initial durable byte total and once
    /// after every newly durably-held chunk; the value never decreases and
    /// never reaches the total before every required chunk is durably held.
    pub async fn run<S, P>(
        &self,
        accepted: &AcceptedTransfer,
        authorization: &AgentTransferAuthorization,
        source: &S,
        options: &TransferRunOptions,
        progress: &mut P,
    ) -> Result<TransferRunOutcome, TransferRunError>
    where
        S: TransferSource,
        P: FnMut(TransferProgress),
    {
        let params = &accepted.params;
        // --- caller-misuse guards -------------------------------------------------
        if params.direction != TransferDirection::AgentToServer
            || params.digest_algorithm != TransferDigestAlgorithm::Sha256
        {
            return Err(TransferRunError::UnsupportedParameters);
        }
        if authorization.transfer_id() != params.transfer_id
            || authorization.artifact_id() != params.artifact_id
            || authorization.direction() != params.direction
        {
            return Err(TransferRunError::AuthorizationMismatch);
        }
        let total_len = source.total_len();
        if total_len == 0 {
            return Err(TransferRunError::EmptySource);
        }

        let client =
            DataPlaneClient::connect(authorization.data_plane_base_url(), self.server_fingerprint)
                .map_err(TransferRunError::Client)?;

        let ctx = RunContext {
            action_id: accepted.action_id,
            transfer_id: params.transfer_id,
            artifact_id: params.artifact_id,
        };

        // --- resume discovery ---------------------------------------------------
        let resume_proof = authorization
            .create_proof_now(TransferOperation::ResumeDiscovery, None)
            .map_err(TransferRunError::Proof)?;
        let manifest = match client
            .discover_resume(authorization.token(), params.transfer_id, &resume_proof)
            .await
        {
            Ok(ResumeOutcome::Approved(manifest)) => manifest,
            Ok(ResumeOutcome::AuthorizationDenied) => {
                return Ok(ctx.suspend(SuspendReason::AuthorizationUnavailable, 0));
            }
            Ok(ResumeOutcome::Malformed) | Ok(ResumeOutcome::Unexpected { .. }) => {
                return Ok(ctx.abandoned());
            }
            Err(_) => return Ok(ctx.suspend(SuspendReason::DataPlaneUnreachable, 0)),
        };

        // --- authoritative chunking facts -------------------------------------
        if manifest.digest_algorithm != "sha256" {
            return Ok(ctx.abandoned());
        }
        let chunk_size = manifest.chunk_size;
        if chunk_size == 0 || chunk_size != params.chunk_size {
            // A `chunk_size` disagreement between the durable manifest and the
            // action parameters is a contract inconsistency — fail closed.
            return Ok(ctx.abandoned());
        }
        let chunk_count = total_len.div_ceil(u64::from(chunk_size));
        if manifest.sealed && manifest.expected_chunk_count != Some(chunk_count) {
            return Ok(ctx.abandoned());
        }

        // --- reproduce every chunk digest + the incremental full-Artifact digest
        let plan = build_chunk_plan(source, chunk_count, chunk_size, total_len)?;

        // --- reconcile durable held chunks against our reproduced identities ---
        let mut held: BTreeSet<u64> = BTreeSet::new();
        for entry in &manifest.held_chunks {
            if entry.chunk_index >= chunk_count {
                // A durably held chunk beyond our chunk count means our source
                // no longer agrees with the recorded manifest — fail closed.
                return Ok(ctx.chunk_verification_failed());
            }
            if entry.digest != plan.chunk_digests[entry.chunk_index as usize] {
                // A recorded chunk identity we can no longer reproduce. Never
                // rewritten (`m0-data-plane-and-storage-contracts.md`).
                return Ok(ctx.chunk_verification_failed());
            }
            held.insert(entry.chunk_index);
        }

        let mut held_bytes: u64 = held
            .iter()
            .map(|i| u64::from(plan.chunk_sizes[*i as usize]))
            .sum();
        progress(TransferProgress {
            bytes_processed: held_bytes,
        });

        // --- upload only the missing chunks ----------------------------------
        let mut newly_held: u32 = 0;
        for index in 0..chunk_count {
            if held.contains(&index) {
                continue;
            }
            let true_bytes = source.chunk_bytes(index, chunk_size);
            if true_bytes.len() as u64 != u64::from(plan.chunk_sizes[index as usize]) {
                return Err(TransferRunError::SourceContractViolation { chunk_index: index });
            }
            let declared_digest = &plan.chunk_digests[index as usize];
            let transmitted = if options.corrupt_transmitted_bytes_of_chunk == Some(index) {
                corrupt(&true_bytes)
            } else {
                true_bytes
            };

            let proof = authorization
                .create_proof_now(TransferOperation::ChunkUpload, Some(index))
                .map_err(TransferRunError::Proof)?;
            let outcome = match client
                .put_chunk(
                    authorization.token(),
                    params.transfer_id,
                    index,
                    declared_digest,
                    &proof,
                    transmitted,
                )
                .await
            {
                Ok(outcome) => outcome,
                Err(_) => {
                    return Ok(ctx.suspend(SuspendReason::DataPlaneUnreachable, held_bytes));
                }
            };

            match outcome {
                PutChunkOutcome::Accepted { .. } | PutChunkOutcome::AlreadyHeld { .. } => {
                    held.insert(index);
                    held_bytes += u64::from(plan.chunk_sizes[index as usize]);
                    newly_held += 1;
                    progress(TransferProgress {
                        bytes_processed: held_bytes,
                    });
                    if let Some(limit) = options.interrupt_after_newly_held_chunks {
                        if newly_held >= limit && (held.len() as u64) < chunk_count {
                            return Ok(
                                ctx.suspend(SuspendReason::InterruptionHookFired, held_bytes)
                            );
                        }
                    }
                }
                PutChunkOutcome::DigestMismatch | PutChunkOutcome::ChunkIdentityConflict => {
                    return Ok(ctx.chunk_verification_failed());
                }
                PutChunkOutcome::TransferNotContinuable | PutChunkOutcome::ChunkTooLarge => {
                    return Ok(ctx.abandoned());
                }
                PutChunkOutcome::AuthorizationDenied => {
                    return Ok(ctx.suspend(SuspendReason::AuthorizationUnavailable, held_bytes));
                }
                PutChunkOutcome::Malformed | PutChunkOutcome::Unexpected { .. } => {
                    return Ok(ctx.abandoned());
                }
            }
        }

        // --- seal + verify ---------------------------------------------------
        let seal_proof = authorization
            .create_proof_now(TransferOperation::SealManifest, None)
            .map_err(TransferRunError::Proof)?;
        match client
            .seal(
                authorization.token(),
                params.transfer_id,
                &seal_proof,
                chunk_count,
                &plan.artifact_digest_wire,
            )
            .await
        {
            Ok(SealOutcome::Completed {
                artifact_id,
                artifact_status,
            }) => {
                if artifact_id != params.artifact_id {
                    // bamepd's sealed Artifact identity disagrees with the one
                    // the ActionDispatch delivered — fail closed.
                    return Ok(ctx.abandoned());
                }
                match artifact_status {
                    SealArtifactStatus::Verified => Ok(ctx.verified()),
                    SealArtifactStatus::Failed => Ok(ctx.artifact_verification_failed()),
                }
            }
            Ok(SealOutcome::IncompleteManifest) | Ok(SealOutcome::ManifestAlreadySealed) => {
                Ok(ctx.abandoned())
            }
            Ok(SealOutcome::AuthorizationDenied) => {
                Ok(ctx.suspend(SuspendReason::AuthorizationUnavailable, held_bytes))
            }
            Ok(SealOutcome::Malformed) | Ok(SealOutcome::Unexpected { .. }) => Ok(ctx.abandoned()),
            Err(_) => Ok(ctx.suspend(SuspendReason::DataPlaneUnreachable, held_bytes)),
        }
    }
}

/// The fixed identity facts a run threads through every outcome constructor.
struct RunContext {
    action_id: ProtocolId,
    transfer_id: Uuid,
    artifact_id: Uuid,
}

impl RunContext {
    fn suspend(&self, reason: SuspendReason, durably_held_bytes: u64) -> TransferRunOutcome {
        TransferRunOutcome::Suspended(SuspendedTransfer {
            reason,
            action_id: self.action_id,
            transfer_id: self.transfer_id,
            artifact_id: self.artifact_id,
            durably_held_bytes,
        })
    }

    fn verified(&self) -> TransferRunOutcome {
        TransferRunOutcome::Completed(TransferActionResult::Verified {
            artifact_id: self.artifact_id,
        })
    }

    fn artifact_verification_failed(&self) -> TransferRunOutcome {
        TransferRunOutcome::Completed(TransferActionResult::ArtifactVerificationFailed {
            artifact_id: self.artifact_id,
        })
    }

    fn chunk_verification_failed(&self) -> TransferRunOutcome {
        TransferRunOutcome::Completed(TransferActionResult::ChunkVerificationFailed {
            artifact_id: self.artifact_id,
        })
    }

    fn abandoned(&self) -> TransferRunOutcome {
        TransferRunOutcome::Completed(TransferActionResult::Abandoned {
            artifact_id: self.artifact_id,
        })
    }
}

struct ChunkPlan {
    chunk_digests: Vec<String>,
    chunk_sizes: Vec<u32>,
    artifact_digest_wire: String,
}

/// Reproduces every chunk's declared digest and the incremental full-Artifact
/// digest from the source. The full-Artifact digest is SHA-256 over the raw
/// concatenation `chunk0 || chunk1 || ... || chunk(N-1)` with no framing
/// (`m0-data-plane-and-storage-contracts.md` "Full-Artifact byte
/// reconstruction").
fn build_chunk_plan<S: TransferSource>(
    source: &S,
    chunk_count: u64,
    chunk_size: u32,
    total_len: u64,
) -> Result<ChunkPlan, TransferRunError> {
    let mut full = Sha256::new();
    let mut chunk_digests = Vec::with_capacity(chunk_count as usize);
    let mut chunk_sizes = Vec::with_capacity(chunk_count as usize);
    for index in 0..chunk_count {
        let bytes = source.chunk_bytes(index, chunk_size);
        let expected_len = if index + 1 < chunk_count {
            u64::from(chunk_size)
        } else {
            total_len - (chunk_count - 1) * u64::from(chunk_size)
        };
        if bytes.len() as u64 != expected_len || bytes.is_empty() {
            return Err(TransferRunError::SourceContractViolation { chunk_index: index });
        }
        full.update(&bytes);
        chunk_digests.push(sha256_base64url(&bytes));
        chunk_sizes.push(bytes.len() as u32);
    }
    Ok(ChunkPlan {
        chunk_digests,
        chunk_sizes,
        artifact_digest_wire: URL_SAFE_NO_PAD.encode(full.finalize()),
    })
}

fn sha256_base64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(bytes))
}

/// Flips bytes so the transmitted body no longer hashes to the declared digest,
/// without ever touching the source of truth (test hook for
/// [`TransferRunOptions::corrupt_transmitted_bytes_of_chunk`]).
fn corrupt(bytes: &[u8]) -> Vec<u8> {
    let mut corrupted = bytes.to_vec();
    if let Some(first) = corrupted.first_mut() {
        *first ^= 0xFF;
    }
    corrupted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer_source::InMemoryTransferSource;
    use serde_json::json;

    fn params_object(chunk_size: i64) -> Map<String, Value> {
        json!({
            "transfer_id": "11111111-1111-4111-8111-111111111111",
            "artifact_id": "22222222-2222-4222-8222-222222222222",
            "direction": "agent_to_server",
            "digest_algorithm": "sha256",
            "chunk_size": chunk_size,
        })
        .as_object()
        .unwrap()
        .clone()
    }

    fn dispatch(
        action_type: &str,
        action_version: &str,
        params: Map<String, Value>,
    ) -> ActionDispatchMessage {
        ActionDispatchMessage::new(ProtocolId::generate(), action_type, action_version, params)
    }

    #[test]
    fn parses_the_exact_rf005_parameters() {
        let parsed = parse_transfer_dispatch_parameters(&dispatch(
            M1_DATA_PLANE_TRANSFER_ACTION_TYPE,
            "1",
            params_object(4096),
        ))
        .unwrap();
        assert_eq!(
            parsed,
            DataPlaneTransferParams {
                transfer_id: Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap(),
                artifact_id: Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap(),
                direction: TransferDirection::AgentToServer,
                digest_algorithm: TransferDigestAlgorithm::Sha256,
                chunk_size: 4096,
            }
        );
    }

    #[test]
    fn rejects_wrong_action_type_and_version() {
        assert_eq!(
            parse_transfer_dispatch_parameters(&dispatch("bamep.m1.other", "1", params_object(1)))
                .unwrap_err(),
            TransferDispatchRejection::UnsupportedAction
        );
        assert_eq!(
            parse_transfer_dispatch_parameters(&dispatch(
                M1_DATA_PLANE_TRANSFER_ACTION_TYPE,
                "2",
                params_object(1)
            ))
            .unwrap_err(),
            TransferDispatchRejection::UnsupportedActionVersion
        );
    }

    #[test]
    fn rejects_structurally_invalid_parameters() {
        // non-positive chunk_size
        assert_eq!(
            parse_transfer_dispatch_parameters(&dispatch(
                M1_DATA_PLANE_TRANSFER_ACTION_TYPE,
                "1",
                params_object(0)
            ))
            .unwrap_err()
            .code(),
            "INVALID_PARAMETERS"
        );
        // missing field
        let mut missing = params_object(4096);
        missing.remove("artifact_id");
        assert!(matches!(
            parse_transfer_dispatch_parameters(&dispatch(
                M1_DATA_PLANE_TRANSFER_ACTION_TYPE,
                "1",
                missing
            )),
            Err(TransferDispatchRejection::InvalidParameters(_))
        ));
        // unknown enum value
        let mut bad_dir = params_object(4096);
        bad_dir.insert("direction".to_string(), json!("server_to_agent"));
        assert!(matches!(
            parse_transfer_dispatch_parameters(&dispatch(
                M1_DATA_PLANE_TRANSFER_ACTION_TYPE,
                "1",
                bad_dir
            )),
            Err(TransferDispatchRejection::InvalidParameters(_))
        ));
        // unexpected extra key (closed schema)
        let mut extra = params_object(4096);
        extra.insert("surprise".to_string(), json!(true));
        assert!(matches!(
            parse_transfer_dispatch_parameters(&dispatch(
                M1_DATA_PLANE_TRANSFER_ACTION_TYPE,
                "1",
                extra
            )),
            Err(TransferDispatchRejection::InvalidParameters(_))
        ));
        // non-canonical UUID
        let mut bad_uuid = params_object(4096);
        bad_uuid.insert(
            "transfer_id".to_string(),
            json!("11111111111141118111111111111111"),
        );
        assert!(matches!(
            parse_transfer_dispatch_parameters(&dispatch(
                M1_DATA_PLANE_TRANSFER_ACTION_TYPE,
                "1",
                bad_uuid
            )),
            Err(TransferDispatchRejection::InvalidParameters(_))
        ));
    }

    #[test]
    fn accept_records_and_a_duplicate_re_emits_a_fresh_ack() {
        let agent = DataPlaneTransferAgent::new(ServerCertFingerprint::from_sha256_digest([0; 32]));
        let d = dispatch(M1_DATA_PLANE_TRANSFER_ACTION_TYPE, "1", params_object(4096));

        let first = agent.accept(&d);
        assert!(matches!(
            first.ack.body.outcome,
            bamep_agent_protocol::ActionAckOutcome::Accepted
        ));
        assert!(first.accepted.is_some());

        let second = agent.accept(&d);
        assert!(matches!(
            second.ack.body.outcome,
            bamep_agent_protocol::ActionAckOutcome::Accepted
        ));
        assert!(second.accepted.is_some());
        assert_ne!(
            first.ack.envelope.message_id,
            second.ack.envelope.message_id
        );
    }

    #[test]
    fn accept_conflicting_content_for_a_known_action_id_is_rejected_and_never_replaces() {
        let agent = DataPlaneTransferAgent::new(ServerCertFingerprint::from_sha256_digest([0; 32]));
        let mut d1 = dispatch(M1_DATA_PLANE_TRANSFER_ACTION_TYPE, "1", params_object(4096));
        let accepted = agent.accept(&d1);
        assert!(accepted.accepted.is_some());

        // Same action_id, different parameters.
        let action_id = d1.body.action_id;
        d1.body.parameters = params_object(8192);
        d1.body.action_id = action_id;
        let conflict = agent.accept(&d1);
        assert!(matches!(
            conflict.ack.body.outcome,
            bamep_agent_protocol::ActionAckOutcome::Rejected
        ));
        assert!(conflict.accepted.is_none());
    }

    #[test]
    fn action_result_detail_shapes_are_exact() {
        let action_id = ProtocolId::generate();
        let artifact_id = Uuid::new_v4();
        let verified = TransferActionResult::Verified { artifact_id }.into_action_result(action_id);
        assert!(matches!(
            verified.body.outcome,
            ActionResultOutcome::Succeeded
        ));
        assert_eq!(
            verified.body.detail.get("code").unwrap().as_str().unwrap(),
            "TRANSFER_VERIFIED"
        );
        assert_eq!(
            verified
                .body
                .detail
                .get("artifact_id")
                .unwrap()
                .as_str()
                .unwrap(),
            artifact_id.to_string()
        );

        for (result, code) in [
            (
                TransferActionResult::ArtifactVerificationFailed { artifact_id },
                "ARTIFACT_VERIFICATION_FAILED",
            ),
            (
                TransferActionResult::ChunkVerificationFailed { artifact_id },
                "CHUNK_VERIFICATION_FAILED",
            ),
            (
                TransferActionResult::Abandoned { artifact_id },
                "TRANSFER_ABANDONED",
            ),
        ] {
            let message = result.into_action_result(action_id);
            assert!(matches!(message.body.outcome, ActionResultOutcome::Failed));
            assert_eq!(
                message.body.detail.get("code").unwrap().as_str().unwrap(),
                code
            );
        }
    }

    #[test]
    fn chunk_plan_reconstructs_the_source_digest_over_raw_concatenation() {
        let source = InMemoryTransferSource::pattern(10, 3); // 10 bytes, chunk_size 4 -> 3 chunks (4,4,2)
        let plan = build_chunk_plan(&source, 3, 4, 10).unwrap();
        assert_eq!(plan.chunk_sizes, vec![4, 4, 2]);
        // full-Artifact digest == SHA-256 of the whole source, since raw concat
        // of the chunks IS the whole source.
        let direct = URL_SAFE_NO_PAD.encode(Sha256::digest(source.as_bytes()));
        assert_eq!(plan.artifact_digest_wire, direct);
    }
}
