//! Rendering for NewSessionDialog

use ratatui::prelude::*;
use ratatui::widgets::*;
use tui_input::Input;

use rattles::presets::prelude as spinners;

use super::{NewSessionDialog, FIELD_HELP, HELP_DIALOG_WIDTH};
use crate::tui::components::{
    focused_input_spans, input_scroll, profile_cycler_spans, render_text_field,
    render_text_field_with_ghost, render_tool_config_overlay, set_prefixed_input_cursor_position,
    tool_cycler_spans, tool_row_suffix_spans, visible_slice,
};
use crate::tui::styles::Theme;

/// What [`NewSessionDialog::render_list_field`] needs to draw one editable
/// list: the two it serves differ only in wording and whether the add/edit
/// input offers a ghost completion.
struct ListField<'a> {
    label: &'static str,
    /// Plural noun in the collapsed `[N <unit>]` summary.
    unit: &'static str,
    hint: &'static str,
    empty_hint: &'static str,
    entries: &'a [String],
    selected: usize,
    expanded: bool,
    editing: Option<&'a Input>,
    adding_new: bool,
    ghost: Option<String>,
    focused: bool,
}

impl NewSessionDialog {
    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        // Rebuilt every frame: a layout change moves every later field, so a
        // stale rect points at the wrong row. Clearing here also empties them
        // while an overlay replaces the main form.
        self.focusable_rects.clear();

        if self.loading {
            self.render_loading(frame, area, theme);
            return;
        }

        if self.sandbox_config_mode {
            self.render_sandbox_config(frame, area, theme);
            return;
        }

        if self.tool_config_mode {
            self.render_tool_config(frame, area, theme);
            return;
        }

        if self.worktree_config_mode {
            self.render_worktree_config(frame, area, theme);
            return;
        }

        let has_profile_selection = self.has_profile_selection();
        let has_tool_selection = self.available_tools.len() > 1;
        let is_host_only = self.selected_tool_host_only();
        let has_sandbox = self.docker_available && !is_host_only;
        let has_yolo = !self.selected_tool_always_yolo();
        let has_structured = self.structured_capable;
        let dialog_width = 80;
        // Captured before the loop below shadows `area` per field, or the
        // centered pickers anchor to whichever row it last held.
        let full_area = area;
        // A profile description adds a line under the name; computed once so
        // the constraint and the renderer agree on the height.
        let profile_field_height: u16 =
            if has_profile_selection && self.selected_profile_description().is_some() {
                3
            } else {
                2
            };

        let mut constraints = Vec::new();
        if has_profile_selection {
            constraints.push(Constraint::Length(profile_field_height)); // Profile
        }
        constraints.extend([
            Constraint::Length(2), // Title
            Constraint::Length(2), // Path
            Constraint::Length(2), // Tool (always shown, interactive or not)
        ]);
        if has_structured {
            constraints.push(Constraint::Length(2)); // Structured view checkbox
        }
        if has_yolo {
            constraints.push(Constraint::Length(2)); // YOLO mode checkbox
        }
        if !is_host_only {
            constraints.push(Constraint::Length(2)); // Worktree Branch
        }
        if has_sandbox {
            constraints.push(Constraint::Length(2)); // Sandbox checkbox (summary only)
        }
        constraints.push(Constraint::Length(2)); // Group (always, at the bottom)

        // Inner width is 76: dialog width less borders and margin. The hint
        // line reserves 2 rows so per-field hints wrap instead of truncating.
        let error_lines: u16 = if let Some(error) = &self.error_message {
            let inner_width = (dialog_width - 4) as usize;
            let error_text = format!("✗ Error: {}", error);
            let needed = (error_text.len() as u16).div_ceil(inner_width as u16);
            needed.clamp(2, 6)
        } else {
            2
        };
        constraints.push(Constraint::Min(error_lines)); // Hints/errors

        let fields_height: u16 = constraints
            .iter()
            .map(|c| match c {
                Constraint::Length(n) => *n,
                Constraint::Min(n) => *n,
                _ => 0,
            })
            .sum();
        let dialog_height = fields_height + 4; // +2 border, +2 margin

        let block = crate::tui::dialogs::dialog_block(" New Session ", theme);
        let (_, inner) = crate::tui::dialogs::render_dialog_frame(
            frame,
            area,
            dialog_width,
            dialog_height,
            block,
        );

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints(constraints)
            .split(inner);

        let mut ci = 0; // chunk index

        let fields = self.field_indices();

        if has_profile_selection {
            let area = chunks[ci];
            self.render_profile_field(frame, area, theme);
            self.focusable_rects.push((0, area));
            ci += 1;
        }

        // Path precedes title: pick the directory before naming the session.
        let path_field_idx = self.path_field();
        let path_placeholder = if self.focused_field == path_field_idx {
            Some("(Ctrl+P to browse directories)")
        } else {
            None
        };
        let area = chunks[ci];
        self.render_path_field(frame, area, path_placeholder, theme);
        self.focusable_rects.push((path_field_idx, area));
        ci += 1;

        let area = chunks[ci];
        render_text_field(
            frame,
            area,
            "Title:",
            &self.title,
            self.focused_field == fields.title,
            Some("(random civ)"),
            theme,
        );
        self.focusable_rects.push((fields.title, area));
        ci += 1;

        // Always shown, interactive or read-only. Cycler and suffix ordering
        // are shared with the Restart dialog.
        let is_tool_focused = has_tool_selection && self.focused_field == fields.tool;
        let selected_tool = self.available_tools[self.tool_index].as_str();
        let mut tool_spans = tool_cycler_spans(
            "Tool:",
            selected_tool,
            self.tool_index,
            self.available_tools.len(),
            is_tool_focused,
            theme,
        );
        let has_config =
            !self.extra_args.value().is_empty() || !self.command_override.value().is_empty();
        tool_spans.extend(tool_row_suffix_spans(
            selected_tool,
            has_config,
            is_tool_focused,
            theme,
        ));
        let area = chunks[ci];
        frame.render_widget(Paragraph::new(Line::from(tool_spans)), area);
        // A read-only tool row must not accept focus on click.
        if has_tool_selection {
            self.focusable_rects.push((fields.tool, area));
        }
        ci += 1;

        if has_structured {
            let is_focused = self.focused_field == fields.structured;
            let label_style = if is_focused {
                Style::default().fg(theme.accent).underlined()
            } else {
                Style::default().fg(theme.text)
            };
            let checkbox = if self.structured_enabled {
                "[x]"
            } else {
                "[ ]"
            };
            let checkbox_style = if self.structured_enabled {
                Style::default().fg(theme.accent).bold()
            } else {
                Style::default().fg(theme.dimmed)
            };
            let text_style = if self.structured_enabled {
                Style::default().fg(theme.accent)
            } else {
                Style::default().fg(theme.dimmed)
            };
            let mut spans = vec![
                Span::styled("Structured:", label_style),
                Span::raw(" "),
                Span::styled(checkbox, checkbox_style),
                Span::styled(" Structured view instead of terminal", text_style),
            ];
            if self.structured_enabled {
                spans.push(Span::styled(
                    "  (runs under aoe serve)",
                    Style::default().fg(theme.dimmed),
                ));
            }
            let area = chunks[ci];
            frame.render_widget(Paragraph::new(Line::from(spans)), area);
            self.focusable_rects.push((fields.structured, area));
            ci += 1;
        }

        if has_yolo {
            let is_yolo_focused = self.focused_field == fields.yolo;
            let yolo_label_style = if is_yolo_focused {
                Style::default().fg(theme.accent).underlined()
            } else {
                Style::default().fg(theme.text)
            };

            let yolo_checkbox = if self.yolo_mode { "[x]" } else { "[ ]" };
            let yolo_checkbox_style = if self.yolo_mode {
                Style::default().fg(theme.accent).bold()
            } else {
                Style::default().fg(theme.dimmed)
            };

            let yolo_line = Line::from(vec![
                Span::styled("YOLO Mode:", yolo_label_style),
                Span::raw(" "),
                Span::styled(yolo_checkbox, yolo_checkbox_style),
                Span::styled(
                    " Skip permission prompts",
                    if self.yolo_mode {
                        Style::default().fg(theme.accent)
                    } else {
                        Style::default().fg(theme.dimmed)
                    },
                ),
            ]);
            let area = chunks[ci];
            frame.render_widget(Paragraph::new(yolo_line), area);
            self.focusable_rects.push((fields.yolo, area));
            ci += 1;
        }

        if !is_host_only {
            let is_wt_focused = self.focused_field == fields.worktree;
            let label_style = if is_wt_focused {
                Style::default().fg(theme.accent).underlined()
            } else {
                Style::default().fg(theme.text)
            };
            let checkbox = if self.worktree_enabled { "[x]" } else { "[ ]" };
            let checkbox_style = if self.worktree_enabled {
                Style::default().fg(theme.accent).bold()
            } else {
                Style::default().fg(theme.dimmed)
            };
            let text_style = if self.worktree_enabled {
                Style::default().fg(theme.accent)
            } else {
                Style::default().fg(theme.dimmed)
            };

            let mut spans = vec![
                Span::styled("Worktree:", label_style),
                Span::raw(" "),
                Span::styled(checkbox, checkbox_style),
                Span::styled(" Create worktree", text_style),
            ];

            if self.worktree_enabled {
                let name = self.worktree_branch.value().trim();
                let branch_mode = if self.create_new_branch {
                    "new"
                } else {
                    "existing"
                };
                let repos_count = self.workspace_repos.len();
                let summary = match (name.is_empty(), repos_count) {
                    (true, 0) => None,
                    (true, n) => Some(format!("  (auto, {}, {} repos)", branch_mode, n)),
                    (false, 0) => Some(format!("  ({}, {})", name, branch_mode)),
                    (false, n) => Some(format!("  ({}, {}, {} repos)", name, branch_mode, n)),
                };
                if let Some(summary) = summary {
                    spans.push(Span::styled(summary, Style::default().fg(theme.dimmed)));
                }
            }

            if self.worktree_enabled {
                spans.push(Span::styled(
                    "  (Ctrl+P to configure)",
                    Style::default().fg(theme.dimmed),
                ));
            }

            let area = chunks[ci];
            frame.render_widget(Paragraph::new(Line::from(spans)), area);
            self.focusable_rects.push((fields.worktree, area));
            ci += 1;
        }

        if has_sandbox {
            let is_sandbox_focused = self.focused_field == fields.sandbox;
            let sandbox_label_style = if is_sandbox_focused {
                Style::default().fg(theme.accent).underlined()
            } else {
                Style::default().fg(theme.text)
            };

            let checkbox = if self.sandbox_enabled { "[x]" } else { "[ ]" };
            let checkbox_style = if self.sandbox_enabled {
                Style::default().fg(theme.accent).bold()
            } else {
                Style::default().fg(theme.dimmed)
            };

            let mut spans = vec![
                Span::styled("Sandbox:", sandbox_label_style),
                Span::raw(" "),
                Span::styled(checkbox, checkbox_style),
                Span::styled(
                    " Run in container",
                    if self.sandbox_enabled {
                        Style::default().fg(theme.accent)
                    } else {
                        Style::default().fg(theme.dimmed)
                    },
                ),
            ];

            if self.sandbox_enabled {
                spans.push(Span::styled(
                    "  (Ctrl+P to configure)",
                    Style::default().fg(theme.dimmed),
                ));
            }

            let area = chunks[ci];
            frame.render_widget(Paragraph::new(Line::from(spans)), area);
            self.focusable_rects.push((fields.sandbox, area));
            ci += 1;
        }

        // Group (always visible, at the bottom before hints)
        let group_placeholder =
            if !self.existing_groups.is_empty() && self.focused_field == fields.group {
                Some("(Ctrl+P to browse groups)")
            } else {
                None
            };
        let area = chunks[ci];
        render_text_field_with_ghost(
            frame,
            area,
            "Group:",
            &self.group,
            self.focused_field == fields.group,
            group_placeholder,
            self.group_ghost_text(),
            theme,
        );
        self.focusable_rects.push((fields.group, area));
        ci += 1;

        // Hints/errors (last chunk)
        let hint_chunk = ci;
        if self.confirm_create_dir.is_some() {
            let selected = self.confirm_create_dir.unwrap_or(false);
            let yes_style = if selected {
                Style::default().fg(theme.accent).bold()
            } else {
                Style::default().fg(theme.dimmed)
            };
            let no_style = if !selected {
                Style::default().fg(theme.accent).bold()
            } else {
                Style::default().fg(theme.dimmed)
            };
            let line = Line::from(vec![
                Span::styled(
                    "⚠ Path does not exist. Create? ",
                    Style::default().fg(theme.error),
                ),
                Span::styled("[y]es", yes_style),
                Span::raw(" "),
                Span::styled("[N]o", no_style),
            ]);
            frame.render_widget(Paragraph::new(line), chunks[hint_chunk]);
        } else if let Some(error) = &self.error_message {
            let error_text = format!("✗ Error: {}", error);
            let error_paragraph = Paragraph::new(error_text)
                .style(Style::default().fg(theme.error))
                .wrap(Wrap { trim: true });
            frame.render_widget(error_paragraph, chunks[hint_chunk]);
        } else {
            let mut hint_spans = Vec::new();
            hint_spans.push(Span::styled("Tab", Style::default().fg(theme.hint)));
            hint_spans.push(Span::raw(" next  "));
            if has_tool_selection {
                hint_spans.push(Span::styled("←/→", Style::default().fg(theme.hint)));
                hint_spans.push(Span::raw(" tool  "));
            }
            if self.focused_field == self.path_field() {
                if self.ghost_text().is_some() {
                    hint_spans.push(Span::styled("→", Style::default().fg(theme.hint)));
                    hint_spans.push(Span::raw(" accept  "));
                }
                hint_spans.push(Span::styled("C-←/M-b", Style::default().fg(theme.hint)));
                hint_spans.push(Span::raw(" prev seg  "));
                hint_spans.push(Span::styled("Home/Ctrl+A", Style::default().fg(theme.hint)));
                hint_spans.push(Span::raw(" start  "));
                hint_spans.push(Span::styled("Ctrl+P", Style::default().fg(theme.hint)));
                hint_spans.push(Span::raw(" browse  "));
            }
            if self.focused_field == fields.group && !self.existing_groups.is_empty() {
                if self.group_ghost_text().is_some() {
                    hint_spans.push(Span::styled("→", Style::default().fg(theme.hint)));
                    hint_spans.push(Span::raw(" accept  "));
                }
                hint_spans.push(Span::styled("Ctrl+P", Style::default().fg(theme.hint)));
                hint_spans.push(Span::raw(" groups  "));
            }
            if self.focused_field == fields.tool {
                hint_spans.push(Span::styled("Ctrl+P", Style::default().fg(theme.hint)));
                hint_spans.push(Span::raw(" configure  "));
            }
            if self.focused_field == fields.worktree && self.worktree_enabled {
                hint_spans.push(Span::styled("Ctrl+P", Style::default().fg(theme.hint)));
                hint_spans.push(Span::raw(" configure  "));
            }
            // Ctrl+T scratch chip. Always present so the binding is
            // discoverable without opening the `?` overlay. When focus is on
            // the Path row the chip is emphasized (bold accent) so users
            // about to type a path can see "you can skip this entirely with
            // Ctrl+T". When scratch is already on, the chip flips to the
            // undo verb and styles as accent so it reads as the active state.
            let path_focused = self.focused_field == self.path_field();
            let (scratch_key_style, scratch_label) = if self.scratch {
                (
                    Style::default().fg(theme.accent).bold(),
                    " scratch on (undo)  ",
                )
            } else if path_focused {
                (Style::default().fg(theme.accent).bold(), " scratch  ")
            } else {
                (Style::default().fg(theme.hint), " scratch  ")
            };
            hint_spans.push(Span::styled("Ctrl+T", scratch_key_style));
            hint_spans.push(Span::raw(scratch_label));

            hint_spans.push(Span::styled("Enter", Style::default().fg(theme.hint)));
            hint_spans.push(Span::raw(" create  "));
            hint_spans.push(Span::styled("?", Style::default().fg(theme.hint)));
            hint_spans.push(Span::raw(" help  "));
            hint_spans.push(Span::styled("Esc", Style::default().fg(theme.hint)));
            hint_spans.push(Span::raw(" cancel"));
            frame.render_widget(
                Paragraph::new(Line::from(hint_spans)).wrap(Wrap { trim: true }),
                chunks[hint_chunk],
            );
        }

        if self.show_help {
            self.render_help_overlay(frame, full_area, theme);
        }

        if self.group_picker.is_active() {
            self.group_picker.render(frame, full_area, theme);
        }

        if self.branch_picker.is_active() {
            self.branch_picker.render(frame, full_area, theme);
        }

        if self.projects_picker.is_active() {
            self.projects_picker.render(frame, full_area, theme);
        }

        if self.dir_picker.is_active() {
            self.dir_picker.render(frame, full_area, theme);
        }
    }

    fn render_profile_field(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let spans = profile_cycler_spans(
            "Profile:",
            self.selected_profile(),
            self.available_profiles.len(),
            self.focused_field == 0,
            theme,
        );

        let mut lines = vec![Line::from(spans)];
        // Show the selected profile's description on the line below when one
        // is set, so users can tell what each profile is for without leaving
        // the dialog. (#949)
        if let Some(desc) = self.selected_profile_description() {
            lines.push(Line::from(Span::styled(
                format!("  {}", desc),
                Style::default().fg(theme.dimmed),
            )));
        }

        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_path_field(
        &self,
        frame: &mut Frame,
        area: Rect,
        placeholder: Option<&str>,
        theme: &Theme,
    ) {
        let is_focused = self.focused_field == self.path_field();
        let flashing_invalid = self.is_path_invalid_flash_active();

        let label_color = if flashing_invalid {
            theme.error
        } else if is_focused {
            theme.accent
        } else {
            theme.text
        };
        let value_color = if flashing_invalid {
            theme.error
        } else if is_focused {
            theme.accent
        } else {
            theme.text
        };

        let label_style = if is_focused {
            Style::default().fg(label_color).underlined()
        } else {
            Style::default().fg(label_color)
        };
        let value_style = Style::default().fg(value_color);

        let value = self.path.value();
        let prefix_width = 6; // "Path: "
        let available_width = area.width.saturating_sub(prefix_width as u16) as usize;

        let mut spans = vec![Span::styled("Path:", label_style), Span::raw(" ")];

        // Scratch mode disables this field. The undo hint lives in the
        // bottom hint chip (`Ctrl+T scratch on (undo)`), so the marker
        // here can be terse.
        if self.scratch {
            spans.push(Span::styled(
                "(scratch directory)",
                Style::default().fg(theme.dimmed),
            ));
            frame.render_widget(Paragraph::new(Line::from(spans)), area);
            return;
        }

        if value.is_empty() && !is_focused {
            if let Some(placeholder_text) = placeholder {
                spans.push(Span::styled(placeholder_text, value_style));
            }
        } else if is_focused {
            let scroll = input_scroll(&self.path, available_width);
            let cursor_style = if flashing_invalid {
                Style::default().fg(theme.background).bg(theme.error)
            } else {
                Style::default().fg(theme.background).bg(theme.accent)
            };
            let (field_spans, end_visible) = focused_input_spans(
                value,
                self.path.cursor(),
                scroll,
                available_width,
                value_style,
                cursor_style,
            );
            spans.extend(field_spans);
            // Only show ghost when end of input is visible
            if end_visible {
                if let Some(ghost) = self.ghost_text() {
                    spans.push(Span::styled(ghost, Style::default().fg(theme.dimmed)));
                }
            }
        } else {
            let scroll = input_scroll(&self.path, available_width);
            let (visible, _) = visible_slice(value, scroll, available_width);
            spans.push(Span::styled(visible, value_style));
        }

        frame.render_widget(Paragraph::new(Line::from(spans)), area);

        if is_focused {
            set_prefixed_input_cursor_position(frame, area, "Path: ", &self.path);
        }
    }

    fn set_input_cursor_on_row(
        frame: &mut Frame,
        area: Rect,
        row: usize,
        prefix: &str,
        input: &Input,
    ) {
        if row >= area.height as usize {
            return;
        }
        let row_area = Rect {
            x: area.x,
            y: area.y.saturating_add(row as u16),
            width: area.width,
            height: 1,
        };
        set_prefixed_input_cursor_position(frame, row_area, prefix, input);
    }

    fn render_sandbox_config(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        self.sandbox_config_rects.clear();
        let dialog_width: u16 = 72;

        // Sandbox config fields: image, env, inherited
        let env_list_height: u16 = if self.env_list_expanded {
            (2 + self.extra_env.len() as u16).clamp(4, 8)
        } else {
            2
        };
        let inherited_height: u16 = 2 + self.inherited_settings.len().max(1) as u16;

        let constraints = vec![
            Constraint::Length(2),                // Image
            Constraint::Length(env_list_height),  // Environment
            Constraint::Length(inherited_height), // Inherited settings
            Constraint::Min(1),                   // Hints
        ];

        let fields_height: u16 = constraints
            .iter()
            .map(|c| match c {
                Constraint::Length(n) => *n,
                Constraint::Min(n) => *n,
                _ => 0,
            })
            .sum();
        let dialog_height = fields_height + 4;

        let block = crate::tui::dialogs::dialog_block(" Sandbox Configuration ", theme);
        let (_, inner) = crate::tui::dialogs::render_dialog_frame(
            frame,
            area,
            dialog_width,
            dialog_height,
            block,
        );

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints(constraints)
            .split(inner);

        let mut ci = 0;

        // Image field
        render_text_field(
            frame,
            chunks[ci],
            "Image:",
            &self.sandbox_image,
            self.sandbox_focused_field == 0,
            None,
            theme,
        );
        self.sandbox_config_rects.push((0, chunks[ci]));
        ci += 1;

        // Environment
        self.render_list_field(
            frame,
            chunks[ci],
            theme,
            ListField {
                label: "Environment",
                unit: "items",
                hint: " (a)dd (d)el (Enter)edit (Esc)close",
                empty_hint: "    (press 'a' to add KEY or KEY=VALUE)",
                entries: &self.extra_env,
                selected: self.env_selected_index,
                expanded: self.env_list_expanded,
                editing: self.env_editing_input.as_ref(),
                adding_new: self.env_adding_new,
                ghost: None,
                focused: self.sandbox_focused_field == 1,
            },
        );
        self.sandbox_config_rects.push((1, chunks[ci]));
        ci += 1;

        // Inherited settings (always visible, not focusable)
        self.render_inherited_field(frame, chunks[ci], theme);
        ci += 1;

        // Hints
        let hint_spans = vec![
            Span::styled("Tab", Style::default().fg(theme.hint)),
            Span::raw(" next  "),
            Span::styled("Enter", Style::default().fg(theme.hint)),
            Span::raw(" edit  "),
            Span::styled("Esc", Style::default().fg(theme.hint)),
            Span::raw(" back"),
        ];
        frame.render_widget(Paragraph::new(Line::from(hint_spans)), chunks[ci]);

        if self.show_help {
            self.render_help_overlay(frame, area, theme);
        }
    }

    fn render_tool_config(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let selected_tool = self
            .available_tools
            .get(self.tool_index)
            .or_else(|| self.available_tools.first())
            .map(|s| s.as_str())
            .unwrap_or("claude");
        self.tool_config_rects = render_tool_config_overlay(
            frame,
            area,
            selected_tool,
            &self.command_override,
            &self.extra_args,
            self.tool_config_focused_field,
            theme,
        );

        if self.show_help {
            self.render_help_overlay(frame, area, theme);
        }
    }

    fn render_worktree_config(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        self.worktree_config_rects.clear();
        let dialog_width: u16 = 72;

        let repos_height: u16 = if self.workspace_repos_expanded {
            (2 + self.workspace_repos.len() as u16).clamp(4, 8)
        } else {
            2
        };

        // Errors share the hint row, so size it to the wrapped text the way the
        // main dialog does; a long git error would otherwise clip to one line.
        let hint_height: u16 = if let Some(error) = &self.error_message {
            let inner_width = dialog_width - 4;
            let error_text = format!("✗ Error: {}", error);
            (error_text.len() as u16).div_ceil(inner_width).clamp(1, 6)
        } else {
            1
        };

        let constraints = vec![
            Constraint::Length(2),            // Name
            Constraint::Length(2),            // New Branch checkbox
            Constraint::Length(2),            // Base Branch
            Constraint::Length(repos_height), // Extra Repos
            Constraint::Min(hint_height),     // Hints or error
        ];

        let fields_height: u16 = constraints
            .iter()
            .map(|c| match c {
                Constraint::Length(n) => *n,
                Constraint::Min(n) => *n,
                _ => 0,
            })
            .sum();
        let dialog_height = fields_height + 4;

        let title = " Worktree Configuration ";

        let block = crate::tui::dialogs::dialog_block(title, theme);
        let (_, inner) = crate::tui::dialogs::render_dialog_frame(
            frame,
            area,
            dialog_width,
            dialog_height,
            block,
        );

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints(constraints)
            .split(inner);

        // Name
        render_text_field(
            frame,
            chunks[0],
            "Name:",
            &self.worktree_branch,
            self.worktree_config_focused_field == 0,
            Some("(empty = title)"),
            theme,
        );
        self.worktree_config_rects.push((0, chunks[0]));

        // New Branch checkbox
        {
            let is_focused = self.worktree_config_focused_field == 1;
            let label_style = if is_focused {
                Style::default().fg(theme.accent).underlined()
            } else {
                Style::default().fg(theme.text)
            };
            let checkbox = if self.create_new_branch { "[x]" } else { "[ ]" };
            let checkbox_style = if self.create_new_branch {
                Style::default().fg(theme.accent).bold()
            } else {
                Style::default().fg(theme.dimmed)
            };
            let text = if self.create_new_branch {
                "Create new branch"
            } else {
                "Attach to existing branch"
            };
            let text_style = if self.create_new_branch {
                Style::default().fg(theme.accent)
            } else {
                Style::default().fg(theme.dimmed)
            };
            let line = Line::from(vec![
                Span::styled("New Branch:", label_style),
                Span::raw(" "),
                Span::styled(checkbox, checkbox_style),
                Span::styled(format!(" {}", text), text_style),
            ]);
            frame.render_widget(Paragraph::new(line), chunks[1]);
            self.worktree_config_rects.push((1, chunks[1]));
        }

        // Base Branch (only meaningful when "new branch" is checked; when
        // unchecked we render the field dimmed so the layout stays stable).
        {
            let placeholder = if self.create_new_branch {
                "(empty = repo default)"
            } else {
                "(ignored: attaching to existing)"
            };
            render_text_field(
                frame,
                chunks[2],
                "Base:",
                &self.base_branch,
                self.worktree_config_focused_field == 2,
                Some(placeholder),
                theme,
            );
            self.worktree_config_rects.push((2, chunks[2]));
        }

        // Extra Repos
        self.render_list_field(
            frame,
            chunks[3],
            theme,
            ListField {
                label: "Extra Repos",
                unit: "repos",
                hint: " (a)dd (d)el (Enter)edit (Ctrl+P)browse (Esc)close",
                empty_hint: "    (press 'a' to add repo path)",
                entries: &self.workspace_repos,
                selected: self.workspace_repo_selected_index,
                expanded: self.workspace_repos_expanded,
                editing: self.workspace_repo_editing_input.as_ref(),
                adding_new: self.workspace_repo_adding_new,
                ghost: self
                    .workspace_repo_ghost
                    .as_ref()
                    .map(|g| g.ghost_text.clone()),
                focused: self.worktree_config_focused_field == 3,
            },
        );
        self.worktree_config_rects.push((3, chunks[3]));

        // Hints, or the pending error (branch listing failures land here so
        // Ctrl+P always produces visible feedback). See #3166.
        if let Some(error) = &self.error_message {
            let error_paragraph = Paragraph::new(format!("✗ Error: {}", error))
                .style(Style::default().fg(theme.error))
                .wrap(Wrap { trim: true });
            frame.render_widget(error_paragraph, chunks[4]);
        } else {
            let mut hint_spans = vec![
                Span::styled("Tab", Style::default().fg(theme.hint)),
                Span::raw(" next  "),
                Span::styled("Space", Style::default().fg(theme.hint)),
                Span::raw(" toggle  "),
                Span::styled("Ctrl+P", Style::default().fg(theme.hint)),
                Span::raw(" branches  "),
                Span::styled("Enter", Style::default().fg(theme.hint)),
                Span::raw(" done  "),
                Span::styled("Esc", Style::default().fg(theme.hint)),
                Span::raw(" back"),
            ];
            if self.worktree_config_focused_field == 3 && !self.workspace_repos_expanded {
                hint_spans = vec![
                    Span::styled("Tab", Style::default().fg(theme.hint)),
                    Span::raw(" next  "),
                    Span::styled("Enter", Style::default().fg(theme.hint)),
                    Span::raw(" edit repos  "),
                    Span::styled("Ctrl+R", Style::default().fg(theme.hint)),
                    Span::raw(" pick project  "),
                    Span::styled("Esc", Style::default().fg(theme.hint)),
                    Span::raw(" back"),
                ];
            }
            frame.render_widget(Paragraph::new(Line::from(hint_spans)), chunks[4]);
        }

        if self.show_help {
            self.render_help_overlay(frame, area, theme);
        }

        if self.branch_picker.is_active() {
            self.branch_picker.render(frame, area, theme);
        }

        if self.projects_picker.is_active() {
            self.projects_picker.render(frame, area, theme);
        }

        if self.dir_picker.is_active() {
            self.dir_picker.render(frame, area, theme);
        }
    }

    /// One editable list field: `Environment` and `Extra Repos` differ only in
    /// their labels, their summary unit, and whether the add/edit input offers
    /// a path ghost completion.
    fn render_list_field(&self, frame: &mut Frame, area: Rect, theme: &Theme, spec: ListField<'_>) {
        let label_style = if spec.focused {
            Style::default().fg(theme.accent).underlined()
        } else {
            Style::default().fg(theme.text)
        };
        let label = Span::styled(format!("{}:", spec.label), label_style);

        if !spec.expanded {
            let count = spec.entries.len();
            let (summary, style) = if count == 0 {
                (
                    "(empty - press Enter to add)".to_string(),
                    Style::default().fg(theme.dimmed),
                )
            } else {
                (
                    format!("[{count} {}]", spec.unit),
                    Style::default().fg(theme.accent),
                )
            };
            let line = Line::from(vec![label, Span::raw(" "), Span::styled(summary, style)]);
            frame.render_widget(Paragraph::new(line), area);
            return;
        }

        let mut lines = vec![Line::from(vec![
            label,
            Span::styled(spec.hint, Style::default().fg(theme.dimmed)),
        ])];
        let mut cursor_row: Option<(usize, &'static str, &Input)> = None;

        // The ghost, when offered, renders after the value only once the
        // visible window reaches the end of the input.
        let prefix_width = 4usize; // "  + " or "  > "
        let available_width = area.width.saturating_sub(prefix_width as u16) as usize;
        let input_line = |prefix: &'static str, input: &Input| -> Line<'static> {
            let scroll = input_scroll(input, available_width);
            let (visible_value, end_visible) =
                visible_slice(input.value(), scroll, available_width);
            let mut spans = vec![
                Span::styled(prefix, Style::default().fg(theme.accent)),
                Span::styled(visible_value, Style::default().fg(theme.accent).bold()),
            ];
            if end_visible {
                if let Some(ghost) = &spec.ghost {
                    spans.push(Span::styled(
                        ghost.clone(),
                        Style::default().fg(theme.dimmed),
                    ));
                }
            }
            spans.push(Span::styled("_", Style::default().fg(theme.accent)));
            Line::from(spans)
        };
        // A selected row keeps its marker while another item is being typed,
        // but drops the accent so the prompt reads as the active one.
        let entry_line = |index: usize, entry: &String, editing: bool| {
            let selected = index == spec.selected;
            let prefix = if selected { "  > " } else { "    " };
            let style = if selected && !editing {
                Style::default().fg(theme.accent).bold()
            } else {
                Style::default().fg(theme.text)
            };
            Line::from(Span::styled(format!("{prefix}{entry}"), style))
        };

        match spec.editing {
            Some(input) if spec.adding_new => {
                for (i, entry) in spec.entries.iter().enumerate() {
                    lines.push(entry_line(i, entry, true));
                }
                lines.push(input_line("  + ", input));
                cursor_row = Some((lines.len() - 1, "  + ", input));
            }
            Some(input) => {
                for (i, entry) in spec.entries.iter().enumerate() {
                    if i == spec.selected {
                        lines.push(input_line("  > ", input));
                        cursor_row = Some((lines.len() - 1, "  > ", input));
                    } else {
                        lines.push(entry_line(i, entry, true));
                    }
                }
            }
            None if spec.entries.is_empty() => lines.push(Line::from(Span::styled(
                spec.empty_hint,
                Style::default().fg(theme.dimmed),
            ))),
            None => {
                for (i, entry) in spec.entries.iter().enumerate() {
                    lines.push(entry_line(i, entry, false));
                }
            }
        }

        frame.render_widget(Paragraph::new(lines), area);
        if let Some((row, prefix, input)) = cursor_row {
            Self::set_input_cursor_on_row(frame, area, row, prefix, input);
        }
    }

    fn render_inherited_field(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let label_style = Style::default().fg(theme.dimmed);
        let mut lines: Vec<Line> = Vec::new();

        lines.push(Line::from(Span::styled("Inherited Settings:", label_style)));

        if self.inherited_settings.is_empty() {
            lines.push(Line::from(Span::styled(
                "    (all defaults)",
                Style::default().fg(theme.dimmed),
            )));
        } else {
            for (label, value) in &self.inherited_settings {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("    {}: ", label),
                        Style::default().fg(theme.dimmed),
                    ),
                    Span::styled(value.as_str(), Style::default().fg(theme.accent)),
                ]));
            }
        }

        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_help_overlay(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let has_tool_selection = self.available_tools.len() > 1;
        let has_sandbox = self.docker_available;
        let show_sandbox_options_help = has_sandbox && self.sandbox_enabled;

        let dialog_width: u16 = HELP_DIALOG_WIDTH;
        let has_profile_selection = self.has_profile_selection();
        // Base fields: Scratch, Title, Path, YOLO, Worktree, Group + close hint
        let base_height: u16 = 20;
        let dialog_height: u16 = base_height
            + if has_profile_selection { 3 } else { 0 }
            + if has_tool_selection { 3 } else { 0 }
            + if has_sandbox { 3 } else { 0 }
            + if show_sandbox_options_help { 12 } else { 0 };

        let block = crate::tui::dialogs::toned_dialog_block(
            " New Session Help ",
            theme.border,
            theme.title,
        );
        let (_, inner) = crate::tui::dialogs::render_dialog_frame(
            frame,
            area,
            dialog_width,
            dialog_height,
            block,
        );

        let mut lines: Vec<Line> = Vec::new();

        // Gate by name (not index) so inserting a new FIELD_HELP entry does
        // not silently shift every condition by one.
        for help in FIELD_HELP {
            let show = match help.name {
                "Profile" => has_profile_selection,
                "Tool" => has_tool_selection,
                "YOLO Mode" => !self.selected_tool_always_yolo(),
                "Sandbox" => has_sandbox,
                "Image" | "Environment" => show_sandbox_options_help,
                _ => true,
            };
            if !show {
                continue;
            }
            lines.push(Line::from(Span::styled(
                help.name,
                Style::default().fg(theme.accent).bold(),
            )));
            lines.push(Line::from(Span::styled(
                format!("  {}", help.description),
                Style::default().fg(theme.text),
            )));
            lines.push(Line::from(""));
        }

        lines.push(Line::from(vec![
            Span::styled("Press ", Style::default().fg(theme.dimmed)),
            Span::styled("?", Style::default().fg(theme.hint)),
            Span::styled(" or ", Style::default().fg(theme.dimmed)),
            Span::styled("Esc", Style::default().fg(theme.hint)),
            Span::styled(" to close", Style::default().fg(theme.dimmed)),
        ]));

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_loading(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let needs_extra_line = self.sandbox_enabled;
        let show_hook_output = self.has_hooks;
        let max_output_lines: usize = 6;

        let dialog_width: u16 = if show_hook_output {
            70
        } else if needs_extra_line {
            55
        } else {
            50
        };
        let dialog_height: u16 = if show_hook_output {
            (6 + max_output_lines as u16).min(area.height)
        } else if needs_extra_line {
            9
        } else {
            7
        };

        let dialog_area = crate::tui::dialogs::centered_rect(area, dialog_width, dialog_height);

        frame.render_widget(Clear, dialog_area);

        let title = if show_hook_output {
            " Running Hooks "
        } else {
            " Creating Session "
        };

        let block = crate::tui::dialogs::dialog_block(title, theme);

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let spinner = spinners::orbit()
            .set_interval(std::time::Duration::from_millis(400))
            .current_frame();

        if show_hook_output {
            let mut lines = vec![];

            let status_text = if let Some(ref cmd) = self.current_hook {
                let max_cmd_len = (dialog_width as usize).saturating_sub(12);
                if cmd.len() > max_cmd_len {
                    let truncated: String =
                        cmd.chars().take(max_cmd_len.saturating_sub(3)).collect();
                    format!("{}...", truncated)
                } else {
                    cmd.clone()
                }
            } else {
                "Preparing...".to_string()
            };

            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {} ", spinner),
                    Style::default().fg(theme.accent).bold(),
                ),
                Span::styled(status_text, Style::default().fg(theme.text)),
            ]));

            let output_start = self.hook_output.len().saturating_sub(max_output_lines);
            let visible_lines = &self.hook_output[output_start..];
            let inner_width = (dialog_width as usize).saturating_sub(6);

            for line in visible_lines {
                let truncated = if line.len() > inner_width {
                    let t: String = line.chars().take(inner_width.saturating_sub(3)).collect();
                    format!("{}...", t)
                } else {
                    line.clone()
                };
                lines.push(Line::from(Span::styled(
                    format!("  {}", truncated),
                    Style::default().fg(theme.dimmed),
                )));
            }

            let used = 1 + visible_lines.len();
            let available = dialog_height.saturating_sub(4) as usize;
            for _ in used..available {
                lines.push(Line::from(""));
            }

            lines.push(Line::from(vec![
                Span::styled(" Press ", Style::default().fg(theme.dimmed)),
                Span::styled("Esc", Style::default().fg(theme.hint)),
                Span::styled(" to cancel", Style::default().fg(theme.dimmed)),
            ]));

            frame.render_widget(Paragraph::new(lines), inner);
        } else {
            let loading_text = if self.sandbox_enabled {
                "Setting up sandbox..."
            } else {
                "Creating session..."
            };

            let mut lines = vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled(
                        format!("  {} ", spinner),
                        Style::default().fg(theme.accent).bold(),
                    ),
                    Span::styled(loading_text, Style::default().fg(theme.text)),
                ]),
            ];

            if needs_extra_line {
                lines.push(Line::from(Span::styled(
                    "    (first time may take a few minutes)",
                    Style::default().fg(theme.dimmed),
                )));
            }

            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("  Press ", Style::default().fg(theme.dimmed)),
                Span::styled("Esc", Style::default().fg(theme.hint)),
                Span::styled(" to cancel", Style::default().fg(theme.dimmed)),
            ]));

            frame.render_widget(Paragraph::new(lines), inner);
        }
    }
}
