import type { RepoColor } from "./repoAppearance";

/** Session data returned by the API */
export interface SessionResponse {
  id: string;
  title: string;
  project_path: string;
  group_path: string;
  tool: string;
  status: SessionStatus;
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
  /** RFC3339 timestamp at which the session was web-pinned, or null /
   *  undefined when not pinned. Distinct from `favorited`: favorite is
   *  the TUI within-tier attention-sort signal; pin is the hard
   *  top-of-sort surfacing primitive used by the web sidebar. Derive
   *  `isPinned = pinned_at != null` client-side; no separate boolean is
   *  exposed (the timestamp itself is the source of truth). See #1581. */
  pinned_at?: string | null;
  /** RFC3339 timestamp at which the session was archived, or null /
   *  undefined when not archived. Archived workspaces sink into the
   *  collapsible "Snoozed & archived" footer of their repo group and
   *  their tmux pane is killed by the archive handler. See #1581. */
  archived_at?: string | null;
  /** RFC3339 timestamp at which an active snooze expires, or null /
   *  undefined when not snoozed. The server gates this on
   *  `Instance::is_snoozed()` so an expired snooze that is still on disk
   *  comes back as null on the wire; the web therefore only needs to
   *  treat any non-null value as an active snooze. See #1581. */
  snoozed_until?: string | null;
  has_managed_worktree: boolean;
  has_terminal: boolean;
  profile: string;
  cleanup_defaults: CleanupDefaults;
  remote_owner: string | null;
  /** Per-session push-notification overrides. null means "inherit the
   *  server default" for that event type; boolean is an explicit toggle. */
  notify_on_waiting: boolean | null;
  notify_on_idle: boolean | null;
  notify_on_error: boolean | null;
  /** True when this session uses ACP cockpit rendering instead of a
   *  tmux-backed PTY. Absent on builds without the cockpit feature. */
  cockpit_mode?: boolean;
  /** Live cockpit worker lifecycle. `absent` for tmux sessions or
   *  cockpit sessions whose worker has not been spawned yet; `resuming`
   *  while the reconciler is mid-spawn or mid-attach; `running` once
   *  the supervisor holds a live worker. Drives the sidebar `Resuming…`
   *  chip and the per-session banner in the cockpit view. See #1088. */
  cockpit_worker_state?: CockpitWorkerState;
  /** True when this session's agent can run in cockpit: a built-in with
   *  an ACP adapter, or a custom agent whose profile config declares a
   *  valid `agent_cockpit_cmd`. The terminal view's "switch to cockpit"
   *  affordance reads this instead of a hardcoded tool list. Absent on
   *  builds without the cockpit feature. */
  acp_capable?: boolean;
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
   *  cockpit sessions whose agent has emitted a Plan. See #1061. */
  plan_summary?: PlanSummary;
  /** Absolute RFC3339 timestamp at which the agent's pending
   *  `ScheduleWakeup` fires. Cleared once a fresh user prompt lands
   *  after the scheduling call. Present only on cockpit sessions
   *  whose agent has called `ScheduleWakeup` since the last prompt.
   *  See #1091. */
  next_wakeup_at?: string;
  /** Reason the agent provided when scheduling the wakeup. Only set
   *  when `next_wakeup_at` is also set. */
  next_wakeup_reason?: string;
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
  status:
    | "added"
    | "modified"
    | "deleted"
    | "renamed"
    | "copied"
    | "untracked"
    | "conflicted";
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

/** Response from /api/sessions/{id}/diff/file?path=... */
export interface RichFileDiffResponse {
  file: RichDiffFile;
  hunks: RichDiffHunk[];
  is_binary: boolean;
  /** True if the file was too large to diff inline. */
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
  /** True when the agent can run in cockpit: a built-in with an ACP
   *  adapter, or a custom agent that declares a valid `agent_cockpit_cmd`.
   *  The wizard reads this to decide whether a new session runs in
   *  cockpit or tmux, replacing the hardcoded client-side tool list. */
  acp_capable: boolean;
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
  /** Substrate selection: true → ACP-based cockpit (Beta),
   *  false → tmux passthrough (legacy). Server defaults to true on
   *  web-created sessions; the wizard may override. */
  cockpit_mode?: boolean;
  /** Scratch mode: server provisions a fresh directory under
   *  `<app_dir>/scratch/<id>/` and ignores `path` (clients send `""`).
   *  Mutually exclusive with `worktree_branch` and `extra_repo_paths`;
   *  the server returns 400 on either combination. */
  scratch?: boolean;
}

/** Live cockpit worker lifecycle, mirrored from
 *  `crate::cockpit::supervisor::CockpitWorkerState`. See #1088. */
export type CockpitWorkerState = "absent" | "resuming" | "running";
