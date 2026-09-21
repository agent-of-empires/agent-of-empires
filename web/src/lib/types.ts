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

/** Explicit take-over of the cross-surface size lock (banner click).
 *  Separate from `activate`, which also fires on mount and must not
 *  steal the size from a live owner on another device. */
export interface ClaimMessage {
  type: "claim";
}

/** Pause the pane's foreground process (SIGSTOP). Sent by mobile web
 *  clients when entering tmux scrollback so claude's continued output
 *  doesn't shift what the user is reading. Paired with `resume_output`. */
export interface PauseOutputMessage {
  type: "pause_output";
}

export interface ResumeOutputMessage {
  type: "resume_output";
}

/** Server → client control message indicating primary status */
export interface PrimaryStatusMessage {
  type: "primary_status";
  is_primary: boolean;
}

/** Client → server latency probe, sent only under
 *  `?debug=terminal-timing`. `client_t` is a `performance.now()` stamp
 *  echoed back unchanged in the pong. Never touches the PTY. See #1453. */
export interface TimingPingMessage {
  type: "timing_ping";
  seq: number;
  client_t: number;
}

/** Server → client reply to {@link TimingPingMessage}. `server_busy_us`
 *  is the server's own recv-to-send duration, so the client can subtract
 *  it from the round trip without clock synchronisation. See #1453. */
export interface TimingPongMessage {
  type: "timing_pong";
  seq: number;
  client_t: number;
  server_busy_us: number;
}

/** Rich diff file info with addition/deletion stats */
export interface RichDiffFile {
  path: string;
  old_path: string | null;
  status: "added" | "modified" | "deleted" | "renamed" | "copied" | "untracked" | "conflicted" | "unchanged";
  additions: number;
  deletions: number;
  /** Workspace repo this file belongs to. Omitted for single-repo
   *  (non-workspace) sessions. The sidebar groups entries by this
   *  field to disambiguate path collisions across repos. See #1047. */
  repo_name?: string;
}

/** One repo's base branch in a (possibly multi-repo) session. */
export interface RepoBase {
  /** Omitted for single-repo sessions. */
  repo_name?: string;
  base_branch: string;
  /** Worktree path this repo's diff was computed in. The base picker queries
   *  it for that repo's branch list. See #3329. */
  repo_path: string;
  /** Set when this repo carries an explicit override, which is what the
   *  picker's reset affordance keys off. See #3329. */
  base_override?: string;
}

/** Response from /api/sessions/{id}/diff/files */
export interface RichDiffFilesResponse {
  files: RichDiffFile[];
  /** One entry per repo whose diff was computed. Single-repo sessions
   *  get a one-element array with `repo_name` omitted; workspace
   *  sessions get one entry per workspace member with each repo's
   *  default branch. Replaces the previous top-level `base_branch`
   *  since workspace members can have different defaults. */
  per_repo_bases: RepoBase[];
  warning: string | null;
}

/** A single line in a structured diff */
export interface RichDiffLine {
  type: "add" | "delete" | "equal";
  old_line_num: number | null;
  new_line_num: number | null;
  content: string;
}

/** A hunk in a structured diff */
export interface RichDiffHunk {
  old_start: number;
  old_lines: number;
  new_start: number;
  new_lines: number;
  lines: RichDiffLine[];
}

/**
 * Response from /api/sessions/{id}/diff/file?path=...
 * Raw old/new file text that the client parses and renders itself via
 * `@pierre/diffs` (virtualized, off-main-thread highlighting).
 */
export interface RichFileContentsResponse {
  file: RichDiffFile;
  old_content: string;
  new_content: string;
  /** Server-computed unified diff of old → new. Parsed client-side as text
   *  (no client diff algorithm); empty for binary files. */
  patch: string;
  is_binary: boolean;
  /** True if the file was too large to send inline; contents are empty. */
  truncated: boolean;
}

/** Workspace status derived from session states */
export type WorkspaceStatus = "active" | "idle";

/** Repository group: workspaces sharing the same parent repo */
export interface RepoGroup {
  id: string;
  repoPath: string;
  displayName: string;
  defaultDisplayName: string;
  alias: string | null;
  color: RepoColor | null;
  remoteOwner: string | null;
  /** Host-scoped identity for `remoteOwner` ("owner@host"); the org axis
   *  buckets repos by this instead of the bare owner. See `SessionResponse
   *  .remote_owner_key`. `null` whenever `remoteOwner` is `null`. */
  remoteOwnerKey: string | null;
  workspaces: Workspace[];
  status: WorkspaceStatus;
  collapsed: boolean;
  /** Registry entries (the "pin") for this repo path, keyed by normalized
   *  path. Empty when the repo is not pinned. More than one entry means the
   *  same path is registered under multiple scopes (global + profile); the
   *  group is rendered pinned and unpin removes every entry. A group with
   *  entries but no workspaces is a pinned-but-empty project. See #2047. */
  registeredProjects: ProjectInfo[];
}

/** Workspace: a group of sessions sharing the same project + branch */
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

/** Agent info returned by /api/agents */
export interface AgentInfo {
  name: string;
  kind: "builtin" | "custom";
  binary: string;
  host_only: boolean;
  installed: boolean;
  install_hint: string;
  /** True when the agent has a one-shot mode (so it can run the smart-rename
   *  title call). The settings smart-rename agent picker filters on this
   *  together with `installed`. Always false for custom agents. Optional so
   *  existing test fixtures need not set it; the backend always sends it. */
  oneshot_capable?: boolean;
  /** True when the agent can run in acp: a built-in with an ACP
   *  adapter, or a custom agent that declares a valid `agent_acp_cmd`.
   *  The wizard reads this to decide whether a new session runs in
   *  acp or tmux, replacing the hardcoded client-side tool list. */
  acp_capable: boolean;
  /** True when the agent's ACP adapter binary is actually resolvable on the
   *  host (not just registered). The import tab gates on this for claude. */
  acp_installed: boolean;
  /** True when `[acp] allowed_agents` permits this agent in the structured
   *  view (#3241). A separate axis from `acp_capable`, which states the
   *  intrinsic fact that an ACP adapter exists; the settings surfaces keep
   *  reading `acp_capable` so per-agent defaults stay editable for an agent
   *  that is currently off the allowlist. Optional so fixtures and older
   *  servers that omit it are treated as permitted. */
  acp_allowed?: boolean;
  /** The ACP command a built-in agent launches in acp (e.g.
   *  `claude-agent-acp`, `opencode`), post `${aoe_data_dir}`
   *  substitution. Can differ from `binary`. Absent for custom agents,
   *  whose command values are never serialized by the backend. */
  acp_command?: string;
  /** Registry args appended to `acp_command` (e.g. `["acp"]` for
   *  opencode, `["--acp"]` for gemini). Absent or empty when none. */
  acp_args?: string[];
  /** Registry lifecycle state from /api/agents. Omitted for Active agents
   *  (the common case), so fixtures and older servers read as active.
   *  Mirrors `AgentLifecycle` in src/agents.rs; the static frontend mirror
   *  lives in agentProfiles.ts (`resolveAgentLifecycle`). */
  lifecycle?: AgentLifecycleInfo;
}

/** Profile info returned by /api/profiles */
export interface ProfileInfo {
  name: string;
  is_default: boolean;
  /** Optional short description of what this profile does, surfaced as
   *  helper text in the wizard profile picker (#949). Omitted from the
   *  server payload when the profile has no description configured. */
  description?: string;
}

/** Per-profile lifecycle-hook overrides, as returned by
 *  GET /api/profiles/:name/settings. Mirrors the Rust
 *  HooksConfigOverride (src/session/config/profile_config.rs): a field that is
 *  absent/undefined means "inherit the global hooks"; an explicit array
 *  (including the empty array) means "override". Hooks are read-only on
 *  the dashboard; see HooksReadOnlyPanel and profileWritableSections. */
export interface HooksOverride {
  on_create?: string[];
  on_launch?: string[];
  on_destroy?: string[];
}

/** Shape of GET /api/profiles/:name/settings: the serialized
 *  ProfileConfig. Only the fields the dashboard reads are typed; the rest
 *  stays indexable. `hooks` is present on reads but never writable. */
export interface ProfileSettingsResponse {
  description?: string | null;
  hooks?: HooksOverride;
  [key: string]: unknown;
}

/** Directory entry returned by /api/filesystem/browse */
export interface DirEntry {
  name: string;
  path: string;
  is_dir: boolean;
  is_git_repo: boolean;
}

/** Browse response returned by /api/filesystem/browse */
export interface BrowseResponse {
  entries: DirEntry[];
  has_more: boolean;
}

/** Group info returned by /api/groups */
export interface GroupInfo {
  path: string;
  session_count: number;
}

/** Project info returned by /api/projects */
export interface ProjectInfo {
  name: string;
  path: string;
  scope: "global" | "profile";
  /** Default base branch for new worktree branches against this project's repo. */
  default_base_branch?: string;
  /** Whether the project is pinned: shown as a sessionless sidebar header. A
   *  registry entry is the saved project; the pin is the separate decision to
   *  keep its header visible without sessions. See #2208. */
  pinned: boolean;
}

/** Docker status returned by /api/docker/status */
export interface DockerStatusResponse {
  available: boolean;
  runtime: string | null;
}

/** Request body for POST /api/sessions */
export interface CreateSessionRequest {
  title?: string;
  path: string;
  tool: string;
  group?: string;
  yolo_mode?: boolean;
  /** Enables worktree mode even when no explicit branch is provided. When
   *  true and `worktree_branch` is omitted, the server derives the branch from
   *  the resolved session title. */
  worktree_enabled?: boolean;
  worktree_branch?: string;
  create_new_branch?: boolean;
  /** Branch the new worktree branch is based on (only honored when
   *  `create_new_branch` is true; empty = repo default). See #948. */
  base_branch?: string;
  sandbox?: boolean;
  extra_args?: string;
  sandbox_image?: string;
  extra_env?: string[];
  extra_repo_paths?: string[];
  /** Base branch for individual repos. `repo` is a repo directory name or one
   *  of the paths in `path` / `extra_repo_paths`; outranks `base_branch`,
   *  which stays the base for every repo no entry names. See #3329. */
  repo_bases?: { repo: string; base_branch: string }[];
  command_override?: string;
  custom_instruction?: string;
  profile?: string;
  /** Substrate selection: true → ACP-based acp (Beta),
   *  false → tmux passthrough (legacy). Server defaults to true on
   *  web-created sessions; the wizard may override. */
  view?: "structured" | "terminal";
  /** Optional acp model selected before the ACP worker starts. */
  agent_model?: string;
  agent_effort?: string;
  /** Optional acp reasoning effort applied after ACP config options load. */
  acp_effort?: string;
  /** Scratch mode: server provisions a fresh directory under
   *  `<app_dir>/scratch/<id>/` and ignores `path` (clients send `""`).
   *  Mutually exclusive with `worktree_branch` and `extra_repo_paths`;
   *  the server returns 400 on either combination. */
  scratch?: boolean;
  /** Approve the repo's `on_create` lifecycle hooks for this create,
   *  mirroring the CLI `--trust-hooks` flag and the TUI trust dialog
   *  (#2066). When a repo defines hooks that need approval and this is
   *  unset, the server returns a `hooks_need_trust` 403; the wizard then
   *  prompts and resubmits with this set to true. */
  trust_hooks?: boolean;
  /** Import an existing Claude Code session: the on-disk session id to
   *  resume via `session/load`. Forces the structured view; `path` must be
   *  the session's original cwd. See #2276. */
  import_acp_session_id?: string;
  /** Fork an existing session: the source session's captured agent session id
   *  (terminal) or ACP session id (structured) to resume and diverge from. The
   *  new session continues that conversation independently; the original is
   *  untouched. Server picks terminal vs structured from `view` + tool. */
  fork_from?: string;
}

/** A discoverable existing Claude Code session on disk, returned by
 *  `GET /api/claude-sessions` for the import picker. See #2276. */
export interface ClaudeSessionSummary {
  session_id: string;
  cwd: string;
  title: string | null;
  last_modified_ms: number;
  cwd_exists: boolean;
}

// --- Settings schema (single source of truth, see #1692) ---
//
// Mirrors `crate::session::config::settings_schema`. `GET /api/settings/schema`
// returns `SettingsFieldDescriptor[]`; the generic settings renderer builds
// the form from it instead of hand-written per-field JSX.

/** One option of a `select` widget. `value` is written to disk; `label` is
 *  shown to the user. */
export interface SettingsSelectOption {
  value: string;
  label: string;
}

/** Discriminated on `kind` (serde `#[serde(tag = "kind")]`). Carries
 *  everything the generic renderer needs to draw the control. */
export type SettingsWidget =
  | { kind: "toggle" }
  | { kind: "text"; multiline?: boolean; mono?: boolean }
  | { kind: "optional_text"; mono?: boolean }
  | { kind: "number"; min?: number; max?: number }
  | { kind: "slider"; min: number; max: number; step: number }
  | { kind: "select"; options: SettingsSelectOption[] }
  | { kind: "list" }
  /** A select whose options the host resolves at render time (API v9). */
  | { kind: "dynamic_select"; source: SettingsOptionSource; depends_on?: string[] }
  /** A repeatable list of structured items (API v9). One level deep. */
  | {
      kind: "object_list";
      id_field: string;
      fields: SettingsObjectField[];
      min_items?: number;
      max_items?: number;
    }
  /** A cron expression, rendered as a validated text field (API v9). */
  | { kind: "cron" }
  /** Escape hatch: a bespoke widget keyed by `id`. The renderer maps the id
   *  to a hand-written component (e.g. the logging per-target matrix). */
  | { kind: "custom"; id: string };

/** Host option source a `dynamic_select` draws from (API v9). Serialized
 *  snake_case, posted back verbatim to the resolver endpoint. */
export type SettingsOptionSource = "acp_agents" | "acp_models" | "acp_modes" | "projects" | "groups";

/** The widget of one `object_list` item field (API v9). A subset of
 *  {@link SettingsWidget} with no object-list variant (non-recursive). */
export type SettingsObjectFieldWidget =
  | { kind: "toggle" }
  | { kind: "text"; multiline?: boolean; mono?: boolean }
  | { kind: "number"; min?: number; max?: number }
  | { kind: "select"; options: SettingsSelectOption[] }
  | { kind: "dynamic_select"; source: SettingsOptionSource; depends_on?: string[] }
  /** A host-resolved multi-select; the value is an array of chosen option
   *  values (API v11). */
  | { kind: "dynamic_multi_select"; source: SettingsOptionSource; depends_on?: string[] }
  | { kind: "cron" };

/** One nested field of an `object_list` item (API v9). */
export interface SettingsObjectField {
  field: string;
  label: string;
  description?: string;
  required?: boolean;
  widget: SettingsObjectFieldWidget;
  validation: SettingsValidation;
  default?: unknown;
}

/** Whether the dashboard may write a field (serde `#[serde(tag = "policy")]`).
 *  `local_only` fields are rejected by the server PATCH. */
export type SettingsWebWritePolicy =
  | { policy: "allow" }
  | { policy: "requires_elevation"; reason: string }
  | { policy: "local_only"; reason: string };

/** Server-authoritative validation (serde `#[serde(tag = "rule")]`). The
 *  widget's min/max is advisory; this is the gate the server enforces. */
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

/** One configurable field. The dotted `${section}.${field}` is its stable id. */
export interface SettingsFieldDescriptor {
  section: string;
  field: string;
  /** Settings tab the row appears under. */
  category: string;
  label: string;
  description: string;
  widget: SettingsWidget;
  web_write: SettingsWebWritePolicy;
  /** `false` means global-only: shown but not overridable per profile/repo. */
  profile_overridable: boolean;
  validation: SettingsValidation;
  /** Operational tuning shown under an "Advanced" fold. */
  advanced: boolean;
  /** Default value shown before anything is stored. Present on plugin
   *  (`plugin:<id>`) fields, which have no value in the config until saved;
   *  omitted for core fields (their value always exists in the config). */
  default?: unknown;
}
