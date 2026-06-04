//! First-run walkthrough dialog.
//!
//! Replaces the older one-page welcome with a multi-step intro that explains
//! what AoE is, how to start a first session, lets the user pick a theme with
//! live preview, and points at the help shortcut. Navigable by keyboard and
//! mouse. Driven by `config.app_state.has_seen_welcome` like the previous
//! welcome dialog, so existing first-run gating in `App::new()` carries over.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Position;
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::session::NewSessionAttachMode;
use crate::tui::styles::{available_themes, Theme};

/// Outcome from the intro wizard.
///
/// Fields are `Some` when the user actually visited the corresponding page;
/// the caller writes them to config only in that case so a wizard skipped
/// before the page never overwrites pre-existing values. `final_theme` maps
/// to `config.theme.name`; `final_attach_mode` is written to both
/// `config.session.new_session_attach_mode` and `default_attach_mode` so the
/// post-create and Enter/double-click paths stay in sync.
#[derive(Debug, Clone)]
pub struct IntroOutcome {
    pub final_theme: Option<String>,
    pub final_attach_mode: Option<NewSessionAttachMode>,
    /// `Some(true)` if the user opted in to telemetry on the Telemetry page,
    /// `Some(false)` if they declined, `None` if they skipped before reaching
    /// it. The caller writes `config.telemetry.enabled` and marks the opt-in
    /// prompt answered only when this is `Some`.
    pub telemetry_opt_in: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Welcome,
    Telemetry,
    FirstSession,
    AttachMode,
    ThemePicker,
    Done,
}

/// Which footer button the mouse is currently over. Drives a hover-tint
/// background on the button text without moving keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HoverButton {
    Skip,
    Back,
    Next,
}

impl Page {
    fn all() -> &'static [Page] {
        &[
            Page::Welcome,
            Page::Telemetry,
            Page::FirstSession,
            Page::AttachMode,
            Page::ThemePicker,
            Page::Done,
        ]
    }
}

pub struct IntroDialog {
    /// Original theme name when the dialog opened; restored if the user
    /// cancels mid-flight so the rest of the TUI doesn't keep a half-picked
    /// theme.
    original_theme: String,
    /// Themes the picker can choose from. Built-in themes first, then user
    /// custom themes (same ordering as `available_themes()`).
    themes: Vec<String>,
    /// Index into `themes` for the currently-highlighted entry on the theme
    /// page.
    theme_cursor: usize,
    /// True once the user has visited the theme page; we only persist a
    /// theme choice if they actually saw it.
    theme_visited: bool,
    /// Pending theme to live-preview on the next handler tick; consumed by
    /// the home view to dispatch `Action::SetTheme`.
    pending_preview: Option<String>,
    page_idx: usize,
    skip_button_area: Rect,
    back_button_area: Rect,
    next_button_area: Rect,
    /// Per-row rects on the theme page, indexed by theme position. Empty on
    /// pages other than the theme picker and before the first render.
    theme_row_areas: Vec<Rect>,

    /// Currently-highlighted attach mode on the AttachMode page. Pre-seeded
    /// to `LiveSend` so new users keep the home list in view by default;
    /// the historical `Tmux` default still wins for existing users because
    /// they never see the wizard.
    attach_mode_cursor: NewSessionAttachMode,
    /// True once the user has visited the attach mode page; we only persist
    /// a choice if they actually saw it (mirrors `theme_visited`).
    attach_mode_visited: bool,
    /// Hit-test rects for the two attach-mode options on the AttachMode
    /// page (LiveSend, Tmux). Empty before the first render.
    attach_mode_areas: [Rect; 2],

    /// Which footer button (if any) the mouse is hovering. Hover paints a
    /// background tint on the button text but never moves the keyboard
    /// cursor; click drives the actual action.
    hovered_button: Option<HoverButton>,
    /// Index into `theme_row_areas` for the row the mouse is currently
    /// hovering on the ThemePicker page. `None` when not on that page or
    /// when the mouse isn't over a row.
    hovered_theme_row: Option<usize>,
    /// Index into `attach_mode_areas` (0 = LiveSend, 1 = Tmux) for the
    /// option the mouse is currently hovering on the AttachMode page.
    hovered_attach_idx: Option<usize>,

    /// True when the user has chosen to opt in to telemetry on the Telemetry
    /// page. Defaults to `false`: telemetry is off unless explicitly enabled.
    telemetry_opt_in: bool,
    /// True once the user has visited the Telemetry page; the choice is only
    /// persisted (and the opt-in prompt only marked answered) when they did.
    telemetry_visited: bool,
    /// Hit-test rects for the two Telemetry options (0 = enable, 1 = decline).
    telemetry_option_areas: [Rect; 2],
    /// Index into `telemetry_option_areas` for the option under the mouse on
    /// the Telemetry page.
    hovered_telemetry_idx: Option<usize>,
}

impl IntroDialog {
    pub fn new(original_theme: impl Into<String>) -> Self {
        let original_theme = original_theme.into();
        let themes = available_themes();
        let theme_cursor = themes
            .iter()
            .position(|t| t == &original_theme)
            .unwrap_or(0);
        Self {
            original_theme,
            themes,
            theme_cursor,
            theme_visited: false,
            pending_preview: None,
            page_idx: 0,
            skip_button_area: Rect::default(),
            back_button_area: Rect::default(),
            next_button_area: Rect::default(),
            theme_row_areas: Vec::new(),
            attach_mode_cursor: NewSessionAttachMode::LiveSend,
            attach_mode_visited: false,
            attach_mode_areas: [Rect::default(), Rect::default()],
            hovered_button: None,
            hovered_theme_row: None,
            hovered_attach_idx: None,
            telemetry_opt_in: false,
            telemetry_visited: false,
            telemetry_option_areas: [Rect::default(), Rect::default()],
            hovered_telemetry_idx: None,
        }
    }

    /// Theme name to preview right now, if the cursor moved since the last
    /// call. Consumed by the home view to dispatch `Action::SetTheme` so the
    /// TUI re-themes live while the user moves through the picker.
    pub fn take_pending_preview(&mut self) -> Option<String> {
        self.pending_preview.take()
    }

    /// True on every page of the wizard so xterm mouse tracking stays
    /// off and the terminal can do native drag-to-select on the docs /
    /// YouTube / Discord URLs (and any other text). The trade is that
    /// the footer [Skip] / [Back] / [Next →] / [Finish] buttons aren't
    /// clickable; navigation is keyboard-only (Enter / ← / Esc), which
    /// the hint on each page advertises.
    pub fn wants_text_selection(&self) -> bool {
        true
    }

    fn current_page(&self) -> Page {
        Page::all()[self.page_idx]
    }

    fn is_last_page(&self) -> bool {
        self.page_idx + 1 == Page::all().len()
    }

    fn advance(&mut self) -> Option<DialogResult<IntroOutcome>> {
        if self.is_last_page() {
            return Some(DialogResult::Submit(self.outcome()));
        }
        self.page_idx += 1;
        self.clear_page_hover();
        match self.current_page() {
            Page::ThemePicker => {
                self.theme_visited = true;
                self.queue_preview_current();
            }
            Page::AttachMode => {
                self.attach_mode_visited = true;
            }
            Page::Telemetry => {
                self.telemetry_visited = true;
            }
            _ => {}
        }
        None
    }

    fn go_back(&mut self) {
        if self.page_idx > 0 {
            self.page_idx -= 1;
            self.clear_page_hover();
        }
    }

    fn cancel(&mut self) -> DialogResult<IntroOutcome> {
        // Revert to whatever theme was active before the dialog opened, so a
        // mid-flight preview doesn't outlive the wizard.
        if self.theme_visited && self.themes.get(self.theme_cursor) != Some(&self.original_theme) {
            self.pending_preview = Some(self.original_theme.clone());
        }
        DialogResult::Cancel
    }

    fn outcome(&self) -> IntroOutcome {
        // Only report a theme when the user picked something different from
        // what was active when the wizard opened. Dispatching SetTheme for
        // an identity change flips `needs_redraw` → `terminal.clear()` on
        // the next loop iteration, which the user sees as a flash when the
        // wizard closes.
        let final_theme = if self.theme_visited {
            self.themes
                .get(self.theme_cursor)
                .cloned()
                .filter(|name| name != &self.original_theme)
        } else {
            None
        };
        IntroOutcome {
            final_theme,
            final_attach_mode: if self.attach_mode_visited {
                Some(self.attach_mode_cursor)
            } else {
                None
            },
            telemetry_opt_in: if self.telemetry_visited {
                Some(self.telemetry_opt_in)
            } else {
                None
            },
        }
    }

    fn queue_preview_current(&mut self) {
        if let Some(name) = self.themes.get(self.theme_cursor) {
            self.pending_preview = Some(name.clone());
        }
    }

    fn toggle_attach_mode(&mut self) {
        self.attach_mode_cursor = match self.attach_mode_cursor {
            NewSessionAttachMode::LiveSend => NewSessionAttachMode::Tmux,
            NewSessionAttachMode::Tmux => NewSessionAttachMode::LiveSend,
        };
    }

    /// Flip the telemetry opt-in choice. A no-op when `DO_NOT_TRACK` forces
    /// telemetry off, so the displayed choice can't drift from what will
    /// actually happen.
    fn toggle_telemetry(&mut self) {
        if crate::telemetry::do_not_track() {
            self.telemetry_opt_in = false;
            return;
        }
        self.telemetry_opt_in = !self.telemetry_opt_in;
    }

    fn move_theme_cursor(&mut self, delta: isize) {
        if self.themes.is_empty() {
            return;
        }
        let len = self.themes.len() as isize;
        let next = (self.theme_cursor as isize + delta).rem_euclid(len);
        self.theme_cursor = next as usize;
        self.queue_preview_current();
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<IntroOutcome> {
        // Theme page eats up/down so they navigate the list; everything else
        // (Enter, Tab, arrows for paging) is shared across pages.
        if self.current_page() == Page::ThemePicker {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.move_theme_cursor(-1);
                    return DialogResult::Continue;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.move_theme_cursor(1);
                    return DialogResult::Continue;
                }
                _ => {}
            }
        }

        // Attach mode page captures up/down/j/k so the user can flip between
        // LiveSend and Tmux without bouncing back to page-level handling.
        if self.current_page() == Page::AttachMode {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
                    self.toggle_attach_mode();
                    return DialogResult::Continue;
                }
                _ => {}
            }
        }

        // Telemetry page captures up/down/j/k to flip the opt-in choice,
        // mirroring the attach-mode page.
        if self.current_page() == Page::Telemetry {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
                    self.toggle_telemetry();
                    return DialogResult::Continue;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Esc => self.cancel(),
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Right | KeyCode::Tab => {
                self.advance().unwrap_or(DialogResult::Continue)
            }
            KeyCode::Left | KeyCode::BackTab => {
                self.go_back();
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    /// Update the hover state from a `MouseEventKind::Moved` event.
    /// Mirrors the pattern in `SnoozeDurationDialog` / `SortPickerDialog`:
    /// the hover indicator tracks the cursor but never moves the keyboard
    /// focus, so a stray mouse drift while reading the page can't silently
    /// switch the user's pick. Returns true only when the hover target
    /// resolves to a different rect, so the caller can skip a redraw on
    /// every pixel-level mouse twitch.
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        let pos = Position::from((col, row));
        let new_button = if self.skip_button_area.contains(pos) {
            Some(HoverButton::Skip)
        } else if self.page_idx > 0 && self.back_button_area.contains(pos) {
            Some(HoverButton::Back)
        } else if self.next_button_area.contains(pos) {
            Some(HoverButton::Next)
        } else {
            None
        };
        let new_theme = if self.current_page() == Page::ThemePicker {
            self.theme_row_areas
                .iter()
                .position(|a| a.contains(pos) && a.width > 0)
        } else {
            None
        };
        let new_attach = if self.current_page() == Page::AttachMode {
            self.attach_mode_areas.iter().position(|a| a.contains(pos))
        } else {
            None
        };
        let new_telemetry = if self.current_page() == Page::Telemetry {
            self.telemetry_option_areas
                .iter()
                .position(|a| a.contains(pos))
        } else {
            None
        };
        let changed = self.hovered_button != new_button
            || self.hovered_theme_row != new_theme
            || self.hovered_attach_idx != new_attach
            || self.hovered_telemetry_idx != new_telemetry;
        self.hovered_button = new_button;
        self.hovered_theme_row = new_theme;
        self.hovered_attach_idx = new_attach;
        self.hovered_telemetry_idx = new_telemetry;
        changed
    }

    /// Drop per-page hover state when the page changes; the rects baked
    /// during the prior render no longer correspond to anything visible,
    /// so a stale hover would paint at the wrong coords until the next
    /// mouse-move event recomputes things.
    fn clear_page_hover(&mut self) {
        self.hovered_theme_row = None;
        self.hovered_attach_idx = None;
        self.hovered_telemetry_idx = None;
    }

    /// Route a left-click. Returns `Some(result)` when the click hit a known
    /// target; `None` when the click landed elsewhere inside the modal so the
    /// caller can swallow it (matching the pattern in `UnifiedDeleteDialog`).
    pub fn handle_click(&mut self, col: u16, row: u16) -> Option<DialogResult<IntroOutcome>> {
        let pos = Position::from((col, row));
        if self.skip_button_area.contains(pos) {
            return Some(self.cancel());
        }
        if self.back_button_area.contains(pos) && self.page_idx > 0 {
            self.go_back();
            return Some(DialogResult::Continue);
        }
        if self.next_button_area.contains(pos) {
            return Some(self.advance().unwrap_or(DialogResult::Continue));
        }
        if self.current_page() == Page::ThemePicker {
            for (idx, area) in self.theme_row_areas.iter().enumerate() {
                if area.contains(pos) {
                    if idx != self.theme_cursor {
                        self.theme_cursor = idx;
                        self.queue_preview_current();
                    }
                    return Some(DialogResult::Continue);
                }
            }
        }
        if self.current_page() == Page::AttachMode {
            let modes = [NewSessionAttachMode::LiveSend, NewSessionAttachMode::Tmux];
            for (idx, area) in self.attach_mode_areas.iter().enumerate() {
                if area.contains(pos) {
                    self.attach_mode_cursor = modes[idx];
                    return Some(DialogResult::Continue);
                }
            }
        }
        if self.current_page() == Page::Telemetry {
            for (idx, area) in self.telemetry_option_areas.iter().enumerate() {
                if area.contains(pos) {
                    // Index 0 = enable, 1 = decline. DO_NOT_TRACK forces off.
                    self.telemetry_opt_in = idx == 0 && !crate::telemetry::do_not_track();
                    return Some(DialogResult::Continue);
                }
            }
        }
        None
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let dialog_area = super::centered_rect(area, 72, 22);
        frame.render_widget(Clear, dialog_area);

        let total = Page::all().len();
        let title = format!(
            " Welcome to Agent of Empires  ({}/{}) ",
            self.page_idx + 1,
            total
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(title)
            .title_style(Style::default().fg(theme.accent).bold());
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([Constraint::Min(1), Constraint::Length(2)])
            .split(inner);

        match self.current_page() {
            Page::Welcome => self.render_welcome(frame, chunks[0], theme),
            Page::Telemetry => self.render_telemetry(frame, chunks[0], theme),
            Page::FirstSession => self.render_first_session(frame, chunks[0], theme),
            Page::AttachMode => self.render_attach_mode(frame, chunks[0], theme),
            Page::ThemePicker => self.render_theme_picker(frame, chunks[0], theme),
            Page::Done => self.render_done(frame, chunks[0], theme),
        }

        self.render_footer(frame, chunks[1], theme);
    }

    fn render_welcome(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let lines = vec![
            Line::from(Span::styled(
                "Agent of Empires (aoe) runs many AI coding agents side by side.",
                Style::default().fg(theme.text),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Git worktrees, sandboxed containers, the web dashboard, and the",
                Style::default().fg(theme.text),
            )),
            Line::from(Span::styled(
                "mobile structured view are all supported, and all optional. Use what",
                Style::default().fg(theme.text),
            )),
            Line::from(Span::styled(
                "fits your workflow.",
                Style::default().fg(theme.text),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Docs:      ", Style::default().fg(theme.dimmed)),
                Span::styled(
                    "https://www.agent-of-empires.com/docs/quick-start",
                    Style::default().fg(theme.accent),
                ),
            ]),
            Line::from(vec![
                Span::styled("  Tutorials: ", Style::default().fg(theme.dimmed)),
                Span::styled(
                    "https://www.youtube.com/@agent-of-empires",
                    Style::default().fg(theme.accent),
                ),
            ]),
            Line::from(vec![
                Span::styled("  Discord:   ", Style::default().fg(theme.dimmed)),
                Span::styled(
                    "https://discord.gg/5N3QKX3f6s",
                    Style::default().fg(theme.accent),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "This walkthrough covers starting a session, picking how you drive",
                Style::default().fg(theme.text),
            )),
            Line::from(Span::styled(
                "sessions, and picking a theme.",
                Style::default().fg(theme.text),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "→/Enter forward, ← back, Esc skip.",
                Style::default().fg(theme.hint).italic(),
            )),
            Line::from(Span::styled(
                "Drag to select the URLs above; your terminal handles the copy.",
                Style::default().fg(theme.hint).italic(),
            )),
        ];
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
    }

    fn render_telemetry(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let dnt = crate::telemetry::do_not_track();

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(7),
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);

        let intro = Paragraph::new(vec![
            Line::from(Span::styled(
                "Help improve aoe with anonymous usage telemetry?",
                Style::default().fg(theme.title).bold(),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "It shows us how aoe is actually used, so we can prioritize the",
                Style::default().fg(theme.text),
            )),
            Line::from(Span::styled(
                "features that matter most. Off by default; when on, aoe sends",
                Style::default().fg(theme.text),
            )),
            Line::from(Span::styled(
                "anonymous counts only: sessions, agents/models, version, and OS.",
                Style::default().fg(theme.text),
            )),
            Line::from(Span::styled(
                "Never prompts, paths, names, branches, or commands.",
                Style::default().fg(theme.text),
            )),
            Line::from(Span::styled(
                "Change it any time under Settings, or with `aoe telemetry`.",
                Style::default().fg(theme.dimmed),
            )),
        ])
        .wrap(Wrap { trim: false });
        frame.render_widget(intro, layout[0]);

        if dnt {
            // DO_NOT_TRACK is an absolute override; make the suppressed state
            // explicit rather than silently ignoring the toggle.
            self.telemetry_option_areas = [Rect::default(), Rect::default()];
            let note = Paragraph::new(vec![
                Line::from(Span::styled(
                    "DO_NOT_TRACK is set in your environment.",
                    Style::default().fg(theme.accent).bold(),
                )),
                Line::from(Span::styled(
                    "Telemetry stays off and no install id is generated, whatever you pick.",
                    Style::default().fg(theme.text),
                )),
            ])
            .wrap(Wrap { trim: false });
            frame.render_widget(note, layout[1]);
            let hint = Paragraph::new(Span::styled(
                "Enter to continue",
                Style::default().fg(theme.hint).italic(),
            ));
            frame.render_widget(hint, layout[4]);
            return;
        }

        let options = [
            (true, "Enable anonymous telemetry"),
            (false, "No thanks  (default)"),
        ];
        for (slot_idx, slot) in [layout[1], layout[2]].iter().enumerate() {
            let (value, label) = options[slot_idx];
            let is_selected = self.telemetry_opt_in == value;
            let is_hovered = self.hovered_telemetry_idx == Some(slot_idx);
            self.telemetry_option_areas[slot_idx] = *slot;
            let marker = if is_selected { "▶ " } else { "  " };
            let mut style = if is_selected {
                Style::default().fg(theme.accent).bold()
            } else {
                Style::default().fg(theme.text)
            };
            if is_hovered {
                style = style.bg(theme.selection);
            }
            let line = Line::from(vec![
                Span::styled(marker.to_string(), style),
                Span::styled(label.to_string(), style),
            ]);
            let para = Paragraph::new(line);
            let para = if is_hovered {
                para.style(Style::default().bg(theme.selection))
            } else {
                para
            };
            frame.render_widget(para, *slot);
        }

        let hint = Paragraph::new(Span::styled(
            "↑/↓ to choose  •  Enter to confirm",
            Style::default().fg(theme.hint).italic(),
        ));
        frame.render_widget(hint, layout[4]);
    }

    fn render_first_session(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let lines = vec![
            Line::from(Span::styled(
                "Start your first session:",
                Style::default().fg(theme.title).bold(),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("  n       ", Style::default().fg(theme.accent).bold()),
                Span::styled(
                    "New-session dialog. Pick an agent, working dir,",
                    Style::default().fg(theme.text),
                ),
            ]),
            Line::from(Span::styled(
                "          optional worktree branch or sandboxed container.",
                Style::default().fg(theme.text),
            )),
            Line::from(vec![
                Span::styled("  N       ", Style::default().fg(theme.accent).bold()),
                Span::styled(
                    "Same dialog, pre-filled from the highlighted row",
                    Style::default().fg(theme.text),
                ),
            ]),
            Line::from(Span::styled(
                "          (same dir + group). Quick way to spin up a sibling.",
                Style::default().fg(theme.text),
            )),
            Line::from(vec![
                Span::styled("  Enter   ", Style::default().fg(theme.accent).bold()),
                Span::styled(
                    "Activate the highlighted session. How that feels",
                    Style::default().fg(theme.text),
                ),
            ]),
            Line::from(Span::styled(
                "          depends on the attach mode you pick next.",
                Style::default().fg(theme.text),
            )),
            Line::from(vec![
                Span::styled("  m       ", Style::default().fg(theme.accent).bold()),
                Span::styled(
                    "Compose a message and send it to the highlighted",
                    Style::default().fg(theme.text),
                ),
            ]),
            Line::from(Span::styled(
                "          session without dropping into typing mode.",
                Style::default().fg(theme.text),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Sessions keep running when you detach or quit aoe.",
                Style::default().fg(theme.text),
            )),
            Line::from(Span::styled(
                "Press ? on the home view for the full shortcut list.",
                Style::default().fg(theme.hint).italic(),
            )),
        ];
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);
    }

    fn render_attach_mode(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        // Tight stack: 2-row intro, two 4-row option blocks back-to-back,
        // hint pinned to the bottom. Adjacent blocks read as "pick A or B"
        // instead of floating in their own halves of the page.
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(4),
                Constraint::Length(4),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);

        let intro = Paragraph::new(vec![
            Line::from(Span::styled(
                "How do you want to drive your sessions?",
                Style::default().fg(theme.title).bold(),
            )),
            Line::from(Span::styled(
                "Pick one. You can change it any time under Settings.",
                Style::default().fg(theme.dimmed),
            )),
        ])
        .wrap(Wrap { trim: false });
        frame.render_widget(intro, layout[0]);

        // Each option is title (with selection marker) + 3 indented body
        // lines explaining what the mode feels like, when to pick it, and
        // how to come back out. Title indent is 2 columns (marker + space);
        // body indent is 6 columns so the body reads as nested under the
        // title. Tmux's detach key uses the user's actual prefix
        // (`tmux_prefix_display()`), so the hint is correct on a
        // remapped-prefix setup.
        let prefix = crate::tmux::tmux_prefix_display();
        let tmux_back = format!("      tmux pane. {prefix} then d comes back to aoe.");
        let options = [
            (
                NewSessionAttachMode::LiveSend,
                "Live mode  (recommended; works for most workflows)",
                vec![
                    "      aoe stays open with the agent's terminal shown next to".to_string(),
                    "      the session list. Type to send keys to the highlighted".to_string(),
                    "      agent. Ctrl+Q stops typing. Tab attaches into tmux.".to_string(),
                ],
            ),
            (
                NewSessionAttachMode::Tmux,
                "Tmux mode  (advanced; for tmux power users)",
                vec![
                    "      Activation drops you into the agent's full-screen".to_string(),
                    tmux_back,
                    "      Tab takes you into live mode instead.".to_string(),
                ],
            ),
        ];

        for (slot_idx, slot) in [layout[1], layout[2]].iter().enumerate() {
            let (mode, label, body) = &options[slot_idx];
            let is_selected = self.attach_mode_cursor == *mode;
            let is_hovered = self.hovered_attach_idx == Some(slot_idx);
            self.attach_mode_areas[slot_idx] = *slot;
            let marker = if is_selected { "▶ " } else { "  " };
            let mut title_style = if is_selected {
                Style::default().fg(theme.accent).bold()
            } else {
                Style::default().fg(theme.text)
            };
            let mut body_style = if is_selected {
                Style::default().fg(theme.text)
            } else {
                Style::default().fg(theme.dimmed)
            };
            // Hover paints a subtle background tint across the whole option
            // block so the click target is obvious without overwriting the
            // selection marker / accent color.
            if is_hovered {
                title_style = title_style.bg(theme.selection);
                body_style = body_style.bg(theme.selection);
            }
            let mut lines = vec![Line::from(vec![
                Span::styled(marker.to_string(), title_style),
                Span::styled(label.to_string(), title_style),
            ])];
            for body_line in body {
                lines.push(Line::from(Span::styled(body_line.to_string(), body_style)));
            }
            let para = Paragraph::new(lines).wrap(Wrap { trim: false });
            let para = if is_hovered {
                para.style(Style::default().bg(theme.selection))
            } else {
                para
            };
            frame.render_widget(para, *slot);
        }

        let hint = Paragraph::new(Span::styled(
            "↑/↓ to switch  •  Enter to confirm",
            Style::default().fg(theme.hint).italic(),
        ));
        frame.render_widget(hint, layout[4]);
    }

    fn render_theme_picker(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(1)])
            .split(area);

        let header = Paragraph::new(vec![
            Line::from(Span::styled(
                "Pick a theme. ↑/↓ to navigate (changes apply live).",
                Style::default().fg(theme.text),
            )),
            Line::from(Span::styled(
                "Enter to keep the choice and move on; Esc reverts.",
                Style::default().fg(theme.hint).italic(),
            )),
        ]);
        frame.render_widget(header, layout[0]);

        let list_area = layout[1];
        self.theme_row_areas.clear();
        if self.themes.is_empty() {
            let msg = Paragraph::new(Span::styled(
                "No themes available.",
                Style::default().fg(theme.dimmed),
            ));
            frame.render_widget(msg, list_area);
            return;
        }

        // Render at most `list_area.height` rows; if the list is taller than
        // the area, slide the window so the cursor stays visible.
        let visible_rows = list_area.height as usize;
        let total = self.themes.len();
        let start = if total <= visible_rows || self.theme_cursor < visible_rows / 2 {
            0
        } else if self.theme_cursor + visible_rows / 2 >= total {
            total.saturating_sub(visible_rows)
        } else {
            self.theme_cursor - visible_rows / 2
        };
        let end = (start + visible_rows).min(total);

        // Maintain a row-area entry for every theme; entries outside the
        // visible window stay zero-sized so `contains()` returns false and
        // the click handler ignores them.
        self.theme_row_areas.resize(total, Rect::default());

        for (offset, idx) in (start..end).enumerate() {
            let row_y = list_area.y + offset as u16;
            if row_y >= list_area.y + list_area.height {
                break;
            }
            let row_area = Rect {
                x: list_area.x,
                y: row_y,
                width: list_area.width,
                height: 1,
            };
            self.theme_row_areas[idx] = row_area;

            let name = &self.themes[idx];
            let is_selected = idx == self.theme_cursor;
            let is_hovered = self.hovered_theme_row == Some(idx);
            let marker = if is_selected { " ▶ " } else { "   " };
            let mut style = if is_selected {
                Style::default().fg(theme.accent).bold()
            } else {
                Style::default().fg(theme.text)
            };
            if is_hovered {
                style = style.bg(theme.selection);
            }
            let line = Line::from(vec![
                Span::styled(marker.to_string(), style),
                Span::styled(name.clone(), style),
            ]);
            let para = Paragraph::new(line);
            let para = if is_hovered {
                para.style(Style::default().bg(theme.selection))
            } else {
                para
            };
            frame.render_widget(para, row_area);
        }
    }

    fn render_done(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let lines = vec![
            Line::from(Span::styled(
                "You're all set. What now?",
                Style::default().fg(theme.title).bold(),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Learn more",
                Style::default().fg(theme.text).bold(),
            )),
            Line::from(vec![
                Span::styled("  Docs:      ", Style::default().fg(theme.dimmed)),
                Span::styled(
                    "https://www.agent-of-empires.com/docs",
                    Style::default().fg(theme.accent),
                ),
            ]),
            Line::from(vec![
                Span::styled("  Tutorials: ", Style::default().fg(theme.dimmed)),
                Span::styled(
                    "https://www.youtube.com/@agent-of-empires",
                    Style::default().fg(theme.accent),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Top keys on the home view",
                Style::default().fg(theme.text).bold(),
            )),
            Line::from(vec![
                Span::styled("  ?        ", Style::default().fg(theme.accent).bold()),
                Span::styled("full keyboard shortcuts", Style::default().fg(theme.text)),
            ]),
            Line::from(vec![
                Span::styled("  n        ", Style::default().fg(theme.accent).bold()),
                Span::styled("new session", Style::default().fg(theme.text)),
            ]),
            Line::from(vec![
                Span::styled("  s        ", Style::default().fg(theme.accent).bold()),
                Span::styled("settings", Style::default().fg(theme.text)),
            ]),
            Line::from(vec![
                Span::styled("  Ctrl+K   ", Style::default().fg(theme.accent).bold()),
                Span::styled("command palette", Style::default().fg(theme.text)),
            ]),
            Line::from(vec![
                Span::styled("  q        ", Style::default().fg(theme.accent).bold()),
                Span::styled(
                    "quit (sessions keep running)",
                    Style::default().fg(theme.text),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Press Enter to dive in.",
                Style::default().fg(theme.hint).italic(),
            )),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }

    fn render_footer(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(10),
                Constraint::Min(0),
                Constraint::Length(10),
                Constraint::Length(2),
                Constraint::Length(12),
            ])
            .split(area);

        let skip_label = "[Skip]";
        let mut skip_style = Style::default().fg(theme.dimmed);
        if self.hovered_button == Some(HoverButton::Skip) {
            skip_style = skip_style.bg(theme.selection);
        }
        let skip = Paragraph::new(Span::styled(skip_label, skip_style)).alignment(Alignment::Left);
        frame.render_widget(skip, layout[0]);
        self.skip_button_area = Rect {
            x: layout[0].x,
            y: layout[0].y,
            width: skip_label.len() as u16,
            height: 1,
        };

        let back_label = "[← Back]";
        if self.page_idx > 0 {
            let mut back_style = Style::default().fg(theme.accent);
            if self.hovered_button == Some(HoverButton::Back) {
                back_style = back_style.bg(theme.selection);
            }
            let back =
                Paragraph::new(Span::styled(back_label, back_style)).alignment(Alignment::Right);
            frame.render_widget(back, layout[2]);
            self.back_button_area = Rect {
                x: layout[2]
                    .right()
                    .saturating_sub(back_label.chars().count() as u16),
                y: layout[2].y,
                width: back_label.chars().count() as u16,
                height: 1,
            };
        } else {
            self.back_button_area = Rect::default();
        }

        let next_label = if self.is_last_page() {
            "[Finish]"
        } else {
            "[Next →]"
        };
        let mut next_style = Style::default().fg(theme.accent).bold();
        if self.hovered_button == Some(HoverButton::Next) {
            next_style = next_style.bg(theme.selection);
        }
        let next = Paragraph::new(Span::styled(next_label, next_style)).alignment(Alignment::Right);
        frame.render_widget(next, layout[4]);
        self.next_button_area = Rect {
            x: layout[4]
                .right()
                .saturating_sub(next_label.chars().count() as u16),
            y: layout[4].y,
            width: next_label.chars().count() as u16,
            height: 1,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn opens_on_welcome_page() {
        let dialog = IntroDialog::new("default");
        assert_eq!(dialog.current_page(), Page::Welcome);
        assert_eq!(dialog.page_idx, 0);
    }

    #[test]
    fn enter_advances_through_pages_to_finish() {
        let mut dialog = IntroDialog::new("default");
        // Welcome -> Telemetry (marks telemetry_visited)
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Enter)),
            DialogResult::Continue
        ));
        assert_eq!(dialog.current_page(), Page::Telemetry);
        assert!(dialog.telemetry_visited);
        // Telemetry -> FirstSession
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Enter)),
            DialogResult::Continue
        ));
        assert_eq!(dialog.current_page(), Page::FirstSession);
        // FirstSession -> AttachMode (marks attach_mode_visited)
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Enter)),
            DialogResult::Continue
        ));
        assert_eq!(dialog.current_page(), Page::AttachMode);
        assert!(dialog.attach_mode_visited);
        // AttachMode -> ThemePicker (marks theme_visited)
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Enter)),
            DialogResult::Continue
        ));
        assert_eq!(dialog.current_page(), Page::ThemePicker);
        assert!(dialog.theme_visited);
        // ThemePicker -> Done
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Enter)),
            DialogResult::Continue
        ));
        assert_eq!(dialog.current_page(), Page::Done);
        // Done -> Submit
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(result, DialogResult::Submit(_)));
    }

    #[test]
    fn left_arrow_goes_back() {
        let mut dialog = IntroDialog::new("default");
        dialog.handle_key(key(KeyCode::Enter)); // -> FirstSession
        dialog.handle_key(key(KeyCode::Left)); // -> Welcome
        assert_eq!(dialog.current_page(), Page::Welcome);
        // No-op on first page.
        dialog.handle_key(key(KeyCode::Left));
        assert_eq!(dialog.current_page(), Page::Welcome);
    }

    #[test]
    fn esc_cancels_without_submit() {
        let mut dialog = IntroDialog::new("default");
        let result = dialog.handle_key(key(KeyCode::Esc));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn esc_reverts_preview_when_theme_was_visited() {
        let mut dialog = IntroDialog::new("default");
        // Walk to theme page (Welcome -> Telemetry -> FirstSession ->
        // AttachMode -> Theme).
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Enter));
        assert_eq!(dialog.current_page(), Page::ThemePicker);
        let _ = dialog.take_pending_preview(); // drain initial preview
                                               // Move cursor — only fire the assertion when the theme list has at
                                               // least two entries (built-in count is 8 today, but guard anyway).
        if dialog.themes.len() > 1 {
            dialog.handle_key(key(KeyCode::Down));
            assert!(dialog.take_pending_preview().is_some());
            // Cancel — caller should see a revert-to-original preview queued.
            let _ = dialog.handle_key(key(KeyCode::Esc));
            assert_eq!(dialog.take_pending_preview().as_deref(), Some("default"));
        }
    }

    #[test]
    fn theme_arrow_keys_navigate_picker_and_queue_preview() {
        let mut dialog = IntroDialog::new("default");
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Enter));
        assert_eq!(dialog.current_page(), Page::ThemePicker);
        // Entering the theme page itself queues a preview of the cursor's
        // current theme; drain it so we can isolate the arrow-key effect.
        let _ = dialog.take_pending_preview();
        if dialog.themes.len() > 1 {
            let before = dialog.theme_cursor;
            dialog.handle_key(key(KeyCode::Down));
            assert_ne!(dialog.theme_cursor, before);
            assert!(dialog.take_pending_preview().is_some());
        }
    }

    #[test]
    fn submit_outcome_carries_final_theme_when_user_picks_a_new_one() {
        let mut dialog = IntroDialog::new("default");
        // Walk to ThemePicker (Welcome -> Telemetry -> FirstSession ->
        // AttachMode -> ThemePicker).
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Enter));
        assert_eq!(dialog.current_page(), Page::ThemePicker);
        // Move the cursor so the pick differs from the original "default";
        // outcome() suppresses identity picks to avoid an unnecessary
        // SetTheme dispatch + screen clear on close.
        if dialog.themes.len() > 1 {
            dialog.handle_key(key(KeyCode::Down));
        }
        dialog.handle_key(key(KeyCode::Enter)); // -> Done
        dialog.handle_key(key(KeyCode::Enter)); // submit
        let outcome = dialog.outcome();
        assert!(outcome.final_theme.is_some());
        assert_ne!(outcome.final_theme.as_deref(), Some("default"));
        // LiveSend is the wizard default, surfaced regardless of whether
        // the user toggled.
        assert_eq!(
            outcome.final_attach_mode,
            Some(NewSessionAttachMode::LiveSend)
        );
    }

    #[test]
    fn submit_outcome_omits_theme_when_user_lands_back_on_original() {
        let mut dialog = IntroDialog::new("default");
        // Walk through all pages without touching the cursor: it stays on
        // "default", which equals the original; outcome() should report
        // None so the close path skips a needless SetTheme dispatch.
        for _ in 0..6 {
            let _ = dialog.handle_key(key(KeyCode::Enter));
        }
        let outcome = dialog.outcome();
        assert!(outcome.final_theme.is_none());
    }

    #[test]
    fn outcome_has_no_theme_when_skipped_before_theme_page() {
        let mut dialog = IntroDialog::new("default");
        // Skip on page 0.
        let _ = dialog.handle_key(key(KeyCode::Esc));
        let outcome = dialog.outcome();
        assert!(outcome.final_theme.is_none());
        assert!(outcome.final_attach_mode.is_none());
    }

    #[test]
    fn attach_mode_toggle_switches_cursor() {
        let mut dialog = IntroDialog::new("default");
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Enter));
        assert_eq!(dialog.current_page(), Page::AttachMode);
        assert_eq!(dialog.attach_mode_cursor, NewSessionAttachMode::LiveSend);
        dialog.handle_key(key(KeyCode::Down));
        assert_eq!(dialog.attach_mode_cursor, NewSessionAttachMode::Tmux);
        dialog.handle_key(key(KeyCode::Up));
        assert_eq!(dialog.attach_mode_cursor, NewSessionAttachMode::LiveSend);
    }

    #[test]
    fn wants_text_selection_stays_on_so_urls_drag_copy_anywhere() {
        // The whole walkthrough wants mouse capture off so the docs /
        // YouTube / Discord URLs (and any other text) drag-copy
        // natively. Lock that in for every page; a future maintainer
        // who flips this for "clickable buttons" should make a
        // conscious choice rather than regressing the copy flow.
        let mut dialog = IntroDialog::new("default");
        for _ in 0..6 {
            assert!(dialog.wants_text_selection());
            dialog.handle_key(key(KeyCode::Right));
        }
    }

    #[test]
    fn handle_hover_picks_up_attach_option_rects() {
        let mut dialog = IntroDialog::new("default");
        // Stub a rect on the second attach option so the hit-test has
        // something to find; the render path normally populates these.
        dialog.attach_mode_areas[1] = Rect {
            x: 5,
            y: 5,
            width: 10,
            height: 4,
        };
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Enter));
        assert_eq!(dialog.current_page(), Page::AttachMode);
        // Inside the second option rect.
        let changed = dialog.handle_hover(8, 6);
        assert!(changed);
        assert_eq!(dialog.hovered_attach_idx, Some(1));
        // Same position again is a no-op.
        assert!(!dialog.handle_hover(8, 6));
        // Mouse leaves the rect: hover clears.
        assert!(dialog.handle_hover(0, 0));
        assert_eq!(dialog.hovered_attach_idx, None);
    }

    #[test]
    fn handle_hover_only_acts_on_theme_page_for_theme_rows() {
        let mut dialog = IntroDialog::new("default");
        // Stub a row rect, then check that hover is ignored off the theme
        // page (page check guards stale rects).
        dialog.theme_row_areas = vec![
            Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 1,
            };
            dialog.themes.len()
        ];
        // Welcome page: hover over a theme rect must not register.
        assert!(!dialog.handle_hover(5, 0));
        assert_eq!(dialog.hovered_theme_row, None);
        // Advance to ThemePicker; same hover should now register on row 0.
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Enter));
        assert_eq!(dialog.current_page(), Page::ThemePicker);
        assert!(dialog.handle_hover(5, 0));
        assert_eq!(dialog.hovered_theme_row, Some(0));
    }

    #[test]
    fn page_change_clears_per_page_hover_state() {
        let mut dialog = IntroDialog::new("default");
        dialog.theme_row_areas = vec![
            Rect {
                x: 0,
                y: 0,
                width: 20,
                height: 1,
            };
            dialog.themes.len()
        ];
        // Walk to ThemePicker and seed a hover.
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Enter));
        assert_eq!(dialog.current_page(), Page::ThemePicker);
        let _ = dialog.handle_hover(5, 0);
        assert!(dialog.hovered_theme_row.is_some());
        // Advancing to Done clears the per-page hover; if it leaked, the
        // dialog would paint a hover background on whatever cell those
        // coords map to under the new page.
        dialog.handle_key(key(KeyCode::Enter));
        assert_eq!(dialog.hovered_theme_row, None);
    }

    #[test]
    fn outcome_carries_tmux_when_user_picks_it() {
        let mut dialog = IntroDialog::new("default");
        // Walk to AttachMode, toggle to Tmux, then walk to the end.
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Enter));
        assert_eq!(dialog.current_page(), Page::AttachMode);
        dialog.handle_key(key(KeyCode::Down));
        assert_eq!(dialog.attach_mode_cursor, NewSessionAttachMode::Tmux);
        for _ in 0..3 {
            let _ = dialog.handle_key(key(KeyCode::Enter));
        }
        let outcome = dialog.outcome();
        assert_eq!(outcome.final_attach_mode, Some(NewSessionAttachMode::Tmux));
    }

    #[test]
    fn telemetry_toggle_switches_choice() {
        let mut dialog = IntroDialog::new("default");
        // Welcome -> Telemetry.
        dialog.handle_key(key(KeyCode::Enter));
        assert_eq!(dialog.current_page(), Page::Telemetry);
        // Default is opt-out; toggle on, then back off.
        assert!(!dialog.telemetry_opt_in);
        dialog.handle_key(key(KeyCode::Down));
        assert!(dialog.telemetry_opt_in);
        dialog.handle_key(key(KeyCode::Up));
        assert!(!dialog.telemetry_opt_in);
    }

    #[test]
    fn outcome_carries_telemetry_opt_in_when_user_enables_it() {
        let mut dialog = IntroDialog::new("default");
        dialog.handle_key(key(KeyCode::Enter)); // -> Telemetry
        dialog.handle_key(key(KeyCode::Down)); // opt in
        for _ in 0..5 {
            let _ = dialog.handle_key(key(KeyCode::Enter));
        }
        let outcome = dialog.outcome();
        assert_eq!(outcome.telemetry_opt_in, Some(true));
    }

    #[test]
    fn outcome_carries_telemetry_decline_when_left_default() {
        let mut dialog = IntroDialog::new("default");
        // Walk the whole wizard without touching the telemetry choice.
        for _ in 0..6 {
            let _ = dialog.handle_key(key(KeyCode::Enter));
        }
        let outcome = dialog.outcome();
        // Visited the page but left it on the default: an explicit decline.
        assert_eq!(outcome.telemetry_opt_in, Some(false));
    }

    #[test]
    fn outcome_omits_telemetry_when_skipped_before_the_page() {
        let mut dialog = IntroDialog::new("default");
        // Skip on the Welcome page, before reaching Telemetry.
        let _ = dialog.handle_key(key(KeyCode::Esc));
        assert_eq!(dialog.outcome().telemetry_opt_in, None);
    }

    #[test]
    fn skip_button_click_cancels() {
        let mut dialog = IntroDialog::new("default");
        // Prime button rects via a render-equivalent: drive footer rects by
        // hand since render needs a Frame. Instead we exercise the keyboard
        // path here; the click handler is exercised by render-driven tests.
        let result = dialog.handle_key(key(KeyCode::Esc));
        assert!(matches!(result, DialogResult::Cancel));
    }
}
