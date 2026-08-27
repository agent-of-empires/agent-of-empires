//! Dialog for answering a session's pending permission prompt.
//!
//! Two paths share this dialog. For a terminal session it answers the CLI's
//! "Do you want to proceed?" style prompt by sending the exact keystrokes a
//! human would type, without attaching: AoE never parses pane content to
//! detect or validate the prompt (see `AgentDef.permission_response`); the
//! user has already seen it on the pane before opening this dialog. For a
//! structured (ACP) session it resolves a daemon-reported approval nonce
//! through ACP, and the body shows the tool, target, and destructive flag
//! from the daemon's projection so the user sees what they are answering
//! without entering the structured view. The Allow Always choice is offered
//! when the terminal agent's mapping has one, and always on the ACP path.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::tui::styles::{has_min_contrast, Theme};

/// WCAG AA-large threshold (matches `remote_home::render::selected_row_style`).
const FOCUSED_CHOICE_CONTRAST_RATIO: f32 = 3.0;

/// Style for the focused choice, backed by `theme.selection` instead of a
/// terminal-level `.reversed()` swap. Falls back to `theme.text` when
/// `theme.accent` wouldn't be legible against the selection background.
fn focused_choice_style(theme: &Theme) -> Style {
    let fg = if has_min_contrast(theme.accent, theme.selection, FOCUSED_CHOICE_CONTRAST_RATIO) {
        theme.accent
    } else {
        theme.text
    };
    Style::default().fg(fg).bg(theme.selection).bold()
}

/// Which choice the user picked; not every agent offers `AllowAlways`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionResponseChoice {
    Allow,
    AllowAlways,
    Deny,
}

/// What a structured (ACP) approval is asking to do, shown in the dialog body
/// so the user sees the tool and target without entering the structured view.
/// `None` for the terminal keystroke path, whose prompt the user has already
/// seen on the session pane.
struct StructuredApprovalDetail {
    /// One-line "tool: target" summary, or just the tool when no target.
    summary: String,
    destructive: bool,
}

pub struct PermissionResponseDialog {
    session_title: String,
    choices: Vec<(&'static str, PermissionResponseChoice)>,
    /// Index of the focused choice within `choices`.
    focused: usize,
    /// Whether `choices` includes `AllowAlways`; computed once in `new`
    /// instead of re-scanning `choices` from `handle_key` and `render`.
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

    /// Construct the shared dialog for a structured (ACP) approval, which
    /// always supports the protocol's AllowAlways decision. `tool_name` /
    /// `target` / `destructive` come from the daemon's pending-approval
    /// projection and are shown in the body so the user sees what they are
    /// answering without opening the structured view.
    pub fn structured(
        session_title: &str,
        tool_name: &str,
        target: &str,
        destructive: bool,
    ) -> Self {
        let summary = if target.is_empty() {
            tool_name.to_string()
        } else {
            format!("{tool_name}: {target}")
        };
        Self::build(
            session_title,
            true,
            Some(StructuredApprovalDetail {
                summary,
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
        let dialog_area = super::centered_rect(area, 56, 9);
        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(" Respond to Permission Prompt ")
            .title_style(Style::default().fg(theme.accent).bold());

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(inner);

        let mut header_lines = vec![Line::from(Span::styled(
            self.session_title.clone(),
            Style::default().fg(theme.title).bold(),
        ))];
        match &self.detail {
            // Structured (ACP) path: show what is being approved. Nothing is
            // sent as keystrokes here, so the terminal guidance would be wrong.
            Some(detail) => {
                let summary_color = if detail.destructive {
                    theme.error
                } else {
                    theme.text
                };
                let mut summary = detail.summary.clone();
                if detail.destructive {
                    summary.push_str("  · destructive");
                }
                header_lines.push(Line::from(Span::styled(
                    summary,
                    Style::default().fg(summary_color),
                )));
                header_lines.push(Line::from(Span::styled(
                    "Resolves the agent's pending permission request.",
                    Style::default().fg(theme.dimmed),
                )));
            }
            // Terminal path: the user has already seen the CLI prompt on the
            // pane; AoE just replays the agent's keystrokes.
            None => {
                header_lines.push(Line::from(Span::styled(
                    "AoE sends these as raw keystrokes; make sure the prompt is on screen.",
                    Style::default().fg(theme.dimmed),
                )));
            }
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
                Style::default().fg(theme.text)
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
    use crossterm::event::KeyModifiers;

    /// Non-empty stand-in for a real agent's `allow_always` mapping. `Some(&[])`
    /// would also satisfy `.is_some()` but reads as "supported with zero
    /// keystrokes", a footgun if copy-pasted into a real `AgentDef`.
    const ALLOW_ALWAYS: Option<&[crate::agents::KeyToken]> =
        Some(&[crate::agents::KeyToken::Literal("2")]);

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn enter_submits_focused_default_allow() {
        let mut dialog = PermissionResponseDialog::new("test", ALLOW_ALWAYS);
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            result,
            DialogResult::Submit(PermissionResponseChoice::Allow)
        ));
    }

    #[test]
    fn a_submits_allow_directly() {
        let mut dialog = PermissionResponseDialog::new("test", ALLOW_ALWAYS);
        let result = dialog.handle_key(key(KeyCode::Char('a')));
        assert!(matches!(
            result,
            DialogResult::Submit(PermissionResponseChoice::Allow)
        ));
    }

    #[test]
    fn shift_a_submits_allow_always_directly() {
        let mut dialog = PermissionResponseDialog::new("test", ALLOW_ALWAYS);
        let result = dialog.handle_key(key(KeyCode::Char('A')));
        assert!(matches!(
            result,
            DialogResult::Submit(PermissionResponseChoice::AllowAlways)
        ));
    }

    #[test]
    fn d_submits_deny_directly() {
        let mut dialog = PermissionResponseDialog::new("test", ALLOW_ALWAYS);
        let result = dialog.handle_key(key(KeyCode::Char('d')));
        assert!(matches!(
            result,
            DialogResult::Submit(PermissionResponseChoice::Deny)
        ));
    }

    #[test]
    fn right_cycles_focus_and_enter_submits_it() {
        let mut dialog = PermissionResponseDialog::new("test", ALLOW_ALWAYS);
        dialog.handle_key(key(KeyCode::Right));
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            result,
            DialogResult::Submit(PermissionResponseChoice::AllowAlways)
        ));
    }

    #[test]
    fn left_wraps_focus_backward() {
        let mut dialog = PermissionResponseDialog::new("test", ALLOW_ALWAYS);
        dialog.handle_key(key(KeyCode::Left));
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            result,
            DialogResult::Submit(PermissionResponseChoice::Deny)
        ));
    }

    #[test]
    fn esc_cancels() {
        let mut dialog = PermissionResponseDialog::new("test", ALLOW_ALWAYS);
        let result = dialog.handle_key(key(KeyCode::Esc));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn allow_always_unsupported_is_skipped_and_shift_a_is_noop() {
        let mut dialog = PermissionResponseDialog::new("test", None);
        assert_eq!(dialog.choices.len(), 2);
        let result = dialog.handle_key(key(KeyCode::Char('A')));
        assert!(matches!(result, DialogResult::Continue));
        dialog.handle_key(key(KeyCode::Right));
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            result,
            DialogResult::Submit(PermissionResponseChoice::Deny)
        ));
    }

    #[test]
    fn focused_choice_style_uses_themed_background_not_reversed() {
        let theme = crate::tui::styles::load_theme_with_mode("empire", false);

        let style = focused_choice_style(&theme);

        assert_eq!(style.bg, Some(theme.selection));
        assert!(!style
            .add_modifier
            .contains(ratatui::style::Modifier::REVERSED));
    }

    #[test]
    fn focused_choice_style_keeps_accent_when_contrast_is_sufficient() {
        let theme = crate::tui::styles::load_theme_with_mode("empire", false);

        let style = focused_choice_style(&theme);

        assert_eq!(style.fg, Some(theme.accent));
    }

    #[test]
    fn focused_choice_style_falls_back_to_text_for_low_contrast_accent() {
        let mut theme = crate::tui::styles::load_theme_with_mode("empire", false);
        theme.accent = theme.selection;

        let style = focused_choice_style(&theme);

        assert_eq!(style.fg, Some(theme.text));
    }

    #[test]
    fn structured_dialog_shows_target_and_destructive_not_keystroke_hint() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let theme = crate::tui::styles::load_theme_with_mode("empire", false);
        let mut dialog =
            PermissionResponseDialog::structured("session-one", "Bash", "rm -rf build", true);
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|f| dialog.render(f, f.area(), &theme))
            .unwrap();
        let out =
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .fold(String::new(), |mut acc, cell| {
                    acc.push_str(cell.symbol());
                    acc
                });
        // The body names the tool, the target, and the destructive flag, and
        // drops the terminal-only "raw keystrokes" guidance that is wrong here.
        assert!(out.contains("Bash: rm -rf build"), "target missing:\n{out}");
        assert!(
            out.contains("destructive"),
            "destructive marker missing:\n{out}"
        );
        assert!(
            !out.contains("raw keystrokes"),
            "structured dialog must not show the keystroke guidance:\n{out}"
        );
    }
}
