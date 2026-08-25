//! Pure selectors for rendering the daemon's plugin UI-state snapshot in the
//! native TUI (#2402). Mirrors the web selectors in `web/src/lib/pluginUi.ts`,
//! narrowed to what a terminal can render: the structured view shows
//! `StatusBar` (global) and `DetailBadge` (per-session) text, tone-colored,
//! plus `Notification` toasts, and `Pane` (per session) and `HomePane` (global)
//! blocks in a toggleable overlay (#2467); the remote-home picker shows
//! `RowBadge` (#2947) and `RowColumn` (#2948) text per session row.
//! Icons, tooltips, hrefs, and the
//! `Card`/`SortKey`/`FilterFacet` slots have no TUI surface here and
//! are ignored.
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

/// This session's `RowBadge` chips as `(text, tone)`, in snapshot order
/// (#2947). One entry is either a single badge or a list of them under `items`,
/// so the picker row can show several chips the way the web sidebar does.
///
/// The presence of `items` selects the list form: the documented `items: []`
/// clears the row rather than falling back to the entry's own `text`, and a
/// malformed `items` clears it too rather than silently rendering the other
/// form. A badge carrying only an `icon` yields nothing, since a lucide name
/// has no terminal glyph and a generic stand-in would keep the badge's presence
/// while dropping which state it meant. `tooltip` and `href` have no terminal
/// surface either; opening the `href` is tracked as #2528.
pub fn row_badge_cells(snapshot: &UiSnapshot, session_id: &str) -> Vec<(String, Option<Tone>)> {
    session_entries(snapshot, UiSlot::RowBadge, session_id)
        .flat_map(|e| match e.payload.get("items") {
            Some(items) => items
                .as_array()
                .map(|items| items.iter().filter_map(badge_cell).collect())
                .unwrap_or_default(),
            None => badge_cell(&e.payload).into_iter().collect::<Vec<_>>(),
        })
        .collect()
}

/// One badge's renderable `(text, tone)`, from either a whole `row-badge`
/// payload or one element of its `items` list: the two carry the same fields.
fn badge_cell(badge: &Value) -> Option<(String, Option<Tone>)> {
    Some((block_str(badge, "text")?.to_string(), block_tone(badge)))
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
    stack_pane_entries(session_entries(snapshot, UiSlot::Pane, session_id), theme)
}

/// Render a run of pane entries to lines, each entry's block body separated
/// from the next by a blank line. An entry that renders nothing contributes no
/// separator, so a malformed payload leaves no gap. Shared by the per-session
/// `pane_lines` and the global `home_pane_lines`.
fn stack_pane_entries<'a>(
    entries: impl Iterator<Item = &'a UiEntry>,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    for entry in entries {
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

/// Render global `HomePane` entries (session-less) with the same block
/// vocabulary as a session `Pane`, the host-wide docked surface a plugin
/// targets when its panel is not tied to a session. Entries stack in snapshot
/// (insertion) order, so several plugins compose without colliding. `HomePane`
/// reuses `PanePayload`, so a payload may carry `default_location`; it is a
/// session-dock concept and is ignored here.
pub fn home_pane_lines(snapshot: &UiSnapshot, theme: &Theme) -> Vec<Line<'static>> {
    stack_pane_entries(global_entries(snapshot, UiSlot::HomePane), theme)
}

/// One pane entry: a heading naming the pane, then an ordered `blocks` list when
/// present, else the simple `{ title, body }` form (matching the web renderer's
/// precedence). The web shows the pane's name on its dock tab and so skips
/// `title` inside a `blocks` body; the TUI overlay has no tabs, so the heading
/// carries the attribution, falling back to the `plugin_id` the way the web's
/// `paneTitle` does (`web/src/lib/pluginPanes.ts`).
fn pane_entry_lines(entry: &UiEntry, theme: &Theme) -> Vec<Line<'static>> {
    // The footer belongs to the entry, not to the `blocks` form: a payload can
    // pair it with the simple `{ title, body }` shape, and a block list that all
    // drops out still has a status line worth showing. Computed once, up front, so
    // both paths below append it and neither can forget.
    let footer = footer_lines(&entry.payload, theme);
    if let Some(blocks) = entry.payload.get("blocks").and_then(Value::as_array) {
        let body: Vec<Line<'static>> = blocks
            .iter()
            .flat_map(|b| block_lines(b, 0, theme))
            .collect();
        // Nothing renderable at all means no heading either: an empty or malformed
        // payload must not leave a bare plugin name on screen. A footer counts as
        // content, so it keeps the entry (and its heading) alive on its own.
        if body.is_empty() && footer.is_empty() {
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
        // The web pins the footer below the scroll area; the TUI has no separate
        // viewport per entry, so it trails the blocks.
        out.extend(footer);
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
    out.extend(footer);
    out
}

/// A pane's `footer` status line: `text` then the tone-colored `value`. Drops the
/// icon. Yields nothing when the footer is absent, malformed, or carries neither
/// half, so a bare separator never appears.
fn footer_lines(payload: &Value, theme: &Theme) -> Vec<Line<'static>> {
    let Some(footer) = payload.get("footer") else {
        return vec![];
    };
    let text = block_str(footer, "text");
    let value = block_str(footer, "value");
    if text.is_none() && value.is_none() {
        return vec![];
    }
    let mut spans = Vec::new();
    if let Some(t) = text {
        spans.push(Span::styled(
            t.to_string(),
            Style::default().fg(theme.dimmed),
        ));
    }
    if let Some(v) = value {
        push_sep(&mut spans, 0);
        spans.push(Span::styled(
            v.to_string(),
            tone_style(block_tone(footer), theme),
        ));
    }
    vec![Line::from(spans)]
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
        Some("callout") => callout_lines(block, indent, theme),
        Some("bar") => bar_lines(block, indent, theme),
        Some("sparkline") => sparkline_lines(block, indent, theme),
        // The terminal has no side-by-side layout, so a `columns` block degrades
        // to its children stacked at the same indent, in order.
        Some("columns") => match block.get("children").and_then(Value::as_array) {
            Some(children) => children
                .iter()
                .flat_map(|c| block_lines(c, indent, theme))
                .collect(),
            None => vec![],
        },
        _ => vec![],
    }
}

/// `callout`: the pane's headline verdict. A toned title line, the detail
/// wrapped beneath it, then each of its actions as an inert `[action]` label
/// (same read-only treatment as a top-level `action`). Renders nothing without a
/// title or detail, matching the web guard.
fn callout_lines(block: &Value, indent: usize, theme: &Theme) -> Vec<Line<'static>> {
    let title = block_str(block, "title");
    let detail = block_str(block, "detail");
    if title.is_none() && detail.is_none() {
        return vec![];
    }
    let mut out: Vec<Line<'static>> = Vec::new();
    if let Some(t) = title {
        out.push(indented_line(
            indent,
            t.to_string(),
            tone_style(block_tone(block), theme).add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(d) = detail {
        for l in d.lines() {
            out.push(indented_line(
                indent,
                l.to_string(),
                Style::default().fg(theme.text),
            ));
        }
    }
    if let Some(actions) = block.get("actions").and_then(Value::as_array) {
        for a in actions {
            out.extend(block_lines(a, indent, theme));
        }
    }
    out
}

/// Cells in a `bar` block's text rendering. Fixed for the same reason
/// [`DIVIDER_WIDTH`] is: the bar is a proportion, not a measurement, so it does
/// not need the live panel width threaded down to read correctly.
const BAR_WIDTH: usize = 24;

/// `bar`: the proportional stacked bar as a run of block glyphs per segment,
/// each tone-colored, followed by the caption. Segments without a positive
/// numeric `value` are dropped; a bar with nothing left renders nothing.
fn bar_lines(block: &Value, indent: usize, theme: &Theme) -> Vec<Line<'static>> {
    let segments: Vec<(f64, Option<Tone>)> = block
        .get("segments")
        .and_then(Value::as_array)
        .map(|segs| {
            segs.iter()
                .filter_map(|s| {
                    let v = s.get("value").and_then(Value::as_f64)?;
                    (v > 0.0 && v.is_finite()).then(|| (v, block_tone(s)))
                })
                .collect()
        })
        .unwrap_or_default();
    if segments.is_empty() {
        return vec![];
    }
    let mut out = vec![Line::from(bar_spans(&segments, indent, theme))];
    if let Some(caption) = block_str(block, "caption") {
        out.push(indented_line(
            indent,
            caption.to_string(),
            Style::default().fg(theme.dimmed),
        ));
    }
    out
}

/// The eight block-fill glyphs, index 0 (lowest) to 7 (full).
const SPARK_GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// `sparkline`: a history plot as a run of block-eighths glyphs, one per value,
/// scaled against `max`, with an optional caption below. A time series the
/// other block kinds can't express (`bar` is a proportion, not a series). Wire
/// shape: `{ kind: "sparkline", values: [f64], max?: f64, tone?, bands?, caption? }`.
///
/// Coloring: `bands: [{ at: f64, tone }]` colors each glyph by the highest `at`
/// threshold its value meets, so a series can change color as it climbs
/// (green/amber/red for a pressure metric). Without `bands`, the whole
/// series takes the single `tone`. Provide `max` for a stable vertical scale;
/// it defaults to the data's own max, which rescales as the window changes.
/// Unknown fields are ignored (forward-compatible); an empty series renders
/// nothing.
fn sparkline_lines(block: &Value, indent: usize, theme: &Theme) -> Vec<Line<'static>> {
    let values: Vec<f64> = block
        .get("values")
        .and_then(Value::as_array)
        .map(|vs| {
            vs.iter()
                .filter_map(Value::as_f64)
                .filter(|v| v.is_finite())
                .collect()
        })
        .unwrap_or_default();
    if values.is_empty() {
        return vec![];
    }
    let data_max = values.iter().cloned().fold(0.0_f64, f64::max);
    let max = block
        .get("max")
        .and_then(Value::as_f64)
        .filter(|m| *m > 0.0)
        .unwrap_or(data_max)
        .max(f64::MIN_POSITIVE);
    let bands = parse_bands(block);
    let base_tone = block_tone(block);

    let mut spans = indent_span(indent);
    spans.extend(values.iter().map(|&v| {
        // frac is clamped to 0..=1, so idx lands in 0..=len-1 without a guard.
        let frac = (v / max).clamp(0.0, 1.0);
        let idx = (frac * (SPARK_GLYPHS.len() as f64 - 1.0)).round() as usize;
        let tone = band_tone(&bands, v).or(base_tone);
        Span::styled(SPARK_GLYPHS[idx].to_string(), tone_style(tone, theme))
    }));

    let mut out = vec![Line::from(spans)];
    if let Some(caption) = block_str(block, "caption") {
        out.push(indented_line(
            indent,
            caption.to_string(),
            Style::default().fg(theme.dimmed),
        ));
    }
    out
}

/// `(at, tone)` thresholds from a sparkline's `bands`, in declared order.
/// Malformed or missing yields none, so coloring falls back to the single `tone`.
fn parse_bands(block: &Value) -> Vec<(f64, Tone)> {
    block
        .get("bands")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|b| {
                    let at = b
                        .get("at")
                        .and_then(Value::as_f64)
                        .filter(|a| a.is_finite())?;
                    Some((at, block_tone(b)?))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The tone of the highest band `value` reaches, or `None` if it clears none.
fn band_tone(bands: &[(f64, Tone)], value: f64) -> Option<Tone> {
    bands
        .iter()
        .filter(|(at, _)| value >= *at)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, tone)| *tone)
}

/// Lay the segments out over [`BAR_WIDTH`] cells. Every positive segment gets at
/// least one cell so a tiny slice is still visible, and the rounding slack is
/// taken off the widest segments so the run is exactly `BAR_WIDTH` wide. With
/// more segments than cells the one-cell floor wins and the run is `segments.len()`
/// wide instead, which is the only case where it exceeds `BAR_WIDTH`.
fn bar_spans(segments: &[(f64, Option<Tone>)], indent: usize, theme: &Theme) -> Vec<Span<'static>> {
    let total: f64 = segments.iter().map(|(v, _)| v).sum();
    let mut cells: Vec<usize> = segments
        .iter()
        .map(|(v, _)| ((v / total) * BAR_WIDTH as f64).round().max(1.0) as usize)
        .collect();
    let sum: usize = cells.iter().sum();
    // Widest segment by cell count, so a correction lands where it is least
    // visible.
    let widest = |cells: &[usize]| {
        cells
            .iter()
            .enumerate()
            .max_by_key(|(_, c)| **c)
            .map(|(i, _)| i)
            .unwrap_or(0)
    };
    if sum > BAR_WIDTH {
        // Shed one cell at a time from whichever segment is currently widest:
        // taking the whole overshoot off a single segment cannot converge once the
        // one-cell floor bites (13 equal segments round to 2 cells each, 26 total,
        // and one segment can only give back 1). Stops when the floor leaves
        // nothing to take, which is exactly the more-segments-than-cells case.
        let mut over = sum - BAR_WIDTH;
        while over > 0 {
            let i = widest(&cells);
            if cells[i] <= 1 {
                break;
            }
            cells[i] -= 1;
            over -= 1;
        }
    } else if sum < BAR_WIDTH {
        let i = widest(&cells);
        cells[i] += BAR_WIDTH - sum;
    }
    let mut spans = indent_span(indent);
    for (cell_count, (_, tone)) in cells.iter().zip(segments) {
        spans.push(Span::styled(
            "█".repeat(*cell_count),
            tone_style(*tone, theme),
        ));
    }
    spans
}

/// Marks a `selected` row, which the web draws as a brand-accent ring. The
/// terminal has no border to tint, so the state has to live in the text.
const SELECTED_MARKER: &str = "▸ ";

/// `row`: `prefix label value sublabel badges` on one line, the prefix and value
/// tone-colored and the badges appended as a trailing status strip. Drops
/// icon / avatar / href / tooltip / color (no terminal surface), and a `method`
/// row renders as text rather than a control, the same read-only treatment
/// `action` gets. Renders nothing without a label, value or prefix: the web guard
/// also admits an icon-or-avatar-only row, and neither leg can carry a row here.
fn row_lines(block: &Value, indent: usize, theme: &Theme) -> Vec<Line<'static>> {
    let label = block_str(block, "label");
    let value = block_str(block, "value");
    let prefix = block_str(block, "prefix");
    let sublabel = block_str(block, "sublabel");
    let badges = row_badges(block);
    if label.is_none() && value.is_none() && prefix.is_none() {
        return vec![];
    }
    let tone = tone_style(block_tone(block), theme);
    let mut spans = indent_span(indent);
    if block.get("selected").and_then(Value::as_bool) == Some(true) {
        spans.push(Span::styled(
            SELECTED_MARKER.to_string(),
            Style::default().fg(theme.title),
        ));
    }
    if let Some(p) = prefix {
        spans.push(Span::styled(p.to_string(), tone));
    }
    if let Some(l) = label {
        push_sep(&mut spans, indent);
        spans.push(Span::styled(
            l.to_string(),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(v) = value {
        push_sep(&mut spans, indent);
        // `value_tone` decouples the trailing token from the row's tone (a status
        // glyph beside a neutral timestamp), falling back to the row's tone.
        spans.push(Span::styled(
            v.to_string(),
            match value_tone(block) {
                Some(t) => tone_style(Some(t), theme),
                None => tone,
            },
        ));
    }
    if let Some(s) = sublabel {
        push_sep(&mut spans, indent);
        spans.push(Span::styled(
            s.to_string(),
            Style::default().fg(theme.dimmed),
        ));
    }
    for (text, badge_tone) in badges {
        push_sep(&mut spans, indent);
        spans.push(Span::styled(text, tone_style(badge_tone, theme)));
    }
    vec![Line::from(spans)]
}

/// A row's or section's `badges` as `(text, tone)` pairs. An item's `text` is
/// what a terminal can show, so an icon-only badge (the web's glyph-only signal)
/// contributes nothing rather than an empty span.
fn row_badges(block: &Value) -> Vec<(String, Option<Tone>)> {
    block
        .get("badges")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|b| Some((block_str(b, "text")?.to_string(), block_tone(b))))
                .collect()
        })
        .unwrap_or_default()
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
    let value = block_str(block, "value");
    let badges = row_badges(block);
    if let Some(title) = block_str(block, "title") {
        // `tone_style` already maps no-tone (and an explicit neutral) to dimmed,
        // which is the web's untoned title color.
        let mut spans = indent_span(indent);
        spans.push(Span::styled(
            title.to_uppercase(),
            tone_style(block_tone(block), theme).add_modifier(Modifier::BOLD),
        ));
        // The web pins the header summary right; the terminal appends it, which
        // keeps the association without needing the panel width.
        if let Some(v) = value {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                v.to_string(),
                tone_style(value_tone(block), theme),
            ));
        }
        for (text, badge_tone) in badges {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(text, tone_style(badge_tone, theme)));
        }
        out.push(Line::from(spans));
    }
    if let Some(children) = block.get("children").and_then(Value::as_array) {
        for c in children {
            out.extend(block_lines(c, indent + 2, theme));
        }
    }
    out
}

/// The tone of a pane block or a `row-badge` payload, if it carries a valid
/// one. Both spell the field the same way.
fn block_tone(block: &Value) -> Option<Tone> {
    block
        .get("tone")
        .and_then(|v| serde_json::from_value::<Tone>(v.clone()).ok())
}

/// The separate tone for a `row`/`section`'s trailing `value`, if it carries one.
fn value_tone(block: &Value) -> Option<Tone> {
    block
        .get("value_tone")
        .and_then(|v| serde_json::from_value::<Tone>(v.clone()).ok())
}

/// A trimmed, non-empty string field of a plugin-authored object, or `None`.
/// Defensive against a malformed or schema-skewed payload so the renderer never
/// panics on plugin data.
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
    fn row_badge_cells_read_both_payload_forms() {
        // One badge payload and the cells it must yield. Covers the single
        // badge form, the `items` list, and every shape that renders nothing.
        let cases: [(serde_json::Value, &[(&str, Option<Tone>)]); 9] = [
            // Single badge: text and tone kept, the web-only fields dropped.
            (
                json!({"text": "open", "tone": "success", "icon": "git-pull-request",
                       "href": "https://example.test/1", "tooltip": "dropped"}),
                &[("open", Some(Tone::Success))],
            ),
            // Items keep snapshot order and carry their own tones.
            (
                json!({"items": [{"text": "draft"}, {"text": "2 checks", "tone": "warn"}]}),
                &[("draft", None), ("2 checks", Some(Tone::Warn))],
            ),
            // The documented "clear the row" form.
            (json!({"items": []}), &[]),
            // `items` present selects the list form, so the entry's own text
            // is not a fallback: a plugin clearing the row means it.
            (json!({"text": "stale", "items": []}), &[]),
            (
                json!({"text": "outer", "items": [{"text": "inner"}]}),
                &[("inner", None)],
            ),
            // Malformed `items` clears the row rather than quietly rendering
            // the other form.
            (json!({"text": "stale", "items": 7}), &[]),
            // A lucide name has no terminal glyph, so an icon-only badge
            // yields nothing; its text-carrying neighbours still render.
            (json!({"icon": "git-pull-request"}), &[]),
            (
                json!({"items": [{"icon": "check"}, {"text": "kept"}, {"text": "  "}, {"text": 7}]}),
                &[("kept", None)],
            ),
            // An unreadable tone degrades to neutral rather than dropping text.
            (
                json!({"text": "kept", "tone": "chartreuse"}),
                &[("kept", None)],
            ),
        ];
        for (payload, expected) in cases {
            let snap = pane_snapshot(json!([
                {"plugin_id": "gh", "slot": "row-badge", "id": "pr",
                 "session_id": "s1", "payload": payload}
            ]));
            let expected: Vec<(String, Option<Tone>)> = expected
                .iter()
                .map(|(t, tone)| ((*t).to_string(), *tone))
                .collect();
            assert_eq!(row_badge_cells(&snap, "s1"), expected);
        }
    }

    #[test]
    fn row_badge_cells_read_this_session_and_slot_only() {
        let snap = pane_snapshot(json!([
            {"plugin_id": "gh", "slot": "row-badge", "id": "pr", "session_id": "s1",
             "payload": {"text": "mine"}},
            {"plugin_id": "gh", "slot": "row-badge", "id": "pr", "session_id": "s2",
             "payload": {"text": "theirs"}},
            {"plugin_id": "gh", "slot": "row-badge", "id": "pr",
             "payload": {"text": "global"}},
            {"plugin_id": "gh", "slot": "row-column", "id": "col", "session_id": "s1",
             "payload": {"text": "column"}}
        ]));
        assert_eq!(
            row_badge_cells(&snap, "s1"),
            vec![("mine".to_string(), None)]
        );
        assert!(row_badge_cells(&snap, "s3").is_empty());
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
    fn pane_footer_renders_without_blocks_and_carries_an_all_dropped_entry() {
        // The footer belongs to the entry, so the simple title/body form gets it
        // too rather than only the `blocks` form.
        let simple = pane_snapshot(pane_entry(json!({
            "title": "GitHub", "body": "no PRs",
            "footer": {"text": "refreshed 12:07", "value": "ready"}
        })));
        assert_eq!(
            texts(&pane_lines(&simple, "s1", &Theme::default())),
            vec!["GitHub", "no PRs", "refreshed 12:07 ready"]
        );
        // And a block list that all drops out still has a status line worth
        // showing, so the footer keeps the entry (and its heading) alive.
        let dropped = pane_snapshot(pane_entry(json!({
            "title": "GitHub", "blocks": [{"kind": "row"}],
            "footer": {"text": "refreshed 12:07"}
        })));
        assert_eq!(
            texts(&pane_lines(&dropped, "s1", &Theme::default())),
            vec!["GitHub", "refreshed 12:07"]
        );
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
            {"kind": "callout"},
            {"kind": "bar", "segments": [{"value": 0}, {"tone": "info"}]},
            {"kind": "columns", "children": []},
            {"kind": "note", "text": "  "}
        ]})));
        let lines = pane_lines(&snap, "s1", &Theme::default());
        assert!(lines.is_empty());
    }

    #[test]
    fn pane_renders_the_api_12_block_kinds() {
        let snap = pane_snapshot(pane_entry(json!({"blocks": [
            {"kind": "callout", "tone": "danger", "icon": "circle-x",
             "title": "2 required checks failing", "detail": "Blocked until Clippy passes.",
             "actions": [{"kind": "action", "label": "Merge blocked", "method": "gh.merge", "disabled": true}]},
            {"kind": "bar", "caption": "18 files", "segments": [
                {"value": 750, "tone": "success"}, {"value": 250, "tone": "danger"}
            ]},
            // No side-by-side layout in a terminal: the children stack in order
            // at the columns block's own indent.
            {"kind": "columns", "children": [
                {"kind": "row", "label": "DIFF", "value": "+842 -317"},
                {"kind": "row", "prefix": "#3180", "label": "Stale daemon"}
            ]}
        ]})));
        let lines = pane_lines(&snap, "s1", &Theme::default());
        // The bar's two tone-colored spans join into one full-width run of cells.
        let bar = "█".repeat(BAR_WIDTH);
        assert_eq!(
            texts(&lines),
            vec![
                "p",
                "2 required checks failing",
                "Blocked until Clippy passes.",
                // A callout's actions get the same inert treatment as a
                // top-level `action`; the TUI cannot fire either yet.
                "[action] Merge blocked",
                &bar,
                "18 files",
                "DIFF +842 -317",
                "#3180 Stale daemon",
            ]
        );
    }

    #[test]
    fn pane_bar_cells_always_total_the_fixed_width() {
        // Rounding must never leave the run short or long, and a segment too
        // small to earn a cell still gets one so it stays visible. 13 equal
        // segments is the case that needs the repair to iterate: each rounds up to
        // 2 cells (26 total) and no single segment can give back more than 1.
        let equal_13 = [1.0; 13];
        let cases: [&[f64]; 6] = [
            &[1.0],
            &[1.0, 1.0, 1.0],
            &[999.0, 1.0],
            &[7.0, 11.0, 13.0],
            &equal_13,
            &[5.0, 5.0, 5.0, 5.0, 5.0, 5.0, 5.0],
        ];
        for values in cases {
            let segments: Vec<(f64, Option<Tone>)> = values.iter().map(|v| (*v, None)).collect();
            let spans = bar_spans(&segments, 0, &Theme::default());
            let width: usize = spans.iter().map(|s| s.content.chars().count()).sum();
            assert_eq!(width, BAR_WIDTH, "{values:?}");
            assert_eq!(spans.len(), values.len(), "{values:?}");
        }
        // More segments than cells: the one-cell floor wins, so the run is as wide
        // as the segment count. The only shape allowed to exceed BAR_WIDTH.
        let crowded: Vec<(f64, Option<Tone>)> = (0..BAR_WIDTH + 6).map(|_| (1.0, None)).collect();
        let spans = bar_spans(&crowded, 0, &Theme::default());
        let width: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        assert_eq!(width, crowded.len());
        assert!(spans.iter().all(|s| s.content.chars().count() == 1));
    }

    #[test]
    fn pane_row_and_section_render_the_api_12_fields() {
        let snap = pane_snapshot(pane_entry(json!({
            "blocks": [
                {"kind": "section", "title": "checks", "value": "1 of 2 approved", "value_tone": "warn",
                 "badges": [{"text": "2 failing", "tone": "danger"}, {"icon": "check", "tone": "success"}],
                 "children": [
                    {"kind": "row", "prefix": "#3231", "label": "warn when daemon is stale",
                     "sublabel": "japanese", "selected": true, "method": "gh.select_pr",
                     "badges": [{"text": "ci", "tone": "danger"}, {"icon": "circle-x", "tone": "danger"}]}
                 ]}
            ],
            "footer": {"text": "refreshed 12:07", "value": "blocked", "tone": "danger", "icon": "refresh-cw"}
        })));
        let lines = pane_lines(&snap, "s1", &Theme::default());
        assert_eq!(
            texts(&lines),
            vec![
                "p",
                // The header summary trails the title rather than pinning right;
                // an icon-only badge has no text a terminal can show.
                "CHECKS  1 of 2 approved  2 failing",
                // `selected` becomes a leading marker (there is no ring to tint),
                // and a `method` row is text, not a control.
                "  ▸ #3231 warn when daemon is stale japanese ci",
                "refreshed 12:07 blocked",
            ]
        );
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
    fn home_pane_renders_a_global_sparkline_entry() {
        // A global HomePane (no session_id) carrying a sparkline block renders
        // heading + glyph row + caption through home_pane_lines, covering the
        // whole payload -> render path for a home-pane plugin.
        let snap = pane_snapshot(json!([
            {"plugin_id": "diag", "slot": "home-pane", "id": "mem",
             "payload": {"title": "memory", "blocks": [
                 {"kind": "sparkline", "values": [0, 250, 500, 750, 1000], "max": 1000,
                  "tone": "warn", "caption": "64% 22.7/32G"},
                 {"kind": "row", "label": "agents", "value": "7"}
             ]}}
        ]));
        let lines = home_pane_lines(&snap, &Theme::default());
        let t = texts(&lines);
        assert_eq!(t[0], "memory", "heading falls back to the pane title");
        // Five values across 0..max, each frac rounded onto the 8-glyph ramp:
        // 0, .25*7=1.75->2, .5*7=3.5->4, .75*7=5.25->5, full->7.
        assert_eq!(t[1], "▁▃▅▆█");
        assert_eq!(t[2], "64% 22.7/32G");
        assert!(t.iter().any(|l| l.contains("agents") && l.contains("7")));
    }

    #[test]
    fn sparkline_block_maps_values_onto_the_glyph_ramp() {
        let theme = Theme::default();
        // Empty / missing series renders nothing.
        assert!(sparkline_lines(&json!({"kind": "sparkline", "values": []}), 0, &theme).is_empty());
        assert!(sparkline_lines(&json!({"kind": "sparkline"}), 0, &theme).is_empty());
        // Without an explicit max, the data's own max pins the top glyph
        // (4/8=.5 -> .5*7=3.5 -> round 4 -> the 5th ramp glyph).
        let lines = sparkline_lines(
            &json!({"kind": "sparkline", "values": [0.0, 4.0, 8.0]}),
            0,
            &theme,
        );
        assert_eq!(texts(&lines), vec!["▁▅█"]);
        // Values above max clamp to full rather than overflowing the ramp
        // (50/100=.5 -> 4th index; 200 clamps to full).
        let capped = sparkline_lines(
            &json!({"kind": "sparkline", "values": [50, 200], "max": 100}),
            0,
            &theme,
        );
        assert_eq!(texts(&capped), vec!["▅█"]);
    }

    #[test]
    fn sparkline_bands_color_each_glyph_by_the_threshold_it_reaches() {
        let theme = Theme::default();
        // Values climb across two thresholds; each glyph takes the highest band
        // it reaches (10 -> none/base, 70 -> warn, 95 -> danger).
        let lines = sparkline_lines(
            &json!({"kind": "sparkline", "values": [10, 70, 95], "max": 100,
                    "bands": [{"at": 70, "tone": "warn"}, {"at": 90, "tone": "danger"}]}),
            0,
            &theme,
        );
        let spans = &lines[0].spans;
        assert_eq!(spans.len(), 3, "one span per sample for per-glyph coloring");
        assert_eq!(
            spans[0].style.fg,
            tone_style(None, &theme).fg,
            "below all bands: base tone"
        );
        assert_eq!(spans[1].style.fg, tone_style(Some(Tone::Warn), &theme).fg);
        assert_eq!(spans[2].style.fg, tone_style(Some(Tone::Danger), &theme).fg);
        // Bands are optional; malformed/missing falls back to the single tone.
        assert!(parse_bands(&json!({"kind": "sparkline", "values": [1]})).is_empty());
        assert!(band_tone(&[], 5.0).is_none());
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
