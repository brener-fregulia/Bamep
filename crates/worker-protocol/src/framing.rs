//! Explicit length-prefixed framing for the Worker IPC v1 UDS transport
//! (`m1-worker-data-plane-control-contract.md` "Transport, framing, and
//! versioning"):
//!
//! ```text
//! frame := u32be(byte_length_of_json_payload) || utf8_json_payload
//! ```
//!
//! `byte_length_of_json_payload` is the exact UTF-8 byte length of the
//! payload. No Rust enum layout, `bincode`, native-serialization framing, or
//! newline-delimited JSON — a plain 4-byte big-endian length prefix over the
//! generic [`tokio::io::AsyncRead`]/[`tokio::io::AsyncWrite`] traits, so
//! this module has no opinion on the concrete stream type (a Unix Domain
//! Socket in production, an in-memory duplex pipe in tests).
//!
//! A declared length above [`MAX_FRAME_PAYLOAD_BYTES`] is a protocol
//! violation: [`read_frame`] returns [`FrameReadError::OversizedFrame`]
//! without ever allocating or reading the announced oversized payload.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::codec::{self, DecodeError, EncodeError};
use crate::messages::WorkerProtocolMessage;

/// Maximum JSON payload size: exactly 1 MiB
/// (`m1-worker-data-plane-control-contract.md` "Transport, framing, and
/// versioning": "Maximum frame size is **1 MiB**").
pub const MAX_FRAME_PAYLOAD_BYTES: u32 = 1024 * 1024;

const LENGTH_PREFIX_BYTES: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum FrameReadError {
    /// The connection closed (cleanly or mid-frame) before a complete frame
    /// was available — never treated as a successfully received empty
    /// message.
    #[error("connection closed while reading a frame")]
    Eof,
    /// The declared length prefix exceeds [`MAX_FRAME_PAYLOAD_BYTES`]. The
    /// announced payload is never allocated or read once this is detected.
    #[error("declared frame length {declared} exceeds the {max}-byte maximum")]
    OversizedFrame { declared: u32, max: u32 },
    #[error("i/o error reading frame")]
    Io(#[source] std::io::Error),
    /// The frame's declared byte length was read successfully, but the
    /// payload bytes are not valid UTF-8 — a framing-level violation
    /// distinct from a JSON syntax error.
    #[error("frame payload is not valid UTF-8")]
    InvalidUtf8(#[source] std::str::Utf8Error),
}

#[derive(Debug, thiserror::Error)]
pub enum FrameWriteError {
    /// This side attempted to send a payload above
    /// [`MAX_FRAME_PAYLOAD_BYTES`] — refused locally before writing anything
    /// to the stream, rather than sending a frame the peer must reject.
    #[error("payload length {declared} exceeds the {max}-byte maximum")]
    OversizedFrame { declared: usize, max: usize },
    #[error("i/o error writing frame")]
    Io(#[source] std::io::Error),
}

/// Reads exactly one frame's raw payload bytes. Blocks (awaits) until either
/// a complete frame is available or the stream ends/errors. Internally loops
/// over partial reads via [`AsyncReadExt::read_exact`] — a peer that writes
/// the length prefix and payload across many small writes is handled
/// identically to one that writes them in a single call.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>, FrameReadError> {
    let mut length_prefix = [0u8; LENGTH_PREFIX_BYTES];
    read_exact_or_eof(reader, &mut length_prefix).await?;

    let declared = u32::from_be_bytes(length_prefix);
    if declared > MAX_FRAME_PAYLOAD_BYTES {
        return Err(FrameReadError::OversizedFrame {
            declared,
            max: MAX_FRAME_PAYLOAD_BYTES,
        });
    }

    let mut payload = vec![0u8; declared as usize];
    read_exact_or_eof(reader, &mut payload).await?;
    Ok(payload)
}

async fn read_exact_or_eof<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut [u8],
) -> Result<(), FrameReadError> {
    if buf.is_empty() {
        return Ok(());
    }
    match reader.read_exact(buf).await {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => Err(FrameReadError::Eof),
        Err(err) => Err(FrameReadError::Io(err)),
    }
}

/// Writes exactly one frame: the 4-byte big-endian length prefix followed by
/// `payload`, then flushes. Refuses locally (no bytes written) when
/// `payload` exceeds [`MAX_FRAME_PAYLOAD_BYTES`].
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    payload: &[u8],
) -> Result<(), FrameWriteError> {
    if payload.len() > MAX_FRAME_PAYLOAD_BYTES as usize {
        return Err(FrameWriteError::OversizedFrame {
            declared: payload.len(),
            max: MAX_FRAME_PAYLOAD_BYTES as usize,
        });
    }
    let length_prefix = (payload.len() as u32).to_be_bytes();
    writer
        .write_all(&length_prefix)
        .await
        .map_err(FrameWriteError::Io)?;
    writer
        .write_all(payload)
        .await
        .map_err(FrameWriteError::Io)?;
    writer.flush().await.map_err(FrameWriteError::Io)?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum ReceiveError {
    #[error(transparent)]
    Frame(#[from] FrameReadError),
    #[error(transparent)]
    Decode(#[from] DecodeError),
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error(transparent)]
    Frame(#[from] FrameWriteError),
    #[error(transparent)]
    Encode(#[from] EncodeError),
}

/// Reads and decodes exactly one [`WorkerProtocolMessage`] frame — the
/// combined framing + codec operation both `bamepd` and Worker use for every
/// inbound message.
pub async fn receive<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<WorkerProtocolMessage, ReceiveError> {
    let payload = read_frame(reader).await?;
    let text = std::str::from_utf8(&payload).map_err(FrameReadError::InvalidUtf8)?;
    Ok(codec::decode(text)?)
}

/// Encodes and writes exactly one [`WorkerProtocolMessage`] frame — the
/// combined codec + framing operation both `bamepd` and Worker use for every
/// outbound message.
pub async fn send<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &WorkerProtocolMessage,
) -> Result<(), SendError> {
    let text = codec::encode(message)?;
    write_frame(writer, text.as_bytes()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncWriteExt, DuplexStream};
    use uuid::Uuid;

    use super::*;
    use crate::messages::{WorkerHelloMessage, WorkerProtocolMessage};

    fn manual_frame(payload: &[u8]) -> Vec<u8> {
        let mut bytes = (payload.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    #[tokio::test]
    async fn round_trips_exact_u32be_framing() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let payload = b"hello worker ipc";
        write_frame(&mut a, payload).await.expect("write");
        let received = read_frame(&mut b).await.expect("read");
        assert_eq!(received, payload);
    }

    #[tokio::test]
    async fn utf8_byte_length_is_exact_for_multibyte_content() {
        // "café" is 4 chars but 5 UTF-8 bytes; the length prefix must be the
        // exact byte length, not a codepoint count.
        let (mut a, mut b) = tokio::io::duplex(4096);
        let payload = "café".as_bytes();
        assert_eq!(payload.len(), 5);
        write_frame(&mut a, payload).await.expect("write");
        let received = read_frame(&mut b).await.expect("read");
        assert_eq!(received, payload);
    }

    #[tokio::test]
    async fn multiple_frames_on_one_stream_are_each_read_independently() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        write_frame(&mut a, b"first").await.expect("write 1");
        write_frame(&mut a, b"second").await.expect("write 2");
        assert_eq!(read_frame(&mut b).await.expect("read 1"), b"first");
        assert_eq!(read_frame(&mut b).await.expect("read 2"), b"second");
    }

    #[tokio::test]
    async fn partial_reads_are_reassembled() {
        // A duplex with a tiny internal buffer forces the writer side to
        // hand off bytes in small pieces; read_frame must still reassemble
        // the complete length prefix and payload.
        let (mut a, mut b) = tokio::io::duplex(1);
        let payload = b"partial read reassembly proof";
        let frame = manual_frame(payload);
        let writer = tokio::spawn(async move {
            a.write_all(&frame).await.expect("write");
            a.flush().await.expect("flush");
            a
        });
        let received = read_frame(&mut b).await.expect("read");
        assert_eq!(received, payload);
        writer.await.expect("writer task");
    }

    #[tokio::test]
    async fn partial_writes_still_deliver_the_complete_frame() {
        let (mut a, mut b): (DuplexStream, DuplexStream) = tokio::io::duplex(1);
        let payload = b"partial write reassembly proof";
        let reader = tokio::spawn(async move {
            let received = read_frame(&mut b).await.expect("read");
            assert_eq!(received, payload);
        });
        write_frame(&mut a, payload).await.expect("write");
        reader.await.expect("reader task");
    }

    #[tokio::test]
    async fn oversized_declared_frame_is_rejected_before_payload_read() {
        let (mut a, mut b) = tokio::io::duplex(16);
        let oversized_len = MAX_FRAME_PAYLOAD_BYTES + 1;
        // Only the length prefix is ever sent. If read_frame attempted to
        // read the announced oversized payload, it would block/EOF waiting
        // for bytes that never arrive; getting `OversizedFrame` back proves
        // the payload read never started.
        a.write_all(&oversized_len.to_be_bytes())
            .await
            .expect("write length prefix");
        a.flush().await.expect("flush");
        drop(a);

        match read_frame(&mut b).await {
            Err(FrameReadError::OversizedFrame { declared, max }) => {
                assert_eq!(declared, oversized_len);
                assert_eq!(max, MAX_FRAME_PAYLOAD_BYTES);
            }
            other => panic!("expected OversizedFrame, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_frame_refuses_oversized_payload_locally() {
        let (mut a, _b) = tokio::io::duplex(16);
        let oversized = vec![0u8; MAX_FRAME_PAYLOAD_BYTES as usize + 1];
        let err = write_frame(&mut a, &oversized).await.unwrap_err();
        assert!(matches!(err, FrameWriteError::OversizedFrame { .. }));
    }

    #[tokio::test]
    async fn empty_stream_yields_eof_not_a_fabricated_message() {
        let (a, mut b) = tokio::io::duplex(16);
        drop(a);
        assert!(matches!(read_frame(&mut b).await, Err(FrameReadError::Eof)));
    }

    #[tokio::test]
    async fn eof_mid_payload_is_reported_as_eof_not_success() {
        let (mut a, mut b) = tokio::io::duplex(16);
        a.write_all(&10u32.to_be_bytes()).await.expect("write len");
        a.write_all(b"short").await.expect("write short payload");
        a.flush().await.expect("flush");
        drop(a);
        assert!(matches!(read_frame(&mut b).await, Err(FrameReadError::Eof)));
    }

    #[tokio::test]
    async fn malformed_json_payload_surfaces_as_decode_error() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        write_frame(&mut a, b"{not json").await.expect("write");
        assert!(matches!(
            receive(&mut b).await,
            Err(ReceiveError::Decode(_))
        ));
    }

    #[tokio::test]
    async fn send_and_receive_round_trip_a_real_message() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let instance_id = Uuid::new_v4();
        let message = WorkerProtocolMessage::WorkerHello(WorkerHelloMessage::new(instance_id));
        send(&mut a, &message).await.expect("send");
        let received = receive(&mut b).await.expect("receive");
        match received {
            WorkerProtocolMessage::WorkerHello(m) => {
                assert_eq!(m.body.worker_instance_id, instance_id)
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }
}
