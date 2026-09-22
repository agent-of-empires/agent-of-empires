//! New session dialog

mod group_input;
mod path_input;
mod render;

#[cfg(test)]
mod tests;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::Instant;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use super::DialogResult;
use crate::containers;
use crate::session::config::profile_config::resolve_config_or_warn;
use crate::session::config::repo_config::HookProgress;
use crate::session::config::{load_config, update_app_state, DefaultTerminalMode, SandboxConfig};
#[cfg(test)]
use crate::session::Config;
use crate::tmux::AvailableTools;
use crate::tui::components::{
    DirPicker, DirPickerResult, GroupGhostCompletion, ListPicker, ListPickerResult,
};
use path_input::PathGhostCompletion;

pub(super) struct FieldHelp {
    pub(super) name: &'static str,
    pub(super) description: &'static str,
}

pub(super) const HELP_DIALOG_WIDTH: u16 = 85;

/// Index of the Base field in the worktree config overlay; shared by the key
/// and mouse handlers.
const WT_BASE_BRANCH_FIELD: usize = 2;

pub(super) const FIELD_HELP: &[FieldHelp] = &[
    FieldHelp {
        name: "Scratch",
        description: "Ctrl+T from any field: run in a fresh scratch dir (no project path needed)",
    },
    FieldHelp {
        name: "Profile",
        description: "Settings profile for session defaults (Left/Right to cycle)",
    },
    FieldHelp {
        name: "Title",
        description: "Session name (auto-generates if empty)",
    },
    FieldHelp {
        name: "Path",
        description: "Working directory for the session",
    },
    FieldHelp {
        name: "Tool",
        description: "Which AI tool to use (Ctrl+P to configure command and extra args)",
    },
    FieldHelp {
        name: "Structured",
        description: "Render as a structured (ACP) transcript; runs under aoe serve",
    },
    FieldHelp {
        name: "YOLO Mode",
        description:
            "Skip permission prompts for autonomous operation (--dangerously-skip-permissions)",
    },
    FieldHelp {
        name: "Worktree",
        description:
            "Create a git worktree (Ctrl+P to configure name, branch mode, and extra repos)",
    },
    FieldHelp {
        name: "Sandbox",
        description: "Run session in a container for isolation (Ctrl+P to configure)",
    },
    FieldHelp {
        name: "Image",
        description: "Container image. Edit config.toml [sandbox] default_image to change default",
    },
    FieldHelp {
        name: "Environment",
        description: "Env vars: bare KEY passes host value, KEY=VALUE sets explicitly",
    },
    FieldHelp {
        name: "Group",
        description: "Optional grouping for organization (Ctrl+P to browse existing groups)",
    },
];

#[derive(Clone)]
pub struct NewSessionData {
    pub profile: String,
    pub title: String,
    pub path: String,
    pub group: String,
    pub tool: String,
    pub worktree_enabled: bool,
    pub worktree_branch: Option<String>,
    pub create_new_branch: bool,
    /// Base for the new worktree branch, only read when `create_new_branch`.
    /// `None` falls back to the repository's default branch.
    pub base_branch: Option<String>,
    pub extra_repo_paths: Vec<String>,
    pub sandbox: bool,
    pub sandbox_image: String,
    pub yolo_mode: bool,
    /// Container env: `KEY` passes through from the host, `KEY=VALUE` sets.
    pub extra_env: Vec<String>,
    pub extra_args: String,
    pub command_override: String,
    /// Provision a fresh `<app_dir>/scratch/<id>/` directory. Mutually
    /// exclusive with worktree mode.
    pub scratch: bool,
    pub fork_seed: Option<crate::session::ForkSeed>,
    /// Create in the structured (ACP) view instead of a tmux terminal. Only
    /// true for ACP-capable tools; `validate_structured_choice` enforces it.
    pub structured: bool,
}

/// The one conversion from wizard output to builder input, so a new field
/// cannot be forwarded on `create_session` and dropped in `CreationPoller`.
/// `profile` is not carried: builders take it as a separate argument. Nor is
/// `structured`: both paths read it off `NewSessionData` and apply it after
/// the build via `builder::structured::apply_structured_choice`.
impl From<NewSessionData> for crate::session::builder::InstanceParams {
    fn from(data: NewSessionData) -> Self {
        Self {
            title: data.title,
            path: data.path,
            group: data.group,
            tool: data.tool,
            worktree_enabled: data.worktree_enabled,
            worktree_branch: data.worktree_branch,
            create_new_branch: data.create_new_branch,
            base_branch: data.base_branch,
            sandbox: data.sandbox,
            sandbox_image: data.sandbox_image,
            yolo_mode: data.yolo_mode,
            extra_env: data.extra_env,
            extra_args: data.extra_args,
            command_override: data.command_override,
            extra_repo_paths: data.extra_repo_paths,
            // The dialog collects one base for the whole session; per-repo
            // bases are a CLI and web-wizard input.
            repo_base_branches: Vec::new(),
            scratch: data.scratch,
            fork_seed: data.fork_seed,
        }
    }
}

pub struct NewSessionDialog {
    pub(super) profile: String,
    pub(super) available_profiles: Vec<String>,
    /// Short descriptions in lockstep with `available_profiles`, shown as
    /// helper text under the profile name.
    pub(super) profile_descriptions: Vec<Option<String>>,
    pub(super) profile_index: usize,
    pub(super) title: Input,
    pub(super) path: Input,
    pub(super) group: Input,
    pub(super) tool_index: usize,
    pub(super) focused_field: usize,
    pub(super) available_tools: Vec<String>,
    pub(super) worktree_enabled: bool,
    /// Set by a direct worktree toggle, so a later path pick keeps the user's choice.
    pub(super) worktree_dirty: bool,
    pub(super) worktree_branch: Input,
    pub(super) create_new_branch: bool,
    /// Base branch input in the worktree config overlay; empty means the
    /// repo default.
    pub(super) base_branch: Input,
    pub(super) sandbox_enabled: bool,
    pub(super) sandbox_image: Input,
    pub(super) docker_available: bool,
    pub(super) yolo_mode: bool,
    pub(super) yolo_mode_default: bool,
    pub(super) structured_enabled: bool,
    /// Configured opening state (`acp.default_new_session_view`), re-applied
    /// when a tool change makes the structured view available again.
    pub(super) structured_default: bool,
    /// The user's own toggle choice, restored instead of the configured
    /// default after passing through a tool that cannot back a structured session.
    pub(super) structured_choice: Option<bool>,
    /// Whether the selected tool can back a structured session, recomputed on
    /// every tool or profile change. Gates the Structured field's visibility
    /// like `has_yolo` / `has_sandbox`.
    pub(super) structured_capable: bool,
    pub(super) workspace_repos: Vec<String>,
    pub(super) workspace_repos_expanded: bool,
    pub(super) workspace_repo_selected_index: usize,
    pub(super) workspace_repo_editing_input: Option<Input>,
    pub(super) workspace_repo_adding_new: bool,
    pub(super) workspace_repo_ghost: Option<path_input::PathGhostCompletion>,
    pub(super) workspace_repo_dir_picker_active: bool,
    pub(super) worktree_config_mode: bool,
    /// Focused field within the worktree config overlay (0=name, 1=new_branch, 2=extra_repos)
    pub(super) worktree_config_focused_field: usize,
    /// Session env: `KEY` passes through, `KEY=VALUE` sets.
    pub(super) extra_env: Vec<String>,
    /// Inherited env is shown for review but only submitted as a per-session
    /// override once the user edits the list.
    pub(super) extra_env_overridden: bool,
    pub(super) env_list_expanded: bool,
    pub(super) env_selected_index: usize,
    pub(super) env_editing_input: Option<Input>,
    pub(super) env_adding_new: bool,
    pub(super) inherited_settings: Vec<(String, String)>,
    pub(super) sandbox_config_mode: bool,
    pub(super) sandbox_focused_field: usize,
    pub(super) tool_config_mode: bool,
    pub(super) tool_config_focused_field: usize,
    pub(super) extra_args: Input,
    pub(super) command_override: Input,
    pub(super) existing_groups: Vec<String>,
    pub(super) group_picker: ListPicker,
    pub(super) branch_picker: ListPicker,
    /// Registered-project picker, opened from the workspace repos list with
    /// Ctrl+R; a selection appends its path.
    pub(super) projects_picker: ListPicker,
    /// Projects as of picker activation, parallel to its display list, to map
    /// the chosen name back to a path.
    pub(super) available_projects: Vec<crate::session::Project>,
    pub(super) dir_picker: DirPicker,
    pub(super) error_message: Option<String>,
    pub(super) show_help: bool,
    pub(super) loading: bool,
    pub(super) has_hooks: bool,
    pub(super) current_hook: Option<String>,
    pub(super) hook_output: Vec<String>,
    pub(super) path_invalid_flash_until: Option<Instant>,
    /// Ghost text completion for the path field (fish-shell style).
    path_ghost: Option<PathGhostCompletion>,
    /// Ghost text completion for the group field (fish-shell style).
    group_ghost: Option<GroupGhostCompletion>,
    /// Inline confirm for creating a missing directory; the bool is the
    /// Yes/No selection.
    pub(super) confirm_create_dir: Option<bool>,
    /// Skips the path canonicalize/exists checks on submit: the server
    /// provisions the scratch directory. Mutually exclusive with worktree mode.
    pub(super) scratch: bool,
    pub(super) fork_seed: Option<crate::session::ForkSeed>,
    /// `(focused_field_index, rect)` per main-form field, repopulated every
    /// frame and empty while an overlay is up, so a click during one cannot
    /// snap focus to the field that used to sit there.
    pub(super) focusable_rects: Vec<(usize, ratatui::layout::Rect)>,
    /// Rects keyed by `sandbox_focused_field`, only while that overlay is up.
    pub(super) sandbox_config_rects: Vec<(usize, ratatui::layout::Rect)>,
    /// Rects keyed by `tool_config_focused_field`.
    pub(super) tool_config_rects: Vec<(usize, ratatui::layout::Rect)>,
    /// Rects keyed by `worktree_config_focused_field`.
    pub(super) worktree_config_rects: Vec<(usize, ratatui::layout::Rect)>,
}

/// Key handling shared by the editable lists.
fn handle_editable_list_key(
    key: KeyEvent,
    items: &mut Vec<String>,
    expanded: &mut bool,
    selected_index: &mut usize,
    editing_input: &mut Option<Input>,
    adding_new: &mut bool,
    validate: impl Fn(&str, &[String]) -> bool,
) -> DialogResult<NewSessionData> {
    if let Some(ref mut input) = editing_input {
        match key.code {
            KeyCode::Enter => {
                let value = input.value().trim().to_string();
                if validate(&value, items) {
                    if *adding_new {
                        items.push(value);
                        *selected_index = items.len().saturating_sub(1);
                    } else if *selected_index < items.len() {
                        items[*selected_index] = value;
                    }
                }
                *editing_input = None;
                *adding_new = false;
                return DialogResult::Continue;
            }
            KeyCode::Esc => {
                *editing_input = None;
                *adding_new = false;
                return DialogResult::Continue;
            }
            _ => {
                input.handle_event(&crossterm::event::Event::Key(key));
                return DialogResult::Continue;
            }
        }
    }

    match key.code {
        KeyCode::Esc => {
            *expanded = false;
            DialogResult::Continue
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if *selected_index > 0 {
                *selected_index -= 1;
            }
            DialogResult::Continue
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if *selected_index < items.len().saturating_sub(1) {
                *selected_index += 1;
            }
            DialogResult::Continue
        }
        KeyCode::Char('a') => {
            *editing_input = Some(Input::default());
            *adding_new = true;
            DialogResult::Continue
        }
        KeyCode::Char('d') => {
            if !items.is_empty() && *selected_index < items.len() {
                items.remove(*selected_index);
                if *selected_index > 0 && *selected_index >= items.len() {
                    *selected_index = items.len().saturating_sub(1);
                }
            }
            DialogResult::Continue
        }
        KeyCode::Enter => {
            if !items.is_empty() && *selected_index < items.len() {
                let current = items[*selected_index].clone();
                *editing_input = Some(Input::new(current));
                *adding_new = false;
            }
            DialogResult::Continue
        }
        _ => DialogResult::Continue,
    }
}

/// The registered project's `worktree.enabled` override for `path`, if any.
fn project_worktree_override(profile: &str, path: &str) -> Option<bool> {
    crate::session::projects::find_by_canonical_path(profile, std::path::Path::new(path.trim()))
        .and_then(|p| p.overrides.worktree_enabled)
}

/// Whether `tool` can back a structured-view (ACP) session, judged against
/// the resolved config.
fn compute_structured_capable(tool: &str, config: &crate::session::Config) -> bool {
    // Opt-in setting, shared with `session_switch_view_target`.
    config.acp.offer_structured_in_new_session
        && crate::session::builder::structured::tool_acp_capable(tool, config)
}

/// Build label/value pairs for non-default inherited sandbox settings.
fn build_inherited_settings(sandbox: &SandboxConfig) -> Vec<(String, String)> {
    let mut settings = Vec::new();
    if sandbox.mount_ssh {
        settings.push(("Mount SSH".to_string(), "yes".to_string()));
    }
    if !sandbox.extra_volumes.is_empty() {
        settings.push((
            "Extra Volumes".to_string(),
            format!("{} items", sandbox.extra_volumes.len()),
        ));
    }
    if !sandbox.volume_ignores.is_empty() {
        settings.push((
            "Volume Ignores".to_string(),
            format!("{} items", sandbox.volume_ignores.len()),
        ));
    }
    if let Some(ref cpu) = sandbox.cpu_limit {
        settings.push(("CPU Limit".to_string(), cpu.clone()));
    }
    if let Some(ref mem) = sandbox.memory_limit {
        settings.push(("Memory Limit".to_string(), mem.clone()));
    }
    if sandbox.default_terminal_mode == DefaultTerminalMode::Container {
        settings.push(("Terminal Mode".to_string(), "container".to_string()));
    }
    settings
}

/// Row index per main-form field, resolved by
/// [`NewSessionDialog::field_indices`].
pub(super) struct FieldIndices {
    pub profile: usize,
    pub path: usize,
    pub title: usize,
    pub tool: usize,
    pub structured: usize,
    pub yolo: usize,
    pub worktree: usize,
    pub sandbox: usize,
    pub group: usize,
    /// One past the last focusable row, where Tab wraps.
    pub count: usize,
}

impl FieldIndices {
    /// Stands in for a field the current layout hides.
    pub const ABSENT: usize = usize::MAX;
}

impl NewSessionDialog {
    pub fn new(
        tools: AvailableTools,
        existing_groups: Vec<String>,
        profile: &str,
        available_profiles: Vec<String>,
    ) -> Self {
        let current_dir = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        let available_tools: Vec<String> = tools.available_list().to_vec();
        let docker_available = containers::get_container_runtime().is_available();

        let config = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
            profile,
            std::path::Path::new(&current_dir),
        );

        let tool_index = if let Some(ref default_tool) = config.session.default_tool {
            available_tools
                .iter()
                .position(|t| t == default_tool)
                .unwrap_or(0)
        } else {
            0
        };

        let is_default_tool_host_only = available_tools
            .get(tool_index)
            .and_then(|t| crate::agents::get_agent(t))
            .is_some_and(|a| a.host_only);
        let sandbox_enabled =
            docker_available && config.sandbox.enabled_by_default && !is_default_tool_host_only;
        let worktree_enabled = project_worktree_override(profile, &current_dir)
            .unwrap_or(config.worktree.enabled)
            && !is_default_tool_host_only;
        let yolo_mode = config.session.yolo_mode_default;

        let selected_tool = available_tools
            .get(tool_index)
            .or_else(|| available_tools.first())
            .map(|s| s.as_str())
            .unwrap_or("claude");
        let extra_args_value = config
            .session
            .agent_extra_args
            .get(selected_tool)
            .cloned()
            .unwrap_or_default();
        let command_override_value = config.session.resolve_tool_command(selected_tool);
        let structured_capable = compute_structured_capable(selected_tool, &config);
        let structured_default = config.acp.default_new_session_view
            == crate::session::config::NewSessionView::Structured;

        let (extra_env, inherited_settings) = if sandbox_enabled {
            let inherited = build_inherited_settings(&config.sandbox);
            (config.sandbox.environment.clone(), inherited)
        } else {
            (Vec::new(), Vec::new())
        };

        let profile_index = available_profiles
            .iter()
            .position(|p| p == profile)
            .unwrap_or(0);

        // A corrupted profile config degrades to None rather than breaking
        // the picker.
        let profile_descriptions = available_profiles
            .iter()
            .map(|name| {
                crate::session::load_profile_config(name)
                    .ok()
                    .and_then(|c| c.description)
            })
            .collect();

        Self {
            profile: profile.to_string(),
            available_profiles,
            profile_descriptions,
            profile_index,
            title: Input::default(),
            path: Input::new(current_dir),
            group: Input::default(),
            tool_index,
            focused_field: 0,
            available_tools,
            existing_groups,
            group_picker: ListPicker::new("Select Group"),
            branch_picker: ListPicker::new("Select Branch"),
            projects_picker: ListPicker::new("Add registered project"),
            available_projects: Vec::new(),
            dir_picker: DirPicker::new(),
            worktree_enabled,
            worktree_dirty: false,
            worktree_branch: Input::default(),
            create_new_branch: true,
            base_branch: Input::default(),
            workspace_repos: Vec::new(),
            workspace_repos_expanded: false,
            workspace_repo_selected_index: 0,
            workspace_repo_editing_input: None,
            workspace_repo_adding_new: false,
            workspace_repo_ghost: None,
            workspace_repo_dir_picker_active: false,
            worktree_config_mode: false,
            worktree_config_focused_field: 0,
            sandbox_enabled,
            sandbox_image: Input::new(config.sandbox.default_image.clone()),
            docker_available,
            yolo_mode,
            yolo_mode_default: yolo_mode,
            structured_enabled: structured_capable && structured_default,
            structured_default,
            structured_choice: None,
            structured_capable,
            extra_env,
            extra_env_overridden: false,
            env_list_expanded: false,
            env_selected_index: 0,
            env_editing_input: None,
            env_adding_new: false,
            inherited_settings,
            sandbox_config_mode: false,
            sandbox_focused_field: 0,
            tool_config_mode: false,
            tool_config_focused_field: 0,
            extra_args: Input::new(extra_args_value),
            command_override: Input::new(command_override_value),
            error_message: None,
            show_help: false,
            loading: false,
            has_hooks: false,
            current_hook: None,
            hook_output: Vec::new(),
            path_invalid_flash_until: None,
            path_ghost: None,
            group_ghost: None,
            confirm_create_dir: None,
            scratch: false,
            fork_seed: None,
            focusable_rects: Vec::new(),
            sandbox_config_rects: Vec::new(),
            tool_config_rects: Vec::new(),
            worktree_config_rects: Vec::new(),
        }
    }

    pub fn set_path(&mut self, path: String) {
        self.path = Input::new(path);
        if !self.extra_env_overridden {
            self.reload_config_defaults();
        }
    }

    pub fn set_group(&mut self, group: String) {
        self.group = Input::new(group);
    }

    pub fn set_title(&mut self, title: String) {
        self.title = Input::new(title);
    }

    pub fn set_fork_from(&mut self, seed: crate::session::ForkSeed) {
        self.fork_seed = Some(seed);
        if self.terminal_fork() {
            self.structured_capable = false;
            self.apply_structured_default();
        }
    }

    /// A terminal fork resumes through the agent CLI; a structured child would
    /// ignore the seed and start empty, so the Structured field is hidden.
    fn terminal_fork(&self) -> bool {
        matches!(
            self.fork_seed,
            Some(crate::session::ForkSeed::Terminal { .. })
        )
    }

    /// Preselect a tool by name, applying the same per-tool side effects as
    /// cycling the tool field. No-op when the tool is not available.
    pub fn set_tool(&mut self, tool: &str) {
        let Some(index) = self.available_tools.iter().position(|t| t == tool) else {
            return;
        };
        self.tool_index = index;
        if self.selected_tool_always_yolo() {
            self.yolo_mode = true;
        } else {
            self.yolo_mode = self.yolo_mode_default;
        }
        if self.selected_tool_host_only() {
            self.sandbox_enabled = false;
            self.worktree_enabled = false;
            self.worktree_branch.reset();
        }
        self.reload_tool_config();
    }

    /// Move focus to the title field, for "new from selection" where the path
    /// is already filled.
    pub fn focus_title(&mut self) {
        self.focused_field = self.title_field();
    }

    #[cfg(test)]
    pub fn path_value(&self) -> &str {
        self.path.value()
    }

    #[cfg(test)]
    pub fn group_value(&self) -> &str {
        self.group.value()
    }

    #[cfg(test)]
    pub fn fork_seed(&self) -> Option<&crate::session::ForkSeed> {
        self.fork_seed.as_ref()
    }

    /// Re-seed the Structured toggle after the selected tool changes: an
    /// incapable tool forces it off, and a capable one restores the user's
    /// choice or else the configured default.
    fn apply_structured_default(&mut self) {
        self.structured_enabled =
            self.structured_capable && self.structured_choice.unwrap_or(self.structured_default);
    }

    /// The test constructors skip config resolution, so capability is opted
    /// into per test.
    #[cfg(test)]
    pub(super) fn set_structured_capable(&mut self, capable: bool) {
        self.structured_capable = capable;
        if !capable {
            self.structured_enabled = false;
        }
    }

    #[cfg(test)]
    pub fn selected_tool(&self) -> &str {
        self.available_tools
            .get(self.tool_index)
            .map(|s| s.as_str())
            .unwrap_or("")
    }

    pub fn push_hook_progress(&mut self, progress: HookProgress) {
        match progress {
            HookProgress::Started(cmd) => {
                self.current_hook = Some(cmd);
            }
            HookProgress::Output(line) => {
                self.hook_output.push(line);
            }
        }
    }

    pub fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
        if loading {
            self.error_message = None;
        }
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Advance dialog timers (spinner and transient highlights).
    /// Returns true when visual state changed and the UI should redraw.
    pub fn tick(&mut self) -> bool {
        let mut changed = false;

        if self.loading {
            // rattles computes the frame from elapsed time; just redraw.
            changed = true;
        }

        if let Some(until) = self.path_invalid_flash_until {
            if Instant::now() >= until {
                self.path_invalid_flash_until = None;
                changed = true;
            }
        }

        changed
    }

    pub(super) fn selected_profile(&self) -> &str {
        &self.available_profiles[self.profile_index]
    }

    /// Description of the selected profile, `None` when unset or the index is
    /// somehow out of bounds.
    pub(super) fn selected_profile_description(&self) -> Option<&str> {
        self.profile_descriptions
            .get(self.profile_index)
            .and_then(|d| d.as_deref())
    }

    pub(super) fn has_profile_selection(&self) -> bool {
        self.available_profiles.len() > 1
    }

    /// Only the worktree toggle follows a picked or typed path, so other edits survive. A direct
    /// toggle or scratch mode keeps the current value.
    fn seed_worktree_for_path(&mut self) {
        if self.worktree_dirty || self.scratch {
            return;
        }
        let profile = self.selected_profile().to_string();
        let on = project_worktree_override(&profile, self.path.value())
            .unwrap_or_else(|| self.resolve_config_for_path(&profile).worktree.enabled);
        self.worktree_enabled = on && !self.selected_tool_host_only();
    }

    fn resolve_config_for_path(&self, profile: &str) -> crate::session::Config {
        let path = self.path.value().trim();
        if path.is_empty() {
            resolve_config_or_warn(profile)
        } else {
            crate::session::config::repo_config::resolve_config_with_repo_or_warn(
                profile,
                std::path::Path::new(path),
            )
        }
    }

    fn refresh_inherited_sandbox_settings(&mut self) {
        if !self.sandbox_enabled || self.extra_env_overridden {
            return;
        }
        let config = self.resolve_config_for_path(&self.profile);
        self.extra_env = config.sandbox.environment.clone();
        self.inherited_settings = build_inherited_settings(&config.sandbox);
    }

    fn selected_tool_always_yolo(&self) -> bool {
        let tool_name = &self.available_tools[self.tool_index];
        crate::agents::get_agent(tool_name)
            .and_then(|a| a.yolo.as_ref())
            .is_some_and(|y| matches!(y, crate::agents::YoloMode::AlwaysYolo))
    }

    fn selected_tool_host_only(&self) -> bool {
        let tool_name = &self.available_tools[self.tool_index];
        crate::agents::get_agent(tool_name).is_some_and(|a| a.host_only)
    }

    /// Index of the path field, which precedes title. Shifts by one when the
    /// profile picker occupies field 0.
    /// Which main-form row each field occupies. The layout is dynamic: a lone
    /// profile or tool hides its cycler, a host-only agent drops the sandbox
    /// and worktree rows, and a non-ACP tool drops the structured toggle.
    /// Hidden fields get [`FieldIndices::ABSENT`], which no focus index can
    /// equal, so callers compare without special-casing.
    pub(super) fn field_indices(&self) -> FieldIndices {
        let is_host_only = self.selected_tool_host_only();
        let mut next = 0;
        let mut take = |present: bool| {
            if !present {
                return FieldIndices::ABSENT;
            }
            let index = next;
            next += 1;
            index
        };
        FieldIndices {
            profile: take(self.has_profile_selection()),
            path: take(true),
            title: take(true),
            tool: take(self.available_tools.len() > 1),
            structured: take(self.structured_capable),
            yolo: take(!self.selected_tool_always_yolo()),
            worktree: take(!is_host_only),
            sandbox: take(self.docker_available && !is_host_only),
            group: take(true),
            count: next,
        }
    }

    fn path_field(&self) -> usize {
        self.field_indices().path
    }

    fn title_field(&self) -> usize {
        self.field_indices().title
    }

    /// Re-resolve defaults on a profile change, preserving what the user
    /// typed (title, path, group, worktree).
    fn reload_config_defaults(&mut self) {
        let profile = self.selected_profile().to_string();
        self.profile = profile.clone();
        let config = self.resolve_config_for_path(&profile);

        self.tool_index = if let Some(ref default_tool) = config.session.default_tool {
            self.available_tools
                .iter()
                .position(|t| t == default_tool)
                .unwrap_or(0)
        } else {
            0
        };

        self.yolo_mode_default = config.session.yolo_mode_default;
        self.yolo_mode = self.yolo_mode_default;
        self.structured_default = config.acp.default_new_session_view
            == crate::session::config::NewSessionView::Structured;
        self.sandbox_enabled = self.docker_available
            && config.sandbox.enabled_by_default
            && !self.selected_tool_host_only();
        self.worktree_enabled = project_worktree_override(&profile, self.path.value())
            .unwrap_or(config.worktree.enabled)
            && !self.selected_tool_host_only();
        self.worktree_dirty = false;

        self.sandbox_image = Input::new(config.sandbox.default_image.clone());

        if self.sandbox_enabled {
            self.extra_env = config.sandbox.environment.clone();
            self.inherited_settings = build_inherited_settings(&config.sandbox);
        } else {
            self.extra_env.clear();
            self.inherited_settings.clear();
        }
        self.extra_env_overridden = false;

        let selected_tool = self
            .available_tools
            .get(self.tool_index)
            .or_else(|| self.available_tools.first())
            .map(|s| s.as_str())
            .unwrap_or("claude");
        self.extra_args = Input::new(
            config
                .session
                .agent_extra_args
                .get(selected_tool)
                .cloned()
                .unwrap_or_default(),
        );
        self.command_override = Input::new(config.session.resolve_tool_command(selected_tool));
        self.structured_capable =
            !self.terminal_fork() && compute_structured_capable(selected_tool, &config);
        self.apply_structured_default();
        self.tool_config_mode = false;
        self.tool_config_focused_field = 0;

        self.env_list_expanded = false;
        self.env_editing_input = None;
        self.sandbox_config_mode = false;
        self.sandbox_focused_field = 0;
        self.worktree_config_mode = false;
        self.worktree_config_focused_field = 0;
    }

    #[cfg(test)]
    pub(super) fn new_with_config(tools: Vec<&str>, path: String, config: Config) -> Self {
        let tools: Vec<String> = tools.iter().map(|s| s.to_string()).collect();
        let tool_index = if let Some(ref default_tool) = config.session.default_tool {
            tools.iter().position(|t| t == default_tool).unwrap_or(0)
        } else {
            0
        };

        Self {
            profile: "default".to_string(),
            available_profiles: vec!["default".to_string()],
            profile_descriptions: vec![None],
            profile_index: 0,
            title: Input::default(),
            path: Input::new(path),
            group: Input::default(),
            tool_index,
            focused_field: 0,
            available_tools: tools,
            existing_groups: Vec::new(),
            group_picker: ListPicker::new("Select Group"),
            branch_picker: ListPicker::new("Select Branch"),
            projects_picker: ListPicker::new("Add registered project"),
            available_projects: Vec::new(),
            dir_picker: DirPicker::new(),
            worktree_enabled: config.worktree.enabled,
            worktree_dirty: false,
            worktree_branch: Input::default(),
            create_new_branch: true,
            base_branch: Input::default(),
            workspace_repos: Vec::new(),
            workspace_repos_expanded: false,
            workspace_repo_selected_index: 0,
            workspace_repo_editing_input: None,
            workspace_repo_adding_new: false,
            workspace_repo_ghost: None,
            workspace_repo_dir_picker_active: false,
            worktree_config_mode: false,
            worktree_config_focused_field: 0,
            sandbox_enabled: false,
            sandbox_image: Input::new(config.sandbox.default_image.clone()),
            docker_available: false,
            yolo_mode: false,
            yolo_mode_default: false,
            structured_enabled: false,
            structured_default: false,
            structured_choice: None,
            structured_capable: false,
            extra_env: Vec::new(),
            extra_env_overridden: false,
            env_list_expanded: false,
            env_selected_index: 0,
            env_editing_input: None,
            env_adding_new: false,
            inherited_settings: Vec::new(),
            sandbox_config_mode: false,
            sandbox_focused_field: 0,
            tool_config_mode: false,
            tool_config_focused_field: 0,
            extra_args: Input::default(),
            command_override: Input::default(),
            error_message: None,
            show_help: false,
            loading: false,
            has_hooks: false,
            current_hook: None,
            hook_output: Vec::new(),
            path_invalid_flash_until: None,
            path_ghost: None,
            group_ghost: None,
            confirm_create_dir: None,
            scratch: false,
            fork_seed: None,
            focusable_rects: Vec::new(),
            sandbox_config_rects: Vec::new(),
            tool_config_rects: Vec::new(),
            worktree_config_rects: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn new_with_tools(tools: Vec<&str>, path: String) -> Self {
        Self {
            profile: "default".to_string(),
            available_profiles: vec!["default".to_string()],
            profile_descriptions: vec![None],
            profile_index: 0,
            title: Input::default(),
            path: Input::new(path),
            group: Input::default(),
            tool_index: 0,
            focused_field: 0,
            available_tools: tools.iter().map(|s| s.to_string()).collect(),
            existing_groups: Vec::new(),
            group_picker: ListPicker::new("Select Group"),
            branch_picker: ListPicker::new("Select Branch"),
            projects_picker: ListPicker::new("Add registered project"),
            available_projects: Vec::new(),
            dir_picker: DirPicker::new(),
            worktree_enabled: false,
            worktree_dirty: false,
            worktree_branch: Input::default(),
            create_new_branch: true,
            base_branch: Input::default(),
            workspace_repos: Vec::new(),
            workspace_repos_expanded: false,
            workspace_repo_selected_index: 0,
            workspace_repo_editing_input: None,
            workspace_repo_adding_new: false,
            workspace_repo_ghost: None,
            workspace_repo_dir_picker_active: false,
            worktree_config_mode: false,
            worktree_config_focused_field: 0,
            sandbox_enabled: false,
            sandbox_image: Input::new(
                containers::get_container_runtime().effective_default_image(),
            ),
            docker_available: false,
            yolo_mode: false,
            yolo_mode_default: false,
            structured_enabled: false,
            structured_default: false,
            structured_choice: None,
            structured_capable: false,
            extra_env: Vec::new(),
            extra_env_overridden: false,
            env_list_expanded: false,
            env_selected_index: 0,
            env_editing_input: None,
            env_adding_new: false,
            inherited_settings: Vec::new(),
            sandbox_config_mode: false,
            sandbox_focused_field: 0,
            tool_config_mode: false,
            tool_config_focused_field: 0,
            extra_args: Input::default(),
            command_override: Input::default(),
            error_message: None,
            show_help: false,
            loading: false,
            has_hooks: false,
            current_hook: None,
            hook_output: Vec::new(),
            path_invalid_flash_until: None,
            path_ghost: None,
            group_ghost: None,
            confirm_create_dir: None,
            scratch: false,
            fork_seed: None,
            focusable_rects: Vec::new(),
            sandbox_config_rects: Vec::new(),
            tool_config_rects: Vec::new(),
            worktree_config_rects: Vec::new(),
        }
    }

    pub fn set_error(&mut self, error: String) {
        self.error_message = Some(error);
    }
}

/// Picker label for a registered project. Carries the scope, since the merger
/// dedupes by path and the same name can exist in two scopes.
pub(crate) fn project_picker_label(p: &crate::session::Project) -> String {
    format!("{} [{}]  {}", p.name, p.scope.as_str(), p.path)
}

impl NewSessionDialog {
    /// Route a left-click on the main form. `Some(Continue)` when it landed
    /// on a focusable field, moving focus and advancing checkbox / cycler rows
    /// as Space would; `None` when it missed every rect. The config overlays
    /// and pickers are keyboard-only, and leave `focusable_rects` empty.
    pub fn handle_click(&mut self, col: u16, row: u16) -> Option<DialogResult<NewSessionData>> {
        let pos = ratatui::layout::Position::from((col, row));

        // Pickers float over the main form and the config overlays alike, so
        // they route first or the overlay below swallows their clicks.
        if self.group_picker.is_active() {
            match self.group_picker.handle_click(col, row) {
                ListPickerResult::Continue | ListPickerResult::Cancelled => {
                    return Some(DialogResult::Continue);
                }
                ListPickerResult::Selected(value) => {
                    self.group = Input::new(value);
                    self.clear_group_ghost();
                    return Some(DialogResult::Continue);
                }
            }
        }
        if self.branch_picker.is_active() {
            match self.branch_picker.handle_click(col, row) {
                ListPickerResult::Continue | ListPickerResult::Cancelled => {
                    return Some(DialogResult::Continue);
                }
                ListPickerResult::Selected(value) => {
                    self.apply_branch_selection(value);
                    return Some(DialogResult::Continue);
                }
            }
        }
        if self.projects_picker.is_active() {
            match self.projects_picker.handle_click(col, row) {
                ListPickerResult::Continue | ListPickerResult::Cancelled => {
                    return Some(DialogResult::Continue);
                }
                ListPickerResult::Selected(value) => {
                    if let Some(project) = self
                        .available_projects
                        .iter()
                        .find(|p| project_picker_label(p) == value)
                    {
                        let path = project.path.clone();
                        if !self.workspace_repos.iter().any(|p| p == &path) {
                            self.workspace_repos.push(path);
                        }
                    }
                    return Some(DialogResult::Continue);
                }
            }
        }

        // Config overlays win over the main form; their rects are populated
        // only while their mode is active.
        if self.sandbox_config_mode {
            if let Some(hit) = self
                .sandbox_config_rects
                .iter()
                .find(|(_, rect)| rect.contains(pos))
                .map(|(f, _)| *f)
            {
                self.sandbox_focused_field = hit;
            }
            return Some(DialogResult::Continue);
        }
        if self.tool_config_mode {
            if let Some(hit) = self
                .tool_config_rects
                .iter()
                .find(|(_, rect)| rect.contains(pos))
                .map(|(f, _)| *f)
            {
                self.tool_config_focused_field = hit;
            }
            return Some(DialogResult::Continue);
        }
        if self.worktree_config_mode {
            if let Some(hit) = self
                .worktree_config_rects
                .iter()
                .find(|(_, rect)| rect.contains(pos))
                .map(|(f, _)| *f)
            {
                self.worktree_config_focused_field = hit;
                // Field 1 is the new-branch checkbox; a click toggles it.
                if hit == 1 {
                    self.create_new_branch = !self.create_new_branch;
                }
            }
            return Some(DialogResult::Continue);
        }

        let hit_field = self
            .focusable_rects
            .iter()
            .find(|(_, rect)| rect.contains(pos))
            .map(|(field, _)| *field)?;
        if self.focused_field == self.path_field() && hit_field != self.focused_field {
            self.seed_worktree_for_path();
        }
        self.focused_field = hit_field;
        self.activate_focused_field();
        Some(DialogResult::Continue)
    }

    /// Hover only moves the active picker's row highlight, never form focus:
    /// a cursor drifting across the dialog must not steal the field being
    /// typed into. Click still sets focus.
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        if self.group_picker.is_active() {
            return self.group_picker.handle_hover(col, row);
        }
        if self.branch_picker.is_active() {
            return self.branch_picker.handle_hover(col, row);
        }
        if self.projects_picker.is_active() {
            return self.projects_picker.handle_hover(col, row);
        }
        false
    }

    /// Toggle or cycle the focused field, mirroring `handle_key`'s Space /
    /// Left / Right branches so a click produces identical state. Text fields
    /// have no primary action.
    fn activate_focused_field(&mut self) {
        let fields = self.field_indices();

        if self.focused_field == fields.profile {
            if self.available_profiles.len() > 1 {
                self.profile_index = (self.profile_index + 1) % self.available_profiles.len();
                self.reload_config_defaults();
            }
        } else if self.focused_field == fields.tool {
            if self.available_tools.len() > 1 {
                self.tool_index = (self.tool_index + 1) % self.available_tools.len();
                if self.selected_tool_always_yolo() {
                    self.yolo_mode = true;
                } else {
                    self.yolo_mode = self.yolo_mode_default;
                }
                if self.selected_tool_host_only() {
                    self.sandbox_enabled = false;
                    self.worktree_enabled = false;
                    self.worktree_branch.reset();
                }
                self.reload_tool_config();
            }
        } else if self.focused_field == fields.structured {
            self.structured_enabled = !self.structured_enabled;
            self.structured_choice = Some(self.structured_enabled);
        } else if self.focused_field == fields.yolo {
            self.yolo_mode = !self.yolo_mode;
        } else if self.focused_field == fields.worktree {
            // Worktree and scratch are mutually exclusive, so this surfaces
            // an inline hint rather than silently toggling.
            if self.scratch {
                self.error_message = Some(
                    "Worktree is disabled in scratch mode. Press Ctrl+T to leave scratch first."
                        .to_string(),
                );
            } else {
                self.worktree_enabled = !self.worktree_enabled;
                self.worktree_dirty = true;
                if !self.worktree_enabled {
                    self.worktree_config_mode = false;
                }
            }
        } else if self.focused_field == fields.sandbox {
            self.sandbox_enabled = !self.sandbox_enabled;
            if self.sandbox_enabled {
                let config = self.resolve_config_for_path(&self.profile);
                self.extra_env = config.sandbox.environment.clone();
                self.inherited_settings = build_inherited_settings(&config.sandbox);
                self.extra_env_overridden = false;
            } else {
                self.extra_env.clear();
                self.extra_env_overridden = false;
                self.env_list_expanded = false;
                self.env_editing_input = None;
                self.inherited_settings.clear();
                self.sandbox_config_mode = false;
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<NewSessionData> {
        if self.loading {
            if matches!(key.code, KeyCode::Esc) {
                self.loading = false;
                return DialogResult::Cancel;
            }
            return DialogResult::Continue;
        }

        if self.show_help {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
                self.show_help = false;
            }
            return DialogResult::Continue;
        }

        if self.sandbox_config_mode {
            return self.handle_sandbox_config_key(key);
        }

        if self.tool_config_mode {
            return self.handle_tool_config_key(key);
        }

        if self.worktree_config_mode {
            return self.handle_worktree_config_key(key);
        }

        if self.confirm_create_dir.is_some() {
            return self.handle_confirm_create_dir_key(key);
        }

        if self.group_picker.is_active() {
            if let ListPickerResult::Selected(value) = self.group_picker.handle_key(key) {
                self.group = Input::new(value);
                self.clear_group_ghost();
            }
            return DialogResult::Continue;
        }

        if self.branch_picker.is_active() {
            if let ListPickerResult::Selected(value) = self.branch_picker.handle_key(key) {
                self.apply_branch_selection(value);
            }
            return DialogResult::Continue;
        }

        if self.dir_picker.is_active() {
            match self.dir_picker.handle_key(key) {
                DirPickerResult::Selected(path) => {
                    persist_last_browse_dir(&path);
                    if self.workspace_repo_dir_picker_active {
                        self.workspace_repo_editing_input = Some(Input::new(path));
                        self.workspace_repo_ghost = self
                            .workspace_repo_editing_input
                            .as_ref()
                            .and_then(path_input::compute_path_ghost);
                        self.workspace_repo_dir_picker_active = false;
                    } else {
                        self.path = Input::new(path);
                        self.seed_worktree_for_path();
                        self.recompute_path_ghost();
                    }
                }
                DirPickerResult::Cancelled => {
                    self.workspace_repo_dir_picker_active = false;
                }
                DirPickerResult::Continue => {}
            }
            return DialogResult::Continue;
        }

        // Scratch is mutually exclusive with worktrees and extra-repo
        // workspaces, so turning it on clears them.
        if key.code == KeyCode::Char('t') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.scratch = !self.scratch;
            if self.scratch {
                self.worktree_enabled = false;
                self.workspace_repos.clear();
                self.workspace_repos_expanded = false;
            }
            self.error_message = None;
            return DialogResult::Continue;
        }

        let fields = self.field_indices();

        if key.code == KeyCode::Char('p') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.focused_field == self.path_field() {
                let path_value = self.path.value().trim().to_string();
                let initial = if path_value.is_empty() {
                    last_browse_dir().unwrap_or_default()
                } else {
                    path_value
                };
                self.dir_picker.activate(&initial);
                return DialogResult::Continue;
            }
            if self.focused_field == fields.tool {
                self.tool_config_mode = true;
                self.tool_config_focused_field = 0;
                return DialogResult::Continue;
            }
            if self.focused_field == fields.group && !self.existing_groups.is_empty() {
                self.group_picker.activate(self.existing_groups.clone());
                return DialogResult::Continue;
            }
            if self.focused_field == fields.worktree {
                self.worktree_config_mode = true;
                self.worktree_config_focused_field = 0;
                self.error_message = None;
                return DialogResult::Continue;
            }
            if self.focused_field == fields.sandbox && self.sandbox_enabled {
                self.refresh_inherited_sandbox_settings();
                self.sandbox_config_mode = true;
                self.sandbox_focused_field = 0;
                return DialogResult::Continue;
            }
        }

        if self.handle_path_shortcuts(key) {
            return DialogResult::Continue;
        }

        if self.handle_group_shortcuts(key, fields.group) {
            return DialogResult::Continue;
        }

        match key.code {
            KeyCode::Char('?') => {
                self.show_help = true;
                DialogResult::Continue
            }
            KeyCode::Esc => {
                self.error_message = None;
                DialogResult::Cancel
            }
            KeyCode::Enter => {
                self.error_message = None;
                if self.focused_field == self.path_field() {
                    self.seed_worktree_for_path();
                }
                // The server provisions the scratch dir, so no path check.
                if !self.scratch {
                    let path_str = self.path.value().trim().to_string();
                    let resolved = path_input::expand_tilde(&path_str);
                    if !std::path::Path::new(&resolved).exists() {
                        self.confirm_create_dir = Some(false);
                        return DialogResult::Continue;
                    }
                }
                if !self.validate_structured() {
                    return DialogResult::Continue;
                }
                self.build_submit_result()
            }
            KeyCode::Tab | KeyCode::Down => {
                if self.focused_field == self.path_field() {
                    self.clear_path_ghost();
                    self.seed_worktree_for_path();
                }
                if self.focused_field == fields.group {
                    self.clear_group_ghost();
                }
                self.focused_field = (self.focused_field + 1) % fields.count;
                if self.focused_field == self.path_field() {
                    self.recompute_path_ghost();
                }
                if self.focused_field == fields.group {
                    self.recompute_group_ghost();
                }
                DialogResult::Continue
            }
            KeyCode::BackTab | KeyCode::Up => {
                if self.focused_field == self.path_field() {
                    self.clear_path_ghost();
                    self.seed_worktree_for_path();
                }
                if self.focused_field == fields.group {
                    self.clear_group_ghost();
                }
                self.focused_field = if self.focused_field == 0 {
                    fields.count - 1
                } else {
                    self.focused_field - 1
                };
                if self.focused_field == self.path_field() {
                    self.recompute_path_ghost();
                }
                if self.focused_field == fields.group {
                    self.recompute_group_ghost();
                }
                DialogResult::Continue
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if self.focused_field == fields.profile =>
            {
                if self.available_profiles.len() > 1 {
                    if key.code == KeyCode::Left {
                        self.profile_index = if self.profile_index == 0 {
                            self.available_profiles.len() - 1
                        } else {
                            self.profile_index - 1
                        };
                    } else {
                        self.profile_index =
                            (self.profile_index + 1) % self.available_profiles.len();
                    }
                    self.reload_config_defaults();
                }
                DialogResult::Continue
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if self.focused_field == fields.tool =>
            {
                if key.code == KeyCode::Left {
                    self.tool_index = if self.tool_index == 0 {
                        self.available_tools.len() - 1
                    } else {
                        self.tool_index - 1
                    };
                } else {
                    self.tool_index = (self.tool_index + 1) % self.available_tools.len();
                }
                if self.selected_tool_always_yolo() {
                    self.yolo_mode = true;
                } else {
                    self.yolo_mode = self.yolo_mode_default;
                }
                if self.selected_tool_host_only() {
                    self.sandbox_enabled = false;
                    self.worktree_enabled = false;
                    self.worktree_branch.reset();
                }
                self.reload_tool_config();
                DialogResult::Continue
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if self.focused_field == fields.worktree =>
            {
                // Without this guard the user could turn scratch on, Space
                // worktree back on, and submit a payload the server rejects.
                if self.scratch {
                    self.error_message = Some(
                        "Worktree is disabled in scratch mode. Press Ctrl+T to leave scratch first.".to_string(),
                    );
                    return DialogResult::Continue;
                }
                self.worktree_enabled = !self.worktree_enabled;
                self.worktree_dirty = true;
                if !self.worktree_enabled {
                    self.worktree_config_mode = false;
                }
                DialogResult::Continue
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if self.focused_field == fields.sandbox =>
            {
                self.sandbox_enabled = !self.sandbox_enabled;
                if self.sandbox_enabled {
                    let config = self.resolve_config_for_path(&self.profile);
                    self.extra_env = config.sandbox.environment.clone();
                    self.inherited_settings = build_inherited_settings(&config.sandbox);
                    self.extra_env_overridden = false;
                } else {
                    self.extra_env.clear();
                    self.extra_env_overridden = false;
                    self.env_list_expanded = false;
                    self.env_editing_input = None;
                    self.inherited_settings.clear();
                    self.sandbox_config_mode = false;
                }
                DialogResult::Continue
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if self.focused_field == fields.yolo =>
            {
                self.yolo_mode = !self.yolo_mode;
                DialogResult::Continue
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if self.focused_field == fields.structured =>
            {
                self.structured_enabled = !self.structured_enabled;
                self.structured_choice = Some(self.structured_enabled);
                DialogResult::Continue
            }
            _ => {
                if self.focused_field != fields.profile
                    && self.focused_field != fields.tool
                    && self.focused_field != fields.worktree
                    && self.focused_field != fields.sandbox
                    && self.focused_field != fields.yolo
                    && self.focused_field != fields.structured
                {
                    self.current_input_mut()
                        .handle_event(&crossterm::event::Event::Key(key));
                    self.error_message = None;
                    if self.focused_field == self.path_field() {
                        self.path_invalid_flash_until = None;
                        self.recompute_path_ghost();
                    }
                    if self.focused_field == fields.group {
                        self.recompute_group_ghost();
                    }
                }
                DialogResult::Continue
            }
        }
    }

    fn handle_sandbox_config_key(&mut self, key: KeyEvent) -> DialogResult<NewSessionData> {
        // Fields: 0=image, 1=env. Inherited settings are not focusable.
        const SANDBOX_IMAGE: usize = 0;
        const SANDBOX_ENV: usize = 1;
        const SANDBOX_MAX: usize = 2;

        if self.env_list_expanded && self.sandbox_focused_field == SANDBOX_ENV {
            return self.handle_env_list_key(key);
        }

        match key.code {
            KeyCode::Esc => {
                self.sandbox_config_mode = false;
                DialogResult::Continue
            }
            KeyCode::Char('?') => {
                self.show_help = true;
                DialogResult::Continue
            }
            KeyCode::Enter if self.sandbox_focused_field == SANDBOX_ENV => {
                self.env_list_expanded = true;
                self.env_selected_index = 0;
                DialogResult::Continue
            }
            KeyCode::Enter => {
                self.sandbox_config_mode = false;
                DialogResult::Continue
            }
            KeyCode::Tab | KeyCode::Down => {
                self.sandbox_focused_field = (self.sandbox_focused_field + 1) % SANDBOX_MAX;
                DialogResult::Continue
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.sandbox_focused_field = if self.sandbox_focused_field == 0 {
                    SANDBOX_MAX - 1
                } else {
                    self.sandbox_focused_field - 1
                };
                DialogResult::Continue
            }
            _ => {
                if self.sandbox_focused_field == SANDBOX_IMAGE {
                    self.sandbox_image
                        .handle_event(&crossterm::event::Event::Key(key));
                }
                DialogResult::Continue
            }
        }
    }

    /// Tool configuration mode. `?` opens the help overlay; everything else
    /// goes to the shared tool-config component.
    fn handle_tool_config_key(&mut self, key: KeyEvent) -> DialogResult<NewSessionData> {
        if key.code == KeyCode::Char('?') {
            self.show_help = true;
            return DialogResult::Continue;
        }
        match crate::tui::components::handle_tool_config_key(
            key,
            &mut self.command_override,
            &mut self.extra_args,
            &mut self.tool_config_focused_field,
        ) {
            crate::tui::components::ToolConfigOutcome::Close => self.tool_config_mode = false,
            crate::tui::components::ToolConfigOutcome::Continue => {}
        }
        DialogResult::Continue
    }

    /// Append the picked project's path to the workspace repos list.
    fn apply_picked_project(&mut self, value: &str) {
        if let Some(project) = self
            .available_projects
            .iter()
            .find(|p| project_picker_label(p) == value)
        {
            if !self.workspace_repos.contains(&project.path) {
                self.workspace_repos.push(project.path.clone());
            }
            self.workspace_repos_expanded = true;
            self.workspace_repo_selected_index = self.workspace_repos.len().saturating_sub(1);
        }
    }

    /// Store a picked branch. The picker serves both Name and Base, so the
    /// focused field decides where it lands and every entry point must route
    /// through here or a Base selection overwrites Name.
    fn apply_branch_selection(&mut self, value: String) {
        if self.worktree_config_mode && self.worktree_config_focused_field == WT_BASE_BRANCH_FIELD {
            self.base_branch = Input::new(value);
        } else {
            self.worktree_branch = Input::new(value);
        }
    }

    /// Activate the branch picker. The path field holds raw input, so it needs
    /// the same tilde expansion submit does or `~/repo` never opens. Failures
    /// surface inline rather than being swallowed.
    fn open_branch_picker(&mut self) {
        let path_str = self.path.value().trim().to_string();
        if path_str.is_empty() {
            self.error_message = Some("Set the project path before picking a branch.".into());
            return;
        }
        let resolved = path_input::expand_tilde(&path_str);
        match crate::git::diff::list_branches(std::path::Path::new(&resolved)) {
            Ok(branches) if !branches.is_empty() => {
                self.error_message = None;
                self.branch_picker.activate(branches);
            }
            Ok(_) => {
                self.error_message = Some(format!("No branches found in {}.", path_str));
            }
            Err(e) => {
                self.error_message = Some(format!("Cannot list branches in {}: {}", path_str, e));
            }
        }
    }

    /// Activate the registered-projects picker, filtering out the primary repo
    /// and paths already listed, to stay clear of the builder's duplicate guard.
    fn open_projects_picker(&mut self) {
        let primary = self.path.value().trim().to_string();
        let merged = crate::session::projects::load_merged(&self.profile).unwrap_or_default();
        self.available_projects = merged
            .into_iter()
            .filter(|p| p.path != primary && !self.workspace_repos.contains(&p.path))
            .collect();
        if self.available_projects.is_empty() {
            self.error_message = Some(
                "No registered projects available. Add one with `aoe project add <path>`.".into(),
            );
        } else {
            let labels: Vec<String> = self
                .available_projects
                .iter()
                .map(project_picker_label)
                .collect();
            self.projects_picker.activate(labels);
        }
    }

    fn handle_worktree_config_key(&mut self, key: KeyEvent) -> DialogResult<NewSessionData> {
        // Fields: 0=name, 1=new_branch, 2=base_branch, 3=extra_repos.
        const WT_NAME: usize = 0;
        const WT_NEW_BRANCH: usize = 1;
        const WT_EXTRA_REPOS: usize = 3;
        const WT_MAX: usize = 4;

        if self.branch_picker.is_active() {
            if let ListPickerResult::Selected(value) = self.branch_picker.handle_key(key) {
                self.apply_branch_selection(value);
            }
            return DialogResult::Continue;
        }

        if self.projects_picker.is_active() {
            if let ListPickerResult::Selected(value) = self.projects_picker.handle_key(key) {
                self.apply_picked_project(&value);
            }
            return DialogResult::Continue;
        }

        if self.workspace_repos_expanded && self.worktree_config_focused_field == WT_EXTRA_REPOS {
            return self.handle_workspace_repos_list_key(key);
        }

        match key.code {
            KeyCode::Esc => {
                self.worktree_config_mode = false;
                self.error_message = None;
                DialogResult::Continue
            }
            KeyCode::Char('?') => {
                self.show_help = true;
                DialogResult::Continue
            }
            // The hint row advertises Ctrl+P for every field but extra-repos,
            // the checkbox included, so the guard has to cover it too.
            KeyCode::Char('p')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(
                        self.worktree_config_focused_field,
                        WT_NAME | WT_NEW_BRANCH | WT_BASE_BRANCH_FIELD
                    ) =>
            {
                self.open_branch_picker();
                DialogResult::Continue
            }
            KeyCode::Char('r')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.worktree_config_focused_field == WT_EXTRA_REPOS =>
            {
                self.open_projects_picker();
                DialogResult::Continue
            }
            KeyCode::Enter if self.worktree_config_focused_field == WT_EXTRA_REPOS => {
                self.workspace_repos_expanded = true;
                self.workspace_repo_selected_index = 0;
                DialogResult::Continue
            }
            KeyCode::Enter => {
                self.worktree_config_mode = false;
                self.error_message = None;
                DialogResult::Continue
            }
            KeyCode::Tab | KeyCode::Down => {
                self.worktree_config_focused_field =
                    (self.worktree_config_focused_field + 1) % WT_MAX;
                DialogResult::Continue
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.worktree_config_focused_field = if self.worktree_config_focused_field == 0 {
                    WT_MAX - 1
                } else {
                    self.worktree_config_focused_field - 1
                };
                DialogResult::Continue
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if self.worktree_config_focused_field == WT_NEW_BRANCH =>
            {
                self.create_new_branch = !self.create_new_branch;
                DialogResult::Continue
            }
            _ if self.worktree_config_focused_field == WT_NAME => {
                self.worktree_branch
                    .handle_event(&crossterm::event::Event::Key(key));
                DialogResult::Continue
            }
            _ if self.worktree_config_focused_field == WT_BASE_BRANCH_FIELD => {
                self.base_branch
                    .handle_event(&crossterm::event::Event::Key(key));
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    fn handle_env_list_key(&mut self, key: KeyEvent) -> DialogResult<NewSessionData> {
        let validate =
            |value: &str, list: &[String]| !value.is_empty() && !list.contains(&value.to_string());
        let snapshot: Vec<String> = self.extra_env.clone();
        let result = handle_editable_list_key(
            key,
            &mut self.extra_env,
            &mut self.env_list_expanded,
            &mut self.env_selected_index,
            &mut self.env_editing_input,
            &mut self.env_adding_new,
            validate,
        );

        if self.extra_env != snapshot {
            self.extra_env_overridden = true;
            self.error_message = self
                .extra_env
                .get(self.env_selected_index)
                .and_then(|entry| crate::session::validate_env_entry(entry));
        }

        result
    }

    fn handle_workspace_repos_list_key(&mut self, key: KeyEvent) -> DialogResult<NewSessionData> {
        if self.workspace_repo_editing_input.is_some() {
            if key.code == KeyCode::Char('p') && key.modifiers.contains(KeyModifiers::CONTROL) {
                let initial = self
                    .workspace_repo_editing_input
                    .as_ref()
                    .map(|i| i.value().trim().to_string())
                    .unwrap_or_default();
                let initial = if initial.is_empty() {
                    last_browse_dir().unwrap_or_else(|| {
                        std::env::current_dir()
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_else(|_| ".".to_string())
                    })
                } else {
                    initial
                };
                self.workspace_repo_dir_picker_active = true;
                self.dir_picker.activate(&initial);
                return DialogResult::Continue;
            }

            if matches!(key.code, KeyCode::Right | KeyCode::End)
                && key.modifiers == KeyModifiers::NONE
            {
                if let Some(ref input) = self.workspace_repo_editing_input {
                    let cursor = input.cursor();
                    let char_len = input.value().chars().count();
                    if cursor >= char_len {
                        if let Some(ghost) = self.workspace_repo_ghost.take() {
                            if let Some(ref mut input) = self.workspace_repo_editing_input {
                                let value = input.value().to_string();
                                let cursor_char = input.cursor().min(value.chars().count());
                                if ghost.input_snapshot == value
                                    && ghost.cursor_snapshot == cursor_char
                                {
                                    let mut new_value = value;
                                    new_value.push_str(&ghost.ghost_text);
                                    *input = Input::new(new_value);
                                    self.workspace_repo_ghost =
                                        path_input::compute_path_ghost(input);
                                    return DialogResult::Continue;
                                }
                            }
                        }
                    }
                }
            }
        }

        // 'a' pre-populates with the expanded cwd, like the main path field.
        if self.workspace_repo_editing_input.is_none()
            && key.code == KeyCode::Char('a')
            && key.modifiers == KeyModifiers::NONE
        {
            let cwd = std::env::current_dir()
                .map(|p| {
                    let mut s = crate::util::collapse_tilde(&p.to_string_lossy());
                    if !s.ends_with('/') {
                        s.push('/');
                    }
                    s
                })
                .unwrap_or_default();
            self.workspace_repo_editing_input = Some(Input::new(cwd));
            self.workspace_repo_adding_new = true;
            self.workspace_repo_ghost = self
                .workspace_repo_editing_input
                .as_ref()
                .and_then(path_input::compute_path_ghost);
            return DialogResult::Continue;
        }

        let validate =
            |value: &str, list: &[String]| !value.is_empty() && !list.contains(&value.to_string());

        let had_input = self.workspace_repo_editing_input.is_some();
        let was_adding = self.workspace_repo_adding_new;
        let edit_index = self.workspace_repo_selected_index;
        let result = handle_editable_list_key(
            key,
            &mut self.workspace_repos,
            &mut self.workspace_repos_expanded,
            &mut self.workspace_repo_selected_index,
            &mut self.workspace_repo_editing_input,
            &mut self.workspace_repo_adding_new,
            validate,
        );

        if had_input && self.workspace_repo_editing_input.is_none() {
            let idx = if was_adding {
                self.workspace_repos.len().saturating_sub(1)
            } else {
                edit_index
            };
            if let Some(entry) = self.workspace_repos.get_mut(idx) {
                *entry = path_input::expand_tilde(entry);
            }
            self.workspace_repo_ghost = None;
        }

        if self.workspace_repo_editing_input.is_some() {
            self.workspace_repo_ghost = self
                .workspace_repo_editing_input
                .as_ref()
                .and_then(path_input::compute_path_ghost);
        } else {
            self.workspace_repo_ghost = None;
        }

        result
    }

    fn reload_tool_config(&mut self) {
        let profile = self.selected_profile().to_string();
        let config = resolve_config_or_warn(&profile);
        let tool = self
            .available_tools
            .get(self.tool_index)
            .or_else(|| self.available_tools.first())
            .map(|s| s.as_str())
            .unwrap_or("claude");
        self.extra_args = Input::new(
            config
                .session
                .agent_extra_args
                .get(tool)
                .cloned()
                .unwrap_or_default(),
        );
        self.command_override = Input::new(config.session.resolve_tool_command(tool));
        self.structured_capable =
            !self.terminal_fork() && compute_structured_capable(tool, &config);
        self.apply_structured_default();
    }

    fn current_input_mut(&mut self) -> &mut Input {
        let fields = self.field_indices();
        match self.focused_field {
            n if n == fields.title => &mut self.title,
            n if n == fields.path => &mut self.path,
            n if n == fields.group => &mut self.group,
            _ => &mut self.title,
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        let target: &mut Input = if let Some(ref mut input) = self.env_editing_input {
            input
        } else if let Some(ref mut input) = self.workspace_repo_editing_input {
            input
        } else if self.tool_config_mode {
            if self.tool_config_focused_field == 0 {
                &mut self.command_override
            } else {
                &mut self.extra_args
            }
        } else if self.worktree_config_mode && self.worktree_config_focused_field == 0 {
            &mut self.worktree_branch
        } else if self.sandbox_config_mode && self.sandbox_focused_field == 0 {
            &mut self.sandbox_image
        } else {
            self.current_input_mut()
        };
        super::paste_into_input(target, text);
    }

    /// Check the structured-view choice still holds (adapter installed, tool
    /// capable), surfacing a refusal inline. Runs before any worktree, scratch
    /// or container work, so a refusal cannot orphan resources.
    fn validate_structured(&mut self) -> bool {
        if !(self.structured_enabled && self.structured_capable) {
            return true;
        }
        let profile = self.selected_profile().to_string();
        let config = self.resolve_config_for_path(&profile);
        if let Err(msg) = crate::session::builder::structured::validate_structured_choice(
            &self.available_tools[self.tool_index],
            self.command_override.value(),
            &config,
        ) {
            self.error_message = Some(msg);
            return false;
        }
        true
    }

    fn build_submit_result(&self) -> DialogResult<NewSessionData> {
        let title_value = self.title.value().trim();
        let final_title = title_value.to_string();
        let worktree_value = self.worktree_branch.value().trim();
        let worktree_branch = if self.worktree_enabled && !worktree_value.is_empty() {
            Some(worktree_value.to_string())
        } else {
            None
        };
        let base_value = self.base_branch.value().trim();
        let base_branch =
            if self.worktree_enabled && self.create_new_branch && !base_value.is_empty() {
                Some(base_value.to_string())
            } else {
                None
            };
        DialogResult::Submit(NewSessionData {
            profile: self.selected_profile().to_string(),
            title: final_title,
            // Scratch sends an empty path; the server provisions the dir.
            path: if self.scratch {
                String::new()
            } else {
                self.path.value().trim().to_string()
            },
            group: self.group.value().trim().to_string(),
            tool: self.available_tools[self.tool_index].clone(),
            worktree_enabled: !self.scratch && self.worktree_enabled,
            worktree_branch,
            create_new_branch: self.create_new_branch,
            base_branch,
            extra_repo_paths: if !self.scratch && self.worktree_enabled {
                self.workspace_repos.clone()
            } else {
                Vec::new()
            },
            sandbox: self.sandbox_enabled,
            sandbox_image: self.sandbox_image.value().trim().to_string(),
            yolo_mode: self.yolo_mode || self.selected_tool_always_yolo(),
            extra_env: if self.sandbox_enabled && self.extra_env_overridden {
                self.extra_env.clone()
            } else {
                Vec::new()
            },
            extra_args: self.extra_args.value().trim().to_string(),
            command_override: self.command_override.value().trim().to_string(),
            scratch: self.scratch,
            fork_seed: self.fork_seed.clone(),
            structured: self.structured_enabled && self.structured_capable,
        })
    }

    fn handle_confirm_create_dir_key(&mut self, key: KeyEvent) -> DialogResult<NewSessionData> {
        let selected = self.confirm_create_dir.as_mut().unwrap();
        match key.code {
            KeyCode::Left | KeyCode::Char('h') => {
                *selected = true;
                DialogResult::Continue
            }
            KeyCode::Right | KeyCode::Char('l') => {
                *selected = false;
                DialogResult::Continue
            }
            KeyCode::Tab => {
                *selected = !*selected;
                DialogResult::Continue
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.confirm_create_dir = None;
                self.try_create_dir_and_submit()
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.confirm_create_dir = None;
                self.focused_field = self.path_field();
                DialogResult::Continue
            }
            KeyCode::Enter => {
                let yes = *selected;
                self.confirm_create_dir = None;
                if yes {
                    self.try_create_dir_and_submit()
                } else {
                    self.focused_field = self.path_field();
                    DialogResult::Continue
                }
            }
            _ => DialogResult::Continue,
        }
    }

    fn try_create_dir_and_submit(&mut self) -> DialogResult<NewSessionData> {
        if !self.validate_structured() {
            return DialogResult::Continue;
        }
        let path_str = self.path.value().trim().to_string();
        let resolved = path_input::expand_tilde(&path_str);
        match std::fs::create_dir_all(&resolved) {
            Ok(()) => self.build_submit_result(),
            Err(e) => {
                self.error_message = Some(format!("Failed to create directory: {}", e));
                self.focused_field = self.path_field();
                DialogResult::Continue
            }
        }
    }
}

fn last_browse_dir() -> Option<String> {
    let cfg = load_config().ok().flatten()?;
    let path = cfg.app_state.last_browse_dir?;
    if path.is_dir() {
        Some(path.to_string_lossy().to_string())
    } else {
        None
    }
}

fn persist_last_browse_dir(selected: &str) {
    let path = std::path::PathBuf::from(selected);
    let dir = if path.is_dir() {
        path
    } else if let Some(parent) = path.parent() {
        parent.to_path_buf()
    } else {
        return;
    };
    let result = update_app_state(|state| {
        state.last_browse_dir = Some(dir);
    });
    if let Err(e) = result {
        tracing::warn!(target: "tui.dialog", "Failed to save last_browse_dir: {}", e);
    }
}
