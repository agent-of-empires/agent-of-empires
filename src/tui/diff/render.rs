//! Rendering for the diff view

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
use similar::ChangeTag;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::DiffView;
use crate::git::diff::FileStatus;
use crate::tui::styles::Theme;

/// Truncate a string from the left, adding an ellipsis prefix if it doesn't fit.
fn truncate_left(s: &str, max_width: usize) -> String {
    if s.len() <= max_width {
        return s.to_string();
    }
    if max_width <= 1 {
        return ".".to_string();
    }
    // "..." + tail of the string
    let tail_len = max_width.saturating_sub(1);
    let start = s.len() - tail_len;
    format!("\u{2026}{}", &s[start..])
}

impl DiffView {
    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        // Clear the area
        frame.render_widget(Clear, area);

        // If branch select dialog is open, render it
        if self.branch_select.is_some() {
            self.render_with_branch_dialog(frame, area, theme);
            return;
        }

        // Main layout: header, content, footer
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Header
                Constraint::Min(10),   // Content
                Constraint::Length(3), // Footer
            ])
            .split(area);

        self.render_header(frame, layout[0], theme);
        self.render_content(frame, layout[1], theme);
        self.render_footer(frame, layout[2], theme);

        // Render help overlay if active
        if self.show_help {
            self.render_help(frame, area, theme);
        }

        // Render warning dialog on top of everything
        if let Some(ref mut dialog) = self.warning_dialog {
            dialog.render(frame, area, theme);
        }
    }

    fn render_header(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(theme.border));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let file_count = self.files.len();
        let additions: usize = self.files.iter().map(|f| f.additions).sum();
        let deletions: usize = self.files.iter().map(|f| f.deletions).sum();

        // Get repo name from path
        let repo_name = self
            .repo_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo");

        let header = Line::from(vec![
            Span::styled(
                format!("  {} ", repo_name),
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
            ),
            Span::styled("vs ", Style::default().fg(theme.dimmed)),
            Span::styled(&self.base_branch, Style::default().fg(theme.accent)),
            Span::styled("  |  ", Style::default().fg(theme.border)),
            Span::styled(
                format!("{} changed", file_count),
                Style::default().fg(theme.dimmed),
            ),
            Span::styled("  ", Style::default()),
            Span::styled(
                format!("+{}", additions),
                Style::default().fg(theme.diff_add),
            ),
            Span::styled(" ", Style::default()),
            Span::styled(
                format!("-{}", deletions),
                Style::default().fg(theme.diff_delete),
            ),
        ]);

        frame.render_widget(Paragraph::new(header), inner);
    }

    fn render_content(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        // Split into file list (left) and diff content (right)
        // On small screens, cap file list width so the diff pane gets adequate space
        let effective_file_list_width = self
            .file_list_width
            .min(area.width.saturating_sub(40))
            .max(5);
        let layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(effective_file_list_width),
                Constraint::Min(40),
            ])
            .split(area);

        self.render_file_list(frame, layout[0], theme);
        self.render_diff_content(frame, layout[1], theme);
    }

    fn render_file_list(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::default()
            .title(" Files ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.border))
            .padding(Padding::horizontal(1));

        let inner = block.inner(area);
        self.file_list_inner = inner;
        frame.render_widget(block, area);

        if self.files.is_empty() {
            let msg = Paragraph::new("No changes").style(Style::default().fg(theme.dimmed));
            frame.render_widget(msg, inner);
            return;
        }

        self.ensure_selected_file_visible_in_list(inner.height as usize);
        let start = self.file_list_scroll_offset;
        let visible_rows = inner.height as usize;

        // Available width for the file path text (subtract borders, padding, prefix, status)
        let max_path_width = inner.width.saturating_sub(4) as usize; // "  M " = 4 chars

        let items: Vec<ListItem> = self
            .files
            .iter()
            .enumerate()
            .skip(start)
            .take(visible_rows)
            .map(|(i, file)| {
                let is_selected = i == self.selected_file;

                let status_color = match file.status {
                    FileStatus::Added => theme.diff_add,
                    FileStatus::Modified => theme.diff_modified,
                    FileStatus::Deleted => theme.diff_delete,
                    FileStatus::Renamed => theme.diff_header,
                    FileStatus::Copied => theme.diff_header,
                    FileStatus::Untracked => theme.dimmed,
                    FileStatus::Conflicted => theme.diff_modified,
                };

                let style = if is_selected {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.dimmed)
                };

                let prefix = if is_selected { "> " } else { "  " };

                let display_path = if is_selected {
                    // Selected: show full path, truncate from left with ellipsis
                    let full = file.path.to_string_lossy();
                    truncate_left(&full, max_path_width)
                } else {
                    // Not selected: show filename only
                    file.path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("?")
                        .to_string()
                };

                let line = Line::from(vec![
                    Span::styled(prefix, style),
                    Span::styled(
                        format!("{} ", file.status.indicator()),
                        Style::default().fg(status_color),
                    ),
                    Span::styled(display_path, style),
                ]);

                ListItem::new(line)
            })
            .collect();

        let list = List::new(items);
        frame.render_widget(list, inner);
    }

    /// Build one side of a split row: right-justified line number, a +/-/space
    /// marker, and content truncated to `content_w` columns. `None` renders an
    /// empty cell of the same width.
    fn split_cell<'a>(
        line: Option<&'a crate::git::diff::DiffLine>,
        is_left: bool,
        num_width: usize,
        content_w: usize,
        theme: &Theme,
    ) -> Vec<Span<'a>> {
        // Every cell occupies exactly this width (line number + space + marker
        // + padded content) so both columns line up and the divider draws as a
        // straight vertical line regardless of content length.
        let cell_w = num_width + 2 + content_w;
        match line {
            None => vec![Span::raw(" ".repeat(cell_w))],
            Some(l) => {
                let (prefix, style) = match l.tag {
                    ChangeTag::Delete => ("-", Style::default().fg(theme.diff_delete)),
                    ChangeTag::Insert => ("+", Style::default().fg(theme.diff_add)),
                    ChangeTag::Equal => (" ", Style::default().fg(theme.dimmed)),
                };
                let num = if is_left {
                    l.old_line_num
                } else {
                    l.new_line_num
                };
                let num_str = num
                    .map(|n| format!("{:>w$}", n, w = num_width))
                    .unwrap_or_else(|| " ".repeat(num_width));
                // Measure and pad by terminal column width, not scalar count,
                // so wide (CJK/emoji) characters keep the two columns aligned.
                let raw = l.content.trim_end_matches('\n');
                let mut content = if UnicodeWidthStr::width(raw) > content_w {
                    let budget = content_w.saturating_sub(1);
                    let mut used = 0usize;
                    let mut truncated = String::new();
                    for ch in raw.chars() {
                        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                        if used + cw > budget {
                            break;
                        }
                        used += cw;
                        truncated.push(ch);
                    }
                    truncated.push('\u{2026}');
                    truncated
                } else {
                    raw.to_string()
                };
                let used = UnicodeWidthStr::width(content.as_str());
                if used < content_w {
                    content.push_str(&" ".repeat(content_w - used));
                }
                vec![
                    Span::styled(format!("{} ", num_str), Style::default().fg(theme.dimmed)),
                    Span::styled(prefix.to_string(), style),
                    Span::styled(content, style),
                ]
            }
        }
    }

    fn render_diff_content(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let title = self
            .selected_file()
            .map(|f| format!(" {} ", f.path.display()))
            .unwrap_or_else(|| " Diff ".to_string());

        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if let Some(file) = self.files.get(self.selected_file) {
            if let Some(diff) = self.diff_cache.get(&file.path) {
                if diff.is_binary {
                    let msg =
                        Paragraph::new("Binary file").style(Style::default().fg(theme.dimmed));
                    frame.render_widget(msg, inner);
                    return;
                }

                // Compute max line number for dynamic width
                let max_line_num = diff
                    .hunks
                    .iter()
                    .flat_map(|h| &h.lines)
                    .flat_map(|l| l.old_line_num.into_iter().chain(l.new_line_num))
                    .max()
                    .unwrap_or(0);
                let num_width = max_line_num.max(1).ilog10() as usize + 1;
                let blank: String = " ".repeat(num_width);

                let split = self.split_view && inner.width >= 80;
                let half_content_w =
                    ((inner.width as usize).saturating_sub(2) / 2).saturating_sub(num_width + 3);

                // Build all diff lines
                let mut lines: Vec<Line> = Vec::new();

                for hunk in &diff.hunks {
                    let header = format!(
                        "@@ -{},{} +{},{} @@",
                        hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
                    );
                    lines.push(Line::from(Span::styled(
                        header,
                        Style::default().fg(theme.diff_header),
                    )));

                    if split {
                        for row in super::split::build_split_rows(hunk) {
                            let mut spans =
                                Self::split_cell(row.left, true, num_width, half_content_w, theme);
                            spans.push(Span::styled(
                                " \u{2502} ",
                                Style::default().fg(theme.border),
                            ));
                            spans.extend(Self::split_cell(
                                row.right,
                                false,
                                num_width,
                                half_content_w,
                                theme,
                            ));
                            lines.push(Line::from(spans));
                        }
                    } else {
                        for line in &hunk.lines {
                            let (prefix, style) = match line.tag {
                                ChangeTag::Delete => ("-", Style::default().fg(theme.diff_delete)),
                                ChangeTag::Insert => ("+", Style::default().fg(theme.diff_add)),
                                ChangeTag::Equal => (" ", Style::default().fg(theme.dimmed)),
                            };

                            let old_num = line
                                .old_line_num
                                .map(|n| format!("{:>w$}", n, w = num_width))
                                .unwrap_or_else(|| blank.clone());
                            let new_num = line
                                .new_line_num
                                .map(|n| format!("{:>w$}", n, w = num_width))
                                .unwrap_or_else(|| blank.clone());

                            let content = line.content.trim_end_matches('\n');

                            lines.push(Line::from(vec![
                                Span::styled(
                                    format!("{} {} ", old_num, new_num),
                                    Style::default().fg(theme.dimmed),
                                ),
                                Span::styled(prefix, style),
                                Span::styled(content, style),
                            ]));
                        }
                    }

                    lines.push(Line::from(""));
                }

                // Update dimensions from actual content
                let total_lines = lines.len();
                let visible_lines = inner.height as usize;
                self.total_lines = total_lines as u16;
                self.visible_lines = visible_lines as u16;

                // Clamp scroll offset to valid range
                let max_scroll = total_lines.saturating_sub(visible_lines);
                if (self.scroll_offset as usize) > max_scroll {
                    self.scroll_offset = max_scroll as u16;
                }

                // Apply scrolling
                let scroll = self.scroll_offset as usize;
                let visible: Vec<Line> =
                    lines.into_iter().skip(scroll).take(visible_lines).collect();

                let paragraph = Paragraph::new(visible);
                frame.render_widget(paragraph, inner);

                // Render scrollbar
                if total_lines > visible_lines {
                    let scrollbar_area = Rect {
                        x: area.x + area.width - 1,
                        y: area.y + 1,
                        width: 1,
                        height: area.height.saturating_sub(2),
                    };
                    let mut scrollbar_state = ScrollbarState::new(max_scroll + 1).position(scroll);
                    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                        .begin_symbol(Some("↑"))
                        .end_symbol(Some("↓"));
                    frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
                }
            } else {
                let msg =
                    Paragraph::new("Loading diff...").style(Style::default().fg(theme.dimmed));
                frame.render_widget(msg, inner);
            }
        } else {
            let msg = Paragraph::new("No file selected").style(Style::default().fg(theme.dimmed));
            frame.render_widget(msg, inner);
        }
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(theme.border));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        // Show error or success message, or help text
        let content = if let Some(ref error) = self.error_message {
            Line::from(Span::styled(error, Style::default().fg(theme.error)))
        } else if let Some(ref success) = self.success_message {
            Line::from(Span::styled(success, Style::default().fg(theme.diff_add)))
        } else {
            Line::from(vec![
                Span::styled("j/k", Style::default().fg(theme.accent)),
                Span::styled(": files  ", Style::default().fg(theme.dimmed)),
                Span::styled("h/l", Style::default().fg(theme.accent)),
                Span::styled(": resize  ", Style::default().fg(theme.dimmed)),
                Span::styled("scroll", Style::default().fg(theme.accent)),
                Span::styled(": diff  ", Style::default().fg(theme.dimmed)),
                Span::styled("e/Enter", Style::default().fg(theme.accent)),
                Span::styled(": edit  ", Style::default().fg(theme.dimmed)),
                Span::styled("b", Style::default().fg(theme.accent)),
                Span::styled(": branch  ", Style::default().fg(theme.dimmed)),
                Span::styled("y", Style::default().fg(theme.accent)),
                Span::styled(": copy path  ", Style::default().fg(theme.dimmed)),
                Span::styled("s", Style::default().fg(theme.accent)),
                // Name the layout `s` switches TO, so the hint stays correct
                // once you are already in split view.
                Span::styled(
                    if self.split_view {
                        ": unified  "
                    } else {
                        ": split  "
                    },
                    Style::default().fg(theme.dimmed),
                ),
                Span::styled("?", Style::default().fg(theme.accent)),
                Span::styled(": help  ", Style::default().fg(theme.dimmed)),
                Span::styled("q/Esc", Style::default().fg(theme.accent)),
                Span::styled(": close", Style::default().fg(theme.dimmed)),
            ])
        };

        let paragraph = Paragraph::new(content).alignment(ratatui::layout::Alignment::Center);
        frame.render_widget(paragraph, inner);
    }

    fn render_with_branch_dialog(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        // Render the normal diff view first
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(3),
            ])
            .split(area);

        self.render_header(frame, layout[0], theme);
        self.render_content(frame, layout[1], theme);
        self.render_footer(frame, layout[2], theme);

        // Render branch selection dialog overlay
        let Some(state) = &self.branch_select else {
            return;
        };

        // Center the dialog. Height caps at 20 but grows to fit when fewer branches;
        // when branches overflow, scroll indicators handle the remainder.
        let dialog_width = 40u16;
        let dialog_height = (state.branches.len() as u16 + 2).clamp(3, 20);
        let dialog_x = (area.width.saturating_sub(dialog_width)) / 2;
        let dialog_y = (area.height.saturating_sub(dialog_height)) / 2;

        let dialog_area = Rect {
            x: area.x + dialog_x,
            y: area.y + dialog_y,
            width: dialog_width,
            height: dialog_height,
        };

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .title(" Select Branch ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .style(Style::default().bg(theme.background));

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let scroll = crate::tui::components::scroll::calculate_scroll(
            state.branches.len(),
            state.selected,
            inner.height as usize,
        );

        let mut lines: Vec<Line> = Vec::new();

        if scroll.has_more_above {
            lines.push(Line::from(Span::styled(
                format!("  [{} more above]", scroll.scroll_offset),
                Style::default().fg(theme.dimmed),
            )));
        }

        for (i, branch) in state
            .branches
            .iter()
            .enumerate()
            .skip(scroll.scroll_offset)
            .take(scroll.list_visible)
        {
            let is_selected = i == state.selected;
            let is_current = branch == &self.base_branch;

            let style = if is_selected {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.text)
            };

            let prefix = if is_selected { "> " } else { "  " };
            let suffix = if is_current { " (current)" } else { "" };

            lines.push(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(branch.as_str(), style),
                Span::styled(suffix, Style::default().fg(theme.dimmed)),
            ]));
        }

        if scroll.has_more_below {
            let remaining = state
                .branches
                .len()
                .saturating_sub(scroll.scroll_offset + scroll.list_visible);
            lines.push(Line::from(Span::styled(
                format!("  [{} more below]", remaining),
                Style::default().fg(theme.dimmed),
            )));
        }

        frame.render_widget(Paragraph::new(lines), inner);

        // Render scrollbar when branches overflow, matching the diff content pane style
        if state.branches.len() > inner.height as usize {
            let max_scroll = state.branches.len().saturating_sub(inner.height as usize);
            let scrollbar_area = Rect {
                x: dialog_area.x + dialog_area.width - 1,
                y: dialog_area.y + 1,
                width: 1,
                height: dialog_area.height.saturating_sub(2),
            };
            let mut scrollbar_state =
                ScrollbarState::new(max_scroll + 1).position(scroll.scroll_offset);
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓"));
            frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
        }
    }

    fn render_help(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let dialog_width = 55u16;
        let dialog_height = 21u16;

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
            .title(" Diff View Help ")
            .title_style(
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            );

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let shortcuts = vec![
            (
                "Navigation",
                vec![
                    ("j/k, ↑/↓", "Navigate between files"),
                    ("PgUp/Dn", "Page up / down in diff"),
                    ("Ctrl+u/d", "Half-page up / down"),
                    ("g/G", "Go to top / bottom of diff"),
                    ("h/l, ←/→", "Shrink / grow file list"),
                ],
            ),
            (
                "Actions",
                vec![
                    ("e/Enter", "Edit file in external editor"),
                    ("b", "Select base branch"),
                    ("r", "Refresh diff"),
                    ("y", "Copy file path to clipboard"),
                    ("s", "Toggle side-by-side (split) layout"),
                ],
            ),
            (
                "Other",
                vec![("?", "Toggle this help"), ("q/Esc", "Close diff view")],
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
                    Span::styled(format!("  {:14}", key), Style::default().fg(theme.help_key)),
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
    use super::*;
    use crate::tui::diff::BranchSelectState;
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn make_view_with_branches(branches: Vec<String>, selected: usize) -> DiffView {
        let mut view = DiffView::test_default();
        view.branch_select = Some(BranchSelectState { branches, selected });
        view
    }

    fn render_dialog_to_string(view: &mut DiffView) -> String {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = load_theme("empire");
        terminal
            .draw(|f| {
                let area = f.area();
                view.render(f, area, &theme);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn branch_select_shows_more_below_when_overflowing() {
        let branches: Vec<String> = (0..40).map(|i| format!("branch-{:02}", i)).collect();
        let mut view = make_view_with_branches(branches, 0);
        let out = render_dialog_to_string(&mut view);
        assert!(
            out.contains("more below"),
            "expected '[N more below]' indicator when branches overflow dialog, got:\n{out}"
        );
        assert!(!out.contains("more above"));
    }

    #[test]
    fn branch_select_shows_more_above_when_cursor_near_end() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let branches: Vec<String> = (0..40).map(|i| format!("branch-{:02}", i)).collect();
        let mut view = make_view_with_branches(branches, 0);

        // Walk cursor to the last branch.
        for _ in 0..39 {
            view.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        }

        let out = render_dialog_to_string(&mut view);
        assert!(
            out.contains("more above"),
            "expected '[N more above]' indicator when cursor is near end, got:\n{out}"
        );
        assert!(!out.contains("more below"));
        // Selected branch (last one) must be visible.
        assert!(
            out.contains("branch-39"),
            "selected branch must be rendered, got:\n{out}"
        );
    }

    fn render_diff_to_string(view: &mut DiffView, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = load_theme("empire");
        terminal
            .draw(|f| {
                let area = f.area();
                view.render(f, area, &theme);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn diff_file(path: &str) -> crate::git::diff::DiffFile {
        crate::git::diff::DiffFile {
            path: std::path::PathBuf::from(path),
            old_path: None,
            status: FileStatus::Modified,
            additions: 1,
            deletions: 0,
        }
    }

    #[test]
    fn file_list_render_keeps_selected_file_visible_after_scroll() {
        use crate::git::diff::{DiffHunk, DiffLine, FileDiff};

        let mut view = DiffView::test_default();
        view.files = (0..20)
            .map(|i| diff_file(&format!("src/file_{i:02}.rs")))
            .collect();
        view.selected_file = 14;

        let selected = view.files[14].clone();
        view.diff_cache.insert(
            selected.path.clone(),
            FileDiff {
                file: selected,
                hunks: vec![DiffHunk {
                    old_start: 1,
                    old_lines: 1,
                    new_start: 1,
                    new_lines: 1,
                    lines: vec![DiffLine {
                        tag: ChangeTag::Equal,
                        old_line_num: Some(1),
                        new_line_num: Some(1),
                        content: "selected file diff content\n".to_string(),
                    }],
                }],
                is_binary: false,
            },
        );

        let out = render_diff_to_string(&mut view, 100, 16);
        assert!(
            out.contains("> M src/file_14.rs"),
            "selected file marker must remain visible in the file list, got:\n{out}"
        );
        assert!(
            out.contains("selected file diff content"),
            "selected file diff content should render in the diff pane, got:\n{out}"
        );
    }

    #[test]
    fn split_view_renders_divider_and_both_sides() {
        use crate::git::diff::{DiffFile, DiffHunk, DiffLine, FileDiff};
        use std::path::PathBuf;

        let path = PathBuf::from("example.txt");
        let file = DiffFile {
            path: path.clone(),
            old_path: None,
            status: FileStatus::Modified,
            additions: 1,
            deletions: 1,
        };
        let diff = FileDiff {
            file: file.clone(),
            hunks: vec![DiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                lines: vec![
                    DiffLine {
                        tag: ChangeTag::Delete,
                        old_line_num: Some(1),
                        new_line_num: None,
                        content: "OLDCONTENT\n".to_string(),
                    },
                    DiffLine {
                        tag: ChangeTag::Insert,
                        old_line_num: None,
                        new_line_num: Some(1),
                        content: "NEWCONTENT\n".to_string(),
                    },
                ],
            }],
            is_binary: false,
        };

        let mut view = DiffView::test_default();
        view.files = vec![file];
        view.selected_file = 0;
        view.diff_cache.insert(path, diff);
        view.split_view = true;

        let out = render_diff_to_string(&mut view, 120, 24);
        assert!(
            out.contains('\u{2502}'),
            "expected the split divider, got:\n{out}"
        );
        assert!(
            out.contains("OLDCONTENT"),
            "expected old content on left side, got:\n{out}"
        );
        assert!(
            out.contains("NEWCONTENT"),
            "expected new content on right side, got:\n{out}"
        );
    }

    #[test]
    fn split_view_aligns_divider_into_one_column() {
        use crate::git::diff::{DiffFile, DiffHunk, DiffLine, FileDiff};
        use std::path::PathBuf;

        let path = PathBuf::from("a.txt");
        let dl = |tag, o: Option<usize>, n: Option<usize>, c: &str| DiffLine {
            tag,
            old_line_num: o,
            new_line_num: n,
            content: format!("{c}\n"),
        };
        let file = DiffFile {
            path: path.clone(),
            old_path: None,
            status: FileStatus::Modified,
            additions: 1,
            deletions: 1,
        };
        let diff = FileDiff {
            file: file.clone(),
            hunks: vec![DiffHunk {
                old_start: 1,
                old_lines: 3,
                new_start: 1,
                new_lines: 3,
                lines: vec![
                    // Deliberately varied left-content lengths: an unpadded
                    // column would put the divider at different offsets.
                    dl(ChangeTag::Equal, Some(1), Some(1), "short"),
                    dl(
                        ChangeTag::Delete,
                        Some(2),
                        None,
                        "a considerably longer line of content",
                    ),
                    dl(ChangeTag::Insert, None, Some(2), "x"),
                    dl(ChangeTag::Equal, Some(3), Some(3), "mid length"),
                ],
            }],
            is_binary: false,
        };

        let mut view = DiffView::test_default();
        view.files = vec![file];
        view.selected_file = 0;
        view.diff_cache.insert(path, diff);
        view.split_view = true;

        let out = render_diff_to_string(&mut view, 200, 20);
        // The split divider is rendered as " | " (space-pipe-space); panel
        // borders never have spaces on both sides, so this finds only the
        // split dividers. Every one must sit in the same column.
        let cols: Vec<usize> = out.lines().filter_map(|l| l.find(" \u{2502} ")).collect();
        assert!(
            cols.len() >= 3,
            "expected a divider on each split row, got {cols:?}:\n{out}"
        );
        assert!(
            cols.iter().all(|&c| c == cols[0]),
            "split divider must align into one column, got {cols:?}:\n{out}"
        );
    }

    #[test]
    fn branch_select_no_indicators_when_fits() {
        let branches: Vec<String> = (0..3).map(|i| format!("br-{i}")).collect();
        let mut view = make_view_with_branches(branches, 0);
        let out = render_dialog_to_string(&mut view);
        assert!(!out.contains("more above"));
        assert!(!out.contains("more below"));
        assert!(out.contains("br-0"));
        assert!(out.contains("br-1"));
        assert!(out.contains("br-2"));
    }
}
