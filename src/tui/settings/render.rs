//! Rendering for the settings view

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, Padding, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Widget,
    },
    Frame,
};
use tui_input::Input;
use unicode_width::UnicodeWidthStr;

use crossterm::event::KeyCode;

use super::{
    CategoryRow, FieldValue, ListEditHits, SettingsCategory, SettingsFocus, SettingsScope,
    SettingsView,
};
use crate::tui::components::hover::{paint_hover_bg, HoverState};
use crate::tui::components::{input_cursor_position, set_input_cursor_position, truncate_to_width};
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
        self.list_edit_hits = ListEditHits::default();
        if self.list_edit_state.is_none() {
            self.list_hover = HoverState::default();
        }
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

        if let Some(ref mut dialog) = self.custom_instruction_dialog {
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
                Constraint::Length(21), // Categories
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
        let mut list_hits = None;

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

            // Draw the field whole off-screen, then copy the rows the viewport shows: a field
            // clipped at the top would otherwise start too low and spill into the next one.
            let visible_y = field_top.saturating_sub(scroll_offset);
            let hidden = scroll_offset.saturating_sub(field_top);
            let is_selected = i == self.selected_field && is_focused;
            let field_area = Rect {
                x: inner.x,
                y: inner.y + visible_y + warning_offset,
                width: inner.width,
                height: (field_h - hidden).min(fields_viewport_height - visible_y),
            };
            let mut canvas = FieldCanvas::new(frame.buffer_mut(), field_area, hidden, field_h);
            let whole = canvas.area();
            let hits = self.render_field(&mut canvas, whole, field, i, is_selected, theme);
            let to_screen = canvas.copy_into(frame.buffer_mut());
            if let Some(position) = canvas.cursor.and_then(&to_screen) {
                frame.set_cursor_position(position);
            }
            if let Some(mut hits) = hits {
                fn shift<T>(
                    rects: &mut Vec<(T, Rect)>,
                    to_screen: impl Fn(Position) -> Option<Position>,
                ) {
                    rects.retain_mut(|(_, r)| match to_screen(r.as_position()) {
                        Some(p) => {
                            r.y = p.y;
                            true
                        }
                        None => false,
                    })
                }
                shift(&mut hits.rows, &to_screen);
                shift(&mut hits.actions, &to_screen);
                list_hits = Some(hits);
            }
            // Dividers are non-interactive, as keyboard navigation reflects.
            if !matches!(field.value, FieldValue::SectionHeader) {
                self.field_rects.push((i, field_area));
            }
            y_pos += field_h + 1; // +1 for spacing
        }
        if let Some(hits) = list_hits {
            if let Some(rect) = self.list_hover.current_in(&hits.rects()) {
                paint_hover_bg(frame, rect, theme.selection);
            }
            self.list_edit_hits = hits;
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
        canvas: &mut FieldCanvas,
        area: Rect,
        field: &super::SettingField,
        index: usize,
        is_selected: bool,
        theme: &Theme,
    ) -> Option<ListEditHits> {
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
            canvas.render_widget(Paragraph::new(heading), area);
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
                canvas.render_widget(Paragraph::new(lines), subtitle_area);
            }
            return None;
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

        let mut spans = vec![
            Span::styled(field.label.clone(), label_style),
            override_indicator,
        ];
        spans.extend(
            field
                .tags()
                .into_iter()
                .map(|tag| Span::styled(format!(" · {tag}"), Style::default().fg(theme.dimmed))),
        );
        let label = Line::from(spans);

        canvas.render_widget(Paragraph::new(label), area);

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
        canvas.render_widget(Paragraph::new(desc_lines), description_area);

        // Inner renderers paint at `value_area.y + 1`, so shift by the wrapped
        // description height.
        let value_area = Rect {
            y: area.y + desc_height,
            height: area.height - desc_height,
            ..area
        };

        match &field.value {
            FieldValue::Bool(value) => {
                self.render_bool_field(canvas, value_area, *value, is_selected, theme);
            }
            FieldValue::Text(value) => {
                self.render_text_field(canvas, value_area, value, index, is_selected, theme);
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
                    Some(text) if !text.trim().is_empty() => text.to_string(),
                    _ => field.unset_display().unwrap_or_default(),
                };
                self.render_text_field(canvas, value_area, &display, index, is_selected, theme);
            }
            FieldValue::Number(value) => {
                self.render_number_field(canvas, value_area, *value, index, is_selected, theme);
            }
            FieldValue::Select { selected, options } => {
                self.render_select_field(
                    canvas,
                    value_area,
                    *selected,
                    options,
                    is_selected,
                    theme,
                );
            }
            FieldValue::List(items) => {
                return self.render_list_field(
                    canvas,
                    value_area,
                    items,
                    index,
                    is_selected,
                    theme,
                );
            }
            FieldValue::SectionHeader => {
                // Handled by the early return at the top of `render_field`.
            }
        }
        None
    }

    fn render_bool_field(
        &self,
        canvas: &mut FieldCanvas,
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
        canvas.render_widget(Paragraph::new(text).style(style), value_area);
    }

    fn render_text_field(
        &self,
        canvas: &mut FieldCanvas,
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
            self.render_input_with_cursor(canvas, value_area, input, theme);
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

            canvas.render_widget(Paragraph::new(display).style(style), value_area);
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
        canvas: &mut FieldCanvas,
        area: Rect,
        input: &Input,
        theme: &Theme,
    ) {
        let spans = Self::build_cursor_spans(input.value(), input.cursor(), theme);
        canvas.render_widget(Paragraph::new(Line::from(spans)), area);
        if self.editing_cursor_visible() {
            canvas.cursor = input_cursor_position(area, 0, input);
        }
    }

    fn render_list_item_with_cursor(
        &self,
        canvas: &mut FieldCanvas,
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
        canvas.render_widget(Paragraph::new(Line::from(spans)), area);
        if self.editing_cursor_visible() {
            canvas.cursor = input_cursor_position(area, prefix.width(), input);
        }
    }

    fn editing_cursor_visible(&self) -> bool {
        self.custom_instruction_dialog.is_none() && !self.show_help
    }

    fn render_number_field(
        &self,
        canvas: &mut FieldCanvas,
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
            self.render_input_with_cursor(canvas, value_area, input, theme);
        } else {
            let style = if is_selected {
                Style::default().fg(theme.accent)
            } else {
                Style::default().fg(theme.dimmed)
            };

            canvas.render_widget(Paragraph::new(value.to_string()).style(style), value_area);
        }
    }

    fn render_select_field(
        &self,
        canvas: &mut FieldCanvas,
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
        canvas.render_widget(
            Paragraph::new(format!("{}{}", display, arrows)).style(style),
            value_area,
        );
    }

    /// Returns the editor's hit rects while this list is expanded.
    fn render_list_field(
        &self,
        canvas: &mut FieldCanvas,
        area: Rect,
        items: &[String],
        index: usize,
        is_selected: bool,
        theme: &Theme,
    ) -> Option<ListEditHits> {
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

            canvas.render_widget(Paragraph::new(text).style(style), value_area);
            None
        } else {
            // Expanded view - show all items
            let list_state = self.list_edit_state.as_ref().unwrap();
            let mut hits = ListEditHits::default();

            let header_area = Rect {
                x: area.x,
                y: area.y + 1,
                width: area.width,
                height: 1,
            };

            const PREFIX: &str = "Items: ";
            const ACTIONS: [(KeyCode, &str); 4] = [
                (KeyCode::Char('a'), "(a)dd"),
                (KeyCode::Char('d'), "(d)elete"),
                (KeyCode::Enter, "(Enter)edit"),
                (KeyCode::Esc, "(Esc)close"),
            ];
            let dimmed = Style::default().fg(theme.dimmed);
            let mut spans = vec![Span::styled(PREFIX, dimmed)];
            // ASCII labels, so a byte count is the cell count.
            let mut x = header_area.x + PREFIX.len() as u16;
            for (i, (code, label)) in ACTIONS.into_iter().enumerate() {
                if i > 0 {
                    spans.push(Span::raw(" "));
                    x += 1;
                }
                spans.push(Span::styled(label, dimmed));
                let width = label.len() as u16;
                if x + width <= header_area.right() {
                    hits.actions
                        .push((code, Rect::new(x, header_area.y, width, 1)));
                }
                x += width;
            }
            canvas.render_widget(Paragraph::new(Line::from(spans)), header_area);
            // An empty expanded list used to render nothing under the
            // header, leaving the user staring at blank rows with no cue
            // that `a` starts an entry (issue #2932).
            if items.is_empty() && !list_state.adding_new {
                let hint_area = Rect {
                    x: area.x + 2,
                    y: area.y + 2,
                    width: area.width.saturating_sub(2),
                    height: 1,
                };
                canvas.render_widget(
                    Paragraph::new("(no items, press a to add one)")
                        .style(Style::default().fg(theme.dimmed)),
                    hint_area,
                );
            }

            // Render items
            for (i, item) in items.iter().enumerate() {
                let item_area = Rect {
                    x: area.x + 2,
                    y: area.y + 2 + i as u16,
                    width: area.width.saturating_sub(2),
                    height: 1,
                };
                hits.rows.push((i, item_area));

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
                    self.render_list_item_with_cursor(canvas, item_area, prefix, input, theme);
                } else {
                    let display = format!("{}{}", prefix, item);
                    canvas.render_widget(Paragraph::new(display).style(style), item_area);
                }
            }

            // Show add prompt if adding new
            if let Some(input) = list_state
                .editing_item
                .as_ref()
                .filter(|_| list_state.adding_new)
            {
                let add_area = Rect {
                    x: area.x + 2,
                    y: area.y + 2 + items.len() as u16,
                    width: area.width.saturating_sub(2),
                    height: 1,
                };
                self.render_list_item_with_cursor(canvas, add_area, "> ", input, theme);
            }
            Some(hits)
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

/// One field drawn at full height off-screen; `render_fields` copies the rows the viewport shows.
struct FieldCanvas {
    buf: Buffer,
    /// Hidden rows above the viewport.
    hidden: u16,
    /// Where the visible rows land on screen.
    target: Rect,
    cursor: Option<Position>,
}

impl FieldCanvas {
    /// The rows that will be copied start out as the screen's, so the copy only changes what the
    /// field draws.
    fn new(screen: &Buffer, target: Rect, hidden: u16, height: u16) -> Self {
        let mut buf = Buffer::empty(Rect::new(target.x, 0, target.width, height));
        for row in 0..target.height {
            for x in target.left()..target.right() {
                buf[(x, hidden + row)] = screen[(x, target.y + row)].clone();
            }
        }
        Self {
            buf,
            hidden,
            target,
            cursor: None,
        }
    }

    fn area(&self) -> Rect {
        self.buf.area
    }

    fn render_widget(&mut self, widget: impl Widget, area: Rect) {
        widget.render(area, &mut self.buf);
    }

    /// Copies the visible rows and returns the map from a canvas position to the screen, `None`
    /// for a row that isn't shown.
    fn copy_into(&self, screen: &mut Buffer) -> impl Fn(Position) -> Option<Position> {
        for row in 0..self.target.height {
            for x in self.target.left()..self.target.right() {
                screen[(x, self.target.y + row)] = self.buf[(x, self.hidden + row)].clone();
            }
        }
        let (hidden, target) = (self.hidden, self.target);
        move |p: Position| {
            let row = p.y.checked_sub(hidden).filter(|row| *row < target.height)?;
            Some(Position::new(p.x, target.y + row))
        }
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

    /// Every category name fits the nav column whole, selected or not.
    #[test]
    #[serial]
    fn category_nav_fits_every_label() {
        let (_temp, _guard, mut view) = fresh_view();
        let theme = load_theme("empire");
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|f| view.render_content(f, f.area(), &theme))
            .unwrap();
        let text = buffer_text(terminal.backend().buffer());
        for label in ["Lifecycle Hooks", "Status Hooks", "ACP"] {
            assert!(text.contains(&format!(" {label} ")), "{label}:\n{text}");
        }
    }

    /// Schema flags render as tags on the label line, and a plain row has none.
    #[test]
    #[serial]
    fn schema_flags_render_as_tags_after_the_label() {
        let (_temp, _guard, mut view) = fresh_view();
        let theme = load_theme("empire");
        let schema_field = |label: &str, restart: bool, web_only: bool| SettingField {
            kind: FieldKind::Schema {
                section: "logging".to_string(),
                field: label.to_lowercase(),
                widget: WidgetKind::Toggle,
                validation: ValidationKind::None,
                profile_overridable: false,
                restart,
                web_only,
            },
            ..bool_field(label, "")
        };
        view.fields = vec![
            schema_field("Restarted", true, false),
            schema_field("Webbed", false, true),
            schema_field("Plain", false, false),
        ];
        view.fields_scroll_offset = 0;

        let area = Rect::new(0, 0, 60, 20);
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal
            .draw(|f| view.render_fields(f, area, &theme, true, None))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let line_of = |label: &str| {
            (0..buf.area().height)
                .map(|y| row_text(&buf, y))
                .find(|row| row.contains(label))
                .unwrap_or_else(|| panic!("no row for {label}:\n{}", buffer_text(&buf)))
        };

        let restarted = line_of("Restarted");
        assert!(
            restarted.contains("Restarted · Needs restart"),
            "{restarted}"
        );
        assert!(!restarted.contains("Web dashboard only"), "{restarted}");
        let webbed = line_of("Webbed");
        assert!(webbed.contains("Webbed · Web dashboard only"), "{webbed}");
        assert!(!webbed.contains("Needs restart"), "{webbed}");
        let plain = line_of("Plain");
        assert!(!plain.contains('·'), "{plain}");
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
                restart: false,
                web_only: false,
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

    fn paint_fields(view: &mut super::super::SettingsView, width: u16, height: u16) -> Buffer {
        let theme = load_theme("empire");
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        view.field_rects.clear();
        terminal
            .draw(|f| view.render_fields(f, Rect::new(0, 0, width, height), &theme, true, None))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn content_height(view: &super::super::SettingsView) -> u16 {
        let rows: u16 = (0..view.fields.len())
            .map(|i| view.field_height(&view.fields[i], i))
            .sum();
        rows + view.fields.len().saturating_sub(1) as u16
    }

    /// #5760: a field cut by the viewport shows exactly its own rows. On every tab, at every
    /// scroll offset, the panel equals an unclipped render shifted by that offset, so no field's
    /// text can land on another's rows. The 9-row panel clips some fields at both ends.
    #[test]
    #[serial]
    fn every_scroll_offset_shows_the_unclipped_rows() {
        use super::super::CategoryRow;
        // Over SSH the Sound tab draws a warning above its fields; this compares fields only.
        let _ssh = crate::session::test_support::EnvGuard::unset(&[
            "SSH_CONNECTION",
            "SSH_CLIENT",
            "SSH_TTY",
        ]);
        let (_temp, _guard, mut view) = fresh_view();
        let tabs: Vec<usize> = (0..view.categories.len())
            .filter(|&i| {
                matches!(view.categories[i], CategoryRow::Tab(c) if c != SettingsCategory::Plugins)
            })
            .collect();
        let mut both_ends = Vec::new();
        // Panel widths of an 80- and a 120-column terminal (the categories take 20).
        for (width, height) in [(60u16, 30u16), (100, 30), (60, 9)] {
            for &tab in &tabs {
                view.selected_category = tab;
                view.rebuild_fields();
                view.fields_scroll_offset = 0;
                let tall = paint_fields(&mut view, width, 400);
                let content = content_height(&view);
                assert!(
                    content <= view.fields_viewport_height,
                    "the reference must not scroll"
                );
                paint_fields(&mut view, width, height);
                let viewport = view.fields_viewport_height;
                let tab_name = format!("{:?} at {width}x{height}", view.categories[tab]);
                if (0..view.fields.len()).any(|i| view.field_height(&view.fields[i], i) > viewport)
                {
                    both_ends.push(tab_name.clone());
                }
                for offset in 0..=content.saturating_sub(viewport) {
                    view.fields_scroll_offset = offset;
                    let buf = paint_fields(&mut view, width, height);
                    for row in 0..viewport {
                        let y = 1 + row;
                        let same = (1..width - 1).all(|x| buf[(x, y)] == tall[(x, y + offset)]);
                        assert!(
                            same,
                            "{tab_name}, offset {offset}, row {row}:\n got  {:?}\n want {:?}",
                            row_text(&buf, y),
                            row_text(&tall, y + offset)
                        );
                    }
                    for (a, (_, ra)) in view.field_rects.iter().enumerate() {
                        assert!(
                            ra.y >= 1 && ra.bottom() <= 1 + viewport,
                            "{tab_name}, offset {offset}: {ra:?}"
                        );
                        for (_, rb) in &view.field_rects[a + 1..] {
                            assert!(
                                !ra.intersects(*rb),
                                "{tab_name}, offset {offset}: {ra:?} {rb:?}"
                            );
                        }
                    }
                }
            }
        }
        assert!(
            !both_ends.is_empty(),
            "no tab has a field taller than the viewport"
        );
    }

    /// An expanded list cut at the top keeps its click targets on the rows that show them.
    #[test]
    #[serial]
    fn top_clipped_list_hits_match_their_rows() {
        use super::super::ListEditState;
        let (_temp, _guard, mut view) = fresh_view();
        let (tab, index) = (0..view.categories.len())
            .find_map(|tab| {
                view.selected_category = tab;
                view.rebuild_fields();
                let index = view
                    .fields
                    .iter()
                    .position(|f| matches!(f.value, FieldValue::List(_)))?;
                Some((tab, index))
            })
            .expect("a tab with a list field");
        view.selected_category = tab;
        view.rebuild_fields();
        let items = ["alpha-item", "bravo-item", "charlie-item"];
        view.fields[index].value = FieldValue::List(items.iter().map(|s| s.to_string()).collect());
        view.selected_field = index;
        view.list_edit_state = Some(ListEditState {
            selected_index: 0,
            editing_item: None,
            adding_new: false,
        });
        paint_fields(&mut view, 60, 30);
        let top: u16 = (0..index)
            .map(|i| view.field_height(&view.fields[i], i) + 1)
            .sum();
        let field_h = view.field_height(&view.fields[index], index);
        // Field rows: label, description, the actions header, then one row per item.
        let header_row = 1 + view.description_height(&view.fields[index].description);
        for hidden in 0..field_h {
            view.fields_scroll_offset = top + hidden;
            let buf = paint_fields(&mut view, 60, 30);
            let hits = &view.list_edit_hits;
            let shown: Vec<usize> = (0..items.len())
                .filter(|i| header_row + 1 + *i as u16 >= hidden)
                .collect();
            let got: Vec<usize> = hits.rows.iter().map(|(i, _)| *i).collect();
            assert_eq!(got, shown, "hidden {hidden}");
            for (i, rect) in &hits.rows {
                let row = row_text(&buf, rect.y);
                assert!(
                    row.contains(items[*i]),
                    "hidden {hidden}: item {i} at row {} reads {row:?}",
                    rect.y
                );
            }
            assert_eq!(
                hits.actions.is_empty(),
                header_row < hidden,
                "hidden {hidden}"
            );
            for (_, rect) in &hits.actions {
                assert!(
                    row_text(&buf, rect.y).contains("(a)dd"),
                    "hidden {hidden}: actions row {}",
                    rect.y
                );
            }
        }
    }
}
