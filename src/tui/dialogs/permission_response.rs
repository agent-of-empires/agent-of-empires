//! Answer a session's pending permission prompt.
//!
//! A terminal session gets the exact keystrokes a human would type, without
//! attaching: AoE never parses pane content to detect or validate the prompt,
//! and the user has already seen it on the pane. A structured session resolves
//! the daemon-reported approval nonce through ACP, showing the tool, target
//! and destructive flag from its projection. Allow Always is offered whenever
//! the agent's mapping has it, and always on the ACP path.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::tui::styles::{has_min_contrast, Theme};

/// Contrast floor for the focused choice against the dialog background. The
/// tightest builtin clears 2.64; a duller custom `accent` falls back to
/// `theme.text` rather than rendering illegibly.
const MIN_FOCUSED_CONTRAST_RATIO: f32 = 2.5;

/// Focused choice: bold `theme.accent`, the fg-only treatment `render_yes_no`
/// gives its focused button. `theme.selection` is a background token and as a
/// foreground would make the focused choice the dimmest item on the row, so
/// focus is carried by the gap to `theme.dimmed` instead.
fn focused_choice_style(theme: &Theme) -> Style {
    let fg = if has_min_contrast(theme.accent, theme.background, MIN_FOCUSED_CONTRAST_RATIO) {
        theme.accent
    } else {
        theme.text
    };
    Style::default().fg(fg).bold()
}

fn unfocused_choice_style(theme: &Theme) -> Style {
    Style::default().fg(theme.dimmed)
}

/// Which choice the user picked; not every agent offers `AllowAlways`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionResponseChoice {
    Allow,
    AllowAlways,
    Deny,
}

struct StructuredApprovalDetail {
    tool_name: String,
    target: String,
    destructive: bool,
}

pub struct PermissionResponseDialog {
    session_title: String,
    choices: Vec<(&'static str, PermissionResponseChoice)>,
    focused: usize,
    /// Whether `choices` includes `AllowAlways`, computed once in `new`.
    supports_allow_always: bool,
    /// `Some` for a structured ACP approval: the dialog shows what is being
    /// approved and drops the terminal-only "raw keystrokes" guidance, which
    /// does not hold on the ACP path.
    detail: Option<StructuredApprovalDetail>,
}

const ALL_CHOICES: [(&str, PermissionResponseChoice); 3] = [
    ("Allow", PermissionResponseChoice::Allow),
    ("Allow Always", PermissionResponseChoice::AllowAlways),
    ("Deny", PermissionResponseChoice::Deny),
];

impl PermissionResponseDialog {
    pub fn new(
        session_title: &str,
        allow_always: Option<&'static [crate::agents::KeyToken]>,
    ) -> Self {
        Self::build(session_title, allow_always.is_some(), None)
    }

    /// Show the ACP tool, target and destructive flag before resolving its request.
    pub fn structured(
        session_title: &str,
        tool_name: &str,
        target: &str,
        destructive: bool,
    ) -> Self {
        Self::build(
            session_title,
            true,
            Some(StructuredApprovalDetail {
                tool_name: tool_name.to_owned(),
                target: target.to_owned(),
                destructive,
            }),
        )
    }

    fn build(
        session_title: &str,
        supports_allow_always: bool,
        detail: Option<StructuredApprovalDetail>,
    ) -> Self {
        let choices = ALL_CHOICES
            .into_iter()
            .filter(|(_, choice)| {
                supports_allow_always || *choice != PermissionResponseChoice::AllowAlways
            })
            .collect();
        Self {
            session_title: session_title.to_string(),
            choices,
            focused: 0,
            supports_allow_always,
            detail,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<PermissionResponseChoice> {
        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            KeyCode::Enter => DialogResult::Submit(self.choices[self.focused].1),
            KeyCode::Left | KeyCode::Up => {
                self.focused = (self.focused + self.choices.len() - 1) % self.choices.len();
                DialogResult::Continue
            }
            KeyCode::Right | KeyCode::Down | KeyCode::Tab => {
                self.focused = (self.focused + 1) % self.choices.len();
                DialogResult::Continue
            }
            // a/d mirror structured_view/input.rs; A offered only when the
            // agent has allow_always.
            KeyCode::Char('a') => DialogResult::Submit(PermissionResponseChoice::Allow),
            KeyCode::Char('A') if self.supports_allow_always => {
                DialogResult::Submit(PermissionResponseChoice::AllowAlways)
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                DialogResult::Submit(PermissionResponseChoice::Deny)
            }
            _ => DialogResult::Continue,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block =
            super::toned_dialog_block(" Respond to Permission Prompt ", theme.accent, theme.accent);
        let (_, inner) = super::render_dialog_frame(frame, area, 56, 9, block);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(inner);

        use crate::tui::components::text::truncate_to_width;

        let width = chunks[0].width as usize;
        let mut header_lines = Vec::with_capacity(3);
        header_lines.push(Line::from(Span::styled(
            truncate_to_width(&self.session_title, width),
            Style::default().fg(theme.title).bold(),
        )));
        match &self.detail {
            Some(detail) => {
                let warning = if detail.destructive {
                    "destructive: "
                } else {
                    ""
                };
                header_lines.push(Line::from(vec![
                    Span::styled(warning, Style::default().fg(theme.error).bold()),
                    Span::styled(
                        truncate_to_width(&detail.tool_name, width.saturating_sub(warning.len())),
                        Style::default().fg(theme.text),
                    ),
                ]));
                header_lines.push(Line::from(Span::styled(
                    truncate_to_width(&detail.target, width),
                    Style::default().fg(theme.text),
                )));
            }
            None => header_lines.push(Line::from(Span::styled(
                "AoE sends these as raw keystrokes; make sure the prompt is on screen.",
                Style::default().fg(theme.dimmed),
            ))),
        }
        let header = Paragraph::new(header_lines).wrap(Wrap { trim: false });
        frame.render_widget(header, chunks[0]);

        let mut spans = Vec::new();
        for (i, (label, _)) in self.choices.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("   "));
            }
            let style = if i == self.focused {
                focused_choice_style(theme)
            } else {
                unfocused_choice_style(theme)
            };
            spans.push(Span::styled(format!("[{}]", label), style));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
            chunks[1],
        );

        let mut hint = String::from("a=allow");
        if self.supports_allow_always {
            hint.push_str("  A=always");
        }
        hint.push_str("  d=deny  Esc=cancel");
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(theme.dimmed),
            )))
            .alignment(Alignment::Center),
            chunks[2],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::dialogs::test_keys::key;

    /// Stand-in for a real agent's `allow_always` mapping. `Some(&[])` would
    /// also satisfy `.is_some()` but reads as "supported with zero
    /// keystrokes", a footgun if copy-pasted into a real `AgentDef`.
    const ALLOW_ALWAYS: Option<&[crate::agents::KeyToken]> =
        Some(&[crate::agents::KeyToken::Literal("2")]);

    fn dialog() -> PermissionResponseDialog {
        PermissionResponseDialog::new("test", ALLOW_ALWAYS)
    }

    #[test]
    fn each_choice_has_a_direct_key_and_a_focus_path() {
        // (key, the choice it submits outright)
        for (code, want) in [
            (KeyCode::Char('a'), PermissionResponseChoice::Allow),
            (KeyCode::Char('A'), PermissionResponseChoice::AllowAlways),
            (KeyCode::Char('d'), PermissionResponseChoice::Deny),
            // Allow is focused when the dialog opens.
            (KeyCode::Enter, PermissionResponseChoice::Allow),
        ] {
            assert!(
                matches!(dialog().handle_key(key(code)), DialogResult::Submit(got) if got == want),
                "{code:?}"
            );
        }

        // The arrows cycle focus in both directions, wrapping.
        for (code, want) in [
            (KeyCode::Right, PermissionResponseChoice::AllowAlways),
            (KeyCode::Left, PermissionResponseChoice::Deny),
        ] {
            let mut d = dialog();
            d.handle_key(key(code));
            assert!(
                matches!(d.handle_key(key(KeyCode::Enter)), DialogResult::Submit(got) if got == want),
                "{code:?}"
            );
        }

        assert!(matches!(
            dialog().handle_key(key(KeyCode::Esc)),
            DialogResult::Cancel
        ));
    }

    #[test]
    fn an_agent_without_allow_always_neither_shows_nor_accepts_it() {
        let mut d = PermissionResponseDialog::new("test", None);
        assert_eq!(d.choices.len(), 2);
        assert!(matches!(
            d.handle_key(key(KeyCode::Char('A'))),
            DialogResult::Continue
        ));
        d.handle_key(key(KeyCode::Right));
        assert!(matches!(
            d.handle_key(key(KeyCode::Enter)),
            DialogResult::Submit(PermissionResponseChoice::Deny)
        ));
    }

    #[test]
    fn the_focused_choice_stays_legible_on_every_builtin_background() {
        // It paints a foreground on the dialog's own background, so its color
        // has to be a foreground token: every builtin's `accent` clears the
        // ratio, while a background surface token such as `theme.selection`
        // lands between 1.10:1 and 1.71:1 and would fail here.
        for name in crate::tui::styles::builtin_theme_names() {
            let theme = crate::tui::styles::load_theme(name);
            let style = focused_choice_style(&theme);
            let fg = style.fg.expect("focused choice must set a foreground");
            assert!(
                crate::tui::styles::has_min_contrast(
                    fg,
                    theme.background,
                    MIN_FOCUSED_CONTRAST_RATIO
                ),
                "{name}: focused choice fg is illegible on the theme background"
            );
            assert_eq!(style.bg, None, "{name}: focused choice must not set a bg");
            assert!(
                style.add_modifier.contains(ratatui::style::Modifier::BOLD),
                "{name}: focused choice must be bold"
            );
            assert_ne!(
                unfocused_choice_style(&theme).fg,
                style.fg,
                "{name}: focused and unfocused choices must differ"
            );
        }

        // A custom theme's `accent` is not contrast-checked on load, so one
        // too close to the background falls back to `theme.text`.
        let mut theme = crate::tui::styles::load_theme_with_mode("empire", false);
        theme.accent = theme.background;
        assert_eq!(focused_choice_style(&theme).fg, Some(theme.text));
    }

    #[test]
    fn the_approval_context_survives_labels_too_long_for_the_dialog() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = crate::tui::styles::load_theme_with_mode("empire", false);
        let long_title = "session-one ".repeat(30);
        let long_tool = "Bash".repeat(30);
        let long_target = format!("rm -rf build/{}", "deep/".repeat(40));
        for (title, tool, target) in [
            ("session-one", "Bash", "rm -rf build"),
            (long_title.as_str(), "Bash", long_target.as_str()),
            ("session-one", long_tool.as_str(), "rm -rf build"),
        ] {
            let mut dialog = PermissionResponseDialog::structured(title, tool, target, true);
            let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
            terminal
                .draw(|f| dialog.render(f, f.area(), &theme))
                .unwrap();
            let out: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            for context in [
                "session-one",
                "Bash",
                "rm -rf build",
                "destructive",
                "[Allow]",
            ] {
                assert!(out.contains(context), "missing {context:?}:\n{out}");
            }
        }
    }
}
