//! Agent-side ephemeral proof-key lifecycle and per-request proof
//! construction for the M1 Agent -> Server data-plane transfer authorization
//! path (`docs/specifications/m0-data-plane-and-storage-contracts.md`
//! "Transfer authorization" / "Ephemeral proof key" / "Per-request proof";
//! Issue #38).
//!
//! This is an **independent** Agent-side implementation. It deliberately does
//! not depend on `bamep-server` or `bamep-domain` and does not call the
//! Server's `build_proof_transcript`/verification helpers — the M1 Agent
//! participant is `bamep-simulator`, and byte-level interoperability with the
//! `bamepd` verifier is only real evidence when the two sides share no code
//! (ADR-0003 "Simulator/Agent wire independence"). The 137-byte transcript
//! layout, the Ed25519 discipline, `SHA-256(token)` capability identity, and
//! the base64url-no-pad wire encodings are all re-derived here directly from
//! the Specification.
//!
//! Key lifetime (`m0-data-plane-and-storage-contracts.md` "Ephemeral proof
//! key" / "Disconnect and restart"):
//!
//! - the ephemeral Ed25519 private key is generated locally, lives only in
//!   Agent-local memory ([`AgentProofKey`]), and is never persisted as
//!   Endpoint state;
//! - only its canonical public half is ever exposed
//!   ([`AgentProofKey::public_key_wire`], carried in
//!   `TransferAuthorizationRequest`);
//! - a fresh key is minted on every authorization context — an Agent restart
//!   or an authorization renewal simply calls [`AgentProofKey::generate`]
//!   again and re-requests a capability, keeping the same
//!   `transfer_id`/`artifact_id`/`action_id`;
//! - a fresh `proof_id` is generated for every operation attempt;
//! - `Debug` for the private key, the capability token, and the raw
//!   signature is redacted.
//!
//! This module intentionally stops before bulk transfer: it produces the
//! authorization request input and the per-request proof/header material that
//! Issue #19 will later compose into a real HTTPS chunk transfer.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::{Signer, SigningKey};
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// The exact 34-byte ASCII domain-separation string
/// (`m0-data-plane-and-storage-contracts.md` "Per-request proof").
const TRANSCRIPT_DOMAIN: &[u8] = b"bamep.m1.data-plane-transfer.proof";
const TRANSCRIPT_SCHEMA_VERSION: u16 = 1;

/// The fixed transcript width (`m0-data-plane-and-storage-contracts.md`
/// "Per-request proof": "The signed payload is exactly 137 bytes").
pub const PROOF_TRANSCRIPT_LEN: usize = 137;

/// The closed per-request-proof operation vocabulary
/// (`m0-data-plane-and-storage-contracts.md` "Per-request proof").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferOperation {
    ChunkUpload,
    ResumeDiscovery,
    SealManifest,
}

impl TransferOperation {
    /// The 1-byte transcript encoding: `1 = chunk_upload`,
    /// `2 = resume_discovery`, `3 = seal_manifest`.
    fn transcript_byte(self) -> u8 {
        match self {
            TransferOperation::ChunkUpload => 1,
            TransferOperation::ResumeDiscovery => 2,
            TransferOperation::SealManifest => 3,
        }
    }

    /// `chunk_index_present` is `1` only for `chunk_upload`.
    pub fn requires_chunk_index(self) -> bool {
        matches!(self, TransferOperation::ChunkUpload)
    }

    /// The HTTPS-contract wire string
    /// (`m0-data-plane-and-storage-contracts.md`; also the Worker UDS
    /// `AuthorizationQuery.operation`).
    pub fn wire_str(self) -> &'static str {
        match self {
            TransferOperation::ChunkUpload => "chunk_upload",
            TransferOperation::ResumeDiscovery => "resume_discovery",
            TransferOperation::SealManifest => "seal_manifest",
        }
    }
}

/// The closed direction vocabulary. Only `agent_to_server` is assigned in V1
/// (`m0-data-plane-and-storage-contracts.md` "Per-request proof": value `2`
/// is reserved and unassigned).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    AgentToServer,
}

impl TransferDirection {
    fn transcript_byte(self) -> u8 {
        match self {
            TransferDirection::AgentToServer => 1,
        }
    }

    pub fn wire_str(self) -> &'static str {
        match self {
            TransferDirection::AgentToServer => "agent_to_server",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProofError {
    #[error(
        "chunk_upload requires a chunk_index; resume_discovery/seal_manifest must not carry one"
    )]
    ChunkIndexRuleViolation,
    #[error("the current system clock is before the Unix epoch")]
    ClockBeforeEpoch,
}

/// The Agent-local ephemeral Ed25519 proof keypair for one authorization
/// context. The private half never leaves this value.
pub struct AgentProofKey {
    signing_key: SigningKey,
}

impl AgentProofKey {
    /// Mints a fresh ephemeral keypair from the operating-system CSPRNG.
    /// Call this once per authorization context, and again on Agent restart
    /// or authorization renewal.
    pub fn generate() -> Self {
        let mut secret = [0u8; 32];
        OsRng.fill_bytes(&mut secret);
        let key = Self {
            signing_key: SigningKey::from_bytes(&secret),
        };
        secret.fill(0);
        key
    }

    /// Deterministic construction from a fixed 32-byte secret — for
    /// reproducible Simulator scenarios only (`docs/development/testing.md`
    /// "Simulator tests must remain reproducible").
    pub fn from_secret_bytes(secret: [u8; 32]) -> Self {
        Self {
            signing_key: SigningKey::from_bytes(&secret),
        }
    }

    /// The canonical wire form of the public half:
    /// `proof_public_key` = raw 32-byte Ed25519 public key, canonical RFC
    /// 4648 base64url without padding (43 ASCII characters) — the exact value
    /// carried in `TransferAuthorizationRequest.proof_public_key`
    /// (`m0-data-plane-and-storage-contracts.md` "Ephemeral proof key" "Wire
    /// encoding").
    pub fn public_key_wire(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.signing_key.verifying_key().to_bytes())
    }

    /// The raw 32-byte public-key value.
    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    fn sign(&self, transcript: &[u8; PROOF_TRANSCRIPT_LEN]) -> [u8; 64] {
        self.signing_key.sign(transcript).to_bytes()
    }
}

impl fmt::Debug for AgentProofKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The private key is secret material — never serialize it, even in
        // Debug output (`m0-data-plane-and-storage-contracts.md`; Issue #38
        // correction §32).
        f.debug_struct("AgentProofKey")
            .field("private_key", &"REDACTED")
            .field("public_key_wire", &self.public_key_wire())
            .finish()
    }
}

/// One freshly generated, single-use per-request `proof_id`: 16 CSPRNG bytes,
/// carried on the wire as canonical base64url-no-pad (22 ASCII characters)
/// (`m0-data-plane-and-storage-contracts.md` "Freshness and replay
/// representation").
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ProofId([u8; 16]);

impl ProofId {
    pub fn generate() -> Self {
        let mut bytes = [0u8; 16];
        OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub fn wire_value(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }
}

impl fmt::Debug for ProofId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ProofId({})", self.wire_value())
    }
}

/// The completed, signed per-request proof carrier
/// (`m0-data-plane-and-storage-contracts.md` "HTTPS data-plane v1 contract"
/// "Common request elements"): `proof_id`, `issued_at` (decimal ms), and the
/// signature, each canonicalized, ready to place on `X-Bamep-Transfer-Proof`
/// or in a Worker UDS `AuthorizationQuery`.
#[derive(Clone)]
pub struct TransferProof {
    pub proof_id_wire: String,
    pub issued_at_millis: u64,
    signature: [u8; 64],
}

impl TransferProof {
    /// The raw 64-byte signature, canonical base64url-no-pad (86 ASCII
    /// characters).
    pub fn signature_wire(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.signature)
    }

    /// The compact `X-Bamep-Transfer-Proof` header value:
    /// `<proof_id>.<issued_at>.<signature>`
    /// (`m0-data-plane-and-storage-contracts.md` "Common request elements").
    pub fn header_value(&self) -> String {
        format!(
            "{}.{}.{}",
            self.proof_id_wire,
            self.issued_at_millis,
            self.signature_wire()
        )
    }
}

impl fmt::Debug for TransferProof {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The raw signature is redacted from Debug (Issue #38 correction
        // §32); `proof_id`/`issued_at` are freshness metadata, not secrets.
        f.debug_struct("TransferProof")
            .field("proof_id_wire", &self.proof_id_wire)
            .field("issued_at_millis", &self.issued_at_millis)
            .field("signature", &"REDACTED")
            .finish()
    }
}

/// The capability material an Agent retains after a `TransferAuthorizationGrant`,
/// plus the ephemeral key it is sender-constrained to — everything needed to
/// construct per-request proofs for one authorization context.
pub struct AgentTransferAuthorization {
    proof_key: AgentProofKey,
    token: String,
    transfer_id: Uuid,
    artifact_id: Uuid,
    direction: TransferDirection,
    data_plane_base_url: String,
}

impl AgentTransferAuthorization {
    /// `token`/`data_plane_base_url` come straight from `TransferAuthorizationGrant`;
    /// `proof_key` is the exact ephemeral key whose public half was sent in
    /// the matching `TransferAuthorizationRequest`.
    pub fn new(
        proof_key: AgentProofKey,
        token: impl Into<String>,
        transfer_id: Uuid,
        artifact_id: Uuid,
        direction: TransferDirection,
        data_plane_base_url: impl Into<String>,
    ) -> Self {
        Self {
            proof_key,
            token: token.into(),
            transfer_id,
            artifact_id,
            direction,
            data_plane_base_url: data_plane_base_url.into(),
        }
    }

    pub fn transfer_id(&self) -> Uuid {
        self.transfer_id
    }

    pub fn artifact_id(&self) -> Uuid {
        self.artifact_id
    }

    pub fn direction(&self) -> TransferDirection {
        self.direction
    }

    pub fn data_plane_base_url(&self) -> &str {
        &self.data_plane_base_url
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// The canonical wire form of the ephemeral proof *public* key this
    /// authorization is sender-constrained to — the exact value that was
    /// sent in the matching `TransferAuthorizationRequest.proof_public_key`.
    /// Not secret (`m1-worker-data-plane-control-contract.md` "Security and
    /// logging").
    pub fn proof_public_key_wire(&self) -> String {
        self.proof_key.public_key_wire()
    }

    /// The derived capability identity: exactly `SHA-256(UTF-8 bytes of the
    /// exact token string)` (`m0-data-plane-and-storage-contracts.md`
    /// "Capability opacity"). Computed independently here — never received
    /// from the Server.
    pub fn capability_id_bytes(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(self.token.as_bytes());
        let mut out = [0u8; 32];
        out.copy_from_slice(hasher.finalize().as_slice());
        out
    }

    /// Constructs and signs a fresh per-request proof for one operation
    /// attempt at `issued_at_millis` (Unix epoch milliseconds). A fresh
    /// `proof_id` is minted every call — a retried operation always gets a
    /// new proof (`m0-data-plane-and-storage-contracts.md` "Idempotent retry
    /// is not proof reuse").
    pub fn create_proof(
        &self,
        operation: TransferOperation,
        chunk_index: Option<u64>,
        issued_at_millis: u64,
    ) -> Result<TransferProof, ProofError> {
        if operation.requires_chunk_index() != chunk_index.is_some() {
            return Err(ProofError::ChunkIndexRuleViolation);
        }
        let proof_id = ProofId::generate();
        let transcript = build_proof_transcript(
            &self.capability_id_bytes(),
            operation,
            self.transfer_id,
            self.artifact_id,
            self.direction,
            chunk_index,
            &proof_id.0,
            issued_at_millis,
        );
        Ok(TransferProof {
            proof_id_wire: proof_id.wire_value(),
            issued_at_millis,
            signature: self.proof_key.sign(&transcript),
        })
    }

    /// [`create_proof`](Self::create_proof) using the current system clock
    /// for `issued_at`.
    pub fn create_proof_now(
        &self,
        operation: TransferOperation,
        chunk_index: Option<u64>,
    ) -> Result<TransferProof, ProofError> {
        let issued_at_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ProofError::ClockBeforeEpoch)?
            .as_millis() as u64;
        self.create_proof(operation, chunk_index, issued_at_millis)
    }
}

impl fmt::Debug for AgentTransferAuthorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The opaque capability `token` is bearer authorization secret
        // material — redacted (`m1-worker-data-plane-control-contract.md`
        // "Security and logging"; Issue #38 correction §32).
        f.debug_struct("AgentTransferAuthorization")
            .field("token", &"REDACTED")
            .field("proof_key", &self.proof_key)
            .field("transfer_id", &self.transfer_id)
            .field("artifact_id", &self.artifact_id)
            .field("direction", &self.direction)
            .field("data_plane_base_url", &self.data_plane_base_url)
            .finish()
    }
}

/// Builds the exact byte-for-byte canonical 137-byte signed transcript
/// (`m0-data-plane-and-storage-contracts.md` "Per-request proof"):
///
/// ```text
/// u16be(34) || ASCII("bamep.m1.data-plane-transfer.proof") || u16be(1)
///   || capability_id[32] || operation[1] || transfer_id[16] || artifact_id[16]
///   || direction[1] || chunk_index_present[1] || chunk_index[8]
///   || proof_id[16] || issued_at[8]
/// ```
///
/// `chunk_index` is encoded as exactly `0` when absent. Re-implemented here
/// independently of the Server's builder — see module docs.
#[allow(clippy::too_many_arguments)]
pub fn build_proof_transcript(
    capability_id: &[u8; 32],
    operation: TransferOperation,
    transfer_id: Uuid,
    artifact_id: Uuid,
    direction: TransferDirection,
    chunk_index: Option<u64>,
    proof_id: &[u8; 16],
    issued_at_millis: u64,
) -> [u8; PROOF_TRANSCRIPT_LEN] {
    let mut buf = [0u8; PROOF_TRANSCRIPT_LEN];
    let mut offset = 0usize;
    let mut put = |dst: &mut [u8; PROOF_TRANSCRIPT_LEN], bytes: &[u8]| {
        dst[offset..offset + bytes.len()].copy_from_slice(bytes);
        offset += bytes.len();
    };

    put(&mut buf, &(TRANSCRIPT_DOMAIN.len() as u16).to_be_bytes());
    put(&mut buf, TRANSCRIPT_DOMAIN);
    put(&mut buf, &TRANSCRIPT_SCHEMA_VERSION.to_be_bytes());
    put(&mut buf, capability_id);
    put(&mut buf, &[operation.transcript_byte()]);
    put(&mut buf, transfer_id.as_bytes());
    put(&mut buf, artifact_id.as_bytes());
    put(&mut buf, &[direction.transcript_byte()]);
    put(&mut buf, &[u8::from(chunk_index.is_some())]);
    put(&mut buf, &chunk_index.unwrap_or(0).to_be_bytes());
    put(&mut buf, proof_id);
    put(&mut buf, &issued_at_millis.to_be_bytes());

    debug_assert_eq!(offset, PROOF_TRANSCRIPT_LEN);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_auth() -> AgentTransferAuthorization {
        AgentTransferAuthorization::new(
            AgentProofKey::from_secret_bytes([5u8; 32]),
            "opaque-capability-token",
            Uuid::new_v4(),
            Uuid::new_v4(),
            TransferDirection::AgentToServer,
            "https://server.example:8443",
        )
    }

    #[test]
    fn public_key_wire_is_43_canonical_base64url_chars() {
        let key = AgentProofKey::generate();
        let wire = key.public_key_wire();
        assert_eq!(wire.len(), 43);
        assert!(wire
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    }

    #[test]
    fn capability_id_is_sha256_of_exact_token_bytes() {
        let auth = sample_auth();
        let expected = {
            let mut h = Sha256::new();
            h.update(b"opaque-capability-token");
            let mut o = [0u8; 32];
            o.copy_from_slice(h.finalize().as_slice());
            o
        };
        assert_eq!(auth.capability_id_bytes(), expected);
    }

    #[test]
    fn transcript_is_exactly_137_bytes_with_the_documented_layout() {
        let auth = sample_auth();
        let proof_id = [7u8; 16];
        let transcript = build_proof_transcript(
            &auth.capability_id_bytes(),
            TransferOperation::ChunkUpload,
            auth.transfer_id,
            auth.artifact_id,
            TransferDirection::AgentToServer,
            Some(9),
            &proof_id,
            1_700_000_000_000,
        );
        assert_eq!(transcript.len(), 137);
        assert_eq!(&transcript[0..2], &34u16.to_be_bytes());
        assert_eq!(&transcript[2..36], TRANSCRIPT_DOMAIN);
        assert_eq!(&transcript[36..38], &1u16.to_be_bytes());
        assert_eq!(&transcript[38..70], &auth.capability_id_bytes());
        assert_eq!(transcript[70], 1); // chunk_upload
        assert_eq!(&transcript[71..87], auth.transfer_id.as_bytes());
        assert_eq!(&transcript[87..103], auth.artifact_id.as_bytes());
        assert_eq!(transcript[103], 1); // agent_to_server
        assert_eq!(transcript[104], 1); // chunk_index_present
        assert_eq!(&transcript[105..113], &9u64.to_be_bytes());
        assert_eq!(&transcript[113..129], &proof_id);
        assert_eq!(&transcript[129..137], &1_700_000_000_000u64.to_be_bytes());
    }

    #[test]
    fn chunk_index_rule_is_enforced() {
        let auth = sample_auth();
        assert!(matches!(
            auth.create_proof(TransferOperation::ChunkUpload, None, 1),
            Err(ProofError::ChunkIndexRuleViolation)
        ));
        assert!(matches!(
            auth.create_proof(TransferOperation::SealManifest, Some(0), 1),
            Err(ProofError::ChunkIndexRuleViolation)
        ));
        assert!(auth
            .create_proof(TransferOperation::ResumeDiscovery, None, 1)
            .is_ok());
    }

    #[test]
    fn every_proof_gets_a_fresh_proof_id() {
        let auth = sample_auth();
        let a = auth
            .create_proof(TransferOperation::ResumeDiscovery, None, 1)
            .unwrap();
        let b = auth
            .create_proof(TransferOperation::ResumeDiscovery, None, 1)
            .unwrap();
        assert_ne!(a.proof_id_wire, b.proof_id_wire);
    }

    #[test]
    fn header_value_has_three_dot_separated_segments_of_the_right_widths() {
        let auth = sample_auth();
        let proof = auth
            .create_proof(TransferOperation::ChunkUpload, Some(3), 1_700_000_000_000)
            .unwrap();
        let header = proof.header_value();
        let parts: Vec<&str> = header.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].len(), 22); // proof_id
        assert_eq!(parts[1], "1700000000000"); // decimal issued_at
        assert_eq!(parts[2].len(), 86); // signature
    }

    #[test]
    fn debug_output_never_leaks_the_private_key_token_or_signature() {
        let auth = sample_auth();
        let proof = auth
            .create_proof(TransferOperation::ResumeDiscovery, None, 1)
            .unwrap();
        let auth_debug = format!("{auth:?}");
        assert!(!auth_debug.contains("opaque-capability-token"));
        assert!(auth_debug.contains("REDACTED"));
        let proof_debug = format!("{proof:?}");
        assert!(!proof_debug.contains(&proof.signature_wire()));
        assert!(proof_debug.contains("REDACTED"));
    }

    #[test]
    fn a_proof_verifies_under_the_agents_own_public_key() {
        use ed25519_dalek::{Signature, Verifier, VerifyingKey};
        let auth = sample_auth();
        let proof = auth
            .create_proof(TransferOperation::ChunkUpload, Some(2), 1_700_000_000_000)
            .unwrap();
        let transcript = build_proof_transcript(
            &auth.capability_id_bytes(),
            TransferOperation::ChunkUpload,
            auth.transfer_id,
            auth.artifact_id,
            TransferDirection::AgentToServer,
            Some(2),
            &decode_proof_id(&proof.proof_id_wire),
            proof.issued_at_millis,
        );
        let vk = VerifyingKey::from_bytes(&auth.proof_key.public_key_bytes()).unwrap();
        let sig_bytes: [u8; 64] = URL_SAFE_NO_PAD
            .decode(proof.signature_wire())
            .unwrap()
            .try_into()
            .unwrap();
        assert!(vk
            .verify(&transcript, &Signature::from_bytes(&sig_bytes))
            .is_ok());
    }

    fn decode_proof_id(wire: &str) -> [u8; 16] {
        URL_SAFE_NO_PAD.decode(wire).unwrap().try_into().unwrap()
    }
}
