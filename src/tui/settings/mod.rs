//! Settings view - configuration management UI

mod fields;
mod input;
mod render;

use tui_input::Input;

use crate::session::{
    list_profiles, load_profile_config, load_repo_config, merge_configs, profile_to_repo_config,
    repo_config_to_profile, save_profile_config, save_repo_config, update_app_state, update_config,
    Config, ProfileConfig, RepoConfig,
};
use crate::tui::dialogs::CustomInstructionDialog;

pub use fields::{FieldValue, HookField, SettingField, SettingsCategory};
pub use input::SettingsAction;

/// How long the "Settings saved" toast lingers before it auto-dismisses.
/// Matches the dashboard's transient update-bar window (`app.rs`).
const SUCCESS_MESSAGE_TTL: std::time::Duration = std::time::Duration::from_secs(10);

/// Serialize a config (or `Option<RepoConfig>`) to JSON for change detection.
/// Comparing the serialized form (the same representation that gets written to
/// disk) sidesteps adding `PartialEq` to every nested config type, and a
/// serialization failure degrades to `Null` so two failures compare equal
/// rather than spuriously flagging changes.
fn config_to_json<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

/// Bonus a query token earns for matching a hit's title (category +
/// label) rather than only its description prose. Sized to dominate any
/// per-token nucleo score so title matches always rank first; a field
/// that merely mentions the term in its description still matches, just
/// lower in the popup.
const TITLE_MATCH_BONUS: u32 = 100_000;

/// Fuzzy-score a field against a settings-search query. The query is
/// split on whitespace and every token must fuzzy-match somewhere in
/// `title` (category label + field label) or `full` (title +
/// description); AND semantics, so "max workers" still matches "Max
/// Concurrent Workers". Per-token scores are summed so closer matches
/// rank higher, and a title match earns [`TITLE_MATCH_BONUS`] so
/// "sandbox" surfaces the Sandbox tab's own settings before fields
/// that only mention it in prose. An empty query scores every field 0,
/// which keeps the popup listing all fields in their natural order.
/// The fuzzy match also covers acronyms, so "mcw" finds "Max
/// Concurrent Workers". Reuses the same nucleo pattern as the command
/// palette.
fn fuzzy_settings_score(query: &str, title: &str, full: &str) -> Option<u32> {
    use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
    use nucleo_matcher::{Config, Matcher, Utf32Str};

    let tokens: Vec<&str> = query.split_whitespace().collect();
    if tokens.is_empty() {
        return Some(0);
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buf = Vec::new();
    let mut total: u32 = 0;
    for token in tokens {
        let atom = Atom::new(
            token,
            CaseMatching::Ignore,
            Normalization::Smart,
            AtomKind::Fuzzy,
            false,
        );
        let title_hay = Utf32Str::new(title, &mut buf);
        if let Some(score) = atom.score(title_hay, &mut matcher) {
            total += score as u32 + TITLE_MATCH_BONUS;
            continue;
        }
        let full_hay = Utf32Str::new(full, &mut buf);
        let score = atom.score(full_hay, &mut matcher)?;
        total += score as u32;
    }
    Some(total)
}

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

/// One result in the settings-search jump popup: a field that matched
/// the user's query along with where it lives.
#[derive(Debug, Clone)]
pub(super) struct SearchHit {
    pub category: SettingsCategory,
    /// Stable field identity (`SettingField::ident`) used to relocate the
    /// cursor on jump, since fields are rebuilt from the schema per category.
    pub field_ident: String,
    pub field_label: String,
    pub category_label: &'static str,
    /// Current value of the field at the time the hit list was built,
    /// rendered dimmed after the label so the popup doubles as a quick
    /// way to review settings without jumping to each one (issue #2932).
    /// Safe to snapshot: editing is frozen while the popup is open.
    pub value_display: String,
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

    /// Whether there are unsaved changes. Recomputed on every edit by diffing
    /// the live configs against [`Self::baseline_*`], so reverting a field back
    /// to its saved value clears the flag rather than latching it (issue #2083).
    pub(super) has_changes: bool,

    /// Serialized snapshots of the editable configs as of the last load or
    /// save. The unsaved-changes flag compares the live configs against these.
    pub(super) baseline_global: serde_json::Value,
    pub(super) baseline_profile: serde_json::Value,
    pub(super) baseline_repo: serde_json::Value,

    /// Whether the help overlay is shown
    pub(super) show_help: bool,

    /// Error message to display
    pub(super) error_message: Option<String>,

    /// Success message to display (e.g. "Settings saved"). Rendered in the
    /// footer status row, not over the fields.
    pub(super) success_message: Option<String>,

    /// When the success toast should auto-dismiss. Set alongside
    /// `success_message` on save so the "Settings saved" notice fades on its
    /// own if the user just walks away, mirroring the dashboard's transient
    /// update bar. Errors are sticky and have no expiry.
    pub(super) success_message_expires_at: Option<std::time::Instant>,

    /// The settings-search query. `Some` while search is active: the
    /// permanent bar becomes the input and the jump popup renders
    /// beneath it with the ranked hits; keys route to the query + hit
    /// list until the user picks a hit (Enter jumps to it) or hits
    /// Esc. `None` is the idle bar with its placeholder.
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

    /// Captured by the popup render: the screen row of each visible
    /// hit along with its `search_hits` index. Drives click + hover
    /// routing without re-deriving the scroll math (the command
    /// palette's `visible_item_rows` pattern).
    pub(super) search_hit_rows: Vec<(u16, usize)>,

    /// Rect of the rendered popup frame. Click routing uses it to
    /// distinguish "inside popup but missed a row" (no-op) from
    /// "outside popup" (dismiss).
    pub(super) search_popup_area: ratatui::layout::Rect,

    /// Rect of the permanent search bar, captured each frame so a
    /// click on the idle bar opens the search like typing `/` does.
    pub(super) search_bar_rect: ratatui::layout::Rect,

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

    /// Embedded plugin manager for the Plugins category: the same dialog the
    /// command palette opens (`crate::tui::dialogs::PluginManagerDialog`),
    /// hosted inline so the builtin plugin list lives on the settings screen.
    /// One implementation, reused; it reloads its own list on mutation.
    pub(super) plugin_manager: crate::tui::dialogs::PluginManagerDialog,

    /// Sub-focus within the Plugins category's right pane: `false` targets
    /// the plugin manager (top), `true` the editable plugin settings fields
    /// beneath it. Tab toggles; reset when the field list rebuilds.
    pub(super) plugins_fields_focus: bool,
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

        let baseline_global = config_to_json(&global_config);
        let baseline_profile = config_to_json(&profile_config);
        let baseline_repo = config_to_json(&repo_config);

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
            baseline_global,
            baseline_profile,
            baseline_repo,
            show_help: false,
            error_message: None,
            success_message: None,
            success_message_expires_at: None,
            search_input: None,
            search_hits: Vec::new(),
            search_selected: 0,
            search_hit_rows: Vec::new(),
            search_popup_area: ratatui::layout::Rect::default(),
            search_bar_rect: ratatui::layout::Rect::default(),
            scope_tab_rects: Vec::new(),
            category_rects: Vec::new(),
            field_rects: Vec::new(),
            mouse_pos: None,
            plugin_manager: crate::tui::dialogs::PluginManagerDialog::embedded(),
            plugins_fields_focus: false,
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
        // Plugin enable/disable is stored in the global config, so the manager
        // tab (which stages toggles into it) only appears under Global scope.
        if scope == SettingsScope::Global {
            push_tab(&mut rows, SettingsCategory::Plugins);
        }

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

    /// The `(scope, base, overrides)` triple `build_fields_for_category`
    /// needs for the current scope tab. Repo scope edits repo overrides
    /// relative to the resolved global+profile base, reusing the Profile
    /// build path.
    fn field_build_inputs(&self) -> (SettingsScope, &Config, &ProfileConfig) {
        match self.scope {
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
        }
    }

    /// Rebuild the fields list based on current category and scope
    pub(super) fn rebuild_fields(&mut self) {
        let category = self.current_category();
        let (scope_for_fields, global_ref, profile_ref) = self.field_build_inputs();
        let built =
            fields::build_fields_for_category(category, scope_for_fields, global_ref, profile_ref);
        self.fields = built;
        // Master-detail on the Plugins tab: the fields pane tracks the
        // manager's selected plugin, so only that plugin's settings render
        // beneath the list (moving the list selection swaps the pane).
        if category == SettingsCategory::Plugins {
            let selected = self.plugin_manager.selected().map(|p| p.id.clone());
            self.fields.retain(|f| {
                f.schema_section()
                    .and_then(crate::session::settings_schema::section_plugin_id)
                    == selected.as_deref()
            });
        }
        if self.selected_field >= self.fields.len() {
            self.selected_field = 0;
        }
        self.fields_scroll_offset = 0;
        // With no fields there is no pane to sub-focus; otherwise keep the
        // Plugins sub-focus where the user left it (a save or plugin mutation
        // rebuilds this list and must not yank focus back to the manager).
        if self.fields.is_empty() {
            self.plugins_fields_focus = false;
        }
        // If the (clamped) selected_field landed on a non-interactive
        // section divider, advance to the next real field so the user
        // never sees the cursor parked on a heading.
        self.snap_to_interactive_field_forward();
    }

    /// Re-sync the in-memory `plugins` config after the embedded manager
    /// mutated it on disk (enable/disable/install/update/uninstall write
    /// immediately and reload the registry). Without this, a later settings
    /// save would write the stale `plugins` table and clobber the change.
    /// Only the `plugins` subtree is touched, so unrelated unsaved edits stay
    /// flagged.
    pub(super) fn resync_after_plugin_mutation(&mut self) {
        let Ok(disk) = Config::load() else {
            return;
        };
        // The user may hold unsaved staged edits (a staged toggle, an edited
        // plugin setting) while a lifecycle operation rewrites plugin config
        // on disk. Re-apply the staged diff (staged vs old baseline) on top
        // of the fresh disk state so those edits survive the resync. The
        // merge is per field: only the user-editable fields (`enabled`,
        // `settings`) carry staged diffs; the lifecycle-owned fields
        // (`source`, `grant`, `dismissed_update`) always take the disk value,
        // so a staged toggle can never wipe a grant the operation just wrote.
        let old_baseline: std::collections::BTreeMap<String, crate::session::PluginConfig> = self
            .baseline_global
            .get("plugins")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let staged = std::mem::take(&mut self.global_config.plugins);
        let baseline_val = serde_json::to_value(&disk.plugins);
        self.global_config.plugins = disk.plugins;
        for (id, staged_config) in staged {
            let was_in_baseline = old_baseline.contains_key(&id);
            match self.global_config.plugins.get_mut(&id) {
                Some(disk_entry) => {
                    let base = old_baseline.get(&id).cloned().unwrap_or_default();
                    if staged_config.enabled != base.enabled {
                        disk_entry.enabled = staged_config.enabled;
                    }
                    if staged_config.settings != base.settings {
                        disk_entry.settings = staged_config.settings;
                    }
                }
                None => {
                    // Not on disk: keep a purely user-staged entry (a first
                    // toggle for a plugin with no config row yet); drop edits
                    // for an id the operation removed (it was in the
                    // baseline, so the removal is the newer intent).
                    if !was_in_baseline {
                        self.global_config.plugins.insert(id, staged_config);
                    }
                }
            }
        }
        if let (Some(obj), Ok(plugins_val)) = (self.baseline_global.as_object_mut(), baseline_val) {
            obj.insert("plugins".to_string(), plugins_val);
        }
        self.recompute_dirty();
        // An install/uninstall/re-approve changes the active plugin set, and
        // with it the virtual `plugin:<id>` settings sections; rebuild so the
        // fields pane under the manager tracks it.
        self.rebuild_fields();
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
        self.recompute_dirty();
    }

    /// Recompute `has_changes` by diffing the live configs against the
    /// baselines. Editing a field and reverting it leaves the configs
    /// byte-identical to the last save, so this clears the flag instead of
    /// leaving a phantom "unsaved changes" warning (issue #2083).
    pub(super) fn recompute_dirty(&mut self) {
        self.has_changes = config_to_json(&self.global_config) != self.baseline_global
            || config_to_json(&self.profile_config) != self.baseline_profile
            || config_to_json(&self.repo_config) != self.baseline_repo;
    }

    /// Adopt the live configs as the new baseline and mark the view clean.
    /// Called after a save or a reload, when on-disk state matches memory.
    pub(super) fn snapshot_baseline(&mut self) {
        self.baseline_global = config_to_json(&self.global_config);
        self.baseline_profile = config_to_json(&self.profile_config);
        self.baseline_repo = config_to_json(&self.repo_config);
        self.has_changes = false;
    }

    /// Save the current configuration
    pub fn save(&mut self) -> anyhow::Result<()> {
        // Validate all fields before saving. Prefix the field's label so the
        // message points at the offending setting instead of a bare reason
        // like "expected a string" with no clue which row it came from
        // (issue #2083).
        for field in &self.fields {
            if let Err(e) = field.validate() {
                self.error_message = Some(format!("{}: {e}", field.label));
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
                let has_responded_to_telemetry =
                    self.global_config.app_state.has_responded_to_telemetry;
                // Write back only the leaves the user actually edited, diffed
                // against the snapshot taken when this view opened, rather than
                // the whole in-memory `Config`. `update_config` hands us a
                // fresh on-disk load; overwriting it wholesale with a snapshot
                // captured at open would revert anything another process (an
                // `aoe serve` PATCH, a second `aoe`, a hand edit) wrote to an
                // unrelated field while the pane sat open, which is the same
                // clobber the removed `save_config` caused.
                let edited = config_to_json(&self.global_config);
                let baseline = self.baseline_global.clone();
                update_config(|c| -> anyhow::Result<()> {
                    let mut fresh = serde_json::to_value(&*c)?;
                    crate::session::settings_schema::apply_changed_leaves(
                        &mut fresh, &baseline, &edited,
                    );
                    *c = serde_json::from_value(fresh)?;
                    Ok(())
                })??;
                // `app_state` lives in state.toml now (not persisted by
                // `update_config`); only write it when this save actually
                // flipped it, so an already-true flag on disk is never
                // clobbered back to false by an unrelated global save.
                if has_responded_to_telemetry {
                    update_app_state(|state| {
                        state.has_responded_to_telemetry = true;
                    })?;
                }
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

        // Plugin enable/disable lives in `config.plugins`. When that subtree
        // changed, reload the registry so the save takes effect live (a
        // disabled plugin drops from the active set). Compared against the
        // still-old baseline before snapshotting. Mirrors what the immediate
        // `aoe plugin enable/disable` CLI path does. A running daemon's
        // workers are nudged too (best-effort, fire-and-forget): the save
        // wrote config wholesale, which a daemon never watches.
        if self.scope == SettingsScope::Global {
            let now_plugins = serde_json::to_value(&self.global_config.plugins).ok();
            if now_plugins.as_ref() != self.baseline_global.get("plugins") {
                crate::plugin::reload_registry();
                // The active set changed, so the virtual `plugin:<id>`
                // settings sections may have too.
                self.rebuild_fields();
                let changes =
                    plugin_enabled_changes(self.baseline_global.get("plugins"), &now_plugins);
                if !changes.is_empty() {
                    crate::plugin::install::nudge_daemon_enabled(changes);
                }
            }
        }

        // The just-written state is the new clean baseline.
        self.snapshot_baseline();
        self.success_message = Some("Settings saved".to_string());
        self.success_message_expires_at = Some(std::time::Instant::now() + SUCCESS_MESSAGE_TTL);
        self.error_message = None;
        Ok(())
    }

    /// Drop the transient "Settings saved" toast once its window passes, so it
    /// fades even when the user leaves the keyboard idle. Returns whether the
    /// toast was cleared so the caller can request a redraw. Errors are sticky
    /// (no expiry) and clear only on the next keypress.
    pub fn tick_status(&mut self) -> bool {
        // Poll the embedded plugin manager's in-flight discovery / update /
        // install / uninstall task so its results land without waiting for the
        // next keypress. A completed lifecycle operation rewrote plugin config
        // on disk; resync right away so this view's staged copy (and the dirty
        // marker) never lags a keypress behind.
        let plugin_changed = self.plugin_manager.tick();
        if plugin_changed && self.plugin_manager.take_mutated() {
            self.resync_after_plugin_mutation();
        }
        let toast_changed = match self.success_message_expires_at {
            Some(expires_at) if std::time::Instant::now() >= expires_at => {
                self.success_message = None;
                self.success_message_expires_at = None;
                true
            }
            _ => false,
        };
        plugin_changed || toast_changed
    }

    /// Open the settings search: the permanent bar becomes the query
    /// input and the jump popup lists the hits beneath it. Builds the
    /// initial hit list (empty query lists every interactive field
    /// across every visible category) and parks the cursor at the top
    /// so Enter on an empty search picks the first hit instead of
    /// doing nothing.
    pub(super) fn open_search(&mut self) {
        self.search_input = Some(Input::default());
        self.search_selected = 0;
        self.recompute_search_hits();
    }

    /// Close the search popup without changing the selected
    /// category/field. Keeps the caller's edit context (focus, scope,
    /// scroll) intact; the bar returns to its idle placeholder.
    pub(super) fn close_search(&mut self) {
        self.search_input = None;
        self.search_hits.clear();
        self.search_selected = 0;
    }

    /// Rebuild `search_hits` from the current `search_input` query.
    /// Iterates every visible category for the current scope, calls
    /// the same `build_fields_for_category` the main panel uses, and
    /// keeps fields where every whitespace-separated query token
    /// fuzzy-matches the category label + field label + description.
    /// Hits are ranked best-match-first (title matches above
    /// description-only mentions); empty query keeps every interactive
    /// field in natural order. Section-header rows are always skipped
    /// because the user can't jump to them.
    pub(super) fn recompute_search_hits(&mut self) {
        let query = self
            .search_input
            .as_ref()
            .map(|i| i.value().to_string())
            .unwrap_or_default();

        let (scope_for_fields, global_ref, profile_ref) = self.field_build_inputs();

        let mut scored: Vec<(SearchHit, u32)> = Vec::new();
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
                // The category label is part of the title so "sandbox"
                // matches (and ranks) every field on the Sandbox tab.
                let title = format!("{} {}", category.label(), field.label);
                let full = format!("{} {}", title, field.description);
                let Some(score) = fuzzy_settings_score(&query, &title, &full) else {
                    continue;
                };
                scored.push((
                    SearchHit {
                        category,
                        field_ident: field.ident(),
                        field_label: field.label.clone(),
                        category_label: category.label(),
                        value_display: field.display_value(),
                    },
                    score,
                ));
            }
        }

        // Stable sort by score descending: ties (and the empty-query case where
        // every field scores 0) keep their natural (category, field) order.
        scored.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
        self.search_hits = scored.into_iter().map(|(hit, _)| hit).collect();
        if self.search_selected >= self.search_hits.len() {
            self.search_selected = self.search_hits.len().saturating_sub(1);
        }
    }

    /// Jump to the currently-selected search hit: switch to its
    /// category, rebuild fields for the new category, position the
    /// field cursor on the matching key, and close the popup.
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
        // A hit inside the Plugins category belongs to one plugin's virtual
        // section: select that plugin's manager row first, then rebuild so
        // the master-detail filter keeps the target field.
        if self.current_category() == SettingsCategory::Plugins
            && self
                .plugin_manager
                .select_plugin_owning_ident(&hit.field_ident)
        {
            self.rebuild_fields();
        }
        if let Some(idx) = self
            .fields
            .iter()
            .position(|f| f.ident() == hit.field_ident)
        {
            self.selected_field = idx;
            self.ensure_field_visible(self.fields_viewport_height);
        }
        self.focus = SettingsFocus::Fields;
        // A hit inside the Plugins category targets a plugin settings field,
        // not the manager pane above it: give the field list the sub-focus so
        // the jump lands on an editable row.
        self.plugins_fields_focus = self.current_category() == SettingsCategory::Plugins;
        self.close_search();
    }
}

/// Ids whose `enabled` flag differs between two serialized `config.plugins`
/// subtrees, with the flag's new value. An absent entry (or an id missing
/// entirely) counts as enabled, the config default. Drives the best-effort
/// daemon worker nudge after a settings save, which writes `config.plugins`
/// wholesale rather than toggling one id at a time.
fn plugin_enabled_changes(
    before: Option<&serde_json::Value>,
    after: &Option<serde_json::Value>,
) -> Vec<(String, bool)> {
    fn enabled_map(value: Option<&serde_json::Value>) -> std::collections::BTreeMap<String, bool> {
        value
            .and_then(|v| v.as_object())
            .map(|map| {
                map.iter()
                    .map(|(id, cfg)| {
                        let enabled = cfg.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true);
                        (id.clone(), enabled)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    let old = enabled_map(before);
    let new = enabled_map(after.as_ref());
    let mut changes = Vec::new();
    for (id, enabled) in &new {
        if old.get(id).copied().unwrap_or(true) != *enabled {
            changes.push((id.clone(), *enabled));
        }
    }
    // An id dropped from the map reverts to the default (enabled).
    for (id, was_enabled) in &old {
        if !new.contains_key(id) && !*was_enabled {
            changes.push((id.clone(), true));
        }
    }
    changes
}

#[cfg(test)]
mod plugin_enabled_changes_tests {
    use super::plugin_enabled_changes;
    use serde_json::json;

    #[test]
    fn detects_toggles_and_ignores_unchanged() {
        let before = json!({
            "a": { "enabled": true },
            "b": { "enabled": false },
            "c": { "enabled": true, "settings": { "k": 1 } },
        });
        let after = Some(json!({
            "a": { "enabled": false },
            "b": { "enabled": false },
            "c": { "enabled": true, "settings": { "k": 2 } },
        }));
        let changes = plugin_enabled_changes(Some(&before), &after);
        assert_eq!(changes, vec![("a".to_string(), false)]);
    }

    #[test]
    fn absent_entry_counts_as_enabled() {
        // A new id appearing as disabled is a change; one appearing enabled
        // is not (enabled is the default for unknown ids).
        let after = Some(json!({
            "fresh-off": { "enabled": false },
            "fresh-on": { "enabled": true },
        }));
        let changes = plugin_enabled_changes(None, &after);
        assert_eq!(changes, vec![("fresh-off".to_string(), false)]);
    }

    #[test]
    fn dropped_disabled_entry_reverts_to_enabled() {
        let before = json!({ "gone": { "enabled": false } });
        let after = Some(json!({}));
        let changes = plugin_enabled_changes(Some(&before), &after);
        assert_eq!(changes, vec![("gone".to_string(), true)]);
    }
}

#[cfg(test)]
pub(super) mod test_util {
    use super::SettingsView;
    use crate::session::test_support::{isolate_app_dir_at, AppDirGuard};
    use crate::session::Storage;
    use tempfile::TempDir;

    /// A `SettingsView` against an isolated app dir, shared by the
    /// input and render test modules. Keep both guards alive for the
    /// test body: the env is restored when `AppDirGuard` drops, before
    /// the `TempDir` deletes itself.
    pub fn fresh_view() -> (TempDir, AppDirGuard, SettingsView) {
        let temp = TempDir::new().unwrap();
        let guard = isolate_app_dir_at(temp.path());
        let _ = Storage::new_unwatched("test").unwrap();
        let view = SettingsView::new("test", None).unwrap();
        (temp, guard, view)
    }
}

#[cfg(test)]
mod dirty_tracking_tests {
    use super::*;
    use crate::session::Storage;
    use serial_test::serial;
    use tempfile::TempDir;

    /// Returns the `HomeGuard` first so it drops before the `TempDir`:
    /// the env is restored before the tempdir is deleted, and the guard
    /// holds the process-global env lock for the whole test body. The old
    /// bare `set_var` never restored HOME, leaking a since-deleted tempdir
    /// HOME into later tests (the #2600 failure mode).
    fn fresh_view() -> (
        crate::session::test_support::HomeGuard,
        TempDir,
        SettingsView,
    ) {
        let temp = TempDir::new().unwrap();
        let home = crate::session::test_support::isolate_home(temp.path());
        let _ = Storage::new_unwatched("test").unwrap();
        let view = SettingsView::new("test", None).unwrap();
        (home, temp, view)
    }

    /// Editing a setting and then reverting it to the saved value must not
    /// leave the view reporting unsaved changes (issue #2083). The flag is
    /// diff-based, not a one-way latch.
    #[test]
    #[serial]
    fn reverting_an_edit_clears_unsaved_changes() {
        let (_home, _temp, mut view) = fresh_view();
        assert!(!view.has_changes, "a freshly loaded view is clean");

        let original = view.global_config.default_profile.clone();

        view.global_config.default_profile = format!("{original}-edited");
        view.recompute_dirty();
        assert!(view.has_changes, "an edit marks unsaved changes");

        view.global_config.default_profile = original;
        view.recompute_dirty();
        assert!(
            !view.has_changes,
            "reverting the edit should clear unsaved changes"
        );
    }

    /// Saving adopts the live config as the new baseline, so an edit that
    /// matches a previously-saved value is correctly seen as a change again.
    #[test]
    #[serial]
    fn save_resets_the_baseline() {
        let (_home, _temp, mut view) = fresh_view();
        view.scope = SettingsScope::Profile;

        view.profile_config.description = Some("from-save".to_string());
        view.recompute_dirty();
        assert!(view.has_changes, "the edit is pending before save");

        view.save().unwrap();
        assert!(!view.has_changes, "saving clears the flag");

        // Reverting to the pre-save value is now itself a change to save.
        view.profile_config.description = None;
        view.recompute_dirty();
        assert!(
            view.has_changes,
            "the post-save baseline tracks the saved value"
        );
    }

    /// The clobber this PR exists to kill, at the Settings pane. A global
    /// field written by another process while the pane sits open must survive
    /// the save. The old `*c = self.global_config.clone()` wrote the
    /// open-time snapshot verbatim and silently reverted it, the same way the
    /// removed `save_config` did.
    #[test]
    #[serial]
    fn global_save_preserves_concurrent_external_edit() {
        let (_home, _temp, mut view) = fresh_view();
        view.scope = SettingsScope::Global;

        // The user edits one field in the pane.
        view.global_config.default_profile = "edited-by-user".to_string();
        view.recompute_dirty();

        // Meanwhile a peer process writes an unrelated global field straight
        // to disk, after this view took its baseline snapshot.
        crate::session::config::update_config(|c| {
            c.session.confirm_delete = true;
        })
        .unwrap();

        view.save().unwrap();

        let on_disk = Config::load().unwrap();
        assert_eq!(
            on_disk.default_profile, "edited-by-user",
            "the field the user edited must be applied"
        );
        assert!(
            on_disk.session.confirm_delete,
            "a peer's concurrent edit to a field the user never touched must survive the save"
        );
    }

    /// A save that changes nothing must not write the snapshot over a peer's
    /// concurrent edits either.
    #[test]
    #[serial]
    fn global_save_with_no_edits_preserves_concurrent_external_edit() {
        let (_home, _temp, mut view) = fresh_view();
        view.scope = SettingsScope::Global;

        crate::session::config::update_config(|c| {
            c.session.confirm_delete = true;
        })
        .unwrap();

        view.save().unwrap();

        assert!(
            Config::load().unwrap().session.confirm_delete,
            "an edit-free save must not revert a peer's write"
        );
    }

    /// A lifecycle operation resync must keep unsaved staged edits: the
    /// staged diff is re-applied per user-editable field on top of the disk
    /// state, while a lifecycle-owned field (the grant) always takes the disk
    /// value, even on a plugin the user also staged an edit for.
    #[test]
    #[serial]
    fn resync_after_plugin_mutation_preserves_staged_edits() {
        let (_home, _temp, mut view) = fresh_view();
        view.scope = SettingsScope::Global;

        // The user stages (unsaved): disable plugin "a".
        view.global_config
            .plugins
            .entry("a".to_string())
            .or_default()
            .enabled = false;
        view.recompute_dirty();
        assert!(view.has_changes);

        // A lifecycle operation rewrites plugin config on disk: grants "a"
        // and installs "b".
        crate::session::config::update_config(|c| {
            let a = c.plugins.entry("a".to_string()).or_default();
            a.grant = Some(crate::session::CapabilityGrant {
                manifest_hash: "sha256:abc".to_string(),
                capabilities: vec!["net".to_string()],
                granted_at: chrono::Utc::now(),
            });
            c.plugins.entry("b".to_string()).or_default().enabled = true;
        })
        .unwrap();

        view.resync_after_plugin_mutation();

        let a = view.global_config.plugins.get("a").expect("a survives");
        assert!(!a.enabled, "the staged toggle must survive the resync");
        assert!(
            a.grant.is_some(),
            "the lifecycle-written grant must win over the staged copy"
        );
        assert!(
            view.global_config.plugins.contains_key("b"),
            "the disk-side install must appear in the staged view"
        );
        assert!(view.has_changes, "the staged toggle keeps the view dirty");
    }

    /// The Plugins tab is Global-only, so `]` from either of its sub-panes
    /// switches scope like on every other Global-only tab (Telemetry),
    /// falling back to the new scope's first tab, instead of the manager
    /// pane swallowing the key.
    #[test]
    #[serial]
    fn scope_keys_from_plugins_tab_switch_scope_in_both_sub_panes() {
        use crossterm::event::{KeyCode, KeyEvent};

        for fields_subfocus in [false, true] {
            let (_home, _temp, mut view) = fresh_view();
            view.scope = SettingsScope::Global;
            let plugins_idx = view
                .categories
                .iter()
                .position(|r| *r == CategoryRow::Tab(SettingsCategory::Plugins))
                .expect("Plugins tab exists in Global scope");
            view.selected_category = plugins_idx;
            view.rebuild_fields();
            view.focus = SettingsFocus::Fields;
            view.plugins_fields_focus = fields_subfocus;

            view.handle_key(KeyEvent::from(KeyCode::Char(']')));

            assert_eq!(
                view.scope,
                SettingsScope::Profile,
                "']' must switch scope with fields_subfocus={fields_subfocus}"
            );
            assert_ne!(
                view.current_category(),
                SettingsCategory::Plugins,
                "the Global-only Plugins tab falls back to another tab in Profile scope"
            );
        }
    }

    /// A staged entry for a plugin with no config row on disk (a first toggle
    /// for a builtin) survives a resync; it was never in the baseline, so no
    /// lifecycle operation can have removed it.
    #[test]
    #[serial]
    fn resync_keeps_staged_entry_for_plugin_absent_from_disk() {
        let (_home, _temp, mut view) = fresh_view();
        view.scope = SettingsScope::Global;
        view.global_config
            .plugins
            .entry("aoe.web".to_string())
            .or_default()
            .enabled = false;

        crate::session::config::update_config(|c| {
            c.plugins.entry("other".to_string()).or_default().enabled = false;
        })
        .unwrap();

        view.resync_after_plugin_mutation();

        assert!(
            !view
                .global_config
                .plugins
                .get("aoe.web")
                .expect("staged entry kept")
                .enabled,
            "a purely user-staged entry must survive the resync"
        );
    }
}

#[cfg(test)]
mod search_tests {
    use super::fuzzy_settings_score;

    const TITLE: &str = "Session Max Concurrent Workers";
    const FULL: &str = "Session Max Concurrent Workers How many agents run at once";

    /// An empty query scores every field 0 so the popup lists all of them.
    #[test]
    fn empty_query_matches_everything() {
        assert_eq!(fuzzy_settings_score("", TITLE, FULL), Some(0));
        assert_eq!(fuzzy_settings_score("   ", TITLE, FULL), Some(0));
    }

    /// The acronym story: "mcw" must fuzzy-match "Max Concurrent Workers",
    /// which the old substring search could not do.
    #[test]
    fn acronym_matches() {
        assert!(
            fuzzy_settings_score("mcw", TITLE, FULL).is_some(),
            "'mcw' should match Max Concurrent Workers"
        );
        assert!(
            fuzzy_settings_score(
                "mcw",
                "Appearance Theme",
                "Appearance Theme Dashboard looks"
            )
            .is_none(),
            "'mcw' should not match an unrelated field"
        );
    }

    /// Multi-token queries keep AND semantics: every whitespace token must
    /// match, so "max workers" still finds the field even out of order.
    #[test]
    fn multi_token_requires_all_tokens() {
        assert!(fuzzy_settings_score("max workers", TITLE, FULL).is_some());
        assert!(fuzzy_settings_score("workers max", TITLE, FULL).is_some());
        assert!(
            fuzzy_settings_score("max banana", TITLE, FULL).is_none(),
            "a token with no match drops the field"
        );
    }

    /// A title (category + label) match must outrank a match that only
    /// appears in the description, so "sandbox" surfaces the Sandbox
    /// tab's own settings before fields that mention it in prose.
    #[test]
    fn title_matches_outrank_description_matches() {
        let title_hit = fuzzy_settings_score(
            "sandbox",
            "Sandbox Default Image",
            "Sandbox Default Image Container image to use",
        )
        .expect("title should match");
        let desc_hit = fuzzy_settings_score(
            "sandbox",
            "Session Host Environment",
            "Session Host Environment For secrets use the sandbox environment instead",
        )
        .expect("description should match");
        assert!(
            title_hit > desc_hit,
            "title match ({title_hit}) must outrank description match ({desc_hit})"
        );
    }
}
