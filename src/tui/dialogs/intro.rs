//! First-run walkthrough: what AoE is, how to start a session, a theme picker
//! with live preview, and the help shortcut. Gated on
//! `config.app_state.has_seen_welcome`.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Position;
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::session::AttachMode;
use crate::tui::styles::{available_themes, Theme};

/// Outcome of the intro wizard. Each field is `Some` only when the user
/// reached its page, so a wizard skipped early overwrites nothing.
#[derive(Debug, Clone)]
pub struct IntroOutcome {
    pub final_theme: Option<String>,
    pub final_attach_mode: Option<AttachMode>,
    /// The Telemetry page's choice; the caller marks the opt-in prompt
    /// answered only when it is `Some`.
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

/// The footer button under the mouse, tinted without moving keyboard focus.
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
    /// Theme active when the dialog opened, restored if the user cancels so
    /// no half-picked preview outlives the wizard.
    original_theme: String,
    themes: Vec<String>,
    theme_cursor: usize,
    /// A theme choice persists only once the user has seen the page.
    theme_visited: bool,
    /// Theme to live-preview next tick; the home view consumes it.
    pending_preview: Option<String>,
    page_idx: usize,
    skip_button_area: Rect,
    back_button_area: Rect,
    next_button_area: Rect,
    /// Per-theme row rects, empty off the theme page.
    theme_row_areas: Vec<Rect>,

    /// Seeded to `LiveSend`, so new users keep the home list in view. Existing
    /// users never see the wizard and keep the historical `Tmux` default.
    attach_mode_cursor: AttachMode,
    /// An attach-mode choice persists only once the page has been seen.
    attach_mode_visited: bool,
    /// Rects for the LiveSend and Tmux options.
    attach_mode_areas: [Rect; 2],

    hovered_button: Option<HoverButton>,
    /// Hovered row on the ThemePicker page.
    hovered_theme_row: Option<usize>,
    /// Hovered option on the AttachMode page (0 = LiveSend, 1 = Tmux).
    hovered_attach_idx: Option<usize>,

    /// Telemetry stays off unless explicitly enabled here.
    telemetry_opt_in: bool,
    /// The telemetry choice persists only once the page has been seen.
    telemetry_visited: bool,
    telemetry_option_areas: [Rect; 2],
    /// Hovered option on the Telemetry page.
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
            attach_mode_cursor: AttachMode::LiveSend,
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

    /// Theme to preview now, if the cursor moved since the last call.
    pub fn take_pending_preview(&mut self) -> Option<String> {
        self.pending_preview.take()
    }

    /// True on every page, so xterm mouse tracking stays off and the terminal
    /// can drag-select the URLs. The trade is keyboard-only navigation, which
    /// each page's hint advertises.
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
        if self.theme_visited && self.themes.get(self.theme_cursor) != Some(&self.original_theme) {
            self.pending_preview = Some(self.original_theme.clone());
        }
        DialogResult::Cancel
    }

    fn outcome(&self) -> IntroOutcome {
        // An identity SetTheme would clear the terminal on the next loop
        // iteration, which reads as a flash as the wizard closes.
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
            AttachMode::LiveSend => AttachMode::Tmux,
            AttachMode::Tmux => AttachMode::LiveSend,
        };
    }

    /// Flip the telemetry opt-in, a no-op under `DO_NOT_TRACK` so the shown
    /// choice cannot drift from what happens.
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
        // The theme page eats up/down to navigate its list.
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

        if self.current_page() == Page::AttachMode {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
                    self.toggle_attach_mode();
                    return DialogResult::Continue;
                }
                _ => {}
            }
        }

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

    /// Track the cursor without moving keyboard focus, so a drift while
    /// reading cannot switch the user's pick. True only when the hover target
    /// changes, so a pixel-level twitch skips the redraw.
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
            let modes = [AttachMode::LiveSend, AttachMode::Tmux];
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
        let block = super::toned_dialog_block(title, theme.accent, theme.accent);
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
        // title. The tmux hint uses the user's actual prefix and names the
        // key that undoes this process's attach path; from inside tmux the
        // switch can fall back to a fresh attach, so both keys are shown.
        let prefix = crate::tmux::tmux_prefix_display();
        let key = crate::tmux::attach_return_hint();
        let tmux_back = format!("      tmux pane. {prefix} then {key} comes back to aoe.");
        let options = [
            (
                AttachMode::LiveSend,
                "Live mode  (recommended; works for most workflows)",
                vec![
                    "      aoe stays open with the agent's terminal shown next to".to_string(),
                    "      the session list. Type to send keys to the highlighted".to_string(),
                    "      agent. Ctrl+Q stops typing. Tab attaches into tmux.".to_string(),
                ],
            ),
            (
                AttachMode::Tmux,
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
    use crate::tui::dialogs::test_keys::key;

    /// The wizard's page order; `Enter` walks it and a sixth press submits.
    const PAGES: &[Page] = &[
        Page::Welcome,
        Page::Telemetry,
        Page::FirstSession,
        Page::AttachMode,
        Page::ThemePicker,
        Page::Done,
    ];

    /// A dialog parked on `page`, having walked there with Enter.
    fn on_page(page: Page) -> IntroDialog {
        let mut dialog = IntroDialog::new("zinc");
        let steps = PAGES.iter().position(|p| *p == page).expect("known page");
        for _ in 0..steps {
            assert!(matches!(
                dialog.handle_key(key(KeyCode::Enter)),
                DialogResult::Continue
            ));
        }
        assert_eq!(dialog.current_page(), page);
        dialog
    }

    fn row_rect() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 1,
        }
    }

    #[test]
    fn enter_walks_every_page_and_marks_the_ones_it_visits() {
        let mut dialog = IntroDialog::new("zinc");
        assert_eq!(dialog.page_idx, 0);
        for page in &PAGES[1..] {
            assert!(matches!(
                dialog.handle_key(key(KeyCode::Enter)),
                DialogResult::Continue
            ));
            assert_eq!(dialog.current_page(), *page);
        }
        assert!(dialog.telemetry_visited);
        assert!(dialog.attach_mode_visited);
        assert!(dialog.theme_visited);
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Enter)),
            DialogResult::Submit(_)
        ));

        // Left steps back and is inert on the first page.
        let mut dialog = on_page(Page::Telemetry);
        for _ in 0..2 {
            dialog.handle_key(key(KeyCode::Left));
            assert_eq!(dialog.current_page(), Page::Welcome);
        }

        // Esc skips out without submitting.
        assert!(matches!(
            IntroDialog::new("zinc").handle_key(key(KeyCode::Esc)),
            DialogResult::Cancel
        ));
    }

    #[test]
    fn the_theme_picker_previews_as_the_cursor_moves_and_reverts_on_skip() {
        let mut dialog = on_page(Page::ThemePicker);
        // Arriving on the page queues a preview of the cursor's theme; drain
        // it to isolate the arrow key's effect.
        let _ = dialog.take_pending_preview();
        if dialog.themes.len() > 1 {
            let before = dialog.theme_cursor;
            dialog.handle_key(key(KeyCode::Down));
            assert_ne!(dialog.theme_cursor, before);
            assert!(dialog.take_pending_preview().is_some());

            // Skipping queues a revert to the theme the wizard opened on.
            let _ = dialog.handle_key(key(KeyCode::Esc));
            assert_eq!(dialog.take_pending_preview().as_deref(), Some("zinc"));
        }
    }

    #[test]
    fn the_outcome_reports_only_the_choices_the_user_reached() {
        // Skipping on the first page reaches no choice at all.
        let mut skipped = IntroDialog::new("zinc");
        let _ = skipped.handle_key(key(KeyCode::Esc));
        let outcome = skipped.outcome();
        assert!(outcome.final_theme.is_none());
        assert!(outcome.final_attach_mode.is_none());
        assert_eq!(outcome.telemetry_opt_in, None);

        // Walking through without touching anything: the theme still equals
        // the original, so it is suppressed rather than dispatching a
        // needless SetTheme and screen clear. Visiting the telemetry page and
        // leaving its default is an explicit decline, and LiveSend is the
        // wizard's attach default.
        let mut untouched = IntroDialog::new("zinc");
        for _ in 0..6 {
            let _ = untouched.handle_key(key(KeyCode::Enter));
        }
        let outcome = untouched.outcome();
        assert!(outcome.final_theme.is_none());
        assert_eq!(outcome.final_attach_mode, Some(AttachMode::LiveSend));
        assert_eq!(outcome.telemetry_opt_in, Some(false));

        // Picking a different theme carries it out.
        let mut picked = on_page(Page::ThemePicker);
        if picked.themes.len() > 1 {
            picked.handle_key(key(KeyCode::Down));
        }
        for _ in 0..2 {
            let _ = picked.handle_key(key(KeyCode::Enter));
        }
        let outcome = picked.outcome();
        assert!(outcome.final_theme.is_some());
        assert_ne!(outcome.final_theme.as_deref(), Some("zinc"));
    }

    #[test]
    fn the_attach_and_telemetry_pages_toggle_with_the_arrows() {
        let mut dialog = on_page(Page::AttachMode);
        assert_eq!(dialog.attach_mode_cursor, AttachMode::LiveSend);
        dialog.handle_key(key(KeyCode::Down));
        assert_eq!(dialog.attach_mode_cursor, AttachMode::Tmux);
        dialog.handle_key(key(KeyCode::Up));
        assert_eq!(dialog.attach_mode_cursor, AttachMode::LiveSend);
        dialog.handle_key(key(KeyCode::Down));
        for _ in 0..3 {
            let _ = dialog.handle_key(key(KeyCode::Enter));
        }
        assert_eq!(dialog.outcome().final_attach_mode, Some(AttachMode::Tmux));

        let mut dialog = on_page(Page::Telemetry);
        assert!(!dialog.telemetry_opt_in, "opt-out is the default");
        dialog.handle_key(key(KeyCode::Down));
        assert!(dialog.telemetry_opt_in);
        dialog.handle_key(key(KeyCode::Up));
        assert!(!dialog.telemetry_opt_in);
        dialog.handle_key(key(KeyCode::Down));
        for _ in 0..5 {
            let _ = dialog.handle_key(key(KeyCode::Enter));
        }
        assert_eq!(dialog.outcome().telemetry_opt_in, Some(true));
    }

    #[test]
    fn every_page_keeps_mouse_capture_off_so_urls_drag_copy() {
        // Flipping this for clickable buttons would regress the copy flow on
        // the docs / YouTube / Discord URLs, so it should be a conscious call.
        let mut dialog = IntroDialog::new("zinc");
        for _ in 0..6 {
            assert!(dialog.wants_text_selection());
            dialog.handle_key(key(KeyCode::Right));
        }
    }

    #[test]
    fn hover_tracks_the_current_page_s_rows_and_clears_when_it_changes() {
        // Rects the render pass would normally populate.
        let mut dialog = IntroDialog::new("zinc");
        dialog.attach_mode_areas[1] = Rect {
            x: 5,
            y: 5,
            width: 10,
            height: 4,
        };
        for _ in 0..3 {
            dialog.handle_key(key(KeyCode::Enter));
        }
        assert_eq!(dialog.current_page(), Page::AttachMode);
        assert!(dialog.handle_hover(8, 6));
        assert_eq!(dialog.hovered_attach_idx, Some(1));
        assert!(!dialog.handle_hover(8, 6), "the same cell is no redraw");
        assert!(dialog.handle_hover(0, 0));
        assert_eq!(dialog.hovered_attach_idx, None);

        // A theme row's rect is ignored off the theme page, so a stale rect
        // cannot paint a highlight over whatever now occupies those cells.
        let mut dialog = IntroDialog::new("zinc");
        dialog.theme_row_areas = vec![row_rect(); dialog.themes.len()];
        assert!(!dialog.handle_hover(5, 0));
        assert_eq!(dialog.hovered_theme_row, None);
        for _ in 0..4 {
            dialog.handle_key(key(KeyCode::Enter));
        }
        assert_eq!(dialog.current_page(), Page::ThemePicker);
        assert!(dialog.handle_hover(5, 0));
        assert_eq!(dialog.hovered_theme_row, Some(0));
        dialog.handle_key(key(KeyCode::Enter));
        assert_eq!(dialog.hovered_theme_row, None, "cleared on page change");
    }
}
