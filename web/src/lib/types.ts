import type { RateLimitInfo } from "./acpTypes";
import type { RepoColor } from "./repoAppearance";
import type { AgentLifecycleInfo } from "./agentProfiles";

export interface SessionResponse {
  id: string;
  title: string;
  project_path: string;
  /** Host path of the session's managed artifact directory. */
  artifact_dir: string;
  group_path: string;
  tool: string;
  status: SessionStatus;
  /** Worker auto-stopped for inactivity (resumable), unlike a deliberate Stop. */
  dormant: boolean;
  yolo_mode: boolean;
  created_at: string;
  last_accessed_at: string | null;
  /** Last transition into Idle; unlike `last_accessed_at`, viewing does not bump it. */
  idle_entered_at: string | null;
  last_error: string | null;
  branch: string | null;
  main_repo_path: string | null;
  /** Base branch of an AoE-created worktree; null for pre-existing or default branches. */
  base_branch?: string | null;
  /** Per-session diff base override. */
  base_branch_override?: string | null;
  is_sandboxed: boolean;
  /** True when the session was created in scratch mode (`aoe add
   *  --scratch` or the wizard toggle). The `project_path` points
   *  at an auto-provisioned directory under `<app_dir>/scratch/<id>/`,
   *  and the deletion path removes it (unless the user opts in to
   *  keeping the directory). The wizard's Recent-projects list filters
   *  scratch sessions out. */
  scratch: boolean;
  favorited: boolean;
  color?: string | null;
  urgent?: boolean;
  /** Web pin time; `pinned_at != null` is the pinned state. */
  pinned_at?: string | null;
  archived_at?: string | null;
  /** Null once the snooze expires, so any non-null value is an active snooze. */
  snoozed_until?: string | null;
  trashed_at?: string | null;
  /** Needs attention (unreviewed finished turn or manual flag). */
  unread?: boolean;
  has_managed_worktree: boolean;
  /** Delete has managed worktree state to clean up (single-repo or workspace). */
  has_cleanable_worktree?: boolean;
  /** Renaming also moves the worktree directory. */
  tie_workdir_to_name?: boolean;
  has_terminal: boolean;
  profile: string;
  cleanup_defaults: CleanupDefaults;
  remote_owner: string | null;
  /** "owner@host", so same-named owners on different hosts never merge; null when `remote_owner` is null. */
  remote_owner_key: string | null;
  /** null inherits the server default. */
  notify_on_waiting: boolean | null;
  notify_on_idle: boolean | null;
  notify_on_error: boolean | null;
  view?: "structured" | "terminal";
  /** Absent on older daemons. */
  context_resume?: ContextResumeAvailability;
  acp_worker_state?: AcpWorkerState;
  /** The provider rate limit the daemon has this session parked on. */
  rate_limit?: RateLimitInfo;
  rate_limit_auto_resume?: boolean;
  smart_rename?: "inactive" | "pending" | "running";
  /** Still carries its auto-generated name (more reliable than `smart_rename`). */
  default_name?: boolean;
  acp_capable?: boolean;
  /** Captured ACP session id; required for a structured fork. */
  acp_session_id?: string;
  /** Resolved ACP registry key, used as the current agent before any `AgentSwitched`. */
  acp_agent?: string;
  /** Switching views preserves the conversation (server-computed). */
  keeps_context?: boolean;
  /** Slash commands that reset the conversation for this agent. */
  clear_aliases?: string[];
  /** The agent implements ACP `session/fork` (resume-only agents do not). */
  acp_can_fork?: boolean;
  /** Claude's fullscreen renderer is on, so mobile skips copy-mode scrollback workarounds. */
  claude_fullscreen: boolean;
  workspace_repos: WorkspaceRepoSummary[];
  /** Only on the create-session response. */
  warnings?: string[];
  plan_summary?: PlanSummary;
  /** When the agent's pending `ScheduleWakeup` fires. */
  next_wakeup_at?: string;
  next_wakeup_reason?: string;
  monitor_active?: boolean;
  monitor_description?: string;
  /** One-line explanation when a lifecycle action took a non-default path,
   *  e.g. a start that fell back to a fresh conversation because the stored
   *  one could not be resumed. Present only on the start/ensure responses
   *  that carry it; absent on regular session fetches. */
  message?: string;
}

export interface PlanSummary {
  /** First non-completed step's title, truncated server-side. */
  current_step_title: string | null;
  completed: number;
  total: number;
}

export interface WorkspaceRepoSummary {
  name: string;
  source_path: string;
  branch: string;
}

export interface CleanupDefaults {
  delete_worktree: boolean;
  delete_branch: boolean;
  delete_sandbox: boolean;
  delete_to_trash: boolean;
}

export type SessionStatus =
  | "Running"
  | "Waiting"
  | "Idle"
  | "Error"
  | "Starting"
  | "Stopped"
  | "Unknown"
  | "Deleting"
  | "Creating";

export type ContextResumeUnavailableReason =
  | "agent_unsupported"
  | "sandbox_unsupported"
  | "command_unsupported"
  | "forced_fresh"
  | "invalid_target"
  | "fork_pending"
  | "previous_failure"
  | "no_target";

export type ContextResumeIndeterminateReason = "runtime_check_required" | "agent_handshake_required";

export type ContextResumeAvailability =
  | { state: "available" }
  | { state: "indeterminate"; reason: ContextResumeIndeterminateReason }
  | { state: "unavailable"; reason: ContextResumeUnavailableReason };

export interface ResizeMessage {
  type: "resize";
  cols: number;
  rows: number;
}

export interface ActivateMessage {
  type: "activate";
}

/** Explicit size-lock take-over; `activate` also fires on mount and must not steal the lock. */
export interface ClaimMessage {
  type: "claim";
}

/** SIGSTOP the pane's foreground process while mobile reads scrollback. */
export interface PauseOutputMessage {
  type: "pause_output";
}

export interface ResumeOutputMessage {
  type: "resume_output";
}

export interface PrimaryStatusMessage {
  type: "primary_status";
  is_primary: boolean;
}

/** Latency probe for `?debug=terminal-timing`; never touches the PTY. */
export interface TimingPingMessage {
  type: "timing_ping";
  seq: number;
  client_t: number;
}

/** `server_busy_us` lets the client subtract server time without clock sync. */
export interface TimingPongMessage {
  type: "timing_pong";
  seq: number;
  client_t: number;
  server_busy_us: number;
}

export interface RichDiffFile {
  path: string;
  old_path: string | null;
  status: "added" | "modified" | "deleted" | "renamed" | "copied" | "untracked" | "conflicted" | "unchanged";
  additions: number;
  deletions: number;
  /** Omitted for single-repo sessions. */
  repo_name?: string;
}

export interface RepoBase {
  /** Omitted for single-repo sessions. */
  repo_name?: string;
  base_branch: string;
  /** Worktree path this repo's diff was computed in. */
  repo_path: string;
  base_override?: string;
}

export interface RichDiffFilesResponse {
  files: RichDiffFile[];
  /** One entry per repo; workspace members can have different defaults. */
  per_repo_bases: RepoBase[];
  warning: string | null;
}

export interface RichDiffLine {
  type: "add" | "delete" | "equal";
  old_line_num: number | null;
  new_line_num: number | null;
  content: string;
}

export interface RichDiffHunk {
  old_start: number;
  old_lines: number;
  new_start: number;
  new_lines: number;
  lines: RichDiffLine[];
}

export interface RichFileContentsResponse {
  file: RichDiffFile;
  old_content: string;
  new_content: string;
  /** Server-computed unified diff; empty for binary files. */
  patch: string;
  is_binary: boolean;
  /** Too large to send inline; contents are empty. */
  truncated: boolean;
}

export type WorkspaceStatus = "active" | "idle";

export interface RepoGroup {
  id: string;
  repoPath: string;
  displayName: string;
  defaultDisplayName: string;
  alias: string | null;
  color: RepoColor | null;
  remoteOwner: string | null;
  /** "owner@host"; null when `remoteOwner` is null. */
  remoteOwnerKey: string | null;
  workspaces: Workspace[];
  status: WorkspaceStatus;
  collapsed: boolean;
  /** Registry entries for this repo path; several when registered in multiple scopes. Entries without workspaces make a pinned-but-empty project. */
  registeredProjects: ProjectInfo[];
}

export interface Workspace {
  id: string;
  branch: string | null;
  projectPath: string;
  displayName: string;
  agents: string[];
  primaryAgent: string;
  status: WorkspaceStatus;
  sessions: SessionResponse[];
}

export interface AgentInfo {
  name: string;
  kind: "builtin" | "custom";
  binary: string;
  host_only: boolean;
  installed: boolean;
  install_hint: string;
  /** Has a one-shot mode for smart rename. Always false for custom agents. */
  oneshot_capable?: boolean;
  acp_capable: boolean;
  /** The ACP adapter binary is resolvable on the host. */
  acp_installed: boolean;
  /** Allowed by `[acp] allowed_agents`; `acp_capable` stays the intrinsic fact. Absent means permitted. */
  acp_allowed?: boolean;
  /** Built-in ACP launch command after `${aoe_data_dir}` substitution; absent for custom agents. */
  acp_command?: string;
  acp_args?: string[];
  /** Omitted for Active agents. Mirrors `AgentLifecycle` in src/agents.rs. */
  lifecycle?: AgentLifecycleInfo;
}

export interface ProfileInfo {
  name: string;
  is_default: boolean;
  description?: string;
}

/** Mirrors Rust HooksConfigOverride: undefined inherits, any array (even empty) overrides. Read-only on the dashboard. */
export interface HooksOverride {
  on_create?: string[];
  on_launch?: string[];
  on_destroy?: string[];
}

/** Serialized ProfileConfig; only the fields the dashboard reads are typed. */
export interface ProfileSettingsResponse {
  description?: string | null;
  hooks?: HooksOverride;
  [key: string]: unknown;
}

export interface DirEntry {
  name: string;
  path: string;
  is_dir: boolean;
  is_git_repo: boolean;
}

export interface BrowseResponse {
  entries: DirEntry[];
  has_more: boolean;
}

export interface GroupInfo {
  path: string;
  session_count: number;
}

export interface ProjectOverrides {
  worktree_enabled?: boolean;
  smart_rename?: boolean;
}

export interface ProjectInfo {
  name: string;
  path: string;
  scope: "global" | "profile";
  default_base_branch?: string;
  /** Absent keys inherit the configured default. */
  overrides?: ProjectOverrides;
  /** Shown as a sessionless sidebar header. */
  pinned: boolean;
}

export interface DockerStatusResponse {
  available: boolean;
  runtime: string | null;
}

export interface CreateSessionRequest {
  title?: string;
  path: string;
  tool: string;
  group?: string;
  yolo_mode?: boolean;
  /** Worktree mode without an explicit branch; the server derives it from the title. */
  worktree_enabled?: boolean;
  worktree_branch?: string;
  create_new_branch?: boolean;
  /** Only honored when `create_new_branch` is true; empty means repo default. */
  base_branch?: string;
  sandbox?: boolean;
  extra_args?: string;
  sandbox_image?: string;
  extra_env?: string[];
  extra_repo_paths?: string[];
  /** Per-repo base (`repo` is a directory name or path); outranks `base_branch`. */
  repo_bases?: { repo: string; base_branch: string }[];
  command_override?: string;
  custom_instruction?: string;
  profile?: string;
  view?: "structured" | "terminal";
  agent_model?: string;
  agent_effort?: string;
  acp_effort?: string;
  /** Server provisions a scratch directory and ignores `path`; exclusive with worktrees and extra repos. */
  scratch?: boolean;
  /** Approve repo lifecycle hooks, like CLI `--trust-hooks`. */
  trust_hooks?: boolean;
  /** Claude Code session id to import; `path` must be its original cwd. */
  import_acp_session_id?: string;
  /** Agent or ACP session id to fork from. */
  fork_from?: string;
}

export interface ClaudeSessionSummary {
  session_id: string;
  cwd: string;
  title: string | null;
  last_modified_ms: number;
  cwd_exists: boolean;
}

export type AcpWorkerState = "absent" | "resuming" | "running" | "stopping";

// Settings schema mirrors `crate::session::config::settings_schema` (`GET /api/settings/schema`).

/** `value` is written to disk; `label` is shown. */
export interface SettingsSelectOption {
  value: string;
  label: string;
}

export type SettingsWidget =
  | { kind: "toggle" }
  | { kind: "text"; multiline?: boolean; mono?: boolean }
  | { kind: "optional_text"; mono?: boolean }
  | { kind: "number"; min?: number; max?: number }
  | { kind: "slider"; min: number; max: number; step: number }
  | { kind: "select"; options: SettingsSelectOption[] }
  | { kind: "list" }
  | { kind: "dynamic_select"; source: SettingsOptionSource; depends_on?: string[] }
  | {
      kind: "object_list";
      id_field: string;
      fields: SettingsObjectField[];
      min_items?: number;
      max_items?: number;
    }
  | { kind: "cron" }
  /** Bespoke widget keyed by `id`. */
  | { kind: "custom"; id: string };

export type SettingsOptionSource = "acp_agents" | "acp_models" | "acp_modes" | "projects" | "groups";

export type SettingsObjectFieldWidget =
  | { kind: "toggle" }
  | { kind: "text"; multiline?: boolean; mono?: boolean }
  | { kind: "number"; min?: number; max?: number }
  | { kind: "select"; options: SettingsSelectOption[] }
  | { kind: "dynamic_select"; source: SettingsOptionSource; depends_on?: string[] }
  | { kind: "dynamic_multi_select"; source: SettingsOptionSource; depends_on?: string[] }
  | { kind: "cron" };

export interface SettingsObjectField {
  field: string;
  label: string;
  description?: string;
  required?: boolean;
  widget: SettingsObjectFieldWidget;
  validation: SettingsValidation;
  default?: unknown;
}

/** `local_only` fields are rejected by the server PATCH. */
export type SettingsWebWritePolicy =
  | { policy: "allow" }
  | { policy: "requires_elevation"; reason: string }
  | { policy: "local_only"; reason: string };

/** Server-enforced; widget min/max is advisory. */
export type SettingsValidation =
  | { rule: "none" }
  | { rule: "bool" }
  | { rule: "str" }
  | { rule: "str_list" }
  | { rule: "range_u64"; min: number; max?: number }
  | { rule: "range_i64"; min?: number; max?: number }
  | { rule: "one_of"; options: string[] }
  | { rule: "non_empty_string" }
  | { rule: "memory_limit" }
  | { rule: "volume_list" }
  | { rule: "env_list" }
  | { rule: "port_mapping_list" }
  | { rule: "capability_list" }
  | { rule: "security_opt_list" }
  | { rule: "network" }
  | { rule: "cron" }
  | {
      rule: "object_list";
      id_field: string;
      fields: SettingsObjectField[];
      min_items?: number;
      max_items?: number;
    };

/** The dotted `${section}.${field}` is its stable id. */
export interface SettingsFieldDescriptor {
  section: string;
  field: string;
  category: string;
  label: string;
  description: string;
  widget: SettingsWidget;
  web_write: SettingsWebWritePolicy;
  /** `false` means global-only. */
  profile_overridable: boolean;
  validation: SettingsValidation;
  advanced: boolean;
  /** Present only on plugin fields, which have no stored value until saved. */
  default?: unknown;
}
