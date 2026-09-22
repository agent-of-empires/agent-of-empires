//! Settings view - configuration management UI

mod fields;
mod input;
mod render;

use tui_input::Input;

use crate::session::{
    list_profiles_for_display, load_profile_config, load_repo_config, merge_configs,
    profile_to_repo_config, repo_config_to_profile, save_profile_config, save_repo_config,
    sort_profiles_for_display, update_app_state, update_config, Config, ProfileConfig, RepoConfig,
};
use crate::tui::dialogs::CustomInstructionDialog;

pub use fields::{FieldValue, HookField, SettingField, SettingsCategory};
pub use input::SettingsAction;

/// How long the "Settings saved" toast lingers before it auto-dismisses.
const SUCCESS_MESSAGE_TTL: std::time::Duration = std::time::Duration::from_secs(10);

/// Serialize a config to JSON for change detection, so no nested config type
/// needs `PartialEq`. A failure degrades to `Null` so two failures compare equal.
fn config_to_json<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

/// Bonus for matching a hit's title rather than only its description, sized to
/// dominate any per-token nucleo score so title matches always rank first.
const TITLE_MATCH_BONUS: u32 = 100_000;

/// Fuzzy-score a field against a settings-search query: every whitespace token
/// must match `title` or `full`, scores are summed, and a title match earns
/// [`TITLE_MATCH_BONUS`]. An empty query scores 0, listing fields in order.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsScope {
    #[default]
    Global,
    Profile,
    Repo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsFocus {
    #[default]
    Categories,
    Fields,
}

#[derive(Debug, Clone, Default)]
pub struct ListEditState {
    pub selected_index: usize,
    pub editing_item: Option<Input>,
    pub adding_new: bool,
}

/// A field that matched the settings-search query, plus where it lives.
#[derive(Debug, Clone)]
pub(super) struct SearchHit {
    pub category: SettingsCategory,
    /// Stable identity used to relocate the cursor on jump, since fields are
    /// rebuilt from the schema per category.
    pub field_ident: String,
    pub field_label: String,
    pub category_label: &'static str,
    /// Value snapshotted when the hit list was built. Safe because editing is
    /// frozen while the popup is open.
    pub value_display: String,
}

/// A row in the categories panel. Sections are non-interactive dividers that
/// navigation skips; `selected_category` is always the index of a `Tab` row.
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

pub struct SettingsView {
    pub(super) profile: String,

    pub(super) available_profiles: Vec<String>,

    pub(super) project_path: Option<String>,

    pub(super) repo_config: Option<RepoConfig>,

    /// Repo config as overrides relative to `resolved_base`.
    pub(super) repo_as_profile: ProfileConfig,

    /// Global + profile merged, the base that Repo scope overrides.
    pub(super) resolved_base: Config,

    pub(super) scope: SettingsScope,

    pub(super) focus: SettingsFocus,

    pub(super) categories: Vec<CategoryRow>,

    /// Always the index of a `Tab` row; navigation helpers keep it so.
    pub(super) selected_category: usize,

    pub(super) fields: Vec<SettingField>,

    pub(super) selected_field: usize,

    pub(super) global_config: Config,

    pub(super) profile_config: ProfileConfig,

    pub(super) editing_input: Option<Input>,

    pub(super) list_edit_state: Option<ListEditState>,

    pub(super) custom_instruction_dialog: Option<CustomInstructionDialog>,

    pub(super) fields_scroll_offset: u16,

    pub(super) fields_viewport_height: u16,

    /// Content width captured during render, so scroll math outside the render
    /// pass wraps descriptions the way the next frame will paint them.
    pub(super) fields_content_width: u16,

    /// Recomputed against `baseline_*`, so reverting an edit clears it.
    pub(super) has_changes: bool,

    /// The editable configs as of the last load or save.
    pub(super) baseline_global: serde_json::Value,
    pub(super) baseline_profile: serde_json::Value,
    pub(super) baseline_repo: serde_json::Value,

    pub(super) show_help: bool,

    pub(super) error_message: Option<String>,

    pub(super) success_message: Option<String>,

    /// When the success toast auto-dismisses. Errors are sticky and have none.
    pub(super) success_message_expires_at: Option<std::time::Instant>,

    /// `Some` while search is active: keys route to the query and hit list
    /// until Enter jumps or Esc closes. `None` is the idle bar.
    pub(super) search_input: Option<Input>,

    /// Hits for the current query; an empty query lists every field.
    pub(super) search_hits: Vec<SearchHit>,

    pub(super) search_selected: usize,

    /// Screen row and `search_hits` index per visible hit, captured by the
    /// popup render so click and hover need no scroll math.
    pub(super) search_hit_rows: Vec<(u16, usize)>,

    /// Popup frame, so a click inside it that misses a row is a no-op rather
    /// than a dismiss.
    pub(super) search_popup_area: ratatui::layout::Rect,

    /// Search bar, so clicking the idle bar opens search like `/`.
    pub(super) search_bar_rect: ratatui::layout::Rect,

    /// Hit rect per scope tab, so a click can switch scope.
    pub(super) scope_tab_rects: Vec<(SettingsScope, ratatui::layout::Rect)>,
    /// Hit rect per `categories` Tab row; Section dividers are skipped.
    pub(super) category_rects: Vec<(usize, ratatui::layout::Rect)>,
    /// Hit rect per visible `fields` row, empty while editing so a stray click
    /// during composition cannot reset focus.
    pub(super) field_rects: Vec<(usize, ratatui::layout::Rect)>,
    /// Fields-panel scrollbar. Zero-area when content fits, so nothing to grab.
    pub(super) scrollbar_area: ratatui::layout::Rect,
    /// Last hovered cell, kept apart from `selected_*` so the mouse never
    /// disturbs the keyboard cursor. Cleared on every keypress.
    pub(super) mouse_pos: Option<(u16, u16)>,

    /// The command palette's plugin manager, hosted inline in the Plugins
    /// category.
    pub(super) plugin_manager: crate::tui::dialogs::PluginManagerDialog,

    /// Plugins right pane sub-focus: `true` targets the settings fields below
    /// the manager. Tab toggles; reset when the field list rebuilds.
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

        // The profile-scope cycler is a picker, so `default` sorts last.
        let mut available_profiles = match list_profiles_for_display() {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(target: "tui.settings", "Failed to list profiles: {e}");
                Vec::new()
            }
        };
        if !available_profiles.contains(&profile.to_string()) {
            available_profiles.push(profile.to_string());
            sort_profiles_for_display(&mut available_profiles);
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
            scrollbar_area: ratatui::layout::Rect::default(),
            mouse_pos: None,
            plugin_manager: crate::tui::dialogs::PluginManagerDialog::embedded(),
            plugins_fields_focus: false,
        };

        // 0 is the leading section divider; land on the first real Tab.
        view.selected_category = view.first_tab_index();
        view.rebuild_fields();
        Ok(view)
    }

    /// Categories grouped under section dividers. Status Hooks, Tmux and Sound
    /// are dropped in Repo scope: `REPO_OVERRIDABLE_SECTIONS` excludes them, so
    /// a repo edit there would strand at save.
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
        if scope != SettingsScope::Repo {
            push_tab(&mut rows, SettingsCategory::Tmux);
        }

        push_section(&mut rows, "Notifications");
        if scope != SettingsScope::Repo {
            push_tab(&mut rows, SettingsCategory::Sound);
        }
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
                    .and_then(crate::session::config::settings_schema::section_plugin_id)
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

    /// Total height of all field rows plus inter-row spacing, in the same
    /// content-space units the render pass and `ensure_field_visible` use.
    /// Depends on `fields_content_width` (set at render) for description
    /// wrapping, so it matches what the next frame paints.
    fn fields_content_height(&self) -> u16 {
        let mut total = 0u16;
        for (i, field) in self.fields.iter().enumerate() {
            if i > 0 {
                total += 1; // spacing between fields
            }
            total += self.field_height(field, i);
        }
        total
    }

    /// Largest valid `fields_scroll_offset`: everything past this would
    /// scroll blank space below the last field into view.
    fn max_fields_scroll(&self) -> u16 {
        self.fields_content_height()
            .saturating_sub(self.fields_viewport_height)
    }

    /// Move the fields viewport by the wheel. `up` scrolls toward the top.
    /// When the search popup is open the wheel drives its ranked-hit
    /// cursor instead, matching the Up/Down keys. Returns true when
    /// something changed so the caller can redraw.
    ///
    /// The wheel scrolls the fields panel wherever the cursor sits in the
    /// takeover, including over the categories panel: that panel is a
    /// short List with no scroll offset of its own, so there is nothing
    /// else a wheel there could reasonably move.
    pub fn handle_wheel_scroll(&mut self, up: bool) -> bool {
        // One content line per wheel notch: field rows have varying
        // heights (label + wrapped description + spacing), so a bigger
        // step lands mid-row and reads as jumpy. Line granularity keeps
        // the panel gliding.
        const STEP: u16 = 1;
        if self.search_input.is_some() {
            if up {
                if self.search_selected > 0 {
                    self.search_selected -= 1;
                    return true;
                }
            } else if self.search_selected + 1 < self.search_hits.len() {
                self.search_selected += 1;
                return true;
            }
            return false;
        }
        let next = if up {
            self.fields_scroll_offset.saturating_sub(STEP)
        } else {
            self.fields_scroll_offset
                .saturating_add(STEP)
                .min(self.max_fields_scroll())
        };
        if next == self.fields_scroll_offset {
            return false;
        }
        self.fields_scroll_offset = next;
        true
    }

    /// Whether `(col, row)` lands on the fields-panel scrollbar. False
    /// when nothing overflows (the bar isn't drawn, so its rect is empty).
    /// The grab zone is widened one column left of the 1-cell bar; that
    /// column is block padding (empty), so it's a free hit that makes the
    /// bar easier to grab without stealing clicks from the field controls
    /// further left.
    pub fn hit_scrollbar(&self, col: u16, row: u16) -> bool {
        let bar = self.scrollbar_area;
        if bar.width == 0 {
            return false;
        }
        let left = bar.x.saturating_sub(1);
        col >= left && col <= bar.x && row >= bar.y && row < bar.y.saturating_add(bar.height)
    }

    /// Map a screen `row` on the scrollbar track to a scroll offset and
    /// apply it, so a grab-drag on the bar moves the viewport. The top of
    /// the track is offset 0; the bottom is `max_fields_scroll`. Returns
    /// true when the offset changed.
    pub fn scrollbar_drag_to_row(&mut self, row: u16) -> bool {
        let bar = self.scrollbar_area;
        let max = self.max_fields_scroll();
        if bar.height == 0 || max == 0 {
            return false;
        }
        let rel = row.saturating_sub(bar.y).min(bar.height.saturating_sub(1));
        let denom = bar.height.saturating_sub(1).max(1) as u32;
        let offset = ((rel as u32 * max as u32) / denom).min(max as u32) as u16;
        if offset == self.fields_scroll_offset {
            return false;
        }
        self.fields_scroll_offset = offset;
        true
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
                    crate::session::config::settings_schema::apply_changed_leaves(
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
    fn reports_only_the_toggles() {
        // (before, after, expected changes). An id absent from `before`
        // counts as enabled, so only a fresh disable is a change; an id
        // dropped from `after` reverts to enabled.
        let cases: &[(
            Option<serde_json::Value>,
            serde_json::Value,
            Vec<(&str, bool)>,
        )] = &[
            (
                Some(json!({
                    "a": { "enabled": true },
                    "b": { "enabled": false },
                    "c": { "enabled": true, "settings": { "k": 1 } },
                })),
                json!({
                    "a": { "enabled": false },
                    "b": { "enabled": false },
                    "c": { "enabled": true, "settings": { "k": 2 } },
                }),
                vec![("a", false)],
            ),
            (
                None,
                json!({ "fresh-off": { "enabled": false }, "fresh-on": { "enabled": true } }),
                vec![("fresh-off", false)],
            ),
            (
                Some(json!({ "gone": { "enabled": false } })),
                json!({}),
                vec![("gone", true)],
            ),
        ];
        for (before, after, want) in cases {
            let want: Vec<(String, bool)> =
                want.iter().map(|(id, on)| (id.to_string(), *on)).collect();
            assert_eq!(
                plugin_enabled_changes(before.as_ref(), &Some(after.clone())),
                want
            );
        }
    }
}

#[cfg(test)]
mod categories_for_scope_tests {
    use super::{CategoryRow, SettingsCategory, SettingsScope, SettingsView};

    /// StatusHooks, Tmux and Sound are gated off Repo scope: their sections
    /// are not repo-overridable, so a Repo tab would strand edits at save.
    #[test]
    fn repo_scope_drops_non_repo_overridable_categories() {
        let has_tab = |rows: &[CategoryRow], cat| rows.iter().any(|r| r.as_tab() == Some(cat));
        let repo = SettingsView::categories_for_scope(SettingsScope::Repo);
        let global = SettingsView::categories_for_scope(SettingsScope::Global);
        for cat in [
            SettingsCategory::StatusHooks,
            SettingsCategory::Tmux,
            SettingsCategory::Sound,
        ] {
            assert!(!has_tab(&repo, cat), "{cat:?} must be absent under Repo");
            assert!(
                has_tab(&global, cat),
                "{cat:?} must be present under Global"
            );
        }
        assert!(
            has_tab(&repo, SettingsCategory::Sandbox),
            "repo-overridable"
        );
    }
}

#[cfg(test)]
pub(super) mod test_util {
    use super::SettingsView;
    use crate::session::test_support::{isolate_app_dir_at, AppDirGuard};
    use crate::session::Storage;
    use tempfile::TempDir;

    /// A `SettingsView` over an isolated app dir. Keep both guards alive for
    /// the test body: `AppDirGuard` restores the env before the `TempDir`
    /// deletes itself.
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

    /// The `HomeGuard` comes first so it drops before the `TempDir`, and it
    /// holds the process-global env lock for the whole body.
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

    /// The unsaved-changes flag is diff-based, not a one-way latch.
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

    /// Saving adopts the live config as the new baseline.
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

    /// A global field written by another process while the pane sits open
    /// must survive the save, rather than being reverted by the open-time
    /// snapshot.
    #[test]
    #[serial]
    fn global_save_preserves_concurrent_external_edit() {
        let (_home, _temp, mut view) = fresh_view();
        view.scope = SettingsScope::Global;

        view.global_config.default_profile = "edited-by-user".to_string();
        view.recompute_dirty();

        // A peer writes an unrelated global field after the baseline snapshot.
        crate::session::config::update_config(|c| {
            c.session.confirm_delete = false;
        })
        .unwrap();

        view.save().unwrap();

        let on_disk = Config::load().unwrap();
        assert_eq!(
            on_disk.default_profile, "edited-by-user",
            "the field the user edited must be applied"
        );
        assert!(
            !on_disk.session.confirm_delete,
            "a peer's concurrent edit to a field the user never touched must survive the save"
        );
    }

    #[test]
    #[serial]
    fn global_save_with_no_edits_preserves_concurrent_external_edit() {
        let (_home, _temp, mut view) = fresh_view();
        view.scope = SettingsScope::Global;

        crate::session::config::update_config(|c| {
            c.session.confirm_delete = false;
        })
        .unwrap();

        view.save().unwrap();

        assert!(
            !Config::load().unwrap().session.confirm_delete,
            "an edit-free save must not revert a peer's write"
        );
    }

    /// A resync re-applies the staged diff per user-editable field over the
    /// disk state, while a lifecycle-owned field takes the disk value.
    #[test]
    #[serial]
    fn resync_after_plugin_mutation_preserves_staged_edits() {
        let (_home, _temp, mut view) = fresh_view();
        view.scope = SettingsScope::Global;

        view.global_config
            .plugins
            .entry("a".to_string())
            .or_default()
            .enabled = false;
        view.recompute_dirty();
        assert!(view.has_changes);

        // A lifecycle operation grants "a" and installs "b" on disk.
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

    /// The Plugins tab is Global-only, so `]` from either sub-pane switches
    /// scope and falls back to the new scope's first tab.
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

    /// A staged entry that was never in the baseline survives a resync: no
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

    #[test]
    fn every_token_must_match_and_a_title_hit_outranks_a_description_hit() {
        // An empty query scores 0, so the popup lists every field.
        for query in ["", "   "] {
            assert_eq!(fuzzy_settings_score(query, TITLE, FULL), Some(0));
        }

        // Fuzzy matching covers acronyms, and multi-token queries keep AND
        // semantics in any order.
        for query in ["mcw", "max workers", "workers max"] {
            assert!(
                fuzzy_settings_score(query, TITLE, FULL).is_some(),
                "{query}"
            );
        }
        assert!(
            fuzzy_settings_score("max banana", TITLE, FULL).is_none(),
            "a token with no match drops the field"
        );
        assert!(fuzzy_settings_score(
            "mcw",
            "Appearance Theme",
            "Appearance Theme Dashboard looks"
        )
        .is_none());

        // So "sandbox" surfaces the Sandbox tab's own settings ahead of the
        // fields that only mention it in prose.
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

#[cfg(test)]
mod scroll_tests {
    use super::test_util::fresh_view;
    use ratatui::layout::Rect;
    use serial_test::serial;

    /// Overflow the fields panel so the scroll math has room to move.
    fn make_overflowing(view: &mut super::SettingsView) {
        assert!(
            view.fields.len() > 1,
            "the default category should load several fields"
        );
        view.fields_content_width = 60;
        view.fields_viewport_height = 3;
        view.fields_scroll_offset = 0;
        assert!(
            view.max_fields_scroll() > 0,
            "test setup must produce an overflowing panel"
        );
    }

    /// A wheel advances the fields offset either way, clamped to the panel.
    #[test]
    #[serial]
    fn wheel_scrolls_fields_panel_with_clamping() {
        let (_t, _guard, mut view) = fresh_view();
        make_overflowing(&mut view);
        let max = view.max_fields_scroll();

        assert!(view.handle_wheel_scroll(false), "wheel-down must move");
        assert_eq!(
            view.fields_scroll_offset,
            1.min(max),
            "one wheel notch scrolls one line"
        );

        for _ in 0..50 {
            view.handle_wheel_scroll(false);
        }
        assert_eq!(view.fields_scroll_offset, max, "must clamp at the bottom");
        assert!(
            !view.handle_wheel_scroll(false),
            "a wheel at the bottom is a no-op"
        );

        for _ in 0..50 {
            view.handle_wheel_scroll(true);
        }
        assert_eq!(view.fields_scroll_offset, 0, "must clamp at the top");
        assert!(
            !view.handle_wheel_scroll(true),
            "a wheel at the top is a no-op"
        );
    }

    /// The hit test covers the bar plus the padding column to its left.
    #[test]
    #[serial]
    fn hit_scrollbar_covers_the_bar_and_its_padding_column() {
        let (_t, _guard, mut view) = fresh_view();
        view.scrollbar_area = Rect::new(70, 3, 1, 10);

        assert!(view.hit_scrollbar(70, 5), "on the bar");
        assert!(view.hit_scrollbar(69, 5), "padding column left of the bar");
        assert!(!view.hit_scrollbar(68, 5), "two columns left is a miss");
        assert!(!view.hit_scrollbar(71, 5), "right of the bar is a miss");
        assert!(!view.hit_scrollbar(70, 2), "above the track is a miss");
        assert!(!view.hit_scrollbar(70, 13), "below the track is a miss");

        view.scrollbar_area = Rect::default();
        assert!(!view.hit_scrollbar(70, 5), "no bar => no hit");
    }

    /// Dragging the thumb pins the offset to either end of the track.
    #[test]
    #[serial]
    fn scrollbar_drag_maps_row_to_offset() {
        let (_t, _guard, mut view) = fresh_view();
        make_overflowing(&mut view);
        let max = view.max_fields_scroll();
        view.scrollbar_area = Rect::new(70, 3, 1, 10); // rows 3..=12

        assert!(view.scrollbar_drag_to_row(12), "drag to the bottom moves");
        assert_eq!(view.fields_scroll_offset, max, "bottom of track => max");

        assert!(view.scrollbar_drag_to_row(3), "drag to the top moves");
        assert_eq!(view.fields_scroll_offset, 0, "top of track => 0");

        view.scrollbar_drag_to_row(99);
        assert_eq!(view.fields_scroll_offset, max, "past-bottom clamps to max");
    }

    /// While the popup is open the wheel drives the hit cursor, not the
    /// fields behind it.
    #[test]
    #[serial]
    fn wheel_moves_search_selection_when_popup_open() {
        let (_t, _guard, mut view) = fresh_view();
        view.open_search(); // empty query lists every interactive field
        assert!(
            view.search_hits.len() > 1,
            "an empty query should list many hits"
        );
        let baseline_offset = view.fields_scroll_offset;

        assert!(view.handle_wheel_scroll(false), "wheel-down moves a hit");
        assert_eq!(view.search_selected, 1);
        assert_eq!(
            view.fields_scroll_offset, baseline_offset,
            "the background fields must not scroll while the popup owns the wheel"
        );

        assert!(view.handle_wheel_scroll(true), "wheel-up moves a hit");
        assert_eq!(view.search_selected, 0);
        assert!(
            !view.handle_wheel_scroll(true),
            "at the first hit a wheel-up is a no-op"
        );
    }
}
