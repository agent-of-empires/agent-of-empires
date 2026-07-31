//! Pure selectors for rendering the daemon's plugin UI-state snapshot in the
//! native TUI (#2402). Mirrors the web selectors in `web/src/lib/pluginUi.ts`,
//! narrowed to what a terminal can render: the structured view shows
//! `StatusBar` (global) and `DetailBadge` (per-session) text, tone-colored,
//! plus `Notification` toasts, and `Pane` blocks in a toggleable overlay
//! (#2467); the remote-home picker shows `RowColumn` text per session row
//! (#2948). Icons, tooltips, hrefs, and the
//! `Card`/`RowBadge`/`SortKey`/`FilterFacet`/`SettingsPage`/
//! `ToolCardBadge` slots have no TUI surface here and are ignored (a terminal
//! cannot render a routed full page). `ToolCardBadge` renders on the web
//! tool-call cards only; the TUI would need MCP/skill target classification it
//! does not carry today, tracked as a follow-up.
//!
//! Kept side-effect-free so the render layer can borrow the snapshot and so the
//! filtering / tone-mapping logic is unit-testable without a daemon.

use aoe_plugin_api::UiSlot;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

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

/// This session's `RowColumn` cells as `(text, tone)`, in snapshot order
/// (#2948). One session can carry several, one per plugin, so the caller
/// renders them side by side the way the web maps over every entry. Entries
/// with no renderable text drop out; `tooltip` has no terminal surface and is
/// ignored, as with the other slots.
pub fn row_column_cells(snapshot: &UiSnapshot, session_id: &str) -> Vec<(String, Option<Tone>)> {
    session_entries(snapshot, UiSlot::RowColumn, session_id)
        .filter_map(|e| entry_text(e).map(|t| (t.to_string(), entry_tone(e))))
        .collect()
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

/// Width of a `divider` block's rule. The renderer pre-wraps every line to the
/// panel width, so a fixed width is fine: a narrow pane wraps the rule
/// (harmless) and a wide one shows a partial rule rather than spanning the whole
/// width. Not worth threading the render width down for a decorative line.
const DIVIDER_WIDTH: usize = 32;

/// Render the open session's `Pane` entries to terminal lines for the
/// toggleable pane panel (#2467). Mirrors the web renderer's block vocabulary
/// (`web/src/components/plugin/PluginSlots.tsx`), narrowed to what a terminal
/// shows: text and tone only, with icons / hrefs / tooltips dropped and
/// `action` blocks rendered as inert labels (interactive firing is a #2467
/// follow-up). Forward-compatible: an unknown block `kind` renders nothing
/// rather than failing, so a newer plugin can push kinds this host has not
/// heard of. Entries are blank-line separated, and an entry that renders
/// nothing contributes no separator (so a malformed payload leaves no gap).
pub fn pane_lines(snapshot: &UiSnapshot, session_id: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    for entry in session_entries(snapshot, UiSlot::Pane, session_id) {
        let lines = pane_entry_lines(entry, theme);
        if lines.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(Line::default());
        }
        out.extend(lines);
    }
    out
}

/// One pane entry: a heading naming the pane, then an ordered `blocks` list when
/// present, else the simple `{ title, body }` form (matching the web renderer's
/// precedence). The web shows the pane's name on its dock tab and so skips
/// `title` inside a `blocks` body; the TUI overlay has no tabs, so the heading
/// carries the attribution, falling back to the `plugin_id` the way the web's
/// `paneTitle` does (`web/src/lib/pluginPanes.ts`).
fn pane_entry_lines(entry: &UiEntry, theme: &Theme) -> Vec<Line<'static>> {
    if let Some(blocks) = entry.payload.get("blocks").and_then(Value::as_array) {
        let body: Vec<Line<'static>> = blocks
            .iter()
            .flat_map(|b| block_lines(b, 0, theme))
            .collect();
        // No renderable block means no heading either: an empty or malformed
        // payload must not leave a bare plugin name on screen.
        if body.is_empty() {
            return body;
        }
        let heading = block_str(&entry.payload, "title").unwrap_or(entry.plugin_id.as_str());
        let mut out = vec![indented_line(
            0,
            heading.to_string(),
            Style::default()
                .fg(theme.title)
                .add_modifier(Modifier::BOLD),
        )];
        out.extend(body);
        return out;
    }
    let mut out: Vec<Line<'static>> = Vec::new();
    if let Some(title) = block_str(&entry.payload, "title") {
        out.push(indented_line(
            0,
            title.to_string(),
            Style::default()
                .fg(theme.title)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(body) = block_str(&entry.payload, "body") {
        for l in body.lines() {
            out.push(indented_line(
                0,
                l.to_string(),
                Style::default().fg(theme.text),
            ));
        }
    }
    out
}

/// Render one block to lines, indented by `indent` spaces. `section` recurses
/// with a deeper indent. An unknown kind, or a known kind missing its required
/// field, yields no lines.
fn block_lines(block: &Value, indent: usize, theme: &Theme) -> Vec<Line<'static>> {
    match block.get("kind").and_then(Value::as_str) {
        Some("heading") => match block_str(block, "text") {
            Some(t) => vec![indented_line(
                indent,
                t.to_string(),
                Style::default()
                    .fg(theme.title)
                    .add_modifier(Modifier::BOLD),
            )],
            None => vec![],
        },
        Some("note") => match block_str(block, "text") {
            Some(t) => vec![indented_line(
                indent,
                t.to_string(),
                tone_style(block_tone(block), theme),
            )],
            None => vec![],
        },
        Some("divider") => vec![indented_line(
            indent,
            "─".repeat(DIVIDER_WIDTH),
            Style::default().fg(theme.dimmed),
        )],
        Some("action") => match block_str(block, "label") {
            // Inert in this read-only pass: the label tells the user the plugin
            // exposes an action the TUI cannot yet fire (#2467 follow-up).
            Some(l) => vec![indented_line(
                indent,
                format!("[action] {l}"),
                Style::default().fg(theme.dimmed),
            )],
            None => vec![],
        },
        Some("row") => row_lines(block, indent, theme),
        Some("comment") => comment_lines(block, indent, theme),
        Some("section") => section_lines(block, indent, theme),
        _ => vec![],
    }
}

/// `row`: `label value sublabel` on one line, the value tone-colored. Drops
/// icon / href / color (no terminal surface). Renders nothing without a label or
/// a value: the web guard is `!label && !value && !iconComp`, and the icon leg
/// can never carry a row here, so a sublabel-only row is dropped just as it is
/// on the web.
fn row_lines(block: &Value, indent: usize, theme: &Theme) -> Vec<Line<'static>> {
    let label = block_str(block, "label");
    let value = block_str(block, "value");
    let sublabel = block_str(block, "sublabel");
    if label.is_none() && value.is_none() {
        return vec![];
    }
    let mut spans = indent_span(indent);
    if let Some(l) = label {
        spans.push(Span::styled(
            l.to_string(),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(v) = value {
        push_sep(&mut spans, indent);
        spans.push(Span::styled(
            v.to_string(),
            tone_style(block_tone(block), theme),
        ));
    }
    if let Some(s) = sublabel {
        push_sep(&mut spans, indent);
        spans.push(Span::styled(
            s.to_string(),
            Style::default().fg(theme.dimmed),
        ));
    }
    vec![Line::from(spans)]
}

/// `comment`: a read-only PR review comment. A header line (author, optional
/// `path:line`, resolved / unresolved marker) then the wrapped body. Drops the
/// href. Renders nothing if both author and body are absent.
fn comment_lines(block: &Value, indent: usize, theme: &Theme) -> Vec<Line<'static>> {
    let author = block_str(block, "author");
    let body = block_str(block, "body");
    if author.is_none() && body.is_none() {
        return vec![];
    }
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut header = indent_span(indent);
    if let Some(a) = author {
        header.push(Span::styled(
            a.to_string(),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(p) = block_str(block, "path") {
        let where_ = match block.get("line").and_then(Value::as_i64) {
            Some(n) => format!("  {p}:{n}"),
            None => format!("  {p}"),
        };
        header.push(Span::styled(where_, Style::default().fg(theme.dimmed)));
    }
    let resolved = block
        .get("resolved")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (marker, color) = if resolved {
        ("  resolved", theme.running)
    } else {
        ("  unresolved", theme.waiting)
    };
    header.push(Span::styled(marker.to_string(), Style::default().fg(color)));
    out.push(Line::from(header));
    if let Some(b) = body {
        for l in b.lines() {
            out.push(indented_line(
                indent,
                l.to_string(),
                Style::default().fg(theme.text),
            ));
        }
    }
    out
}

/// `section`: an uppercase title then its children, recursively, indented one
/// level deeper. A toned section keeps its tone on the title (the web tints the
/// title when the block carries a tone or an icon, else dims it), so a folded-up
/// status like a failing check stays visible at a glance. Always expanded; the
/// TUI has no fold affordance, so hiding the children would drop data with no way
/// to reveal it.
fn section_lines(block: &Value, indent: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    if let Some(title) = block_str(block, "title") {
        // `tone_style` already maps no-tone (and an explicit neutral) to dimmed,
        // which is the web's untoned title color.
        out.push(indented_line(
            indent,
            title.to_uppercase(),
            tone_style(block_tone(block), theme).add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(children) = block.get("children").and_then(Value::as_array) {
        for c in children {
            out.extend(block_lines(c, indent + 2, theme));
        }
    }
    out
}

/// The block's tone, if it carries a valid one.
fn block_tone(block: &Value) -> Option<Tone> {
    block
        .get("tone")
        .and_then(|v| serde_json::from_value::<Tone>(v.clone()).ok())
}

/// A trimmed, non-empty string field, or `None`. Defensive against a malformed
/// or schema-skewed block so the renderer never panics on plugin data.
fn block_str<'a>(block: &'a Value, key: &str) -> Option<&'a str> {
    block
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Leading indent spans for a line, empty at indent 0.
fn indent_span(indent: usize) -> Vec<Span<'static>> {
    if indent == 0 {
        Vec::new()
    } else {
        vec![Span::raw(" ".repeat(indent))]
    }
}

/// Push a single-space separator before the next span on a multi-field line.
/// Skips the leading space when the line started with an indent span (the
/// indent already separates it from the margin).
fn push_sep(spans: &mut Vec<Span<'static>>, indent: usize) {
    // The line is empty, or holds only the leading indent span: no field has
    // been pushed yet, so the next one needs no separator.
    let only_indent = indent > 0 && spans.len() == 1;
    if !spans.is_empty() && !only_indent {
        spans.push(Span::raw(" "));
    }
}

/// A single styled line at `indent` spaces.
fn indented_line(indent: usize, text: String, style: Style) -> Line<'static> {
    if indent == 0 {
        Line::from(Span::styled(text, style))
    } else {
        Line::from(vec![
            Span::raw(" ".repeat(indent)),
            Span::styled(text, style),
        ])
    }
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

    fn pane_snapshot(entries: serde_json::Value) -> UiSnapshot {
        snapshot(entries, json!([]))
    }

    #[test]
    fn row_column_cells_read_text_and_tone_for_this_session_only() {
        let snap = pane_snapshot(json!([
            {"plugin_id": "gh", "slot": "row-column", "id": "st", "session_id": "s1",
             "payload": {"text": "CI failing", "tone": "danger", "tooltip": "dropped"}},
            {"plugin_id": "gh", "slot": "row-column", "id": "st", "session_id": "s2",
             "payload": {"text": "approved", "tone": "success"}},
            {"plugin_id": "gh", "slot": "row-column", "id": "st",
             "payload": {"text": "global"}}
        ]));
        assert_eq!(
            row_column_cells(&snap, "s1"),
            vec![("CI failing".to_string(), Some(Tone::Danger))]
        );
        assert!(row_column_cells(&snap, "s3").is_empty());
    }

    #[test]
    fn row_column_cells_keep_every_plugin_in_snapshot_order() {
        let snap = pane_snapshot(json!([
            {"plugin_id": "a", "slot": "row-column", "id": "x", "session_id": "s1",
             "payload": {"text": "first"}},
            {"plugin_id": "b", "slot": "row-column", "id": "y", "session_id": "s1",
             "payload": {"text": "second", "tone": "warn"}}
        ]));
        assert_eq!(
            row_column_cells(&snap, "s1"),
            vec![
                ("first".to_string(), None),
                ("second".to_string(), Some(Tone::Warn))
            ]
        );
    }

    #[test]
    fn row_column_cells_drop_blank_nonstring_and_missing_text() {
        let snap = pane_snapshot(json!([
            {"plugin_id": "a", "slot": "row-column", "id": "1", "session_id": "s1",
             "payload": {"text": "   "}},
            {"plugin_id": "a", "slot": "row-column", "id": "2", "session_id": "s1",
             "payload": {"text": 7}},
            {"plugin_id": "a", "slot": "row-column", "id": "3", "session_id": "s1",
             "payload": {}},
            {"plugin_id": "a", "slot": "row-column", "id": "4", "session_id": "s1",
             "payload": {"text": "kept", "tone": "chartreuse"}}
        ]));
        // The invalid tone degrades to None rather than dropping the cell.
        assert_eq!(
            row_column_cells(&snap, "s1"),
            vec![("kept".to_string(), None)]
        );
    }

    #[test]
    fn row_column_cells_ignore_other_slots() {
        let snap = pane_snapshot(json!([
            {"plugin_id": "a", "slot": "row-badge", "id": "b", "session_id": "s1",
             "payload": {"text": "badge"}},
            {"plugin_id": "a", "slot": "detail-badge", "id": "d", "session_id": "s1",
             "payload": {"text": "detail"}}
        ]));
        assert!(row_column_cells(&snap, "s1").is_empty());
    }

    /// Flatten rendered lines to their plain text, one string per line, so a
    /// test can assert on content without spelling out styles.
    fn texts(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn pane_entry(payload: serde_json::Value) -> serde_json::Value {
        json!([{"plugin_id": "p", "slot": "pane", "id": "gh", "session_id": "s1", "payload": payload}])
    }

    #[test]
    fn pane_simple_title_body_form() {
        let snap = pane_snapshot(pane_entry(json!({"title": "Checks", "body": "all\ngood"})));
        let lines = pane_lines(&snap, "s1", &Theme::default());
        assert_eq!(texts(&lines), vec!["Checks", "all", "good"]);
    }

    #[test]
    fn pane_filters_by_session_exactly() {
        let snap = pane_snapshot(json!([
            {"plugin_id": "p", "slot": "pane", "id": "a", "session_id": "s1", "payload": {"title": "mine"}},
            {"plugin_id": "p", "slot": "pane", "id": "b", "session_id": "s2", "payload": {"title": "other"}},
            {"plugin_id": "p", "slot": "pane", "id": "c", "payload": {"title": "global"}}
        ]));
        let lines = pane_lines(&snap, "s1", &Theme::default());
        assert_eq!(texts(&lines), vec!["mine"]);
    }

    #[test]
    fn pane_separates_multiple_entries_with_blank_line() {
        let snap = pane_snapshot(json!([
            {"plugin_id": "p", "slot": "pane", "id": "a", "session_id": "s1", "payload": {"title": "one"}},
            {"plugin_id": "p", "slot": "pane", "id": "b", "session_id": "s1", "payload": {"title": "two"}}
        ]));
        let lines = pane_lines(&snap, "s1", &Theme::default());
        assert_eq!(texts(&lines), vec!["one", "", "two"]);
    }

    #[test]
    fn pane_renders_known_block_kinds() {
        let snap = pane_snapshot(pane_entry(json!({"title": "GH", "blocks": [
            {"kind": "heading", "text": "GitHub"},
            {"kind": "row", "label": "nexus", "value": "PR #12", "sublabel": "open"},
            {"kind": "note", "text": "heads up", "tone": "warn"},
            {"kind": "divider"},
            {"kind": "action", "label": "Refresh", "method": "refresh"}
        ]})));
        let lines = pane_lines(&snap, "s1", &Theme::default());
        let t = texts(&lines);
        assert_eq!(t[0], "GH");
        assert_eq!(t[1], "GitHub");
        assert_eq!(t[2], "nexus PR #12 open");
        assert_eq!(t[3], "heads up");
        assert_eq!(t[4], "─".repeat(DIVIDER_WIDTH));
        // Action is inert: a label, not a fired button.
        assert_eq!(t[5], "[action] Refresh");
    }

    #[test]
    fn pane_renders_nested_section_indented() {
        let snap = pane_snapshot(pane_entry(json!({"blocks": [
            {"kind": "section", "title": "Reviews", "children": [
                {"kind": "row", "label": "approved", "value": "2"}
            ]}
        ]})));
        let lines = pane_lines(&snap, "s1", &Theme::default());
        let t = texts(&lines);
        assert_eq!(t[1], "REVIEWS");
        assert_eq!(t[2], "  approved 2");
    }

    #[test]
    fn pane_renders_comment_header_and_body() {
        let snap = pane_snapshot(pane_entry(json!({"blocks": [
            {"kind": "comment", "author": "octocat", "path": "src/x.rs", "line": 9,
             "resolved": false, "body": "needs a test"}
        ]})));
        let lines = pane_lines(&snap, "s1", &Theme::default());
        let t = texts(&lines);
        assert_eq!(t[1], "octocat  src/x.rs:9  unresolved");
        assert_eq!(t[2], "needs a test");
    }

    #[test]
    fn pane_ignores_unknown_kinds_and_titles_the_entry() {
        // Unknown kind drops out; the payload title heads the entry, and a
        // stray `body` stays ignored while `blocks` is present (web parity).
        let snap = pane_snapshot(pane_entry(json!({
            "title": "GitHub",
            "body": "ignored",
            "blocks": [
                {"kind": "some-future-kind", "whatever": true},
                {"kind": "heading", "text": "kept"}
            ]
        })));
        let lines = pane_lines(&snap, "s1", &Theme::default());
        assert_eq!(texts(&lines), vec!["GitHub", "kept"]);
    }

    #[test]
    fn pane_entry_heading_falls_back_to_plugin_id() {
        // No payload title: the heading names the plugin, so two plugins'
        // stacked panes stay attributable without the web's dock tabs.
        let snap = pane_snapshot(pane_entry(
            json!({"blocks": [{"kind": "heading", "text": "Checks"}]}),
        ));
        let lines = pane_lines(&snap, "s1", &Theme::default());
        assert_eq!(texts(&lines), vec!["p", "Checks"]);
    }

    #[test]
    fn pane_skips_blocks_missing_required_fields_without_panicking() {
        let snap = pane_snapshot(pane_entry(json!({"blocks": [
            {"kind": "heading"},
            {"kind": "row"},
            {"kind": "comment"},
            {"kind": "note", "text": "  "}
        ]})));
        let lines = pane_lines(&snap, "s1", &Theme::default());
        assert!(lines.is_empty());
    }

    #[test]
    fn pane_entry_rendering_nothing_contributes_no_heading_or_separator() {
        // A middle entry whose blocks all drop out must leave no heading and no
        // blank-line gap: exactly one separator between the two that do render.
        let snap = pane_snapshot(json!([
            {"plugin_id": "p", "slot": "pane", "id": "a", "session_id": "s1", "payload": {"title": "one"}},
            {"plugin_id": "q", "slot": "pane", "id": "b", "session_id": "s1",
             "payload": {"title": "empty", "blocks": [{"kind": "row"}]}},
            {"plugin_id": "r", "slot": "pane", "id": "c", "session_id": "s1", "payload": {"title": "two"}}
        ]));
        let lines = pane_lines(&snap, "s1", &Theme::default());
        assert_eq!(texts(&lines), vec!["one", "", "two"]);
    }

    #[test]
    fn pane_row_needs_a_label_or_value_but_a_comment_needs_only_one_field() {
        // Web parity: `BlockRow` bails on `!label && !value && !iconComp`, and
        // the TUI renders no icons, so a sublabel-only row is dropped. A
        // `BlockComment` bails only on `!author && !body`, so a body-only
        // comment still renders.
        let snap = pane_snapshot(pane_entry(json!({"blocks": [
            {"kind": "row", "sublabel": "orphan"},
            {"kind": "comment", "body": "no author"}
        ]})));
        let lines = pane_lines(&snap, "s1", &Theme::default());
        let t = texts(&lines);
        assert!(!t.iter().any(|l| l.contains("orphan")), "{t:?}");
        assert_eq!(t[1], "  unresolved");
        assert_eq!(t[2], "no author");
    }

    #[test]
    fn pane_section_title_keeps_its_tone() {
        let theme = Theme::default();
        let snap = pane_snapshot(pane_entry(json!({"blocks": [
            {"kind": "section", "title": "checks", "tone": "danger", "children": []},
            {"kind": "section", "title": "reviews", "children": []}
        ]})));
        let lines = pane_lines(&snap, "s1", &theme);
        // A toned section header carries the tone color; an untoned one dims,
        // matching the web's `tone ? toneTextClass(tone) : "text-text-dim"`.
        assert_eq!(lines[1].spans[0].style.fg, Some(theme.error));
        assert_eq!(lines[2].spans[0].style.fg, Some(theme.dimmed));
    }
}
