//! Command palette: a fuzzy-searchable list of named TUI actions, the twin of
//! the web `CommandPalette`. Built-in entries come from the shared keybinding
//! registry and carry an [`ActionId`] that `HomeView::run_action` executes, so
//! the palette cannot drift from the keyboard. Session and group entries carry
//! a "jump to cursor" payload instead.

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

/// Group buckets, in render order. Twin of the web `groups.ts`.
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
    /// Run a registry action directly. The canonical path: it synthesizes no
    /// keypress, so strict mode's typing-guard cannot misfire on it.
    Invoke(ActionId),
    /// Activate the selected session. `Enter` is not relocatable, so it is
    /// not in the registry.
    Activate,
    /// Enter live-send mode, `Tab` being likewise not relocatable.
    LiveSend,
    /// Move the cursor to a position in `flat_items`.
    JumpToCursor(usize),
    ToolSession(String),
    /// The query matched an Age of Empires cheat code; toast its message.
    Cheat(String),
}

/// One palette entry; `payload` is returned when it is picked.
pub struct PaletteCommand {
    pub id: &'static str,
    pub title: String,
    pub group: PaletteGroup,
    pub keywords: Vec<&'static str>,
    /// Hotkey shown on the right, empty when unbound.
    pub hotkey: String,
    pub payload: PaletteAction,
}

/// Built-in commands from the shared keybinding registry, so labels and
/// actions cannot drift from the dispatcher. Pure-navigation keys are
/// excluded; `Enter` and `Tab` are appended, not being relocatable.
pub fn builtin_commands(strict_hotkeys: bool) -> Vec<PaletteCommand> {
    let mut cmds: Vec<PaletteCommand> = bindings::BINDINGS
        .iter()
        .filter_map(|b| {
            let meta = b.palette.as_ref()?;
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
    entries: Vec<PaletteCommand>,
    matches: Vec<usize>,
    selected: usize,
    /// Screen row and `matches` index per visible item, captured by `render`
    /// so click and hover need no scroll math.
    visible_item_rows: Vec<(u16, usize)>,
    /// The dialog frame, so a click inside it that misses a row is a no-op
    /// rather than a cancel.
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
        // Ctrl+K closes the palette again. Without this the wildcard arm
        // forwards it to tui_input, which drops it, stranding the palette.
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
                let cmd = self.entries.swap_remove(idx);
                DialogResult::Submit(cmd.payload)
            }
            _ => {
                self.input.handle_event(&crossterm::event::Event::Key(key));
                self.recompute_matches();
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
        let block = Block::default()
            .style(Style::default().bg(theme.background))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(Line::styled(
                " Commands ",
                Style::default().fg(theme.title).bold(),
            ));
        let (dialog_area, inner) =
            super::render_dialog_frame(frame, area, dialog_width, dialog_height, block);
        self.dialog_area = dialog_area;

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

        let input_line = Line::from(vec![
            Span::styled("> ", Style::default().fg(theme.accent).bold()),
            Span::styled(self.input.value(), Style::default().fg(theme.text)),
            Span::styled("_", Style::default().fg(theme.accent)),
        ]);
        frame.render_widget(Paragraph::new(input_line), chunks[0]);
        set_prefixed_input_cursor_position(frame, chunks[0], "> ", &self.input);

        let sep = "─".repeat(chunks[1].width as usize);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                sep,
                Style::default().fg(theme.dimmed),
            ))),
            chunks[1],
        );

        let list_area = chunks[2];
        let visible = list_area.height as usize;

        let mut lines: Vec<Line> = Vec::new();
        // Parallel to `lines`: the `matches` index, or None for a header.
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

                // Headers only without a query: fuzzy results mix groups.
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
        for (i, line_idx) in (start..end).enumerate() {
            if let Some(idx) = line_to_display_idx.get(line_idx).copied().flatten() {
                self.visible_item_rows.push((list_area.y + i as u16, idx));
            }
        }
        frame.render_widget(Paragraph::new(lines[start..end].to_vec()), list_area);

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

/// Truncate to `max_cols` terminal columns, appending "…" if cut. Counts
/// Unicode display width and cuts only on char boundaries.
fn truncate_with_ellipsis(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    if max_cols == 1 {
        // No room for ellipsis plus content; let the layout clip it.
        return s.to_string();
    }
    if s.width() <= max_cols {
        return s.to_string();
    }
    // Reserve a cell for the ellipsis, then walk char by char within budget.
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

/// Stable sort by group then insertion order, for the no-query layout.
fn sort_indices_by_group(entries: &[PaletteCommand]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..entries.len()).collect();
    idx.sort_by_key(|&i| (entries[i].group.order(), i));
    idx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::dialogs::test_keys::{ctrl_key, key};
    use std::collections::HashSet;

    fn make_dialog() -> CommandPaletteDialog {
        CommandPaletteDialog::new(builtin_commands(false))
    }

    fn type_query(dialog: &mut CommandPaletteDialog, text: &str) {
        for c in text.chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }
    }

    fn command<'a>(cmds: &'a [PaletteCommand], id: &str) -> &'a PaletteCommand {
        cmds.iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("builtin commands must include {id:?}"))
    }

    #[test]
    fn a_query_ranks_the_entry_it_names_first() {
        // No query lists everything, led by the lowest-order group.
        let dialog = make_dialog();
        assert_eq!(dialog.matches.len(), dialog.entries.len());
        assert_eq!(
            dialog.entries[dialog.matches[0]].group,
            PaletteGroup::Actions
        );

        // (query, id of the entry it should surface). "move" only matches the
        // rename entry through its keywords.
        for (query, id) in [("ren", "rename"), ("move", "rename")] {
            let mut dialog = make_dialog();
            type_query(&mut dialog, query);
            assert!(!dialog.matches.is_empty(), "{query} matched nothing");
            assert_eq!(dialog.entries[dialog.matches[0]].id, id, "{query}");
        }

        // A query that matches nothing leaves the list empty, and Enter on it
        // cancels rather than panicking.
        let mut dialog = make_dialog();
        type_query(&mut dialog, "zzzqxqxq");
        assert!(dialog.matches.is_empty());
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Enter)),
            DialogResult::Cancel
        ));
    }

    #[test]
    fn enter_submits_the_entry_payload_and_the_close_keys_cancel() {
        let mut dialog = make_dialog();
        type_query(&mut dialog, "settings");
        match dialog.handle_key(key(KeyCode::Enter)) {
            DialogResult::Submit(PaletteAction::Invoke(id)) => assert_eq!(id, ActionId::Settings),
            _ => panic!("expected Submit(Invoke(Settings))"),
        }

        // Esc closes, and so does Ctrl+K again, in either case the terminal
        // may send it in.
        let mut dialog = make_dialog();
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Esc)),
            DialogResult::Cancel
        ));
        for c in ['k', 'K'] {
            assert!(matches!(
                dialog.handle_key(ctrl_key(KeyCode::Char(c))),
                DialogResult::Cancel
            ));
        }

        // A dynamic session entry round-trips its cursor index.
        let entries = vec![PaletteCommand {
            id: "jump-test",
            title: "Jump to my-session".to_string(),
            group: PaletteGroup::Sessions,
            keywords: vec!["session"],
            hotkey: String::new(),
            payload: PaletteAction::JumpToCursor(7),
        }];
        match CommandPaletteDialog::new(entries).handle_key(key(KeyCode::Enter)) {
            DialogResult::Submit(PaletteAction::JumpToCursor(idx)) => assert_eq!(idx, 7),
            _ => panic!("expected JumpToCursor"),
        }
    }

    #[test]
    fn navigation_clamps_at_both_ends() {
        let mut dialog = make_dialog();
        dialog.handle_key(key(KeyCode::Up));
        assert_eq!(dialog.selected, 0);

        let len = dialog.matches.len();
        for _ in 0..len + 5 {
            dialog.handle_key(key(KeyCode::Down));
        }
        assert_eq!(dialog.selected, len - 1);
    }

    #[test]
    fn entries_keep_their_dedicated_payloads_and_strict_mode_labels() {
        let normal = builtin_commands(false);
        let strict = builtin_commands(true);

        // Live-send and the two pickers must dispatch their own payloads. A
        // synthesized keypress would be swallowed by strict mode's
        // typing-guard, which is the only way those users reach them.
        assert_eq!(command(&normal, "live-send").hotkey, "Tab");
        assert!(matches!(
            command(&normal, "live-send").payload,
            PaletteAction::LiveSend
        ));
        assert!(matches!(
            command(&strict, "pick-sort").payload,
            PaletteAction::Invoke(ActionId::SortPicker)
        ));
        assert!(matches!(
            command(&strict, "pick-group-by").payload,
            PaletteAction::Invoke(ActionId::GroupBy)
        ));

        // (id, normal label, strict label). A binding with no strict variant
        // keeps its label.
        for (id, want_normal, want_strict) in [
            ("new-session", "n", "N"),
            ("diff", "D", "Ctrl+D"),
            ("attach", "Enter", "Enter"),
        ] {
            assert_eq!(command(&normal, id).hotkey, want_normal, "{id}");
            assert_eq!(command(&strict, id).hotkey, want_strict, "{id}");
        }
    }

    #[test]
    fn a_full_cheat_code_fires_its_toast_and_nothing_else_does() {
        let mut dialog = make_dialog();
        // A prefix is not a match, so the palette stays open.
        for c in "wolol".chars() {
            assert!(matches!(
                dialog.handle_key(key(KeyCode::Char(c))),
                DialogResult::Continue
            ));
        }
        match dialog.handle_key(key(KeyCode::Char('o'))) {
            DialogResult::Submit(PaletteAction::Cheat(message)) => {
                assert!(message.contains("converts to your cause"), "got: {message}");
            }
            _ => panic!("expected Submit(Cheat) after a full cheat code"),
        }

        let mut dialog = make_dialog();
        for c in "settings".chars() {
            assert!(matches!(
                dialog.handle_key(key(KeyCode::Char(c))),
                DialogResult::Continue
            ));
        }
    }

    /// Registry actions that intentionally have no palette command.
    const PALETTE_EXEMPT: &[(ActionId, &str)] = &[
        (ActionId::Quit, "q is quick-exit and needs no discovery"),
        (
            ActionId::ToolPicker,
            "tool sessions get dynamic palette entries instead",
        ),
        (ActionId::SearchStart, "a modal trigger, not an action"),
        (ActionId::Update, "surfaced via the update banner"),
        (
            ActionId::ToggleContainer,
            "only valid on a sandboxed session in Terminal view",
        ),
        (
            ActionId::SearchNext,
            "only meaningful while a search is active",
        ),
        (
            ActionId::ToggleProjectPin,
            "only valid on a project header, reached via its context menu and `p`",
        ),
    ];

    /// Drift guard in both directions: the palette is generated from
    /// `bindings::BINDINGS`, so every binding either carries palette metadata
    /// or sits on the exempt list, and no exemption outlives its action.
    #[test]
    fn every_registry_action_is_in_the_palette_or_exempt() {
        let exempt: HashSet<ActionId> = PALETTE_EXEMPT.iter().map(|(id, _)| *id).collect();
        let missing: Vec<ActionId> = bindings::BINDINGS
            .iter()
            .filter(|b| b.palette.is_none() && !exempt.contains(&b.id))
            .map(|b| b.id)
            .collect();
        assert!(
            missing.is_empty(),
            "no palette metadata and not in PALETTE_EXEMPT: {missing:?}. Add a \
             PaletteMeta to the binding in home/bindings.rs, or list it here."
        );

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
             metadata: {stale:?}. Remove them."
        );
    }

    #[test]
    fn truncation_cuts_on_display_width_not_bytes() {
        // The dynamic "Jump to session: 😀 my-session" rows make naive byte
        // slicing a panic, and a wide char overflowing the row a real case.
        let emoji = "😀 my-session-with-a-long-title";
        // (input, max_cols, output)
        let cases: &[(&str, usize, &str)] = &[
            (emoji, 100, emoji),
            // A budget too small for even one char returns the original
            // rather than a useless lone ellipsis.
            (emoji, 1, emoji),
            ("hello world", 7, "hello …"),
            // "ab😀cd": the emoji takes two cells, so neither a 3- nor a
            // 4-column budget leaves room for it once the ellipsis is
            // reserved.
            ("ab😀cd", 3, "ab…"),
            ("ab😀cd", 4, "ab…"),
            // Each CJK char is two cells, so a 5-column budget fits two.
            ("中文测试abc", 5, "中文…"),
            // Zero columns renders nothing rather than overflowing the row.
            ("anything", 0, ""),
        ];
        for (input, max_cols, want) in cases {
            let out = truncate_with_ellipsis(input, *max_cols);
            assert_eq!(out, *want, "{input:?} at {max_cols}");
            if out != *input {
                assert!(out.width() <= *max_cols, "{out:?} overflows {max_cols}");
            }
        }

        let out = truncate_with_ellipsis(emoji, 5);
        assert!(out.ends_with('…'), "got {out:?}");
        assert!(out.width() <= 5);
    }
}
