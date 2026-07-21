//! Command palette dialog: fuzzy-searchable list of named TUI actions.
//!
//! Mirrors the web UI's `CommandPalette` (web/src/components/command-palette/).
//! Activated with Ctrl+K. Built-in entries are generated from the shared
//! keybinding registry and carry an [`ActionId`] that `HomeView::run_action`
//! executes directly (so the palette is additive, not a parallel command
//! implementation, and can't drift from the keyboard). Dynamic session/group
//! entries use a "jump to cursor" payload instead.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::*;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;
use unicode_width::UnicodeWidthStr;

use super::DialogResult;
use crate::tui::components::set_prefixed_input_cursor_position;
use crate::tui::home::bindings::{self, ActionId};
use crate::tui::styles::Theme;

/// Group buckets, rendered in this order. Mirrors `web/src/components/command-palette/groups.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteGroup {
    Actions,
    Views,
    Settings,
    Sessions,
    Groups,
}

impl PaletteGroup {
    fn label(&self) -> &'static str {
        match self {
            PaletteGroup::Actions => "Actions",
            PaletteGroup::Views => "Views",
            PaletteGroup::Settings => "Settings",
            PaletteGroup::Sessions => "Sessions",
            PaletteGroup::Groups => "Groups",
        }
    }

    fn order(&self) -> u8 {
        match self {
            PaletteGroup::Actions => 0,
            PaletteGroup::Views => 1,
            PaletteGroup::Settings => 2,
            PaletteGroup::Sessions => 3,
            PaletteGroup::Groups => 4,
        }
    }
}

/// What the dialog asks the input handler to do when the user picks an entry.
pub enum PaletteAction {
    /// Run a registry action directly via `HomeView::run_action`. This is the
    /// canonical path: it never synthesizes a keypress, so it can't misfire in
    /// strict mode (where a synthesized bare letter would hit the typing-guard
    /// or a relocated arm).
    Invoke(ActionId),
    /// Activate the selected session (the `Enter` action; not a relocatable
    /// keybinding, so it's not in the registry).
    Activate,
    /// Enter live-send mode on the selected session (the `Tab` action; likewise
    /// not a relocatable keybinding).
    LiveSend,
    /// Move the cursor to a position in `flat_items` (used for session/group jump items).
    JumpToCursor(usize),
    /// Open a tool session by name (lazygit, yazi, etc.)
    ToolSession(String),
    /// The query matched an Age of Empires cheat code; show its message as a
    /// transient toast. The payload is the message to display.
    Cheat(String),
}

/// One entry in the palette. `payload` is what gets returned when the user picks it.
pub struct PaletteCommand {
    pub id: &'static str,
    pub title: String,
    pub group: PaletteGroup,
    pub keywords: Vec<&'static str>,
    /// Human-readable hotkey shown on the right (e.g. "n", "Ctrl+D"). Empty if no binding.
    pub hotkey: String,
    pub payload: PaletteAction,
}

/// Built-in named commands, generated from the shared keybinding registry so
/// the palette's hotkey labels and dispatched actions can never drift from the
/// keyboard dispatcher. Pure-navigation keys (j/k, arrows, h/l) are excluded.
/// `Enter` (attach) and `Tab` (live-send) aren't relocatable keybindings, so
/// they're appended explicitly rather than pulled from the registry.
pub fn builtin_commands(serve_enabled: bool, strict_hotkeys: bool) -> Vec<PaletteCommand> {
    let mut cmds: Vec<PaletteCommand> = bindings::BINDINGS
        .iter()
        .filter_map(|b| {
            let meta = b.palette.as_ref()?;
            if meta.serve_only && !serve_enabled {
                return None;
            }
            // Drop the unread toggle entirely when the feature is off, so the
            // palette can't invoke a removed binding.
            if b.id == bindings::ActionId::ToggleUnread && !crate::session::unread_enabled() {
                return None;
            }
            Some(PaletteCommand {
                id: bindings::palette_id(b.id),
                title: meta.title.to_string(),
                group: meta.group,
                keywords: meta.keywords.to_vec(),
                hotkey: bindings::label(b.id, strict_hotkeys),
                payload: PaletteAction::Invoke(b.id),
            })
        })
        .collect();

    cmds.push(PaletteCommand {
        id: "attach",
        title: "Attach to selected session".to_string(),
        group: PaletteGroup::Actions,
        keywords: vec!["open", "enter"],
        hotkey: "Enter".to_string(),
        payload: PaletteAction::Activate,
    });
    cmds.push(PaletteCommand {
        id: "live-send",
        title: "Live send: pass keys straight to the agent".to_string(),
        group: PaletteGroup::Actions,
        keywords: vec![
            "live",
            "passthrough",
            "attach",
            "keys",
            "escape",
            "arrow",
            "tab",
            "interrupt",
        ],
        hotkey: "Tab".to_string(),
        payload: PaletteAction::LiveSend,
    });

    cmds
}

pub struct CommandPaletteDialog {
    input: Input,
    /// All entries (built-ins + dynamic session/group jumps), in display order.
    entries: Vec<PaletteCommand>,
    /// Indices into `entries` matching the current query, in score order.
    matches: Vec<usize>,
    /// Cursor within `matches`.
    selected: usize,
    /// Captured by `render`: the screen row of each visible (non-header)
    /// item along with its `matches` index. Drives click + hover routing
    /// without having to re-derive the scroll math.
    visible_item_rows: Vec<(u16, usize)>,
    /// Rect of the rendered dialog frame. Used by click routing to
    /// distinguish "inside dialog but missed a row" (no-op) from
    /// "outside dialog" (cancel).
    dialog_area: Rect,
}

impl CommandPaletteDialog {
    pub fn new(entries: Vec<PaletteCommand>) -> Self {
        let mut dialog = Self {
            input: Input::default(),
            entries,
            matches: Vec::new(),
            selected: 0,
            visible_item_rows: Vec::new(),
            dialog_area: Rect::default(),
        };
        dialog.recompute_matches();
        dialog
    }

    pub fn handle_click(&mut self, col: u16, row: u16) -> DialogResult<PaletteAction> {
        if !self
            .dialog_area
            .contains(ratatui::layout::Position::from((col, row)))
        {
            return DialogResult::Cancel;
        }
        // Hit-test the visible item rows.
        let Some(display_idx) = self
            .visible_item_rows
            .iter()
            .find(|(r, _)| *r == row)
            .map(|(_, idx)| *idx)
        else {
            return DialogResult::Continue;
        };
        self.selected = display_idx;
        let Some(&entry_idx) = self.matches.get(self.selected) else {
            return DialogResult::Continue;
        };
        let cmd = self.entries.swap_remove(entry_idx);
        DialogResult::Submit(cmd.payload)
    }

    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        if col == 0 && row == 0 {
            return false;
        }
        let Some(display_idx) = self
            .visible_item_rows
            .iter()
            .find(|(r, _)| *r == row)
            .map(|(_, idx)| *idx)
        else {
            return false;
        };
        if self.selected == display_idx {
            return false;
        }
        self.selected = display_idx;
        true
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<PaletteAction> {
        // Ctrl+K toggles the palette: if the user re-presses the activation
        // key, close it (matches VS Code / cmdk behavior). Without this branch
        // the wildcard arm would forward Ctrl+K to tui_input, which silently
        // discards it, leaving the palette stuck open until Esc.
        if matches!(key.code, KeyCode::Char('k') | KeyCode::Char('K'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            return DialogResult::Cancel;
        }
        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
                DialogResult::Continue
            }
            KeyCode::Down => {
                if !self.matches.is_empty() && self.selected + 1 < self.matches.len() {
                    self.selected += 1;
                }
                DialogResult::Continue
            }
            KeyCode::Enter => {
                let Some(&idx) = self.matches.get(self.selected) else {
                    return DialogResult::Cancel;
                };
                // Move the chosen entry out so we can return its payload by value.
                let cmd = self.entries.swap_remove(idx);
                DialogResult::Submit(cmd.payload)
            }
            _ => {
                self.input.handle_event(&crossterm::event::Event::Key(key));
                self.recompute_matches();
                // A full-string match on a known cheat code fires its toast and
                // closes the palette, mirroring the web easter egg.
                if let Some(message) = super::cheats::match_cheat(self.input.value()) {
                    return DialogResult::Submit(PaletteAction::Cheat(message.to_string()));
                }
                DialogResult::Continue
            }
        }
    }

    fn recompute_matches(&mut self) {
        use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
        use nucleo_matcher::{Config, Matcher, Utf32Str};

        let query = self.input.value().trim();
        if query.is_empty() {
            // No query: show everything in the original (group, insertion) order.
            self.matches = sort_indices_by_group(&self.entries);
            self.selected = 0;
            return;
        }

        let mut matcher = Matcher::new(Config::DEFAULT);
        let atom = Atom::new(
            query,
            CaseMatching::Ignore,
            Normalization::Smart,
            AtomKind::Fuzzy,
            false,
        );

        let mut scored: Vec<(usize, u16)> = Vec::new();
        let mut buf = Vec::new();
        for (idx, cmd) in self.entries.iter().enumerate() {
            let mut haystack = cmd.title.clone();
            for kw in &cmd.keywords {
                haystack.push(' ');
                haystack.push_str(kw);
            }
            let h = Utf32Str::new(&haystack, &mut buf);
            if let Some(score) = atom.score(h, &mut matcher) {
                scored.push((idx, score));
            }
        }
        scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        self.matches = scored.into_iter().map(|(idx, _)| idx).collect();
        self.selected = 0;
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        self.visible_item_rows.clear();
        let dialog_width: u16 = area.width.saturating_sub(8).clamp(40, 70);
        let dialog_height: u16 = area.height.saturating_sub(6).clamp(10, 20);
        let dialog_area = super::centered_rect(area, dialog_width, dialog_height);
        self.dialog_area = dialog_area;

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .style(Style::default().bg(theme.background))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(Line::styled(
                " Commands ",
                Style::default().fg(theme.title).bold(),
            ));

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(1), // input
                Constraint::Length(1), // separator
                Constraint::Min(1),    // list
                Constraint::Length(1), // hint
            ])
            .split(inner);

        // Input row
        let input_line = Line::from(vec![
            Span::styled("> ", Style::default().fg(theme.accent).bold()),
            Span::styled(self.input.value(), Style::default().fg(theme.text)),
            Span::styled("_", Style::default().fg(theme.accent)),
        ]);
        frame.render_widget(Paragraph::new(input_line), chunks[0]);
        set_prefixed_input_cursor_position(frame, chunks[0], "> ", &self.input);

        // Separator
        let sep = "─".repeat(chunks[1].width as usize);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                sep,
                Style::default().fg(theme.dimmed),
            ))),
            chunks[1],
        );

        // List
        let list_area = chunks[2];
        let visible = list_area.height as usize;

        let mut lines: Vec<Line> = Vec::new();
        // Parallel to `lines`: None for a group-header line, Some(idx)
        // for an item line where idx is the `matches` index.
        let mut line_to_display_idx: Vec<Option<usize>> = Vec::new();
        let mut selected_line: usize = 0;
        if self.matches.is_empty() {
            lines.push(Line::from(Span::styled(
                "  No matches",
                Style::default().fg(theme.dimmed),
            )));
            line_to_display_idx.push(None);
        } else {
            let mut last_group: Option<PaletteGroup> = None;
            for (display_idx, &entry_idx) in self.matches.iter().enumerate() {
                let cmd = &self.entries[entry_idx];

                // Show group header on transition (only when no query, since
                // fuzzy results mix groups by score and headers would be confusing).
                let show_headers = self.input.value().trim().is_empty();
                if show_headers && last_group != Some(cmd.group) {
                    lines.push(Line::from(Span::styled(
                        cmd.group.label(),
                        Style::default().fg(theme.accent).bold(),
                    )));
                    line_to_display_idx.push(None);
                    last_group = Some(cmd.group);
                }

                let is_selected = display_idx == self.selected;
                if is_selected {
                    selected_line = lines.len();
                }
                let prefix = if is_selected { "▶ " } else { "  " };
                let title_style = if is_selected {
                    Style::default().fg(theme.title).bold()
                } else {
                    Style::default().fg(theme.text)
                };
                let row_width = list_area.width as usize;
                let hotkey_width = if cmd.hotkey.is_empty() {
                    0
                } else {
                    cmd.hotkey.width() + 2
                };
                let title_max = row_width
                    .saturating_sub(prefix.width())
                    .saturating_sub(hotkey_width);
                let truncated_title = truncate_with_ellipsis(&cmd.title, title_max);
                let title_width = truncated_title.width();
                let pad_len = row_width
                    .saturating_sub(prefix.width())
                    .saturating_sub(title_width)
                    .saturating_sub(hotkey_width);
                let padding = " ".repeat(pad_len);
                let mut spans = vec![
                    Span::styled(prefix, title_style),
                    Span::styled(truncated_title, title_style),
                    Span::raw(padding),
                ];
                if !cmd.hotkey.is_empty() {
                    spans.push(Span::styled(
                        cmd.hotkey.clone(),
                        Style::default().fg(theme.hint),
                    ));
                }
                lines.push(Line::from(spans));
                line_to_display_idx.push(Some(display_idx));
            }
        }
        let start = selected_line.saturating_sub(visible.saturating_sub(1));
        let end = (start + visible).min(lines.len());
        // Capture screen rows for visible item lines so a click can map
        // directly back to the `matches` display index.
        for (i, line_idx) in (start..end).enumerate() {
            if let Some(idx) = line_to_display_idx.get(line_idx).copied().flatten() {
                self.visible_item_rows.push((list_area.y + i as u16, idx));
            }
        }
        frame.render_widget(Paragraph::new(lines[start..end].to_vec()), list_area);

        // Hint footer
        let footer_left = Line::from(vec![
            Span::styled("↑↓", Style::default().fg(theme.hint)),
            Span::raw(" navigate  "),
            Span::styled("Enter", Style::default().fg(theme.hint)),
            Span::raw(" run  "),
            Span::styled("Esc", Style::default().fg(theme.hint)),
            Span::raw(" close"),
        ]);
        frame.render_widget(Paragraph::new(footer_left), chunks[3]);
    }
}

/// Truncate a string to fit within `max_cols` terminal columns, appending "…"
/// if cut. Uses Unicode display width (so a wide char like an emoji counts as
/// 2 cells), and only ever cuts on char boundaries so this can't panic on
/// session titles with multi-byte characters.
fn truncate_with_ellipsis(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    if max_cols == 1 {
        // Not enough room for ellipsis + content; return original and let the
        // surrounding layout truncate at the column boundary.
        return s.to_string();
    }
    if s.width() <= max_cols {
        return s.to_string();
    }
    // Reserve 1 cell for the ellipsis, then walk char-by-char until adding
    // the next char would exceed the budget. Tracking width per char avoids
    // mid-grapheme byte-slicing.
    let budget = max_cols - 1;
    let mut used = 0usize;
    let mut cut_byte = 0usize;
    for (i, ch) in s.char_indices() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > budget {
            break;
        }
        used += w;
        cut_byte = i + ch.len_utf8();
    }
    format!("{}…", &s[..cut_byte])
}

/// Stable sort: primary by group order, secondary by original insertion order.
/// Used when no query is active so the palette has a predictable layout.
fn sort_indices_by_group(entries: &[PaletteCommand]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..entries.len()).collect();
    idx.sort_by_key(|&i| (entries[i].group.order(), i));
    idx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use std::collections::HashSet;

    fn ke(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn make_dialog() -> CommandPaletteDialog {
        CommandPaletteDialog::new(builtin_commands(false, false))
    }

    #[test]
    fn empty_query_shows_all_entries_grouped() {
        let dialog = make_dialog();
        assert_eq!(dialog.matches.len(), dialog.entries.len());
        // First match should be in the Actions group (lowest order).
        let first = &dialog.entries[dialog.matches[0]];
        assert_eq!(first.group, PaletteGroup::Actions);
    }

    #[test]
    fn fuzzy_filters_to_matching_entries() {
        let mut dialog = make_dialog();
        dialog.handle_key(ke(KeyCode::Char('r')));
        dialog.handle_key(ke(KeyCode::Char('e')));
        dialog.handle_key(ke(KeyCode::Char('n')));
        // "ren" should match "Rename or move to group" near the top.
        let top = &dialog.entries[dialog.matches[0]];
        assert!(
            top.title.to_lowercase().contains("rename"),
            "got: {}",
            top.title
        );
    }

    #[test]
    fn live_send_entry_is_bound_to_tab_with_dedicated_payload() {
        // Regression guard: the live-send palette entry must keep its
        // hotkey label and dedicated payload variant. A future rebinding
        // (e.g., moving Tab elsewhere) or accidentally regressing the
        // payload to `Key(Tab)` would break strict-mode users who reach
        // live-send only through the palette.
        let cmds = builtin_commands(false, false);
        let entry = cmds
            .iter()
            .find(|c| c.id == "live-send")
            .expect("builtin commands must include 'live-send'");
        assert_eq!(entry.hotkey, "Tab");
        assert!(
            matches!(&entry.payload, PaletteAction::LiveSend),
            "live-send entry must dispatch PaletteAction::LiveSend"
        );
    }

    #[test]
    fn picker_entries_invoke_their_actions() {
        // The sort and group picker palette entries route through
        // `Invoke(ActionId::…)` so `run_action` opens the picker directly.
        // Previously these synthesized `Key('o')` / `Key('g')`, which the
        // strict-mode typing-guard would have swallowed.
        let cmds = builtin_commands(false, true);
        let sort = cmds
            .iter()
            .find(|c| c.id == "pick-sort")
            .expect("builtin commands must include 'pick-sort'");
        assert!(
            matches!(&sort.payload, PaletteAction::Invoke(ActionId::SortPicker)),
            "sort-picker entry must Invoke(SortPicker)"
        );
        let group = cmds
            .iter()
            .find(|c| c.id == "pick-group-by")
            .expect("builtin commands must include 'pick-group-by'");
        assert!(
            matches!(&group.payload, PaletteAction::Invoke(ActionId::GroupBy)),
            "group-picker entry must Invoke(GroupBy)"
        );
    }

    #[test]
    fn keywords_match_searches() {
        // "Move session to group" complaint from issue #889: searching for
        // "move" should surface the rename entry via its keyword.
        let mut dialog = make_dialog();
        for c in "move".chars() {
            dialog.handle_key(ke(KeyCode::Char(c)));
        }
        assert!(!dialog.matches.is_empty(), "'move' should match something");
        let top = &dialog.entries[dialog.matches[0]];
        assert_eq!(top.id, "rename");
    }

    #[test]
    fn enter_submits_payload() {
        let mut dialog = make_dialog();
        // Filter to a known entry so we control which payload comes back.
        for c in "settings".chars() {
            dialog.handle_key(ke(KeyCode::Char(c)));
        }
        let result = dialog.handle_key(ke(KeyCode::Enter));
        match result {
            DialogResult::Submit(PaletteAction::Invoke(id)) => {
                assert_eq!(id, ActionId::Settings);
            }
            _ => panic!("expected Submit(Invoke(Settings))"),
        }
    }

    #[test]
    fn esc_cancels() {
        let mut dialog = make_dialog();
        let result = dialog.handle_key(ke(KeyCode::Esc));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn ctrl_k_toggles_closed() {
        let mut dialog = make_dialog();
        let ctrl_k = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert!(matches!(dialog.handle_key(ctrl_k), DialogResult::Cancel));
        // Same with uppercase K (some terminals send Ctrl+Shift+K as `K`).
        let ctrl_shift_k = KeyEvent::new(KeyCode::Char('K'), KeyModifiers::CONTROL);
        assert!(matches!(
            dialog.handle_key(ctrl_shift_k),
            DialogResult::Cancel
        ));
    }

    #[test]
    fn navigation_clamps() {
        let mut dialog = make_dialog();
        // Up at top stays at 0.
        dialog.handle_key(ke(KeyCode::Up));
        assert_eq!(dialog.selected, 0);

        // Walk to the bottom, then Down should clamp.
        let len = dialog.matches.len();
        for _ in 0..len + 5 {
            dialog.handle_key(ke(KeyCode::Down));
        }
        assert_eq!(dialog.selected, len - 1);
    }

    #[test]
    fn no_match_query_shows_empty() {
        let mut dialog = make_dialog();
        for c in "zzzqxqxq".chars() {
            dialog.handle_key(ke(KeyCode::Char(c)));
        }
        assert!(dialog.matches.is_empty());
        // Enter on empty result should cancel rather than panic.
        let result = dialog.handle_key(ke(KeyCode::Enter));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn serve_command_only_with_feature() {
        let with = builtin_commands(true, false);
        let without = builtin_commands(false, false);
        assert!(with.iter().any(|c| c.id == "serve"));
        assert!(!without.iter().any(|c| c.id == "serve"));
    }

    #[test]
    fn hotkey_labels_follow_strict_mode() {
        // Picks one entry whose label moves under strict mode and one whose
        // binding gets relocated to Ctrl. Catches regressions where strict
        // mode was forgotten when adding a new entry.
        let normal = builtin_commands(false, false);
        let strict = builtin_commands(false, true);

        let new_normal = normal.iter().find(|c| c.id == "new-session").unwrap();
        let new_strict = strict.iter().find(|c| c.id == "new-session").unwrap();
        assert_eq!(new_normal.hotkey, "n");
        assert_eq!(new_strict.hotkey, "N");

        let diff_normal = normal.iter().find(|c| c.id == "diff").unwrap();
        let diff_strict = strict.iter().find(|c| c.id == "diff").unwrap();
        assert_eq!(diff_normal.hotkey, "D");
        assert_eq!(diff_strict.hotkey, "Ctrl+D");

        // Bindings without a strict variant (Enter, w, ?, P) stay the same.
        let attach_normal = normal.iter().find(|c| c.id == "attach").unwrap();
        let attach_strict = strict.iter().find(|c| c.id == "attach").unwrap();
        assert_eq!(attach_normal.hotkey, "Enter");
        assert_eq!(attach_strict.hotkey, "Enter");
    }

    #[test]
    fn typing_a_cheat_code_submits_a_cheat_payload() {
        let mut dialog = make_dialog();
        // Intermediate prefixes are not a full match and keep the palette open.
        for c in "wolol".chars() {
            assert!(matches!(
                dialog.handle_key(ke(KeyCode::Char(c))),
                DialogResult::Continue
            ));
        }
        // Completing the code fires the cheat and closes the palette.
        match dialog.handle_key(ke(KeyCode::Char('o'))) {
            DialogResult::Submit(PaletteAction::Cheat(message)) => {
                assert!(message.contains("converts to your cause"), "got: {message}");
            }
            _ => panic!("expected Submit(Cheat) after typing a full cheat code"),
        }
    }

    #[test]
    fn ordinary_query_does_not_trigger_a_cheat() {
        let mut dialog = make_dialog();
        for c in "settings".chars() {
            assert!(matches!(
                dialog.handle_key(ke(KeyCode::Char(c))),
                DialogResult::Continue
            ));
        }
    }

    #[test]
    fn jump_to_cursor_payload_round_trips() {
        // Build a custom palette with one dynamic jump entry so we can
        // exercise the JumpToCursor path the same way real session items do.
        let entries = vec![PaletteCommand {
            id: "jump-test",
            title: "Jump to my-session".to_string(),
            group: PaletteGroup::Sessions,
            keywords: vec!["session"],
            hotkey: String::new(),
            payload: PaletteAction::JumpToCursor(7),
        }];
        let mut dialog = CommandPaletteDialog::new(entries);
        let result = dialog.handle_key(ke(KeyCode::Enter));
        match result {
            DialogResult::Submit(PaletteAction::JumpToCursor(idx)) => assert_eq!(idx, 7),
            _ => panic!("expected JumpToCursor"),
        }
    }

    /// Registry actions that intentionally have no palette command. Anything
    /// not listed here must carry palette metadata in the registry; the palette
    /// is generated from that metadata, so this is the one place "added an
    /// action, forgot the palette" can still slip through.
    const PALETTE_EXEMPT: &[(ActionId, &str)] = &[
        (
            ActionId::Quit,
            "intentionally excluded from the palette; q is quick-exit, doesn't need discovery",
        ),
        (
            ActionId::ToolPicker,
            "Tool-view toggle; tool sessions get dynamic palette entries instead",
        ),
        (
            ActionId::SearchStart,
            "search activation; modal trigger, not an action",
        ),
        (
            ActionId::Update,
            "update-banner action; meta key, surfaced via the banner",
        ),
        (
            ActionId::ToggleContainer,
            "only valid on a sandboxed session in Terminal view",
        ),
        (
            ActionId::SearchNext,
            "search-cycle; only meaningful while a search is active",
        ),
        (
            ActionId::ToggleProjectPin,
            "only valid on a project header in project view; reached via the header context menu and `p`",
        ),
    ];

    /// Drift guard. The palette is generated from `bindings::BINDINGS`, so a
    /// binding either exposes palette metadata (and thus a command) or is on
    /// the exempt list. Catches "added an action, forgot to decide whether it
    /// belongs in the palette."
    #[test]
    fn registry_actions_have_palette_or_are_exempt() {
        let exempt: HashSet<ActionId> = PALETTE_EXEMPT.iter().map(|(id, _)| *id).collect();
        let missing: Vec<ActionId> = bindings::BINDINGS
            .iter()
            .filter(|b| b.palette.is_none() && !exempt.contains(&b.id))
            .map(|b| b.id)
            .collect();
        assert!(
            missing.is_empty(),
            "registry actions with no palette metadata and not in PALETTE_EXEMPT: {:?}\n\
             Add a PaletteMeta to the binding in home/bindings.rs, or add the action to \
             PALETTE_EXEMPT here with a note.",
            missing
        );
    }

    /// Reverse drift: an exempt action that no longer exists or that gained
    /// palette metadata (so the exemption is now stale/contradictory).
    #[test]
    fn palette_exempt_entries_are_still_exempt() {
        let stale: Vec<ActionId> = PALETTE_EXEMPT
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| {
                bindings::BINDINGS
                    .iter()
                    .find(|b| b.id == *id)
                    .is_none_or(|b| b.palette.is_some())
            })
            .collect();
        assert!(
            stale.is_empty(),
            "PALETTE_EXEMPT lists actions that no longer exist or now have palette \
             metadata: {:?}. Remove them from PALETTE_EXEMPT.",
            stale
        );
    }

    #[test]
    fn truncate_handles_multibyte_chars() {
        // Naive byte slicing would panic mid-emoji; this exercises the
        // dynamic "Jump to session: 😀 my-session" rendering path.
        let s = "😀 my-session-with-a-long-title";
        // No-op when string already fits.
        assert_eq!(truncate_with_ellipsis(s, 100), s);
        // Truncation lands on a char boundary and appends ellipsis.
        let out = truncate_with_ellipsis(s, 5);
        assert!(out.ends_with('…'), "got {out:?}");
        assert!(out.width() <= 5);
        // Tiny budget returns the original to avoid producing useless "…".
        assert_eq!(truncate_with_ellipsis(s, 1), s);
        // Pure ASCII still works.
        assert_eq!(truncate_with_ellipsis("hello world", 7), "hello …");

        // Cut budget that lands mid-emoji under any naive char/byte impl:
        // "ab😀cd" is bytes [a, b, 0xF0,0x9F,0x98,0x80, c, d] and the emoji
        // takes 2 display cols. With max_cols=3, budget=2 cells, the function
        // must keep "ab" + ellipsis (the emoji would push to 4 cells).
        let cut = truncate_with_ellipsis("ab😀cd", 3);
        assert_eq!(cut, "ab…");
        assert!(cut.width() <= 3);

        // Max_cols=4 leaves no room for the emoji either (a=1 + b=1 + 😀=2
        // = 4, but we need 1 cell reserved for ellipsis -> budget=3, and
        // a+b+😀 = 4 overflows it). Should still keep "ab".
        let cut = truncate_with_ellipsis("ab😀cd", 4);
        assert_eq!(cut, "ab…");

        // CJK width: each char is 2 cells. With max_cols=5 (budget=4) we
        // keep two CJK chars + ellipsis.
        let cut = truncate_with_ellipsis("中文测试abc", 5);
        assert_eq!(cut, "中文…");
        assert!(cut.width() <= 5);

        // Zero-budget returns empty (the surrounding layout will allocate no
        // visible cells). Previously this returned the full string and
        // overflowed the row.
        assert_eq!(truncate_with_ellipsis("anything", 0), "");
    }
}
