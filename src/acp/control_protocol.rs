//! Typed control channel between the daemon and the `aoe __acp-runner`
//! shim, carried on a sibling `<id>.control.sock` alongside the raw ACP
//! byte relay on `<id>.sock`.
//!
//! Phase A of #1054 (runner-side ACP protocol termination): the runner
//! observes the agent's response to the daemon-issued `session/prompt`
//! request and reports a native turn-complete signal over this channel,
//! so the daemon fires `Stopped { reason: "prompt_complete" }`
//! deterministically instead of guessing with the 30s resume-idle
//! watchdog. Later phases move the ACP handshake and the agent's
//! server-method callbacks onto this channel too.
//!
//! Wire format: each frame is a 4-byte big-endian length prefix followed
//! by that many bytes of JSON (a serialized [`ControlBody`]). The byte
//! relay on `<id>.sock` stays newline-delimited JSON; this channel uses
//! length framing so a future opaque, possibly-nested payload cannot be
//! confused with a newline in the body.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Bumped when the frame set changes in a wire-incompatible way. The
/// runner announces it in [`ControlBody::Hello`]; a daemon that does not
/// recognize the version keeps the legacy resume-idle watchdog rather
/// than trusting the channel.
pub const CONTROL_PROTOCOL_VERSION: u32 = 1;

/// Hard cap on a single control frame. Phase A frames are tiny; reject
/// anything larger as a framing error instead of allocating a huge
/// buffer for a corrupt length prefix.
pub const MAX_CONTROL_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// A single control frame. `kind` tags the variant so the wire form is
/// self-describing and forward-compatible: an unknown variant fails to
/// deserialize rather than being silently misread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControlBody {
    // ---- runner -> daemon ----
    /// First frame the runner sends on a fresh control connection. Lets
    /// the daemon confirm the protocol version and the session identity
    /// it dialed.
    Hello {
        control_protocol_version: u32,
        session_id: String,
    },
    /// The runner observed the agent's response to the tracked
    /// `session/prompt` request. `prompt_req_id` is the JSON-RPC id the
    /// daemon issued for that prompt. `stop_reason` is the ACP
    /// `stopReason` from the response result when present (None for an
    /// error-envelope response, which still ends the turn).
    PromptCompleted {
        prompt_req_id: i64,
        stop_reason: Option<String>,
    },

    // ---- daemon -> runner ----
    /// First frame the daemon sends after [`ControlBody::Hello`],
    /// acknowledging the version it will speak. Phase A carries no other
    /// daemon-to-runner traffic.
    Attach { control_protocol_version: u32 },
}

/// Encode a frame: 4-byte big-endian length prefix, then the JSON body.
pub fn encode_frame(body: &ControlBody) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(body)?;
    let len = u32::try_from(json.len())
        .map_err(|_| anyhow::anyhow!("control frame exceeds u32 length"))?;
    if len > MAX_CONTROL_FRAME_BYTES {
        bail!("control frame {len} bytes exceeds cap {MAX_CONTROL_FRAME_BYTES}");
    }
    let mut buf = Vec::with_capacity(4 + json.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&json);
    Ok(buf)
}

/// Write one frame and flush.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, body: &ControlBody) -> Result<()> {
    let buf = encode_frame(body)?;
    w.write_all(&buf).await?;
    w.flush().await?;
    Ok(())
}

/// Read one frame. Returns `Ok(None)` on a clean EOF at a frame boundary
/// (the peer closed the socket), so callers can treat that as a normal
/// disconnect rather than an error.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<ControlBody>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_CONTROL_FRAME_BYTES {
        bail!("control frame length {len} exceeds cap {MAX_CONTROL_FRAME_BYTES}");
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    let parsed: ControlBody = serde_json::from_slice(&body)?;
    Ok(Some(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn roundtrip(body: ControlBody) -> ControlBody {
        let encoded = encode_frame(&body).expect("encode");
        // Length prefix plus a body that deserializes back to the same value.
        let len = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]);
        assert_eq!(len as usize, encoded.len() - 4);
        serde_json::from_slice(&encoded[4..]).expect("decode")
    }

    #[test]
    fn hello_roundtrips() {
        let body = ControlBody::Hello {
            control_protocol_version: CONTROL_PROTOCOL_VERSION,
            session_id: "abc-123".into(),
        };
        assert_eq!(roundtrip(body.clone()), body);
    }

    #[test]
    fn prompt_completed_roundtrips() {
        let body = ControlBody::PromptCompleted {
            prompt_req_id: 42,
            stop_reason: Some("end_turn".into()),
        };
        assert_eq!(roundtrip(body.clone()), body);
    }

    #[tokio::test]
    async fn write_then_read_frame() {
        let body = ControlBody::PromptCompleted {
            prompt_req_id: 7,
            stop_reason: None,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &body).await.expect("write");
        let mut cursor = Cursor::new(buf);
        let got = read_frame(&mut cursor).await.expect("read");
        assert_eq!(got, Some(body));
    }

    #[tokio::test]
    async fn multiple_frames_in_one_stream() {
        let a = ControlBody::Hello {
            control_protocol_version: 1,
            session_id: "s".into(),
        };
        let b = ControlBody::PromptCompleted {
            prompt_req_id: 1,
            stop_reason: Some("cancelled".into()),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &a).await.unwrap();
        write_frame(&mut buf, &b).await.unwrap();
        let mut cursor = Cursor::new(buf);
        assert_eq!(read_frame(&mut cursor).await.unwrap(), Some(a));
        assert_eq!(read_frame(&mut cursor).await.unwrap(), Some(b));
        assert_eq!(read_frame(&mut cursor).await.unwrap(), None);
    }

    #[tokio::test]
    async fn clean_eof_returns_none() {
        let mut cursor = Cursor::new(Vec::new());
        assert_eq!(read_frame(&mut cursor).await.unwrap(), None);
    }

    #[tokio::test]
    async fn oversized_length_prefix_is_rejected() {
        // Length prefix past the cap, with no body: must error, not
        // attempt a multi-gigabyte allocation.
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_CONTROL_FRAME_BYTES + 1).to_be_bytes());
        let mut cursor = Cursor::new(buf);
        assert!(read_frame(&mut cursor).await.is_err());
    }

    #[tokio::test]
    async fn truncated_body_is_error_not_eof() {
        // A full length prefix but a short body is a corrupt frame, not a
        // clean close.
        let mut buf = Vec::new();
        buf.extend_from_slice(&16u32.to_be_bytes());
        buf.extend_from_slice(b"only-4"); // fewer than 16 bytes
        let mut cursor = Cursor::new(buf);
        assert!(read_frame(&mut cursor).await.is_err());
    }
}
