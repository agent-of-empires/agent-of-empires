//! Client for a daemon's `/sessions/{id}/live-ws` capture stream, the same
//! stream the web dashboard renders. Frames arrive as ANSI text and keystrokes
//! go back as raw pane bytes, so no tmux client runs on this machine.

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::acp::client::discovery::DaemonEndpoint;
use crate::daemon::websocket::{self, NativeSocket};
use crate::tmux::PaneCursor;

#[derive(Debug, Deserialize)]
struct WireCursor {
    x: u16,
    y: u16,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireMessage {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    seq: Option<u64>,
    #[serde(default)]
    base: Option<u64>,
    #[serde(default)]
    shift: usize,
    #[serde(default)]
    lines: Vec<(usize, String)>,
    #[serde(default)]
    cursor: Option<WireCursor>,
    #[serde(default)]
    rows: u16,
    #[serde(default)]
    history: u32,
    #[serde(default)]
    alt_screen: bool,
    #[serde(default)]
    mouse: bool,
    #[serde(default)]
    mouse_sgr: bool,
    #[serde(default, rename = "is_owner")]
    is_owner: Option<bool>,
    /// Who holds the size lock instead of us.
    #[serde(default)]
    holder: Option<String>,
}

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

impl WireMessage {
    /// The daemon already moved the cursor onto the window grid, so no
    /// composite origin is applied again here.
    fn pane_cursor(&self) -> PaneCursor {
        PaneCursor {
            x: self.cursor.as_ref().map_or(0, |c| c.x),
            y: self.cursor.as_ref().map_or(0, |c| c.y),
            visible: self.cursor.is_some(),
            pane_height: self.rows,
            history_size: self.history,
            pane_width: 0,
            alternate_on: self.alt_screen,
            mouse_tracking: self.mouse,
            mouse_sgr: self.mouse_sgr,
            mouse_all: false,
            position_reliable: true,
            composite_pane0: None,
        }
    }
}

fn parse_text(text: &str) -> Option<LiveMessage> {
    let msg: WireMessage = serde_json::from_str(text).ok()?;
    match msg.kind.as_str() {
        "frame" => Some(LiveMessage::Frame {
            seq: msg.seq,
            cursor: msg.pane_cursor(),
            content: msg.content.unwrap_or_default(),
        }),
        "patch" => Some(LiveMessage::Patch {
            seq: msg.seq?,
            base: msg.base?,
            shift: msg.shift,
            cursor: msg.pane_cursor(),
            lines: msg.lines,
        }),
        "size_owner" => Some(LiveMessage::SizeOwner {
            is_owner: msg.is_owner.unwrap_or(true),
            holder: msg.holder,
        }),
        _ => None,
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
