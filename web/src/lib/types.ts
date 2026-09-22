import type { SessionResponse as WireSessionResponse } from "./apiWire";

// The REST contract's types are generated from the Rust ones; re-exported here
// so callers keep importing them from the module they always did.
export type {
  AcpWorkerState,
  CleanupDefaults,
  ContextResumeAvailability,
  ContextResumeIndeterminateReason,
  ContextResumeUnavailableReason,
  PlanSummary,
  WorkspaceRepoSummary,
} from "./apiWire";
import type { RepoColor } from "./repoAppearance";
import type { AgentLifecycleInfo } from "./agentProfiles";

/** One session as the daemon reports it. Generated from `src/daemon/wire.rs`
 *  into `apiWire.ts`; `status` is narrowed here to the values this dashboard
 *  renders, because the wire carries a plain string so that a client older
 *  than a status still parses the row. */
export type SessionResponse = Omit<WireSessionResponse, "status"> & {
  status: SessionStatus;
};

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

/** WebSocket control messages sent from browser to server */
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
