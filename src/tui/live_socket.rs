//! Client for a daemon's `/sessions/{id}/live-ws` capture stream, the same
//! stream the web dashboard renders. Frames arrive as ANSI text and keystrokes
//! go back as raw pane bytes, so no tmux client runs on this machine.
//!
//! Frames ride the compressed binary stream (`caps.deflate`): consecutive
//! screens are near-identical, so one connection-lifetime dictionary turns each
//! into back-references. That matters most for an alternate-screen app, whose
//! every scroll rewrites all its rows and so always ships as a full frame.

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::acp::client::discovery::DaemonEndpoint;
use crate::daemon::websocket::{self, NativeSocket};
use crate::tmux::PaneCursor;

/// What the reader task hands its owner.
#[derive(Debug)]
pub(crate) enum LiveMessage {
    Frame {
        seq: Option<u64>,
        content: String,
        cursor: PaneCursor,
    },
    /// Row changes against the message numbered `base` (module doc of
    /// `crate::server::live_ws`).
    Patch {
        seq: u64,
        base: u64,
        shift: usize,
        lines: Vec<(usize, String)>,
        cursor: PaneCursor,
    },
    SizeOwner {
        is_owner: bool,
        holder: Option<String>,
    },
    Closed(String),
}

/// The pane the frame describes, as this machine's own capture would report
/// it.
///
/// Two fields deliberately do not mean what their local namesakes do. The
/// daemon has already moved the cursor onto the window grid, so pane 0's
/// origin is dropped rather than carried: the cursor painter adds
/// `composite_pane0`'s origin itself, and carrying it would place the cursor
/// twice as far in. `pane_width` has no wire field at all.
fn pane_cursor(meta: &crate::daemon::LivePaneMeta) -> PaneCursor {
    PaneCursor {
        x: meta.cursor.map_or(0, |c| c.x),
        y: meta.cursor.map_or(0, |c| c.y),
        visible: meta.cursor.is_some(),
        pane_height: meta.rows,
        history_size: meta.history,
        pane_width: 0,
        alternate_on: meta.alt_screen,
        mouse_tracking: meta.mouse,
        mouse_sgr: meta.mouse_sgr,
        mouse_all: meta.mouse_all,
        position_reliable: true,
        composite_pane0: meta.pane0.map(|p| crate::tmux::PaneGeom {
            left: 0,
            top: 0,
            width: p.cols,
            height: p.rows,
        }),
    }
}

fn parse_text(text: &str) -> Option<LiveMessage> {
    // A message this build does not know, or one that cannot be applied (a
    // patch with no base), is dropped rather than guessed at.
    match serde_json::from_str::<crate::daemon::LiveServerMessage>(text).ok()? {
        crate::daemon::LiveServerMessage::Frame { seq, content, meta } => {
            Some(LiveMessage::Frame {
                seq,
                cursor: pane_cursor(&meta),
                content,
            })
        }
        crate::daemon::LiveServerMessage::Patch {
            seq,
            base,
            shift,
            lines,
            meta,
        } => Some(LiveMessage::Patch {
            seq,
            base,
            shift,
            cursor: pane_cursor(&meta),
            lines,
        }),
        crate::daemon::LiveServerMessage::SizeOwner { is_owner, holder } => {
            Some(LiveMessage::SizeOwner { is_owner, holder })
        }
        // The TUI drives its own clipboard and does not choose a transport.
        crate::daemon::LiveServerMessage::Clipboard { .. }
        | crate::daemon::LiveServerMessage::Transport { .. } => None,
    }
}

/// Inflates the daemon's `caps.deflate` stream: one raw-deflate stream for the
/// whole connection, sync-flushed per frame, whose plaintext is a run of
/// `u32-LE length || frame JSON` records. The inflater chunks independently of
/// those records, so a partial one is carried to the next binary message.
#[derive(Default)]
struct FrameInflater {
    stream: Option<flate2::Decompress>,
    plain: Vec<u8>,
}

impl FrameInflater {
    /// Frame JSON records completed by one binary message, or `None` once the
    /// stream is corrupt, which no later message can recover from.
    fn push(&mut self, bytes: &[u8]) -> Option<Vec<String>> {
        let stream = self
            .stream
            .get_or_insert_with(|| flate2::Decompress::new(false));
        let mut consumed = 0usize;
        loop {
            self.plain.reserve(4096);
            let before_in = stream.total_in();
            let before_out = self.plain.len();
            stream
                .decompress_vec(
                    &bytes[consumed..],
                    &mut self.plain,
                    flate2::FlushDecompress::Sync,
                )
                .ok()?;
            consumed += (stream.total_in() - before_in) as usize;
            // All input taken and the inflater left spare room, so nothing is
            // still pending inside it.
            if consumed == bytes.len() && self.plain.len() < self.plain.capacity() {
                break;
            }
            // Neither side moved: the stream cannot make progress, so treat it
            // as corrupt rather than spinning.
            if stream.total_in() == before_in && self.plain.len() == before_out {
                return None;
            }
        }
        Some(self.take_records())
    }

    fn take_records(&mut self) -> Vec<String> {
        let mut records = Vec::new();
        let mut pos = 0usize;
        while self.plain.len() - pos >= 4 {
            let len = u32::from_le_bytes(self.plain[pos..pos + 4].try_into().unwrap_or_default());
            let len = len as usize;
            if self.plain.len() - pos - 4 < len {
                break;
            }
            if let Ok(text) = std::str::from_utf8(&self.plain[pos + 4..pos + 4 + len]) {
                records.push(text.to_string());
            }
            pos += 4 + len;
        }
        self.plain.drain(..pos);
        records
    }
}

pub(crate) struct LiveSocket {
    pub(crate) rx: mpsc::Receiver<LiveMessage>,
    pub(crate) tx: mpsc::Sender<Message>,
    pub(crate) task: JoinHandle<()>,
}

/// Credentials travel in headers via the shared native WebSocket transport,
/// which also refuses them over non-loopback plaintext.
pub(crate) async fn connect(endpoint: &DaemonEndpoint, session_id: &str) -> Result<LiveSocket> {
    let session_id = crate::daemon::transport::path_segment(session_id)?;
    let path = format!("/sessions/{session_id}/live-ws");
    let (in_tx, in_rx) = mpsc::channel(64);
    let (out_tx, out_rx) = mpsc::channel(64);
    let task = match websocket::connect(endpoint, &path, None).await? {
        NativeSocket::Unix(stream) => tokio::spawn(pump(*stream, in_tx, out_rx)),
        NativeSocket::Tcp(stream) => tokio::spawn(pump(*stream, in_tx, out_rx)),
    };
    Ok(LiveSocket {
        rx: in_rx,
        tx: out_tx,
        task,
    })
}

async fn pump<S>(
    mut stream: WebSocketStream<S>,
    tx: mpsc::Sender<LiveMessage>,
    mut out_rx: mpsc::Receiver<Message>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut inflater = FrameInflater::default();
    loop {
        tokio::select! {
            outbound = out_rx.recv() => {
                let Some(msg) = outbound else { break };
                if stream.send(msg).await.is_err() {
                    break;
                }
            }
            inbound = stream.next() => {
                match inbound {
                    Some(Ok(Message::Text(text))) => {
                        if let Some(msg) = parse_text(text.as_str()) {
                            if tx.send(msg).await.is_err() {
                                break;
                            }
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        let Some(records) = inflater.push(&bytes) else {
                            let _ = tx
                                .send(LiveMessage::Closed(
                                    "the live frame stream could not be decompressed".to_string(),
                                ))
                                .await;
                            break;
                        };
                        let mut closed = false;
                        for record in records {
                            if let Some(msg) = parse_text(&record) {
                                if tx.send(msg).await.is_err() {
                                    closed = true;
                                    break;
                                }
                            }
                        }
                        if closed {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        let _ = tx
                            .send(LiveMessage::Closed(
                                "the daemon closed the live stream".to_string(),
                            ))
                            .await;
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        let _ = tx.send(LiveMessage::Closed(error.to_string())).await;
                        break;
                    }
                }
            }
        }
    }
    let _ = stream.close(None).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The daemon's `caps.deflate` encoder: one raw-deflate stream, sync
    /// flushed per frame, each record prefixed with its `u32-LE` length.
    fn deflate_records(records: &[&str]) -> Vec<Vec<u8>> {
        let mut stream = flate2::Compress::new(flate2::Compression::fast(), false);
        records
            .iter()
            .map(|json| {
                let mut input = (json.len() as u32).to_le_bytes().to_vec();
                input.extend_from_slice(json.as_bytes());
                let mut out = Vec::new();
                let mut consumed = 0usize;
                loop {
                    out.reserve(1024);
                    let before = stream.total_in();
                    stream
                        .compress_vec(&input[consumed..], &mut out, flate2::FlushCompress::Sync)
                        .unwrap();
                    consumed += (stream.total_in() - before) as usize;
                    if consumed == input.len() && out.len() < out.capacity() {
                        return out;
                    }
                }
            })
            .collect()
    }

    #[test]
    fn the_inflater_rebuilds_frames_however_the_binary_messages_are_split() {
        let frames: Vec<String> = (1..=3)
            .map(|seq| {
                format!(
                    r#"{{"type":"frame","seq":{seq},"content":"row {seq}\nrow {seq}\n","rows":2,"cursor":null}}"#
                )
            })
            .collect();
        let borrowed: Vec<&str> = frames.iter().map(String::as_str).collect();
        let wire = deflate_records(&borrowed);

        let mut whole = FrameInflater::default();
        let got: Vec<String> = wire
            .iter()
            .flat_map(|m| whole.push(m).expect("stream stays valid"))
            .collect();
        assert_eq!(got, frames, "one binary message per frame");

        // The inflater must not assume message boundaries land on records: a
        // split mid-record has to carry the remainder to the next message.
        let mut split = FrameInflater::default();
        let mut got = Vec::new();
        for message in &wire {
            let (head, tail) = message.split_at(message.len() / 2);
            got.extend(split.push(head).expect("stream stays valid"));
            got.extend(split.push(tail).expect("stream stays valid"));
        }
        assert_eq!(got, frames, "records split across binary messages");

        assert!(
            FrameInflater::default()
                .push(b"not deflate at all")
                .is_none(),
            "a corrupt stream is reported, not silently dropped"
        );
    }

    #[test]
    fn an_inflated_frame_parses_like_a_text_one() {
        let json = r#"{"type":"frame","seq":4,"content":"hi\n","rows":1,"history":7,"cursor":{"x":2,"y":0},"altScreen":true,"mouse":true,"mouseSgr":true}"#;
        let wire = deflate_records(&[json]);
        let mut inflater = FrameInflater::default();
        let records = inflater.push(&wire[0]).expect("stream stays valid");
        let [record] = records.as_slice() else {
            panic!("expected one record, got {records:?}");
        };
        match parse_text(record).expect("frame parses") {
            LiveMessage::Frame {
                seq,
                content,
                cursor,
            } => {
                assert_eq!((seq, content.as_str()), (Some(4), "hi\n"));
                assert!(cursor.alternate_on && cursor.mouse_sgr);
                assert_eq!((cursor.x, cursor.y, cursor.history_size), (2, 0, 7));
            }
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    #[test]
    fn a_frame_carries_the_cursor_and_the_panes_scroll_modes() {
        let cases = [
            (
                r#"{"type":"frame","seq":7,"content":"hello\n","rows":2,"history":40,"cursor":{"x":3,"y":1},"altScreen":false,"mouse":false,"mouseSgr":false}"#,
                (3, 1, true, 2, 40, false, false, false),
            ),
            (
                r#"{"type":"frame","content":"x\n","rows":5,"cursor":null,"altScreen":true,"mouse":true,"mouseSgr":true}"#,
                (0, 0, false, 5, 0, true, true, true),
            ),
        ];
        for (text, expected) in cases {
            match parse_text(text).expect("frame parses") {
                LiveMessage::Frame { cursor: c, .. } => assert_eq!(
                    (
                        c.x,
                        c.y,
                        c.visible,
                        c.pane_height,
                        c.history_size,
                        c.alternate_on,
                        c.mouse_tracking,
                        c.mouse_sgr
                    ),
                    expected,
                    "{text}"
                ),
                other => panic!("expected a frame, got {other:?}"),
            }
        }
    }

    /// Hover forwarding and pointer mapping read these two fields, so a frame
    /// that drops them leaves a remote pane feeling unlike a local one.
    #[test]
    fn a_frame_carries_what_the_pointer_paths_read() {
        let with = r#"{"type":"frame","content":"x\n","rows":5,"cursor":null,"altScreen":true,"mouse":true,"mouseSgr":true,"mouseAll":true,"pane0":{"cols":40,"rows":10,"left":41,"top":0}}"#;
        match parse_text(with).expect("frame parses") {
            LiveMessage::Frame { cursor, .. } => {
                assert!(cursor.mouse_all, "bare motion is forwarded");
                assert_eq!(
                    cursor.composite_pane0.map(|p| (p.width, p.height)),
                    Some((40, 10)),
                    "input is pinned to pane 0 of a split window"
                );
                assert_eq!(
                    cursor.composite_pane0.map(|p| (p.left, p.top)),
                    Some((0, 0)),
                    "the origin is already in the cursor; adding it twice moves it"
                );
            }
            other => panic!("expected a frame, got {other:?}"),
        }
        // A server that predates the fields: button tracking still works, and
        // an unsplit frame reports no pane rectangle at all.
        let without = r#"{"type":"frame","content":"x\n","rows":5,"cursor":null,"altScreen":true,"mouse":true,"mouseSgr":true}"#;
        match parse_text(without).expect("frame parses") {
            LiveMessage::Frame { cursor, .. } => {
                assert!(!cursor.mouse_all);
                assert!(cursor.composite_pane0.is_none());
            }
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    #[test]
    fn a_patch_carries_its_base_rows_and_cursor() {
        let text = r#"{"type":"patch","seq":8,"base":7,"shift":2,"lines":[[0,"a"],[3,"\u001b[1mb"]],"rows":4,"history":12,"cursor":{"x":1,"y":3},"altScreen":false,"mouse":false,"mouseSgr":false,"pane0":null}"#;
        match parse_text(text).expect("patch parses") {
            LiveMessage::Patch {
                seq,
                base,
                shift,
                lines,
                cursor,
            } => {
                assert_eq!((seq, base, shift), (8, 7, 2));
                assert_eq!(lines, [(0, "a".into()), (3, "\u{1b}[1mb".into())]);
                assert_eq!((cursor.x, cursor.y, cursor.history_size), (1, 3, 12));
            }
            other => panic!("expected a patch, got {other:?}"),
        }
        assert!(
            parse_text(r#"{"type":"patch","seq":8,"lines":[]}"#).is_none(),
            "a patch without a base cannot be applied"
        );
    }

    #[test]
    fn reads_ownership_notices() {
        assert!(matches!(
            parse_text(r#"{"type":"size_owner","is_owner":false}"#),
            Some(LiveMessage::SizeOwner {
                is_owner: false,
                holder: None
            })
        ));
        match parse_text(r#"{"type":"size_owner","is_owner":false,"holder":"mac-mini (aoe)"}"#) {
            Some(LiveMessage::SizeOwner { holder, .. }) => {
                assert_eq!(holder.as_deref(), Some("mac-mini (aoe)"))
            }
            other => panic!("expected an ownership notice, got {other:?}"),
        }
    }

    #[test]
    fn unknown_and_malformed_messages_are_dropped() {
        // A message type this client ignores must not be parsed as a frame.
        assert!(parse_text(r#"{"type":"clipboard","text":"hi"}"#).is_none());
        assert!(parse_text(r#"{"type":"transport","grid":false}"#).is_none());
        assert!(parse_text("not json").is_none());
    }

    #[tokio::test]
    async fn credentials_are_refused_over_non_loopback_plaintext() {
        use crate::acp::client::discovery::{DaemonEndpoint, Source};
        let login = crate::daemon::SessionCredential {
            session: "s".into(),
            binding: "b".into(),
        };
        for endpoint in [
            DaemonEndpoint::new(
                "http://mini.example.com:8080".into(),
                Some("t".into()),
                Source::Remote,
            ),
            DaemonEndpoint::new("http://mini.example.com:8080".into(), None, Source::Remote)
                .with_login(Some(login)),
        ] {
            let err = connect(&endpoint, "id").await.err().expect("must refuse");
            assert!(matches!(
                err.downcast_ref::<websocket::WsError>(),
                Some(websocket::WsError::Daemon(
                    crate::daemon::DaemonClientError::InsecureBearerTransport
                ))
            ));
        }
    }
}
