//! Rendering for the settings view

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, Padding, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState,
    },
    Frame,
};
use tui_input::Input;
use unicode_width::UnicodeWidthStr;

use super::{
    CategoryRow, FieldValue, SettingsCategory, SettingsFocus, SettingsScope, SettingsView,
};
use crate::tui::components::set_input_cursor_position;
use crate::tui::styles::Theme;

/// Paint a hover background over `area` by mutating the buffer cell by
/// cell. Preserves each cell's existing fg / modifiers so previously
/// rendered text stays readable; only the bg gets overwritten. Using
/// this instead of `Style::default().bg(...)` on the underlying widget
/// avoids fighting per-row styles (ratatui widget styles cascade in ways
/// that are hard to predict for things like List rows or Paragraph
/// spans), so the hover overlay reliably shows where a click will land.
fn paint_hover_bg(frame: &mut Frame, area: Rect, bg: Color) {
    let buf = frame.buffer_mut();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_bg(bg);
            }
        }
    }
}

/// Detect if we're running over SSH
fn is_ssh_session() -> bool {
    std::env::var("SSH_CONNECTION").is_ok()
        || std::env::var("SSH_CLIENT").is_ok()
        || std::env::var("SSH_TTY").is_ok()
}

/// Word-wrap `text` to a maximum display width, collapsing runs of
/// whitespace so the multi-line `\`-continued descriptions in
/// `fields.rs` (which preserve indentation on each source line) render
/// without runs of extra spaces. Returns at least one line so callers
/// can use `lines.len()` as a height directly. A word wider than
/// `width` is left on its own line and will overflow; descriptions are
/// natural prose so this isn't a real-world case.
pub(super) fn wrap_description_lines(text: &str, width: u16) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    if width == 0 {
        return vec![text.to_string()];
    }
    let max_width = width as usize;
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_w = 0usize;
    for word in text.split_whitespace() {
        let w = word.width();
        if current.is_empty() {
            current.push_str(word);
            current_w = w;
        } else if current_w + 1 + w <= max_width {
            current.push(' ');
            current.push_str(word);
            current_w += 1 + w;
        } else {
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
            current_w = w;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Allocation-free twin of [`wrap_description_lines`]: walks the same
/// greedy word-wrap and returns the line count. Used by
/// `field_height`, which runs in the render hot path once per field
/// and only cares about the row count, not the wrapped text.
pub(super) fn wrap_description_height(text: &str, width: u16) -> u16 {
    if text.is_empty() {
        return 0;
    }
    if width == 0 {
        return 1;
    }
    let max_width = width as usize;
    let mut lines: u16 = 0;
    let mut current_w: usize = 0;
    for word in text.split_whitespace() {
        let w = word.width();
        if current_w == 0 {
            current_w = w;
        } else if current_w + 1 + w <= max_width {
            current_w += 1 + w;
        } else {
            lines = lines.saturating_add(1);
            current_w = w;
        }
    }
    if current_w > 0 {
        lines = lines.saturating_add(1);
    }
    lines.max(1)
}

impl SettingsView {
    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        // Rebuilt every frame: scope tabs, category rows, and visible
        // field rows all shift when the layout changes (scope switch,
        // category resort, scroll), so stale rects from the prior
        // frame would point at the wrong cells.
        self.scope_tab_rects.clear();
        self.category_rects.clear();
        self.field_rects.clear();

        // Clear the area
        frame.render_widget(Clear, area);

        // Main layout: title bar, content, footer
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Title/tabs
                Constraint::Min(10),   // Content
                Constraint::Length(3), // Footer/help
            ])
            .split(area);

        self.render_header(frame, layout[0], theme);
        self.render_content(frame, layout[1], theme);
        self.render_footer(frame, layout[2], theme);

        // Render custom instruction dialog overlay if active
        if let Some(ref dialog) = self.custom_instruction_dialog {
            dialog.render(frame, area, theme);
        }

        // Render help overlay on top
        if self.show_help {
            self.render_help_overlay(frame, area, theme);
        }

        // Render the search overlay last so it sits above every other
        // surface (help, dialogs, etc.). The input handler already
        // gates other key dispatch on `search_input.is_some()`, but
        // painting last makes that gate visible too.
        if self.search_input.is_some() {
            self.render_search_overlay(frame, area, theme);
        }
    }

    fn render_header(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(theme.border));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let modified = if self.has_changes { " *" } else { "" };

        let scope_style = |scope: SettingsScope| -> Style {
            if self.scope == scope {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.dimmed)
            }
        };

        let global_style = scope_style(SettingsScope::Global);
        let profile_style = scope_style(SettingsScope::Profile);

        let profile_label =
            if self.scope == SettingsScope::Profile && self.available_profiles.len() > 1 {
                format!("Profile: {} {}/{}", self.profile, "{", "}")
            } else {
                format!("Profile: {}", self.profile)
            };

        // Pre-compute the rect for each `[ <Scope> ]` chip so clicks can
        // switch scope. The widths must stay in sync with the spans
        // pushed just below; the layout is deterministic enough to
        // mirror it inline without re-querying the paragraph.
        let chip_y = inner.y;
        let chip_height: u16 = 1;
        let global_chip_width: u16 = 2 + 6 + 2; // "[ Global ]"
        let profile_chip_width: u16 = 2 + profile_label.chars().count() as u16 + 2;
        let repo_chip_width: u16 = 2 + 4 + 2;
        let prefix_width: u16 =
            ("  Settings".chars().count() + modified.chars().count() + 4) as u16;
        let global_x = inner.x.saturating_add(prefix_width);
        let profile_x = global_x.saturating_add(global_chip_width).saturating_add(2);
        let repo_x = profile_x
            .saturating_add(profile_chip_width)
            .saturating_add(2);

        self.scope_tab_rects.push((
            SettingsScope::Global,
            Rect::new(global_x, chip_y, global_chip_width, chip_height),
        ));
        self.scope_tab_rects.push((
            SettingsScope::Profile,
            Rect::new(profile_x, chip_y, profile_chip_width, chip_height),
        ));

        let mut spans = vec![
            Span::styled("  Settings", Style::default().fg(theme.text)),
            Span::styled(modified, Style::default().fg(theme.error)),
            Span::raw("    "),
            Span::styled("[ ", Style::default().fg(theme.border)),
            Span::styled("Global", global_style),
            Span::styled(" ]", Style::default().fg(theme.border)),
            Span::raw("  "),
            Span::styled("[ ", Style::default().fg(theme.border)),
            Span::styled(profile_label, profile_style),
            Span::styled(" ]", Style::default().fg(theme.border)),
        ];

        if self.project_path.is_some() {
            let repo_style = scope_style(SettingsScope::Repo);
            spans.push(Span::raw("  "));
            spans.push(Span::styled("[ ", Style::default().fg(theme.border)));
            spans.push(Span::styled("Repo", repo_style));
            spans.push(Span::styled(" ]", Style::default().fg(theme.border)));
            self.scope_tab_rects.push((
                SettingsScope::Repo,
                Rect::new(repo_x, chip_y, repo_chip_width, chip_height),
            ));
        }

        frame.render_widget(Paragraph::new(Line::from(spans)), inner);

        // Hover overlay: paint a dim bg over the chip the mouse is on,
        // unless it's already the active scope (whose accent fg is its
        // own indicator). Resolved after the paragraph paints so the
        // chip text remains readable on top.
        if let Some(scope) = self.hovered_scope() {
            if scope != self.scope {
                if let Some((_, rect)) = self
                    .scope_tab_rects
                    .iter()
                    .find(|(s, _)| *s == scope)
                    .copied()
                {
                    paint_hover_bg(frame, rect, theme.selection);
                }
            }
        }
    }

    fn render_content(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        // Split into categories (left) and fields (right)
        let layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(20), // Categories
                Constraint::Min(40),    // Fields
            ])
            .split(area);

        self.render_categories(frame, layout[0], theme);
        self.render_fields(frame, layout[1], theme);
    }

    fn render_categories(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let is_focused = self.focus == SettingsFocus::Categories;

        let border_style = if is_focused {
            Style::default().fg(theme.accent)
        } else {
            Style::default().fg(theme.border)
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(border_style)
            .padding(Padding::horizontal(1));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Categories panel: sections render as dimmed, non-selectable
        // dividers; tabs render with the existing "> "/"  " prefix and
        // selection highlight. The first tab in each section is
        // visually indented by the prefix already; sections take the
        // same horizontal slot so the eye reads the group label as a
        // heading above the tabs that follow.
        let items: Vec<ListItem> = self
            .categories
            .iter()
            .enumerate()
            .map(|(i, row)| match row {
                CategoryRow::Section(label) => {
                    // Bumped from `theme.dimmed` to `theme.text` so the
                    // section dividers read as headings rather than as
                    // faded background. Bold helps them anchor the
                    // group visually without competing with the accent
                    // color used for the active tab.
                    let style = Style::default().fg(theme.text).add_modifier(Modifier::BOLD);
                    ListItem::new(*label).style(style)
                }
                CategoryRow::Tab(cat) => {
                    let style = if i == self.selected_category {
                        if is_focused {
                            Style::default()
                                .fg(theme.accent)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(theme.text)
                        }
                    } else {
                        Style::default().fg(theme.dimmed)
                    };
                    let prefix = if i == self.selected_category {
                        "> "
                    } else {
                        "  "
                    };
                    ListItem::new(format!("{}{}", prefix, cat.label())).style(style)
                }
            })
            .collect();

        // Capture hit rect per Tab row (Section dividers are skipped).
        // The List renders rows top-down starting at `inner.y`. We
        // mirror that layout here so each rect points at the same row
        // the user sees.
        for (i, row) in self.categories.iter().enumerate() {
            if matches!(row, CategoryRow::Tab(_)) && (i as u16) < inner.height {
                self.category_rects
                    .push((i, Rect::new(inner.x, inner.y + i as u16, inner.width, 1)));
            }
        }

        let list = List::new(items);
        frame.render_widget(list, inner);

        // Hover overlay: dim bg on whichever category row the mouse
        // sits over, suppressed when that row is already the selected
        // category (selection wins, same rule as the sidebar).
        if let Some(idx) = self.hovered_category() {
            if idx != self.selected_category {
                if let Some((_, rect)) =
                    self.category_rects.iter().find(|(i, _)| *i == idx).copied()
                {
                    paint_hover_bg(frame, rect, theme.selection);
                }
            }
        }
    }

    fn render_fields(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let is_focused = self.focus == SettingsFocus::Fields;

        let border_style = if is_focused {
            Style::default().fg(theme.accent)
        } else {
            Style::default().fg(theme.border)
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(border_style)
            .padding(Padding::new(1, 1, 0, 0));

        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.fields_content_width = inner.width;

        if self.fields.is_empty() {
            let msg = if self.scope == SettingsScope::Repo {
                "No repo-level settings for this category"
            } else {
                "No settings in this category"
            };
            let msg = Paragraph::new(msg).style(Style::default().fg(theme.dimmed));
            frame.render_widget(msg, inner);
            return;
        }

        // Show SSH warning for Sound category
        let current_category = self.current_category();
        let warning_offset = if current_category == SettingsCategory::Sound && is_ssh_session() {
            let warning = vec![
                Line::from(vec![
                    Span::styled("⚠ ", Style::default().fg(theme.waiting)),
                    Span::styled(
                        "Warning: Audio playback doesn't work over SSH",
                        Style::default().fg(theme.waiting),
                    ),
                ]),
                Line::from(vec![Span::styled(
                    "  Sounds require local terminal with audio output.",
                    Style::default().fg(theme.dimmed),
                )]),
                Line::from(""),
            ];
            let warning_widget = Paragraph::new(warning);
            let warning_area = Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: 3,
            };
            frame.render_widget(warning_widget, warning_area);
            3u16
        } else {
            0u16
        };

        // Reserve space for messages at the bottom
        let has_message = self.error_message.is_some() || self.success_message.is_some();
        let message_height: u16 = if has_message { 2 } else { 0 };
        let fields_viewport_height = inner
            .height
            .saturating_sub(message_height)
            .saturating_sub(warning_offset);
        self.fields_viewport_height = fields_viewport_height;

        // Calculate total content height
        let mut total_content_height = 0u16;
        for (i, field) in self.fields.iter().enumerate() {
            if i > 0 {
                total_content_height += 1; // spacing between fields
            }
            total_content_height += self.field_height(field, i);
        }

        let scroll_offset = self.fields_scroll_offset;

        // Render fields with scroll offset applied
        let mut y_pos = 0u16; // absolute position in content space
        for (i, field) in self.fields.iter().enumerate() {
            let field_h = self.field_height(field, i);
            let field_top = y_pos;
            let field_bottom = y_pos + field_h;

            // Skip fields entirely above the viewport
            if field_bottom <= scroll_offset {
                y_pos += field_h + 1;
                continue;
            }

            // Stop if we're past the viewport
            if field_top >= scroll_offset + fields_viewport_height {
                break;
            }

            let visible_y = field_top.saturating_sub(scroll_offset);
            let is_selected = i == self.selected_field && is_focused;
            let field_area = Rect {
                x: inner.x,
                y: inner.y + visible_y + warning_offset,
                width: inner.width,
                height: field_h.min(fields_viewport_height.saturating_sub(visible_y)),
            };

            self.render_field(frame, field_area, field, i, is_selected, theme);
            // SectionHeader rows are non-interactive dividers; skipping
            // them matches the keyboard navigation that hops over them.
            if !matches!(field.value, FieldValue::SectionHeader) {
                self.field_rects.push((i, field_area));
            }
            y_pos += field_h + 1; // +1 for spacing
        }

        // Hover overlay: dim bg on whichever field the mouse sits over.
        // Suppressed when that field is the selected one; the selected
        // styling is already brighter and should win. Routed after the
        // whole field loop so SectionHeader rows can't bleed an
        // overlay on themselves (they never make it into field_rects).
        if let Some(idx) = self.hovered_field() {
            let suppress = is_focused && idx == self.selected_field;
            if !suppress {
                if let Some((_, rect)) = self.field_rects.iter().find(|(i, _)| *i == idx).copied() {
                    paint_hover_bg(frame, rect, theme.selection);
                }
            }
        }

        // Render scrollbar if content overflows
        if total_content_height > fields_viewport_height {
            let scrollbar_area = Rect {
                x: area.x + area.width - 1,
                y: area.y + 1,
                width: 1,
                height: area.height.saturating_sub(2),
            };

            let mut scrollbar_state = ScrollbarState::new(
                total_content_height.saturating_sub(fields_viewport_height) as usize,
            )
            .position(scroll_offset as usize);

            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .track_style(Style::default().fg(theme.border))
                    .thumb_style(Style::default().fg(theme.dimmed)),
                scrollbar_area,
                &mut scrollbar_state,
            );
        }

        // Render messages at the bottom if present
        if let Some(ref error) = self.error_message {
            let msg_area = Rect {
                x: inner.x,
                y: inner.y + inner.height.saturating_sub(2),
                width: inner.width,
                height: 1,
            };
            let msg = Paragraph::new(error.as_str()).style(Style::default().fg(theme.error));
            frame.render_widget(msg, msg_area);
        } else if let Some(ref success) = self.success_message {
            let msg_area = Rect {
                x: inner.x,
                y: inner.y + inner.height.saturating_sub(2),
                width: inner.width,
                height: 1,
            };
            let msg = Paragraph::new(success.as_str()).style(Style::default().fg(theme.running));
            frame.render_widget(msg, msg_area);
        }
    }

    pub(super) fn field_height(&self, field: &super::SettingField, index: usize) -> u16 {
        let desc_height = self.description_height(&field.description);
        match &field.value {
            FieldValue::SectionHeader => {
                // heading line + dimmed subtitle (wrapped). No value row.
                1 + desc_height
            }
            FieldValue::List(items)
                if self.list_edit_state.is_some() && index == self.selected_field =>
            {
                // label + description + header + items + add prompt
                1 + desc_height + 1 + items.len() as u16 + 1
            }
            _ => 1 + desc_height + 1, // Label + description + value/summary
        }
    }

    /// Height in rows of a field's description after word-wrapping to
    /// the fields panel width. Empty descriptions reserve zero rows so
    /// section headers without a subtitle don't waste a blank line.
    pub(super) fn description_height(&self, description: &str) -> u16 {
        wrap_description_height(description, self.fields_content_width.max(1))
    }

    fn render_field(
        &self,
        frame: &mut Frame,
        area: Rect,
        field: &super::SettingField,
        index: usize,
        is_selected: bool,
        theme: &Theme,
    ) {
        // Section headers are non-interactive group dividers (e.g.
        // "Advanced" inside Acp). Render as a styled heading with
        // a dimmed subtitle. They never appear "selected" because the
        // input handler skips navigation past them. Label uses
        // `theme.text` (not dimmed) so it matches the categories-panel
        // section dividers and reads as a heading rather than fading
        // into the background.
        if matches!(field.value, FieldValue::SectionHeader) {
            let heading = Line::from(vec![
                Span::styled("── ", Style::default().fg(theme.border)),
                Span::styled(
                    field.label.clone(),
                    Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" ──", Style::default().fg(theme.border)),
            ]);
            frame.render_widget(Paragraph::new(heading), area);
            if !field.description.is_empty() {
                let wrapped = wrap_description_lines(&field.description, area.width);
                let subtitle_area = Rect {
                    x: area.x,
                    y: area.y + 1,
                    width: area.width,
                    height: wrapped.len() as u16,
                };
                let lines: Vec<Line> = wrapped
                    .into_iter()
                    .map(|line| Line::from(Span::styled(line, Style::default().fg(theme.dimmed))))
                    .collect();
                frame.render_widget(Paragraph::new(lines), subtitle_area);
            }
            return;
        }

        let label_style = if is_selected {
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text)
        };

        let override_indicator = if field.has_override && self.scope != SettingsScope::Global {
            if let Some(ref inherited) = field.inherited_display {
                Span::styled(
                    format!(" (override, inherits: {})", inherited),
                    Style::default().fg(theme.accent),
                )
            } else {
                Span::styled(" (override)", Style::default().fg(theme.accent))
            }
        } else {
            Span::raw("")
        };

        let label = Line::from(vec![
            Span::styled(field.label.clone(), label_style),
            override_indicator,
        ]);

        frame.render_widget(Paragraph::new(label), area);

        let wrapped_desc = wrap_description_lines(&field.description, area.width);
        let desc_height = wrapped_desc.len() as u16;
        let description_area = Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: desc_height,
        };
        let desc_lines: Vec<Line> = wrapped_desc
            .into_iter()
            .map(|line| Line::from(Span::styled(line, Style::default().fg(theme.dimmed))))
            .collect();
        frame.render_widget(Paragraph::new(desc_lines), description_area);

        // Inner value renderers paint at `value_area.y + 1`, so shift
        // by the wrapped description height to keep the value aligned
        // directly under the (potentially multi-line) description.
        let value_area = Rect {
            y: area.y + desc_height,
            ..area
        };

        match &field.value {
            FieldValue::Bool(value) => {
                self.render_bool_field(frame, value_area, *value, is_selected, theme);
            }
            FieldValue::Text(value) => {
                self.render_text_field(frame, value_area, value, index, is_selected, theme);
            }
            FieldValue::OptionalText(value) => {
                let display = match value.as_deref() {
                    Some(text) if field.is_custom_instruction() => {
                        let collapsed: String = text
                            .chars()
                            .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
                            .collect();
                        if collapsed.len() > 47 {
                            format!("{}...", &collapsed[..47])
                        } else {
                            collapsed
                        }
                    }
                    Some(text) => text.to_string(),
                    None => String::new(),
                };
                self.render_text_field(frame, value_area, &display, index, is_selected, theme);
            }
            FieldValue::Number(value) => {
                self.render_number_field(frame, value_area, *value, index, is_selected, theme);
            }
            FieldValue::Select { selected, options } => {
                self.render_select_field(frame, value_area, *selected, options, is_selected, theme);
            }
            FieldValue::List(items) => {
                self.render_list_field(frame, value_area, items, index, is_selected, theme);
            }
            FieldValue::SectionHeader => {
                // Already handled by the early return at the top of
                // render_field; reaching this arm would mean the early
                // return was bypassed, which is a programmer bug.
            }
        }
    }

    fn render_bool_field(
        &self,
        frame: &mut Frame,
        area: Rect,
        value: bool,
        is_selected: bool,
        theme: &Theme,
    ) {
        let value_area = Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: 1,
        };

        let checkbox = if value { "[x]" } else { "[ ]" };
        let style = if is_selected {
            Style::default().fg(theme.accent)
        } else {
            Style::default().fg(theme.dimmed)
        };

        let text = format!(
            "{} {}",
            checkbox,
            if value { "Enabled" } else { "Disabled" }
        );
        frame.render_widget(Paragraph::new(text).style(style), value_area);
    }

    fn render_text_field(
        &self,
        frame: &mut Frame,
        area: Rect,
        value: &str,
        index: usize,
        is_selected: bool,
        theme: &Theme,
    ) {
        let value_area = Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width.min(50),
            height: 1,
        };

        let is_editing = self.editing_input.is_some() && index == self.selected_field;

        if is_editing {
            // Render with inverse-video cursor
            let input = self.editing_input.as_ref().unwrap();
            self.render_input_with_cursor(frame, value_area, input, theme);
        } else {
            let style = if is_selected {
                Style::default().fg(theme.accent)
            } else {
                Style::default().fg(theme.dimmed)
            };

            let display = if value.is_empty() {
                "(empty)".to_string()
            } else {
                value.to_string()
            };

            frame.render_widget(Paragraph::new(display).style(style), value_area);
        }
    }

    /// Build spans for text with an inverse-video cursor at the given position
    fn build_cursor_spans(value: &str, cursor_pos: usize, theme: &Theme) -> Vec<Span<'static>> {
        let value_style = Style::default().fg(theme.accent);
        let cursor_style = Style::default().fg(theme.background).bg(theme.accent);

        let before: String = value.chars().take(cursor_pos).collect();
        let cursor_char: String = value
            .chars()
            .nth(cursor_pos)
            .map(|c| c.to_string())
            .unwrap_or_else(|| " ".to_string());
        let after: String = value.chars().skip(cursor_pos + 1).collect();

        let mut spans = Vec::new();
        if !before.is_empty() {
            spans.push(Span::styled(before, value_style));
        }
        spans.push(Span::styled(cursor_char, cursor_style));
        if !after.is_empty() {
            spans.push(Span::styled(after, value_style));
        }
        spans
    }

    /// Render an Input with inverse-video cursor styling
    fn render_input_with_cursor(
        &self,
        frame: &mut Frame,
        area: Rect,
        input: &Input,
        theme: &Theme,
    ) {
        let spans = Self::build_cursor_spans(input.value(), input.cursor(), theme);
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        if self.editing_cursor_visible() {
            set_input_cursor_position(frame, area, 0, input);
        }
    }

    /// Render a list item with prefix and inverse-video cursor
    fn render_list_item_with_cursor(
        &self,
        frame: &mut Frame,
        area: Rect,
        prefix: &str,
        input: &Input,
        theme: &Theme,
    ) {
        let value_style = Style::default().fg(theme.accent);
        let mut spans = vec![Span::styled(prefix.to_string(), value_style)];
        spans.extend(Self::build_cursor_spans(
            input.value(),
            input.cursor(),
            theme,
        ));
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
        if self.editing_cursor_visible() {
            set_input_cursor_position(frame, area, prefix.width(), input);
        }
    }

    fn editing_cursor_visible(&self) -> bool {
        self.custom_instruction_dialog.is_none() && !self.show_help
    }

    fn render_number_field(
        &self,
        frame: &mut Frame,
        area: Rect,
        value: u64,
        index: usize,
        is_selected: bool,
        theme: &Theme,
    ) {
        let value_area = Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width.min(20),
            height: 1,
        };

        let is_editing = self.editing_input.is_some() && index == self.selected_field;

        if is_editing {
            // Render with inverse-video cursor
            let input = self.editing_input.as_ref().unwrap();
            self.render_input_with_cursor(frame, value_area, input, theme);
        } else {
            let style = if is_selected {
                Style::default().fg(theme.accent)
            } else {
                Style::default().fg(theme.dimmed)
            };

            frame.render_widget(Paragraph::new(value.to_string()).style(style), value_area);
        }
    }

    fn render_select_field(
        &self,
        frame: &mut Frame,
        area: Rect,
        selected: usize,
        options: &[String],
        is_selected: bool,
        theme: &Theme,
    ) {
        let value_area = Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: 1,
        };

        let style = if is_selected {
            Style::default().fg(theme.accent)
        } else {
            Style::default().fg(theme.dimmed)
        };

        let display = options.get(selected).map(|s| s.as_str()).unwrap_or("?");
        let arrows = if is_selected { " < >" } else { "" };
        frame.render_widget(
            Paragraph::new(format!("{}{}", display, arrows)).style(style),
            value_area,
        );
    }

    fn render_list_field(
        &self,
        frame: &mut Frame,
        area: Rect,
        items: &[String],
        index: usize,
        is_selected: bool,
        theme: &Theme,
    ) {
        let is_expanded = self.list_edit_state.is_some() && index == self.selected_field;

        if !is_expanded {
            // Collapsed view - show count
            let value_area = Rect {
                x: area.x,
                y: area.y + 1,
                width: area.width,
                height: 1,
            };

            let style = if is_selected {
                Style::default().fg(theme.accent)
            } else {
                Style::default().fg(theme.dimmed)
            };

            let text = if items.is_empty() {
                "(empty)".to_string()
            } else {
                format!("[{} items]", items.len())
            };

            frame.render_widget(Paragraph::new(text).style(style), value_area);
        } else {
            // Expanded view - show all items
            let list_state = self.list_edit_state.as_ref().unwrap();

            let header_area = Rect {
                x: area.x,
                y: area.y + 1,
                width: area.width,
                height: 1,
            };

            let header = Line::from(vec![
                Span::styled("Items: ", Style::default().fg(theme.dimmed)),
                Span::styled(
                    "(a)dd (d)elete (Enter)edit (Esc)close",
                    Style::default().fg(theme.dimmed),
                ),
            ]);
            frame.render_widget(Paragraph::new(header), header_area);

            // Render items
            for (i, item) in items.iter().enumerate() {
                let item_y = area.y + 2 + i as u16;
                if item_y >= area.y + area.height {
                    break;
                }

                let item_area = Rect {
                    x: area.x + 2,
                    y: item_y,
                    width: area.width.saturating_sub(2),
                    height: 1,
                };

                let style = if i == list_state.selected_index {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.dimmed)
                };

                let prefix = if i == list_state.selected_index {
                    "> "
                } else {
                    "  "
                };

                // If editing this item (not adding new), render with cursor
                if let Some(input) = list_state
                    .editing_item
                    .as_ref()
                    .filter(|_| i == list_state.selected_index && !list_state.adding_new)
                {
                    self.render_list_item_with_cursor(frame, item_area, prefix, input, theme);
                } else {
                    let display = format!("{}{}", prefix, item);
                    frame.render_widget(Paragraph::new(display).style(style), item_area);
                }
            }

            // Show add prompt if adding new
            if list_state.adding_new {
                let add_y = area.y + 2 + items.len() as u16;
                if add_y < area.y + area.height {
                    let add_area = Rect {
                        x: area.x + 2,
                        y: add_y,
                        width: area.width.saturating_sub(2),
                        height: 1,
                    };

                    if let Some(input) = &list_state.editing_item {
                        self.render_list_item_with_cursor(frame, add_area, "> ", input, theme);
                    }
                }
            }
        }
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(theme.border));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let key_style = Style::default().fg(theme.accent);
        let desc_style = Style::default().fg(theme.dimmed);

        let spans: Vec<Span> = if self.custom_instruction_dialog.is_some() {
            vec![
                Span::styled("Tab", key_style),
                Span::styled(": focus  ", desc_style),
                Span::styled("Enter", key_style),
                Span::styled(": confirm  ", desc_style),
                Span::styled("Esc", key_style),
                Span::styled(": cancel", desc_style),
            ]
        } else if self.editing_input.is_some() {
            vec![
                Span::styled("Enter", key_style),
                Span::styled(": confirm  ", desc_style),
                Span::styled("Esc", key_style),
                Span::styled(": cancel", desc_style),
            ]
        } else if self.list_edit_state.is_some() {
            vec![
                Span::styled("a", key_style),
                Span::styled(": add  ", desc_style),
                Span::styled("d", key_style),
                Span::styled(": delete  ", desc_style),
                Span::styled("Enter", key_style),
                Span::styled(": edit  ", desc_style),
                Span::styled("Esc", key_style),
                Span::styled(": close list", desc_style),
            ]
        } else {
            let mut s: Vec<Span> = Vec::new();

            match self.focus {
                SettingsFocus::Categories => {
                    s.extend([
                        Span::styled("j/k", key_style),
                        Span::styled(": nav  ", desc_style),
                        Span::styled("Enter/Tab", key_style),
                        Span::styled(": fields  ", desc_style),
                    ]);
                }
                SettingsFocus::Fields => {
                    s.extend([
                        Span::styled("j/k", key_style),
                        Span::styled(": nav  ", desc_style),
                        Span::styled("Enter", key_style),
                        Span::styled(": edit  ", desc_style),
                        Span::styled("Space", key_style),
                        Span::styled(": toggle  ", desc_style),
                    ]);
                    // Show reset hint when on an override field in Profile/Repo scope
                    if self.scope != SettingsScope::Global
                        && !self.fields.is_empty()
                        && self.fields[self.selected_field].has_override
                    {
                        s.extend([
                            Span::styled("r", key_style),
                            Span::styled(": reset  ", desc_style),
                        ]);
                    }
                }
            }

            s.extend([
                Span::styled("[]", key_style),
                Span::styled(": scope  ", desc_style),
            ]);

            if self.scope == SettingsScope::Profile && self.available_profiles.len() > 1 {
                s.extend([
                    Span::styled("{}", key_style),
                    Span::styled(": profile  ", desc_style),
                ]);
            }

            s.extend([
                Span::styled("Ctrl+s", key_style),
                Span::styled(": save  ", desc_style),
                Span::styled("?", key_style),
                Span::styled(": help  ", desc_style),
                Span::styled("q", key_style),
                Span::styled(": close", desc_style),
            ]);

            s
        };

        let help = Paragraph::new(Line::from(spans)).alignment(ratatui::layout::Alignment::Center);

        frame.render_widget(help, inner);
    }

    fn render_help_overlay(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let dialog_width = 58u16;
        let dialog_height = 28u16;

        let x = area.x + (area.width.saturating_sub(dialog_width)) / 2;
        let y = area.y + (area.height.saturating_sub(dialog_height)) / 2;

        let dialog_area = Rect {
            x,
            y,
            width: dialog_width.min(area.width),
            height: dialog_height.min(area.height),
        };

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .style(Style::default().bg(theme.background))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.border))
            .title(" Settings Help ")
            .title_style(
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            );

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let shortcuts: Vec<(&str, Vec<(&str, &str)>)> = vec![
            (
                "Navigation",
                vec![
                    ("j/k, Up/Dn", "Move up / down"),
                    ("Tab, l/h", "Switch to fields / categories"),
                    ("Enter", "Edit field / expand list / select"),
                    ("Esc", "Back one level (fields -> categories -> close)"),
                ],
            ),
            (
                "Editing",
                vec![
                    ("Space", "Toggle boolean field"),
                    ("Enter/Esc", "Confirm / cancel text edit"),
                    ("r", "Reset field to inherited value (Profile/Repo)"),
                ],
            ),
            (
                "Scope & Profile",
                vec![
                    ("[ and ]", "Cycle scope (Global / Profile / Repo)"),
                    ("{ and }", "Cycle profile (in Profile scope)"),
                ],
            ),
            (
                "List Editing",
                vec![
                    ("a", "Add item"),
                    ("d", "Delete item"),
                    ("Enter", "Edit item"),
                    ("Esc", "Close list"),
                ],
            ),
            (
                "Other",
                vec![
                    ("/", "Search settings across all tabs"),
                    ("Ctrl+s", "Save settings"),
                    ("?", "Toggle this help"),
                    ("q", "Close settings"),
                ],
            ),
        ];

        let mut lines: Vec<Line> = Vec::new();

        for (section, keys) in shortcuts {
            lines.push(Line::from(Span::styled(
                section,
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )));
            for (key, desc) in keys {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {:14}", key), Style::default().fg(theme.waiting)),
                    Span::styled(desc, Style::default().fg(theme.text)),
                ]));
            }
            lines.push(Line::from(""));
        }

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }

    /// Render the settings-wide search overlay: a query input at the
    /// top, the matching hits below, each prefixed with their
    /// category label. Empty query lists every interactive field
    /// across every visible category so the user can browse the full
    /// catalog as a flat list.
    fn render_search_overlay(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let dialog_width = area.width.saturating_sub(8).clamp(40, 80);
        let dialog_height = area.height.saturating_sub(4).clamp(10, 24);

        let x = area.x + (area.width.saturating_sub(dialog_width)) / 2;
        let y = area.y + (area.height.saturating_sub(dialog_height)) / 2;
        let dialog_area = Rect {
            x,
            y,
            width: dialog_width.min(area.width),
            height: dialog_height.min(area.height),
        };

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .style(Style::default().bg(theme.background))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(" Search settings ")
            .title_style(
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            );
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        // Layout: input line, separator, hit list, footer hint.
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // input
                Constraint::Length(1), // separator
                Constraint::Min(3),    // hits
                Constraint::Length(1), // footer
            ])
            .split(inner);

        // Input row: "/ <query>"
        if let Some(input) = self.search_input.as_ref() {
            let prompt_span = Span::styled("/ ", Style::default().fg(theme.accent));
            let cursor_spans = Self::build_cursor_spans(input.value(), input.cursor(), theme);
            let mut spans = vec![prompt_span];
            spans.extend(cursor_spans);
            frame.render_widget(Paragraph::new(Line::from(spans)), layout[0]);
        }

        // Separator.
        frame.render_widget(
            Paragraph::new("─".repeat(layout[1].width as usize))
                .style(Style::default().fg(theme.border)),
            layout[1],
        );

        // Hits.
        if self.search_hits.is_empty() {
            let msg = if self
                .search_input
                .as_ref()
                .map(|i| i.value().is_empty())
                .unwrap_or(true)
            {
                "Type to search settings"
            } else {
                "No matching settings"
            };
            frame.render_widget(
                Paragraph::new(msg).style(Style::default().fg(theme.dimmed)),
                layout[2],
            );
        } else {
            let visible = layout[2].height as usize;
            let scroll_start = self
                .search_selected
                .saturating_sub(visible.saturating_sub(1));
            let mut lines: Vec<Line> = Vec::new();
            for (i, hit) in self
                .search_hits
                .iter()
                .enumerate()
                .skip(scroll_start)
                .take(visible)
            {
                let is_selected = i == self.search_selected;
                let prefix = if is_selected { "> " } else { "  " };
                let label_style = if is_selected {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.text)
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix, label_style),
                    Span::styled(
                        format!("[{}] ", hit.category_label),
                        Style::default().fg(theme.dimmed),
                    ),
                    Span::styled(hit.field_label.clone(), label_style),
                ]));
            }
            frame.render_widget(Paragraph::new(lines), layout[2]);
        }

        // Footer.
        let footer = Line::from(vec![
            Span::styled("Enter ", Style::default().fg(theme.waiting)),
            Span::styled("jump  ", Style::default().fg(theme.dimmed)),
            Span::styled("↑/↓ ", Style::default().fg(theme.waiting)),
            Span::styled("select  ", Style::default().fg(theme.dimmed)),
            Span::styled("Esc ", Style::default().fg(theme.waiting)),
            Span::styled("close", Style::default().fg(theme.dimmed)),
        ]);
        frame.render_widget(Paragraph::new(footer), layout[3]);
    }
}

#[cfg(test)]
mod tests {
    use super::{wrap_description_height, wrap_description_lines};

    #[test]
    fn wrap_description_lines_returns_empty_for_empty_input() {
        assert!(wrap_description_lines("", 40).is_empty());
    }

    #[test]
    fn wrap_description_lines_fits_short_text_on_one_line() {
        let lines = wrap_description_lines("short text", 40);
        assert_eq!(lines, vec!["short text".to_string()]);
    }

    #[test]
    fn wrap_description_lines_breaks_at_word_boundaries() {
        let lines = wrap_description_lines("one two three four", 8);
        // "one two" fits (7 chars), "three" needs new line, "four" fits with "three"
        assert_eq!(
            lines,
            vec![
                "one two".to_string(),
                "three".to_string(),
                "four".to_string(),
            ]
        );
    }

    #[test]
    fn wrap_description_lines_collapses_runs_of_whitespace() {
        // Mimics the multi-line `\`-continued descriptions in fields.rs
        // where the continuation indentation produces runs of spaces.
        let text = "hello      world      again";
        let lines = wrap_description_lines(text, 40);
        assert_eq!(lines, vec!["hello world again".to_string()]);
    }

    #[test]
    fn wrap_description_lines_handles_long_setting_description() {
        // Approximation of the Interaction tab description that
        // triggered the cutoff bug at narrow widths (issue #1551).
        let text = "What Enter (and double-click) does on a session row in \
                    the Structured view: attach to tmux (default, historical \
                    behavior) or enter live-send mode so the home list stays \
                    visible and keystrokes pipe through to the agent. \
                    Terminal/Tool views and structured-view sessions ignore this \
                    setting.";
        // At a 120-col-wide settings panel none of the wrapped lines
        // should exceed the available width.
        let lines = wrap_description_lines(text, 120);
        assert!(lines.len() > 1, "long text should wrap to multiple lines");
        for line in &lines {
            assert!(
                line.chars().count() <= 120,
                "wrapped line {line:?} exceeds width"
            );
        }
    }

    #[test]
    fn wrap_description_lines_zero_width_returns_single_line() {
        let lines = wrap_description_lines("anything", 0);
        assert_eq!(lines, vec!["anything".to_string()]);
    }

    /// `wrap_description_height` must agree with `wrap_description_lines().len()`
    /// for every input; it is the allocation-free shortcut the render hot
    /// path uses. If they ever drift, `field_height` will paint values on
    /// top of (or below) the description in real renders.
    #[test]
    fn wrap_description_height_matches_wrap_description_lines() {
        let cases: &[(&str, u16)] = &[
            ("", 40),
            ("short text", 40),
            ("one two three four", 8),
            ("hello      world      again", 40),
            ("anything", 0),
            (
                "What Enter (and double-click) does on a session row in \
                 the Structured view: attach to tmux (default, historical \
                 behavior) or enter live-send mode so the home list stays \
                 visible and keystrokes pipe through to the agent.",
                40,
            ),
        ];
        for (text, width) in cases {
            let expected = wrap_description_lines(text, *width).len() as u16;
            let actual = wrap_description_height(text, *width);
            assert_eq!(
                actual, expected,
                "height mismatch for text {text:?} width {width}"
            );
        }
    }
}

#[cfg(test)]
mod field_height_tests {
    use super::super::fields::FieldKind;
    use super::super::{FieldValue, SettingField, SettingsCategory, SettingsView};
    use crate::session::Storage;
    use serial_test::serial;
    use tempfile::TempDir;

    fn setup_test_home(temp: &TempDir) {
        std::env::set_var("HOME", temp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
    }

    fn fresh_view() -> (TempDir, SettingsView) {
        let temp = TempDir::new().unwrap();
        setup_test_home(&temp);
        let _ = Storage::new_unwatched("test").unwrap();
        let view = SettingsView::new("test", None).unwrap();
        (temp, view)
    }

    /// At a normal panel width, a short description fits on one row, so
    /// `field_height` returns the historical `1 + 1 + 1`. At a width
    /// narrow enough to force two wrap lines, the height grows by exactly
    /// the extra row. Locks the contract between `description_height`
    /// (consumed by the scroll math) and what the render pass paints.
    #[test]
    #[serial]
    fn field_height_grows_with_wrapped_description() {
        let (_temp, mut view) = fresh_view();

        let field = SettingField {
            kind: FieldKind::HostEnvironment,
            label: "Test Label".to_string(),
            description: "alpha beta gamma delta".to_string(),
            value: FieldValue::Bool(false),
            category: SettingsCategory::Interaction,
            has_override: false,
            inherited_display: None,
        };

        view.fields_content_width = 80;
        assert_eq!(
            view.field_height(&field, 0),
            3,
            "wide panel: label + 1-line desc + value"
        );

        // Width that fits "alpha beta" (10) but not "alpha beta gamma" (16),
        // forcing two wrap lines.
        view.fields_content_width = 12;
        assert_eq!(
            view.field_height(&field, 0),
            4,
            "narrow panel: label + 2-line desc + value"
        );
    }

    /// Section headers have no value row. When the subtitle wraps, the
    /// reported height must still match `1 + wrapped_subtitle_lines` so
    /// the surrounding scroll math doesn't drift.
    #[test]
    #[serial]
    fn field_height_section_header_tracks_wrapped_subtitle() {
        let (_temp, mut view) = fresh_view();

        let header = SettingField {
            kind: FieldKind::SectionMarker,
            label: "Section".to_string(),
            description: "alpha beta gamma delta".to_string(),
            value: FieldValue::SectionHeader,
            category: SettingsCategory::Acp,
            has_override: false,
            inherited_display: None,
        };

        view.fields_content_width = 80;
        assert_eq!(view.field_height(&header, 0), 2);

        view.fields_content_width = 12;
        assert_eq!(view.field_height(&header, 0), 3);
    }
}
