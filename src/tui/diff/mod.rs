//! Diff view - view changes against a base branch

mod input;
mod render;
mod split;

use std::collections::HashMap;
use std::path::PathBuf;

use crate::git::diff::{
    check_merge_base_status, compute_changed_files, compute_file_diff, get_default_base_ref,
    list_branches, DiffFile, FileDiff,
};
use crate::session::config::{load_config, save_config};
use crate::session::{load_profile_config, resolve_config_or_warn, save_profile_config, Config};
use crate::tui::dialogs::InfoDialog;

pub use input::DiffAction;

/// State for branch selection dialog
#[derive(Debug, Clone, Default)]
pub struct BranchSelectState {
    pub branches: Vec<String>,
    pub selected: usize,
}

/// The diff view state
pub struct DiffView {
    /// Path to the repository root
    pub(crate) repo_path: PathBuf,

    /// Session id this diff view belongs to. None when opened in a
    /// session-agnostic context (legacy `DiffView::new`); persistence
    /// of the per-session base-branch override is skipped in that case.
    pub(crate) session_id: Option<String>,

    /// Profile the session belongs to, used to look up `Storage` when
    /// persisting the base-branch override.
    pub(crate) profile: String,

    /// Base branch to compare against
    pub(crate) base_branch: String,

    /// List of changed files
    pub(crate) files: Vec<DiffFile>,

    /// Currently selected file index
    pub(crate) selected_file: usize,

    /// Cached file diffs
    pub(crate) diff_cache: HashMap<PathBuf, FileDiff>,

    /// Scroll offset for the diff content
    pub(crate) scroll_offset: u16,

    /// Number of visible lines (set during render)
    pub(crate) visible_lines: u16,

    /// Total lines in current diff
    pub(crate) total_lines: u16,

    /// Branch selection dialog state
    pub(crate) branch_select: Option<BranchSelectState>,

    /// Error message to display
    pub(crate) error_message: Option<String>,

    /// Success message to display
    pub(crate) success_message: Option<String>,

    /// Context lines for diff
    pub(crate) context_lines: usize,

    /// Render the selected file's diff side-by-side instead of unified.
    pub(crate) split_view: bool,

    /// Show help overlay
    pub(crate) show_help: bool,

    /// Width of the file list panel (resizable with h/l)
    pub(crate) file_list_width: u16,

    /// Warning dialog shown when merge-base can't be computed
    pub(crate) warning_dialog: Option<InfoDialog>,

    /// Override that has been persisted to disk but not yet propagated
    /// back to HomeView's in-memory `Instance.base_branch_override`.
    /// HomeView consumes this after each key event via
    /// `take_pending_override` and applies it to its cache; without
    /// this hand-off, HomeView's next `commit` would overwrite the
    /// disk value with its stale memory copy. See #1175.
    pub(crate) pending_override: Option<(String, Option<String>)>,

    /// Inner rect of the file-list panel, captured during `render`.
    /// Lets a click on a file row select it and a hover highlight it
    /// the same way `j`/`k` would.
    pub(crate) file_list_inner: ratatui::layout::Rect,
}

impl DiffView {
    /// Create a session-agnostic diff view. Selecting a different
    /// branch through the picker only mutates in-memory state. Callers
    /// that have a session id should use `new_for_session` so the
    /// override persists.
    pub fn new(repo_path: PathBuf) -> anyhow::Result<Self> {
        Self::new_for_session(repo_path, None, String::new(), None, None)
    }

    /// Create a diff view bound to a session. `base_override` (the
    /// session's persisted `base_branch_override`) wins over the
    /// worktree's recorded base branch (`worktree_base`), which wins
    /// over the profile default and auto-detection. Subsequent calls
    /// to `select_branch` persist the new ref back to the session
    /// record.
    pub fn new_for_session(
        repo_path: PathBuf,
        session_id: Option<String>,
        profile: String,
        base_override: Option<String>,
        worktree_base: Option<String>,
    ) -> anyhow::Result<Self> {
        // Use the profile-merged config so a per-profile Diff override (e.g.
        // split_view) is honored on open. The session-agnostic path (empty
        // profile) falls back to the global config.
        let config = if profile.is_empty() {
            Config::load_or_warn()
        } else {
            resolve_config_or_warn(&profile)
        };

        let base_branch = base_override
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .or_else(|| {
                worktree_base
                    .as_deref()
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string)
            })
            .or_else(|| config.diff.default_branch.clone())
            .or_else(|| get_default_base_ref(&repo_path).ok())
            .unwrap_or_else(|| "main".to_string());

        let context_lines = config.diff.context_lines;
        let split_view = config.diff.split_view;

        let warning_dialog = check_merge_base_status(&repo_path, &base_branch)
            .map(|msg| InfoDialog::new("Warning", &msg));

        let mut view = Self {
            repo_path,
            session_id,
            profile,
            base_branch,
            files: Vec::new(),
            selected_file: 0,
            diff_cache: HashMap::new(),
            scroll_offset: 0,
            visible_lines: 20,
            total_lines: 0,
            branch_select: None,
            error_message: None,
            success_message: None,
            context_lines,
            split_view,
            show_help: false,
            file_list_width: config.app_state.diff_file_list_width.unwrap_or(35),
            warning_dialog,
            pending_override: None,
            file_list_inner: ratatui::layout::Rect::default(),
        };

        view.refresh_files()?;
        Ok(view)
    }

    /// Refresh the list of changed files
    pub fn refresh_files(&mut self) -> anyhow::Result<()> {
        self.files = compute_changed_files(&self.repo_path, &self.base_branch)?;
        self.diff_cache.clear();
        if self.selected_file >= self.files.len() {
            self.selected_file = self.files.len().saturating_sub(1);
        }
        self.scroll_offset = 0;
        Ok(())
    }

    /// Get the currently selected file
    pub fn selected_file(&self) -> Option<&DiffFile> {
        self.files.get(self.selected_file)
    }

    /// Repo-relative path of the selected file as a string, if any.
    pub(crate) fn selected_path_string(&self) -> Option<String> {
        self.selected_file()
            .map(|f| f.path.to_string_lossy().to_string())
    }

    /// Copy the selected file's repo-relative path to the system clipboard and
    /// surface a confirmation in the footer.
    pub(crate) fn copy_selected_path(&mut self) {
        if let Some(path) = self.selected_path_string() {
            crate::tui::clipboard::copy_to_clipboard(&path);
            self.success_message = Some(format!("Copied {path}"));
        }
    }

    /// Get or compute the diff for the selected file
    pub fn get_current_diff(&mut self) -> Option<&FileDiff> {
        let file = self.files.get(self.selected_file)?;
        let path = file.path.clone();

        if !self.diff_cache.contains_key(&path) {
            match compute_file_diff(
                &self.repo_path,
                &path,
                &self.base_branch,
                self.context_lines,
            ) {
                Ok(diff) => {
                    self.diff_cache.insert(path.clone(), diff);
                }
                Err(e) => {
                    self.error_message = Some(format!("Failed to compute diff: {}", e));
                    return None;
                }
            }
        }

        self.diff_cache.get(&path)
    }

    /// Open the branch selection dialog
    pub fn open_branch_select(&mut self) {
        match list_branches(&self.repo_path) {
            Ok(branches) => {
                let selected = branches
                    .iter()
                    .position(|b| b == &self.base_branch)
                    .unwrap_or(0);
                self.branch_select = Some(BranchSelectState { branches, selected });
            }
            Err(e) => {
                self.error_message = Some(format!("Failed to list branches: {}", e));
            }
        }
    }

    /// Select a branch and refresh. When the view is bound to a
    /// session, the choice is persisted as `base_branch_override` on
    /// the session record so the next launch comes back to the same
    /// comparison. Persistence failures only surface as a soft error
    /// message; the in-memory switch still applies. See #970.
    pub fn select_branch(&mut self, branch: String) {
        self.base_branch = branch;
        self.branch_select = None;
        self.warning_dialog = check_merge_base_status(&self.repo_path, &self.base_branch)
            .map(|msg| InfoDialog::new("Warning", &msg));
        if let Err(e) = self.persist_base_override() {
            self.error_message = Some(format!("Failed to persist base branch: {e}"));
        }
        if let Err(e) = self.refresh_files() {
            self.error_message = Some(format!("Failed to refresh: {}", e));
        }
    }

    fn persist_base_override(&mut self) -> anyhow::Result<()> {
        let Some(session_id) = self.session_id.clone() else {
            return Ok(());
        };
        let storage = crate::session::Storage::new(&self.profile)?;
        let new_override = Some(self.base_branch.clone());
        let id_for_closure = session_id.clone();
        let new_override_for_closure = new_override.clone();
        storage.update(|instances, _groups| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id_for_closure) {
                inst.base_branch_override = new_override_for_closure;
            }
            Ok(())
        })?;
        self.pending_override = Some((session_id, new_override));
        Ok(())
    }

    /// Drain a pending base-branch override that was just persisted to
    /// disk. HomeView calls this after each key event so its in-memory
    /// `Instance.base_branch_override` stays consistent with disk;
    /// otherwise its next `commit` would overwrite the persisted value.
    pub fn take_pending_override(&mut self) -> Option<(String, Option<String>)> {
        self.pending_override.take()
    }

    /// Navigate to next file
    pub fn next_file(&mut self) {
        if self.selected_file < self.files.len().saturating_sub(1) {
            self.selected_file += 1;
            self.scroll_offset = 0;
        }
    }

    /// Navigate to previous file
    pub fn prev_file(&mut self) {
        if self.selected_file > 0 {
            self.selected_file -= 1;
            self.scroll_offset = 0;
        }
    }

    /// Scroll diff content down
    pub fn scroll_down(&mut self, amount: u16) {
        let max_scroll = self.total_lines.saturating_sub(self.visible_lines);
        self.scroll_offset = (self.scroll_offset + amount).min(max_scroll);
    }

    /// Scroll diff content up
    pub fn scroll_up(&mut self, amount: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
    }

    /// Page down in diff content
    pub fn page_down(&mut self) {
        self.scroll_down(self.visible_lines.saturating_sub(2));
    }

    /// Page up in diff content
    pub fn page_up(&mut self) {
        self.scroll_up(self.visible_lines.saturating_sub(2));
    }

    /// Half-page down in diff content
    pub fn half_page_down(&mut self) {
        self.scroll_down(self.visible_lines / 2);
    }

    /// Half-page up in diff content
    pub fn half_page_up(&mut self) {
        self.scroll_up(self.visible_lines / 2);
    }

    /// Shrink the file list panel
    pub fn shrink_file_list(&mut self) {
        self.file_list_width = self.file_list_width.saturating_sub(5).max(5);
        self.save_file_list_width();
    }

    /// Grow the file list panel
    pub fn grow_file_list(&mut self) {
        self.file_list_width = (self.file_list_width + 5).min(80);
        self.save_file_list_width();
    }

    fn save_file_list_width(&self) {
        if let Ok(mut config) = load_config().map(|c| c.unwrap_or_default()) {
            config.app_state.diff_file_list_width = Some(self.file_list_width);
            let _ = save_config(&config);
        }
    }

    /// Persist the current `split_view` choice so it survives restarts and
    /// stays in sync with the settings TUI. A profile-scoped session writes the
    /// choice to that profile's override; a session-agnostic view writes the
    /// global default.
    pub(crate) fn persist_split_view(&self) {
        if self.profile.is_empty() {
            if let Ok(mut config) = load_config().map(|c| c.unwrap_or_default()) {
                config.diff.split_view = self.split_view;
                if let Err(e) = save_config(&config) {
                    tracing::warn!("failed to persist diff split_view: {e}");
                }
            }
            return;
        }
        match load_profile_config(&self.profile) {
            Ok(mut profile_config) => {
                // Overrides are sparse JSON (#1692): set diff.split_view in the
                // generic override map rather than a typed DiffConfigOverride.
                let diff = profile_config
                    .overrides
                    .entry("diff".to_string())
                    .or_insert_with(|| serde_json::json!({}));
                if let Some(obj) = diff.as_object_mut() {
                    obj.insert("split_view".to_string(), serde_json::json!(self.split_view));
                }
                if let Err(e) = save_profile_config(&self.profile, &profile_config) {
                    tracing::warn!(
                        "failed to persist diff split_view for profile {}: {e}",
                        self.profile
                    );
                }
            }
            Err(e) => {
                tracing::warn!("failed to load profile config {}: {e}", self.profile);
            }
        }
    }

    /// Minimal DiffView for unit tests. Centralised so new fields only need
    /// a default value in one place.
    #[cfg(test)]
    pub(crate) fn test_default() -> Self {
        Self {
            repo_path: std::path::PathBuf::from("/tmp/fake"),
            session_id: None,
            profile: String::new(),
            base_branch: "main".to_string(),
            files: Vec::new(),
            selected_file: 0,
            diff_cache: HashMap::new(),
            scroll_offset: 0,
            visible_lines: 20,
            total_lines: 0,
            branch_select: None,
            error_message: None,
            success_message: None,
            context_lines: 3,
            split_view: false,
            show_help: false,
            file_list_width: 35,
            warning_dialog: None,
            pending_override: None,
            file_list_inner: ratatui::layout::Rect::default(),
        }
    }
}
