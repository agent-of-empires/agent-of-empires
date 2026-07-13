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
use crate::session::config::{load_config, save_config, DefaultTerminalMode, SandboxConfig};
use crate::session::profile_config::resolve_config_or_warn;
use crate::session::repo_config::HookProgress;
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
    /// Branch the new worktree branch is based on. Only meaningful when
    /// `create_new_branch` is true; ignored otherwise. `None` falls
    /// back to the repository's default branch. See #948.
    pub base_branch: Option<String>,
    pub extra_repo_paths: Vec<String>,
    pub sandbox: bool,
    /// The sandbox image to use (always populated from the input field).
    pub sandbox_image: String,
    pub yolo_mode: bool,
    /// Additional environment entries for the container.
    /// `KEY` = pass through from host, `KEY=VALUE` = set explicitly.
    pub extra_env: Vec<String>,
    /// Extra arguments to append after the agent binary
    pub extra_args: String,
    /// Command override for the agent binary (replaces the default binary)
    pub command_override: String,
    /// Scratch session: provision a fresh directory under
    /// `<app_dir>/scratch/<id>/` and persist `instance.scratch = true`.
    /// Mutually exclusive with worktree mode.
    pub scratch: bool,
    /// One-shot fork seed carried when this session was opened as a fork of
    /// another. `None` for an ordinary new session.
    pub fork_seed: Option<crate::session::ForkSeed>,
}

/// Single conversion point for turning wizard output into builder input.
/// Both creation paths (the synchronous `create_session` and the background
/// `CreationPoller`) go through this, so a newly added field cannot be
/// forwarded on one path and silently dropped on the other (the bug class
/// behind the hardcoded `fork_seed: None` regression). `profile` is the one
/// field deliberately not carried: builders take it as a separate argument.
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
            scratch: data.scratch,
            fork_seed: data.fork_seed,
        }
    }
}

pub struct NewSessionDialog {
    pub(super) profile: String,
    pub(super) available_profiles: Vec<String>,
    /// Per-profile short descriptions, indexed in lockstep with
    /// `available_profiles`. `None` when a profile has no description
    /// configured. Surfaced as helper text under the profile name in the
    /// picker when one is set.
    pub(super) profile_descriptions: Vec<Option<String>>,
    pub(super) profile_index: usize,
    pub(super) title: Input,
    pub(super) path: Input,
    pub(super) group: Input,
    pub(super) tool_index: usize,
    pub(super) focused_field: usize,
    pub(super) available_tools: Vec<String>,
    pub(super) worktree_enabled: bool,
    pub(super) worktree_branch: Input,
    pub(super) create_new_branch: bool,
    /// Free-text "base branch" input shown in the worktree config
    /// overlay when "new branch" is on. Empty value = use repo
    /// default. See #948.
    pub(super) base_branch: Input,
    pub(super) sandbox_enabled: bool,
    pub(super) sandbox_image: Input,
    pub(super) docker_available: bool,
    pub(super) yolo_mode: bool,
    pub(super) yolo_mode_default: bool,
    /// Additional repo paths for multi-repo workspace
    pub(super) workspace_repos: Vec<String>,
    /// Whether the workspace repos list is expanded (editing mode)
    pub(super) workspace_repos_expanded: bool,
    /// Currently selected index in the workspace repos list
    pub(super) workspace_repo_selected_index: usize,
    /// Input for editing/adding workspace repo entries
    pub(super) workspace_repo_editing_input: Option<Input>,
    /// Whether we are adding a new repo entry (vs editing existing)
    pub(super) workspace_repo_adding_new: bool,
    /// Ghost completion for workspace repo path editing
    pub(super) workspace_repo_ghost: Option<path_input::PathGhostCompletion>,
    /// Whether the dir picker was opened for a workspace repo (vs the main path)
    pub(super) workspace_repo_dir_picker_active: bool,
    /// Worktree configuration overlay mode (Ctrl+P on worktree field)
    pub(super) worktree_config_mode: bool,
    /// Focused field within the worktree config overlay (0=name, 1=new_branch, 2=extra_repos)
    pub(super) worktree_config_focused_field: usize,
    /// Extra environment entries (session-specific).
    /// `KEY` = pass through, `KEY=VALUE` = set explicitly.
    pub(super) extra_env: Vec<String>,
    /// True after the user edits the sandbox env list in this dialog.
    /// Inherited config is shown for review, but is not submitted as a
    /// per-session override unless this flips.
    pub(super) extra_env_overridden: bool,
    /// Whether the env list is expanded (editing mode)
    pub(super) env_list_expanded: bool,
    /// Currently selected index in the env list
    pub(super) env_selected_index: usize,
    /// Input for editing/adding env entries
    pub(super) env_editing_input: Option<Input>,
    /// Whether we are adding a new entry (vs editing existing)
    pub(super) env_adding_new: bool,
    /// Pre-computed label/value pairs for non-default inherited sandbox settings.
    pub(super) inherited_settings: Vec<(String, String)>,
    pub(super) sandbox_config_mode: bool,
    pub(super) sandbox_focused_field: usize,
    /// Tool configuration mode (Ctrl+P on tool field)
    pub(super) tool_config_mode: bool,
    pub(super) tool_config_focused_field: usize,
    /// Extra args for the selected tool (loaded from config)
    pub(super) extra_args: Input,
    /// Command override for the selected tool (loaded from config)
    pub(super) command_override: Input,
    pub(super) existing_groups: Vec<String>,
    pub(super) group_picker: ListPicker,
    pub(super) branch_picker: ListPicker,
    /// Picker over registered projects (global ∪ profile). Activated from
    /// the workspace_repos list with Ctrl+R; selection appends the
    /// project's path to `workspace_repos`.
    pub(super) projects_picker: ListPicker,
    /// Snapshot of registered projects at picker activation, parallel to
    /// the picker's display list, used to map the chosen name back to a path.
    pub(super) available_projects: Vec<crate::session::Project>,
    pub(super) dir_picker: DirPicker,
    pub(super) error_message: Option<String>,
    pub(super) show_help: bool,
    /// Whether the dialog is in loading state (creating session in background)
    pub(super) loading: bool,
    /// Spinner animation frame counter
    /// Whether hooks are being executed during loading
    pub(super) has_hooks: bool,
    /// The currently running hook command
    pub(super) current_hook: Option<String>,
    /// Accumulated output lines from hook execution
    pub(super) hook_output: Vec<String>,
    /// Temporary highlight state for invalid path input.
    pub(super) path_invalid_flash_until: Option<Instant>,
    /// Ghost text completion for the path field (fish-shell style).
    path_ghost: Option<PathGhostCompletion>,
    /// Ghost text completion for the group field (fish-shell style).
    group_ghost: Option<GroupGhostCompletion>,
    /// Inline confirmation for creating a non-existent directory.
    /// None = inactive, Some(true) = Yes selected, Some(false) = No selected.
    pub(super) confirm_create_dir: Option<bool>,
    /// Scratch-session marker. When true, submission skips the path
    /// canonicalize/exists checks; the server (or `aoe add` CLI path)
    /// provisions a fresh scratch directory. Toggled with Ctrl+T from
    /// anywhere in the form. Mutually exclusive with worktree mode.
    pub(super) scratch: bool,
    /// One-shot fork seed when this dialog was opened as a fork. Carried
    /// verbatim into the resulting `NewSessionData` on submit; `None` for an
    /// ordinary new session.
    pub(super) fork_seed: Option<crate::session::ForkSeed>,
    /// Per-field hit rect captured by the renderer of the main form
    /// so a mouse click / hover can target the same cells the user
    /// sees. Each entry is `(focused_field_index, rect)`. Cleared and
    /// repopulated every frame; empty while an overlay (sandbox config,
    /// tool config, worktree config, loading, dir picker, etc.) is up,
    /// so a stray click during one of those modes won't snap focus to
    /// the underlying main-form field that used to sit there.
    pub(super) focusable_rects: Vec<(usize, ratatui::layout::Rect)>,
    /// Rects for the sandbox-config overlay, keyed by
    /// `sandbox_focused_field`. Populated only when that overlay is up.
    pub(super) sandbox_config_rects: Vec<(usize, ratatui::layout::Rect)>,
    /// Rects for the tool-config overlay, keyed by
    /// `tool_config_focused_field`.
    pub(super) tool_config_rects: Vec<(usize, ratatui::layout::Rect)>,
    /// Rects for the worktree-config overlay, keyed by
    /// `worktree_config_focused_field`.
    pub(super) worktree_config_rects: Vec<(usize, ratatui::layout::Rect)>,
}

/// Shared logic for handling key events in an editable list (env keys or env values).
fn handle_editable_list_key(
    key: KeyEvent,
    items: &mut Vec<String>,
    expanded: &mut bool,
    selected_index: &mut usize,
    editing_input: &mut Option<Input>,
    adding_new: &mut bool,
    validate: impl Fn(&str, &[String]) -> bool,
) -> DialogResult<NewSessionData> {
    // Handle text input mode (editing or adding)
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

        // Load resolved config (global + profile + repo overrides from cwd)
        let config = crate::session::repo_config::resolve_config_with_repo_or_warn(
            profile,
            std::path::Path::new(&current_dir),
        );

        // Determine default tool index based on config
        let tool_index = if let Some(ref default_tool) = config.session.default_tool {
            available_tools
                .iter()
                .position(|t| t == default_tool)
                .unwrap_or(0)
        } else {
            0
        };

        // Apply sandbox defaults from config (disabled for host-only agents like settl)
        let is_default_tool_host_only = available_tools
            .get(tool_index)
            .and_then(|t| crate::agents::get_agent(t))
            .is_some_and(|a| a.host_only);
        let sandbox_enabled =
            docker_available && config.sandbox.enabled_by_default && !is_default_tool_host_only;
        let worktree_enabled = config.worktree.enabled && !is_default_tool_host_only;
        let yolo_mode = config.session.yolo_mode_default;

        // Load extra args and command override for the default tool
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

        // Initialize env entries and inherited settings from config when sandbox is enabled
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

        // Resolve each profile's description from its on-disk config. Failures
        // fall through to None so a corrupted profile config does not break
        // the new-session picker.
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
            sandbox_image: Input::new(
                containers::get_container_runtime().effective_default_image(),
            ),
            docker_available,
            yolo_mode,
            yolo_mode_default: yolo_mode,
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

    /// Pre-fill the path field (e.g. from a selected session).
    pub fn set_path(&mut self, path: String) {
        self.path = Input::new(path);
        if !self.extra_env_overridden {
            self.reload_config_defaults();
        }
    }

    /// Pre-fill the group field (e.g. from a selected session or group).
    pub fn set_group(&mut self, group: String) {
        self.group = Input::new(group);
    }

    /// Pre-fill the title field (e.g. a "(fork)" suffix when forking).
    pub fn set_title(&mut self, title: String) {
        self.title = Input::new(title);
    }

    /// Seed this dialog as a fork (carried into the resulting NewSessionData).
    pub fn set_fork_from(&mut self, seed: crate::session::ForkSeed) {
        self.fork_seed = Some(seed);
    }

    /// Preselect a specific tool by name (e.g. so a fork opens on the parent's
    /// agent rather than the configured default, matching the fork seed). No-op
    /// when the tool isn't in the available list. Applies the same per-tool side
    /// effects as cycling the tool field: always-yolo agents force the toggle,
    /// host-only agents clear sandbox and worktree, and the per-tool extra
    /// args / command override are re-resolved.
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

    /// Move focus to the title field. Used by "new from selection", where the
    /// path is pre-filled so the user lands directly on naming the session.
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

    #[cfg(test)]
    pub fn selected_tool(&self) -> &str {
        self.available_tools
            .get(self.tool_index)
            .map(|s| s.as_str())
            .unwrap_or("")
    }

    /// Push a hook progress message into the dialog state
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

    /// Set the dialog to loading state
    pub fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
        if loading {
            self.error_message = None;
        }
    }

    /// Check if the dialog is in loading state
    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Advance dialog timers (spinner and transient highlights).
    /// Returns true when visual state changed and the UI should redraw.
    pub fn tick(&mut self) -> bool {
        let mut changed = false;

        if self.loading {
            // Spinner frame is computed from elapsed time by rattles,
            // so we just need to trigger a redraw
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

    /// Description for the currently selected profile, if one was configured.
    /// Indexed in lockstep with `available_profiles`; an out-of-bounds index
    /// (shouldn't happen in practice) yields `None` so the picker degrades
    /// gracefully instead of panicking.
    pub(super) fn selected_profile_description(&self) -> Option<&str> {
        self.profile_descriptions
            .get(self.profile_index)
            .and_then(|d| d.as_deref())
    }

    pub(super) fn has_profile_selection(&self) -> bool {
        self.available_profiles.len() > 1
    }

    fn resolve_config_for_path(&self, profile: &str) -> crate::session::Config {
        let path = self.path.value().trim();
        if path.is_empty() {
            resolve_config_or_warn(profile)
        } else {
            crate::session::repo_config::resolve_config_with_repo_or_warn(
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

    /// Whether the currently selected tool is always in YOLO mode (no opt-in needed).
    fn selected_tool_always_yolo(&self) -> bool {
        let tool_name = &self.available_tools[self.tool_index];
        crate::agents::get_agent(tool_name)
            .and_then(|a| a.yolo.as_ref())
            .is_some_and(|y| matches!(y, crate::agents::YoloMode::AlwaysYolo))
    }

    /// Whether the currently selected tool can only run on the host (no sandbox/worktree).
    fn selected_tool_host_only(&self) -> bool {
        let tool_name = &self.available_tools[self.tool_index];
        crate::agents::get_agent(tool_name).is_some_and(|a| a.host_only)
    }

    /// The field index of the path field. Path comes BEFORE title in the
    /// dialog so the user picks the working directory before naming the
    /// session. Shifts based on whether the profile picker is visible at
    /// field 0.
    fn path_field(&self) -> usize {
        if self.has_profile_selection() {
            1
        } else {
            0
        }
    }

    /// The field index of the title field. Title sits one slot AFTER path.
    fn title_field(&self) -> usize {
        if self.has_profile_selection() {
            2
        } else {
            1
        }
    }

    /// Re-resolve config defaults when the profile changes.
    /// Resets tool, yolo, sandbox, and env settings but preserves user inputs
    /// (title, path, group, worktree).
    fn reload_config_defaults(&mut self) {
        let profile = self.selected_profile().to_string();
        self.profile = profile.clone();
        let config = self.resolve_config_for_path(&profile);

        // Reset tool index
        self.tool_index = if let Some(ref default_tool) = config.session.default_tool {
            self.available_tools
                .iter()
                .position(|t| t == default_tool)
                .unwrap_or(0)
        } else {
            0
        };

        // Reset sandbox/yolo defaults
        self.yolo_mode_default = config.session.yolo_mode_default;
        self.yolo_mode = self.yolo_mode_default;
        self.sandbox_enabled = self.docker_available
            && config.sandbox.enabled_by_default
            && !self.selected_tool_host_only();
        self.worktree_enabled = config.worktree.enabled && !self.selected_tool_host_only();

        // Reset sandbox image from resolved config (includes profile overrides)
        self.sandbox_image = Input::new(config.sandbox.default_image.clone());

        // Reset env entries and inherited settings
        if self.sandbox_enabled {
            self.extra_env = config.sandbox.environment.clone();
            self.inherited_settings = build_inherited_settings(&config.sandbox);
        } else {
            self.extra_env.clear();
            self.inherited_settings.clear();
        }
        self.extra_env_overridden = false;

        // Reset extra args and command override for new default tool
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
        self.tool_config_mode = false;
        self.tool_config_focused_field = 0;

        // Reset expanded states
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

/// Label shown in the registered-projects picker. Includes scope so users
/// can disambiguate when the same name exists across scopes (rare; the
/// merger dedupes by path, not name).
pub(crate) fn project_picker_label(p: &crate::session::Project) -> String {
    format!("{} [{}]  {}", p.name, p.scope.as_str(), p.path)
}

impl NewSessionDialog {
    /// Route a left-click on the main form. Returns `Some(Continue)`
    /// when the click landed on a focusable field (focus moves and,
    /// for checkbox / cycler rows, the value advances the same way
    /// Space / Left+Right would). Returns `None` when the click missed
    /// every captured rect or when the dialog is in an overlay mode
    /// where the main-form rects don't apply (the renderer leaves
    /// `focusable_rects` empty in those modes).
    ///
    /// The overlay modes (sandbox/tool/worktree config, dir picker,
    /// group/branch/projects picker, help) are still keyboard-only;
    /// `handle_click` only fires on the top-level form.
    pub fn handle_click(&mut self, col: u16, row: u16) -> Option<DialogResult<NewSessionData>> {
        let pos = ratatui::layout::Position::from((col, row));

        // List pickers (group / branch / projects) float over every
        // other surface (main form AND config overlays), so route their
        // clicks first. Otherwise the worktree config overlay below
        // would swallow clicks meant for the branch picker that pops
        // up from inside it.
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
                    self.worktree_branch = Input::new(value);
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
                    // Mirror the Enter-key handler in the worktree-config
                    // path: resolve the display name back to a project
                    // path via `available_projects` and append to
                    // `workspace_repos` if not already present.
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

        // Config overlays (sandbox / tool / worktree) take precedence
        // over the main form: their rects are populated only while
        // their mode is active.
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
                // Field index 1 is the new-branch checkbox in the
                // worktree-config overlay; a click toggles it like Space
                // would on the keyboard.
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
        self.focused_field = hit_field;
        self.activate_focused_field();
        Some(DialogResult::Continue)
    }

    /// Hover only updates the active picker's row highlight (menu-style
    /// behavior the user expects). It deliberately does NOT move focus
    /// between form fields; main form or any of the config overlays.
    /// Stealing focus from the field the user is typing into just
    /// because the mouse cursor drifts across the dialog is jarring.
    /// Click still sets focus.
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

    /// Perform the focused field's primary action: toggle a checkbox,
    /// cycle a picker. Text fields (path, title, group) have no primary
    /// action and are left alone. Mirrors the per-field branches of
    /// `handle_key`'s Space / Left / Right handlers so a click produces
    /// byte-identical state changes.
    fn activate_focused_field(&mut self) {
        let has_profile_selection = self.available_profiles.len() > 1;
        let has_tool_selection = self.available_tools.len() > 1;
        let is_host_only = self.selected_tool_host_only();
        let has_sandbox = self.docker_available && !is_host_only;
        let has_yolo = !self.selected_tool_always_yolo();
        let profile_field = if has_profile_selection { 0 } else { usize::MAX };
        let mut fi = if has_profile_selection { 1 } else { 0 };
        fi += 2; // title + path
        let tool_field = if has_tool_selection {
            let f = fi;
            fi += 1;
            f
        } else {
            usize::MAX
        };
        let yolo_mode_field = if has_yolo {
            let f = fi;
            fi += 1;
            f
        } else {
            usize::MAX
        };
        let worktree_field = if !is_host_only {
            let f = fi;
            fi += 1;
            f
        } else {
            usize::MAX
        };
        let sandbox_field = if has_sandbox {
            // No `fi += 1` here; sandbox is the last index we need to
            // resolve, and the unused assignment trips clippy.
            fi
        } else {
            usize::MAX
        };

        if self.focused_field == profile_field {
            if self.available_profiles.len() > 1 {
                self.profile_index = (self.profile_index + 1) % self.available_profiles.len();
                // Mirror the keyboard cycle: pick up the new profile's
                // defaults (sandbox, yolo, hooks, tool override) so the
                // dialog reflects what a submit would actually create.
                self.reload_config_defaults();
            }
        } else if self.focused_field == tool_field {
            if self.available_tools.len() > 1 {
                self.tool_index = (self.tool_index + 1) % self.available_tools.len();
                // Same side effects the Space/Left/Right tool handler
                // applies: always-yolo tools force the toggle, host-only
                // tools clear sandbox + worktree, and the per-tool config
                // overlay (and its branch override) get re-resolved.
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
        } else if self.focused_field == yolo_mode_field {
            self.yolo_mode = !self.yolo_mode;
        } else if self.focused_field == worktree_field {
            // Mirror the keyboard handler: worktree and scratch are
            // mutually exclusive, so a click on the worktree row while
            // scratch is on surfaces an inline hint rather than
            // silently toggling.
            if self.scratch {
                self.error_message = Some(
                    "Worktree is disabled in scratch mode. Press Ctrl+T to leave scratch first."
                        .to_string(),
                );
            } else {
                self.worktree_enabled = !self.worktree_enabled;
                if !self.worktree_enabled {
                    self.worktree_config_mode = false;
                }
            }
        } else if self.focused_field == sandbox_field {
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
        // Path / Title / Group: focus change only, no toggle action.
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<NewSessionData> {
        // When loading, only allow Esc to cancel
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

        // Delegate to sandbox config mode handler when active
        if self.sandbox_config_mode {
            return self.handle_sandbox_config_key(key);
        }

        // Delegate to tool config mode handler when active
        if self.tool_config_mode {
            return self.handle_tool_config_key(key);
        }

        // Delegate to worktree config mode handler when active
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
                self.worktree_branch = Input::new(value);
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

        // Ctrl+T toggles the scratch-session mode from anywhere in the
        // form. Mutually exclusive with worktrees AND with extra-repo
        // workspaces; turning on scratch clears all three so the
        // submit payload mirrors the user's intent if they toggle
        // scratch off again later.
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

        let has_profile_selection = self.available_profiles.len() > 1;
        let has_tool_selection = self.available_tools.len() > 1;
        let is_host_only = self.selected_tool_host_only();
        let has_sandbox = self.docker_available && !is_host_only;
        let has_yolo = !self.selected_tool_always_yolo();
        // Field order: [profile], path, title, [tool], [yolo], worktree, [sandbox], group
        // Worktree sub-options (new_branch, extra_repos) are in a Ctrl+P overlay.
        // Tool config (extra_args, command_override) is in a Ctrl+P overlay on tool field.
        // Sandbox sub-options are in a separate sandbox_config_mode overlay.
        let profile_field = if has_profile_selection { 0 } else { usize::MAX };
        let mut fi = if has_profile_selection { 1 } else { 0 }; // next field index
        fi += 2; // title + path
        let tool_field = if has_tool_selection {
            let f = fi;
            fi += 1;
            f
        } else {
            usize::MAX
        };
        let yolo_mode_field = if has_yolo {
            let f = fi;
            fi += 1;
            f
        } else {
            usize::MAX
        };
        let worktree_field = if !is_host_only {
            let f = fi;
            fi += 1;
            f
        } else {
            usize::MAX
        };
        let sandbox_field = if has_sandbox {
            let f = fi;
            fi += 1;
            f
        } else {
            usize::MAX
        };
        let group_field = fi;
        fi += 1;
        let max_field = fi;

        // Ctrl+P opens a context-sensitive picker/config overlay
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
            if self.focused_field == tool_field {
                self.tool_config_mode = true;
                self.tool_config_focused_field = 0;
                return DialogResult::Continue;
            }
            if self.focused_field == group_field && !self.existing_groups.is_empty() {
                self.group_picker.activate(self.existing_groups.clone());
                return DialogResult::Continue;
            }
            if self.focused_field == worktree_field {
                self.worktree_config_mode = true;
                self.worktree_config_focused_field = 0;
                return DialogResult::Continue;
            }
            if self.focused_field == sandbox_field && self.sandbox_enabled {
                self.refresh_inherited_sandbox_settings();
                self.sandbox_config_mode = true;
                self.sandbox_focused_field = 0;
                return DialogResult::Continue;
            }
        }

        if self.handle_path_shortcuts(key) {
            return DialogResult::Continue;
        }

        if self.handle_group_shortcuts(key, group_field) {
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
                // Scratch sessions skip the path-existence check: the
                // server (or `aoe add` CLI) provisions the scratch dir on
                // submit.
                if !self.scratch {
                    let path_str = self.path.value().trim().to_string();
                    let resolved = path_input::expand_tilde(&path_str);
                    if !std::path::Path::new(&resolved).exists() {
                        self.confirm_create_dir = Some(false);
                        return DialogResult::Continue;
                    }
                }
                self.build_submit_result()
            }
            KeyCode::Tab | KeyCode::Down => {
                if self.focused_field == self.path_field() {
                    self.clear_path_ghost();
                }
                if self.focused_field == group_field {
                    self.clear_group_ghost();
                }
                self.focused_field = (self.focused_field + 1) % max_field;
                if self.focused_field == self.path_field() {
                    self.recompute_path_ghost();
                }
                if self.focused_field == group_field {
                    self.recompute_group_ghost();
                }
                DialogResult::Continue
            }
            KeyCode::BackTab | KeyCode::Up => {
                if self.focused_field == self.path_field() {
                    self.clear_path_ghost();
                }
                if self.focused_field == group_field {
                    self.clear_group_ghost();
                }
                self.focused_field = if self.focused_field == 0 {
                    max_field - 1
                } else {
                    self.focused_field - 1
                };
                if self.focused_field == self.path_field() {
                    self.recompute_path_ghost();
                }
                if self.focused_field == group_field {
                    self.recompute_group_ghost();
                }
                DialogResult::Continue
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if self.focused_field == profile_field =>
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
                if self.focused_field == tool_field =>
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
                if self.focused_field == worktree_field =>
            {
                // Scratch and worktree are mutually exclusive. Without this
                // guard the user could Ctrl+T (scratch on, worktree forced
                // off) then Space the worktree row back on, and submit a
                // payload the server rejects with 400. Surface the conflict
                // here instead of trusting the server-side mutex to catch
                // every UI path.
                if self.scratch {
                    self.error_message = Some(
                        "Worktree is disabled in scratch mode. Press Ctrl+T to leave scratch first.".to_string(),
                    );
                    return DialogResult::Continue;
                }
                self.worktree_enabled = !self.worktree_enabled;
                if !self.worktree_enabled {
                    self.worktree_config_mode = false;
                }
                DialogResult::Continue
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if self.focused_field == sandbox_field =>
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
                if self.focused_field == yolo_mode_field =>
            {
                self.yolo_mode = !self.yolo_mode;
                DialogResult::Continue
            }
            _ => {
                if self.focused_field != profile_field
                    && self.focused_field != tool_field
                    && self.focused_field != worktree_field
                    && self.focused_field != sandbox_field
                    && self.focused_field != yolo_mode_field
                {
                    self.current_input_mut()
                        .handle_event(&crossterm::event::Event::Key(key));
                    self.error_message = None;
                    if self.focused_field == self.path_field() {
                        self.path_invalid_flash_until = None;
                        self.recompute_path_ghost();
                    }
                    if self.focused_field == group_field {
                        self.recompute_group_ghost();
                    }
                }
                DialogResult::Continue
            }
        }
    }

    /// Handle key events when in sandbox configuration mode.
    fn handle_sandbox_config_key(&mut self, key: KeyEvent) -> DialogResult<NewSessionData> {
        // Sandbox config fields: 0=image, 1=env (inherited is always-visible, not focusable)
        const SANDBOX_IMAGE: usize = 0;
        const SANDBOX_ENV: usize = 1;
        const SANDBOX_MAX: usize = 2;

        // Handle env list editing when expanded
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
                // Text input for image field only
                if self.sandbox_focused_field == SANDBOX_IMAGE {
                    self.sandbox_image
                        .handle_event(&crossterm::event::Event::Key(key));
                }
                DialogResult::Continue
            }
        }
    }

    /// Handle key events when in tool configuration mode. `?` opens the help
    /// overlay (new-session only); everything else delegates to the shared
    /// tool-config component.
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

    /// Resolve a label chosen from the project picker back to the underlying
    /// project entry and append its path to the workspace repos list.
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

    /// Build and activate the registered-projects picker (Ctrl+R on the
    /// extra-repos field). Filters out the primary repo and any paths already
    /// in the workspace_repos list to avoid the builder's duplicate-name guard.
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

    /// Handle key events when in worktree configuration mode.
    fn handle_worktree_config_key(&mut self, key: KeyEvent) -> DialogResult<NewSessionData> {
        // Worktree config fields: 0=name, 1=new_branch checkbox, 2=extra_repos list
        const WT_NAME: usize = 0;
        const WT_NEW_BRANCH: usize = 1;
        const WT_BASE_BRANCH: usize = 2;
        const WT_EXTRA_REPOS: usize = 3;
        const WT_MAX: usize = 4;

        if self.branch_picker.is_active() {
            if let ListPickerResult::Selected(value) = self.branch_picker.handle_key(key) {
                if self.worktree_config_focused_field == WT_BASE_BRANCH {
                    self.base_branch = Input::new(value);
                } else {
                    self.worktree_branch = Input::new(value);
                }
            }
            return DialogResult::Continue;
        }

        if self.projects_picker.is_active() {
            if let ListPickerResult::Selected(value) = self.projects_picker.handle_key(key) {
                self.apply_picked_project(&value);
            }
            return DialogResult::Continue;
        }

        // Handle workspace repos list editing when expanded
        if self.workspace_repos_expanded && self.worktree_config_focused_field == WT_EXTRA_REPOS {
            return self.handle_workspace_repos_list_key(key);
        }

        match key.code {
            KeyCode::Esc => {
                self.worktree_config_mode = false;
                DialogResult::Continue
            }
            KeyCode::Char('?') => {
                self.show_help = true;
                DialogResult::Continue
            }
            // Ctrl+P on name field opens branch picker
            KeyCode::Char('p')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.worktree_config_focused_field == WT_NAME =>
            {
                let path = std::path::Path::new(self.path.value().trim());
                if let Ok(branches) = crate::git::diff::list_branches(path) {
                    if !branches.is_empty() {
                        self.branch_picker.activate(branches);
                    }
                }
                DialogResult::Continue
            }
            // Ctrl+P on base-branch field opens the branch picker too.
            // Selection routes through `branch_picker` like WT_NAME, so we
            // disambiguate after selection by checking the focused field.
            // See `branch_picker` handling at the top of this function.
            KeyCode::Char('p')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.worktree_config_focused_field == WT_BASE_BRANCH =>
            {
                let path = std::path::Path::new(self.path.value().trim());
                if let Ok(branches) = crate::git::diff::list_branches(path) {
                    if !branches.is_empty() {
                        self.branch_picker.activate(branches);
                    }
                }
                DialogResult::Continue
            }
            // Ctrl+R on extra_repos field opens the registered-projects picker.
            // Selection appends the project's path to the workspace_repos list.
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
            _ if self.worktree_config_focused_field == WT_BASE_BRANCH => {
                self.base_branch
                    .handle_event(&crossterm::event::Event::Key(key));
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    /// Handle key events when the env list is expanded
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

        // Validate the current entry if the list changed
        if self.extra_env != snapshot {
            self.extra_env_overridden = true;
            self.error_message = self
                .extra_env
                .get(self.env_selected_index)
                .and_then(|entry| crate::session::validate_env_entry(entry));
        }

        result
    }

    /// Handle key events when the workspace repos list is expanded
    fn handle_workspace_repos_list_key(&mut self, key: KeyEvent) -> DialogResult<NewSessionData> {
        // When actively editing a repo path, handle path-specific keys first
        if self.workspace_repo_editing_input.is_some() {
            // Ctrl+P: open dir picker for repo path
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

            // Right/End at end of input: accept ghost text
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

        // Intercept 'a' to pre-populate with the expanded cwd (like the main path field)
        if self.workspace_repo_editing_input.is_none()
            && key.code == KeyCode::Char('a')
            && key.modifiers == KeyModifiers::NONE
        {
            let cwd = std::env::current_dir()
                .map(|p| {
                    let mut s = path_input::collapse_tilde(&p.to_string_lossy());
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

        // Wrap the generic handler to add tilde expansion and ghost recomputation
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

        // If editing just finished (Enter pressed), expand tilde in the stored value
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

        // If still editing, recompute ghost
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
    }

    fn current_input_mut(&mut self) -> &mut Input {
        let has_tool_selection = self.available_tools.len() > 1;
        let has_yolo = !self.selected_tool_always_yolo();
        let base = if self.has_profile_selection() { 1 } else { 0 };

        let is_host_only = self.selected_tool_host_only();
        // Field layout: [profile], title, path, [tool], [yolo], [worktree], [sandbox], group
        let mut fi = base + 2 + if has_tool_selection { 1 } else { 0 };
        if has_yolo {
            fi += 1;
        }
        if !is_host_only {
            fi += 1; // worktree checkbox
        }
        if self.docker_available && !is_host_only {
            fi += 1; // sandbox checkbox
        }
        let group_field = fi;

        let path_field = self.path_field();
        let title_field = self.title_field();
        match self.focused_field {
            n if n == title_field => &mut self.title,
            n if n == path_field => &mut self.path,
            n if n == group_field => &mut self.group,
            _ => &mut self.title,
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        // Route to the active sub-mode input if one is open
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
            // Scratch sessions send an empty path; the server / `aoe add`
            // CLI provisions a fresh scratch directory keyed on the instance id.
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
    let mut cfg = match load_config() {
        Ok(Some(c)) => c,
        Ok(None) => Default::default(),
        Err(e) => {
            tracing::warn!(target: "tui.dialog", "Failed to load config for last_browse_dir: {}", e);
            return;
        }
    };
    cfg.app_state.last_browse_dir = Some(dir);
    if let Err(e) = save_config(&cfg) {
        tracing::warn!(target: "tui.dialog", "Failed to save last_browse_dir: {}", e);
    }
}
