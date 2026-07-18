//! Pure selectors for rendering the daemon's plugin UI-state snapshot in the
//! native TUI (#2402). Mirrors the web selectors in `web/src/lib/pluginUi.ts`,
//! narrowed to what a terminal can render: the structured view shows
//! `StatusBar` (global) and `DetailBadge` (per-session) text, tone-colored,
//! plus `Notification` toasts. Icons, tooltips, hrefs, and the
//! `Card`/`Pane`/`RowBadge`/`RowColumn`/`SortKey`/`FilterFacet`/`SettingsPage`
//! slots have no TUI surface here and are ignored (a terminal cannot render a
//! routed full page).
//!
//! Kept side-effect-free so the render layer can borrow the snapshot and so the
//! filtering / tone-mapping logic is unit-testable without a daemon.

use aoe_plugin_api::UiSlot;
use ratatui::style::{Color, Style};

use crate::plugin::ui_state::{Notification, Tone, UiEntry, UiSnapshot};
use crate::tui::styles::Theme;

/// Global entries for `slot`: those a plugin pushed without a `session_id`.
pub fn global_entries(snapshot: &UiSnapshot, slot: UiSlot) -> impl Iterator<Item = &UiEntry> {
    snapshot
        .entries
        .iter()
        .filter(move |e| e.slot == slot && e.session_id.is_none())
}

/// Per-session entries for `slot` whose `session_id` matches exactly. The
/// exact match is a tearing guard: a snapshot can momentarily carry entries
/// for a session other than the one on screen, and showing those would
/// mislabel another session's state as this one's.
pub fn session_entries<'a>(
    snapshot: &'a UiSnapshot,
    slot: UiSlot,
    session_id: &'a str,
) -> impl Iterator<Item = &'a UiEntry> {
    snapshot
        .entries
        .iter()
        .filter(move |e| e.slot == slot && e.session_id.as_deref() == Some(session_id))
}

/// The renderable `text` of a `StatusBar` / `DetailBadge` entry, if present
/// and a non-empty string. Defensive: the daemon validates payloads, but a
/// malformed or schema-skewed entry must not panic the renderer.
pub fn entry_text(entry: &UiEntry) -> Option<&str> {
    entry
        .payload
        .get("text")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// The entry's tone, if it carries a valid one.
pub fn entry_tone(entry: &UiEntry) -> Option<Tone> {
    entry
        .payload
        .get("tone")
        .and_then(|v| serde_json::from_value::<Tone>(v.clone()).ok())
}

/// Map a tone to a foreground style against the active theme. `None` (no tone)
/// renders neutral. Reuses existing theme status colors rather than inventing
/// new fields, matching how the home view tones session rows.
pub fn tone_style(tone: Option<Tone>, theme: &Theme) -> Style {
    let color = tone_color(tone, theme);
    Style::default().fg(color)
}

fn tone_color(tone: Option<Tone>, theme: &Theme) -> Color {
    match tone {
        None | Some(Tone::Neutral) => theme.dimmed,
        Some(Tone::Info) => theme.accent,
        Some(Tone::Success) => theme.running,
        Some(Tone::Warn) => theme.waiting,
        Some(Tone::Danger) => theme.error,
    }
}

/// The highest notification seq in the snapshot, or 0 when there are none.
/// Used to initialize the "already seen" watermark so notifications that
/// predate opening the view do not toast on first load.
pub fn max_notification_seq(snapshot: &UiSnapshot) -> u64 {
    snapshot
        .notifications
        .iter()
        .map(|n| n.seq)
        .max()
        .unwrap_or(0)
}

/// Notifications newer than `since_seq` that target this session (global ones,
/// `session_id == None`, always count), in ascending seq order so they toast
/// in the order the plugin posted them.
pub fn new_notifications<'a>(
    snapshot: &'a UiSnapshot,
    since_seq: u64,
    session_id: &str,
) -> Vec<&'a Notification> {
    let mut out: Vec<&Notification> = snapshot
        .notifications
        .iter()
        .filter(|n| n.seq > since_seq)
        .filter(|n| {
            n.session_id.as_deref().is_none() || n.session_id.as_deref() == Some(session_id)
        })
        .collect();
    out.sort_by_key(|n| n.seq);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snapshot(entries: serde_json::Value, notifications: serde_json::Value) -> UiSnapshot {
        serde_json::from_value(json!({
            "entries": entries,
            "notifications": notifications,
        }))
        .expect("snapshot deserializes")
    }

    #[test]
    fn deserializes_wire_shape_with_omitted_optionals() {
        // session_id / body omitted on the wire (skip_serializing_if) must
        // still decode, not error.
        let snap = snapshot(
            json!([{
                "plugin_id": "p",
                "slot": "status-bar",
                "id": "x",
                "payload": {"text": "ok", "tone": "success"}
            }]),
            json!([{"seq": 1, "plugin_id": "p", "tone": "info", "title": "hi"}]),
        );
        assert_eq!(snap.entries.len(), 1);
        assert!(snap.entries[0].session_id.is_none());
        assert!(snap.notifications[0].body.is_none());
    }

    #[test]
    fn global_entries_exclude_per_session() {
        let snap = snapshot(
            json!([
                {"plugin_id": "p", "slot": "status-bar", "id": "g", "payload": {"text": "global"}},
                {"plugin_id": "p", "slot": "status-bar", "id": "s", "session_id": "sess-1", "payload": {"text": "scoped"}}
            ]),
            json!([]),
        );
        let got: Vec<&str> = global_entries(&snap, UiSlot::StatusBar)
            .filter_map(entry_text)
            .collect();
        assert_eq!(got, vec!["global"]);
    }

    #[test]
    fn session_entries_require_exact_match() {
        let snap = snapshot(
            json!([
                {"plugin_id": "p", "slot": "detail-badge", "id": "a", "session_id": "sess-1", "payload": {"text": "mine"}},
                {"plugin_id": "p", "slot": "detail-badge", "id": "b", "session_id": "sess-2", "payload": {"text": "other"}},
                {"plugin_id": "p", "slot": "detail-badge", "id": "c", "payload": {"text": "no-session"}}
            ]),
            json!([]),
        );
        let got: Vec<&str> = session_entries(&snap, UiSlot::DetailBadge, "sess-1")
            .filter_map(entry_text)
            .collect();
        assert_eq!(got, vec!["mine"]);
    }

    #[test]
    fn entry_text_ignores_missing_blank_or_nonstring() {
        let snap = snapshot(
            json!([
                {"plugin_id": "p", "slot": "status-bar", "id": "1", "payload": {"text": "   "}},
                {"plugin_id": "p", "slot": "status-bar", "id": "2", "payload": {"text": 42}},
                {"plugin_id": "p", "slot": "status-bar", "id": "3", "payload": {}}
            ]),
            json!([]),
        );
        assert_eq!(global_entries(&snap, UiSlot::StatusBar).count(), 3);
        assert_eq!(
            global_entries(&snap, UiSlot::StatusBar)
                .filter_map(entry_text)
                .count(),
            0
        );
    }

    #[test]
    fn entry_tone_parses_valid_and_drops_invalid() {
        let snap = snapshot(
            json!([
                {"plugin_id": "p", "slot": "status-bar", "id": "1", "payload": {"text": "a", "tone": "danger"}},
                {"plugin_id": "p", "slot": "status-bar", "id": "2", "payload": {"text": "b", "tone": "chartreuse"}},
                {"plugin_id": "p", "slot": "status-bar", "id": "3", "payload": {"text": "c"}}
            ]),
            json!([]),
        );
        let tones: Vec<Option<Tone>> = snap.entries.iter().map(entry_tone).collect();
        assert_eq!(tones, vec![Some(Tone::Danger), None, None]);
    }

    #[test]
    fn new_notifications_filters_by_seq_and_session_in_order() {
        let snap = snapshot(
            json!([]),
            json!([
                {"seq": 1, "plugin_id": "p", "tone": "info", "title": "old"},
                {"seq": 3, "plugin_id": "p", "tone": "info", "title": "global-new"},
                {"seq": 2, "plugin_id": "p", "tone": "info", "title": "mine", "session_id": "sess-1"},
                {"seq": 4, "plugin_id": "p", "tone": "info", "title": "other", "session_id": "sess-2"}
            ]),
        );
        let titles: Vec<&str> = new_notifications(&snap, 1, "sess-1")
            .iter()
            .map(|n| n.title.as_str())
            .collect();
        // seq>1, global or sess-1, ascending: seq 2 (mine) then seq 3 (global).
        assert_eq!(titles, vec!["mine", "global-new"]);
    }

    #[test]
    fn max_seq_handles_empty() {
        let snap = snapshot(json!([]), json!([]));
        assert_eq!(max_notification_seq(&snap), 0);
    }
}
