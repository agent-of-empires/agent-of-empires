//! Wire contract for a session's live pane stream (`/sessions/{id}/live-ws`).
//!
//! Both Rust ends build these types, so a field added here either reaches the
//! TUI or fails to compile. That is the point of the module: the frame shape
//! used to be a `serde_json::Map` on the daemon and a separate struct on the
//! client, and fields published on one side went unread on the other for
//! releases at a time.
//!
//! The dashboard reads the same messages, and its copy is generated from
//! these types into `web/src/lib/liveWire.ts` when the lib tests run, so a
//! field added here reaches TypeScript as well. Generation agrees on shape
//! and says nothing about meaning: `the_wire_is_what_both_clients_parse`
//! stays because the cursor-origin bug had both sides reading the same field
//! and disagreeing about what its number counted from.
//!
//! `docs/development/internals/client-transports.md` is the canonical
//! description of this transport and the two beside it.
//!
//! Field names are the wire's, not Rust's: `altScreen` and `mouseSgr` are
//! camelCase because the dashboard read them first, while `size_owner` and
//! `is_owner` are snake_case for the same reason. Neither can be tidied
//! without breaking a client that is already deployed.

use serde::{Deserialize, Serialize};

/// Cursor position, already translated onto the window grid by the daemon.
/// Clients must not add a composite origin to it a second time.
// The `TS` derives below write the dashboard's copy of these types when the
// lib tests run. `export_to` is resolved against ts-rs's own `bindings/`
// directory, which is why the path climbs out of it.
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../web/src/lib/liveWire.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveCursor {
    pub x: u16,
    pub y: u16,
}

/// Pane 0's rectangle inside a composited window. Input is pinned to pane 0,
/// so a client maps pointer cells against this rather than the whole window.
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../web/src/lib/liveWire.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LivePane0 {
    pub cols: u16,
    pub rows: u16,
    pub left: u16,
    pub top: u16,
}

/// The pane state every content message carries. Each field defaults, so a
/// frame from a daemon that predates one of them still parses.
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../web/src/lib/liveWire.ts"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LivePaneMeta {
    /// Rows of live screen at the end of `content`; the rest is history.
    #[serde(default)]
    pub rows: u16,
    /// Lines in the pane's scrollback, for a client sizing a scroll spacer.
    #[serde(default)]
    pub history: u32,
    #[serde(default)]
    pub cursor: Option<LiveCursor>,
    /// `#{alternate_on}`: a full-screen app, so there is no scrollback to
    /// widen and the wheel belongs to the app.
    #[serde(default)]
    pub alt_screen: bool,
    /// `#{mouse_any_flag}`: the app asked for button reports.
    #[serde(default)]
    pub mouse: bool,
    /// `#{mouse_sgr_flag}`: SGR (1006) encoding rather than legacy X10.
    #[serde(default)]
    pub mouse_sgr: bool,
    /// `#{mouse_all_flag}`: any-event tracking (1003), so the app wants bare
    /// motion and a viewer can forward hover.
    #[serde(default)]
    pub mouse_all: bool,
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub pane0: Option<LivePane0>,
}

/// Daemon to client.
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../web/src/lib/liveWire.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum LiveServerMessage {
    /// The whole window: history lines first, then the last `meta.rows` lines
    /// of live screen.
    #[serde(rename = "frame")]
    Frame {
        /// Absent only from a daemon that predates sequencing, which then
        /// cannot send patches either.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        // ts-rs maps u64 to `bigint`, which `JSON.parse` never produces.
        #[cfg_attr(test, ts(type = "number | null"))]
        seq: Option<u64>,
        content: String,
        #[serde(flatten)]
        meta: LivePaneMeta,
    },
    /// Rows that changed against the message numbered `base`, after `shift`
    /// rows scrolled off the top.
    #[serde(rename = "patch")]
    Patch {
        #[cfg_attr(test, ts(type = "number"))]
        seq: u64,
        #[cfg_attr(test, ts(type = "number"))]
        base: u64,
        shift: usize,
        lines: Vec<(usize, String)>,
        #[serde(flatten)]
        meta: LivePaneMeta,
    },
    /// Whether this viewer owns the session's size, and who holds it if not.
    #[serde(rename = "size_owner")]
    SizeOwner {
        is_owner: bool,
        #[cfg_attr(test, ts(optional = nullable))]
        holder: Option<String>,
    },
    /// An OSC 52 copy from the pane, for the viewer's own clipboard.
    #[serde(rename = "clipboard")]
    Clipboard { text: String },
    /// Which transport is producing frames, and the ceiling the daemon
    /// clamps `window.lines` to. The grid tears where the snapshot fallback
    /// does not, so a viewer debugging tearing needs to know which it has;
    /// the ceiling rides along so a client need not hardcode the constant.
    #[serde(rename = "transport")]
    Transport {
        grid: bool,
        #[serde(default, rename = "maxWindow")]
        max_window: usize,
    },
}

/// Client to daemon. Binary messages are pane input and carry no envelope.
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../web/src/lib/liveWire.ts"))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// live owner. Mobile startup uses this while the soft keyboard prevents a
    /// safe grid measurement.
    #[serde(rename = "claim_if_vacant")]
    ClaimIfVacant,
    /// Take over from a live holder. A user tap is intentional, unlike the
    /// passive flap the heartbeat guards against.
    #[serde(rename = "claim")]
    Claim,
    /// Capability advertisement. `deflate` switches frames to the compressed
    /// binary stream; `patch` enables row patches.
    #[serde(rename = "caps")]
    Caps {
        #[serde(default)]
        deflate: bool,
        #[serde(default)]
        patch: bool,
        /// What another client calls this one in its "took over" notice.
        #[serde(default)]
        #[cfg_attr(test, ts(optional = nullable))]
        label: Option<String>,
    },
    /// Patch continuity is lost; send a full frame.
    #[serde(rename = "resync")]
    Resync,
    /// Wheel notches at a pane cell, which the daemon encodes for the pane's
    /// own mouse modes. Accepted from a viewer that does not own the lock, so
    /// a watcher can still scroll a full-screen app.
    #[serde(rename = "wheel")]
    Wheel {
        up: bool,
        col: u16,
        row: u16,
        /// Notches this stands for. Absent means one, which is what a client
        /// predating coalescing sends.
        #[serde(default = "one_notch")]
        count: u16,
    },
}

fn one_notch() -> u16 {
    1
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
            mouse_all: true,
            pane0: Some(LivePane0 {
                cols: 40,
                rows: 10,
                left: 41,
                top: 0,
            }),
        }
    }

    /// The names here are read by a TypeScript client that cannot be made to
    /// fail at compile time, so they are pinned: a rename has to break this
    /// test before it can silently stop reaching the dashboard.
    #[test]
    fn the_wire_is_what_both_clients_parse() {
        let cases: Vec<(LiveServerMessage, serde_json::Value)> = vec![
            (
                LiveServerMessage::Frame {
                    seq: Some(7),
                    content: "hi\n".into(),
                    meta: meta(),
                },
                serde_json::json!({
                    "type": "frame", "seq": 7, "content": "hi\n",
                    "rows": 24, "history": 40, "cursor": {"x": 3, "y": 1},
                    "altScreen": true, "mouse": true, "mouseSgr": true, "mouseAll": true,
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
                    "altScreen": false, "mouse": false, "mouseSgr": false, "mouseAll": false,
                    "pane0": null,
                }),
            ),
            (
                LiveServerMessage::SizeOwner {
                    is_owner: false,
                    holder: Some("the web dashboard".into()),
                },
                serde_json::json!({
                    "type": "size_owner", "is_owner": false, "holder": "the web dashboard",
                }),
            ),
            (
                LiveServerMessage::Clipboard {
                    text: "copied\n".into(),
                },
                serde_json::json!({"type": "clipboard", "text": "copied\n"}),
            ),
            (
                LiveServerMessage::Transport {
                    grid: true,
                    max_window: 4000,
                },
                serde_json::json!({"type": "transport", "grid": true, "maxWindow": 4000}),
            ),
        ];
        for (message, expected) in cases {
            let encoded = serde_json::to_value(&message).unwrap();
            assert_eq!(encoded, expected, "{message:?}");
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
                    label: Some("mini (aoe)".into()),
                },
                serde_json::json!({
                    "type": "caps", "deflate": true, "patch": true, "label": "mini (aoe)",
                }),
            ),
            (
                LiveClientMessage::Resync,
                serde_json::json!({"type": "resync"}),
            ),
            (
                LiveClientMessage::Wheel {
                    up: true,
                    col: 4,
                    row: 9,
                    count: 3,
                },
                serde_json::json!({"type": "wheel", "up": true, "col": 4, "row": 9, "count": 3}),
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

    /// Every optional field is optional on the way in too, so a client or
    /// daemon that predates one keeps working.
    #[test]
    fn an_older_peer_still_parses() {
        let frame: LiveServerMessage =
            serde_json::from_str(r#"{"type":"frame","content":"x\n","rows":5}"#).unwrap();
        let LiveServerMessage::Frame { seq, meta, .. } = frame else {
            panic!("expected a frame");
        };
        assert_eq!(seq, None);
        assert_eq!(meta.rows, 5);
        assert!(!meta.mouse_all && meta.pane0.is_none() && meta.cursor.is_none());

        let wheel: LiveClientMessage =
            serde_json::from_str(r#"{"type":"wheel","up":true,"col":4,"row":9}"#).unwrap();
        assert_eq!(
            wheel,
            LiveClientMessage::Wheel {
                up: true,
                col: 4,
                row: 9,
                count: 1,
            }
        );
    }
}
