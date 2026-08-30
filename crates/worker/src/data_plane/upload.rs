//! Streaming composition of the `chunk_upload` body into the Phase D1 storage
//! mechanism (Issue #39 Phase E2B).
//!
//! The HTTP request body is pumped **frame by frame** through a small bounded
//! channel into one `spawn_blocking` staging worker that owns the
//! [`StagingChunk`] for its whole life. Peak memory stays at a few in-flight
//! body frames — a whole chunk is never buffered (D1's no-whole-chunk-in-
//! memory guarantee; `m0-data-plane-and-storage-contracts.md` "Durable chunk
//! acceptance ordering"). Backpressure: when the channel is full the async
//! pump stops polling the body, which stops reading the socket.
//!
//! This module performs **only** the mechanical stage -> hash -> validate ->
//! finalize steps. It sends nothing over the UDS and makes no authorization
//! or `Verified`/`Failed` decision; the caller (`http::chunk_upload`) drives
//! E1 `authorize_chunk` before calling in and E1 `commit_chunk` after.

use axum::body::{Body, Bytes};
use http_body_util::BodyExt;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::storage::{ChunkStore, FilesystemChunkStore, StagingChunk, StorageError};

/// How many body frames may sit buffered between the async pump and the
/// blocking staging worker. Small: this only smooths over the boundary
/// between reading a frame and writing it, never holds a chunk.
const FRAME_CHANNEL_CAPACITY: usize = 4;

/// The already-authorized inputs the staging composition needs. `chunk_size`
/// and `declared_digest` are authoritative — `chunk_size` from the approved
/// `AuthorizationDecision`, `declared_digest` the canonical `X-Bamep-Chunk-
/// Digest` value the Worker already validated for shape.
pub(super) struct StageRequest {
    pub transfer_id: Uuid,
    pub chunk_index: u64,
    pub chunk_size: u32,
    pub declared_digest: String,
}

/// The mechanical result of staging one body. The caller maps each to an
/// exact HTTP status; nothing here is itself durable.
pub(super) enum StageOutcome {
    /// D1 published a restart-stable final (fresh install or a byte-identical
    /// already-present file). Carries the Worker-verified digest and exact
    /// received size for the `ChunkAcceptanceRequest`.
    Finalized { size: u32, digest: String },
    /// The body carried zero bytes — cannot represent a `1..=chunk_size`
    /// chunk.
    EmptyBody,
    /// The received bytes exceeded the authoritative `chunk_size`.
    TooLarge,
    /// The received bytes hash to a value other than the declared digest.
    DigestMismatch,
    /// A *different* chunk is already finalized locally at this identity
    /// (restart-stable residue whose bytes differ). Fails closed.
    LocalIdentityConflict,
    /// The local storage mechanism failed (staging I/O, fsync, placement),
    /// the client broke the upload stream, or the blocking task did not
    /// complete. Fails closed.
    StorageUnavailable,
}

/// One message from the async body pump to the blocking staging worker.
enum PumpMessage {
    Data(Bytes),
    /// The body stream ended abnormally (client reset, transport error).
    /// The staging worker must discard and fail closed — a truncated upload
    /// must never be finalized.
    Truncated,
}

/// Streams `body` into a fresh D1 staging file, validates size + digest, and
/// finalizes into a restart-stable no-replace final iff the bytes are valid.
pub(super) async fn stage_chunk_body(
    store: FilesystemChunkStore,
    request: StageRequest,
    body: Body,
) -> StageOutcome {
    let (tx, rx) = mpsc::channel::<PumpMessage>(FRAME_CHANNEL_CAPACITY);
    let worker = tokio::task::spawn_blocking(move || stage_worker(store, request, rx));

    let mut body = body;
    loop {
        match body.frame().await {
            Some(Ok(frame)) => {
                let Ok(data) = frame.into_data() else {
                    // A trailers-only frame — no payload bytes.
                    continue;
                };
                if data.is_empty() {
                    continue;
                }
                if tx.send(PumpMessage::Data(data)).await.is_err() {
                    // The staging worker already stopped (oversize / I/O
                    // error). Stop reading; its outcome stands.
                    break;
                }
            }
            Some(Err(_)) => {
                let _ = tx.send(PumpMessage::Truncated).await;
                break;
            }
            None => break,
        }
    }
    drop(tx);

    match worker.await {
        Ok(outcome) => outcome,
        Err(_) => StageOutcome::StorageUnavailable,
    }
}

/// The blocking half: owns the [`StagingChunk`] and never yields it across an
/// `await`. Returns as soon as the outcome is known.
fn stage_worker(
    store: FilesystemChunkStore,
    request: StageRequest,
    mut rx: mpsc::Receiver<PumpMessage>,
) -> StageOutcome {
    let StageRequest {
        transfer_id,
        chunk_index,
        chunk_size,
        declared_digest,
    } = request;

    let mut staging: StagingChunk =
        match store.begin_stage(transfer_id, chunk_index, u64::from(chunk_size)) {
            Ok(staging) => staging,
            Err(_) => return StageOutcome::StorageUnavailable,
        };

    while let Some(message) = rx.blocking_recv() {
        match message {
            PumpMessage::Data(bytes) => {
                if let Err(err) = staging.write(&bytes) {
                    let outcome = match err {
                        StorageError::Oversize { .. } => StageOutcome::TooLarge,
                        _ => StageOutcome::StorageUnavailable,
                    };
                    staging.discard();
                    return outcome;
                }
            }
            PumpMessage::Truncated => {
                staging.discard();
                return StageOutcome::StorageUnavailable;
            }
        }
    }

    let size = staging.staged_len();
    if size == 0 {
        staging.discard();
        return StageOutcome::EmptyBody;
    }

    // Both values are canonical base64url-no-pad SHA-256 text and public
    // integrity identities, not secrets — a plain canonical-string compare is
    // the contract's identity test (`m0-...` "Durable chunk acceptance
    // ordering", step 5).
    let computed = staging.digest().to_base64url_no_pad();
    if computed != declared_digest {
        staging.discard();
        return StageOutcome::DigestMismatch;
    }

    let size = match u32::try_from(size) {
        Ok(size) => size,
        // Unreachable: `size <= chunk_size <= u32::MAX`. Fail closed anyway.
        Err(_) => {
            staging.discard();
            return StageOutcome::StorageUnavailable;
        }
    };

    match staging.finalize() {
        Ok(_finalized) => StageOutcome::Finalized {
            size,
            digest: computed,
        },
        Err(StorageError::FinalizedChunkConflict { .. }) => StageOutcome::LocalIdentityConflict,
        Err(StorageError::EmptyChunk) => StageOutcome::EmptyBody,
        Err(_) => StageOutcome::StorageUnavailable,
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use axum::body::Body;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use http_body::{Body as HttpBody, Frame};
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::storage::FilesystemChunkStore;

    fn digest_b64(bytes: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(Sha256::digest(bytes))
    }

    struct TempRoot(std::path::PathBuf);
    impl TempRoot {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("bamep-upload-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn store(&self) -> FilesystemChunkStore {
            FilesystemChunkStore::initialize(&self.0).expect("initialize store")
        }
        fn final_path(&self, transfer_id: Uuid, index: u64) -> std::path::PathBuf {
            self.0
                .join("transfers")
                .join(transfer_id.as_hyphenated().to_string())
                .join("chunks")
                .join(format!("{index}.chunk"))
        }
    }
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// One data frame, then a mid-stream transport error — a client that
    /// broke the upload connection.
    struct FrameThenError(Option<Bytes>);
    impl HttpBody for FrameThenError {
        type Data = Bytes;
        type Error = std::io::Error;
        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
            match self.0.take() {
                Some(bytes) => Poll::Ready(Some(Ok(Frame::data(bytes)))),
                None => Poll::Ready(Some(Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "peer went away",
                )))),
            }
        }
    }

    fn request(transfer_id: Uuid, chunk_size: u32, declared_digest: String) -> StageRequest {
        StageRequest {
            transfer_id,
            chunk_index: 0,
            chunk_size,
            declared_digest,
        }
    }

    #[tokio::test]
    async fn valid_body_finalizes_a_restart_stable_file() {
        let root = TempRoot::new();
        let transfer_id = Uuid::new_v4();
        let payload = vec![9u8; 2048];
        let outcome = stage_chunk_body(
            root.store(),
            request(transfer_id, 4096, digest_b64(&payload)),
            Body::from(payload.clone()),
        )
        .await;
        match outcome {
            StageOutcome::Finalized { size, digest } => {
                assert_eq!(size, 2048);
                assert_eq!(digest, digest_b64(&payload));
            }
            other => panic!("expected Finalized, got {:?}", DebugOutcome(&other)),
        }
        assert_eq!(
            std::fs::read(root.final_path(transfer_id, 0)).unwrap(),
            payload
        );
    }

    #[tokio::test]
    async fn a_body_that_errors_mid_stream_fails_closed_and_finalizes_nothing() {
        let root = TempRoot::new();
        let transfer_id = Uuid::new_v4();
        let body = Body::new(FrameThenError(Some(Bytes::from(vec![1u8; 512]))));
        let outcome = stage_chunk_body(
            root.store(),
            request(transfer_id, 65536, "A".repeat(43)),
            body,
        )
        .await;
        assert!(
            matches!(outcome, StageOutcome::StorageUnavailable),
            "a truncated upload must never finalize"
        );
        assert!(!root.final_path(transfer_id, 0).exists());
    }

    #[tokio::test]
    async fn an_empty_body_is_reported_as_empty() {
        let root = TempRoot::new();
        let outcome = stage_chunk_body(
            root.store(),
            request(Uuid::new_v4(), 4096, digest_b64(&[])),
            Body::empty(),
        )
        .await;
        assert!(matches!(outcome, StageOutcome::EmptyBody));
    }

    #[tokio::test]
    async fn a_body_over_chunk_size_is_reported_too_large() {
        let root = TempRoot::new();
        let payload = vec![2u8; 5000];
        let outcome = stage_chunk_body(
            root.store(),
            request(Uuid::new_v4(), 4096, digest_b64(&payload)),
            Body::from(payload),
        )
        .await;
        assert!(matches!(outcome, StageOutcome::TooLarge));
    }

    #[tokio::test]
    async fn bytes_that_disagree_with_the_declared_digest_are_a_mismatch() {
        let root = TempRoot::new();
        let transfer_id = Uuid::new_v4();
        let outcome = stage_chunk_body(
            root.store(),
            request(transfer_id, 4096, digest_b64(&[0u8; 100])),
            Body::from(vec![1u8; 100]),
        )
        .await;
        assert!(matches!(outcome, StageOutcome::DigestMismatch));
        assert!(!root.final_path(transfer_id, 0).exists());
    }

    // `StageOutcome` deliberately carries no `Debug`; this is test-only.
    struct DebugOutcome<'a>(&'a StageOutcome);
    impl std::fmt::Debug for DebugOutcome<'_> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let name = match self.0 {
                StageOutcome::Finalized { .. } => "Finalized",
                StageOutcome::EmptyBody => "EmptyBody",
                StageOutcome::TooLarge => "TooLarge",
                StageOutcome::DigestMismatch => "DigestMismatch",
                StageOutcome::LocalIdentityConflict => "LocalIdentityConflict",
                StageOutcome::StorageUnavailable => "StorageUnavailable",
            };
            f.write_str(name)
        }
    }
}
