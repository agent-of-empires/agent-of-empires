//! Input handling for the settings view

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use crate::tui::dialogs::{CustomInstructionDialog, DialogResult};

use super::{FieldKey, FieldValue, ListEditState, SettingsFocus, SettingsScope, SettingsView};

/// Result of handling a key event in the settings view
pub enum SettingsAction {
    /// Continue showing the settings view
    Continue,
    /// Close the settings view (with optional unsaved changes warning)
    Close,
    /// Close was cancelled due to unsaved changes
    UnsavedChangesWarning,
    /// Live-preview a theme change (theme name)
    PreviewTheme(String),
}

impl SettingsView {
    pub fn handle_key(&mut self, key: KeyEvent) -> SettingsAction {
        // Clear transient messages on any key
        self.success_message = None;
        // Any keypress invalidates the mouse hover highlight; otherwise
        // a stationary cursor keeps highlighting an unrelated row while
        // the keyboard cursor moves elsewhere. Mirrors the sidebar's
        // move_cursor_clears_hover pattern.
        self.mouse_pos = None;

        // Handle custom instruction dialog
        if let Some(ref mut dialog) = self.custom_instruction_dialog {
            match dialog.handle_key(key) {
                DialogResult::Submit(value) => {
                    let field = &mut self.fields[self.selected_field];
                    if let FieldValue::OptionalText(ref mut v) = field.value {
                        *v = value;
                    }
                    self.apply_field_to_config(self.selected_field);
                    self.custom_instruction_dialog = None;
                    return SettingsAction::Continue;
                }
                DialogResult::Cancel => {
                    self.custom_instruction_dialog = None;
                    return SettingsAction::Continue;
                }
                DialogResult::Continue => {
                    return SettingsAction::Continue;
                }
            }
        }

        // Handle help overlay
        if self.show_help {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q')
            ) {
                self.show_help = false;
            }
            return SettingsAction::Continue;
        }

        // Handle text editing mode
        if self.editing_input.is_some() {
            return self.handle_text_edit_key(key);
        }

        // Handle list editing mode
        if self.list_edit_state.is_some() {
            return self.handle_list_edit_key(key);
        }

        // Handle settings-wide search overlay. While the overlay is
        // open every other dispatch (scope cycle, save, navigation in
        // the main panels) is suppressed: the user is typing into the
        // search input and picking a hit, not driving the underlying
        // settings view.
        if self.search_input.is_some() {
            return self.handle_search_key(key);
        }

        // Normal mode
        match (key.code, key.modifiers) {
            // Save
            (KeyCode::Char('s'), KeyModifiers::CONTROL) => {
                if let Err(e) = self.save() {
                    self.error_message = Some(format!("Failed to save: {}", e));
                }
                SettingsAction::Continue
            }

            // Close from anywhere
            (KeyCode::Char('q'), _) => {
                if self.has_changes {
                    SettingsAction::UnsavedChangesWarning
                } else {
                    SettingsAction::Close
                }
            }

            // Escape goes up one level
            (KeyCode::Esc, _) => match self.focus {
                SettingsFocus::Fields => {
                    self.focus = SettingsFocus::Categories;
                    SettingsAction::Continue
                }
                SettingsFocus::Categories => {
                    if self.has_changes {
                        SettingsAction::UnsavedChangesWarning
                    } else {
                        SettingsAction::Close
                    }
                }
            },

            // Switch scope: [ and ] cycle between Global / Profile / Repo
            (KeyCode::Char(']'), _) => {
                if self.has_changes {
                    return SettingsAction::UnsavedChangesWarning;
                }
                self.scope = match self.scope {
                    SettingsScope::Global => SettingsScope::Profile,
                    SettingsScope::Profile => {
                        if self.project_path.is_some() {
                            SettingsScope::Repo
                        } else {
                            SettingsScope::Global
                        }
                    }
                    SettingsScope::Repo => SettingsScope::Global,
                };
                self.rebuild_categories_for_scope();
                self.rebuild_fields();
                SettingsAction::Continue
            }
            (KeyCode::Char('['), _) => {
                if self.has_changes {
                    return SettingsAction::UnsavedChangesWarning;
                }
                self.scope = match self.scope {
                    SettingsScope::Global => {
                        if self.project_path.is_some() {
                            SettingsScope::Repo
                        } else {
                            SettingsScope::Profile
                        }
                    }
                    SettingsScope::Profile => SettingsScope::Global,
                    SettingsScope::Repo => SettingsScope::Profile,
                };
                self.rebuild_categories_for_scope();
                self.rebuild_fields();
                SettingsAction::Continue
            }

            // Cycle through profiles when in Profile scope: { and }
            (KeyCode::Char('}'), _) | (KeyCode::Char('{'), _) => {
                if self.scope == SettingsScope::Profile && !self.available_profiles.is_empty() {
                    if self.has_changes {
                        return SettingsAction::UnsavedChangesWarning;
                    }
                    let current_idx = self
                        .available_profiles
                        .iter()
                        .position(|p| p == &self.profile)
                        .unwrap_or(0);
                    let next_idx = if key.code == KeyCode::Char('}') {
                        (current_idx + 1) % self.available_profiles.len()
                    } else if current_idx == 0 {
                        self.available_profiles.len() - 1
                    } else {
                        current_idx - 1
                    };
                    let new_profile = self.available_profiles[next_idx].clone();
                    if let Err(e) = self.switch_profile(&new_profile) {
                        self.error_message = Some(format!("Failed to load profile: {}", e));
                    }
                }
                SettingsAction::Continue
            }

            // Switch focus between categories and fields
            (KeyCode::Tab, _) | (KeyCode::Right, _) | (KeyCode::Char('l'), _) => {
                self.focus = SettingsFocus::Fields;
                SettingsAction::Continue
            }
            (KeyCode::BackTab, _) | (KeyCode::Left, _) | (KeyCode::Char('h'), _) => {
                self.focus = SettingsFocus::Categories;
                SettingsAction::Continue
            }

            // Navigate up/down. Inside the field list, navigation skips
            // past non-interactive section dividers
            // (`FieldValue::SectionHeader`) so the cursor never lands on
            // a row the user can't edit.
            (KeyCode::Up, _) | (KeyCode::Char('k'), _) => {
                match self.focus {
                    SettingsFocus::Categories => {
                        // Skip non-selectable section dividers so the
                        // cursor jumps category-to-category.
                        let mut idx = self.selected_category;
                        while idx > 0 {
                            idx -= 1;
                            if matches!(self.categories[idx], super::CategoryRow::Tab(_)) {
                                self.selected_category = idx;
                                self.rebuild_fields();
                                self.snap_to_interactive_field_forward();
                                break;
                            }
                        }
                    }
                    SettingsFocus::Fields => {
                        let mut idx = self.selected_field;
                        while idx > 0 {
                            idx -= 1;
                            if !self.fields[idx].is_section_header() {
                                self.selected_field = idx;
                                self.ensure_field_visible(self.fields_viewport_height);
                                break;
                            }
                        }
                    }
                }
                SettingsAction::Continue
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), _) => {
                match self.focus {
                    SettingsFocus::Categories => {
                        let mut idx = self.selected_category + 1;
                        while idx < self.categories.len() {
                            if matches!(self.categories[idx], super::CategoryRow::Tab(_)) {
                                self.selected_category = idx;
                                self.rebuild_fields();
                                self.snap_to_interactive_field_forward();
                                break;
                            }
                            idx += 1;
                        }
                    }
                    SettingsFocus::Fields => {
                        let mut idx = self.selected_field + 1;
                        while idx < self.fields.len() {
                            if !self.fields[idx].is_section_header() {
                                self.selected_field = idx;
                                self.ensure_field_visible(self.fields_viewport_height);
                                break;
                            }
                            idx += 1;
                        }
                    }
                }
                SettingsAction::Continue
            }

            // Toggle boolean / edit field
            (KeyCode::Char(' '), _) => {
                if self.focus == SettingsFocus::Fields && !self.fields.is_empty() {
                    let field = &mut self.fields[self.selected_field];
                    if let FieldValue::Bool(ref mut value) = field.value {
                        *value = !*value;
                        self.apply_field_to_config(self.selected_field);
                    }
                }
                SettingsAction::Continue
            }

            // Enter - edit field or expand list
            (KeyCode::Enter, _) => {
                if self.focus == SettingsFocus::Fields && !self.fields.is_empty() {
                    let field = &self.fields[self.selected_field];
                    match &field.value {
                        FieldValue::Bool(value) => {
                            let new_value = !value;
                            self.fields[self.selected_field].value = FieldValue::Bool(new_value);
                            self.apply_field_to_config(self.selected_field);
                        }
                        FieldValue::Text(value) => {
                            self.editing_input = Some(Input::new(value.clone()));
                        }
                        FieldValue::OptionalText(value) => {
                            if field.key == FieldKey::CustomInstruction {
                                self.custom_instruction_dialog =
                                    Some(CustomInstructionDialog::new(value.clone()));
                            } else {
                                self.editing_input =
                                    Some(Input::new(value.clone().unwrap_or_default()));
                            }
                        }
                        FieldValue::Number(value) => {
                            self.editing_input = Some(Input::new(value.to_string()));
                        }
                        FieldValue::Select { selected, options } => {
                            let new_selected = (*selected + 1) % options.len();
                            let new_options = options.clone();
                            self.fields[self.selected_field].value = FieldValue::Select {
                                selected: new_selected,
                                options: new_options,
                            };
                            self.apply_field_to_config(self.selected_field);

                            if self.fields[self.selected_field].key == FieldKey::ThemeName {
                                if let FieldValue::Select { selected, options } =
                                    &self.fields[self.selected_field].value
                                {
                                    if let Some(name) = options.get(*selected) {
                                        return SettingsAction::PreviewTheme(name.clone());
                                    }
                                }
                            }
                        }
                        FieldValue::List(_) => {
                            // Expand list for editing
                            self.list_edit_state = Some(ListEditState::default());
                        }
                        FieldValue::SectionHeader => {
                            // Non-interactive divider. Navigation should
                            // never land the cursor here in the first
                            // place; this arm just makes the match
                            // exhaustive.
                        }
                    }
                } else if self.focus == SettingsFocus::Categories {
                    // Move to fields when pressing Enter on a category
                    self.focus = SettingsFocus::Fields;
                }
                SettingsAction::Continue
            }

            // Toggle help overlay
            (KeyCode::Char('?'), _) => {
                self.show_help = true;
                SettingsAction::Continue
            }

            // Open the settings-wide search overlay. Any field with a
            // matching label or description (across every category) is
            // a hit; Enter jumps to that field.
            (KeyCode::Char('/'), _) => {
                self.open_search();
                SettingsAction::Continue
            }

            // Reset field to default (clear profile/repo override)
            (KeyCode::Char('r'), _) => {
                if (self.scope == SettingsScope::Profile || self.scope == SettingsScope::Repo)
                    && self.focus == SettingsFocus::Fields
                    && !self.fields.is_empty()
                {
                    let was_theme = self.fields[self.selected_field].key == FieldKey::ThemeName;
                    // Clearing an override doesn't change which fields exist, only
                    // their inherited values. rebuild_fields() resets scroll to 0,
                    // which would yank the user away from the field they just reset.
                    // Preserve the cursor and scroll position.
                    let saved_selected = self.selected_field;
                    let saved_scroll = self.fields_scroll_offset;
                    self.clear_profile_override(self.selected_field);
                    self.rebuild_fields();
                    if saved_selected < self.fields.len() {
                        self.selected_field = saved_selected;
                    }
                    self.fields_scroll_offset = saved_scroll;

                    if was_theme {
                        if let Some(field) =
                            self.fields.iter().find(|f| f.key == FieldKey::ThemeName)
                        {
                            if let FieldValue::Select { selected, options } = &field.value {
                                if let Some(name) = options.get(*selected) {
                                    return SettingsAction::PreviewTheme(name.clone());
                                }
                            }
                        }
                    }
                }
                SettingsAction::Continue
            }

            _ => SettingsAction::Continue,
        }
    }

    /// Drive the settings-wide search overlay. Esc closes without
    /// changing selection; Enter jumps to the highlighted hit; up/down
    /// navigates the hit list; every other key feeds `search_input`
    /// and re-runs the filter so the list narrows as the user types.
    fn handle_search_key(&mut self, key: KeyEvent) -> SettingsAction {
        match key.code {
            KeyCode::Esc => {
                self.close_search();
            }
            KeyCode::Enter => {
                self.jump_to_selected_search_hit();
            }
            KeyCode::Up => {
                if self.search_selected > 0 {
                    self.search_selected -= 1;
                }
            }
            KeyCode::Down => {
                if self.search_selected + 1 < self.search_hits.len() {
                    self.search_selected += 1;
                }
            }
            _ => {
                // Edit the search query and refresh hits.
                if let Some(ref mut input) = self.search_input {
                    input.handle_event(&crossterm::event::Event::Key(key));
                }
                self.search_selected = 0;
                self.recompute_search_hits();
            }
        }
        SettingsAction::Continue
    }

    fn handle_text_edit_key(&mut self, key: KeyEvent) -> SettingsAction {
        match key.code {
            KeyCode::Esc => {
                self.editing_input = None;
                self.error_message = None;
            }
            KeyCode::Enter => {
                if let Some(input) = self.editing_input.take() {
                    let text = input.value().to_string();
                    let field = &mut self.fields[self.selected_field];

                    // Apply the new value
                    match &mut field.value {
                        FieldValue::Text(ref mut v) => {
                            *v = text;
                        }
                        FieldValue::OptionalText(ref mut v) => {
                            *v = if text.is_empty() { None } else { Some(text) };
                        }
                        FieldValue::Number(ref mut v) => {
                            if let Ok(n) = text.parse() {
                                *v = n;
                            } else {
                                self.error_message = Some("Invalid number".to_string());
                                self.editing_input = Some(Input::new(text));
                                return SettingsAction::Continue;
                            }
                        }
                        _ => {}
                    }

                    // Validate
                    if let Err(e) = field.validate() {
                        self.error_message = Some(e);
                        // Revert to editing
                        self.editing_input = match &field.value {
                            FieldValue::Text(v) => Some(Input::new(v.clone())),
                            FieldValue::OptionalText(v) => {
                                Some(Input::new(v.clone().unwrap_or_default()))
                            }
                            FieldValue::Number(v) => Some(Input::new(v.to_string())),
                            _ => None,
                        };
                        return SettingsAction::Continue;
                    }

                    self.apply_field_to_config(self.selected_field);
                    self.error_message = None;
                }
            }
            _ => {
                // Delegate all other key events to tui_input
                if let Some(ref mut input) = self.editing_input {
                    input.handle_event(&crossterm::event::Event::Key(key));
                }
            }
        }
        SettingsAction::Continue
    }

    fn handle_list_edit_key(&mut self, key: KeyEvent) -> SettingsAction {
        let state = match self.list_edit_state.as_mut() {
            Some(s) => s,
            None => return SettingsAction::Continue,
        };

        // If we're editing an item or adding new
        if state.editing_item.is_some() {
            return self.handle_list_item_edit_key(key);
        }

        match key.code {
            KeyCode::Esc => {
                self.list_edit_state = None;
            }
            KeyCode::Up | KeyCode::Char('k') if state.selected_index > 0 => {
                state.selected_index -= 1;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let FieldValue::List(items) = &self.fields[self.selected_field].value {
                    if state.selected_index < items.len().saturating_sub(1) {
                        state.selected_index += 1;
                    }
                }
            }
            KeyCode::Char('a') => {
                // Add new item
                state.adding_new = true;
                state.editing_item = Some(Input::default());
            }
            KeyCode::Char('d') => {
                // Delete selected item - capture index before borrowing fields
                let selected_idx = state.selected_index;
                let mut new_selected_idx = selected_idx;

                if let FieldValue::List(ref mut items) = self.fields[self.selected_field].value {
                    if !items.is_empty() && selected_idx < items.len() {
                        items.remove(selected_idx);
                        if selected_idx >= items.len() && !items.is_empty() {
                            new_selected_idx = items.len() - 1;
                        }
                    }
                }

                if let Some(ref mut s) = self.list_edit_state {
                    s.selected_index = new_selected_idx;
                }
                self.apply_field_to_config(self.selected_field);
            }
            KeyCode::Enter => {
                // Edit selected item
                if let FieldValue::List(items) = &self.fields[self.selected_field].value {
                    if !items.is_empty() && state.selected_index < items.len() {
                        state.editing_item = Some(Input::new(items[state.selected_index].clone()));
                    }
                }
            }
            _ => {}
        }
        SettingsAction::Continue
    }

    fn handle_list_item_edit_key(&mut self, key: KeyEvent) -> SettingsAction {
        let state = match self.list_edit_state.as_mut() {
            Some(s) => s,
            None => return SettingsAction::Continue,
        };

        match key.code {
            KeyCode::Esc => {
                state.editing_item = None;
                state.adding_new = false;
                self.error_message = None;
            }
            KeyCode::Enter => {
                // Take the input and flags out to avoid borrow conflict
                let input = state.editing_item.take();
                let adding_new = state.adding_new;
                let selected_idx = state.selected_index;
                state.adding_new = false;

                if let Some(input) = input {
                    let text = input.value().to_string();
                    if !text.is_empty() {
                        let field_key = self.fields[self.selected_field].key;

                        // Validate key=value format for agent override fields
                        let validation_result = match field_key {
                            FieldKey::AgentExtraArgs | FieldKey::AgentCommandOverride => {
                                Some(validate_agent_key_value(&text))
                            }
                            FieldKey::CustomAgents => Some(validate_custom_agent_entry(&text)),
                            FieldKey::AgentDetectAs => Some(validate_detect_as_entry(&text)),
                            FieldKey::AgentCockpitCmd => Some(validate_cockpit_cmd_entry(&text)),
                            _ => None,
                        };
                        if let Some(Err(msg)) = validation_result {
                            self.error_message = Some(msg);
                            // Re-open the editor so the user can fix the entry
                            if let Some(ref mut s) = self.list_edit_state {
                                s.editing_item = Some(tui_input::Input::new(text));
                                s.adding_new = adding_new;
                            }
                            return SettingsAction::Continue;
                        }

                        // Validate env var references before accepting
                        if field_key == FieldKey::Environment {
                            self.error_message = crate::session::validate_env_entry(&text);
                        }

                        if let FieldValue::List(ref mut items) =
                            self.fields[self.selected_field].value
                        {
                            if adding_new {
                                items.push(text);
                                if let Some(ref mut s) = self.list_edit_state {
                                    s.selected_index = items.len() - 1;
                                }
                            } else if selected_idx < items.len() {
                                items[selected_idx] = text;
                            }
                        }
                        self.apply_field_to_config(self.selected_field);
                        // Clear stale errors, but preserve env validation warnings set above
                        if field_key != FieldKey::Environment {
                            self.error_message = None;
                        }
                    }
                }
            }
            _ => {
                // Delegate all other key events to tui_input
                if let Some(ref mut input) = state.editing_item {
                    input.handle_event(&crossterm::event::Event::Key(key));
                }
            }
        }
        SettingsAction::Continue
    }

    fn clear_profile_override(&mut self, field_index: usize) {
        if field_index >= self.fields.len() {
            return;
        }

        let key = self.fields[field_index].key;

        // Pick the right ProfileConfig to clear from based on scope
        let config = if self.scope == SettingsScope::Repo {
            &mut self.repo_as_profile
        } else {
            &mut self.profile_config
        };

        match key {
            // Profile description: clears straight to None since there is no
            // global counterpart to inherit from.
            FieldKey::ProfileDescription => {
                config.description = None;
            }
            // Theme
            FieldKey::ThemeName => {
                if let Some(ref mut t) = config.theme {
                    t.name = None;
                }
            }
            FieldKey::ThemeColorMode => {
                if let Some(ref mut t) = config.theme {
                    t.color_mode = None;
                }
            }
            FieldKey::IdleDecayMinutes => {
                if let Some(ref mut t) = config.theme {
                    t.idle_decay_minutes = None;
                }
            }
            // Updates
            FieldKey::UpdateCheckMode => {
                if let Some(ref mut u) = config.updates {
                    u.update_check_mode = None;
                }
            }
            FieldKey::CheckIntervalHours => {
                if let Some(ref mut u) = config.updates {
                    u.check_interval_hours = None;
                }
            }
            FieldKey::NotifyInCli => {
                if let Some(ref mut u) = config.updates {
                    u.notify_in_cli = None;
                }
            }
            FieldKey::WebPollIntervalMinutes => {
                if let Some(ref mut u) = config.updates {
                    u.web_poll_interval_minutes = None;
                }
            }
            // Worktree
            FieldKey::WorktreeEnabled => {
                if let Some(ref mut w) = config.worktree {
                    w.enabled = None;
                }
            }
            FieldKey::PathTemplate => {
                if let Some(ref mut w) = config.worktree {
                    w.path_template = None;
                }
            }
            FieldKey::BareRepoPathTemplate => {
                if let Some(ref mut w) = config.worktree {
                    w.bare_repo_path_template = None;
                }
            }
            FieldKey::WorktreeAutoCleanup => {
                if let Some(ref mut w) = config.worktree {
                    w.auto_cleanup = None;
                }
            }
            FieldKey::DeleteBranchOnCleanup => {
                if let Some(ref mut w) = config.worktree {
                    w.delete_branch_on_cleanup = None;
                }
            }
            FieldKey::WorkspacePathTemplate => {
                if let Some(ref mut w) = config.worktree {
                    w.workspace_path_template = None;
                }
            }
            FieldKey::InitSubmodules => {
                if let Some(ref mut w) = config.worktree {
                    w.init_submodules = None;
                }
            }
            // Diff
            FieldKey::DiffSplitView => {
                if let Some(d) = config.diff.as_mut() {
                    d.split_view = None;
                }
            }
            // Sandbox
            FieldKey::DefaultImage => {
                if let Some(ref mut s) = config.sandbox {
                    s.default_image = None;
                }
            }
            FieldKey::Environment => {
                if let Some(ref mut s) = config.sandbox {
                    s.environment = None;
                }
            }
            FieldKey::SandboxAutoCleanup => {
                if let Some(ref mut s) = config.sandbox {
                    s.auto_cleanup = None;
                }
            }
            // Tmux
            FieldKey::StatusBar => {
                if let Some(ref mut t) = config.tmux {
                    t.status_bar = None;
                }
            }
            FieldKey::Mouse => {
                if let Some(ref mut t) = config.tmux {
                    t.mouse = None;
                }
            }
            FieldKey::Clipboard => {
                if let Some(ref mut t) = config.tmux {
                    t.clipboard = None;
                }
            }
            // Session
            FieldKey::DefaultTool => {
                if let Some(ref mut s) = config.session {
                    s.default_tool = None;
                }
            }
            FieldKey::SandboxEnabledByDefault => {
                if let Some(ref mut s) = config.sandbox {
                    s.enabled_by_default = None;
                }
            }
            FieldKey::YoloModeDefault => {
                if let Some(ref mut s) = config.session {
                    s.yolo_mode_default = None;
                }
            }
            FieldKey::StrictHotkeys => {
                if let Some(ref mut s) = config.session {
                    s.strict_hotkeys = None;
                }
            }
            FieldKey::SnoozeDurationMinutes => {
                if let Some(ref mut s) = config.session {
                    s.snooze_duration_minutes = None;
                }
            }
            FieldKey::SessionAutoStopIdleSecs => {
                if let Some(ref mut s) = config.session {
                    s.auto_stop_idle_secs = None;
                }
            }
            FieldKey::RestartWakeMessage => {
                if let Some(ref mut s) = config.session {
                    s.restart_wake_message = None;
                }
            }
            FieldKey::LiveSendExitChord => {
                if let Some(ref mut s) = config.session {
                    s.live_send_exit_chord = None;
                }
            }
            FieldKey::LiveSendLeader => {
                if let Some(ref mut s) = config.session {
                    s.live_send_leader = None;
                }
            }
            FieldKey::NewSessionAttachMode => {
                if let Some(ref mut s) = config.session {
                    s.new_session_attach_mode = None;
                }
            }
            FieldKey::DefaultAttachMode => {
                if let Some(ref mut s) = config.session {
                    s.default_attach_mode = None;
                }
            }
            FieldKey::ClickAction => {
                if let Some(ref mut s) = config.session {
                    s.click_action = None;
                }
            }
            FieldKey::RowTag => {
                if let Some(ref mut s) = config.session {
                    s.row_tag = None;
                }
            }
            FieldKey::AgentExtraArgs => {
                if let Some(ref mut s) = config.session {
                    s.agent_extra_args = None;
                }
            }
            FieldKey::AgentCommandOverride => {
                if let Some(ref mut s) = config.session {
                    s.agent_command_override = None;
                }
            }
            FieldKey::CustomAgents => {
                if let Some(ref mut s) = config.session {
                    s.custom_agents = None;
                }
            }
            FieldKey::AgentDetectAs => {
                if let Some(ref mut s) = config.session {
                    s.agent_detect_as = None;
                }
            }
            FieldKey::AgentCockpitCmd => {
                if let Some(ref mut s) = config.session {
                    s.agent_cockpit_cmd = None;
                }
            }
            FieldKey::AgentStatusHooks => {
                if let Some(ref mut s) = config.session {
                    s.agent_status_hooks = None;
                }
            }
            FieldKey::MouseCapture => {
                if let Some(ref mut s) = config.session {
                    s.mouse_capture = None;
                }
            }
            FieldKey::DefaultTerminalMode => {
                if let Some(ref mut s) = config.sandbox {
                    s.default_terminal_mode = None;
                }
            }
            FieldKey::ExtraVolumes => {
                if let Some(ref mut s) = config.sandbox {
                    s.extra_volumes = None;
                }
            }
            FieldKey::PortMappings => {
                if let Some(ref mut s) = config.sandbox {
                    s.port_mappings = None;
                }
            }
            FieldKey::VolumeIgnores => {
                if let Some(ref mut s) = config.sandbox {
                    s.volume_ignores = None;
                }
            }
            FieldKey::MountSsh => {
                if let Some(ref mut s) = config.sandbox {
                    s.mount_ssh = None;
                }
            }
            FieldKey::SelinuxRelabel => {
                if let Some(ref mut s) = config.sandbox {
                    s.selinux_relabel = None;
                }
            }
            FieldKey::CpuLimit => {
                if let Some(ref mut s) = config.sandbox {
                    s.cpu_limit = None;
                }
            }
            FieldKey::MemoryLimit => {
                if let Some(ref mut s) = config.sandbox {
                    s.memory_limit = None;
                }
            }
            FieldKey::CustomInstruction => {
                if let Some(ref mut s) = config.sandbox {
                    s.custom_instruction = None;
                }
            }
            FieldKey::ContainerRuntime => {
                if let Some(ref mut s) = config.sandbox {
                    s.container_runtime = None;
                }
            }
            FieldKey::VolumeIgnoresStrategy => {
                if let Some(ref mut s) = config.sandbox {
                    s.volume_ignores_strategy = None;
                }
            }
            // Sound
            FieldKey::SoundEnabled => {
                if let Some(ref mut s) = config.sound {
                    s.enabled = None;
                }
            }
            FieldKey::SoundMode => {
                if let Some(ref mut s) = config.sound {
                    s.mode = None;
                }
            }
            FieldKey::SoundVolume => {
                if let Some(ref mut s) = config.sound {
                    s.volume = None;
                }
            }
            FieldKey::SoundOnStart => {
                if let Some(ref mut s) = config.sound {
                    s.on_start = None;
                }
            }
            FieldKey::SoundOnRunning => {
                if let Some(ref mut s) = config.sound {
                    s.on_running = None;
                }
            }
            FieldKey::SoundOnWaiting => {
                if let Some(ref mut s) = config.sound {
                    s.on_waiting = None;
                }
            }
            FieldKey::SoundOnIdle => {
                if let Some(ref mut s) = config.sound {
                    s.on_idle = None;
                }
            }
            FieldKey::SoundOnError => {
                if let Some(ref mut s) = config.sound {
                    s.on_error = None;
                }
            }
            FieldKey::SoundOnApproval => {
                if let Some(ref mut s) = config.sound {
                    s.on_approval = None;
                }
            }
            // Status hooks
            FieldKey::StatusHooksEnabled => {
                if let Some(ref mut s) = config.status_hooks {
                    s.enabled = None;
                }
            }
            FieldKey::StatusHookDebounceMs => {
                if let Some(ref mut s) = config.status_hooks {
                    s.debounce_ms = None;
                }
            }
            FieldKey::StatusHookOnStarting => {
                if let Some(ref mut s) = config.status_hooks {
                    s.on_starting = None;
                }
            }
            FieldKey::StatusHookOnRunning => {
                if let Some(ref mut s) = config.status_hooks {
                    s.on_running = None;
                }
            }
            FieldKey::StatusHookOnWaiting => {
                if let Some(ref mut s) = config.status_hooks {
                    s.on_waiting = None;
                }
            }
            FieldKey::StatusHookOnIdle => {
                if let Some(ref mut s) = config.status_hooks {
                    s.on_idle = None;
                }
            }
            FieldKey::StatusHookOnError => {
                if let Some(ref mut s) = config.status_hooks {
                    s.on_error = None;
                }
            }
            FieldKey::StatusHookOnChange => {
                if let Some(ref mut s) = config.status_hooks {
                    s.on_change = None;
                }
            }
            // Hooks
            FieldKey::HookOnCreate => {
                if let Some(ref mut h) = config.hooks {
                    h.on_create = None;
                }
            }
            FieldKey::HookOnLaunch => {
                if let Some(ref mut h) = config.hooks {
                    h.on_launch = None;
                }
            }
            FieldKey::HookOnDestroy => {
                if let Some(ref mut h) = config.hooks {
                    h.on_destroy = None;
                }
            }
            // Web settings are server-global; no per-profile override to clear.
            FieldKey::WebNotificationsEnabled
            | FieldKey::WebNotifyOnWaiting
            | FieldKey::WebNotifyOnIdle
            | FieldKey::WebNotifyOnError
            | FieldKey::WebNotifyOnWakeFire => {}
            // Cockpit overrides clear by setting the override field to None.
            FieldKey::CockpitEnabled => {
                if let Some(c) = config.cockpit.as_mut() {
                    c.enabled = None;
                }
            }
            FieldKey::CockpitDefaultForClaude => {
                if let Some(c) = config.cockpit.as_mut() {
                    c.default_for_claude = None;
                }
            }
            FieldKey::CockpitDefaultAgent => {
                if let Some(c) = config.cockpit.as_mut() {
                    c.default_agent = None;
                }
            }
            FieldKey::CockpitMaxConcurrentWorkers => {
                if let Some(c) = config.cockpit.as_mut() {
                    c.max_concurrent_workers = None;
                }
            }
            FieldKey::CockpitReplayEvents => {
                if let Some(c) = config.cockpit.as_mut() {
                    c.replay_events = None;
                }
            }
            FieldKey::CockpitReplayBytes => {
                if let Some(c) = config.cockpit.as_mut() {
                    c.replay_bytes = None;
                }
            }
            FieldKey::CockpitNodePath => {
                if let Some(c) = config.cockpit.as_mut() {
                    c.node_path = None;
                }
            }
            FieldKey::CockpitShowToolDurations => {
                if let Some(c) = config.cockpit.as_mut() {
                    c.show_tool_durations = None;
                }
            }
            FieldKey::CockpitQueueDrainMode => {
                if let Some(c) = config.cockpit.as_mut() {
                    c.queue_drain_mode = None;
                }
            }
            FieldKey::CockpitMaxConcurrentResumes => {
                if let Some(c) = config.cockpit.as_mut() {
                    c.max_concurrent_resumes = None;
                }
            }
            FieldKey::CockpitForceEndTurnThresholdSecs => {
                if let Some(c) = config.cockpit.as_mut() {
                    c.force_end_turn_threshold_secs = None;
                }
            }
            FieldKey::CockpitSilentOrphanGraceSecs => {
                if let Some(c) = config.cockpit.as_mut() {
                    c.silent_orphan_grace_secs = None;
                }
            }
            FieldKey::CockpitSilentOrphanFastGraceSecs => {
                if let Some(c) = config.cockpit.as_mut() {
                    c.silent_orphan_fast_grace_secs = None;
                }
            }
            FieldKey::CockpitAutoStopIdleSecs => {
                if let Some(c) = config.cockpit.as_mut() {
                    c.auto_stop_idle_secs = None;
                }
            }
            // Logging is global-only for v1 (no profile overrides); the
            // "clear override" gesture is a no-op for these keys.
            // SessionIdPollerMaxThreads and ConfirmBeforeQuit are also
            // global-only.
            FieldKey::ConfirmBeforeQuit
            | FieldKey::LoggingDefaultLevel
            | FieldKey::LoggingTarget(_)
            | FieldKey::LoggingOutput
            | FieldKey::LoggingFilePath
            | FieldKey::LoggingRotation
            | FieldKey::LoggingMaxSizeMib
            | FieldKey::LoggingKeepCount
            | FieldKey::LoggingShowSpans
            | FieldKey::TelemetryEnabled
            | FieldKey::SessionIdPollerMaxThreads => {}
            FieldKey::HostEnvironment => {
                config.environment = None;
            }
            // Section markers are non-interactive dividers; navigation
            // and the reset gesture never land here, but the match
            // arm must exist for exhaustiveness.
            FieldKey::SectionMarker => {}
        }

        // Sync repo_config when in Repo scope
        if self.scope == SettingsScope::Repo {
            self.repo_config = Some(crate::session::profile_to_repo_config(
                &self.repo_as_profile,
            ));
        }

        self.has_changes = true;
    }

    /// Force close without saving
    pub fn force_close(&mut self) {
        self.has_changes = false;
    }

    /// Discard changes and reload
    pub fn discard_changes(&mut self) -> anyhow::Result<()> {
        self.global_config = crate::session::Config::load()?;
        self.profile_config = crate::session::load_profile_config(&self.profile)?;
        self.repo_config = self.project_path.as_ref().and_then(|p| {
            crate::session::load_repo_config(std::path::Path::new(p))
                .ok()
                .flatten()
        });
        self.resolved_base =
            crate::session::merge_configs(self.global_config.clone(), &self.profile_config);
        self.repo_as_profile = self
            .repo_config
            .as_ref()
            .map(crate::session::repo_config_to_profile)
            .unwrap_or_default();
        self.has_changes = false;
        self.rebuild_fields();
        Ok(())
    }

    pub fn handle_paste(&mut self, text: &str) {
        if let Some(ref mut dialog) = self.custom_instruction_dialog {
            dialog.handle_paste(text);
            return;
        }
        // The search overlay is a full editing mode (gated on
        // `search_input.is_some()` in `handle_key`), so bracketed
        // pastes need a path into it. Without this, terminals that
        // emit `Paste` events for clipboard input would silently
        // drop pasted search queries.
        if let Some(ref mut input) = self.search_input {
            let sanitized: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
            for ch in sanitized.chars() {
                input.handle(tui_input::InputRequest::InsertChar(ch));
            }
            self.search_selected = 0;
            self.recompute_search_hits();
            return;
        }
        if let Some(ref mut input) = self.editing_input {
            let sanitized: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
            for ch in sanitized.chars() {
                input.handle(tui_input::InputRequest::InsertChar(ch));
            }
        }
    }

    /// Route a left-click into the settings view. Returns
    /// `Some(SettingsAction)` when the click was consumed (the
    /// settings view stays open, only the focus/scope/selection
    /// changes; the caller still needs to redraw). Returns `None`
    /// when nothing hit and the click should be treated as a swallow
    /// (since settings is a full-screen takeover, clicks anywhere
    /// inside it are absorbed by the modal regardless).
    ///
    /// Editing modes (`editing_input`, `list_edit_state`, custom
    /// instruction dialog, help overlay, search overlay) intentionally
    /// skip click routing so a stray click during composition doesn't
    /// reset focus or drop a half-typed value. The keyboard's Esc /
    /// Enter handlers remain the way out of those modes.
    pub fn handle_click(&mut self, col: u16, row: u16) -> Option<SettingsAction> {
        if self.editing_input.is_some()
            || self.list_edit_state.is_some()
            || self.custom_instruction_dialog.is_some()
            || self.show_help
            || self.search_input.is_some()
        {
            return None;
        }
        let pos = ratatui::layout::Position::from((col, row));

        if let Some((scope, _)) = self
            .scope_tab_rects
            .iter()
            .find(|(_, rect)| rect.contains(pos))
            .copied()
        {
            if scope != self.scope {
                if self.has_changes {
                    return Some(SettingsAction::UnsavedChangesWarning);
                }
                self.scope = scope;
                self.rebuild_categories_for_scope();
                self.rebuild_fields();
            }
            return Some(SettingsAction::Continue);
        }

        if let Some((idx, _)) = self
            .category_rects
            .iter()
            .find(|(_, rect)| rect.contains(pos))
            .copied()
        {
            self.focus = SettingsFocus::Categories;
            if self.selected_category != idx {
                self.selected_category = idx;
                self.selected_field = 0;
                self.fields_scroll_offset = 0;
                self.rebuild_fields();
            }
            return Some(SettingsAction::Continue);
        }

        if let Some((idx, _)) = self
            .field_rects
            .iter()
            .find(|(_, rect)| rect.contains(pos))
            .copied()
        {
            self.focus = SettingsFocus::Fields;
            self.selected_field = idx;
            return Some(SettingsAction::Continue);
        }

        None
    }

    /// Track the mouse position so the renderer can paint a hover
    /// highlight on whichever scope chip / category row / field row
    /// the cursor is over. Hover never moves the keyboard cursor;
    /// see `ConfirmDialog::handle_hover` for why. Editing / search /
    /// help modes clear the hover so the highlight doesn't bleed
    /// behind the overlay.
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        let suppress = self.editing_input.is_some()
            || self.list_edit_state.is_some()
            || self.custom_instruction_dialog.is_some()
            || self.show_help
            || self.search_input.is_some();
        let new_pos = if suppress { None } else { Some((col, row)) };
        if self.mouse_pos == new_pos {
            return false;
        }
        // Only request a redraw when the resolved hover target
        // actually changes; a mouse drift inside the same field or
        // entirely off the rects shouldn't repaint every pixel.
        let prev_scope = self.hovered_scope();
        let prev_cat = self.hovered_category();
        let prev_field = self.hovered_field();
        self.mouse_pos = new_pos;
        prev_scope != self.hovered_scope()
            || prev_cat != self.hovered_category()
            || prev_field != self.hovered_field()
    }
}

/// Validate that an entry for AgentExtraArgs or AgentCommandOverride is in `agent_name=value` format.
fn validate_agent_key_value(text: &str) -> Result<(), String> {
    let Some((key, value)) = text.split_once('=') else {
        let names = crate::agents::agent_names().join(", ");
        return Err(format!(
            "Must be in agent_name=value format (e.g. claude=my-command). Known agents: {}",
            names
        ));
    };

    if key.is_empty() {
        return Err("Agent name cannot be empty".to_string());
    }

    if value.is_empty() {
        return Err("Value cannot be empty".to_string());
    }

    if crate::agents::get_agent(key).is_none() {
        let names = crate::agents::agent_names().join(", ");
        return Err(format!(
            "'{}' is not a known agent. Known agents: {}",
            key, names
        ));
    }

    Ok(())
}

/// Validate a custom agent entry: name=command. Name must not collide with built-in agents.
fn validate_custom_agent_entry(text: &str) -> Result<(), String> {
    let Some((key, value)) = text.split_once('=') else {
        return Err(
            "Must be in name=command format (e.g. lenovo-claude=ssh -t lenovo claude)".to_string(),
        );
    };
    if key.is_empty() {
        return Err("Agent name cannot be empty".to_string());
    }
    if value.is_empty() {
        return Err("Command cannot be empty".to_string());
    }
    if crate::agents::get_agent(key).is_some() {
        return Err(format!(
            "'{}' is a built-in agent. Use Agent Command Override to override built-in agents.",
            key
        ));
    }
    Ok(())
}

/// Validate an agent_cockpit_cmd entry: name=command. The command is the
/// ACP launch line, split with shell-word rules into argv, so it must be
/// non-empty and have balanced quoting.
fn validate_cockpit_cmd_entry(text: &str) -> Result<(), String> {
    let Some((key, value)) = text.split_once('=') else {
        return Err(
            "Must be in name=command format (e.g. oc-superpowers=ocp run sp acp)".to_string(),
        );
    };
    if key.is_empty() {
        return Err("Agent name cannot be empty".to_string());
    }
    if crate::agents::get_agent(key).is_some() {
        return Err(format!(
            "'{}' is a built-in agent, which already has a cockpit adapter.",
            key
        ));
    }
    match shell_words::split(value) {
        Ok(argv) if argv.is_empty() => Err("Command cannot be empty".to_string()),
        Ok(_) => Ok(()),
        Err(e) => Err(format!("Malformed command: {e}")),
    }
}

/// Validate a detect_as entry: name=builtin_agent. Value must be a known built-in agent.
fn validate_detect_as_entry(text: &str) -> Result<(), String> {
    let Some((key, value)) = text.split_once('=') else {
        return Err("Must be in name=builtin format (e.g. lenovo-claude=claude)".to_string());
    };
    if key.is_empty() {
        return Err("Agent name cannot be empty".to_string());
    }
    if value.is_empty() {
        return Err("Built-in agent name cannot be empty".to_string());
    }
    if crate::agents::get_agent(value).is_none() {
        let names = crate::agents::agent_names().join(", ");
        return Err(format!(
            "'{}' is not a known built-in agent. Known agents: {}",
            value, names
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_agent_key_value_valid() {
        assert!(validate_agent_key_value("claude=my-wrapper").is_ok());
        assert!(validate_agent_key_value("opencode=--port 8080").is_ok());
    }

    #[test]
    fn test_validate_agent_key_value_missing_equals() {
        let err = validate_agent_key_value("just-a-command").unwrap_err();
        assert!(err.contains("agent_name=value"));
    }

    #[test]
    fn test_validate_agent_key_value_empty_key() {
        let err = validate_agent_key_value("=some-value").unwrap_err();
        assert!(err.contains("cannot be empty"));
    }

    #[test]
    fn test_validate_agent_key_value_empty_value() {
        let err = validate_agent_key_value("claude=").unwrap_err();
        assert!(err.contains("cannot be empty"));
    }

    #[test]
    fn test_validate_agent_key_value_unknown_agent() {
        let err = validate_agent_key_value("nonexistent=cmd").unwrap_err();
        assert!(err.contains("not a known agent"));
    }

    // Tests for validate_custom_agent_entry
    #[test]
    fn test_validate_custom_agent_entry_valid() {
        assert!(validate_custom_agent_entry("lenovo-claude=ssh -t lenovo claude").is_ok());
        assert!(validate_custom_agent_entry("my-wrapper=./run.sh").is_ok());
    }

    #[test]
    fn test_validate_custom_agent_entry_missing_equals() {
        let err = validate_custom_agent_entry("just-a-name").unwrap_err();
        assert!(err.contains("name=command"));
    }

    #[test]
    fn test_validate_custom_agent_entry_empty_name() {
        let err = validate_custom_agent_entry("=ssh -t host claude").unwrap_err();
        assert!(err.contains("name cannot be empty"));
    }

    #[test]
    fn test_validate_custom_agent_entry_empty_command() {
        let err = validate_custom_agent_entry("my-agent=").unwrap_err();
        assert!(err.contains("Command cannot be empty"));
    }

    #[test]
    fn test_validate_custom_agent_entry_rejects_builtin() {
        let err = validate_custom_agent_entry("claude=my-wrapper").unwrap_err();
        assert!(err.contains("built-in agent"));
        assert!(err.contains("Agent Command Override"));
    }

    // Tests for validate_detect_as_entry
    #[test]
    fn test_validate_detect_as_entry_valid() {
        assert!(validate_detect_as_entry("lenovo-claude=claude").is_ok());
    }

    #[test]
    fn test_validate_detect_as_entry_missing_equals() {
        let err = validate_detect_as_entry("just-a-name").unwrap_err();
        assert!(err.contains("name=builtin"));
    }

    #[test]
    fn test_validate_detect_as_entry_empty_name() {
        let err = validate_detect_as_entry("=claude").unwrap_err();
        assert!(err.contains("name cannot be empty"));
    }

    #[test]
    fn test_validate_detect_as_entry_empty_value() {
        let err = validate_detect_as_entry("my-agent=").unwrap_err();
        assert!(err.contains("cannot be empty"));
    }

    #[test]
    fn test_validate_detect_as_entry_unknown_builtin() {
        let err = validate_detect_as_entry("my-agent=nonexistent").unwrap_err();
        assert!(err.contains("not a known built-in agent"));
        assert!(err.contains("Known agents:"));
    }

    mod search_overlay {
        use super::*;
        use crate::session::Storage;
        use crate::tui::settings::SettingsView;
        use serial_test::serial;
        use tempfile::TempDir;

        fn setup_test_home(temp: &TempDir) {
            std::env::set_var("HOME", temp.path());
            #[cfg(target_os = "linux")]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        }

        fn fresh_view() -> (TempDir, SettingsView) {
            let temp = TempDir::new().unwrap();
            setup_test_home(&temp);
            let _ = Storage::new("test").unwrap();
            let view = SettingsView::new("test", None).unwrap();
            (temp, view)
        }

        fn press(view: &mut SettingsView, code: KeyCode) {
            let _ = view.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
        }

        fn type_text(view: &mut SettingsView, text: &str) {
            for c in text.chars() {
                press(view, KeyCode::Char(c));
            }
        }

        /// `/` opens the search overlay; the overlay is then routed to
        /// for every subsequent key (gated by `search_input.is_some()`).
        #[test]
        #[serial]
        fn slash_opens_search_overlay() {
            let (_t, mut view) = fresh_view();
            assert!(view.search_input.is_none());
            press(&mut view, KeyCode::Char('/'));
            assert!(view.search_input.is_some(), "/ must enter search mode");
            // Empty query lists every interactive field across all
            // visible categories, so the hit list is nonzero and
            // Enter has a target.
            assert!(
                !view.search_hits.is_empty(),
                "empty-query search should list every interactive field"
            );
        }

        /// Typing a query narrows the hit list to matching fields.
        /// "live" should match the Live-Send Exit Chord row, which now
        /// lives under the Interaction tab.
        #[test]
        #[serial]
        fn typing_filters_hits_and_finds_moved_field() {
            let (_t, mut view) = fresh_view();
            press(&mut view, KeyCode::Char('/'));
            type_text(&mut view, "live");
            let labels: Vec<&'static str> =
                view.search_hits.iter().map(|h| h.field_label).collect();
            assert!(
                labels
                    .iter()
                    .any(|l| l.to_lowercase().contains("live-send exit chord")),
                "search 'live' should surface the Live-Send Exit Chord field; got {:?}",
                labels
            );
        }

        /// Enter on a hit jumps to that hit's category + field and
        /// closes the overlay. We pick "default tool" (moved to the
        /// Agents tab in the Session split) to also verify the jump
        /// crosses categories cleanly.
        #[test]
        #[serial]
        fn enter_jumps_to_hit_category_and_field() {
            let (_t, mut view) = fresh_view();
            press(&mut view, KeyCode::Char('/'));
            type_text(&mut view, "default tool");
            assert!(!view.search_hits.is_empty(), "no hits for 'default tool'");
            // Position cursor on a hit whose label exactly matches.
            let target_idx = view
                .search_hits
                .iter()
                .position(|h| h.field_label == "Default Tool")
                .expect("Default Tool should appear in hits");
            view.search_selected = target_idx;
            press(&mut view, KeyCode::Enter);

            assert!(
                view.search_input.is_none(),
                "Enter on a hit must close the search overlay"
            );
            assert_eq!(
                view.current_category(),
                crate::tui::settings::SettingsCategory::Agents,
                "must jump to the Agents tab (where Default Tool lives now)"
            );
            assert_eq!(
                view.fields[view.selected_field].key,
                FieldKey::DefaultTool,
                "must position the field cursor on Default Tool"
            );
        }

        /// `j`/`k` (and Down/Up) in the categories panel must skip
        /// non-selectable section dividers so the cursor jumps
        /// category-to-category. Without this, hitting `j` on the last
        /// tab of a section would land on the next section header and
        /// the user would have to press it twice to get to the next
        /// tab.
        #[test]
        #[serial]
        fn category_nav_skips_section_dividers() {
            use crate::tui::settings::CategoryRow;
            let (_t, mut view) = fresh_view();
            // Constructor lands on the first Tab (Theme, the only tab
            // in Appearance). One Down should jump over the next
            // section header ("Sessions") and onto Session.
            let start = view.selected_category;
            assert!(
                matches!(view.categories[start], CategoryRow::Tab(_)),
                "initial selected_category must be a Tab"
            );
            press(&mut view, KeyCode::Down);
            assert!(
                matches!(view.categories[view.selected_category], CategoryRow::Tab(_)),
                "after Down, selected_category must still point at a Tab"
            );
            assert_eq!(
                view.current_category(),
                crate::tui::settings::SettingsCategory::Session,
                "Down from Theme should land on Session, skipping the Sessions section header"
            );
            // Going back up should return to Theme, skipping the
            // Appearance section header.
            press(&mut view, KeyCode::Up);
            assert_eq!(
                view.current_category(),
                crate::tui::settings::SettingsCategory::Theme,
                "Up from Session should return to Theme, skipping the Sessions/Appearance headers"
            );
        }

        /// Esc closes the overlay without changing the selected
        /// category/field; the caller's edit context is preserved.
        #[test]
        #[serial]
        fn esc_closes_search_without_changing_selection() {
            let (_t, mut view) = fresh_view();
            let cat_before = view.selected_category;
            let field_before = view.selected_field;
            press(&mut view, KeyCode::Char('/'));
            type_text(&mut view, "tmux");
            press(&mut view, KeyCode::Esc);
            assert!(view.search_input.is_none());
            assert_eq!(view.selected_category, cat_before);
            assert_eq!(view.selected_field, field_before);
        }
    }

    mod mouse_routing {
        use super::*;
        use crate::session::Storage;
        use crate::tui::settings::{SettingsScope, SettingsView};
        use ratatui::layout::Rect;
        use serial_test::serial;
        use tempfile::TempDir;

        fn setup_test_home(temp: &TempDir) {
            std::env::set_var("HOME", temp.path());
            #[cfg(target_os = "linux")]
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        }

        fn fresh_view() -> (TempDir, SettingsView) {
            let temp = TempDir::new().unwrap();
            setup_test_home(&temp);
            let _ = Storage::new("test").unwrap();
            let view = SettingsView::new("test", None).unwrap();
            (temp, view)
        }

        #[test]
        #[serial]
        fn click_on_scope_tab_switches_scope() {
            let (_t, mut view) = fresh_view();
            // Stage a Profile scope rect at known coords.
            view.scope_tab_rects
                .push((SettingsScope::Profile, Rect::new(40, 0, 18, 1)));
            assert_eq!(view.scope, SettingsScope::Global);
            view.handle_click(45, 0);
            assert_eq!(view.scope, SettingsScope::Profile);
        }

        #[test]
        #[serial]
        fn click_on_scope_tab_with_unsaved_changes_warns() {
            let (_t, mut view) = fresh_view();
            view.has_changes = true;
            view.scope_tab_rects
                .push((SettingsScope::Profile, Rect::new(40, 0, 18, 1)));
            let result = view.handle_click(45, 0);
            assert!(matches!(
                result,
                Some(SettingsAction::UnsavedChangesWarning)
            ));
            assert_eq!(
                view.scope,
                SettingsScope::Global,
                "scope must not change while there are unsaved changes"
            );
        }

        #[test]
        #[serial]
        fn click_on_category_row_focuses_and_selects() {
            let (_t, mut view) = fresh_view();
            view.focus = crate::tui::settings::SettingsFocus::Fields;
            let original = view.selected_category;
            // Pick a different Tab row to stage a click against.
            let other_tab = (0..view.categories.len())
                .find(|&i| {
                    i != original
                        && matches!(
                            view.categories[i],
                            crate::tui::settings::CategoryRow::Tab(_)
                        )
                })
                .expect("expected at least two Tab rows in test layout");
            view.category_rects
                .push((other_tab, Rect::new(0, 10, 20, 1)));
            view.handle_click(5, 10);
            assert_eq!(view.focus, crate::tui::settings::SettingsFocus::Categories);
            assert_eq!(view.selected_category, other_tab);
        }

        #[test]
        #[serial]
        fn click_on_field_focuses_and_selects() {
            let (_t, mut view) = fresh_view();
            view.field_rects.push((0, Rect::new(20, 5, 50, 2)));
            view.field_rects.push((1, Rect::new(20, 8, 50, 2)));
            view.selected_field = 0;
            view.handle_click(25, 9);
            assert_eq!(view.focus, crate::tui::settings::SettingsFocus::Fields);
            assert_eq!(view.selected_field, 1);
        }

        #[test]
        #[serial]
        fn handle_click_returns_none_when_editing() {
            let (_t, mut view) = fresh_view();
            view.editing_input = Some(tui_input::Input::new("typing".to_string()));
            view.scope_tab_rects
                .push((SettingsScope::Profile, Rect::new(40, 0, 18, 1)));
            // A click during edit should NOT switch scope or even
            // resolve a hit; the keyboard's Esc / Enter own the exit.
            assert!(view.handle_click(45, 0).is_none());
            assert_eq!(view.scope, SettingsScope::Global);
        }

        #[test]
        #[serial]
        fn hover_never_moves_focus() {
            // Hover must not shift the keyboard cursor in settings;
            // otherwise the mouse drifting across the fields panel
            // silently changes which field a subsequent Enter / Space
            // targets. Click still navigates.
            let (_t, mut view) = fresh_view();
            view.field_rects.push((0, Rect::new(20, 5, 50, 2)));
            view.field_rects.push((1, Rect::new(20, 8, 50, 2)));
            view.focus = crate::tui::settings::SettingsFocus::Categories;
            view.selected_field = 0;
            view.handle_hover(25, 9);
            assert_eq!(view.focus, crate::tui::settings::SettingsFocus::Categories);
            assert_eq!(view.selected_field, 0);
        }

        #[test]
        #[serial]
        fn hover_records_mouse_pos_and_resolves_to_field() {
            // Hover only paints a visual highlight (drawn by the
            // renderer from `hovered_field()` against `field_rects`);
            // it must not touch keyboard selection state. Verify both:
            // mouse_pos is set and resolves to the right field, but
            // selected_field stays put.
            let (_t, mut view) = fresh_view();
            view.field_rects.push((0, Rect::new(20, 5, 50, 2)));
            view.field_rects.push((1, Rect::new(20, 8, 50, 2)));
            view.selected_field = 0;
            let changed = view.handle_hover(25, 9);
            assert!(changed, "hover entering a new field should redraw");
            assert_eq!(view.hovered_field(), Some(1));
            assert_eq!(view.selected_field, 0, "selection must not move");
        }

        #[test]
        #[serial]
        fn keypress_clears_hover() {
            // A stationary hover left over from before the user
            // switched to keyboard would otherwise stay lit on a row
            // the user is no longer interacting with. Any keystroke
            // invalidates it.
            let (_t, mut view) = fresh_view();
            view.field_rects.push((0, Rect::new(20, 5, 50, 2)));
            view.handle_hover(25, 5);
            assert_eq!(view.hovered_field(), Some(0));
            view.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Down,
                crossterm::event::KeyModifiers::NONE,
            ));
            assert_eq!(view.hovered_field(), None);
        }

        #[test]
        #[serial]
        fn hover_suppressed_while_editing() {
            // While a text field is being edited the rest of the
            // surface is keyboard-only; a lingering hover highlight
            // there would mislead the user about what a click does
            // (in fact, click is also gated during edit).
            let (_t, mut view) = fresh_view();
            view.field_rects.push((0, Rect::new(20, 5, 50, 2)));
            view.editing_input = Some(tui_input::Input::new(String::new()));
            view.handle_hover(25, 5);
            assert_eq!(view.hovered_field(), None);
        }
    }
}
