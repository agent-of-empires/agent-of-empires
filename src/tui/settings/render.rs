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

/// Line count of [`wrap_description_lines`], used by `field_height`.
// ponytail: allocates the wrapped Vec just to count it; settings render is
// not hot enough to warrant a second copy of the wrap algorithm.
pub(super) fn wrap_description_height(text: &str, width: u16) -> u16 {
    wrap_description_lines(text, width).len() as u16
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
        self.search_hit_rows.clear();
        self.search_popup_area = Rect::default();

        // Clear the area
        frame.render_widget(Clear, area);

        // Main layout: title bar, the permanent search bar, content,
        // footer. The bar always renders (a placeholder when idle) so
        // the search affordance is visible without knowing the hotkey.
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

        // Render custom instruction dialog overlay if active
        if let Some(ref dialog) = self.custom_instruction_dialog {
            dialog.render(frame, area, theme);
        }

        // Render help overlay on top
        if self.show_help {
            self.render_help_overlay(frame, area, theme);
        }

        // The jump popup paints last so it drops over the panels (and
        // any overlay beneath), anchored under the bar like a
        // command-palette dropdown. Key dispatch is already gated on
        // `search_input.is_some()`; painting last makes that gate
        // visible too.
        if self.search_input.is_some() {
            let content_area = layout[2];
            self.render_search_dropdown(frame, layout[1], content_area, theme);
        }
    }

    /// The permanent settings search bar between the header and the
    /// panels (issue #2932). Idle, it shows a placeholder advertising
    /// `/`; active, it is the query input for the jump popup below,
    /// with the hit count right-aligned.
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

    /// The jump popup: a dropdown of ranked hits anchored under the
    /// search bar, command-palette style. Each row shows the hit's
    /// category, label, and current value (truncated); Enter jumps to
    /// the highlighted hit in its category.
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
        // Rows + borders, capped to the content area so the footer
        // hints stay visible.
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
        // Screen row per visible hit, for click + hover routing (the
        // command palette's visible_item_rows pattern).
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
            // The current value renders dimmed after the label so the
            // popup doubles as a settings review surface, truncated to
            // what fits on the row.
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
        // The Plugins category hosts the embedded plugin manager in the right
        // pane; every other category renders the normal field list.
        if self.current_category() == SettingsCategory::Plugins {
            let focused = self.focus == SettingsFocus::Fields;
            // Master-detail: the manager list on top, sized to its rows, and
            // the SELECTED plugin's editable settings beneath it (the same
            // generic field list every other category renders;
            // `rebuild_fields` filters it to the selection). Tab moves the
            // sub-focus between the panes. While the manager captures input
            // (discover mode, an open consent/progress popup) it owns the
            // whole pane: those surfaces need the space, and popups center
            // within its rect.
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

        // Status messages render in the footer status row (see
        // `render_footer`), not over the fields, so the fields panel keeps its
        // full height and nothing has to be reserved here.
        let fields_viewport_height = inner.height.saturating_sub(warning_offset);
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
                // Clamp the subtitle to the slice of `area` left below the
                // heading. When the header sits at the bottom of the viewport
                // `area` is clipped to fewer rows than the header's natural
                // height, and an unclamped subtitle would paint past the panel,
                // over its bottom border (issue #2083).
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

        // `area` is clipped to the field's visible slice when the field sits at
        // the bottom of the viewport. Bound the description and value to that
        // slice so neither bleeds past the panel, over its bottom border or
        // into the footer below (issue #2083).
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

        // Inner value renderers paint at `value_area.y + 1`, so shift
        // by the wrapped description height to keep the value aligned
        // directly under the (potentially multi-line) description. Skip the
        // value entirely when that row falls outside the clipped slice rather
        // than letting it spill past the field. The value occupies the row at
        // `desc_height + 1` within the field, so it fits only when the clipped
        // height leaves room for it.
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
    /// for every input; it now delegates to `wrap_description_lines`, so this
    /// guards against the delegation regressing. If they ever drift,
    /// `field_height` will paint values on top of (or below) the description
    /// in real renders.
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
    use super::super::test_util::fresh_view;
    use super::super::{FieldValue, SettingField, SettingsCategory};
    use serial_test::serial;

    /// At a normal panel width, a short description fits on one row, so
    /// `field_height` returns the historical `1 + 1 + 1`. At a width
    /// narrow enough to force two wrap lines, the height grows by exactly
    /// the extra row. Locks the contract between `description_height`
    /// (consumed by the scroll math) and what the render pass paints.
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
        let (_temp, _guard, mut view) = fresh_view();

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
    use crate::session::settings_schema::{ValidationKind, WidgetKind};
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

    /// A field clipped to a partial row at the bottom of the fields panel must
    /// not paint its description or value past the panel, over its bottom
    /// border or into the footer below it (issue #2083). The status message no
    /// longer lives in the panel, so the only thing that can spill is field
    /// content, and the clamps must stop it.
    #[test]
    #[serial]
    fn clipped_bottom_field_does_not_spill_below_panel() {
        let (_temp, _guard, mut view) = fresh_view();
        let theme = load_theme("empire");

        // FieldA fits fully; FieldB lands at the bottom clipped to ~2 rows even
        // though its wrapped description plus value need five. Its value
        // ("SPILLVALUE") and the lower description lines would, before the fix,
        // paint over the panel's bottom border and onto the blank rows beneath.
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

        // 8-row panel inside a 12-row buffer: rows 8..11 sit below the panel, so
        // any spill is visible (not clipped off-screen) and readable.
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
        // The panel's bottom border row (y = 7) must stay border-only; before
        // the fix a wrapped description line painted letters over it.
        let border_row = row_text(&buf, 7);
        assert!(
            !border_row.chars().any(|c| c.is_ascii_alphabetic()),
            "field text must not overwrite the panel's bottom border, got {border_row:?}"
        );
    }

    /// The save/error status renders on its own footer row beneath the key
    /// hints, colouring only its text, so it never collides with field content
    /// (issue #2083).
    #[test]
    #[serial]
    fn footer_shows_status_below_hints() {
        let (_temp, _guard, mut view) = fresh_view();
        let theme = load_theme("empire");
        let area = Rect::new(0, 0, 100, 3);

        // Success toast: green, on the second inner row (y = 2), hints on y = 1.
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

        // Error: red, same row, sticky.
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

    /// The "Settings saved" toast auto-dismisses once its window passes, while
    /// a sticky error is left untouched (issue #2083).
    #[test]
    #[serial]
    fn tick_status_expires_success_but_keeps_error() {
        let (_temp, _guard, mut view) = fresh_view();

        // Expired success toast: cleared, and the tick reports a redraw.
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

        // Sticky error with no expiry: untouched.
        view.error_message = Some("Memory Limit: expected a string".to_string());
        view.success_message_expires_at = None;
        assert!(!view.tick_status(), "a sticky error should not tick away");
        assert!(view.error_message.is_some(), "the error should persist");

        // Unexpired toast: left in place.
        view.success_message = Some("Settings saved".to_string());
        view.success_message_expires_at = Some(Instant::now() + Duration::from_secs(60));
        assert!(!view.tick_status(), "an unexpired toast should stay");
        assert!(
            view.success_message.is_some(),
            "the toast should still show"
        );
    }

    /// A successful save arms the auto-dismiss timer alongside the toast.
    #[test]
    #[serial]
    fn save_arms_the_success_toast_timer() {
        let (_temp, _guard, mut view) = fresh_view();
        // Profile scope avoids the Global telemetry side effect; no fields means
        // validation passes straight through to a real write.
        view.scope = SettingsScope::Profile;
        view.fields = Vec::new();

        view.save().unwrap();

        assert_eq!(view.success_message.as_deref(), Some("Settings saved"));
        assert!(
            view.success_message_expires_at.is_some(),
            "save should arm the auto-dismiss timer"
        );
    }

    /// The `/` search is the fastest way around a settings surface with this
    /// many fields, so normal mode must advertise it in the footer instead of
    /// hiding it in the `?` help overlay (issue #2932).
    #[test]
    #[serial]
    fn footer_advertises_search_in_normal_mode() {
        let (_temp, _guard, view) = fresh_view();
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
    }

    /// While a list item is being typed, Enter confirms the item and Esc
    /// cancels it. The footer must say so; the old hints (add / delete /
    /// close list) described keys that do something else entirely in that
    /// sub-mode (issue #2932).
    #[test]
    #[serial]
    fn footer_shows_item_edit_hints_while_typing_a_list_item() {
        let (_temp, _guard, mut view) = fresh_view();
        let theme = load_theme("empire");
        let area = Rect::new(0, 0, 100, 3);

        // Adding a new item: Enter adds, Esc cancels.
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

        // Editing an existing item: Enter confirms the edit.
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

        // Navigating the expanded list (no item being typed): the
        // list-navigation hints remain.
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

    /// An expanded empty list must tell the user how to add the first item
    /// instead of rendering blank rows (issue #2932).
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

    /// With search active, the full render shows the bar as the query
    /// input with the hit count, and the jump popup drops below it
    /// listing `[Category] Label  value` rows with long values
    /// truncated (issue #2932).
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

    /// The search bar is permanent: idle it advertises `/` with a
    /// placeholder instead of disappearing (issue #2932 review).
    #[test]
    #[serial]
    fn idle_search_bar_shows_placeholder() {
        let (_temp, _guard, mut view) = fresh_view();
        let theme = load_theme("empire");

        let area = Rect::new(0, 0, 110, 40);
        let mut terminal = Terminal::new(TestBackend::new(110, 40)).unwrap();
        terminal.draw(|f| view.render(f, area, &theme)).unwrap();
        let all = buffer_text(terminal.backend().buffer());
        assert!(
            all.contains("Press / to search settings"),
            "the idle bar should show the placeholder, got:\n{all}"
        );
    }

    /// While the add prompt is open, the previously selected list item
    /// must not keep its `>` marker; two cursors at once made the add
    /// flow read as messy (issue #2932).
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

    /// A validation failure on save names the offending field so the user can
    /// find it, instead of surfacing a bare reason like "expected a string"
    /// (issue #2083).
    #[test]
    #[serial]
    fn save_error_names_the_field() {
        let (_temp, _guard, mut view) = fresh_view();

        // A set-but-invalid value (not a cleared one, which now validates as
        // unset) so validation genuinely fails and we can check the prefix.
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
