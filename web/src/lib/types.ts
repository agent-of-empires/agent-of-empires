import type { RepoColor } from "./repoAppearance";

/** Session data returned by the API */
export interface SessionResponse {
  id: string;
  title: string;
  project_path: string;
  /** Absolute host path of the session's managed artifact directory; the
   *  transcript maps agent-emitted artifact paths under it (or the fixed
   *  sandbox mount) to the authenticated artifact route. See #2587. */
  artifact_dir: string;
  group_path: string;
  tool: string;
  status: SessionStatus;
  /** True when the session's structured-view worker was auto-stopped for
   *  inactivity (resumable/dormant), as opposed to a deliberate Stop. Lets the
   *  sidebar render a distinct dormant dot instead of a live-idle one. A
   *  deliberate Stop keeps `status: "Stopped"` and reports `false`. See #2250. */
  dormant: boolean;
  yolo_mode: boolean;
  created_at: string;
  last_accessed_at: string | null;
  /** Wall-clock time of the most recent transition into Idle. Used by the
   *  dashboard to fade a freshly-stopped session's color toward neutral.
   *  Distinct from `last_accessed_at`: viewing or messaging a session bumps
   *  `last_accessed_at` but leaves `idle_entered_at` alone. */
  idle_entered_at: string | null;
  last_error: string | null;
  branch: string | null;
  main_repo_path: string | null;
  /** Base branch the worktree was created from when AoE managed the
   *  creation. null for sessions attached to a pre-existing branch or
   *  those that took the repo's default branch. See #948. */
  base_branch?: string | null;
  /** Per-session override for the diff base. When set, the sidebar
   *  diff compares the worktree against this ref instead of the
   *  auto-detected default. Edited via the `vs <ref>` chip in the
   *  diff header. See #970. */
  base_branch_override?: string | null;
  is_sandboxed: boolean;
  /** True when the session was created in scratch mode (`aoe add
   *  --scratch` or the wizard toggle). The `project_path` points
   *  at an auto-provisioned directory under `<app_dir>/scratch/<id>/`,
   *  and the deletion path removes it (unless the user opts in to
   *  keeping the directory). The wizard's Recent-projects list filters
   *  scratch sessions out. */
  scratch: boolean;
  /** True when the session is marked as a user favorite. Mirrors
   *  `Instance::is_favorited()` server-side. The sidebar pins favorited
   *  rows and prepends a `*` marker. Toggled via the TUI `f`/`F` keybind
   *  or `aoe session favorite|unfavorite`. */
  favorited: boolean;
  /** Per-session color label (`red` / `amber` / `green`), or null / undefined
   *  when unset. Rendered as a colored status dot in the sidebar for
   *  at-a-glance agent status signaling. Set via the sidebar context menu or
   *  `aoe session color <id> <color>`. See #2383. */
  color?: string | null;
  /** True when the agent has flagged this session as urgent via the
   *  `attention-urgent` hook. Mirrors `Instance::is_urgent()` server-side
   *  (false for archived / snoozed sessions). The sidebar's Attention sort
   *  floats urgent rows above non-urgent ones within their triage tier.
   *  Optional so older payloads and test fixtures without the field read as
   *  not-urgent. See #1640. */
  urgent?: boolean;
  /** RFC3339 timestamp at which the session was web-pinned, or null /
   *  undefined when not pinned. Distinct from `favorited`: favorite is
   *  the TUI within-tier attention-sort signal; pin is the hard
   *  top-of-sort surfacing primitive used by the web sidebar. Derive
   *  `isPinned = pinned_at != null` client-side; no separate boolean is
   *  exposed (the timestamp itself is the source of truth). See #1581. */
  pinned_at?: string | null;
  /** RFC3339 timestamp; null when not archived. Sinks into "Snoozed &
   *  archived"; archive tears down all tmux. See #1581, #1868. */
  archived_at?: string | null;
  /** RFC3339 timestamp at which an active snooze expires, or null /
   *  undefined when not snoozed. The server gates this on
   *  `Instance::is_snoozed()` so an expired snooze that is still on disk
   *  comes back as null on the wire; the web therefore only needs to
   *  treat any non-null value as an active snooze. See #1581. */
  snoozed_until?: string | null;
  /** RFC3339 timestamp at which the session was moved to trash, or null /
   *  undefined when not trashed. Trashed sessions are bucketed into a
   *  dedicated Trash section with restore and permanent-delete actions; they
   *  are excluded from the active and archived buckets. See #2489. */
  trashed_at?: string | null;
  /** Unread marker mirroring `Instance::unread`: `true` when the session
   *  needs attention (a finished turn the user hasn't engaged with, or a
   *  manual flag), false / undefined when read. The sidebar paints an unread
   *  accent and offers a right-click "Mark as read/unread" toggle, both gated
   *  on the `session.unread_indicator` setting. The chip is suppressed for the
   *  session currently open, which also clears the marker. */
  unread?: boolean;
  has_managed_worktree: boolean;
  /** True when deleting this session has aoe-managed worktree state to clean
   *  up, covering single-repo worktrees AND multi-repo workspaces. Only the
   *  delete dialog's worktree/branch checkboxes read this; worktree-only
   *  actions (Edit workdir) keep reading `has_managed_worktree`. Always sent
   *  by the server; optional only so existing test fixtures need not set it. */
  has_cleanable_worktree?: boolean;
  /** True when renaming this session also moves its worktree directory (the
   *  resolved `session.tie_workdir_to_name` for an aoe-managed worktree). The
   *  sidebar uses this to collapse the standalone "edit workdir name" action
   *  into the unified rename. Populated by the list endpoint. See #1927. */
  tie_workdir_to_name?: boolean;
  has_terminal: boolean;
  profile: string;
  cleanup_defaults: CleanupDefaults;
  remote_owner: string | null;
  /** Per-session push-notification overrides. null means "inherit the
   *  server default" for that event type; boolean is an explicit toggle. */
  notify_on_waiting: boolean | null;
  notify_on_idle: boolean | null;
  notify_on_error: boolean | null;
  /** True when this session uses ACP acp rendering instead of a
   *  tmux-backed PTY. Absent on builds without the acp feature. */
  view?: "structured" | "terminal";
  /** Live acp worker lifecycle. `absent` for tmux sessions or
   *  acp sessions whose worker has not been spawned yet; `resuming`
   *  while the reconciler is mid-spawn or mid-attach; `running` once
   *  the supervisor holds a live worker. Drives the sidebar `Resuming…`
   *  chip and the per-session banner in the acp view. See #1088. */
  acp_worker_state?: AcpWorkerState;
  /** Smart-rename indicator for structured view sessions. `pending`: still
   *  default-named and eligible, will auto-name on the next prompt; `running`:
   *  a one-shot title call is in flight; `inactive`/absent otherwise. Drives
   *  the sidebar auto-name chip. See session::smart_rename. */
  smart_rename?: "inactive" | "pending" | "running";
  /** True when the session still carries its auto-generated civilization name.
   *  Gates the sidebar "Auto-name now" action, which only re-runs smart rename
   *  on a still-default session. More reliable than `smart_rename` for this: a
   *  timed-out one-shot stays `pending` while an unusable-output one goes
   *  `inactive`, but both leave the name default and recoverable. Populated by
   *  the session list; absent on single-session responses. */
  default_name?: boolean;
  /** True when this session's agent can run in acp: a built-in with
   *  an ACP adapter, or a custom agent whose profile config declares a
   *  valid `agent_acp_cmd`. The terminal view's "switch to acp"
   *  affordance reads this instead of a hardcoded tool list. Absent on
   *  builds without the acp feature. */
  acp_capable?: boolean;
  /** The session's captured ACP session id, present only once the structured
   *  view worker has minted one. The sidebar passes this as `fork_from` on a
   *  structured fork create, and gates the "Fork" action on its presence (a
   *  structured row with a captured id to diverge from). Absent for terminal
   *  sessions and structured ones whose worker has not minted an id yet. */
  acp_session_id?: string;
  /** The session's resolved ACP registry key (`agent_name` when set, else
   *  `tool`), matching the `name` entries `/api/acp/agents` returns. The
   *  structured view's switch-agent modal uses this as the current-agent
   *  fallback before the first `AgentSwitched` event lands (which is the only
   *  event that populates the reduced `state.agent`), so it can gray out the
   *  running backend on a never-switched session. See #2803. */
  acp_agent?: string;
  /** True when this session's agent can run a structured ACP `session/fork`:
   *  it is ACP-capable AND declares a real fork strategy. Resume-only ACP
   *  agents (e.g. the bundled `aoe-agent`, which advertises `loadSession` but
   *  not `session/fork`) are ACP-capable yet not forkable. The sidebar gates the
   *  "Fork" action on this together with `acp_session_id` so a resume-only row
   *  never shows a dead-end fork button. Absent (read as not-forkable) for
   *  terminal sessions and non-forkable agents. */
  acp_can_fork?: boolean;
  /** True when this is a Claude Code session AND the user has enabled
   *  Claude's fullscreen renderer (`tui: "fullscreen"` in
   *  ~/.claude/settings.json). The mobile rendering path uses this to
   *  skip scrollback-tracking workarounds that target tmux copy-mode. */
  claude_fullscreen: boolean;
  /** Repos in the multi-repo workspace. Empty array for single-repo sessions. */
  workspace_repos: WorkspaceRepoSummary[];
  /** Non-fatal warnings emitted during worktree creation (e.g. post-checkout
   *  hook failures where the worktree was created successfully anyway). Only
   *  populated on the create-session response; absent on subsequent fetches. */
  warnings?: string[];
  /** Latest plan snapshot summarised for the sidebar. Present only on
   *  acp sessions whose agent has emitted a Plan. See #1061. */
  plan_summary?: PlanSummary;
  /** Absolute RFC3339 timestamp at which the agent's pending
   *  `ScheduleWakeup` fires. Cleared once a fresh user prompt lands
   *  after the scheduling call. Present only on acp sessions
   *  whose agent has called `ScheduleWakeup` since the last prompt.
   *  See #1091. */
  next_wakeup_at?: string;
  /** Reason the agent provided when scheduling the wakeup. Only set
   *  when `next_wakeup_at` is also set. */
  next_wakeup_reason?: string;
  /** True when the acp session has an armed `Monitor` (a background
   *  watch). Drives a static "monitoring" sidebar badge. Cleared once a
   *  fresh user prompt lands after the monitor was armed. */
  monitor_active?: boolean;
  /** The `description` the agent gave the `Monitor` tool, shown as the
   *  badge tooltip. Only set when `monitor_active` is true. */
  monitor_description?: string;
}

export interface PlanSummary {
  /** First non-completed step's title, truncated server-side. */
  current_step_title: string | null;
  /** Count of steps with status `Done`. */
  completed: number;
  /** Total step count. */
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
  /** Resolved `session.delete_to_trash`: the delete dialog defaults to
   *  "Move to Trash" when true, permanent delete when false. See #2489. */
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
  /** The ACP command a built-in agent launches in acp (e.g.
   *  `claude-agent-acp`, `opencode`), post `${aoe_data_dir}`
   *  substitution. Can differ from `binary`. Absent for custom agents,
   *  whose command values are never serialized by the backend. */
  acp_command?: string;
  /** Registry args appended to `acp_command` (e.g. `["acp"]` for
   *  opencode, `["--acp"]` for gemini). Absent or empty when none. */
  acp_args?: string[];
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
 *  HooksConfigOverride (src/session/profile_config.rs): a field that is
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

/** Live acp worker lifecycle, mirrored from
 *  `crate::acp::supervisor::AcpWorkerState`. See #1088. */
export type AcpWorkerState = "absent" | "resuming" | "running";

// --- Settings schema (single source of truth, see #1692) ---
//
// Mirrors `crate::session::settings_schema`. `GET /api/settings/schema`
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
  /** Escape hatch: a bespoke widget keyed by `id`. The renderer maps the id
   *  to a hand-written component (e.g. the logging per-target matrix). */
  | { kind: "custom"; id: string };

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
  | { rule: "range_u64"; min: number; max?: number }
  | { rule: "range_i64"; min: number; max?: number }
  | { rule: "one_of"; options: string[] }
  | { rule: "non_empty_string" }
  | { rule: "memory_limit" }
  | { rule: "volume_list" }
  | { rule: "env_list" }
  | { rule: "port_mapping_list" };

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
