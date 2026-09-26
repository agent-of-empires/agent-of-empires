//! Rendering for the settings view

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
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
use crate::tui::components::hover::paint_hover_bg;
use crate::tui::components::{set_input_cursor_position, truncate_to_width};
use crate::tui::styles::Theme;

fn is_ssh_session() -> bool {
    std::env::var("SSH_CONNECTION").is_ok()
        || std::env::var("SSH_CLIENT").is_ok()
        || std::env::var("SSH_TTY").is_ok()
}

/// Word-wrap `text`, collapsing whitespace runs so the `\`-continued
/// descriptions in `fields.rs` do not render their source indentation.
/// Always returns at least one line, so callers can use it as a height.
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

/// Line count of [`wrap_description_lines`], used by `field_height`.
pub(super) fn wrap_description_height(text: &str, width: u16) -> u16 {
    wrap_description_lines(text, width).len() as u16
}

impl SettingsView {
    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        // Rebuilt every frame: a stale rect points at the wrong cell.
        self.scope_tab_rects.clear();
        self.category_rects.clear();
        self.field_rects.clear();
        self.search_hit_rows.clear();
        self.search_popup_area = Rect::default();
        // Repopulated below only when the panel overflows; a zero rect means
        // "no bar to grab".
        self.scrollbar_area = Rect::default();

        frame.render_widget(Clear, area);

        // The search bar always renders, placeholder when idle, so the
        // affordance is visible without knowing the hotkey.
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Title/tabs
                Constraint::Length(3), // Search bar
                Constraint::Min(10),   // Content
                Constraint::Length(3), // Footer/help
            ])
            .split(area);

        self.render_header(frame, layout[0], theme);
        self.search_bar_rect = layout[1];
        self.render_search_bar(frame, layout[1], theme);
        self.render_content(frame, layout[2], theme);
        self.render_footer(frame, layout[3], theme);

        if let Some(ref dialog) = self.custom_instruction_dialog {
            dialog.render(frame, area, theme);
        }

        if self.show_help {
            self.render_help_overlay(frame, area, theme);
        }

        // Painted last so it drops over the panels and any overlay beneath.
        if self.search_input.is_some() {
            let content_area = layout[2];
            self.render_search_dropdown(frame, layout[1], content_area, theme);
        }
    }

    /// The permanent search bar: a placeholder advertising `/` when idle, the
    /// query input with a right-aligned hit count when active.
    fn render_search_bar(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let Some(input) = self.search_input.as_ref() else {
            let block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme.border))
                .padding(Padding::horizontal(1));
            let inner = block.inner(area);
            frame.render_widget(block, area);
            frame.render_widget(
                Paragraph::new("Press / to search settings")
                    .style(Style::default().fg(theme.dimmed)),
                inner,
            );
            return;
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(" Search settings ")
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let count = self.search_hits.len();
        let count_text = if count == 1 {
            "1 match".to_string()
        } else {
            format!("{count} matches")
        };
        let count_w = count_text.width() as u16;

        let query_area = Rect {
            width: inner.width.saturating_sub(count_w + 2),
            ..inner
        };
        let prompt = Span::styled("/ ", Style::default().fg(theme.accent));
        let mut spans = vec![prompt];
        spans.extend(Self::build_cursor_spans(
            input.value(),
            input.cursor(),
            theme,
        ));
        frame.render_widget(Paragraph::new(Line::from(spans)), query_area);
        if self.editing_cursor_visible() {
            set_input_cursor_position(frame, query_area, "/ ".width(), input);
        }

        if inner.width > count_w {
            let count_area = Rect {
                x: inner.x + inner.width - count_w,
                width: count_w,
                ..inner
            };
            frame.render_widget(
                Paragraph::new(count_text).style(Style::default().fg(theme.dimmed)),
                count_area,
            );
        }
    }

    /// The ranked-hit dropdown under the search bar. Enter jumps to the
    /// highlighted hit in its category.
    fn render_search_dropdown(
        &mut self,
        frame: &mut Frame,
        bar_area: Rect,
        content_area: Rect,
        theme: &Theme,
    ) {
        let width = bar_area.width.saturating_sub(4).max(20);
        let x = bar_area.x + 2;
        let y = content_area.y;
        let height = (self.search_hits.len().max(1) as u16 + 2).min(content_area.height);
        let dialog_area = Rect {
            x,
            y,
            width,
            height,
        };
        self.search_popup_area = dialog_area;

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .style(Style::default().bg(theme.background))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .padding(Padding::horizontal(1));
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        if self.search_hits.is_empty() {
            frame.render_widget(
                Paragraph::new("No matching settings").style(Style::default().fg(theme.dimmed)),
                inner,
            );
            return;
        }

        let visible = inner.height as usize;
        let scroll_start = self
            .search_selected
            .saturating_sub(visible.saturating_sub(1));
        let mut lines: Vec<Line> = Vec::new();
        // Screen row per visible hit, for click and hover routing.
        let mut hit_rows: Vec<(u16, usize)> = Vec::new();
        for (i, hit) in self
            .search_hits
            .iter()
            .enumerate()
            .skip(scroll_start)
            .take(visible)
        {
            hit_rows.push((inner.y + lines.len() as u16, i));
            let is_selected = i == self.search_selected;
            let prefix = if is_selected { "> " } else { "  " };
            let label_style = if is_selected {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };
            let mut spans = vec![
                Span::styled(prefix, label_style),
                Span::styled(
                    format!("[{}] ", hit.category_label),
                    Style::default().fg(theme.dimmed),
                ),
                Span::styled(hit.field_label.clone(), label_style),
            ];
            if !hit.value_display.is_empty() {
                let used = 2 + hit.category_label.width() + 3 + hit.field_label.width();
                let budget = (inner.width as usize).saturating_sub(used + 2);
                if budget >= 4 {
                    let value = truncate_to_width(&hit.value_display, budget);
                    spans.push(Span::styled(
                        format!("  {value}"),
                        Style::default().fg(theme.dimmed),
                    ));
                }
            }
            lines.push(Line::from(spans));
        }
        self.search_hit_rows = hit_rows;
        frame.render_widget(Paragraph::new(lines), inner);
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

        // Rect per `[ <Scope> ]` chip so clicks can switch scope. Widths must
        // stay in sync with the spans pushed below.
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

        // Dim bg over the hovered chip, unless it is the active scope, whose
        // accent fg is its own indicator. After the paragraph, to stay legible.
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
        let layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(20), // Categories
                Constraint::Min(40),    // Fields
            ])
            .split(area);

        self.render_categories(frame, layout[0], theme);
        if self.current_category() == SettingsCategory::Plugins {
            let focused = self.focus == SettingsFocus::Fields;
            // Master-detail: the manager list on top, the selected plugin's
            // fields beneath. While the manager captures input (discover mode,
            // a popup) it owns the whole pane: those surfaces need the space.
            self.plugin_manager
                .set_has_settings_pane(!self.fields.is_empty());
            if self.fields.is_empty() || self.plugin_manager.captures_input() {
                self.plugin_manager
                    .render_inline(frame, layout[1], theme, focused);
            } else {
                let manager_height = self
                    .plugin_manager
                    .preferred_inline_height()
                    .min(layout[1].height / 2)
                    .max(5);
                let split = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(manager_height), Constraint::Min(3)])
                    .split(layout[1]);
                self.plugin_manager.render_inline(
                    frame,
                    split[0],
                    theme,
                    focused && !self.plugins_fields_focus,
                );
                let title = self
                    .plugin_manager
                    .selected()
                    .map(|p| format!(" {} settings ", p.name));
                self.render_fields(
                    frame,
                    split[1],
                    theme,
                    focused && self.plugins_fields_focus,
                    title.as_deref(),
                );
            }
        } else {
            self.render_fields(
                frame,
                layout[1],
                theme,
                self.focus == SettingsFocus::Fields,
                None,
            );
        }
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

        // Sections are non-selectable dividers sharing the tabs' horizontal
        // slot, so they read as headings above the tabs that follow.
        let items: Vec<ListItem> = self
            .categories
            .iter()
            .enumerate()
            .map(|(i, row)| match row {
                CategoryRow::Section(label) => {
                    // `theme.text`, not dimmed, so dividers read as headings
                    // without competing with the active tab's accent.
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

        // Hit rect per Tab row, mirroring the List's top-down layout from
        // `inner.y`. Section dividers are skipped.
        for (i, row) in self.categories.iter().enumerate() {
            if matches!(row, CategoryRow::Tab(_)) && (i as u16) < inner.height {
                self.category_rects
                    .push((i, Rect::new(inner.x, inner.y + i as u16, inner.width, 1)));
            }
        }

        let list = List::new(items);
        frame.render_widget(list, inner);

        // Dim bg on the hovered category row; selection wins over hover.
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

    fn render_fields(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        is_focused: bool,
        title: Option<&str>,
    ) {
        let border_style = if is_focused {
            Style::default().fg(theme.accent)
        } else {
            Style::default().fg(theme.border)
        };

        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(border_style)
            .padding(Padding::new(1, 1, 0, 0));
        // The Plugins master-detail pane names the plugin it shows.
        if let Some(title) = title {
            block = block.title(title.to_string());
        }

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

        let fields_viewport_height = inner.height.saturating_sub(warning_offset);
        self.fields_viewport_height = fields_viewport_height;

        let mut total_content_height = 0u16;
        for (i, field) in self.fields.iter().enumerate() {
            if i > 0 {
                total_content_height += 1; // spacing between fields
            }
            total_content_height += self.field_height(field, i);
        }

        let scroll_offset = self.fields_scroll_offset;

        let mut y_pos = 0u16; // absolute position in content space
        for (i, field) in self.fields.iter().enumerate() {
            let field_h = self.field_height(field, i);
            let field_top = y_pos;
            let field_bottom = y_pos + field_h;

            if field_bottom <= scroll_offset {
                y_pos += field_h + 1;
                continue;
            }

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
            // Dividers are non-interactive, as keyboard navigation reflects.
            if !matches!(field.value, FieldValue::SectionHeader) {
                self.field_rects.push((i, field_area));
            }
            y_pos += field_h + 1; // +1 for spacing
        }

        // Dim bg on the hovered field; selection wins. After the field loop,
        // so divider rows cannot bleed an overlay on themselves.
        if let Some(idx) = self.hovered_field() {
            let suppress = is_focused && idx == self.selected_field;
            if !suppress {
                if let Some((_, rect)) = self.field_rects.iter().find(|(i, _)| *i == idx).copied() {
                    paint_hover_bg(frame, rect, theme.selection);
                }
            }
        }

        if total_content_height > fields_viewport_height {
            let scrollbar_area = Rect {
                x: area.x + area.width - 1,
                y: area.y + 1,
                width: 1,
                height: area.height.saturating_sub(2),
            };
            // Captured so a grab-drag on the bar can move the viewport.
            self.scrollbar_area = scrollbar_area;

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

    /// Wrapped height of a field's description; zero when it is empty, so a
    /// subtitle-less section header wastes no line.
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
        // A styled heading with a dimmed subtitle, never selected because
        // navigation skips it. `theme.text` matches the categories panel.
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
                // `area` is clipped when the header sits at the bottom of the
                // viewport, so an unclamped subtitle paints over the border.
                let subtitle_height = (wrapped.len() as u16).min(area.height.saturating_sub(1));
                if subtitle_height > 0 {
                    let subtitle_area = Rect {
                        x: area.x,
                        y: area.y + 1,
                        width: area.width,
                        height: subtitle_height,
                    };
                    let lines: Vec<Line> = wrapped
                        .into_iter()
                        .map(|line| {
                            Line::from(Span::styled(line, Style::default().fg(theme.dimmed)))
                        })
                        .collect();
                    frame.render_widget(Paragraph::new(lines), subtitle_area);
                }
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

        // `area` is the field's visible slice: bound description and value to
        // it so neither bleeds over the border or into the footer.
        let wrapped_desc = wrap_description_lines(&field.description, area.width);
        let desc_height = wrapped_desc.len() as u16;
        let desc_visible = desc_height.min(area.height.saturating_sub(1));
        if desc_visible > 0 {
            let description_area = Rect {
                x: area.x,
                y: area.y + 1,
                width: area.width,
                height: desc_visible,
            };
            let desc_lines: Vec<Line> = wrapped_desc
                .into_iter()
                .map(|line| Line::from(Span::styled(line, Style::default().fg(theme.dimmed))))
                .collect();
            frame.render_widget(Paragraph::new(desc_lines), description_area);
        }

        // Inner renderers paint at `value_area.y + 1`, so shift by the wrapped
        // description height. The value sits at row `desc_height + 1`, so it is
        // skipped when the clipped slice leaves no room for it.
        if desc_height.saturating_add(1) >= area.height {
            return;
        }
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
                        // Truncate on a char boundary: byte slicing panics
                        // when a multi-byte char spans the cut.
                        match collapsed.char_indices().nth(47) {
                            Some((cut, _)) => format!("{}...", &collapsed[..cut]),
                            None => collapsed,
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
                // Handled by the early return at the top of `render_field`.
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

            // An empty expanded list used to render nothing under the
            // header, leaving the user staring at blank rows with no cue
            // that `a` starts an entry (issue #2932).
            if items.is_empty() && !list_state.adding_new {
                let hint_y = area.y + 2;
                if hint_y < area.y + area.height {
                    let hint_area = Rect {
                        x: area.x + 2,
                        y: hint_y,
                        width: area.width.saturating_sub(2),
                        height: 1,
                    };
                    frame.render_widget(
                        Paragraph::new("(no items, press a to add one)")
                            .style(Style::default().fg(theme.dimmed)),
                        hint_area,
                    );
                }
            }

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

                // While the add prompt is open the cursor belongs to the new
                // row at the bottom; suppress the marker on the previously
                // selected item so two `>` never show at once (issue #2932).
                let is_cursor_row = i == list_state.selected_index && !list_state.adding_new;
                let style = if is_cursor_row {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.dimmed)
                };

                let prefix = if is_cursor_row { "> " } else { "  " };

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

        let spans: Vec<Span> = if self.search_input.is_some() {
            vec![
                Span::styled("↑/↓", key_style),
                Span::styled(": select  ", desc_style),
                Span::styled("Enter", key_style),
                Span::styled(": jump  ", desc_style),
                Span::styled("Esc", key_style),
                Span::styled(": close  ", desc_style),
                Span::styled("Ctrl+s", key_style),
                Span::styled(": save", desc_style),
            ]
        } else if self.custom_instruction_dialog.is_some() {
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
        } else if let Some(list_state) = self.list_edit_state.as_ref() {
            // While an item is being typed, Enter confirms the item and Esc
            // cancels it; showing the list-navigation hints here (add /
            // delete / close list) would describe keys that do something
            // else entirely (issue #2932).
            if list_state.editing_item.is_some() {
                let confirm_label = if list_state.adding_new {
                    ": add item  "
                } else {
                    ": confirm  "
                };
                vec![
                    Span::styled("Enter", key_style),
                    Span::styled(confirm_label, desc_style),
                    Span::styled("Esc", key_style),
                    Span::styled(": cancel", desc_style),
                ]
            } else {
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
            }
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
                Span::styled("/", key_style),
                Span::styled(": search  ", desc_style),
                Span::styled("Ctrl+s", key_style),
                Span::styled(": save  ", desc_style),
                Span::styled("?", key_style),
                Span::styled(": help  ", desc_style),
                Span::styled("q", key_style),
                Span::styled(": close", desc_style),
            ]);

            s
        };

        // Key hints sit on the first footer row, exactly where they were.
        let help_area = Rect { height: 1, ..inner };
        let help = Paragraph::new(Line::from(spans)).alignment(ratatui::layout::Alignment::Center);
        frame.render_widget(help, help_area);

        // The save/error status renders on its own footer row below the hints
        // (the dashboard's hints-then-bar ordering), so it can never collide
        // with field content the way the old in-panel message did (issue
        // #2083). Only the message text is coloured; errors are red and stick
        // until the next keypress, the "Settings saved" toast is green and
        // auto-dismisses (see `tick_status`).
        if inner.height > 1 {
            let status = self
                .error_message
                .as_deref()
                .map(|text| (text, theme.error))
                .or_else(|| {
                    self.success_message
                        .as_deref()
                        .map(|text| (text, theme.running))
                });
            if let Some((text, color)) = status {
                let status_area = Rect {
                    y: inner.y + 1,
                    height: 1,
                    ..inner
                };
                let line = Line::from(vec![
                    Span::raw(" "),
                    Span::styled(text.to_string(), Style::default().fg(color)),
                ]);
                frame.render_widget(Paragraph::new(line), status_area);
            }
        }
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
                    ("/", "Search settings across all tabs, Enter jumps"),
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
}

#[cfg(test)]
mod tests {
    use super::{wrap_description_height, wrap_description_lines};

    /// Approximates the Interaction tab's description, which is long enough
    /// to wrap at any panel width.
    const LONG: &str = "What Enter (and double-click) does on a session row in \
                        the Structured view: attach to tmux (default, historical \
                        behavior) or enter live-send mode so the home list stays \
                        visible and keystrokes pipe through to the agent.";

    #[test]
    fn descriptions_wrap_on_word_boundaries_and_collapse_whitespace() {
        // (text, width, wrapped lines)
        let cases: &[(&str, u16, &[&str])] = &[
            ("", 40, &[]),
            ("short text", 40, &["short text"]),
            ("one two three four", 8, &["one two", "three", "four"]),
            // The `\`-continued descriptions in fields.rs carry their source
            // indentation, so runs of spaces collapse.
            ("hello      world      again", 40, &["hello world again"]),
            ("anything", 0, &["anything"]),
        ];
        // The height must agree with the line count, or `field_height` paints
        // values over the description in a real render.
        for (text, width, want) in cases {
            assert_eq!(wrap_description_lines(text, *width), *want, "{text:?}");
            assert_eq!(wrap_description_height(text, *width), want.len() as u16);
        }

        let lines = wrap_description_lines(LONG, 120);
        assert!(lines.len() > 1, "long text should wrap");
        for line in &lines {
            assert!(line.chars().count() <= 120, "{line:?} exceeds the width");
        }
        assert_eq!(
            wrap_description_height(LONG, 40),
            wrap_description_lines(LONG, 40).len() as u16
        );
    }
}

#[cfg(test)]
mod field_height_tests {
    use super::super::fields::FieldKind;
    use super::super::test_util::fresh_view;
    use super::super::{FieldValue, SettingField, SettingsCategory};
    use serial_test::serial;

    /// Locks the contract between the height the scroll math uses and what
    /// the render pass paints: a narrower panel grows it by the extra rows. A
    /// section header has no value row, so it is the label plus the subtitle.
    #[test]
    #[serial]
    fn field_height_grows_with_wrapped_description() {
        let (_temp, _guard, mut view) = fresh_view();

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

        // Fits "alpha beta" but not "alpha beta gamma", so it wraps twice.
        view.fields_content_width = 12;
        assert_eq!(
            view.field_height(&field, 0),
            4,
            "narrow panel: label + 2-line desc + value"
        );

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

#[cfg(test)]
mod status_message_tests {
    use super::super::fields::FieldKind;
    use super::super::test_util::fresh_view;
    use super::super::{FieldValue, SettingField, SettingsCategory, SettingsScope};
    use crate::session::config::settings_schema::{ValidationKind, WidgetKind};
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
    use serial_test::serial;
    use std::time::{Duration, Instant};

    fn row_text(buf: &Buffer, y: u16) -> String {
        let area = *buf.area();
        (area.x..area.x + area.width)
            .map(|x| buf[(x, y)].symbol())
            .collect()
    }

    fn buffer_text(buf: &Buffer) -> String {
        (0..buf.area().height)
            .map(|y| row_text(buf, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn bool_field(label: &str, desc: &str) -> SettingField {
        SettingField {
            kind: FieldKind::HostEnvironment,
            label: label.to_string(),
            description: desc.to_string(),
            value: FieldValue::Bool(false),
            category: SettingsCategory::Sandbox,
            has_override: false,
            inherited_display: None,
        }
    }

    /// A field clipped to a partial row at the bottom of the panel must not
    /// paint past it, over the border or into the footer.
    #[test]
    #[serial]
    fn clipped_bottom_field_does_not_spill_below_panel() {
        let (_temp, _guard, mut view) = fresh_view();
        let theme = load_theme("empire");

        // FieldB lands at the bottom clipped to about two rows, though its
        // wrapped description plus value need five.
        view.fields = vec![
            bool_field("FieldA", "alpha"),
            SettingField {
                value: FieldValue::Text("SPILLVALUE".to_string()),
                ..bool_field(
                    "FieldB",
                    "WRAPTOKEN alpha bravo charlie delta echo foxtrot golf hotel india juliet",
                )
            },
        ];
        view.fields_scroll_offset = 0;

        // An 8-row panel in a 12-row buffer, so a spill is visible below it.
        let area = Rect::new(0, 0, 30, 8);
        let mut terminal = Terminal::new(TestBackend::new(30, 12)).unwrap();
        terminal
            .draw(|f| view.render_fields(f, area, &theme, true, None))
            .unwrap();
        let buf = terminal.backend().buffer().clone();

        let all = buffer_text(&buf);
        assert!(
            all.contains("FieldB"),
            "the clipped field's label should still render, got:\n{all}"
        );
        assert!(
            !all.contains("SPILLVALUE"),
            "the clipped field's value must not render past its slice, got:\n{all}"
        );
        // The panel's bottom border row must stay border-only.
        let border_row = row_text(&buf, 7);
        assert!(
            !border_row.chars().any(|c| c.is_ascii_alphabetic()),
            "field text must not overwrite the panel's bottom border, got {border_row:?}"
        );
    }

    /// The status has its own footer row beneath the key hints, so it can
    /// never collide with field content.
    #[test]
    #[serial]
    fn footer_shows_status_below_hints() {
        let (_temp, _guard, mut view) = fresh_view();
        let theme = load_theme("empire");
        let area = Rect::new(0, 0, 100, 3);

        view.success_message = Some("Settings saved".to_string());
        let mut terminal = Terminal::new(TestBackend::new(100, 3)).unwrap();
        terminal
            .draw(|f| view.render_footer(f, area, &theme))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        assert!(
            row_text(&buf, 1).contains("save"),
            "key hints should remain on the first footer row"
        );
        assert!(
            row_text(&buf, 2).contains("Settings saved"),
            "the toast should render on the status row, got {:?}",
            row_text(&buf, 2)
        );
        assert_eq!(
            buf[(1, 2)].fg,
            theme.running,
            "the success toast should use the running (green) colour"
        );

        view.success_message = None;
        view.error_message = Some("Memory Limit: expected a string".to_string());
        let mut terminal = Terminal::new(TestBackend::new(100, 3)).unwrap();
        terminal
            .draw(|f| view.render_footer(f, area, &theme))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        assert!(
            row_text(&buf, 2).contains("Memory Limit: expected a string"),
            "the error should render on the status row, got {:?}",
            row_text(&buf, 2)
        );
        assert_eq!(
            buf[(1, 2)].fg,
            theme.error,
            "the error should use the error (red) colour"
        );
    }

    /// The toast auto-dismisses once its window passes; an error is sticky.
    #[test]
    #[serial]
    fn save_toast_expires_but_errors_stay() {
        let (_temp, _guard, mut view) = fresh_view();

        view.success_message = Some("Settings saved".to_string());
        view.success_message_expires_at = Instant::now().checked_sub(Duration::from_secs(1));
        assert!(
            view.tick_status(),
            "an expired toast should request a redraw"
        );
        assert!(
            view.success_message.is_none(),
            "the toast should be cleared"
        );

        view.error_message = Some("Memory Limit: expected a string".to_string());
        view.success_message_expires_at = None;
        assert!(!view.tick_status(), "a sticky error should not tick away");
        assert!(view.error_message.is_some(), "the error should persist");

        view.success_message = Some("Settings saved".to_string());
        view.success_message_expires_at = Some(Instant::now() + Duration::from_secs(60));
        assert!(!view.tick_status(), "an unexpired toast should stay");
        assert!(
            view.success_message.is_some(),
            "the toast should still show"
        );

        // A successful save arms the auto-dismiss timer alongside the toast.
        // Profile scope avoids the Global telemetry side effect, and no fields
        // means validation passes straight through to a real write.
        view.scope = SettingsScope::Profile;
        view.fields = Vec::new();

        view.save().unwrap();

        assert_eq!(view.success_message.as_deref(), Some("Settings saved"));
        assert!(
            view.success_message_expires_at.is_some(),
            "save should arm the auto-dismiss timer"
        );
    }

    /// Search is the fastest way around this many fields, so the footer
    /// advertises it rather than the `?` overlay hiding it.
    #[test]
    #[serial]
    fn footer_and_idle_bar_advertise_search() {
        let (_temp, _guard, mut view) = fresh_view();
        let theme = load_theme("empire");
        let area = Rect::new(0, 0, 120, 3);

        let mut terminal = Terminal::new(TestBackend::new(120, 3)).unwrap();
        terminal
            .draw(|f| view.render_footer(f, area, &theme))
            .unwrap();
        let hints = row_text(terminal.backend().buffer(), 1);
        assert!(
            hints.contains("/: search"),
            "normal-mode footer should advertise the search overlay, got {hints:?}"
        );

        let area = Rect::new(0, 0, 110, 40);
        let mut terminal = Terminal::new(TestBackend::new(110, 40)).unwrap();
        terminal.draw(|f| view.render(f, area, &theme)).unwrap();
        let all = buffer_text(terminal.backend().buffer());
        assert!(
            all.contains("Press / to search settings"),
            "the idle bar should show the placeholder, got:\n{all}"
        );
    }

    /// While an item is being typed, Enter confirms and Esc cancels, so the
    /// footer must not keep advertising the list-navigation keys.
    #[test]
    #[serial]
    fn footer_shows_item_edit_hints_while_typing_a_list_item() {
        let (_temp, _guard, mut view) = fresh_view();
        let theme = load_theme("empire");
        let area = Rect::new(0, 0, 100, 3);

        view.list_edit_state = Some(super::super::ListEditState {
            selected_index: 0,
            editing_item: Some(tui_input::Input::new("FOO=bar".to_string())),
            adding_new: true,
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 3)).unwrap();
        terminal
            .draw(|f| view.render_footer(f, area, &theme))
            .unwrap();
        let hints = row_text(terminal.backend().buffer(), 1);
        assert!(
            hints.contains("Enter: add item") && hints.contains("Esc: cancel"),
            "add-item footer should show confirm/cancel hints, got {hints:?}"
        );
        assert!(
            !hints.contains("close list"),
            "add-item footer must not show list-navigation hints, got {hints:?}"
        );

        view.list_edit_state = Some(super::super::ListEditState {
            selected_index: 0,
            editing_item: Some(tui_input::Input::new("FOO=bar".to_string())),
            adding_new: false,
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 3)).unwrap();
        terminal
            .draw(|f| view.render_footer(f, area, &theme))
            .unwrap();
        let hints = row_text(terminal.backend().buffer(), 1);
        assert!(
            hints.contains("Enter: confirm") && hints.contains("Esc: cancel"),
            "edit-item footer should show confirm/cancel hints, got {hints:?}"
        );

        // With no item being typed, the list-navigation hints remain.
        view.list_edit_state = Some(super::super::ListEditState::default());
        let mut terminal = Terminal::new(TestBackend::new(100, 3)).unwrap();
        terminal
            .draw(|f| view.render_footer(f, area, &theme))
            .unwrap();
        let hints = row_text(terminal.backend().buffer(), 1);
        assert!(
            hints.contains("a: add") && hints.contains("Esc: close list"),
            "list-navigation footer keeps the add/delete/close hints, got {hints:?}"
        );
    }

    /// An expanded empty list says how to add the first item.
    #[test]
    #[serial]
    fn expanded_empty_list_shows_add_hint() {
        let (_temp, _guard, mut view) = fresh_view();
        let theme = load_theme("empire");

        view.fields = vec![SettingField {
            kind: FieldKind::HostEnvironment,
            label: "Environment Variables".to_string(),
            description: "Env entries for the sandbox.".to_string(),
            value: FieldValue::List(Vec::new()),
            category: SettingsCategory::Sandbox,
            has_override: false,
            inherited_display: None,
        }];
        view.selected_field = 0;
        view.list_edit_state = Some(super::super::ListEditState::default());

        let area = Rect::new(0, 0, 60, 10);
        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        terminal
            .draw(|f| view.render_fields(f, area, &theme, true, None))
            .unwrap();
        let all = buffer_text(terminal.backend().buffer());
        assert!(
            all.contains("(no items, press a to add one)"),
            "expanded empty list should hint at `a`, got:\n{all}"
        );
    }

    /// With search active the bar is the query input with a hit count, and
    /// the popup below lists `[Category] Label  value` rows, truncated.
    #[test]
    #[serial]
    fn search_popup_renders_hits_with_values() {
        use super::super::SearchHit;

        let (_temp, _guard, mut view) = fresh_view();
        let theme = load_theme("empire");

        view.search_input = Some(tui_input::Input::new("sandbox".to_string()));
        view.search_hits = vec![
            SearchHit {
                category: SettingsCategory::Sandbox,
                field_ident: "sandbox.default_image".to_string(),
                field_label: "Default Image".to_string(),
                category_label: "Sandbox",
                value_display: "SEARCHVALUE".to_string(),
            },
            SearchHit {
                category: SettingsCategory::Sandbox,
                field_ident: "sandbox.custom_instruction".to_string(),
                field_label: "Custom Instruction".to_string(),
                category_label: "Sandbox",
                value_display: "LONGSTART ".repeat(30),
            },
        ];
        view.search_selected = 0;

        let area = Rect::new(0, 0, 110, 40);
        let mut terminal = Terminal::new(TestBackend::new(110, 40)).unwrap();
        terminal.draw(|f| view.render(f, area, &theme)).unwrap();
        let all = buffer_text(terminal.backend().buffer());

        assert!(
            all.contains("Search settings") && all.contains("/ sandbox"),
            "the bar should render the query, got:\n{all}"
        );
        assert!(
            all.contains("2 matches"),
            "the bar should show the hit count, got:\n{all}"
        );
        assert!(
            all.contains("[Sandbox] Default Image") && all.contains("SEARCHVALUE"),
            "popup rows should show category, label, and current value, got:\n{all}"
        );
        assert!(
            all.contains("LONGSTART") && all.contains('…'),
            "an overlong value should render truncated with an ellipsis, got:\n{all}"
        );
        assert_eq!(
            view.search_hit_rows.len(),
            2,
            "the render must capture a screen row per visible hit for \
             click/hover routing"
        );
        assert_ne!(
            view.search_popup_area,
            Rect::default(),
            "the render must capture the popup frame rect"
        );
    }

    /// The bar is permanent: idle, it advertises `/`.
    /// The add prompt owns the only cursor, so the previously selected item
    /// drops its `>` marker.
    #[test]
    #[serial]
    fn add_prompt_suppresses_the_item_cursor() {
        let (_temp, _guard, mut view) = fresh_view();
        let theme = load_theme("empire");

        view.fields = vec![SettingField {
            kind: FieldKind::HostEnvironment,
            label: "Environment Variables".to_string(),
            description: "Env entries.".to_string(),
            value: FieldValue::List(vec!["AAA".to_string(), "BBB".to_string()]),
            category: SettingsCategory::Sandbox,
            has_override: false,
            inherited_display: None,
        }];
        view.selected_field = 0;
        view.list_edit_state = Some(super::super::ListEditState {
            selected_index: 1,
            editing_item: Some(tui_input::Input::new("NEW".to_string())),
            adding_new: true,
        });

        let area = Rect::new(0, 0, 60, 12);
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal
            .draw(|f| view.render_fields(f, area, &theme, true, None))
            .unwrap();
        let buf = terminal.backend().buffer().clone();

        let bbb_row = (0..buf.area().height)
            .map(|y| row_text(&buf, y))
            .find(|row| row.contains("BBB"))
            .expect("the BBB item should render");
        assert!(
            !bbb_row.contains('>'),
            "the item row must not show the cursor while the add prompt is \
             open, got {bbb_row:?}"
        );
        let new_row = (0..buf.area().height)
            .map(|y| row_text(&buf, y))
            .find(|row| row.contains("NEW"))
            .expect("the add prompt should render");
        assert!(
            new_row.contains('>'),
            "the add prompt keeps the single cursor, got {new_row:?}"
        );
    }

    /// A validation failure names the offending field, not just the reason.
    #[test]
    #[serial]
    fn save_error_names_the_field() {
        let (_temp, _guard, mut view) = fresh_view();

        // Set-but-invalid, since a cleared value validates as unset.
        view.fields = vec![SettingField {
            kind: FieldKind::Schema {
                section: "sandbox".to_string(),
                field: "memory_limit".to_string(),
                widget: WidgetKind::OptionalText { mono: false },
                validation: ValidationKind::MemoryLimit,
                profile_overridable: true,
            },
            label: "Memory Limit".to_string(),
            description: "Memory ceiling for sandbox containers.".to_string(),
            value: FieldValue::OptionalText(Some("not-a-size".to_string())),
            category: SettingsCategory::Sandbox,
            has_override: false,
            inherited_display: None,
        }];

        view.save().unwrap();

        let msg = view
            .error_message
            .expect("save should surface a validation error");
        assert!(
            msg.starts_with("Memory Limit: "),
            "error should be prefixed with the field label, got {msg:?}"
        );
    }
}
