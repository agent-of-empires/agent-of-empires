//! Shared text input rendering component

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use tui_input::Input;
use unicode_width::UnicodeWidthStr;

use crate::tui::styles::Theme;

/// Finds the longest common prefix among a set of strings.
pub fn longest_common_prefix(values: &[String]) -> String {
    if values.is_empty() {
        return String::new();
    }

    let mut prefix = values[0].clone();
    for value in &values[1..] {
        while !value.starts_with(&prefix) {
            if prefix.pop().is_none() {
                break;
            }
        }
        if prefix.is_empty() {
            break;
        }
    }
    prefix
}

/// Ghost text completion state for group name autocomplete.
///
/// Computes and stores a ghost suggestion based on the current input value
/// and a list of existing group names. The ghost text is shown as dimmed text
/// after the cursor and can be accepted with Right/End.
pub struct GroupGhostCompletion {
    input_snapshot: String,
    cursor_snapshot: usize,
    ghost_text: String,
}

impl GroupGhostCompletion {
    /// Compute a ghost completion for the given input against existing groups.
    /// Returns `None` if there is no matching suggestion.
    pub fn compute(input: &Input, existing_groups: &[String]) -> Option<Self> {
        if existing_groups.is_empty() {
            return None;
        }

        let value = input.value().to_string();
        if value.is_empty() {
            return None;
        }

        let char_len = value.chars().count();
        let cursor_char = input.cursor().min(char_len);

        // Only show ghost when cursor is at end of input
        if cursor_char < char_len {
            return None;
        }

        let mut matches: Vec<String> = existing_groups
            .iter()
            .filter(|g| g.starts_with(&value))
            .cloned()
            .collect();

        if matches.is_empty() {
            return None;
        }
        matches.sort();

        let ghost_text = if matches.len() == 1 {
            matches[0][value.len()..].to_string()
        } else {
            let common = longest_common_prefix(&matches);
            if common.len() > value.len() {
                common[value.len()..].to_string()
            } else {
                matches[0][value.len()..].to_string()
            }
        };

        if ghost_text.is_empty() {
            return None;
        }

        Some(Self {
            input_snapshot: value,
            cursor_snapshot: cursor_char,
            ghost_text,
        })
    }

    /// Try to accept the ghost text into the input. Returns the new input value
    /// if the ghost was still valid (not stale), or `None` if stale.
    pub fn accept(self, input: &Input) -> Option<String> {
        let value = input.value().to_string();
        let cursor_char = input.cursor().min(value.chars().count());

        // Staleness check
        if self.input_snapshot != value || self.cursor_snapshot != cursor_char {
            return None;
        }

        let mut new_value = value;
        new_value.push_str(&self.ghost_text);
        Some(new_value)
    }

    pub fn ghost_text(&self) -> &str {
        &self.ghost_text
    }
}

fn char_width(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Finds the first char index to render given a horizontal scroll offset
/// measured in display columns.
fn visible_char_start(value: &str, scroll: usize) -> usize {
    let mut col = 0;
    let mut start = 0;
    for (i, c) in value.chars().enumerate() {
        if col >= scroll {
            break;
        }
        col += char_width(c);
        start = i + 1;
    }
    start
}

/// Builds the spans for a focused, horizontally-scrolled text input: the text
/// before the cursor, a block-styled cursor cell, and the text after it.
///
/// Returns the spans plus whether the end of the input is within the viewport,
/// which callers use to decide whether to append ghost (autocomplete) text.
/// Pass `scroll` from `Input::visual_scroll(available_width.saturating_sub(1))`
/// so the cursor cell always has a reserved column even at end of input.
pub(crate) fn focused_input_spans(
    value: &str,
    cursor_pos: usize,
    scroll: usize,
    available_width: usize,
    value_style: Style,
    cursor_style: Style,
) -> (Vec<Span<'static>>, bool) {
    let visible_start = visible_char_start(value, scroll);

    let mut visible_col = 0;
    let mut before = String::new();
    let mut cursor_char = String::new();
    let mut after = String::new();
    let mut cursor_visible = false;
    let mut end_visible = true;

    for (i, c) in value.chars().enumerate().skip(visible_start) {
        let w = char_width(c);
        if visible_col + w > available_width {
            end_visible = false;
            break;
        }
        if i < cursor_pos {
            before.push(c);
        } else if i == cursor_pos {
            cursor_char = c.to_string();
            cursor_visible = true;
        } else {
            after.push(c);
        }
        visible_col += w;
    }

    // Cursor sitting past the last char renders as a blank cell. The reserved
    // column (see `scroll` note above) guarantees room for it.
    if cursor_pos >= value.chars().count() && !cursor_visible {
        cursor_char = " ".to_string();
        cursor_visible = true;
    }

    let mut spans = Vec::new();
    if !before.is_empty() {
        spans.push(Span::styled(before, value_style));
    }
    if cursor_visible {
        spans.push(Span::styled(cursor_char, cursor_style));
    }
    if !after.is_empty() {
        spans.push(Span::styled(after, value_style));
    }
    (spans, end_visible)
}

/// Returns the visible substring of `value` for a non-focused (or cursor-less)
/// input, clipped to `available_width` starting at `scroll`, plus whether the
/// end of the input fit within the viewport.
pub(crate) fn visible_slice(value: &str, scroll: usize, available_width: usize) -> (String, bool) {
    let visible_start = visible_char_start(value, scroll);
    let mut visible_col = 0;
    let mut out = String::new();
    let mut end_visible = true;
    for c in value.chars().skip(visible_start) {
        let w = char_width(c);
        if visible_col + w > available_width {
            end_visible = false;
            break;
        }
        out.push(c);
        visible_col += w;
    }
    (out, end_visible)
}

/// Horizontal scroll offset (in display columns) that keeps the cursor visible,
/// reserving one column for the cursor cell at end of input.
pub(crate) fn input_scroll(input: &Input, available_width: usize) -> usize {
    input.visual_scroll(available_width.saturating_sub(1))
}

/// Renders a text input field with a label and cursor.
///
/// When focused, displays an inverse-video cursor over the current character position.
/// When not focused, displays the value (or placeholder if empty).
pub fn render_text_field(
    frame: &mut Frame,
    area: Rect,
    label: &str,
    input: &Input,
    is_focused: bool,
    placeholder: Option<&str>,
    theme: &Theme,
) {
    render_text_field_with_ghost(
        frame,
        area,
        label,
        input,
        is_focused,
        placeholder,
        None,
        theme,
    );
}

/// Like `render_text_field` but with optional ghost (autocomplete) text.
/// If `ghost_text` is provided, it is rendered after the cursor in dimmed style.
/// Supports horizontal scrolling when text exceeds available width.
#[allow(clippy::too_many_arguments)]
pub fn render_text_field_with_ghost(
    frame: &mut Frame,
    area: Rect,
    label: &str,
    input: &Input,
    is_focused: bool,
    placeholder: Option<&str>,
    ghost_text: Option<&str>,
    theme: &Theme,
) {
    let label_style = if is_focused {
        Style::default().fg(theme.accent).underlined()
    } else {
        Style::default().fg(theme.text)
    };
    let value_style = if is_focused {
        Style::default().fg(theme.accent)
    } else {
        Style::default().fg(theme.text)
    };

    let value = input.value();
    let prefix_width = label.width() + 1; // label + space
    let available_width = area.width.saturating_sub(prefix_width as u16) as usize;

    let mut spans = vec![Span::styled(label, label_style), Span::raw(" ")];

    if value.is_empty() && !is_focused {
        if let Some(placeholder_text) = placeholder {
            spans.push(Span::styled(placeholder_text, value_style));
        }
    } else if is_focused {
        let scroll = input_scroll(input, available_width);
        let cursor_style = Style::default().fg(theme.background).bg(theme.accent);
        let (field_spans, end_visible) = focused_input_spans(
            value,
            input.cursor(),
            scroll,
            available_width,
            value_style,
            cursor_style,
        );
        spans.extend(field_spans);
        if end_visible {
            if let Some(ghost) = ghost_text {
                spans.push(Span::styled(ghost, Style::default().fg(theme.dimmed)));
            }
        }
    } else {
        let scroll = input_scroll(input, available_width);
        let (visible, _) = visible_slice(value, scroll, available_width);
        spans.push(Span::styled(visible, value_style));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);

    if is_focused {
        let prefix = format!("{label} ");
        set_prefixed_input_cursor_position(frame, area, &prefix, input);
    }
}

pub fn set_prefixed_input_cursor_position(
    frame: &mut Frame,
    area: Rect,
    prefix: &str,
    input: &Input,
) {
    set_input_cursor_position(frame, area, prefix.width(), input);
}

pub fn set_input_cursor_position(
    frame: &mut Frame,
    area: Rect,
    prefix_width: usize,
    input: &Input,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let available_width = area.width.saturating_sub(prefix_width as u16) as usize;
    let scroll = input_scroll(input, available_width);
    let visual_cursor = input.visual_cursor();
    let visible_cursor = visual_cursor.saturating_sub(scroll);

    let cursor_col = prefix_width.saturating_add(visible_cursor);
    let max_col = area.width.saturating_sub(1) as usize;
    let x = area.x.saturating_add(cursor_col.min(max_col) as u16);
    frame.set_cursor_position(Position::new(x, area.y));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::styles::load_theme;

    fn groups(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    // --- longest_common_prefix tests ---

    #[test]
    fn lcp_empty_input() {
        assert_eq!(longest_common_prefix(&[]), "");
    }

    #[test]
    fn lcp_single_value() {
        assert_eq!(longest_common_prefix(&groups(&["hello"])), "hello");
    }

    #[test]
    fn lcp_identical_values() {
        assert_eq!(longest_common_prefix(&groups(&["abc", "abc"])), "abc");
    }

    #[test]
    fn lcp_common_prefix() {
        assert_eq!(
            longest_common_prefix(&groups(&["work/api", "work/backend", "work/frontend"])),
            "work/"
        );
    }

    #[test]
    fn lcp_no_common_prefix() {
        assert_eq!(longest_common_prefix(&groups(&["alpha", "beta"])), "");
    }

    #[test]
    fn lcp_unicode() {
        assert_eq!(
            longest_common_prefix(&groups(&["cafe\u{0301}1", "cafe\u{0301}2"])),
            "cafe\u{0301}"
        );
    }

    #[test]
    fn lcp_one_is_prefix_of_another() {
        assert_eq!(
            longest_common_prefix(&groups(&["work", "work/frontend"])),
            "work"
        );
    }

    // --- GroupGhostCompletion tests ---

    #[test]
    fn ghost_no_groups() {
        let input = Input::new("w".to_string());
        assert!(GroupGhostCompletion::compute(&input, &[]).is_none());
    }

    #[test]
    fn ghost_empty_input() {
        let input = Input::default();
        let groups = groups(&["work"]);
        assert!(GroupGhostCompletion::compute(&input, &groups).is_none());
    }

    #[test]
    fn ghost_no_match() {
        let input = Input::new("z".to_string());
        let groups = groups(&["work", "personal"]);
        assert!(GroupGhostCompletion::compute(&input, &groups).is_none());
    }

    #[test]
    fn ghost_single_match() {
        let input = Input::new("per".to_string());
        let groups = groups(&["work", "personal"]);
        let ghost = GroupGhostCompletion::compute(&input, &groups).unwrap();
        assert_eq!(ghost.ghost_text(), "sonal");
    }

    #[test]
    fn ghost_multiple_matches_with_common_prefix() {
        let input = Input::new("w".to_string());
        let groups = groups(&["work/api", "work/backend"]);
        let ghost = GroupGhostCompletion::compute(&input, &groups).unwrap();
        assert_eq!(ghost.ghost_text(), "ork/");
    }

    #[test]
    fn ghost_multiple_matches_no_extra_common_prefix() {
        let input = Input::new("work/".to_string());
        let groups = groups(&["work/api", "work/backend"]);
        let ghost = GroupGhostCompletion::compute(&input, &groups).unwrap();
        // Common prefix is "work/" which equals input, so falls back to first sorted match
        assert_eq!(ghost.ghost_text(), "api");
    }

    #[test]
    fn ghost_exact_match_returns_none() {
        let input = Input::new("work".to_string());
        let groups = groups(&["work"]);
        // Ghost text would be empty since input == match
        assert!(GroupGhostCompletion::compute(&input, &groups).is_none());
    }

    #[test]
    fn ghost_case_sensitive() {
        let input = Input::new("W".to_string());
        let groups = groups(&["work"]);
        assert!(GroupGhostCompletion::compute(&input, &groups).is_none());
    }

    #[test]
    fn ghost_accept_valid() {
        let input = Input::new("per".to_string());
        let groups = groups(&["personal"]);
        let ghost = GroupGhostCompletion::compute(&input, &groups).unwrap();
        let result = ghost.accept(&input).unwrap();
        assert_eq!(result, "personal");
    }

    #[test]
    fn ghost_accept_stale_value() {
        let input = Input::new("per".to_string());
        let groups = groups(&["personal"]);
        let ghost = GroupGhostCompletion::compute(&input, &groups).unwrap();
        // Input changed after computing ghost
        let changed_input = Input::new("pers".to_string());
        assert!(ghost.accept(&changed_input).is_none());
    }

    #[test]
    fn focused_text_field_sets_terminal_cursor() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(40, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        let input = Input::new("hi".to_string());
        let theme = load_theme("empire");

        terminal
            .draw(|f| {
                render_text_field(
                    f,
                    Rect::new(2, 1, 30, 1),
                    "Name:",
                    &input,
                    true,
                    None,
                    &theme,
                );
            })
            .unwrap();

        terminal
            .backend_mut()
            .assert_cursor_position(Position::new(10, 1));
    }

    #[test]
    fn focused_text_field_uses_display_columns_for_wide_chars() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(40, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        let input = Input::new("你".to_string());
        let theme = load_theme("empire");

        terminal
            .draw(|f| {
                render_text_field(
                    f,
                    Rect::new(2, 1, 30, 1),
                    "Name:",
                    &input,
                    true,
                    None,
                    &theme,
                );
            })
            .unwrap();

        terminal
            .backend_mut()
            .assert_cursor_position(Position::new(10, 1));
    }

    // --- horizontal scrolling ---

    #[test]
    fn visible_char_start_skips_scrolled_columns() {
        assert_eq!(visible_char_start("abcdef", 0), 0);
        assert_eq!(visible_char_start("abcdef", 2), 2);
        // Wide chars are two columns each, so a 2-column scroll skips one char.
        assert_eq!(visible_char_start("你好世界", 2), 1);
        assert_eq!(visible_char_start("你好世界", 4), 2);
    }

    #[test]
    fn visible_slice_clips_and_reports_end_visibility() {
        let (s, end) = visible_slice("abcdefghij", 0, 5);
        assert_eq!(s, "abcde");
        assert!(!end, "end of a too-long value is not visible");

        let (s, end) = visible_slice("abc", 0, 5);
        assert_eq!(s, "abc");
        assert!(end, "short value fits entirely");

        // Scrolled to the tail: the end becomes visible again.
        let (s, end) = visible_slice("abcdefghij", 5, 5);
        assert_eq!(s, "fghij");
        assert!(end);
    }

    #[test]
    fn focused_spans_keep_cursor_visible_at_end_of_long_value() {
        let value = "0123456789"; // 10 single-width chars
        let available_width = 5;
        let input = Input::new(value.to_string()); // cursor at end
        let scroll = input_scroll(&input, available_width);

        let (spans, end_visible) = focused_input_spans(
            value,
            input.cursor(),
            scroll,
            available_width,
            Style::default(),
            Style::default(),
        );

        let rendered: String = spans.iter().map(|s| s.content.as_ref()).collect();
        // A column is reserved for the cursor, so the last char plus a blank
        // cursor cell are both visible and the whole window stays within width.
        assert!(end_visible);
        assert!(rendered.ends_with("9 "), "got {rendered:?}");
        assert!(rendered.width() <= available_width);
    }

    #[test]
    fn focused_spans_hide_end_when_cursor_in_middle() {
        let value = "0123456789";
        let available_width = 5;
        // Cursor near the start: the end of the value is scrolled off-screen.
        let input = Input::new(value.to_string()).with_cursor(1);
        let scroll = input_scroll(&input, available_width);

        let (_, end_visible) = focused_input_spans(
            value,
            input.cursor(),
            scroll,
            available_width,
            Style::default(),
            Style::default(),
        );

        assert!(
            !end_visible,
            "ghost text must stay hidden when end is clipped"
        );
    }

    fn row_text(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>, y: u16) -> String {
        let buf = terminal.backend().buffer();
        let area = *buf.area();
        let mut out = String::new();
        for x in area.x..area.x + area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out
    }

    #[test]
    fn long_value_scrolls_to_show_tail_and_keeps_cursor_in_field() {
        use ratatui::backend::{Backend, TestBackend};
        use ratatui::Terminal;

        let backend = TestBackend::new(20, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        // Label "Path:" + space = 6 cols; field width 16 → 10 cols for the value.
        let value = "/very/long/path/that/exceeds/the/field";
        let input = Input::new(value.to_string()); // cursor at end
        let theme = load_theme("empire");
        let field = Rect::new(0, 1, 16, 1);

        terminal
            .draw(|f| {
                render_text_field(f, field, "Path:", &input, true, None, &theme);
            })
            .unwrap();

        let row = row_text(&terminal, 1);
        assert!(
            row.contains("field"),
            "tail of the value should be visible, got {row:?}"
        );
        assert!(
            !row.contains("/very/long"),
            "head should be scrolled off, got {row:?}"
        );

        // The cursor must land inside the field, not stuck past the right edge.
        let pos = terminal.backend_mut().get_cursor_position().unwrap();
        assert!(
            pos.x < field.x + field.width,
            "cursor {} escaped the field right edge {}",
            pos.x,
            field.x + field.width
        );
    }
}
