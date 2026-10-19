//! Wire contract for a session's live pane stream (`/sessions/{id}/live-ws`).
//!
//! The daemon builds these types and the dashboard reads them, so a field
//! added here either reaches TypeScript or fails the generated-file check:
//! `cargo xtask gen-live-wire` writes `web/src/lib/generated/liveWire.ts`.
//! Generation agrees on shape only; `the_wire_is_what_both_clients_parse`
//! pins the names and values, which is where a meaning change would show.
//!
//! `server::live_ws` documents the transport. Field names are the wire's:
//! `altScreen` and `mouseSgr` are camelCase because the dashboard read them
//! first, while `size_owner` and `is_owner` are snake_case. Neither can be
//! tidied without breaking a client that is already deployed.

use serde::{Deserialize, Serialize};

/// Cursor position on the window grid. Clients must not add a composite
/// origin to it a second time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct LiveCursor {
    pub x: u16,
    pub y: u16,
}

/// Pane 0's rectangle inside a composited window, `null` for a single pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct LivePane0 {
    pub cols: u16,
    pub rows: u16,
    pub left: u16,
    pub top: u16,
}

/// The pane state every content message carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct LivePaneMeta {
    /// Rows of live screen at the end of `content`; the rest is history.
    pub rows: u16,
    /// Lines in the pane's scrollback.
    pub history: u32,
    pub cursor: Option<LiveCursor>,
    /// Full-screen app: no scrollback to widen, the wheel belongs to the app.
    pub alt_screen: bool,
    /// The app asked for mouse button reports.
    pub mouse: bool,
    /// SGR (1006) mouse encoding rather than legacy X10.
    pub mouse_sgr: bool,
    pub pane0: Option<LivePane0>,
}

/// Daemon to client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(tag = "type")]
pub enum LiveServerMessage {
    /// The whole window: history lines first, then the last `meta.rows` lines
    /// of live screen.
    #[serde(rename = "frame")]
    Frame {
        #[cfg_attr(feature = "typescript", ts(type = "number"))]
        seq: u64,
        content: String,
        #[serde(flatten)]
        meta: LivePaneMeta,
    },
    /// Rows that changed against the message numbered `base`, after `shift`
    /// rows scrolled off the top.
    #[serde(rename = "patch")]
    Patch {
        #[cfg_attr(feature = "typescript", ts(type = "number"))]
        seq: u64,
        #[cfg_attr(feature = "typescript", ts(type = "number"))]
        base: u64,
        shift: usize,
        lines: Vec<(usize, String)>,
        #[serde(flatten)]
        meta: LivePaneMeta,
    },
    /// Whether this viewer owns the session's size.
    #[serde(rename = "size_owner")]
    SizeOwner { is_owner: bool },
    /// An OSC 52 copy from the pane, for the viewer's own clipboard.
    #[serde(rename = "clipboard")]
    Clipboard { text: String },
    /// `false` is the capture fallback, which cannot suppress a half-drawn repaint.
    #[serde(rename = "transport")]
    Transport { grid: bool },
}

/// Client to daemon. Binary messages are pane input and carry no envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(tag = "type")]
pub enum LiveClientMessage {
    /// Claim the size lock and set the pane's grid.
    #[serde(rename = "resize")]
    Resize { cols: u16, rows: u16 },
    /// Total capture window, history included.
    #[serde(rename = "window")]
    Window { lines: usize },
    /// Fast while the viewer is interacting, idle otherwise.
    #[serde(rename = "cadence")]
    Cadence { fast: bool },
    /// Take the lock only if it is vacant, without resizing or displacing a
    /// live owner.
    #[serde(rename = "claim_if_vacant")]
    ClaimIfVacant,
    /// Take over from a live holder.
    #[serde(rename = "claim")]
    Claim,
    /// `deflate` switches frames to the compressed binary stream; `patch`
    /// enables row patches.
    #[serde(rename = "caps")]
    Caps {
        #[serde(default)]
        deflate: bool,
        #[serde(default)]
        patch: bool,
    },
    /// Patch continuity is lost; send a full frame.
    #[serde(rename = "resync")]
    Resync,
    /// Text for tmux's paste path; `submit` presses Enter after it.
    #[serde(rename = "paste")]
    Paste {
        text: String,
        #[serde(default)]
        submit: bool,
    },
}

/// The dashboard's copy of the wire, in declaration order.
#[cfg(feature = "typescript")]
pub fn typescript_declarations() -> String {
    use ts_rs::TS;
    let cfg = ts_rs::Config::default();
    let decls = [
        LiveCursor::decl(&cfg),
        LivePane0::decl(&cfg),
        LivePaneMeta::decl(&cfg),
        LiveServerMessage::decl(&cfg),
        LiveClientMessage::decl(&cfg),
    ];
    let mut out = String::from("// Generated by `cargo xtask gen-live-wire`. Do not edit.\n\n");
    for decl in decls {
        out.push_str("export ");
        out.push_str(&decl);
        out.push_str("\n\n");
    }
    out.truncate(out.trim_end().len());
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> LivePaneMeta {
        LivePaneMeta {
            rows: 24,
            history: 40,
            cursor: Some(LiveCursor { x: 3, y: 1 }),
            alt_screen: true,
            mouse: true,
            mouse_sgr: true,
            pane0: Some(LivePane0 {
                cols: 40,
                rows: 10,
                left: 41,
                top: 0,
            }),
        }
    }

    /// The names are read by a TypeScript client that cannot fail at compile
    /// time on a rename, so they are pinned here.
    #[test]
    fn the_wire_is_what_both_clients_parse() {
        let cases: Vec<(LiveServerMessage, serde_json::Value)> = vec![
            (
                LiveServerMessage::Frame {
                    seq: 7,
                    content: "hi\n".into(),
                    meta: meta(),
                },
                serde_json::json!({
                    "type": "frame", "seq": 7, "content": "hi\n",
                    "rows": 24, "history": 40, "cursor": {"x": 3, "y": 1},
                    "altScreen": true, "mouse": true, "mouseSgr": true,
                    "pane0": {"cols": 40, "rows": 10, "left": 41, "top": 0},
                }),
            ),
            (
                LiveServerMessage::Patch {
                    seq: 8,
                    base: 7,
                    shift: 2,
                    lines: vec![(0, "a".into())],
                    meta: LivePaneMeta {
                        rows: 24,
                        ..LivePaneMeta::default()
                    },
                },
                serde_json::json!({
                    "type": "patch", "seq": 8, "base": 7, "shift": 2, "lines": [[0, "a"]],
                    "rows": 24, "history": 0, "cursor": null,
                    "altScreen": false, "mouse": false, "mouseSgr": false,
                    "pane0": null,
                }),
            ),
            (
                LiveServerMessage::SizeOwner { is_owner: false },
                serde_json::json!({"type": "size_owner", "is_owner": false}),
            ),
            (
                LiveServerMessage::Clipboard {
                    text: "copied\n".into(),
                },
                serde_json::json!({"type": "clipboard", "text": "copied\n"}),
            ),
            (
                LiveServerMessage::Transport { grid: true },
                serde_json::json!({"type": "transport", "grid": true}),
            ),
        ];
        for (message, expected) in cases {
            assert_eq!(
                serde_json::to_value(&message).unwrap(),
                expected,
                "{message:?}"
            );
            let decoded: LiveServerMessage = serde_json::from_value(expected).unwrap();
            assert_eq!(decoded, message);
        }
    }

    #[test]
    fn the_control_messages_are_what_the_daemon_accepts() {
        let cases: Vec<(LiveClientMessage, serde_json::Value)> = vec![
            (
                LiveClientMessage::Resize { cols: 80, rows: 24 },
                serde_json::json!({"type": "resize", "cols": 80, "rows": 24}),
            ),
            (
                LiveClientMessage::Window { lines: 2000 },
                serde_json::json!({"type": "window", "lines": 2000}),
            ),
            (
                LiveClientMessage::Cadence { fast: true },
                serde_json::json!({"type": "cadence", "fast": true}),
            ),
            (
                LiveClientMessage::ClaimIfVacant,
                serde_json::json!({"type": "claim_if_vacant"}),
            ),
            (
                LiveClientMessage::Claim,
                serde_json::json!({"type": "claim"}),
            ),
            (
                LiveClientMessage::Caps {
                    deflate: true,
                    patch: true,
                },
                serde_json::json!({"type": "caps", "deflate": true, "patch": true}),
            ),
            (
                LiveClientMessage::Resync,
                serde_json::json!({"type": "resync"}),
            ),
            (
                LiveClientMessage::Paste {
                    text: "x".into(),
                    submit: true,
                },
                serde_json::json!({"type": "paste", "text": "x", "submit": true}),
            ),
        ];
        for (message, expected) in cases {
            assert_eq!(
                serde_json::to_value(&message).unwrap(),
                expected,
                "{message:?}"
            );
            let decoded: LiveClientMessage = serde_json::from_value(expected).unwrap();
            assert_eq!(decoded, message);
        }
    }

    /// Defaulted client fields stay optional, so an older dashboard keeps working.
    #[test]
    fn an_older_peer_still_parses() {
        let caps: LiveClientMessage = serde_json::from_str(r#"{"type":"caps"}"#).unwrap();
        assert_eq!(
            caps,
            LiveClientMessage::Caps {
                deflate: false,
                patch: false
            }
        );
        let paste: LiveClientMessage =
            serde_json::from_str(r#"{"type":"paste","text":"x"}"#).unwrap();
        assert_eq!(
            paste,
            LiveClientMessage::Paste {
                text: "x".into(),
                submit: false
            }
        );
    }
}
