//! Who may set one tmux session's pane size.
//!
//! Every surface that can resize a pane (the local TUI's preview and live
//! send, the remote TUI client, the web dashboard's live and passive views,
//! `aoe attach`) asks this module the same question, against the same lock in
//! tmux user options, so one rule decides for all of them:
//!
//! * Live mode and a full attach always take the lock and size the pane, and
//!   mark it live sized.
//! * Merely watching takes the lock only while it is free and the pane has
//!   never been live sized, so a reader never resizes a pane someone else has
//!   set up, and never steals from a holder.
//! * Releasing the lock (exit, deselect, quit, stale heartbeat) leaves the
//!   pane at its size. The live-sized mark outlives the holder and clears
//!   when the pane restarts.

use std::time::Duration;

/// How a client holds the lock: `Live` drives the pane (input, its own
/// geometry), `View` only reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeMode {
    View,
    Live,
}

impl SizeMode {
    pub(crate) fn wire(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Live => "live",
        }
    }

    pub(crate) fn parse(value: &str) -> Self {
        // An older writer stored no mode. It only ever claimed to drive the
        // pane, so an unlabelled lock reads as live.
        match value {
            "view" => Self::View,
            _ => Self::Live,
        }
    }
}

/// The lock as tmux holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizeLock {
    /// Stable per client, and the identity every write is fenced against.
    pub holder: String,
    /// What this holder called itself, e.g. `mac-mini (aoe)`. `None` when it
    /// wrote no label: one left behind by a previous holder is dropped rather
    /// than read as this one's, since naming the wrong device is worse than
    /// naming none.
    pub label: Option<String>,
    pub mode: SizeMode,
    pub heartbeat_ms: u64,
}

impl SizeLock {
    /// What to call this holder to a user. Ids are minted per surface, so an
    /// unlabelled holder is still placed: the web dashboard's live viewers
    /// are `live-*` (`crate::server::live_ws`), other aoe TUIs `tui-*`.
    pub fn describe(&self) -> String {
        if let Some(label) = &self.label {
            return label.clone();
        }
        if self.holder.starts_with("live-") {
            "the web dashboard".to_string()
        } else if self.holder.starts_with("tui-") {
            "another aoe TUI".to_string()
        } else {
            self.holder.clone()
        }
    }
}

/// One session's size state: the lock, and whether a live client has sized
/// this pane since it started.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SizeState {
    pub lock: Option<SizeLock>,
    pub live_sized: bool,
}

impl SizeState {
    /// The holder, ignoring one whose heartbeat has aged out.
    pub fn active(&self, now_ms: u64, ttl: Duration) -> Option<&SizeLock> {
        self.lock
            .as_ref()
            .filter(|lock| now_ms.saturating_sub(lock.heartbeat_ms) <= ttl.as_millis() as u64)
    }
}

/// Whether `who` may take the lock in `want`, given what tmux holds.
///
/// Live always takes it: entering live mode or attaching is an explicit act on
/// this pane, and the client it displaces is told who took over. A watcher
/// takes it only over a free lock on a pane no live client has sized, so
/// pre-sizing never fights another viewer and never undoes a live layout.
pub fn may_claim(now_ms: u64, state: &SizeState, who: &str, want: SizeMode, ttl: Duration) -> bool {
    if want == SizeMode::Live {
        return true;
    }
    let free = state
        .active(now_ms, ttl)
        .is_none_or(|lock| lock.holder == who);
    free && !state.live_sized
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(4);

    fn lock(holder: &str, mode: SizeMode, heartbeat_ms: u64) -> Option<SizeLock> {
        Some(SizeLock {
            holder: holder.to_string(),
            label: Some(format!("{holder} (aoe)")),
            mode,
            heartbeat_ms,
        })
    }

    #[test]
    fn live_always_takes_the_lock_and_a_viewer_only_takes_a_free_untouched_pane() {
        let now = 10_000;
        let stale = now - TTL.as_millis() as u64 - 1;
        let cases = [
            ("free pane, viewer", None, false, SizeMode::View, true),
            ("free pane, live", None, false, SizeMode::Live, true),
            (
                "another viewer holds it",
                lock("mini", SizeMode::View, now),
                false,
                SizeMode::View,
                false,
            ),
            (
                "live takes it from a viewer",
                lock("mini", SizeMode::View, now),
                false,
                SizeMode::Live,
                true,
            ),
            (
                "live takes it from a live holder",
                lock("mini", SizeMode::Live, now),
                true,
                SizeMode::Live,
                true,
            ),
            (
                "a stale holder counts as free",
                lock("mini", SizeMode::View, stale),
                false,
                SizeMode::View,
                true,
            ),
            (
                "our own lock refreshes",
                lock("me", SizeMode::View, now),
                false,
                SizeMode::View,
                true,
            ),
            (
                "a live-sized pane refuses viewers",
                None,
                true,
                SizeMode::View,
                false,
            ),
            (
                "a live-sized pane still takes live",
                None,
                true,
                SizeMode::Live,
                true,
            ),
        ];
        for (what, lock, live_sized, want, expected) in cases {
            let state = SizeState { lock, live_sized };
            assert_eq!(may_claim(now, &state, "me", want, TTL), expected, "{what}");
        }
    }

    #[test]
    fn a_stale_holder_is_not_active_and_an_unlabelled_mode_reads_as_live() {
        let now = 10_000;
        let state = SizeState {
            lock: lock("mini", SizeMode::Live, now - 10),
            live_sized: true,
        };
        assert_eq!(state.active(now, TTL).map(|l| l.mode), Some(SizeMode::Live));
        assert!(state
            .active(now + TTL.as_millis() as u64 + 1, TTL)
            .is_none());
        assert_eq!(SizeMode::parse(""), SizeMode::Live);
        assert_eq!(SizeMode::parse("view"), SizeMode::View);
        assert_eq!(SizeMode::Live.wire(), "live");
    }

    /// An unlabelled holder is still named, and from its own id: the label is
    /// a separate tmux option, so the one a previous holder left behind must
    /// never be read as this holder's name.
    #[test]
    fn a_holder_without_its_own_label_is_named_from_its_id() {
        let named = |holder: &str, label: Option<&str>| {
            SizeLock {
                holder: holder.to_string(),
                label: label.map(str::to_string),
                mode: SizeMode::Live,
                heartbeat_ms: 0,
            }
            .describe()
        };
        assert_eq!(named("live-7", None), "the web dashboard");
        assert_eq!(named("tui-7", None), "another aoe TUI");
        assert_eq!(named("live-7", Some("phone (web)")), "phone (web)");
        assert_eq!(named("terminal attach", None), "terminal attach");
    }
}
