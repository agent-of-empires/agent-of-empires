//! Shared tool-configuration UI for the New and Restart session dialogs.
//!
//! Both modals let the user override the agent's launch command and append
//! extra args via a `Ctrl+P` overlay, and both annotate the Tool row with a
//! `(configured)` summary plus a contextual `Ctrl+P` hint. Centralizing the
//! span construction, overlay rendering, and key handling keeps the two
//! dialogs in lockstep; previously each carried its own divergent copy.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use super::{cycler::tool_lifecycle_spans, render_text_field};
use crate::tui::styles::Theme;

/// Tool-config overlay fields: command override, then extra args.
pub const TOOL_CONFIG_CMD: usize = 0;
pub const TOOL_CONFIG_ARGS: usize = 1;
const TOOL_CONFIG_FIELD_COUNT: usize = 2;

/// Trailing lifecycle and configuration spans appended to the Tool row.
/// Lifecycle status comes first so lower-priority configuration metadata cannot
/// clip it. The focused configuration hint is compact when a status is present.
pub fn tool_row_suffix_spans(
    tool: &str,
    has_config: bool,
    focused: bool,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let mut spans = tool_lifecycle_spans(tool, theme);
    let compact_config = !spans.is_empty();
    spans.extend(tool_config_suffix_spans(
        has_config,
        focused,
        compact_config,
        theme,
    ));
    spans
}

/// Configuration spans appended after higher-priority Tool row status.
fn tool_config_suffix_spans(
    has_config: bool,
    focused: bool,
    compact: bool,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if has_config {
        spans.push(Span::styled(
            "  (configured)",
            Style::default().fg(theme.dimmed),
        ));
    }
    if focused {
        spans.push(Span::styled(
            match (has_config, compact) {
                (true, true) => " Ctrl+P",
                (true, false) => "  Ctrl+P: edit",
                (false, _) => "  (Ctrl+P to configure)",
            },
            Style::default().fg(theme.dimmed),
        ));
    }
    spans
}

pub enum ToolConfigOutcome {
    Continue,
    Close,
}

/// Handle a key event for the tool-config overlay, editing the two inputs and
/// advancing `focused_field` in place. Enter/Esc request close; everything
/// else (besides Tab/arrows navigation) is fed to the focused input. Callers
/// that want a help binding should intercept it before delegating here.
pub fn handle_tool_config_key(
    key: KeyEvent,
    command_override: &mut Input,
    extra_args: &mut Input,
    focused_field: &mut usize,
) -> ToolConfigOutcome {
    match key.code {
        KeyCode::Esc | KeyCode::Enter => ToolConfigOutcome::Close,
        KeyCode::Tab | KeyCode::Down => {
            *focused_field = (*focused_field + 1) % TOOL_CONFIG_FIELD_COUNT;
            ToolConfigOutcome::Continue
        }
        KeyCode::BackTab | KeyCode::Up => {
            *focused_field = if *focused_field == 0 {
                TOOL_CONFIG_FIELD_COUNT - 1
            } else {
                *focused_field - 1
            };
            ToolConfigOutcome::Continue
        }
        _ => {
            match *focused_field {
                TOOL_CONFIG_CMD => {
                    command_override.handle_event(&crossterm::event::Event::Key(key));
                }
                TOOL_CONFIG_ARGS => {
                    extra_args.handle_event(&crossterm::event::Event::Key(key));
                }
                _ => {}
            }
            ToolConfigOutcome::Continue
        }
    }
}

/// Render the centered tool-config overlay (command override + extra args +
/// hints). Returns the per-field hit rects keyed by field index, so callers
/// that support mouse focus can store them; callers that don't can ignore the
/// return value.
pub fn render_tool_config_overlay(
    frame: &mut Frame,
    area: Rect,
    selected_tool: &str,
    command_override: &Input,
    extra_args: &Input,
    focused_field: usize,
    theme: &Theme,
) -> Vec<(usize, Rect)> {
    let dialog_width: u16 = 72;
    let constraints = [
        Constraint::Length(2), // Command override
        Constraint::Length(2), // Extra args
        Constraint::Min(1),    // Hints
    ];
    let fields_height: u16 = 2 + 2 + 1;
    let dialog_height = fields_height + 4;

    let title = format!(" Tool Configuration: {} ", selected_tool);
    let dialog_area = crate::tui::dialogs::centered_rect(area, dialog_width, dialog_height);
    frame.render_widget(Clear, dialog_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(title)
        .title_style(Style::default().fg(theme.title).bold());

    let inner = block.inner(dialog_area);
    frame.render_widget(block, dialog_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints(constraints)
        .split(inner);

    let cmd_placeholder = if focused_field == TOOL_CONFIG_CMD {
        Some("(replaces default binary)")
    } else if command_override.value().is_empty() {
        Some("(default)")
    } else {
        None
    };
    render_text_field(
        frame,
        chunks[0],
        "Command:",
        command_override,
        focused_field == TOOL_CONFIG_CMD,
        cmd_placeholder,
        theme,
    );

    let args_placeholder = if focused_field == TOOL_CONFIG_ARGS {
        Some("(e.g. --port 8080)")
    } else if extra_args.value().is_empty() {
        Some("(none)")
    } else {
        None
    };
    render_text_field(
        frame,
        chunks[1],
        "Extra Args:",
        extra_args,
        focused_field == TOOL_CONFIG_ARGS,
        args_placeholder,
        theme,
    );

    let hint_spans = vec![
        Span::styled("Tab", Style::default().fg(theme.hint)),
        Span::raw(" next  "),
        Span::styled("Enter", Style::default().fg(theme.hint)),
        Span::raw(" done  "),
        Span::styled("Esc", Style::default().fg(theme.hint)),
        Span::raw(" back"),
    ];
    frame.render_widget(Paragraph::new(Line::from(hint_spans)), chunks[2]);

    vec![(TOOL_CONFIG_CMD, chunks[0]), (TOOL_CONFIG_ARGS, chunks[1])]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contents(spans: &[Span<'static>]) -> Vec<String> {
        spans.iter().map(|s| s.content.to_string()).collect()
    }

    #[test]
    fn suffix_spans_cases() {
        let theme = Theme::default();
        // (configured, focused, compact) -> spans
        let cases: [((bool, bool, bool), &[&str]); 5] = [
            ((false, false, false), &[]),
            ((false, true, false), &["  (Ctrl+P to configure)"]),
            ((true, true, false), &["  (configured)", "  Ctrl+P: edit"]),
            ((true, true, true), &["  (configured)", " Ctrl+P"]),
            ((true, false, false), &["  (configured)"]),
        ];
        for ((configured, focused, compact), want) in cases {
            let spans = tool_config_suffix_spans(configured, focused, compact, &theme);
            assert_eq!(contents(&spans), want, "{configured} {focused} {compact}");
        }
    }

    #[test]
    fn keys_close_wrap_and_route_typing() {
        let mut cmd = Input::default();
        let mut args = Input::default();
        let mut field = TOOL_CONFIG_CMD;
        let press = |code, field: &mut usize, cmd: &mut Input, args: &mut Input| {
            handle_tool_config_key(KeyEvent::from(code), cmd, args, field)
        };
        for code in [KeyCode::Enter, KeyCode::Esc] {
            assert!(matches!(
                press(code, &mut field, &mut cmd, &mut args),
                ToolConfigOutcome::Close
            ));
        }
        press(KeyCode::Char('z'), &mut field, &mut cmd, &mut args);
        assert_eq!((cmd.value(), args.value()), ("z", ""));
        press(KeyCode::Tab, &mut field, &mut cmd, &mut args);
        assert_eq!(field, TOOL_CONFIG_ARGS);
        press(KeyCode::Char('q'), &mut field, &mut cmd, &mut args);
        assert_eq!((cmd.value(), args.value()), ("z", "q"));
        press(KeyCode::Tab, &mut field, &mut cmd, &mut args);
        assert_eq!(field, TOOL_CONFIG_CMD, "Tab wraps");
    }
}
