//! Live output of the selected remote session for the home view's preview
//! pane, over the daemon's live terminal socket.
//!
//! Watching never claims the remote pane's size: a viewer renders at whatever
//! grid the pane already has, so moving the cursor across remote rows cannot
//! resize a pane for anyone else. Only live-send takes the size, the same
//! take-over local live-send makes, and leaving it reconnects to let go.

use std::collections::VecDeque;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::acp::client::discovery::DaemonEndpoint;
use crate::daemon::LiveClientMessage;
use crate::tui::live_socket::{self, LiveMessage};

/// `(remote name, session id)`.
pub(crate) type RemoteKey = (String, String);

pub(crate) enum PreviewCommand {
    Watch {
        key: RemoteKey,
        endpoint: DaemonEndpoint,
        lines: usize,
    },
    Stop,
    Input(Vec<u8>),
    /// Claim the size-owner lock and size the pane to the preview.
    TakeOver {
        cols: u16,
        rows: u16,
    },
    /// A size waits in [`PendingSlots`]; only the newest one is worth sending.
    Resize,
    /// Wheel notches wait in [`PendingSlots`], for a pane this viewer only
    /// watches; the daemon ignores them unless the pane is full-screen.
    Wheel,
    /// Capture `lines` of history plus screen; `fast` while at the live edge.
    Window {
        lines: usize,
        fast: bool,
    },
}

/// Net wheel scroll waiting to go out, at the 0-based pane cell it applies to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PendingWheel {
    pub(crate) col: u16,
    pub(crate) row: u16,
    /// Signed notch count; positive scrolls up.
    pub(crate) notches: i32,
}

impl PendingWheel {
    /// Direction and wire count, or `None` once a burst has cancelled itself
    /// out. The count clamps to what one message may carry.
    fn burst(&self) -> Option<(bool, u16)> {
        let capped = crate::tmux::mouse::MAX_WHEEL_NOTCHES;
        let count = self.notches.unsigned_abs().min(u32::from(capped)) as u16;
        (count > 0).then_some((self.notches > 0, count))
    }
}

/// Latest-value slots for the commands a render or a wheel burst can fire
/// faster than the link drains them, the input-side twin of [`FrameSlot`].
/// Their [`PreviewCommand`] carries no payload and is only sent on the
/// transition out of empty, so a burst wakes the worker at most once instead
/// of growing the command queue.
#[derive(Default)]
pub(crate) struct PendingSlots {
    wheel: Mutex<Option<PendingWheel>>,
    resize: Mutex<Option<(u16, u16)>>,
}

impl PendingSlots {
    /// Fold one notch in; `true` when the worker needs a wake. Notches at one
    /// cell accumulate and opposing ones cancel, while a notch at another cell
    /// replaces the slot, since the cell decides which pane cell it targets.
    fn wheel(&self, up: bool, col: u16, row: u16) -> bool {
        let step = if up { 1 } else { -1 };
        let mut slot = self.wheel.lock().unwrap_or_else(|e| e.into_inner());
        match slot.as_mut() {
            Some(pending) if (pending.col, pending.row) == (col, row) => {
                pending.notches = pending.notches.saturating_add(step);
                false
            }
            Some(pending) => {
                pending.col = col;
                pending.row = row;
                pending.notches = step;
                false
            }
            None => {
                *slot = Some(PendingWheel {
                    col,
                    row,
                    notches: step,
                });
                true
            }
        }
    }

    pub(crate) fn take_wheel(&self) -> Option<PendingWheel> {
        self.wheel.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Replace the pending size; `true` when the worker needs a wake.
    fn resize(&self, cols: u16, rows: u16) -> bool {
        self.resize
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace((cols, rows))
            .is_none()
    }

    fn take_resize(&self) -> Option<(u16, u16)> {
        self.resize.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// The newest content of the watched pane.
#[derive(Debug)]
pub(crate) struct PreviewFrame {
    pub(crate) key: RemoteKey,
    pub(crate) content: String,
    pub(crate) cursor: crate::tmux::PaneCursor,
}

/// Socket state changes, delivered in order and never coalesced.
#[derive(Debug)]
pub(crate) enum PreviewEvent {
    SizeOwner {
        key: RemoteKey,
        is_owner: bool,
        /// Who holds the pane's size instead of us.
        holder: Option<String>,
    },
    Closed {
        key: RemoteKey,
        reason: String,
    },
}

type FrameSlot = Arc<Mutex<Option<PreviewFrame>>>;

/// Worker side of the hand-off to the TUI. A frame replaces any the TUI has
/// not taken yet, so a busy TUI never works through stale frames; events
/// queue behind each other.
struct EventSink {
    events: std_mpsc::Sender<PreviewEvent>,
    frame: FrameSlot,
    wake: Arc<tokio::sync::Notify>,
}

impl EventSink {
    fn frame(&self, frame: PreviewFrame) {
        *self.frame.lock().unwrap_or_else(|e| e.into_inner()) = Some(frame);
        self.wake.notify_one();
    }

    fn event(&self, event: PreviewEvent) {
        let _ = self.events.send(event);
        self.wake.notify_one();
    }
}

/// Worker thread owning at most one live socket at a time.
pub struct RemotePreview {
    commands: mpsc::UnboundedSender<PreviewCommand>,
    events: std_mpsc::Receiver<PreviewEvent>,
    frame: FrameSlot,
    pub(crate) slots: Arc<PendingSlots>,
    /// Kept only by [`RemotePreview::recording`], so a test can deliver an
    /// event the way the worker does.
    #[cfg(test)]
    sink: Option<EventSink>,
}

impl RemotePreview {
    /// `wake` is notified with each frame so the TUI paints it without
    /// waiting for its ticker.
    pub fn new(wake: Arc<tokio::sync::Notify>) -> Self {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (preview, sink) = Self::with_sink(commands, wake);
        let slots = Arc::clone(&preview.slots);
        let spawned = std::thread::Builder::new()
            .name("aoe-remote-preview".into())
            .spawn(move || {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt.block_on(run(command_rx, sink, slots)),
                    Err(e) => {
                        tracing::warn!(target: "tui.remote_preview", "runtime build failed: {e}")
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(target: "tui.remote_preview", "worker spawn failed: {e}");
        }
        preview
    }

    fn with_sink(
        commands: mpsc::UnboundedSender<PreviewCommand>,
        wake: Arc<tokio::sync::Notify>,
    ) -> (Self, EventSink) {
        let (events_tx, events) = std_mpsc::channel();
        let frame = FrameSlot::default();
        let sink = EventSink {
            events: events_tx,
            frame: Arc::clone(&frame),
            wake,
        };
        (
            Self {
                commands,
                events,
                frame,
                slots: Arc::default(),
                #[cfg(test)]
                sink: None,
            },
            sink,
        )
    }

    /// A preview with no worker, whose commands land on the returned receiver.
    #[cfg(test)]
    pub(crate) fn recording() -> (Self, mpsc::UnboundedReceiver<PreviewCommand>) {
        let (commands, recorded) = mpsc::unbounded_channel();
        let (mut preview, sink) = Self::with_sink(commands, Arc::default());
        preview.sink = Some(sink);
        (preview, recorded)
    }

    /// Deliver an event as the worker would.
    #[cfg(test)]
    pub(crate) fn emit(&self, event: PreviewEvent) {
        self.sink
            .as_ref()
            .expect("a recording preview")
            .event(event);
    }

    pub(crate) fn send(&self, command: PreviewCommand) {
        let _ = self.commands.send(command);
    }

    /// Accumulate one wheel notch for the watched pane. Only the net scroll
    /// goes out, so a burst the link cannot keep up with never replays.
    pub(crate) fn wheel(&self, up: bool, col: u16, row: u16) {
        if self.slots.wheel(up, col, row) {
            self.send(PreviewCommand::Wheel);
        }
    }

    /// Offer the preview's grid to the pane; only the newest one is sent.
    pub(crate) fn resize(&self, cols: u16, rows: u16) {
        if self.slots.resize(cols, rows) {
            self.send(PreviewCommand::Resize);
        }
    }

    pub(crate) fn try_recv(&self) -> Option<PreviewEvent> {
        self.events.try_recv().ok()
    }

    /// The newest frame the worker delivered since the last take.
    pub(crate) fn take_frame(&self) -> Option<PreviewFrame> {
        self.frame.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// Whether typed bytes may reach the pane. The daemon drops input from a
/// client that does not hold the size-owner lock, so bytes typed between a
/// take-over and its grant wait here and go out in order once it lands.
#[derive(Debug, Default, PartialEq)]
enum InputGate {
    #[default]
    Viewer,
    Claiming(Vec<Vec<u8>>),
    Owner,
}

impl InputGate {
    fn claim(&mut self) {
        if !matches!(self, Self::Claiming(_)) {
            *self = Self::Claiming(Vec::new());
        }
    }

    /// Bytes to send now, if any; a viewer's input goes nowhere.
    fn input(&mut self, bytes: Vec<u8>) -> Option<Vec<u8>> {
        match self {
            Self::Owner => Some(bytes),
            Self::Claiming(buffered) => {
                buffered.push(bytes);
                None
            }
            Self::Viewer => None,
        }
    }

    /// Apply a `size_owner` notice; returns the held bytes a grant releases.
    fn ownership(&mut self, is_owner: bool) -> Vec<Vec<u8>> {
        match (std::mem::take(self), is_owner) {
            (Self::Claiming(buffered), true) => {
                *self = Self::Owner;
                buffered
            }
            (Self::Owner, true) => {
                *self = Self::Owner;
                Vec::new()
            }
            (Self::Viewer, true) | (_, false) => Vec::new(),
        }
    }
}

/// The window the last frame or patch produced, which the next patch edits.
#[derive(Debug, Default)]
struct PatchBase {
    /// `None` before a numbered frame lands and after continuity is lost.
    seq: Option<u64>,
    rows: Vec<String>,
    /// Whether the window's text ended with a newline, restored on rebuild.
    terminated: bool,
    resync_sent: bool,
}

#[derive(Debug, PartialEq)]
enum Patched {
    Content(String),
    /// Continuity lost: ask the daemon for a full frame.
    Resync,
    /// A resync is already on its way; drop patches until the frame lands.
    Skip,
}

impl PatchBase {
    fn frame(&mut self, seq: Option<u64>, content: &str) {
        let mut rows: Vec<String> = content.split('\n').map(str::to_string).collect();
        let terminated = rows.len() > 1 && rows.last().is_some_and(String::is_empty);
        if terminated {
            rows.pop();
        }
        *self = Self {
            seq,
            rows,
            terminated,
            resync_sent: false,
        };
    }

    /// Drop `shift` leading rows, pad the bottom with blanks, then replace the
    /// listed rows, which is how the daemon diffed the window. Indices outside
    /// the window are ignored so a patch can never resize it.
    fn patch(&mut self, seq: u64, base: u64, shift: usize, lines: Vec<(usize, String)>) -> Patched {
        if self.seq != Some(base) {
            self.seq = None;
            if self.resync_sent {
                return Patched::Skip;
            }
            self.resync_sent = true;
            return Patched::Resync;
        }
        let n = self.rows.len();
        let shift = shift.min(n);
        self.rows.drain(..shift);
        self.rows.resize(n, String::new());
        for (i, row) in lines {
            if let Some(slot) = self.rows.get_mut(i) {
                *slot = row;
            }
        }
        self.seq = Some(seq);
        let mut content = self.rows.join("\n");
        if self.terminated {
            content.push('\n');
        }
        Patched::Content(content)
    }
}

struct Connection {
    key: RemoteKey,
    tx: mpsc::Sender<Message>,
    task: tokio::task::JoinHandle<()>,
    gate: InputGate,
    base: PatchBase,
}

impl Connection {
    async fn control(&self, message: LiveClientMessage) {
        let Ok(text) = serde_json::to_string(&message) else {
            return;
        };
        let _ = self.tx.send(Message::Text(text.into())).await;
    }

    async fn size(&self, cols: u16, rows: u16) {
        self.control(LiveClientMessage::Resize {
            cols: cols.max(1),
            rows: rows.max(1),
        })
        .await;
    }

    async fn window(&self, lines: usize, fast: bool) {
        self.control(LiveClientMessage::Window {
            lines: lines.max(1),
        })
        .await;
        self.control(LiveClientMessage::Cadence { fast }).await;
    }

    fn close(self) {
        drop(self.tx);
        self.task.abort();
    }
}

async fn next_message(rx: &mut Option<mpsc::Receiver<LiveMessage>>) -> Option<LiveMessage> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// A cursor sweeping across rows queues a Watch per row; only the last one (or
/// a later Stop) matters, and each stale connect would cost a round trip.
/// Commands after the first non-target one keep their order in `backlog`, so a
/// take-over or keystroke queued behind a Watch is never dropped.
fn latest_target(
    first: PreviewCommand,
    commands: &mut mpsc::UnboundedReceiver<PreviewCommand>,
    backlog: &mut VecDeque<PreviewCommand>,
) -> PreviewCommand {
    let mut latest = first;
    while let Ok(next) = commands.try_recv() {
        match next {
            PreviewCommand::Watch { .. } | PreviewCommand::Stop if backlog.is_empty() => {
                latest = next;
            }
            other => backlog.push_back(other),
        }
    }
    latest
}

async fn run(
    mut commands: mpsc::UnboundedReceiver<PreviewCommand>,
    events: EventSink,
    slots: Arc<PendingSlots>,
) {
    let mut conn: Option<Connection> = None;
    let mut rx: Option<mpsc::Receiver<LiveMessage>> = None;
    let mut backlog = VecDeque::new();
    loop {
        let command = match backlog.pop_front() {
            Some(command) => command,
            None => tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else { break };
                    command
                }
                message = next_message(&mut rx) => {
                    on_message(message, &mut conn, &mut rx, &events).await;
                    continue;
                }
            },
        };
        let command = match command {
            PreviewCommand::Watch { .. } | PreviewCommand::Stop => {
                latest_target(command, &mut commands, &mut backlog)
            }
            other => other,
        };
        match command {
            PreviewCommand::Watch {
                key,
                endpoint,
                lines,
            } => {
                if let Some(old) = conn.take() {
                    old.close();
                }
                rx = None;
                match live_socket::connect(&endpoint, &key.1).await {
                    Ok(socket) => {
                        let next = Connection {
                            key,
                            tx: socket.tx,
                            task: socket.task,
                            gate: InputGate::Viewer,
                            base: PatchBase::default(),
                        };
                        next.control(LiveClientMessage::Caps {
                            deflate: true,
                            patch: true,
                            label: Some(crate::tui::view_lock::viewer_label()),
                        })
                        .await;
                        next.window(lines, true).await;
                        rx = Some(socket.rx);
                        conn = Some(next);
                    }
                    Err(e) => {
                        events.event(PreviewEvent::Closed {
                            key,
                            reason: format!("{e:#}"),
                        });
                    }
                }
            }
            PreviewCommand::Stop => {
                if let Some(old) = conn.take() {
                    old.close();
                }
                rx = None;
            }
            PreviewCommand::Input(bytes) => {
                if let Some(c) = &mut conn {
                    if let Some(bytes) = c.gate.input(bytes) {
                        let _ = c.tx.send(Message::Binary(bytes.into())).await;
                    }
                }
            }
            PreviewCommand::TakeOver { cols, rows } => {
                if let Some(c) = &mut conn {
                    c.gate.claim();
                    c.control(LiveClientMessage::Claim).await;
                    c.size(cols, rows).await;
                }
            }
            PreviewCommand::Resize => {
                if let (Some(c), Some((cols, rows))) = (&conn, slots.take_resize()) {
                    c.size(cols, rows).await;
                }
            }
            PreviewCommand::Wheel => {
                if let (Some(c), Some(pending)) = (&conn, slots.take_wheel()) {
                    if let Some((up, count)) = pending.burst() {
                        c.control(LiveClientMessage::Wheel {
                            up,
                            col: pending.col,
                            row: pending.row,
                            count,
                        })
                        .await;
                    }
                }
            }
            PreviewCommand::Window { lines, fast } => {
                if let Some(c) = &conn {
                    c.window(lines, fast).await;
                }
            }
        }
    }
}

async fn on_message(
    message: Option<LiveMessage>,
    conn: &mut Option<Connection>,
    rx: &mut Option<mpsc::Receiver<LiveMessage>>,
    events: &EventSink,
) {
    let Some(c) = conn.as_mut() else {
        *rx = None;
        return;
    };
    let key = c.key.clone();
    let closed = match message {
        Some(LiveMessage::Frame {
            seq,
            content,
            cursor,
        }) => {
            c.base.frame(seq, &content);
            events.frame(PreviewFrame {
                key,
                content,
                cursor,
            });
            None
        }
        Some(LiveMessage::Patch {
            seq,
            base,
            shift,
            lines,
            cursor,
        }) => {
            match c.base.patch(seq, base, shift, lines) {
                Patched::Content(content) => events.frame(PreviewFrame {
                    key,
                    content,
                    cursor,
                }),
                Patched::Resync => c.control(LiveClientMessage::Resync).await,
                Patched::Skip => {}
            }
            None
        }
        Some(LiveMessage::SizeOwner { is_owner, holder }) => {
            for bytes in c.gate.ownership(is_owner) {
                let _ = c.tx.send(Message::Binary(bytes.into())).await;
            }
            events.event(PreviewEvent::SizeOwner {
                key,
                is_owner,
                holder,
            });
            None
        }
        Some(LiveMessage::Closed(reason)) => Some((key, reason)),
        None => Some((key, "the live terminal stream ended".to_string())),
    };
    if let Some((key, reason)) = closed {
        if let Some(old) = conn.take() {
            old.close();
        }
        *rx = None;
        events.event(PreviewEvent::Closed { key, reason });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn input_waits_for_the_ownership_grant_and_leaves_in_order() {
        let mut gate = InputGate::default();
        assert_eq!(
            gate.input(b"ignored".to_vec()),
            None,
            "a viewer cannot type"
        );

        gate.claim();
        assert_eq!(gate.input(b"a".to_vec()), None);
        assert_eq!(gate.input(b"b".to_vec()), None);
        assert_eq!(gate.ownership(true), [b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(gate.input(b"c".to_vec()), Some(b"c".to_vec()));
        assert!(
            gate.ownership(true).is_empty(),
            "a repeat grant flushes nothing"
        );

        gate.claim();
        gate.input(b"lost".to_vec());
        assert!(
            gate.ownership(false).is_empty(),
            "a refusal drops held input"
        );
        assert_eq!(gate, InputGate::Viewer);
    }

    #[test]
    fn a_stalled_tui_takes_only_the_newest_frame_and_every_event_in_order() {
        let (commands, _) = mpsc::unbounded_channel();
        let (preview, sink) = RemotePreview::with_sink(commands, Arc::default());
        let key: RemoteKey = ("mini".into(), "s1".into());
        let cursor = crate::tmux::PaneCursor {
            x: 0,
            y: 0,
            visible: true,
            pane_height: 1,
            history_size: 0,
            pane_width: 0,
            alternate_on: false,
            mouse_tracking: false,
            mouse_sgr: false,
            mouse_all: false,
            position_reliable: true,
            composite_pane0: None,
        };
        for i in 0..100 {
            sink.frame(PreviewFrame {
                key: key.clone(),
                content: format!("{i}\n"),
                cursor,
            });
            if i % 25 == 0 {
                sink.event(PreviewEvent::SizeOwner {
                    key: key.clone(),
                    is_owner: i % 50 == 0,
                    holder: None,
                });
            }
        }
        sink.event(PreviewEvent::Closed {
            key: key.clone(),
            reason: "gone".into(),
        });

        let frame = preview.take_frame().expect("a frame waits");
        assert_eq!(frame.content, "99\n");
        assert!(preview.take_frame().is_none(), "older frames were replaced");
        let events: Vec<String> = std::iter::from_fn(|| preview.try_recv())
            .map(|event| match event {
                PreviewEvent::SizeOwner { is_owner, .. } => format!("owner={is_owner}"),
                PreviewEvent::Closed { reason, .. } => format!("closed={reason}"),
            })
            .collect();
        assert_eq!(
            events,
            [
                "owner=true",
                "owner=false",
                "owner=true",
                "owner=false",
                "closed=gone"
            ]
        );
    }

    fn rows(content: &str) -> Vec<(usize, String)> {
        content
            .lines()
            .enumerate()
            .map(|(i, row)| (i, row.to_string()))
            .collect()
    }

    #[test]
    fn a_patch_rebuilds_the_window_the_daemon_diffed() {
        type Case = (
            &'static str,
            &'static str,
            usize,
            &'static [(usize, &'static str)],
            &'static str,
        );
        let cases: &[Case] = &[
            ("in place", "a\nb\nc\n", 0, &[(1, "B")], "a\nB\nc\n"),
            ("empty patch", "a\nb\n", 0, &[], "a\nb\n"),
            ("history grew", "a\nb\nc\n", 1, &[(2, "d")], "b\nc\nd\n"),
            ("shift appends blanks", "a\nb\nc\n", 2, &[], "c\n\n\n"),
            ("shift past the window", "a\nb\n", 9, &[(0, "x")], "x\n\n"),
            (
                "rows outside are ignored",
                "a\nb\n",
                0,
                &[(2, "z"), (0, "A")],
                "A\nb\n",
            ),
            ("unterminated window", "a\nb", 0, &[(1, "c")], "a\nc"),
            ("blank last row", "a\n\n", 0, &[(0, "b")], "b\n\n"),
        ];
        for (what, prev, shift, lines, want) in cases {
            let mut base = PatchBase::default();
            base.frame(Some(4), prev);
            let lines = lines.iter().map(|(i, r)| (*i, r.to_string())).collect();
            assert_eq!(
                base.patch(5, 4, *shift, lines),
                Patched::Content(want.to_string()),
                "{what}"
            );
            assert_eq!(base.seq, Some(5), "{what}");
        }
    }

    #[test]
    fn a_patch_off_its_base_asks_once_for_a_full_frame() {
        let mut base = PatchBase::default();
        assert_eq!(base.patch(1, 0, 0, vec![]), Patched::Resync, "no frame yet");

        base.frame(Some(3), "a\nb\n");
        assert_eq!(base.patch(5, 4, 0, rows("x\n")), Patched::Resync);
        assert_eq!(base.patch(6, 5, 0, rows("y\n")), Patched::Skip);
        assert_eq!(
            base.patch(7, 3, 0, rows("z\n")),
            Patched::Skip,
            "continuity stays lost until a frame lands"
        );

        // A wider window arrives as a full frame and becomes the base.
        base.frame(Some(8), "0\n1\na\nb\n");
        assert_eq!(
            base.patch(9, 8, 0, vec![(3, "c".into())]),
            Patched::Content("0\n1\na\nc\n".into())
        );
        assert_eq!(base.patch(10, 8, 0, vec![]), Patched::Resync);
    }

    #[tokio::test]
    async fn patches_from_the_daemon_encoder_land_as_the_full_window() {
        use crate::acp::client::discovery::Source;
        use crate::server::live_ws::{FrameEncoder, PendingFrame};
        use futures_util::{SinkExt, StreamExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = DaemonEndpoint::new(
            format!("http://{}", listener.local_addr().unwrap()),
            None,
            Source::Remote,
        );
        let wake = Arc::new(tokio::sync::Notify::new());
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (preview, sink) = RemotePreview::with_sink(commands, Arc::clone(&wake));
        let worker = tokio::spawn(run(command_rx, sink, Arc::clone(&preview.slots)));
        let key: RemoteKey = ("mini".into(), "s1".into());
        preview.send(PreviewCommand::Watch {
            key: key.clone(),
            endpoint,
            lines: 4,
        });
        let (tcp, _) = listener.accept().await.unwrap();
        let mut server = tokio_tungstenite::accept_async(tcp).await.unwrap();

        async fn client_text<S>(
            server: &mut tokio_tungstenite::WebSocketStream<S>,
        ) -> serde_json::Value
        where
            S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
        {
            loop {
                if let Message::Text(text) = server.next().await.unwrap().unwrap() {
                    return serde_json::from_str(&text).unwrap();
                }
            }
        }
        let caps = client_text(&mut server).await;
        assert_eq!(
            (&caps["type"], &caps["patch"], &caps["deflate"]),
            (&"caps".into(), &true.into(), &true.into()),
        );

        let cursor = |history_size: u32, y: u16| crate::tmux::PaneCursor {
            x: 0,
            y,
            visible: true,
            pane_height: 4,
            history_size,
            pane_width: 0,
            alternate_on: false,
            mouse_tracking: false,
            mouse_sgr: false,
            mouse_all: false,
            position_reliable: true,
            composite_pane0: None,
        };
        // A streaming shell: each step scrolls a line into history and edits
        // the prompt row.
        let windows = [
            ("l1\nl2\nl3\n$ \n", 0, 3),
            ("l1\nl2\nl3\n$ ls\n", 0, 3),
            ("l2\nl3\nl4\n$ \n", 1, 3),
            ("l4\nl5\nl6\n$ \n", 3, 3),
            ("l4\nl5\nl6\n\x1b[1m$ \x1b[0m\n", 3, 2),
        ];
        let mut encoder = FrameEncoder::default();
        let mut kinds = Vec::new();
        for (content, history, y) in windows {
            let json = encoder.json(
                &PendingFrame {
                    content: content.to_string(),
                    cursor: Some(cursor(history, y)),
                    full: false,
                },
                true,
            );
            kinds.push(serde_json::from_str::<serde_json::Value>(&json).unwrap()["type"].clone());
            server.send(Message::Text(json.into())).await.unwrap();
            let frame = loop {
                if let Some(frame) = preview.take_frame() {
                    break frame;
                }
                tokio::time::timeout(Duration::from_secs(5), wake.notified())
                    .await
                    .expect("frame lands");
            };
            assert_eq!(frame.key, key);
            assert_eq!(frame.content, content);
            assert_eq!((frame.cursor.history_size, frame.cursor.y), (history, y));
        }
        // The two-line scroll rewrites most of a four-row window, so the
        // daemon sends that one whole.
        assert_eq!(kinds, ["frame", "patch", "patch", "frame", "patch"]);

        // A patch the client cannot place brings a resync, then a full frame.
        let stray = r#"{"type":"patch","seq":99,"base":98,"shift":0,"lines":[[0,"lost"]],"rows":4,"history":3,"cursor":null}"#;
        server.send(Message::Text(stray.into())).await.unwrap();
        let resync = loop {
            let value = client_text(&mut server).await;
            if value["type"] != "window" && value["type"] != "cadence" {
                break value;
            }
        };
        assert_eq!(resync["type"], "resync");
        assert!(
            preview.take_frame().is_none(),
            "the stray patch is not shown"
        );

        let recovered = "r1\nr2\nr3\n$ \n";
        let json = encoder.json(
            &PendingFrame {
                content: recovered.to_string(),
                cursor: Some(cursor(3, 3)),
                full: true,
            },
            true,
        );
        server.send(Message::Text(json.into())).await.unwrap();
        let frame = loop {
            if let Some(frame) = preview.take_frame() {
                break frame;
            }
            tokio::time::timeout(Duration::from_secs(5), wake.notified())
                .await
                .expect("the resync frame lands");
        };
        assert_eq!(frame.content, recovered);

        // The client advertised `deflate`, so the daemon may switch frames to
        // the compressed binary stream at any point. Both sides have to stay in
        // step across the switch and across the shared dictionary.
        let deflate = std::sync::atomic::AtomicBool::new(true);
        let compressed = ["c1\nc2\nc3\n$ \n", "c2\nc3\nc4\n$ \n"];
        for (step, content) in compressed.iter().enumerate() {
            let message = encoder.encode(
                PendingFrame {
                    content: (*content).to_string(),
                    cursor: Some(cursor(4 + step as u32, 3)),
                    full: false,
                },
                true,
                &deflate,
            );
            let axum::extract::ws::Message::Binary(bytes) = message else {
                panic!("an advertised deflate stream sends binary frames");
            };
            server
                .send(Message::Binary(bytes.to_vec().into()))
                .await
                .unwrap();
            let frame = loop {
                if let Some(frame) = preview.take_frame() {
                    break frame;
                }
                tokio::time::timeout(Duration::from_secs(5), wake.notified())
                    .await
                    .expect("the compressed frame lands");
            };
            assert_eq!(frame.content, *content, "step {step}");
        }

        worker.abort();
    }

    #[test]
    fn a_wheel_burst_collapses_to_its_net_scroll_and_wakes_the_worker_once() {
        let (preview, mut commands) = RemotePreview::recording();
        for _ in 0..10 {
            preview.wheel(true, 4, 9);
        }
        for _ in 0..3 {
            preview.wheel(false, 4, 9);
        }
        assert!(matches!(commands.try_recv(), Ok(PreviewCommand::Wheel)));
        assert!(
            commands.try_recv().is_err(),
            "13 notches ride one bounded wake"
        );
        let pending = preview.slots.take_wheel().expect("a pending burst");
        assert_eq!(
            pending,
            PendingWheel {
                col: 4,
                row: 9,
                notches: 7
            },
            "opposing notches cancel"
        );
        assert_eq!(pending.burst(), Some((true, 7)));
        assert!(preview.slots.take_wheel().is_none(), "the slot is drained");

        // A drained slot wakes the worker again.
        preview.wheel(false, 4, 9);
        assert!(matches!(commands.try_recv(), Ok(PreviewCommand::Wheel)));

        // Moving to another cell restarts the count there.
        preview.wheel(false, 5, 9);
        preview.wheel(false, 5, 9);
        assert_eq!(
            preview.slots.take_wheel(),
            Some(PendingWheel {
                col: 5,
                row: 9,
                notches: -2
            }),
            "a new cell replaces rather than merges"
        );
    }

    #[test]
    fn a_settled_wheel_sends_nothing_and_a_fling_clamps_to_the_wire_cap() {
        let cap = crate::tmux::mouse::MAX_WHEEL_NOTCHES;
        let cases = [
            (0, None),
            (1, Some((true, 1))),
            (-1, Some((false, 1))),
            (i32::from(cap) + 5, Some((true, cap))),
            (i32::MIN, Some((false, cap))),
        ];
        for (notches, want) in cases {
            let pending = PendingWheel {
                col: 0,
                row: 0,
                notches,
            };
            assert_eq!(pending.burst(), want, "{notches}");
        }
    }

    #[test]
    fn only_the_newest_preview_size_is_kept() {
        let (preview, mut commands) = RemotePreview::recording();
        preview.resize(80, 24);
        preview.resize(100, 30);
        preview.resize(120, 40);
        assert!(matches!(commands.try_recv(), Ok(PreviewCommand::Resize)));
        assert!(commands.try_recv().is_err(), "one wake for the whole drag");
        assert_eq!(preview.slots.take_resize(), Some((120, 40)));
        assert!(preview.slots.take_resize().is_none());
    }

    #[test]
    fn a_take_over_queued_behind_a_watch_is_kept() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(PreviewCommand::Stop).unwrap();
        tx.send(PreviewCommand::TakeOver { cols: 80, rows: 24 })
            .unwrap();
        tx.send(PreviewCommand::Input(b"x".to_vec())).unwrap();
        tx.send(PreviewCommand::Stop).unwrap();
        let mut backlog = VecDeque::new();
        assert!(matches!(
            latest_target(PreviewCommand::Stop, &mut rx, &mut backlog),
            PreviewCommand::Stop
        ));
        let kinds: Vec<_> = backlog
            .iter()
            .map(|c| match c {
                PreviewCommand::TakeOver { .. } => "take-over",
                PreviewCommand::Input(_) => "input",
                PreviewCommand::Stop => "stop",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["take-over", "input", "stop"]);
    }
}
