//! Settings view - configuration management UI

mod fields;
mod input;
mod render;

use tui_input::Input;

use crate::session::{
    list_profiles, load_profile_config, load_repo_config, merge_configs, profile_to_repo_config,
    repo_config_to_profile, save_config, save_profile_config, save_repo_config, Config,
    ProfileConfig, RepoConfig,
};
use crate::tui::dialogs::CustomInstructionDialog;

pub use fields::{FieldValue, HookField, SettingField, SettingsCategory};
pub use input::SettingsAction;

/// Which scope of settings is being edited
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsScope {
    #[default]
    Global,
    Profile,
    Repo,
}

/// Focus state for the settings view
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsFocus {
    #[default]
    Categories,
    Fields,
}

/// State for editing a list field
#[derive(Debug, Clone, Default)]
pub struct ListEditState {
    pub selected_index: usize,
    pub editing_item: Option<Input>,
    pub adding_new: bool,
}

/// One result of the settings-wide search overlay: a field that
/// matched the user's query along with where it lives.
#[derive(Debug, Clone)]
pub(super) struct SearchHit {
    pub category: SettingsCategory,
    /// Stable field identity (`SettingField::ident`) used to relocate the
    /// cursor on jump, since fields are rebuilt from the schema per category.
    pub field_ident: String,
    pub field_label: String,
    pub category_label: &'static str,
}

/// One row in the left-hand categories panel. Sections are
/// non-interactive dividers that group related categories visually
/// (Sessions, Hooks, Environment, etc.); navigation skips past them
/// and `selected_category` is always the index of a `Tab` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CategoryRow {
    Section(&'static str),
    Tab(SettingsCategory),
}

impl CategoryRow {
    fn as_tab(self) -> Option<SettingsCategory> {
        match self {
            CategoryRow::Tab(c) => Some(c),
            CategoryRow::Section(_) => None,
        }
    }
}

/// The settings view state
pub struct SettingsView {
    /// Current profile name being edited
    pub(super) profile: String,

    /// All available profile names (sorted)
    pub(super) available_profiles: Vec<String>,

    /// Project path for repo-level settings (None if no session selected)
    pub(super) project_path: Option<String>,

    /// Repo-level config (original, for load/save)
    pub(super) repo_config: Option<RepoConfig>,

    /// Repo config converted to ProfileConfig for TUI editing (overrides relative to resolved base)
    pub(super) repo_as_profile: ProfileConfig,

    /// Resolved base config (global + profile merged) used as the "global" when editing Repo scope
    pub(super) resolved_base: Config,

    /// Which scope tab is selected
    pub(super) scope: SettingsScope,

    /// Which panel has focus
    pub(super) focus: SettingsFocus,

    /// Rows in the left-hand categories panel: a mix of non-interactive
    /// section dividers and selectable category tabs. `selected_category`
    /// is always the index of a `CategoryRow::Tab` entry.
    pub(super) categories: Vec<CategoryRow>,

    /// Currently selected category-row index. Points at a `Tab`
    /// row; navigation helpers maintain this invariant.
    pub(super) selected_category: usize,

    /// Fields for the current category
    pub(super) fields: Vec<SettingField>,

    /// Currently selected field index
    pub(super) selected_field: usize,

    /// Global config being edited
    pub(super) global_config: Config,

    /// Profile config being edited (overrides)
    pub(super) profile_config: ProfileConfig,

    /// Text input when editing a text/number field
    pub(super) editing_input: Option<Input>,

    /// State for list editing
    pub(super) list_edit_state: Option<ListEditState>,

    /// Custom instruction editor dialog
    pub(super) custom_instruction_dialog: Option<CustomInstructionDialog>,

    /// Scroll offset for the fields panel (in lines)
    pub(super) fields_scroll_offset: u16,

    /// Last known viewport height for the fields panel (set during render)
    pub(super) fields_viewport_height: u16,

    /// Last known content width for the fields panel (set during render).
    /// Used to compute description wrap heights outside the render pass,
    /// so `ensure_field_visible` and the scroll math match what the
    /// next frame will actually paint.
    pub(super) fields_content_width: u16,

    /// Whether there are unsaved changes
    pub(super) has_changes: bool,

    /// Whether the help overlay is shown
    pub(super) show_help: bool,

    /// Error message to display
    pub(super) error_message: Option<String>,

    /// Success message to display
    pub(super) success_message: Option<String>,

    /// Active search input. `Some` while the user is typing in the
    /// settings-wide `/` search overlay. The settings view freezes
    /// the categories/fields panels behind the overlay and routes
    /// keys to the input + hit list until the user picks a hit or
    /// hits Esc.
    pub(super) search_input: Option<Input>,

    /// Hits that match the current `search_input` query, recomputed
    /// each time the query changes. Empty query lists every
    /// interactive field across every category, so the user can
    /// browse the full catalog as a flat list sorted by category
    /// then by field order.
    pub(super) search_hits: Vec<SearchHit>,

    /// Cursor inside `search_hits`, bounded by `search_hits.len()`
    /// so it stays valid as the query narrows.
    pub(super) search_selected: usize,

    /// Hit rect per scope tab in the header. Captured during render
    /// so a click on `[ Global ]` / `[ Profile ]` / `[ Repo ]` can
    /// switch scope without going through the keyboard. Cleared and
    /// repopulated each frame.
    pub(super) scope_tab_rects: Vec<(SettingsScope, ratatui::layout::Rect)>,
    /// Hit rect per row in the categories panel, indexed into
    /// `self.categories`. Only Tab rows are pushed; Section dividers
    /// are skipped so a click on a heading is a no-op.
    pub(super) category_rects: Vec<(usize, ratatui::layout::Rect)>,
    /// Hit rect per visible field row, indexed into `self.fields`.
    /// Skipped while a field is being edited or a list is being
    /// edited so a stray click during composition doesn't reset focus.
    pub(super) field_rects: Vec<(usize, ratatui::layout::Rect)>,
    /// Last `(col, row)` reported by a `MouseEventKind::Moved` event
    /// while a non-editing settings surface is in view. Drives the
    /// hover highlight on scope chips, categories, and fields, kept
    /// separate from `selected_*` / `focus` so the mouse never
    /// disturbs the keyboard cursor. Cleared on every keypress so
    /// hover doesn't linger after the user switches modalities.
    pub(super) mouse_pos: Option<(u16, u16)>,
}

impl SettingsView {
    pub fn new(profile: &str, project_path: Option<String>) -> anyhow::Result<Self> {
        let global_config = Config::load()?;
        let profile_config = load_profile_config(profile)?;

        let repo_config = project_path
            .as_ref()
            .and_then(|p| load_repo_config(std::path::Path::new(p)).ok().flatten());

        let resolved_base = merge_configs(global_config.clone(), &profile_config);
        let repo_as_profile = repo_config
            .as_ref()
            .map(repo_config_to_profile)
            .unwrap_or_default();

        let mut available_profiles = match list_profiles() {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(target: "tui.settings", "Failed to list profiles: {e}");
                Vec::new()
            }
        };
        if !available_profiles.contains(&profile.to_string()) {
            available_profiles.push(profile.to_string());
            available_profiles.sort();
        }

        let categories = Self::categories_for_scope(SettingsScope::Global);

        let mut view = Self {
            profile: profile.to_string(),
            available_profiles,
            project_path,
            repo_config,
            repo_as_profile,
            resolved_base,
            scope: SettingsScope::Global,
            focus: SettingsFocus::Categories,
            categories,
            // 0 is the leading section divider; seek to the first
            // Tab below so the user lands on a real category.
            selected_category: 0,
            fields: Vec::new(),
            selected_field: 0,
            global_config,
            profile_config,
            editing_input: None,
            list_edit_state: None,
            custom_instruction_dialog: None,
            fields_scroll_offset: 0,
            fields_viewport_height: 0,
            fields_content_width: 0,
            has_changes: false,
            show_help: false,
            error_message: None,
            success_message: None,
            search_input: None,
            search_hits: Vec::new(),
            search_selected: 0,
            scope_tab_rects: Vec::new(),
            category_rects: Vec::new(),
            field_rects: Vec::new(),
            mouse_pos: None,
        };

        // The constructor parks `selected_category` at 0, which is the
        // first section divider in the layout. Snap to the first real
        // Tab before the first render so the cursor lands on Theme.
        view.selected_category = view.first_tab_index();
        view.rebuild_fields();
        Ok(view)
    }

    /// Build the categories-panel layout. Categories are grouped under
    /// section dividers (Appearance / Sessions / Hooks / Environment /
    /// Notifications / System) so the list isn't 14 unrelated tabs in
    /// arbitrary order. Status Hooks is dropped in Repo scope (the only
    /// scope-conditional category today).
    fn categories_for_scope(scope: SettingsScope) -> Vec<CategoryRow> {
        let mut rows: Vec<CategoryRow> = Vec::new();
        let push_section = |rows: &mut Vec<CategoryRow>, label: &'static str| {
            rows.push(CategoryRow::Section(label));
        };
        let push_tab = |rows: &mut Vec<CategoryRow>, cat: SettingsCategory| {
            rows.push(CategoryRow::Tab(cat));
        };

        push_section(&mut rows, "Appearance");
        push_tab(&mut rows, SettingsCategory::Theme);

        push_section(&mut rows, "Sessions");
        push_tab(&mut rows, SettingsCategory::Session);
        push_tab(&mut rows, SettingsCategory::Agents);
        push_tab(&mut rows, SettingsCategory::Interaction);
        push_tab(&mut rows, SettingsCategory::Diff);
        push_tab(&mut rows, SettingsCategory::Acp);

        push_section(&mut rows, "Hooks");
        push_tab(&mut rows, SettingsCategory::Hooks);
        if scope != SettingsScope::Repo {
            push_tab(&mut rows, SettingsCategory::StatusHooks);
        }

        push_section(&mut rows, "Environment");
        push_tab(&mut rows, SettingsCategory::Sandbox);
        push_tab(&mut rows, SettingsCategory::Worktree);
        push_tab(&mut rows, SettingsCategory::Tmux);

        push_section(&mut rows, "Notifications");
        push_tab(&mut rows, SettingsCategory::Sound);
        push_tab(&mut rows, SettingsCategory::Web);

        push_section(&mut rows, "System");
        push_tab(&mut rows, SettingsCategory::Updates);
        // Telemetry is an install-level consent toggle, not a per-profile or
        // per-repo setting, so it only appears under the Global scope.
        if scope == SettingsScope::Global {
            push_tab(&mut rows, SettingsCategory::Telemetry);
        }
        push_tab(&mut rows, SettingsCategory::Logging);

        rows
    }

    /// Scope chip currently under the mouse cursor, if any. Resolved
    /// each call against the rects captured by the last render. Used
    /// for the hover highlight only; click + keyboard own the actual
    /// selection.
    pub(super) fn hovered_scope(&self) -> Option<SettingsScope> {
        let (col, row) = self.mouse_pos?;
        let pos = ratatui::layout::Position::from((col, row));
        self.scope_tab_rects
            .iter()
            .find(|(_, rect)| rect.contains(pos))
            .map(|(scope, _)| *scope)
    }

    /// Category-row index under the mouse cursor, if any.
    pub(super) fn hovered_category(&self) -> Option<usize> {
        let (col, row) = self.mouse_pos?;
        let pos = ratatui::layout::Position::from((col, row));
        self.category_rects
            .iter()
            .find(|(_, rect)| rect.contains(pos))
            .map(|(idx, _)| *idx)
    }

    /// Field-row index under the mouse cursor, if any.
    pub(super) fn hovered_field(&self) -> Option<usize> {
        let (col, row) = self.mouse_pos?;
        let pos = ratatui::layout::Position::from((col, row));
        self.field_rects
            .iter()
            .find(|(_, rect)| rect.contains(pos))
            .map(|(idx, _)| *idx)
    }

    /// The category at `selected_category`, by invariant always a
    /// `Tab` row. Falls back to the first tab in the list if the
    /// invariant is violated (e.g., an empty layout), so callers can
    /// dereference without panicking.
    pub(super) fn current_category(&self) -> SettingsCategory {
        self.categories
            .get(self.selected_category)
            .and_then(|row| row.as_tab())
            .or_else(|| self.categories.iter().find_map(|r| r.as_tab()))
            .expect("layout has at least one Tab row")
    }

    pub(super) fn rebuild_categories_for_scope(&mut self) {
        let current = self
            .categories
            .get(self.selected_category)
            .and_then(|row| row.as_tab());
        self.categories = Self::categories_for_scope(self.scope);
        self.selected_category = current
            .and_then(|category| {
                self.categories
                    .iter()
                    .position(|r| *r == CategoryRow::Tab(category))
            })
            .unwrap_or_else(|| self.first_tab_index());
    }

    /// First selectable row in `self.categories`. Section dividers are
    /// not selectable, so the initial cursor and post-rebuild fallback
    /// must land on a `Tab`. Layout always starts with a section
    /// header so the answer is typically `1`, but this is computed
    /// rather than hard-coded.
    pub(super) fn first_tab_index(&self) -> usize {
        self.categories
            .iter()
            .position(|r| matches!(r, CategoryRow::Tab(_)))
            .unwrap_or(0)
    }

    /// Rebuild the fields list based on current category and scope
    pub(super) fn rebuild_fields(&mut self) {
        let category = self.current_category();
        let (scope_for_fields, global_ref, profile_ref) = match self.scope {
            SettingsScope::Global => (
                SettingsScope::Global,
                &self.global_config,
                &self.profile_config,
            ),
            SettingsScope::Profile => (
                SettingsScope::Profile,
                &self.global_config,
                &self.profile_config,
            ),
            SettingsScope::Repo => (
                SettingsScope::Profile,
                &self.resolved_base,
                &self.repo_as_profile,
            ),
        };
        self.fields =
            fields::build_fields_for_category(category, scope_for_fields, global_ref, profile_ref);
        if self.selected_field >= self.fields.len() {
            self.selected_field = 0;
        }
        self.fields_scroll_offset = 0;
        // If the (clamped) selected_field landed on a non-interactive
        // section divider, advance to the next real field so the user
        // never sees the cursor parked on a heading.
        self.snap_to_interactive_field_forward();
    }

    /// Advance `selected_field` to the first interactive field
    /// (`!is_section_header`) at or after the current index. Used
    /// after a category change so we don't land on a non-editable
    /// section divider when the new tab happens to begin with one.
    pub(super) fn snap_to_interactive_field_forward(&mut self) {
        let mut idx = self.selected_field;
        while idx < self.fields.len() && self.fields[idx].is_section_header() {
            idx += 1;
        }
        if idx < self.fields.len() {
            self.selected_field = idx;
        }
    }

    /// Switch to a different profile, reloading its config from disk
    pub(super) fn switch_profile(&mut self, new_profile: &str) -> anyhow::Result<()> {
        self.profile = new_profile.to_string();
        self.profile_config = load_profile_config(new_profile)?;
        self.resolved_base = merge_configs(self.global_config.clone(), &self.profile_config);
        self.repo_as_profile = self
            .repo_config
            .as_ref()
            .map(repo_config_to_profile)
            .unwrap_or_default();
        self.rebuild_fields();
        Ok(())
    }

    /// Ensure the selected field is visible within the given viewport height.
    /// Call this after changing `selected_field`.
    pub(super) fn ensure_field_visible(&mut self, viewport_height: u16) {
        let mut y = 0u16;
        let mut selected_y = 0u16;
        let mut selected_h = 0u16;

        for (i, field) in self.fields.iter().enumerate() {
            let h = self.field_height(field, i);
            if i == self.selected_field {
                selected_y = y;
                selected_h = h;
                break;
            }
            y += h + 1; // +1 spacing
        }

        // Scroll up if field starts above viewport
        if selected_y < self.fields_scroll_offset {
            self.fields_scroll_offset = selected_y;
        }
        // Scroll down if field ends below viewport
        let field_bottom = selected_y + selected_h;
        if field_bottom > self.fields_scroll_offset + viewport_height {
            self.fields_scroll_offset = field_bottom.saturating_sub(viewport_height);
        }
    }

    /// Apply the current field values back to the configs
    pub(super) fn apply_field_to_config(&mut self, field_index: usize) {
        if field_index >= self.fields.len() {
            return;
        }

        let field = &self.fields[field_index];
        let is_telemetry = field.ident() == "telemetry.enabled";

        match self.scope {
            SettingsScope::Global | SettingsScope::Profile => {
                fields::apply_field_to_config(
                    field,
                    self.scope,
                    &mut self.global_config,
                    &mut self.profile_config,
                );
                // Editing the telemetry toggle counts as responding to the
                // opt-in prompt, so the one-time standalone consent popup
                // never re-appears for a user who already made a choice here.
                if is_telemetry {
                    self.global_config.app_state.has_responded_to_telemetry = true;
                }
            }
            SettingsScope::Repo => {
                // Use Profile logic but against resolved_base and repo_as_profile
                fields::apply_field_to_config(
                    field,
                    SettingsScope::Profile,
                    &mut self.resolved_base,
                    &mut self.repo_as_profile,
                );
                // Sync back to repo_config
                self.repo_config = Some(profile_to_repo_config(&self.repo_as_profile));
            }
        }
        self.has_changes = true;
    }

    /// Save the current configuration
    pub fn save(&mut self) -> anyhow::Result<()> {
        // Validate all fields before saving
        for field in &self.fields {
            if let Err(e) = field.validate() {
                self.error_message = Some(e);
                return Ok(());
            }
        }

        match self.scope {
            SettingsScope::Global => {
                // Saving the Telemetry page counts as answering the opt-in
                // prompt even if the toggle was left untouched, so the one-time
                // standalone popup doesn't reappear for someone who reviewed it
                // here and chose to leave it off.
                if self.current_category() == SettingsCategory::Telemetry {
                    self.global_config.app_state.has_responded_to_telemetry = true;
                }
                save_config(&self.global_config)?;
                self.resolved_base =
                    merge_configs(self.global_config.clone(), &self.profile_config);
                // Persist + live-apply the logging filter so a running
                // `aoe serve` daemon (and its structured view runners) pick up the
                // change without a restart. No-ops when no controller is
                // installed (TUI-only process).
                if let Ok(app_dir) = crate::session::get_app_dir() {
                    crate::logging::apply_persisted_config(
                        &self.global_config.logging.default_level,
                        &self.global_config.logging.targets,
                        &app_dir,
                    );
                }
                crate::session::poller::set_session_id_poller_max_threads(
                    self.global_config.session.session_id_poller_max_threads,
                );
                // Reconcile the on-disk install id with the saved opt-in
                // state: generate one when enabled, delete it on opt-out.
                // Idempotent, so running it on every global save is safe.
                crate::telemetry::apply_opt_in_change(self.global_config.telemetry.enabled);
            }
            SettingsScope::Profile => {
                save_profile_config(&self.profile, &self.profile_config)?;
            }
            SettingsScope::Repo => {
                if let (Some(ref project_path), Some(ref repo_config)) =
                    (&self.project_path, &self.repo_config)
                {
                    save_repo_config(std::path::Path::new(project_path), repo_config)?;
                }
            }
        }

        self.has_changes = false;
        self.success_message = Some("Settings saved".to_string());
        self.error_message = None;
        Ok(())
    }

    /// Check if there are unsaved changes
    pub fn has_unsaved_changes(&self) -> bool {
        self.has_changes
    }

    /// Check if currently in an editing state (text field, list, dialog, etc.)
    pub fn is_editing(&self) -> bool {
        self.editing_input.is_some()
            || self.list_edit_state.is_some()
            || self.custom_instruction_dialog.is_some()
            || self.search_input.is_some()
    }

    /// Open the settings-wide search overlay. Builds the initial hit
    /// list (empty query → all interactive fields across every
    /// visible category) and parks the cursor at the top so Enter on
    /// an empty search picks the first hit instead of doing nothing.
    pub(super) fn open_search(&mut self) {
        self.search_input = Some(Input::default());
        self.search_selected = 0;
        self.recompute_search_hits();
    }

    /// Close the search overlay without changing the selected
    /// category/field. Keeps the caller's edit context (focus, scope,
    /// scroll) intact.
    pub(super) fn close_search(&mut self) {
        self.search_input = None;
        self.search_hits.clear();
        self.search_selected = 0;
    }

    /// Rebuild `search_hits` from the current `search_input` query.
    /// Iterates every visible category for the current scope, calls
    /// the same `build_fields_for_category` the main panel uses, and
    /// keeps fields whose label or description contains every
    /// whitespace-separated query token (case-insensitive). Empty
    /// query keeps every interactive field; section-header rows are
    /// always skipped because the user can't jump to them.
    pub(super) fn recompute_search_hits(&mut self) {
        let query = self
            .search_input
            .as_ref()
            .map(|i| i.value().to_string())
            .unwrap_or_default();
        let tokens: Vec<String> = query.split_whitespace().map(|t| t.to_lowercase()).collect();

        let (scope_for_fields, global_ref, profile_ref) = match self.scope {
            SettingsScope::Global => (
                SettingsScope::Global,
                &self.global_config,
                &self.profile_config,
            ),
            SettingsScope::Profile => (
                SettingsScope::Profile,
                &self.global_config,
                &self.profile_config,
            ),
            SettingsScope::Repo => (
                SettingsScope::Profile,
                &self.resolved_base,
                &self.repo_as_profile,
            ),
        };

        let mut hits: Vec<SearchHit> = Vec::new();
        for category in self.categories.iter().filter_map(|r| r.as_tab()) {
            let fields = fields::build_fields_for_category(
                category,
                scope_for_fields,
                global_ref,
                profile_ref,
            );
            for field in fields {
                if field.is_section_header() {
                    continue;
                }
                let label_lower = field.label.to_lowercase();
                let desc_lower = field.description.to_lowercase();
                let matches_all = tokens
                    .iter()
                    .all(|t| label_lower.contains(t) || desc_lower.contains(t));
                if !matches_all {
                    continue;
                }
                hits.push(SearchHit {
                    category,
                    field_ident: field.ident(),
                    field_label: field.label.clone(),
                    category_label: category.label(),
                });
            }
        }

        self.search_hits = hits;
        if self.search_selected >= self.search_hits.len() {
            self.search_selected = self.search_hits.len().saturating_sub(1);
        }
    }

    /// Jump to the currently-selected search hit: switch to its
    /// category, rebuild fields for the new category, position the
    /// field cursor on the matching key, and close the overlay.
    /// No-op when the hit list is empty (Enter on a query with no
    /// matches stays in search so the user can correct the query).
    pub(super) fn jump_to_selected_search_hit(&mut self) {
        let Some(hit) = self.search_hits.get(self.search_selected).cloned() else {
            return;
        };
        if let Some(idx) = self
            .categories
            .iter()
            .position(|r| *r == CategoryRow::Tab(hit.category))
        {
            self.selected_category = idx;
        }
        self.rebuild_fields();
        if let Some(idx) = self
            .fields
            .iter()
            .position(|f| f.ident() == hit.field_ident)
        {
            self.selected_field = idx;
            self.ensure_field_visible(self.fields_viewport_height);
        }
        self.focus = SettingsFocus::Fields;
        self.close_search();
    }
}
