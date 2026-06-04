//! Shared checkbox-line builder for dialog rendering.
//!
//! `delete_options` and `group_delete_options` both render rows of `[x]` /
//! `[ ]` checkboxes with a focus highlight. The shape (indent, checkbox glyph,
//! label, optional `(detail)` suffix) is identical; only the highlight style
//! differs slightly between dialogs (some use `bold` on the box, some use
//! `underlined` on the label). The caller passes a [`CheckboxStyle`] to
//! choose, keeping per-dialog visual tweaks while sharing layout + glyph
//! logic.

use ratatui::prelude::*;

use crate::tui::styles::Theme;

/// Which part of a focused checkbox row receives the `underlined()` modifier.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UnderlineScope {
    /// Only the label text is underlined.
    Label,
    /// Both the box glyph and the label are underlined.
    Row,
}

/// How a focused/checked checkbox should be styled. Each dialog picks its
/// own combination so neither has to change.
#[derive(Clone, Copy)]
pub struct CheckboxStyle {
    /// Color for both the box glyph and label when the row is focused.
    pub focused_color: Color,
    /// Color for the box glyph when checked but not focused.
    pub checked_color: Color,
    /// `bold()` the box glyph.
    pub focused_glyph_bold: bool,
    /// Where the underline highlight goes, if anywhere.
    pub focused_underline: Option<UnderlineScope>,
}

impl CheckboxStyle {
    /// Style used by the per-session delete dialog: bold box, underlined label.
    pub fn delete_session(theme: &Theme) -> Self {
        Self {
            focused_color: theme.accent,
            checked_color: theme.error,
            focused_glyph_bold: true,
            focused_underline: Some(UnderlineScope::Label),
        }
    }

    /// Style used by the group-delete dialog: underline-the-row highlight.
    pub fn delete_group(theme: &Theme) -> Self {
        Self {
            focused_color: theme.error,
            checked_color: theme.error,
            focused_glyph_bold: false,
            focused_underline: Some(UnderlineScope::Row),
        }
    }

    /// Style used by the quit confirmation's "don't warn me again"
    /// checkbox: a single always-toggleable opt-in that reads as a normal
    /// control (accent when ticked), not a destructive one.
    pub fn confirm(theme: &Theme) -> Self {
        Self {
            focused_color: theme.accent,
            checked_color: theme.accent,
            focused_glyph_bold: false,
            focused_underline: None,
        }
    }
}

/// Build a single checkbox row.
///
/// `indent` is the number of leading spaces (use 0 / 4 / 8 to match the
/// existing dialogs).
pub fn checkbox_line(
    theme: &Theme,
    label: &str,
    detail: Option<&str>,
    indent: usize,
    checked: bool,
    focused: bool,
    style: CheckboxStyle,
) -> Line<'static> {
    let glyph = if checked { "[x]" } else { "[ ]" };

    let mut box_style = if focused {
        Style::default().fg(style.focused_color)
    } else if checked {
        Style::default().fg(style.checked_color)
    } else {
        Style::default().fg(theme.dimmed)
    };
    if focused && style.focused_glyph_bold {
        box_style = box_style.bold();
    }
    if focused && style.focused_underline == Some(UnderlineScope::Row) {
        box_style = box_style.underlined();
    }

    let mut label_style = if focused {
        Style::default().fg(style.focused_color)
    } else if checked {
        Style::default().fg(style.checked_color)
    } else {
        Style::default().fg(theme.text)
    };
    if focused && style.focused_underline.is_some() {
        label_style = label_style.underlined();
    }

    let mut spans = Vec::with_capacity(5);
    if indent > 0 {
        spans.push(Span::raw(" ".repeat(indent)));
    }
    spans.push(Span::styled(glyph.to_string(), box_style));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(label.to_string(), label_style));
    if let Some(d) = detail {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("({})", d),
            Style::default().fg(theme.dimmed),
        ));
    }
    Line::from(spans)
}
