import { clientFormFactor } from "./formFactor";
import type {
  SessionResponse,
  RichDiffFilesResponse,
  RichFileContentsResponse,
  AgentInfo,
  ProfileInfo,
  ProfileSettingsResponse,
  BrowseResponse,
  GroupInfo,
  ProjectInfo,
  DockerStatusResponse,
  CreateSessionRequest,
  SettingsFieldDescriptor,
} from "./types";
import { clearDeviceBindingSecret, getOrCreateDeviceBindingSecret } from "./deviceBinding";

// GET a JSON endpoint; returns null on non-2xx or network/parse errors.
async function fetchJson<T>(url: string, init?: RequestInit): Promise<T | null> {
  try {
    const res = await fetch(url, init);
    if (!res.ok) return null;
    return (await res.json()) as T;
  } catch {
    return null;
  }
}

// --- Sessions ---

export interface SessionsEnvelope {
  sessions: SessionResponse[];
  workspace_ordering: string[];
}

export function fetchSessions(): Promise<SessionsEnvelope | null> {
  return fetchJson<SessionsEnvelope>("/api/sessions");
}

export async function updateWorkspaceOrdering(order: string[]): Promise<boolean> {
  try {
    const res = await fetch("/api/workspace-ordering", {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ order }),
    });
    return res.ok;
  } catch {
    return false;
  }
}

export interface EnsureSessionResult {
  ok: boolean;
  status?: "alive" | "restarted";
  error?: string;
  message?: string;
}

export async function ensureSession(id: string, signal?: AbortSignal): Promise<EnsureSessionResult> {
  try {
    const res = await fetch(`/api/sessions/${id}/ensure`, {
      method: "POST",
      signal,
    });
    const body = await res.json().catch(() => ({}));
    if (!res.ok) {
      return {
        ok: false,
        error: typeof body.error === "string" ? body.error : undefined,
        message: typeof body.message === "string" ? body.message : `Server error (${res.status})`,
      };
    }
    return {
      ok: true,
      status: body.status as "alive" | "restarted" | undefined,
    };
  } catch (e) {
    if ((e as { name?: string }).name === "AbortError") {
      return { ok: false, error: "aborted" };
    }
    return {
      ok: false,
      message: e instanceof Error ? e.message : "Network error",
    };
  }
}

export async function ensureTerminal(id: string, container = false): Promise<boolean> {
  const path = container ? "container-terminal" : "terminal";
  try {
    const res = await fetch(`/api/sessions/${id}/${path}`, {
      method: "POST",
    });
    return res.ok;
  } catch {
    return false;
  }
}

export function getSessionDiffFiles(id: string): Promise<RichDiffFilesResponse | null> {
  return fetchJson<RichDiffFilesResponse>(`/api/sessions/${id}/diff/files`);
}

/**
 * Fetch raw old/new contents plus a server-computed unified patch for a
 * file; the client renders the patch via `@pierre/diffs` without re-diffing.
 * See {@link RichFileContentsResponse}.
 */
export function getSessionFileContents(
  id: string,
  filePath: string,
  repoName?: string,
): Promise<RichFileContentsResponse | null> {
  const params = new URLSearchParams({ path: filePath });
  if (repoName) params.set("repo", repoName);
  return fetchJson<RichFileContentsResponse>(`/api/sessions/${id}/diff/file?${params.toString()}`);
}

// --- Settings ---

export interface SettingsResponse {
  theme?: {
    idle_decay_minutes?: number;
  };
  app_state?: {
    has_seen_web_tour?: boolean;
  };
  [key: string]: unknown;
}

export function fetchSettings(profile?: string): Promise<SettingsResponse | null> {
  const params = profile ? `?profile=${encodeURIComponent(profile)}` : "";
  return fetchJson<SettingsResponse>(`/api/settings${params}`);
}

// The schema is static for the server's run, and the profile-settings write
// guard (`updateProfileSettings`) derives its section allowlist from it, so we
// cache the first successful fetch and reuse it instead of refetching on every
// save. A failed fetch is not cached, so the next call retries.
let schemaPromise: Promise<SettingsFieldDescriptor[] | null> | null = null;

/** Fetch the settings schema (single source of truth, #1692). The generic
 *  settings renderer builds form rows from these descriptors instead of
 *  hand-written per-field JSX. */
export function getSettingsSchema(): Promise<SettingsFieldDescriptor[] | null> {
  if (!schemaPromise) {
    schemaPromise = fetchJson<SettingsFieldDescriptor[]>("/api/settings/schema").then((s) => {
      if (!s) schemaPromise = null;
      return s;
    });
  }
  return schemaPromise;
}

/** Test-only seam: drop the cached schema so each test starts cold. */
export function resetSettingsSchemaCache(): void {
  schemaPromise = null;
}

export async function updateSettings(updates: Record<string, unknown>): Promise<boolean> {
  try {
    const res = await fetch("/api/settings", {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(updates),
    });
    return res.ok;
  } catch {
    return false;
  }
}

/**
 * Sets the global theme (name and/or color mode). Dedicated endpoint, not
 * `PATCH /api/settings`: the theme is a global preference but cosmetic, so it
 * must not trip the passphrase/elevation wall the general settings surface
 * carries. Returns false on read-only servers (403) or network failure.
 */
export async function updateTheme(patch: { name?: string; color_mode?: string }): Promise<boolean> {
  try {
    const res = await fetch("/api/theme", {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(patch),
    });
    return res.ok;
  } catch {
    return false;
  }
}

/**
 * Marks the first-run dashboard tour as seen for this server. Single-purpose
 * endpoint (not PATCH /api/settings) so the cosmetic flag stays off the
 * passphrase/elevation wall. Returns false on read-only servers (403) or
 * network failure; callers treat that as nonfatal and suppress the tour in
 * memory for the current page.
 */
export async function markWebTourSeen(): Promise<boolean> {
  try {
    const res = await fetch("/api/app-state/web-tour-seen", {
      method: "POST",
    });
    return res.ok;
  } catch {
    return false;
  }
}

// --- Sandbox volume_ignores glob expansion (#2045) ---

export interface VolumeIgnoresGlobPreview {
  pattern: string;
  /** Container-side paths the pattern matches in the workspace right now. */
  matched_paths: string[];
}

export interface VolumeIgnoresPreviewResponse {
  /** True once the snapshot-expansion behavior has been acknowledged. */
  acknowledged: boolean;
  /** One entry per configured glob pattern; empty when none are configured. */
  globs: VolumeIgnoresGlobPreview[];
}

/**
 * Dry-run how glob `volume_ignores` entries (recursive `**` patterns) expand for a
 * sandbox session rooted at `path`. The wizard calls this before creating to
 * decide whether to show the one-time snapshot-expansion confirm modal (#2045).
 * Returns null on failure; callers treat that as "nothing to confirm".
 */
export async function fetchVolumeIgnoresPreview(
  path: string,
  profile?: string,
): Promise<VolumeIgnoresPreviewResponse | null> {
  try {
    const params = new URLSearchParams({ path });
    if (profile) params.set("profile", profile);
    const res = await fetch(`/api/sandbox/volume-ignores-preview?${params.toString()}`);
    if (!res.ok) return null;
    return (await res.json()) as VolumeIgnoresPreviewResponse;
  } catch {
    return null;
  }
}

/**
 * Records that the user acknowledged glob `volume_ignores` snapshot expansion,
 * so the confirm modal shows once and never again. Single-purpose endpoint
 * (not PATCH /api/settings) so the flag stays off the passphrase/elevation
 * wall. Returns false on read-only servers (403) or network failure.
 */
export async function markVolumeIgnoresGlobsAcknowledged(): Promise<boolean> {
  try {
    const res = await fetch("/api/app-state/volume-ignores-globs-acknowledged", {
      method: "POST",
    });
    return res.ok;
  } catch {
    return false;
  }
}

// --- Profile management ---

export async function createProfile(name: string): Promise<boolean> {
  try {
    const res = await fetch("/api/profiles", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name }),
    });
    return res.ok;
  } catch {
    return false;
  }
}

export async function deleteProfile(name: string): Promise<boolean> {
  try {
    const res = await fetch(`/api/profiles/${encodeURIComponent(name)}`, {
      method: "DELETE",
    });
    return res.ok;
  } catch {
    return false;
  }
}

export async function renameProfile(name: string, newName: string): Promise<boolean> {
  try {
    const res = await fetch(`/api/profiles/${encodeURIComponent(name)}/rename`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ new_name: newName }),
    });
    return res.ok;
  } catch {
    return false;
  }
}

export async function setDefaultProfile(name: string): Promise<boolean> {
  try {
    const res = await fetch("/api/default-profile", {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name }),
    });
    return res.ok;
  } catch {
    return false;
  }
}

export function getProfileSettings(name: string): Promise<ProfileSettingsResponse | null> {
  return fetchJson<ProfileSettingsResponse>(`/api/profiles/${encodeURIComponent(name)}/settings`);
}

/** Profile-settings sections the dashboard is allowed to PATCH, derived from
 *  the live settings schema (single source of truth, #1692) so the client
 *  guard cannot drift from the server. Every schema section is profile-
 *  writable; `description` is the profile-only top-level field that carries no
 *  schema descriptor. Sections absent from the schema, notably `hooks` plus the
 *  agent-command and env fields, are remote-code-execution surfaces the server
 *  rejects (`validate_patch` in src/session/settings_schema/policy.rs); we
 *  reject them client side too as defense in depth. Because both sides read the
 *  same schema, there is no hand-kept list to keep in sync. */
export function profileWritableSections(schema: SettingsFieldDescriptor[]): Set<string> {
  const sections = new Set(schema.map((d) => d.section));
  sections.add("description");
  return sections;
}

/** PATCH a profile's settings, refusing any section the schema does not list as
 *  writable (see {@link profileWritableSections}) before sending. Returns
 *  whether the server accepted the write. */
export async function updateProfileSettings(name: string, updates: Record<string, unknown>): Promise<boolean> {
  const schema = await getSettingsSchema();
  // With the schema in hand, refuse a blocked section before sending. If the
  // schema fetch failed, defer to the server's authoritative guard rather than
  // blocking a legitimate save on a transient network error.
  if (schema) {
    const writable = profileWritableSections(schema);
    for (const key of Object.keys(updates)) {
      if (!writable.has(key)) {
        // Refuse loudly rather than silently dropping the key. A blocked
        // section in a profile PATCH (e.g. `hooks`) is a caller bug; the
        // server would 400 it anyway. Failing here keeps a buggy caller
        // from reporting a partial save as success.
        console.error(`updateProfileSettings: refusing to send blocked profile section "${key}"`);
        return false;
      }
    }
  }
  try {
    const res = await fetch(`/api/profiles/${encodeURIComponent(name)}/settings`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(updates),
    });
    return res.ok;
  } catch {
    return false;
  }
}

// --- Themes & Sounds ---

import type { ResolvedTheme } from "./theme";

export async function fetchThemes(): Promise<string[]> {
  return (await fetchJson<string[]>("/api/themes")) ?? [];
}

/** Fetch the resolved theme projection (web CSS vars, terminal CSS
 *  vars, syntax highlighter selection) for a named theme. The server
 *  falls back to Empire for unknown names; check `source` to detect. */
export function fetchResolvedTheme(name: string): Promise<ResolvedTheme | null> {
  return fetchJson<ResolvedTheme>(`/api/themes/${encodeURIComponent(name)}`);
}

/** Fetch the resolved theme for the active profile's current
 *  selection. Server reads from profile_config so per-profile overrides
 *  land in the right place. */
export function fetchCurrentTheme(): Promise<ResolvedTheme | null> {
  return fetchJson<ResolvedTheme>("/api/theme/current");
}

export async function fetchSounds(): Promise<string[]> {
  return (await fetchJson<string[]>("/api/sounds")) ?? [];
}

/** Fetch a sound file as a Blob so the acp's browser-side approval
 *  player can hand a blob URL to `new Audio(...)`. The fetch path runs
 *  through `fetchInterceptor.ts`, which injects `Authorization: Bearer`
 *  on every request; an `<audio src="...">` element does not, so a
 *  blob round-trip is necessary in PWA mode. See #1038. */
export async function fetchSoundBlob(name: string): Promise<Blob | null> {
  try {
    const res = await fetch(`/api/sounds/file/${encodeURIComponent(name)}`);
    if (!res.ok) return null;
    return await res.blob();
  } catch {
    return null;
  }
}

// --- About / server info ---

export interface ServerAbout {
  version: string;
  auth_required: boolean;
  passphrase_enabled: boolean;
  /** Resolved `--auth` mode. `"token"` means the URL token gates
   *  requests; `"passphrase"` means the passphrase login wall is the
   *  only human gate; `"none"` means no authentication at all. The
   *  Security panel renders an accurate label off this instead of
   *  guessing "--no-auth" from `auth_required === false`. */
  auth_mode: "token" | "passphrase" | "none";
  read_only: boolean;
  behind_tunnel: boolean;
  profile: string;
  /** Resolved `acp.show_tool_durations` from the active profile's
   *  config. Drives the per-tool elapsed-time label in the acp
   *  web UI; cross-device since it lives in config.toml. */
  acp_show_tool_durations: boolean;
  /** Resolved `acp.queue_drain_mode` from the active profile's
   *  config. Selects how the composer drains client-side queued
   *  follow-up prompts on Stopped: `combined` (default) joins them
   *  with blank lines into a single prompt; `serial` fires one entry
   *  at a time. See #1031. */
  acp_queue_drain_mode: "combined" | "serial";
  /** Resolved `acp.max_concurrent_resumes` from the active
   *  profile's config. Upper bound on parallel acp worker
   *  spawns/attaches the reconciler runs on `aoe serve` cold start.
   *  See #1088. */
  acp_max_concurrent_resumes: number;
  /** Resolved `acp.force_end_turn_threshold_secs` from the active
   *  profile's config. Seconds of streaming inactivity after which
   *  the acp web UI offers a "Force end turn" button. See #1100. */
  acp_force_end_turn_threshold_secs: number;
  /** Resolved `acp.replay_events` from the active profile's
   *  config. Per-session retention cap on the acp event log;
   *  0 means unlimited. Mirrored onto the in-memory activity buffer
   *  so the rendered transcript matches the user's chosen ceiling
   *  instead of clipping at a hard-coded frontend constant. See #1111. */
  acp_replay_events: number;
  build_flavor: "debug" | "release"; // `"debug"` => debug_assertions; drives topbar DEV badge. See #1055.
  /** Content-hashed entry bundle name (`index-<hash>.js`) of the
   *  embedded dashboard build. Compared against this page's own entry
   *  script by DashboardUpdateBanner to detect a stale client (an
   *  installed PWA has no refresh affordance, so it keeps running old
   *  code after the binary updates until prompted to reload). */
  web_build_id?: string | null;
}

export function fetchAbout(): Promise<ServerAbout | null> {
  return fetchJson<ServerAbout>("/api/about");
}

export interface TelemetryStatus {
  enabled: boolean;
  responded: boolean;
  do_not_track: boolean;
}

export function fetchTelemetryStatus(): Promise<TelemetryStatus | null> {
  return fetchJson<TelemetryStatus>("/api/telemetry/status");
}

/// Set the opt-in state. The daemon owns the anonymous install id; the
/// browser never posts to the telemetry backend itself. Returns the updated
/// status, or null on failure.
export async function setTelemetryConsent(enabled: boolean): Promise<TelemetryStatus | null> {
  try {
    const res = await fetch("/api/telemetry/consent", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ enabled }),
    });
    if (!res.ok) return null;
    return await res.json();
  } catch {
    return null;
  }
}

/// Allowlisted usage-signal names the daemon accepts on `/api/telemetry/seen`.
/// Mirrors `USAGE_SIGNALS` in `src/telemetry/usage_signals.rs`; an off-list
/// name is rejected with a 400 server-side. `web` / `structured_view` are whole-UI
/// opens; the rest are feature-level opens within the dashboard (#1881).
export type TelemetrySignal = "web" | "structured_view" | "diff_panel" | "diff_comments" | "web_terminal";

/// Tell the daemon an allowlisted surface or feature was opened, so its next
/// opt-in snapshot can carry the `usage_seen` open-count map plus the coarse
/// client form-factor class (#1883). Best-effort; the daemon only forwards the
/// count when the install is opted in. The browser never posts to the telemetry
/// backend; it pings the local daemon, which folds both into its own snapshot.
export function reportTelemetrySeen(surface: TelemetrySignal): void {
  void fetch("/api/telemetry/seen", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ surface, form_factor: clientFormFactor() }),
  }).catch(() => {});
}

/// Report an acp interaction the daemon cannot observe itself, so its next
/// opt-in snapshot can fold it in. Today the only kind is a queued prompt: the
/// prompt queue lives entirely in client state, so the browser is the one
/// surface that can report it. Best-effort; the daemon only counts when the
/// user is opted in.
export function reportAcpInteraction(kind: "prompt_queued"): void {
  void fetch("/api/telemetry/structured-interaction", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ kind }),
  }).catch(() => {});
}

/** Runtime helper around `ServerAbout.build_flavor`. See #1055. */
export function isDebugBuild(about: ServerAbout | null | undefined): boolean {
  if (!about) return false;
  return about.build_flavor === "debug";
}
export type UpdateCheckMode = "auto" | "notify" | "off";

export interface UpdateStatus {
  update_check_mode: UpdateCheckMode;
  current_version: string;
  latest_version: string | null;
  update_available: boolean;
  release_url: string | null;
  web_poll_interval_minutes: number;
  error: string | null;
}

export function fetchUpdateStatus(): Promise<UpdateStatus | null> {
  return fetchJson<UpdateStatus>("/api/system/update-status");
}

// --- Branches ---

export interface BranchInfo {
  name: string;
  is_current: boolean;
  remote_only?: boolean;
}

/** Lists branches for a repo path. When `includeRemote` is true the
 *  response includes branches that only exist on the remote (with
 *  `remote_only: true`); selecting one bases the new worktree off the
 *  remote tip. See #948. */
export function fetchBranches(path: string, includeRemote = false): Promise<BranchInfo[] | null> {
  const params = new URLSearchParams({ path });
  if (includeRemote) params.set("include_remote", "true");
  return fetchJson<BranchInfo[]>(`/api/git/branches?${params.toString()}`);
}

// --- Acp context primer ---

export interface ContextPrimerResponse {
  primer: string;
  included_event_count: number;
  included_turn_count: number;
  truncated: boolean;
  max_chars: number;
  /** When the recap was built from a session that ended in a non-
   *  success terminal (rate-limit park or AgentStartupError), the
   *  user's most recent UserPromptSent never reached the agent. The
   *  backend pops it from the primer body and surfaces it here so the
   *  recovery UI can drop it back into the composer as the user's
   *  pending request. See #1281 / #1282. */
  unprocessed_prompt?: string | null;
}

// --- Acp ACP registry ---

export interface AcpAgentInfo {
  name: string;
  description: string;
  command: string;
}

/** List ACP registry entries the acp supervisor knows about.
 *  Distinct from `/api/agents` (session-tool agents for the wizard);
 *  this is the *acp* registry used by the rate-limit recovery
 *  modal to populate the handoff target list. See #1282. */
export async function fetchAcpAgents(): Promise<AcpAgentInfo[]> {
  return (await fetchJson<AcpAgentInfo[]>("/api/acp/agents")) ?? [];
}

// --- Acp switch agent ---

export interface SwitchAgentResponse {
  session_id: string;
  agent: string;
  /** Highest seq BEFORE AgentSwitched was emitted. Pass to
   *  fetchContextPrimer so the recap excludes the handoff event. */
  before_seq: number;
  /** Seq assigned to the AgentSwitched event. The frontend awaits the
   *  reducer reaching this seq before prefilling so the divider and
   *  composer prefill arrive in order. */
  switch_seq: number;
  status: string;
}

/** Hand off an acp session from its current ACP backend to
 *  `target` (registry key, e.g. "codex"). Backend stops the old
 *  worker, spawns the new one, persists the agent change, and emits
 *  an AgentSwitched event. On failure (unknown target, spawn error)
 *  the instance is left untouched. `reason` is recorded on the event
 *  and shown in the transcript divider: "rate_limited" for the
 *  recovery flow, "manual" for an explicit user switch. See #1282. */
export async function switchAcpAgent(
  sessionId: string,
  target: string,
  model?: string | null,
  reason?: string | null,
): Promise<SwitchAgentResponse | null> {
  const body: { target: string; model?: string; reason?: string } = { target };
  if (model) body.model = model;
  if (reason) body.reason = reason;
  return fetchJson<SwitchAgentResponse>(`/api/sessions/${encodeURIComponent(sessionId)}/acp/switch-agent`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
}

/** Fetch a markdown primer built from events `seq < beforeSeq`. Used
 *  after a `session/load` failure: the agent's model context is empty
 *  but the transcript is intact in SQLite, so the user can opt in to
 *  pre-filling the composer with a compact recap. See #1004. */
export function fetchContextPrimer(
  sessionId: string,
  beforeSeq: number,
  signal?: AbortSignal,
): Promise<ContextPrimerResponse | null> {
  const params = new URLSearchParams({ before_seq: String(beforeSeq) });
  return fetchJson<ContextPrimerResponse>(
    `/api/sessions/${encodeURIComponent(sessionId)}/acp/context-primer?${params.toString()}`,
    signal ? { signal } : undefined,
  );
}

// --- Devices ---

/** A persisted login session, surfaced as a connected device. Backed by
 *  the server's login-session store (#1235), so it survives a daemon
 *  restart. `current` flags the session making the request. */
export interface DeviceSession {
  session_id: string;
  user_agent: string;
  created_ip: string;
  created_at: string;
  last_seen: string;
  current: boolean;
}

export function fetchDevices(): Promise<DeviceSession[] | null> {
  return fetchJson<DeviceSession[]>("/api/devices");
}

/** Revoke a single device's login session. Elevation-gated: a 403
 *  elevation_required pops the global passphrase prompt (handled by the
 *  fetch interceptor) and this resolves false so the caller can ask the
 *  user to retry after confirming. */
export async function revokeDevice(sessionId: string): Promise<boolean> {
  try {
    const res = await fetch(`/api/login/sessions/${encodeURIComponent(sessionId)}`, { method: "DELETE" });
    return res.ok;
  } catch {
    return false;
  }
}

/** Sign every device out (the escape hatch that replaces "restart logs
 *  everyone out"). Ends this session too. Elevation-gated. */
export async function signOutAllDevices(): Promise<boolean> {
  try {
    const res = await fetch("/api/login/logout-all", { method: "POST" });
    return res.ok;
  } catch {
    return false;
  }
}

// --- Wizard APIs ---

export async function fetchAgents(): Promise<AgentInfo[]> {
  return (await fetchJson<AgentInfo[]>("/api/agents")) ?? [];
}

export async function fetchProfiles(): Promise<ProfileInfo[]> {
  return (await fetchJson<ProfileInfo[]>("/api/profiles")) ?? [];
}

export async function getHomePath(): Promise<string | null> {
  const data = await fetchJson<{ path?: string }>("/api/filesystem/home");
  return data?.path ?? null;
}

export async function browseFilesystem(
  path: string,
  limit?: number,
  filter?: string,
): Promise<BrowseResponse & { ok: boolean }> {
  const params = new URLSearchParams({ path });
  if (limit != null) params.set("limit", String(limit));
  if (filter) params.set("filter", filter);
  const data = await fetchJson<BrowseResponse>(`/api/filesystem/browse?${params}`);
  if (!data) return { entries: [], has_more: false, ok: false };
  return { ...data, ok: true };
}

export async function fetchGroups(): Promise<GroupInfo[]> {
  return (await fetchJson<GroupInfo[]>("/api/groups")) ?? [];
}

export async function fetchProjects(scope?: "global" | "profile"): Promise<ProjectInfo[]> {
  const url = scope ? `/api/projects?scope=${scope}` : "/api/projects";
  return (await fetchJson<ProjectInfo[]>(url)) ?? [];
}

export async function createProject(body: {
  path: string;
  name?: string;
  scope?: "global" | "profile";
  allow_override?: boolean;
  default_base_branch?: string;
}): Promise<{ ok: boolean; error?: string; project?: ProjectInfo }> {
  try {
    const res = await fetch("/api/projects", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    if (!res.ok) {
      const text = await res.text();
      try {
        const data = JSON.parse(text);
        return {
          ok: false,
          error: data.message || `Server error (${res.status})`,
        };
      } catch {
        return { ok: false, error: text || `Server error (${res.status})` };
      }
    }
    const project = (await res.json()) as ProjectInfo;
    return { ok: true, project };
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

export async function deleteProject(
  name: string,
  scope: "global" | "profile",
): Promise<{ ok: boolean; error?: string }> {
  try {
    const res = await fetch(`/api/projects/${encodeURIComponent(name)}?scope=${scope}`, { method: "DELETE" });
    if (!res.ok) {
      const text = await res.text();
      try {
        const data = JSON.parse(text);
        return {
          ok: false,
          error: data.message || `Server error (${res.status})`,
        };
      } catch {
        return { ok: false, error: text || `Server error (${res.status})` };
      }
    }
    return { ok: true };
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

/** Update a project's default base branch. Pass `null` to clear it. */
export async function updateProject(
  name: string,
  scope: "global" | "profile",
  defaultBaseBranch: string | null,
): Promise<{ ok: boolean; error?: string; project?: ProjectInfo }> {
  try {
    const res = await fetch(`/api/projects/${encodeURIComponent(name)}?scope=${scope}`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ default_base_branch: defaultBaseBranch }),
    });
    if (!res.ok) {
      const text = await res.text();
      try {
        const data = JSON.parse(text);
        return {
          ok: false,
          error: data.message || `Server error (${res.status})`,
        };
      } catch {
        return { ok: false, error: text || `Server error (${res.status})` };
      }
    }
    const project = (await res.json()) as ProjectInfo;
    return { ok: true, project };
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

export async function fetchDockerStatus(): Promise<DockerStatusResponse> {
  return (
    (await fetchJson<DockerStatusResponse>("/api/docker/status")) ?? {
      available: false,
      runtime: null,
    }
  );
}

/** The repo's hooks need approval before this session can be created
 *  (#2066). Surfaced from a `hooks_need_trust` 403 so the wizard can show
 *  the commands and resubmit with `trust_hooks: true`. */
export interface HooksNeedTrust {
  /** The `on_create` commands that will run once approved. */
  onCreate: string[];
  /** The `on_launch` commands the same approval trusts (run on every later
   *  session start, including TUI/CLI ones). */
  onLaunch: string[];
  /** The `on_destroy` commands the same approval trusts (run on delete). */
  onDestroy: string[];
  /** Whether the repo's `.mcp.json` also needs approval at this fingerprint. */
  needsMcpTrust: boolean;
}

export async function createSession(body: CreateSessionRequest): Promise<{
  ok: boolean;
  error?: string;
  session?: SessionResponse;
  hooksNeedTrust?: HooksNeedTrust;
}> {
  try {
    const res = await fetch("/api/sessions", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    if (!res.ok) {
      const text = await res.text();
      try {
        const data = JSON.parse(text);
        if (data.error === "hooks_need_trust") {
          return {
            ok: false,
            error: data.message || "Repository hooks require trust",
            hooksNeedTrust: {
              onCreate: Array.isArray(data.on_create) ? data.on_create : [],
              onLaunch: Array.isArray(data.on_launch) ? data.on_launch : [],
              onDestroy: Array.isArray(data.on_destroy) ? data.on_destroy : [],
              needsMcpTrust: data.needs_mcp_trust === true,
            },
          };
        }
        return {
          ok: false,
          error: data.message || `Server error (${res.status})`,
        };
      } catch {
        return {
          ok: false,
          error: `Server error (${res.status}): ${text.slice(0, 200)}`,
        };
      }
    }
    const data = await res.json();
    return { ok: true, session: data };
  } catch (e) {
    return {
      ok: false,
      error: `Network error: ${e instanceof Error ? e.message : "connection failed"}`,
    };
  }
}

// --- Clone ---

export async function cloneRepo(
  url: string,
  opts?: { destination?: string; shallow?: boolean },
): Promise<{ ok: boolean; path?: string; error?: string }> {
  try {
    const body: Record<string, unknown> = { url };
    if (opts?.destination) body.destination = opts.destination;
    if (opts?.shallow) body.shallow = true;
    const res = await fetch("/api/git/clone", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    const data = await res.json().catch(() => ({}));
    if (!res.ok) {
      return {
        ok: false,
        error: data.message || `Clone failed (${res.status})`,
      };
    }
    return { ok: true, path: data.path };
  } catch (e) {
    return {
      ok: false,
      error: `Network error: ${e instanceof Error ? e.message : "connection failed"}`,
    };
  }
}

// --- Login ---

export interface LoginStatus {
  required: boolean;
  authenticated: boolean;
  /** Whether the session currently sits inside the 15-minute step-up
   *  window. Sensitive routes (terminal attach, acp prompt /
   *  approval / file mutations) only execute while this is true.
   *  See #1131. */
  elevated: boolean;
  /** Seconds remaining on the current elevation window, or null when
   *  not elevated. */
  elevated_until_secs: number | null;
}

export async function loginStatus(): Promise<LoginStatus> {
  return (
    (await fetchJson<LoginStatus>("/api/login/status")) ?? {
      required: false,
      authenticated: true,
      elevated: true,
      elevated_until_secs: null,
    }
  );
}

/** Verify the auth token via a session-exempt endpoint (`/api/login/status`).
 *  Returning `true` means the token authenticated; the caller still has to
 *  consult `loginStatus()` to decide between the main app and LoginPage.
 *  Used by the token entry page so a valid-token-but-needs-passphrase paste
 *  is accepted instead of being misread as a token rejection. */
export async function verifyToken(): Promise<boolean> {
  try {
    const res = await fetch("/api/login/status");
    return res.ok;
  } catch {
    return false;
  }
}

export async function login(passphrase: string): Promise<{ ok: boolean; error?: string }> {
  let deviceBindingSecret: string;
  try {
    deviceBindingSecret = getOrCreateDeviceBindingSecret();
  } catch (err) {
    return {
      ok: false,
      error: err instanceof Error ? err.message : "Could not create device binding for this browser",
    };
  }
  try {
    const res = await fetch("/api/login", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        passphrase,
        device_binding_secret: deviceBindingSecret,
      }),
    });
    if (res.ok) return { ok: true };
    const data = await res.json().catch(() => null);
    return {
      ok: false,
      error: data?.message ?? `Login failed (${res.status})`,
    };
  } catch {
    return { ok: false, error: "Network error" };
  }
}

/**
 * Re-verify the passphrase to open a fresh 15-minute elevation
 * window. Required before the acp/terminal can perform
 * SSH-equivalent actions when the prior window has lapsed. See
 * #1131.
 *
 * Attaches the device-binding header explicitly rather than relying
 * on the global fetch interceptor; auth-sensitive endpoints should
 * not depend on monkey-patching to carry their second factor.
 */
export async function elevateLogin(
  passphrase: string,
): Promise<{ ok: boolean; error?: string; elevated_until_secs?: number }> {
  let bindingSecret: string;
  try {
    bindingSecret = getOrCreateDeviceBindingSecret();
  } catch (err) {
    return {
      ok: false,
      error: err instanceof Error ? err.message : "Could not access device binding for this browser",
    };
  }
  try {
    const res = await fetch("/api/login/elevate", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Aoe-Device-Binding": bindingSecret,
      },
      body: JSON.stringify({ passphrase }),
    });
    if (res.ok) {
      const data = (await res.json().catch(() => null)) as {
        elevated_until_secs?: number;
      } | null;
      return {
        ok: true,
        elevated_until_secs: data?.elevated_until_secs,
      };
    }
    const data = await res.json().catch(() => null);
    return {
      ok: false,
      error: data?.message ?? `Elevation failed (${res.status})`,
    };
  } catch {
    return { ok: false, error: "Network error" };
  }
}

export async function logout(): Promise<void> {
  try {
    await fetch("/api/logout", { method: "POST" });
  } catch {
    // Best effort
  } finally {
    // Drop the per-device binding secret so a future login generates
    // a fresh one alongside the new session cookie. Without this, an
    // attacker who later obtains a stale localStorage snapshot still
    // holds a valid binding for the next session created on this
    // browser. See #1131.
    try {
      clearDeviceBindingSecret();
    } catch {
      // ignore
    }
    // Drop the in-memory approval-sound caches so a future user on the
    // same tab does not see the previous user's settings snapshot or
    // hear their cached blob.
    try {
      const { clearApprovalSoundCache } = await import("../hooks/useApprovalSound");
      clearApprovalSoundCache();
    } catch {
      // ignore
    }
  }
}

/**
 * Rename a session's title. When the session is a tied aoe-managed worktree
 * (session.tie_workdir_to_name), the server also moves the worktree directory
 * to match and returns 409 if the session is running, so the message is
 * surfaced to the caller. See #1927.
 */
export async function renameSession(id: string, title: string): Promise<{ ok: boolean; message?: string }> {
  try {
    const res = await fetch(`/api/sessions/${id}`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ title }),
    });
    if (res.ok) return { ok: true };
    let message: string | undefined;
    try {
      const body = await res.json();
      message = typeof body?.message === "string" ? body.message : undefined;
    } catch {
      // non-JSON error body; fall through with no message
    }
    return { ok: false, message };
  } catch {
    return { ok: false };
  }
}

/**
 * Edit a managed worktree session's workdir name: move the worktree
 * directory and, optionally, rename its git branch. The session must not be
 * running. Returns the server's validation message on failure so the caller
 * can surface it. See #1723.
 */
export async function setWorktreeName(
  id: string,
  name: string,
  renameBranch: boolean,
): Promise<{ ok: boolean; message?: string }> {
  try {
    const res = await fetch(`/api/sessions/${id}/worktree-name`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name, rename_branch: renameBranch }),
    });
    if (res.ok) return { ok: true };
    let message: string | undefined;
    try {
      const body = await res.json();
      message = typeof body?.message === "string" ? body.message : undefined;
    } catch {
      // non-JSON error body; fall through with no message
    }
    return { ok: false, message };
  } catch {
    return { ok: false };
  }
}

/** Move an existing session to another group, create a new group by
 *  passing a path that does not exist yet, or clear the group with an
 *  empty string (the ungroup sentinel, matching session creation and the
 *  TUI). Hits the dedicated `PATCH /api/sessions/:id/group` sub-route. */
export async function updateSessionGroup(id: string, group: string): Promise<boolean> {
  try {
    const res = await fetch(`/api/sessions/${encodeURIComponent(id)}/group`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ group }),
    });
    return res.ok;
  } catch {
    return false;
  }
}

/** Three-preset helper for the sidebar context menu:
 *  - "off":     set all three overrides to false (silence this session)
 *  - "default": clear all three overrides (inherit server defaults)
 *  - "all":     set all three overrides to true (notify on any event)
 *  Sends all three fields in one PATCH to avoid multi-request ordering. */
export async function setSessionNotifications(id: string, preset: "off" | "default" | "all"): Promise<boolean> {
  const value = preset === "off" ? false : preset === "all" ? true : null;
  try {
    const res = await fetch(`/api/sessions/${id}/notifications`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        notify_on_waiting: value,
        notify_on_idle: value,
        notify_on_error: value,
      }),
    });
    return res.ok;
  } catch {
    return false;
  }
}

/** Set the per-session diff-base override. Pass `null` to clear the
 *  override and fall back to the profile default / auto-detection.
 *  See #970. */
export async function setSessionDiffBase(id: string, baseBranch: string | null): Promise<SessionResponse | null> {
  try {
    const res = await fetch(`/api/sessions/${id}/diff-base`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ base_branch: baseBranch }),
    });
    if (!res.ok) return null;
    return (await res.json()) as SessionResponse;
  } catch {
    return null;
  }
}

/** Toggle the web-only "pin" marker on a session. Pinned workspaces sink
 *  to the top of the sidebar in all sort modes (manual and lastActivity).
 *  Distinct from the TUI favorite signal. See #1581. */
export async function setSessionPin(id: string, pinned: boolean): Promise<SessionResponse | null> {
  try {
    const res = await fetch(`/api/sessions/${id}/pin`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ pinned }),
    });
    if (!res.ok) return null;
    return (await res.json()) as SessionResponse;
  } catch {
    return null;
  }
}

/** Archive or unarchive a session. On archive, the server kills the tmux
 *  pane (when `killPane` is true or omitted, matching TUI/CLI semantics)
 *  and shuts down the acp worker for acp-mode sessions; the
 *  reconciler will not respawn it because archived sessions are excluded
 *  from the resume target list. Sending a message via the dashboard
 *  auto-unarchives via the existing `touch_last_accessed` invariant in
 *  the send handler. See #1581. */
export async function setSessionArchive(
  id: string,
  archived: boolean,
  killPane = true,
): Promise<SessionResponse | null> {
  try {
    const res = await fetch(`/api/sessions/${id}/archive`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ archived, kill_pane: killPane }),
    });
    if (!res.ok) return null;
    return (await res.json()) as SessionResponse;
  } catch {
    return null;
  }
}

/** Snooze or unsnooze a session. Pass `null` to unsnooze, or a positive
 *  number of minutes between 1 and 43200 (30 days) to snooze. The server
 *  validates against the shared `validate_snooze_duration` so the bounds
 *  match the TUI dialog presets and the CLI's `aoe session snooze`. See
 *  #1581. */
export async function setSessionSnooze(id: string, minutes: number | null): Promise<SessionResponse | null> {
  try {
    const res = await fetch(`/api/sessions/${id}/snooze`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ minutes }),
    });
    if (!res.ok) return null;
    return (await res.json()) as SessionResponse;
  } catch {
    return null;
  }
}

export interface DeleteSessionOptions {
  delete_worktree?: boolean;
  delete_branch?: boolean;
  delete_sandbox?: boolean;
  force_delete?: boolean;
  /** For scratch sessions, keep the scratch directory on disk instead of
   *  removing it. The session record is still deleted. No effect on
   *  non-scratch sessions. */
  keep_scratch?: boolean;
}

export interface DeleteSessionResult {
  ok: boolean;
  error?: string;
  messages?: string[];
}

export async function deleteSession(id: string, options: DeleteSessionOptions = {}): Promise<DeleteSessionResult> {
  try {
    const res = await fetch(`/api/sessions/${id}`, {
      method: "DELETE",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(options),
    });
    if (!res.ok) {
      const data = await res.json().catch(() => ({}));
      return {
        ok: false,
        error: data.message || `Server error (${res.status})`,
      };
    }
    const data = (await res.json().catch(() => ({}))) as {
      messages?: string[];
    };
    return { ok: true, messages: data.messages };
  } catch (e) {
    return {
      ok: false,
      error: `Network error: ${e instanceof Error ? e.message : "connection failed"}`,
    };
  }
}

// --- MCP servers (#1996) ---

export interface McpServerView {
  name: string;
  transport: string;
  command?: string;
  args?: string[];
  url?: string;
  envNames?: string[];
  headerNames?: string[];
  provenance: string;
  shadowed?: string[];
}

export interface McpConflictView {
  name: string;
  agent: string;
  previous: string;
  current: string;
  fingerprint: string;
}

export interface McpServersResponse {
  agent: string;
  effective: McpServerView[];
  keptOnRemoval: McpServerView[];
  conflicts: McpConflictView[];
  driftPaused: boolean;
}

export function fetchMcpServers(agent?: string): Promise<McpServersResponse | null> {
  const q = agent ? `?agent=${encodeURIComponent(agent)}` : "";
  return fetchJson<McpServersResponse>(`/api/mcp/servers${q}`);
}

async function postMcp(url: string, body: unknown): Promise<Response | null> {
  try {
    return await fetch(url, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
  } catch {
    return null;
  }
}

export type McpResolveResult = "applied" | "stale" | "error";

export async function resolveMcpConflict(
  name: string,
  agent: string,
  winner: "aoe" | "native",
  fingerprint: string,
): Promise<McpResolveResult> {
  const res = await postMcp(`/api/mcp/servers/${encodeURIComponent(name)}/resolve`, {
    agent,
    winner,
    fingerprint,
  });
  if (!res) return "error";
  if (res.ok) return "applied";
  if (res.status === 409) return "stale";
  return "error";
}

export async function keepMcpServer(name: string, agent: string): Promise<boolean> {
  const res = await postMcp(`/api/mcp/servers/${encodeURIComponent(name)}/keep`, { agent });
  return !!res && res.ok;
}

export async function dropMcpServer(name: string, agent: string): Promise<boolean> {
  const res = await postMcp(`/api/mcp/servers/${encodeURIComponent(name)}/drop`, { agent });
  return !!res && res.ok;
}
