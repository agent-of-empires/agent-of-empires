import type { AgentLifecycleInfo } from "./agentProfiles";
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
  ProjectOverrides,
  DockerStatusResponse,
  CreateSessionRequest,
  ClaudeSessionSummary,
  SettingsFieldDescriptor,
} from "./types";
import type { ConfigOptionDescriptor } from "./acpTypes";
import type { ResolvedTheme } from "./theme";
import { clearDeviceBindingSecret, getOrCreateDeviceBindingSecret } from "./deviceBinding";

// --- Request helpers ---

type Payload = Record<string, unknown>;

interface Reply {
  ok: boolean;
  status: number;
  /** Parsed JSON body, or null when the body is not JSON. */
  payload: Payload | null;
}

/** Null on non-2xx or network/parse errors. */
async function fetchJson<T>(url: string, init?: RequestInit): Promise<T | null> {
  try {
    const res = await fetch(url, init);
    if (!res.ok) return null;
    return (await res.json()) as T;
  } catch {
    return null;
  }
}

async function fetchOk(url: string, init?: RequestInit): Promise<boolean> {
  try {
    return (await fetch(url, init)).ok;
  } catch {
    return false;
  }
}

/** Rejects only on a network failure. */
async function send(url: string, init?: RequestInit): Promise<Reply> {
  const res = await fetch(url, init);
  const payload = (await res.json().catch(() => null)) as Payload | null;
  return { ok: res.ok, status: res.status, payload };
}

const jsonInit = (method: string, body?: unknown, headers: Record<string, string> = {}): RequestInit => ({
  method,
  headers: { "Content-Type": "application/json", ...headers },
  body: JSON.stringify(body),
});

const ACCEPT_JSON = { Accept: "application/json" };

const stringField = (payload: Payload | null | undefined, key: string): string | undefined =>
  typeof payload?.[key] === "string" ? (payload[key] as string) : undefined;

/** The unchecked `message` of an error body. */
const rawMessage = (payload: Payload | null | undefined) => payload?.message as string | undefined;

const networkError = (e: unknown) => `Network error: ${e instanceof Error ? e.message : "connection failed"}`;

type Failure = { kind: "error"; message: string };

/** Map a reply to `toOk`'s variant, or a failure carrying the server message. */
async function kindRequest<Ok>(
  url: string,
  init: RequestInit,
  toOk: (payload: Payload) => Ok | null,
  fallback: string | ((status: number) => string),
): Promise<Ok | Failure> {
  const reply = await send(url, init).catch(() => null);
  if (!reply) return { kind: "error", message: "Network error." };
  const ok = reply.ok && reply.payload ? toOk(reply.payload) : null;
  if (ok) return ok;
  const message =
    stringField(reply.payload, "message") ??
    (typeof fallback === "string" ? `${fallback} (HTTP ${reply.status}).` : fallback(reply.status));
  return { kind: "error", message };
}

/** `{ ok }` plus the server's string `message` on failure. */
async function okWithMessage(url: string, init: RequestInit): Promise<{ ok: boolean; message?: string }> {
  const reply = await send(url, init).catch(() => null);
  if (!reply) return { ok: false };
  return reply.ok ? { ok: true } : { ok: false, message: stringField(reply.payload, "message") };
}

// --- Sessions ---

export interface SessionsEnvelope {
  sessions: SessionResponse[];
  workspace_ordering: string[];
}

export function fetchSessions(): Promise<SessionsEnvelope | null> {
  return fetchJson<SessionsEnvelope>("/api/sessions");
}

export interface ConversationSearchHit {
  session_id: string;
  seq: number;
  kind: string;
  snippet: string;
  match_count: number;
}

/** Full-text search over conversations: one hit per matching session, newest first. */
export async function searchConversations(query: string, signal?: AbortSignal): Promise<ConversationSearchHit[]> {
  const res = await fetchJson<{ results: ConversationSearchHit[] }>(
    `/api/sessions/search?q=${encodeURIComponent(query)}`,
    { signal },
  );
  return res?.results ?? [];
}

// --- Recent projects ---

export interface RecentProjectEntry {
  path: string;
  display_name: string;
  tool: string;
  last_used_at: string;
}

export interface RecentProjectsEnvelope {
  projects: RecentProjectEntry[];
}

export function fetchRecentProjects(): Promise<RecentProjectsEnvelope | null> {
  return fetchJson<RecentProjectsEnvelope>("/api/recent-projects");
}

export function updateWorkspaceOrdering(order: string[]): Promise<boolean> {
  return fetchOk("/api/workspace-ordering", jsonInit("PUT", { order }));
}

export interface EnsureSessionResult {
  ok: boolean;
  status?: "alive" | "restarted";
  error?: string;
  message?: string;
}

export async function ensureSession(id: string, signal?: AbortSignal): Promise<EnsureSessionResult> {
  try {
    const { ok, status, payload } = await send(`/api/sessions/${id}/ensure`, { method: "POST", signal });
    if (!ok) {
      return {
        ok: false,
        error: stringField(payload, "error"),
        message: stringField(payload, "message") ?? `Server error (${status})`,
      };
    }
    return {
      ok: true,
      status: payload?.status as "alive" | "restarted" | undefined,
      message: stringField(payload, "message"),
    };
  } catch (e) {
    if ((e as { name?: string }).name === "AbortError") return { ok: false, error: "aborted" };
    return { ok: false, message: e instanceof Error ? e.message : "Network error" };
  }
}

export function ensureTerminal(id: string, index = 0, container = false): Promise<boolean> {
  const path = container ? "container-terminal" : "terminal";
  return fetchOk(`/api/sessions/${id}/${path}?index=${index}`, { method: "POST" });
}

function fileToBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(reader.error ?? new Error("read failed"));
    reader.onload = () => {
      const result = reader.result as string;
      // The server wants raw base64 without the `data:<mime>;base64,` prefix.
      const comma = result.indexOf(",");
      resolve(comma >= 0 ? result.slice(comma + 1) : result);
    };
    reader.readAsDataURL(file);
  });
}

/** Upload a clipboard image pasted into the live terminal; returns the host path the agent can read. */
export async function pasteImage(id: string, file: File): Promise<string | null> {
  try {
    const data = await fileToBase64(file);
    const body = await fetchJson<Payload>(
      `/api/sessions/${id}/paste-image`,
      jsonInit("POST", { mime_type: file.type, data }),
    );
    return stringField(body, "path") ?? null;
  } catch {
    return null;
  }
}

/** Kill an extra terminal tab (index >= 1); the server rejects the primary terminal. */
export function killTerminal(id: string, index: number): Promise<boolean> {
  return fetchOk(`/api/sessions/${id}/terminal?index=${index}`, { method: "DELETE" });
}

export function getSessionDiffFiles(id: string): Promise<RichDiffFilesResponse | null> {
  return fetchJson<RichDiffFilesResponse>(`/api/sessions/${id}/diff/files`);
}

export function getSessionFileContents(
  id: string,
  filePath: string,
  repoName?: string,
): Promise<RichFileContentsResponse | null> {
  const params = new URLSearchParams({ path: filePath });
  if (repoName) params.set("repo", repoName);
  return fetchJson<RichFileContentsResponse>(`/api/sessions/${id}/diff/file?${params.toString()}`);
}

/** URL of a diffed file's current worktree bytes, for opening in a new tab. */
export function sessionDiffRawFileUrl(id: string, filePath: string, repoName?: string): string {
  const params = new URLSearchParams({ path: filePath });
  if (repoName) params.set("repo", repoName);
  return `/api/sessions/${id}/diff/file/raw?${params.toString()}`;
}

export interface SessionFileResponse {
  content: string;
  is_binary: boolean;
  truncated: boolean;
}

/** Read a session file; the server confines the path to the session's provenance. */
export function getSessionFile(id: string, filePath: string): Promise<SessionFileResponse | null> {
  const params = new URLSearchParams({ path: filePath });
  return fetchJson<SessionFileResponse>(`/api/sessions/${id}/file?${params.toString()}`);
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

export interface SystemHealthAgent {
  id: string;
  title: string;
  cpu_fraction: number | null;
  memory_bytes: number | null;
  procs: number | null;
  sandboxed: boolean;
}

export interface SystemHealth {
  status: "ok" | "warn" | "critical";
  cpu_fraction: number | null;
  memory_used_bytes: number;
  memory_total_bytes: number;
  load_average: [number, number, number] | null;
  swap_used_bytes: number;
  swap_total_bytes: number;
  agent_count: number;
  proc_count: number;
  agents: SystemHealthAgent[];
}

export function fetchSystemHealth(): Promise<SystemHealth | null> {
  return fetchJson<SystemHealth>("/api/system/health");
}

export function fetchSettings(profile?: string): Promise<SettingsResponse | null> {
  const params = profile ? `?profile=${encodeURIComponent(profile)}` : "";
  return fetchJson<SettingsResponse>(`/api/settings${params}`);
}

/** This install's CityHall config bundle as TOML; throws with the server's message. */
export async function fetchCityHallBundle(): Promise<string> {
  const res = await fetch("/api/cityhall/bundle");
  if (!res.ok) {
    const detail = await res.json().catch(() => null);
    throw new Error(detail?.message ?? `Export failed (HTTP ${res.status})`);
  }
  return res.text();
}

// The schema is static per server run; only a successful fetch is cached.
let schemaPromise: Promise<SettingsFieldDescriptor[] | null> | null = null;

export function getSettingsSchema(): Promise<SettingsFieldDescriptor[] | null> {
  if (!schemaPromise) {
    schemaPromise = fetchJson<SettingsFieldDescriptor[]>("/api/settings/schema").then((s) => {
      if (!s) schemaPromise = null;
      return s;
    });
  }
  return schemaPromise;
}

/** Test-only seam: drop the cached schema. */
export function resetSettingsSchemaCache(): void {
  schemaPromise = null;
}

// --- Plugins ---

export interface PluginView {
  id: string;
  name: string;
  version: string;
  description: string;
  /** Lucide kebab-case icon name. */
  icon: string | null;
  icon_asset_url: string | null;
  enabled: boolean;
  builtin: boolean;
  /** "builtin" | "featured" | "community" | "local". */
  validation: string;
  source: string | null;
  capabilities: string[];
  ui_contributions: { slot: string; id: string }[];
  granted: boolean;
  needs_reapproval: boolean;
}

export interface PluginListResponse {
  plugins: PluginView[];
  load_errors: string[];
}

export type PluginToggleResult = { kind: "ok"; data: PluginListResponse } | Failure;

export function fetchPlugins(): Promise<PluginListResponse | null> {
  return fetchJson<PluginListResponse>("/api/plugins");
}

/** `open-ui-link` opens the `href` from the plugin's UI-state entry at `(slot, id)`. */
export type PluginClientAction = { kind: "open-ui-link"; slot: PluginUiSlot; id: string };

export interface PluginCommand {
  fqid: string;
  plugin_id: string;
  id: string;
  title: string;
  description: string;
  keybinds: string[];
  action: PluginClientAction | null;
}

export interface PluginCommandsResponse {
  commands: PluginCommand[];
}

export function fetchPluginCommands(): Promise<PluginCommandsResponse | null> {
  return fetchJson<PluginCommandsResponse>("/api/plugins/commands");
}

/** `available` is a short commit for an outdated GitHub source, "modified" for a changed local tree, or null. */
export interface PluginUpdateStatus {
  id: string;
  source: string;
  current: string;
  available: string | null;
  needs_update: boolean;
  error: string | null;
}

export type PluginUpdatesResult = { kind: "ok"; updates: PluginUpdateStatus[] } | Failure;

export function fetchPluginUpdates(): Promise<PluginUpdatesResult> {
  return kindRequest(
    "/api/plugins/updates",
    { headers: ACCEPT_JSON },
    (p) => (Array.isArray(p.updates) ? { kind: "ok" as const, updates: p.updates as PluginUpdateStatus[] } : null),
    "Update check failed",
  );
}

export interface PluginDiscoveryResult {
  slug: string;
  html_url: string;
  description: string | null;
  stars: number;
  badge: "installed" | "featured" | "unvetted";
  install_command: string;
  /** The repo owner's GitHub avatar, not the plugin's own icon. */
  source_avatar_url: string;
}

export type DiscoverResult = { kind: "ok"; results: PluginDiscoveryResult[] } | Failure;

/** Search the `aoe-plugin` GitHub topic. */
export function discoverPlugins(query: string): Promise<DiscoverResult> {
  const qs = query.trim() ? `?q=${encodeURIComponent(query.trim())}` : "";
  return kindRequest(
    `/api/plugins/discover${qs}`,
    { headers: ACCEPT_JSON },
    (p) => (Array.isArray(p.results) ? { kind: "ok" as const, results: p.results as PluginDiscoveryResult[] } : null),
    "Discovery failed",
  );
}

export interface PluginDetailManifest {
  id: string;
  name: string;
  version: string;
  description: string;
  api_version: number;
  capabilities: string[];
  ui_contributions: { slot: string; id: string }[];
  screenshots: { src: string; alt: string; caption: string }[];
  icon: string | null;
  icon_asset_url: string | null;
}

export interface PluginDetail {
  source: string;
  manifest: PluginDetailManifest | null;
  manifest_error: string | null;
  release_tags: string[];
}

export type PluginDetailResult = { kind: "ok"; detail: PluginDetail } | Failure;

export function fetchPluginDetails(source: string): Promise<PluginDetailResult> {
  return kindRequest(
    `/api/plugins/details?source=${encodeURIComponent(source)}`,
    { headers: ACCEPT_JSON },
    (p) => (typeof p.source === "string" ? { kind: "ok" as const, detail: p as unknown as PluginDetail } : null),
    "Details failed",
  );
}

export interface PluginUpdateUiView {
  slot: string;
  id: string;
}

/** Mirrors Rust `ChangelogEntry`. */
export type PluginChangelogEntry =
  | { kind: "release"; tag: string; body: string | null; published_at: string | null }
  | { kind: "commit"; sha: string; subject: string; url: string | null };

/** Mirrors Rust `UpdateChangelog`; `unavailable_reason` separates "could not load" from "no entries". */
export interface PluginUpdateChangelog {
  entries: PluginChangelogEntry[];
  truncated: boolean;
  unavailable_reason: string | null;
  more_url: string | null;
}

/** Mirrors Rust `UpdateConsent`. */
export interface PluginUpdateConsent {
  id: string;
  from_version: string;
  to_version: string;
  prior_capabilities: string[];
  new_capabilities: string[];
  added_capabilities: string[];
  removed_capabilities: string[];
  ui: PluginUpdateUiView[];
  build_steps: string[];
  runtime_change: string | null;
  trust_downgrade: boolean;
  fingerprint: string;
  stays_active_if_declined: boolean;
  changelog: PluginUpdateChangelog;
}

/** Mirrors Rust `UpdatePreview`. */
export type PluginUpdatePreview =
  | { kind: "no_update" }
  | { kind: "safe_update"; to_version: string; fingerprint: string; changelog: PluginUpdateChangelog }
  | { kind: "consent_required"; consent: PluginUpdateConsent; dismissed: boolean };

export type PluginUpdatePreviewResult = { kind: "ok"; preview: PluginUpdatePreview } | Failure;

/** Reject drifted payloads: an apply needs the fingerprint and the modal needs the consent. */
function isValidPreview(payload: Payload): payload is PluginUpdatePreview {
  switch (payload.kind) {
    case "no_update":
      return true;
    case "safe_update":
      return typeof payload.fingerprint === "string";
    case "consent_required":
      return typeof payload.consent === "object" && payload.consent !== null;
    default:
      return false;
  }
}

export function previewPluginUpdate(id: string): Promise<PluginUpdatePreviewResult> {
  return kindRequest(
    `/api/plugins/${encodeURIComponent(id)}/update/preview`,
    { headers: ACCEPT_JSON },
    (p) => (isValidPreview(p) ? { kind: "ok" as const, preview: p } : null),
    "Update preview failed",
  );
}

/** Start an update job pinned to the previewed fingerprint; a moved remote fails the job. */
export function applyPluginUpdate(id: string, expectedFingerprint: string | null): Promise<PluginJobStartResult> {
  return startPluginJob(`/api/plugins/${encodeURIComponent(id)}/update/apply`, {
    expected_fingerprint: expectedFingerprint,
  });
}

/** Mirrors Rust `InstallConsent`. */
export interface PluginInstallConsent {
  id: string;
  version: string;
  source: string;
  notice: string;
  /** The source is off the audited-release default path. */
  unverified: boolean;
  /** "featured" | "community" | "local". */
  validation: string;
  capabilities: string[];
  ui: PluginUpdateUiView[];
  build_steps: string[];
  fingerprint: string;
}

export type PluginInstallPreviewResult = { kind: "ok"; consent: PluginInstallConsent } | Failure;

export function previewPluginInstall(source: string): Promise<PluginInstallPreviewResult> {
  return kindRequest(
    "/api/plugins/install/preview",
    jsonInit("POST", { source }, ACCEPT_JSON),
    (p) =>
      typeof p.fingerprint === "string" ? { kind: "ok" as const, consent: p as unknown as PluginInstallConsent } : null,
    "Install preview failed",
  );
}

export type PluginJobStartResult = { kind: "ok"; jobId: string } | Failure;

function startPluginJob(url: string, body: unknown): Promise<PluginJobStartResult> {
  return kindRequest(
    url,
    jsonInit("POST", body, ACCEPT_JSON),
    (p) => (typeof p.job_id === "string" ? { kind: "ok" as const, jobId: p.job_id } : null),
    "Request failed",
  );
}

export function startPluginInstall(source: string, expectedFingerprint: string): Promise<PluginJobStartResult> {
  return startPluginJob("/api/plugins/install", { source, expected_fingerprint: expectedFingerprint });
}

export function startPluginUninstall(id: string): Promise<PluginJobStartResult> {
  return startPluginJob(`/api/plugins/${encodeURIComponent(id)}/uninstall`, {});
}

/** Mirrors Rust `PluginJobStatus`. */
export type PluginJobState = { state: "running" } | { state: "succeeded" } | { state: "failed"; error: string };

export interface PluginJob {
  job: {
    id: string;
    kind: "install" | "update" | "uninstall";
    target: string;
    status: PluginJobState;
    started_at: number;
    finished_at: number | null;
  };
  log: {
    exists: boolean;
    tail: string;
    lines_returned: number;
    truncated: boolean;
  };
}

export type PluginJobResult = { kind: "ok"; job: PluginJob } | { kind: "error"; status: number; message: string };

/** Job status plus a log tail. `status` lets the caller tell a terminal 404 from a transient failure. */
export async function fetchPluginJob(jobId: string, tail = 200): Promise<PluginJobResult> {
  const reply = await send(`/api/plugins/jobs/${encodeURIComponent(jobId)}?tail=${tail}`, {
    headers: ACCEPT_JSON,
  }).catch(() => null);
  if (!reply) return { kind: "error", status: 0, message: "Network error." };
  const { ok, status, payload } = reply;
  if (ok && typeof payload?.job === "object" && payload.job !== null) {
    return { kind: "ok", job: payload as unknown as PluginJob };
  }
  return { kind: "error", status, message: stringField(payload, "message") ?? `Job status failed (HTTP ${status}).` };
}

export type PluginDismissResult = { kind: "ok" } | Failure;

/** Decline an available update until the next version. */
export async function dismissPluginUpdate(id: string, fingerprint: string): Promise<PluginDismissResult> {
  const reply = await send(
    `/api/plugins/${encodeURIComponent(id)}/update/dismiss`,
    jsonInit("POST", { fingerprint }),
  ).catch(() => null);
  if (!reply) return { kind: "error", message: "Network error." };
  if (reply.ok) return { kind: "ok" };
  return { kind: "error", message: stringField(reply.payload, "message") ?? `Dismiss failed (HTTP ${reply.status}).` };
}

// --- Plugin UI extension points ---

export type PluginUiTone = "neutral" | "info" | "success" | "warn" | "danger";

export type PluginUiSlot =
  | "status-bar"
  | "row-badge"
  | "row-column"
  | "sort-key"
  | "filter-facet"
  | "card"
  | "pane"
  | "composer-action"
  | "detail-badge"
  | "home-pane"
  | "notification";

/** `payload` shape is determined by `slot`. */
export interface PluginUiEntry {
  plugin_id: string;
  slot: PluginUiSlot;
  id: string;
  session_id?: string;
  payload: Record<string, unknown>;
}

/** `seq` is monotonic so each notification toasts once. */
export interface PluginUiNotification {
  seq: number;
  plugin_id: string;
  tone: PluginUiTone;
  title: string;
  body?: string;
  session_id?: string;
  /** http/https URL opened on click, since browsers block `window.open` from an async push. */
  href?: string;
}

export interface PluginUiState {
  entries: PluginUiEntry[];
  notifications: PluginUiNotification[];
  /** Mutation counter per plugin, then per scope (session id, or `""` for global). Absent on older daemons. */
  revisions?: Record<string, Record<string, number>>;
}

export function fetchPluginUiState(): Promise<PluginUiState | null> {
  return fetchJson<PluginUiState>("/api/plugins/ui-state");
}

export function setPluginEnabled(id: string, enabled: boolean): Promise<PluginToggleResult> {
  return kindRequest(
    `/api/plugins/${encodeURIComponent(id)}/enabled`,
    jsonInit("POST", { enabled }),
    (p) =>
      Array.isArray(p.plugins) && Array.isArray(p.load_errors)
        ? { kind: "ok" as const, data: p as unknown as PluginListResponse }
        : null,
    (status) => `Failed to ${enabled ? "enable" : "disable"} plugin (${status}).`,
  );
}

/** `baselineRevision` is the scope's UI mutation counter before forwarding; null when the daemon omits it. */
export interface PluginActionAccepted {
  baselineRevision: number | null;
}

/** Forward a plugin UI action to its worker. Null on 403, 404 (no worker), or network failure. */
export async function invokePluginAction(
  pluginId: string,
  method: string,
  sessionId?: string,
  params: Record<string, unknown> = {},
): Promise<PluginActionAccepted | null> {
  const reply = await send(
    `/api/plugins/${encodeURIComponent(pluginId)}/action`,
    jsonInit("POST", { method, params, session_id: sessionId ?? null }),
  ).catch(() => null);
  if (!reply?.ok) return null;
  // A missing baseline is a sentinel, not revision 0, which would wedge the spinner.
  const rev = reply.payload?.baseline_revision;
  return { baselineRevision: typeof rev === "number" ? rev : null };
}

/** Invoke an action-less plugin command; true when the daemon dispatched it. */
export function invokePluginCommand(fqid: string, sessionId: string): Promise<boolean> {
  return fetchOk(
    `/api/plugins/commands/${encodeURIComponent(fqid)}/invoke`,
    jsonInit("POST", { session_id: sessionId }),
  );
}

export function updateSettings(updates: Record<string, unknown>): Promise<boolean> {
  return fetchOk("/api/settings", jsonInit("PATCH", updates));
}

// Theme, tour, tips and acknowledgement flags use dedicated endpoints so these
// cosmetic writes stay off the passphrase/elevation wall of PATCH /api/settings.

export function updateTheme(patch: { name?: string; color_mode?: string }): Promise<boolean> {
  return fetchOk("/api/theme", jsonInit("PATCH", patch));
}

export function markWebTourSeen(): Promise<boolean> {
  return fetchOk("/api/app-state/web-tour-seen", { method: "POST" });
}

// --- Tips ---

export interface TipDto {
  id: string;
  title: string;
  body: string;
  seen: boolean;
}

export interface TipsResponse {
  /** Mirror of `session.show_tips`. */
  enabled: boolean;
  tips: TipDto[];
}

export function fetchTips(): Promise<TipsResponse | null> {
  return fetchJson<TipsResponse>("/api/tips");
}

export function markTipSeen(id: string): Promise<boolean> {
  return fetchOk("/api/app-state/tip-seen", jsonInit("POST", { id }));
}

export function setShowTips(enabled: boolean): Promise<boolean> {
  return fetchOk("/api/tips/show", jsonInit("POST", { enabled }));
}

// --- Web UI state sync ---

/** Server-side mirror of synced localStorage keys. */
export function getWebUiState(): Promise<Record<string, string> | null> {
  return fetchJson<Record<string, string>>("/api/app-state/web-ui-state");
}

/** `null` values delete the key. */
export function patchWebUiState(patch: Record<string, string | null>): Promise<boolean> {
  return fetchOk("/api/app-state/web-ui-state", jsonInit("PATCH", patch));
}

// --- Sandbox volume_ignores glob expansion ---

export interface VolumeIgnoresGlobPreview {
  pattern: string;
  /** Container-side paths the pattern currently matches. */
  matched_paths: string[];
}

export interface VolumeIgnoresPreviewResponse {
  acknowledged: boolean;
  globs: VolumeIgnoresGlobPreview[];
}

/** Dry-run glob `volume_ignores` expansion for a sandbox rooted at `path`. */
export function fetchVolumeIgnoresPreview(
  path: string,
  profile?: string,
): Promise<VolumeIgnoresPreviewResponse | null> {
  const params = new URLSearchParams({ path });
  if (profile) params.set("profile", profile);
  return fetchJson<VolumeIgnoresPreviewResponse>(`/api/sandbox/volume-ignores-preview?${params.toString()}`);
}

export function markVolumeIgnoresGlobsAcknowledged(): Promise<boolean> {
  return fetchOk("/api/app-state/volume-ignores-globs-acknowledged", { method: "POST" });
}

// --- Profile management ---

export function createProfile(name: string): Promise<boolean> {
  return fetchOk("/api/profiles", jsonInit("POST", { name }));
}

export function deleteProfile(name: string): Promise<boolean> {
  return fetchOk(`/api/profiles/${encodeURIComponent(name)}`, { method: "DELETE" });
}

export function renameProfile(name: string, newName: string): Promise<boolean> {
  return fetchOk(`/api/profiles/${encodeURIComponent(name)}/rename`, jsonInit("PATCH", { new_name: newName }));
}

export function setDefaultProfile(name: string): Promise<boolean> {
  return fetchOk("/api/default-profile", jsonInit("PATCH", { name }));
}

export function getProfileSettings(name: string): Promise<ProfileSettingsResponse | null> {
  return fetchJson<ProfileSettingsResponse>(`/api/profiles/${encodeURIComponent(name)}/settings`);
}

/** Sections a profile PATCH may touch: every schema section plus `description`. Sections absent
 *  from the schema (hooks, agent commands, env) are RCE surfaces the server also rejects. */
export function profileWritableSections(schema: SettingsFieldDescriptor[]): Set<string> {
  const sections = new Set(schema.map((d) => d.section));
  sections.add("description");
  return sections;
}

/** PATCH profile settings, refusing blocked sections. Without a schema, defers to the server's guard. */
export async function updateProfileSettings(name: string, updates: Record<string, unknown>): Promise<boolean> {
  const schema = await getSettingsSchema();
  if (schema) {
    const writable = profileWritableSections(schema);
    const blocked = Object.keys(updates).find((key) => !writable.has(key));
    if (blocked !== undefined) {
      console.error(`updateProfileSettings: refusing to send blocked profile section "${blocked}"`);
      return false;
    }
  }
  return fetchOk(`/api/profiles/${encodeURIComponent(name)}/settings`, jsonInit("PATCH", updates));
}

// --- Themes & Sounds ---

export async function fetchThemes(): Promise<string[]> {
  return (await fetchJson<string[]>("/api/themes")) ?? [];
}

/** The server falls back to Empire for unknown names; check `source` to detect. */
export function fetchResolvedTheme(name: string): Promise<ResolvedTheme | null> {
  return fetchJson<ResolvedTheme>(`/api/themes/${encodeURIComponent(name)}`);
}

export function fetchCurrentTheme(): Promise<ResolvedTheme | null> {
  return fetchJson<ResolvedTheme>("/api/theme/current");
}

export async function fetchSounds(): Promise<string[]> {
  return (await fetchJson<string[]>("/api/sounds")) ?? [];
}

/** Fetched as a Blob because `<audio src>` would skip the interceptor's bearer token. */
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
  auth_mode: "token" | "passphrase" | "none";
  read_only: boolean;
  behind_tunnel: boolean;
  /** CityHall client mode: a locked-down composer-only client, enforced server-side too. */
  cityhall_mode: boolean;
  profile: string;
  acp_show_tool_durations: boolean;
  /** Per-session event log retention cap; 0 means unlimited. */
  acp_replay_events: number;
  acp_compaction_reminder: boolean;
  acp_compaction_reminder_percent: number;
  build_flavor: "debug" | "release";
  /** Entry bundle name, compared against this page's own to detect a stale client. */
  web_build_id?: string | null;
  sleep_inhibit: {
    prevent_sleep_enabled: boolean;
    currently_held: boolean;
    /** Optimistic: true means no failure has latched yet. */
    backend_available: boolean;
  };
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

export function setTelemetryConsent(enabled: boolean): Promise<TelemetryStatus | null> {
  return fetchJson<TelemetryStatus>("/api/telemetry/consent", jsonInit("POST", { enabled }));
}

/** Mirrors `USAGE_SIGNALS` in `src/telemetry/usage_signals.rs`. */
export type TelemetrySignal = "web" | "structured_view" | "diff_panel" | "diff_comments" | "web_terminal";

/** Best-effort ping to the local daemon, which only forwards counts when opted in. */
export function reportTelemetrySeen(surface: TelemetrySignal): void {
  void fetch("/api/telemetry/seen", jsonInit("POST", { surface, form_factor: clientFormFactor() })).catch(() => {});
}

export function reportAcpInteraction(kind: "prompt_queued"): void {
  void fetch("/api/telemetry/structured-interaction", jsonInit("POST", { kind })).catch(() => {});
}

export function isDebugBuild(about: ServerAbout | null | undefined): boolean {
  return about?.build_flavor === "debug";
}

export type UpdateCheckMode = "auto" | "notify" | "off";

export interface UpdateStatus {
  update_check_mode: UpdateCheckMode;
  current_version: string;
  latest_version: string | null;
  update_available: boolean;
  release_url: string | null;
  error: string | null;
  dismissed_version: string | null;
}

export function fetchUpdateStatus(): Promise<UpdateStatus | null> {
  return fetchJson<UpdateStatus>("/api/system/update-status");
}

export function dismissUpdate(version: string): Promise<boolean> {
  return fetchOk("/api/app-state/dismiss-update", jsonInit("POST", { version }));
}

// --- Branches ---

export interface BranchInfo {
  name: string;
  is_current: boolean;
  remote_only?: boolean;
}

export function fetchBranches(path: string, includeRemote = false): Promise<BranchInfo[] | null> {
  const params = new URLSearchParams({ path });
  if (includeRemote) params.set("include_remote", "true");
  return fetchJson<BranchInfo[]>(`/api/git/branches?${params.toString()}`);
}

/** Null on a transient failure so callers stay optimistic. */
export async function fetchIsGitRepo(path: string): Promise<boolean | null> {
  const params = new URLSearchParams({ path });
  const res = await fetchJson<{ is_git_repo: boolean }>(`/api/git/is-repo?${params.toString()}`);
  return res ? res.is_git_repo : null;
}

// --- ACP ---

export interface ContextPrimerResponse {
  primer: string;
  included_event_count: number;
  included_turn_count: number;
  truncated: boolean;
  max_chars: number;
  /** The last prompt that never reached the agent, popped from the primer for the composer. */
  unprocessed_prompt?: string | null;
}

export interface AcpAgentInfo {
  name: string;
  description: string;
  command: string;
  /** Omitted while Active. Mirrors `AgentLifecycle` in src/agents.rs. */
  lifecycle?: AgentLifecycleInfo;
}

/** The ACP registry, distinct from the session-tool agents at `/api/agents`. */
export async function fetchAcpAgents(): Promise<AcpAgentInfo[]> {
  return (await fetchJson<AcpAgentInfo[]>("/api/acp/agents")) ?? [];
}

export interface AgentOptionEntry {
  /** RFC 3339 timestamp of the last observation. */
  updated_at: string;
  options: ConfigOptionDescriptor[];
}

/** Config options each agent last advertised, keyed by agent name. */
export interface AcpOptionCatalog {
  version: number;
  agents: Record<string, AgentOptionEntry>;
}

export async function fetchAcpOptionCatalog(): Promise<AcpOptionCatalog> {
  return (await fetchJson<AcpOptionCatalog>("/api/acp/option-catalog")) ?? { version: 1, agents: {} };
}

export interface SwitchAgentResponse {
  session_id: string;
  agent: string;
  /** Highest seq before AgentSwitched; pass to fetchContextPrimer to exclude the handoff. */
  before_seq: number;
  switch_seq: number;
  status: string;
}

/** Hand a session off to ACP backend `target`. `reason` is shown in the transcript divider. */
export function switchAcpAgent(
  sessionId: string,
  target: string,
  model?: string | null,
  reason?: string | null,
): Promise<SwitchAgentResponse | null> {
  const body: { target: string; model?: string; reason?: string } = { target };
  if (model) body.model = model;
  if (reason) body.reason = reason;
  return fetchJson<SwitchAgentResponse>(
    `/api/sessions/${encodeURIComponent(sessionId)}/acp/switch-agent`,
    jsonInit("POST", body),
  );
}

export interface ViewSwitchResponse {
  session_id: string;
  view?: "structured" | "terminal";
}

export function acpEnable(sessionId: string): Promise<ViewSwitchResponse | null> {
  return fetchJson<ViewSwitchResponse>(`/api/sessions/${encodeURIComponent(sessionId)}/acp/enable`, { method: "POST" });
}

export async function acpDisable(
  sessionId: string,
): Promise<{ ok: true; data: ViewSwitchResponse } | { ok: false; message?: string }> {
  try {
    const response = await fetch("/api/sessions/" + encodeURIComponent(sessionId) + "/acp/disable", { method: "POST" });
    if (response.ok) return { ok: true, data: (await response.json()) as ViewSwitchResponse };
    const message = (await response.text()).trim();
    return message ? { ok: false, message } : { ok: false };
  } catch {
    return { ok: false };
  }
}

// --- Server-owned prompt queue ---

/** Metadata only; the bytes stay server-side until drain. */
export interface ServerQueuedAttachmentRef {
  id: string;
  kind: "image" | "audio" | "resource";
  mime_type: string;
  name?: string | null;
  size: number;
}

export interface ServerQueuedPrompt {
  id: string;
  seq: number;
  text: string;
  attachments?: ServerQueuedAttachmentRef[];
  created_at: string;
  origin_device?: string | null;
}

/** Base64 `dataB64` without a `data:` prefix. */
export interface QueueAttachmentUpload {
  kind: "image" | "audio" | "resource";
  mimeType: string;
  name?: string;
  dataB64: string;
}

const queuePath = (sessionId: string, promptId?: string) =>
  `/api/sessions/${encodeURIComponent(sessionId)}/queue${promptId === undefined ? "" : `/${encodeURIComponent(promptId)}`}`;

/** `id` is client-minted, so re-posting it updates the entry in place. */
export function enqueueServerPrompt(
  sessionId: string,
  prompt: {
    id: string;
    text: string;
    createdAt?: string;
    originDevice?: string;
    attachments?: QueueAttachmentUpload[];
  },
): Promise<ServerQueuedPrompt | null> {
  return fetchJson<ServerQueuedPrompt>(
    queuePath(sessionId),
    jsonInit("POST", {
      id: prompt.id,
      text: prompt.text,
      created_at: prompt.createdAt,
      origin_device: prompt.originDevice,
      attachments: (prompt.attachments ?? []).map((a) => ({
        kind: a.kind,
        mime_type: a.mimeType,
        data: a.dataB64,
        name: a.name,
      })),
    }),
  );
}

/** Always an array, ordered by `seq`. */
export async function listServerQueue(sessionId: string): Promise<ServerQueuedPrompt[]> {
  const rows = await fetchJson<ServerQueuedPrompt[]>(queuePath(sessionId));
  return Array.isArray(rows) ? rows : [];
}

export function editServerQueuedPrompt(sessionId: string, promptId: string, text: string): Promise<boolean> {
  return fetchOk(queuePath(sessionId, promptId), jsonInit("PATCH", { text }));
}

export function removeServerQueuedPrompt(sessionId: string, promptId: string): Promise<boolean> {
  return fetchOk(queuePath(sessionId, promptId), { method: "DELETE" });
}

export function clearServerQueue(sessionId: string): Promise<boolean> {
  return fetchOk(queuePath(sessionId), { method: "DELETE" });
}

export interface InstallAgentResponse {
  session_id: string;
  package: string;
  success: boolean;
  exit_code: number | null;
  stdout: string;
  stderr: string;
  /** Other sessions blocked on the same adapter that were queued for respawn. */
  recovered_sessions: number;
}

/** Run `npm install -g` for the session's agent on the host; throws with the server's message. */
export async function installAcpAgent(sessionId: string): Promise<InstallAgentResponse> {
  const { ok, status, payload } = await send(`/api/sessions/${encodeURIComponent(sessionId)}/acp/install-agent`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
  });
  if (!ok) throw new Error(rawMessage(payload) || (payload?.error as string) || `Server returned ${status}`);
  if (!payload) throw new Error("Server returned an invalid or empty response");
  return payload as unknown as InstallAgentResponse;
}

/** Markdown recap of events `seq < beforeSeq`, offered after a `session/load` failure. */
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

/** A persisted login session; `current` flags the requesting one. */
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

/** Elevation-gated; resolves false when the interceptor pops the passphrase prompt. */
export function revokeDevice(sessionId: string): Promise<boolean> {
  return fetchOk(`/api/login/sessions/${encodeURIComponent(sessionId)}`, { method: "DELETE" });
}

/** Sign every device out, including this one. Elevation-gated. */
export function signOutAllDevices(): Promise<boolean> {
  return fetchOk("/api/login/logout-all", { method: "POST" });
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
  showHidden = false,
): Promise<BrowseResponse & { ok: boolean }> {
  const params = new URLSearchParams({ path });
  if (limit != null) params.set("limit", String(limit));
  if (filter) params.set("filter", filter);
  if (showHidden) params.set("show_hidden", "true");
  const data = await fetchJson<BrowseResponse>(`/api/filesystem/browse?${params}`);
  if (!data) return { entries: [], has_more: false, ok: false };
  return { ...data, ok: true };
}

export async function fetchGroups(): Promise<GroupInfo[]> {
  return (await fetchJson<GroupInfo[]>("/api/groups")) ?? [];
}

/** Resolve a plugin `dynamic_select` widget's options; `depends` holds sibling values in order. */
export async function resolvePluginOptions(
  pluginId: string,
  source: string,
  depends: string[],
): Promise<{ value: string; label: string }[]> {
  const body = await fetchJson<{ options?: { value: string; label: string }[] }>(
    `/api/plugins/${encodeURIComponent(pluginId)}/settings/options/resolve`,
    jsonInit("POST", { source, depends }),
  );
  return body?.options ?? [];
}

export async function fetchProjects(scope?: "global" | "profile"): Promise<ProjectInfo[]> {
  const url = scope ? `/api/projects?scope=${scope}` : "/api/projects";
  return (await fetchJson<ProjectInfo[]>(url)) ?? [];
}

/** Claude Code sessions on disk, newest first, for the import picker. */
export async function listClaudeSessions(): Promise<ClaudeSessionSummary[]> {
  return (await fetchJson<ClaudeSessionSummary[]>("/api/claude-sessions")) ?? [];
}

type ProjectResult = { ok: boolean; error?: string; project?: ProjectInfo };

/** Error bodies may be JSON `{message}` or plain text. */
async function projectRequest(url: string, init: RequestInit, returnsProject = true): Promise<ProjectResult> {
  try {
    const res = await fetch(url, init);
    if (!res.ok) {
      const text = await res.text();
      try {
        return { ok: false, error: JSON.parse(text).message || `Server error (${res.status})` };
      } catch {
        return { ok: false, error: text || `Server error (${res.status})` };
      }
    }
    return returnsProject ? { ok: true, project: (await res.json()) as ProjectInfo } : { ok: true };
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

const projectPath = (name: string, scope: "global" | "profile") =>
  `/api/projects/${encodeURIComponent(name)}?scope=${scope}`;

export function createProject(body: {
  path: string;
  name?: string;
  scope?: "global" | "profile";
  allow_override?: boolean;
  default_base_branch?: string;
  /** Show it as a sessionless sidebar header. */
  pinned?: boolean;
  overrides?: ProjectOverrides;
}): Promise<ProjectResult> {
  return projectRequest("/api/projects", jsonInit("POST", body));
}

export function deleteProject(name: string, scope: "global" | "profile"): Promise<{ ok: boolean; error?: string }> {
  return projectRequest(projectPath(name, scope), { method: "DELETE" }, false);
}

/** Pass `null` to clear the default base branch. An override sub-field of `null` clears it; an absent
 *  one is left untouched. */
export function updateProject(
  name: string,
  scope: "global" | "profile",
  defaultBaseBranch: string | null,
  overrides?: { [K in keyof ProjectOverrides]?: ProjectOverrides[K] | null },
): Promise<ProjectResult> {
  return projectRequest(
    projectPath(name, scope),
    jsonInit("PATCH", { default_base_branch: defaultBaseBranch, ...(overrides ? { overrides } : {}) }),
  );
}

/** Unpinning keeps the registry entry; it only drops from the sidebar. */
export function setProjectPinned(name: string, scope: "global" | "profile", pinned: boolean): Promise<ProjectResult> {
  return projectRequest(projectPath(name, scope), jsonInit("PATCH", { pinned }));
}

export async function fetchDockerStatus(): Promise<DockerStatusResponse> {
  return (await fetchJson<DockerStatusResponse>("/api/docker/status")) ?? { available: false, runtime: null };
}

/** Repo hooks awaiting approval, from a `hooks_need_trust` 403; resubmit with `trust_hooks: true`. */
export interface HooksNeedTrust {
  onCreate: string[];
  /** Also trusted: run on every later session start, including TUI/CLI ones. */
  onLaunch: string[];
  onDestroy: string[];
  /** Whether the repo's `.mcp.json` also needs approval. */
  needsMcpTrust: boolean;
}

const stringList = (value: unknown) => (Array.isArray(value) ? value : []);

export async function createSession(body: CreateSessionRequest): Promise<{
  ok: boolean;
  error?: string;
  session?: SessionResponse;
  hooksNeedTrust?: HooksNeedTrust;
}> {
  try {
    const res = await fetch("/api/sessions", jsonInit("POST", body));
    if (res.ok) return { ok: true, session: await res.json() };
    const text = await res.text();
    try {
      const data = JSON.parse(text);
      if (data.error !== "hooks_need_trust")
        return { ok: false, error: data.message || `Server error (${res.status})` };
      return {
        ok: false,
        error: data.message || "Repository hooks require trust",
        hooksNeedTrust: {
          onCreate: stringList(data.on_create),
          onLaunch: stringList(data.on_launch),
          onDestroy: stringList(data.on_destroy),
          needsMcpTrust: data.needs_mcp_trust === true,
        },
      };
    } catch {
      return { ok: false, error: `Server error (${res.status}): ${text.slice(0, 200)}` };
    }
  } catch (e) {
    return { ok: false, error: networkError(e) };
  }
}

// --- Clone ---

export async function cloneRepo(
  url: string,
  opts?: { destination?: string; shallow?: boolean; bare?: boolean },
): Promise<{ ok: boolean; path?: string; error?: string }> {
  const body: Record<string, unknown> = { url };
  if (opts?.destination) body.destination = opts.destination;
  if (opts?.shallow) body.shallow = true;
  if (opts?.bare) body.bare = true;
  try {
    const { ok, status, payload } = await send("/api/git/clone", jsonInit("POST", body));
    if (!ok) return { ok: false, error: rawMessage(payload) || `Clone failed (${status})` };
    return { ok: true, path: payload?.path as string | undefined };
  } catch (e) {
    return { ok: false, error: networkError(e) };
  }
}

// --- Login ---

export interface LoginStatus {
  required: boolean;
  authenticated: boolean;
  /** Inside the step-up window that sensitive routes require. */
  elevated: boolean;
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

/** True when the token authenticates; a passphrase may still be required (see `loginStatus`). */
export function verifyToken(): Promise<boolean> {
  return fetchOk("/api/login/status");
}

/** Run `request` with this browser's device-binding secret. */
async function withBindingSecret<T extends { ok: boolean; error?: string }>(
  unavailable: string,
  request: (secret: string) => Promise<T>,
): Promise<T | { ok: false; error: string }> {
  let secret: string;
  try {
    secret = getOrCreateDeviceBindingSecret();
  } catch (err) {
    return { ok: false, error: err instanceof Error ? err.message : unavailable };
  }
  try {
    return await request(secret);
  } catch {
    return { ok: false, error: "Network error" };
  }
}

export function login(passphrase: string): Promise<{ ok: boolean; error?: string }> {
  return withBindingSecret("Could not create device binding for this browser", async (secret) => {
    const { ok, status, payload } = await send(
      "/api/login",
      jsonInit("POST", { passphrase, device_binding_secret: secret }),
    );
    return ok ? { ok: true } : { ok: false, error: rawMessage(payload) ?? `Login failed (${status})` };
  });
}

/** Re-verify the passphrase to open a fresh elevation window. Sends the binding header
 *  explicitly so the second factor does not depend on the fetch interceptor. */
export function elevateLogin(
  passphrase: string,
): Promise<{ ok: boolean; error?: string; elevated_until_secs?: number }> {
  return withBindingSecret("Could not access device binding for this browser", async (secret) => {
    const { ok, status, payload } = await send(
      "/api/login/elevate",
      jsonInit("POST", { passphrase }, { "X-Aoe-Device-Binding": secret }),
    );
    if (ok) return { ok: true, elevated_until_secs: payload?.elevated_until_secs as number | undefined };
    return { ok: false, error: rawMessage(payload) ?? `Elevation failed (${status})` };
  });
}

export async function logout(): Promise<void> {
  try {
    await fetch("/api/logout", { method: "POST" });
  } catch {
    // Best effort
  } finally {
    // A fresh login gets a fresh binding secret, so a stale localStorage snapshot is useless.
    try {
      clearDeviceBindingSecret();
    } catch {
      // ignore
    }
    // Don't leak the previous user's cached approval sound to the next user of this tab.
    try {
      const { clearApprovalSoundCache } = await import("../hooks/useApprovalSound");
      clearApprovalSoundCache();
    } catch {
      // ignore
    }
  }
}

/** Rename a session. A tied worktree moves too, and a running session returns 409 with a message. */
export async function renameSession(
  id: string,
  title: string,
): Promise<{ ok: boolean; message?: string; warnings?: string[] }> {
  const reply = await send(`/api/sessions/${id}`, jsonInit("PATCH", { title })).catch(() => null);
  if (!reply) return { ok: false };
  if (!reply.ok) return { ok: false, message: stringField(reply.payload, "message") };
  const warnings = stringList(reply.payload?.warnings).filter((w): w is string => typeof w === "string");
  return warnings.length > 0 ? { ok: true, warnings } : { ok: true };
}

/** Re-trigger smart rename; a 202 means it started, not that the title changed. */
export function smartRenameSession(id: string): Promise<{ ok: boolean; message?: string }> {
  return okWithMessage(`/api/sessions/${encodeURIComponent(id)}/smart-rename`, { method: "POST" });
}

/** Start an on-demand conversation summary; the result arrives as a ConversationSummary event. */
export function summarizeSession(id: string): Promise<{ ok: boolean; message?: string }> {
  return okWithMessage(`/api/sessions/${encodeURIComponent(id)}/summarize`, { method: "POST" });
}

/** Move a stopped managed worktree and optionally rename its branch. */
export function setWorktreeName(
  id: string,
  name: string,
  renameBranch: boolean,
): Promise<{ ok: boolean; message?: string }> {
  return okWithMessage(`/api/sessions/${id}/worktree-name`, jsonInit("PATCH", { name, rename_branch: renameBranch }));
}

export type AttachProjectWorker = "restarted" | "not_running" | "restart_failed";

export interface AttachProjectResult {
  ok: boolean;
  /** Validation message on failure, or the worker message on a failed restart. */
  message?: string;
  worker?: AttachProjectWorker;
  /** Directory leaf the repo was attached under. */
  name?: string;
  branch?: string;
  /** False when an existing branch was checked out. */
  branchCreated?: boolean;
  /** New working directory when the attach converted the session into a workspace. */
  movedTo?: string;
  warnings?: string[];
}

/** Attach another repo (path or registered project name) to an existing session, converting it into a
 *  workspace. `worker: "restart_failed"` means the repo is attached but the session did not come back. */
export async function attachSessionProject(
  id: string,
  project: string,
  opts: { attachExistingBranch?: boolean } = {},
): Promise<AttachProjectResult> {
  const reply = await send(
    `/api/sessions/${id}/projects`,
    jsonInit("POST", { project, attach_existing_branch: opts.attachExistingBranch ?? false }),
  ).catch(() => null);
  if (!reply) return { ok: false };
  const body = reply.payload;
  if (!reply.ok) return { ok: false, message: stringField(body, "message") };
  const attached = body?.attached as Payload | undefined;
  return {
    ok: true,
    worker: body?.worker as AttachProjectWorker | undefined,
    message: stringField(body, "worker_message"),
    name: stringField(attached, "name"),
    branch: stringField(attached, "branch"),
    branchCreated: typeof attached?.branch_created === "boolean" ? attached.branch_created : undefined,
    movedTo: stringField(attached, "moved_to"),
    warnings: Array.isArray(body?.warnings) ? (body.warnings as string[]) : undefined,
  };
}

/** Move a session to a group (created if new); an empty string ungroups. */
export function updateSessionGroup(id: string, group: string): Promise<boolean> {
  return fetchOk(`/api/sessions/${encodeURIComponent(id)}/group`, jsonInit("PATCH", { group }));
}

/** "off"/"all" force all three notification overrides; "default" clears them. */
export function setSessionNotifications(id: string, preset: "off" | "default" | "all"): Promise<boolean> {
  const value = preset === "off" ? false : preset === "all" ? true : null;
  return fetchOk(
    `/api/sessions/${id}/notifications`,
    jsonInit("PATCH", { notify_on_waiting: value, notify_on_idle: value, notify_on_error: value }),
  );
}

const sessionUpdate = (id: string, action: string, init: RequestInit) =>
  fetchJson<SessionResponse>(`/api/sessions/${id}/${action}`, init);

/** Set or clear (`null`) one repo's diff base; omit `repo` for a single-repo session. */
export function setSessionDiffBase(
  id: string,
  baseBranch: string | null,
  repo?: string | null,
): Promise<SessionResponse | null> {
  return sessionUpdate(
    id,
    "diff-base",
    jsonInit("PATCH", repo ? { base_branch: baseBranch, repo } : { base_branch: baseBranch }),
  );
}

/** Web-only pin: pinned workspaces sink to the top of the sidebar in every sort mode. */
export function setSessionPin(id: string, pinned: boolean): Promise<SessionResponse | null> {
  return sessionUpdate(id, "pin", jsonInit("PATCH", { pinned }));
}

/** Set or clear (`null`) the color label: `red` / `amber` / `green`. */
export function setSessionColor(id: string, color: string | null): Promise<SessionResponse | null> {
  return sessionUpdate(id, "color", jsonInit("PATCH", { color }));
}

export function setSessionArchive(id: string, archived: boolean, killPane = true): Promise<SessionResponse | null> {
  return sessionUpdate(id, "archive", jsonInit("PATCH", { archived, kill_pane: killPane }));
}

/** Stop the live session but keep every durable artifact so it can be restored. */
export function trashSession(id: string, killPane = true): Promise<SessionResponse | null> {
  return sessionUpdate(id, "trash", jsonInit("POST", { kill_pane: killPane }));
}

export function restoreSession(id: string): Promise<SessionResponse | null> {
  return sessionUpdate(id, "restore", jsonInit("POST"));
}

/** Stop the pane/worker but keep the session record (status `Stopped`). */
export function stopSession(id: string): Promise<SessionResponse | null> {
  return sessionUpdate(id, "stop", jsonInit("POST"));
}

export function startSession(id: string): Promise<SessionResponse | null> {
  return sessionUpdate(id, "start", jsonInit("POST"));
}

/** `null` unsnoozes; otherwise 1..=43200 minutes, validated server-side. */
export function setSessionSnooze(id: string, minutes: number | null): Promise<SessionResponse | null> {
  return sessionUpdate(id, "snooze", jsonInit("PATCH", { minutes }));
}

/** `false` clears both the auto and manual unread markers. */
export function setSessionUnread(id: string, unread: boolean): Promise<SessionResponse | null> {
  return sessionUpdate(id, "unread", jsonInit("PATCH", { unread }));
}

export interface DeleteSessionOptions {
  delete_worktree?: boolean;
  delete_branch?: boolean;
  delete_sandbox?: boolean;
  force_delete?: boolean;
  /** Keep a scratch session's directory on disk. */
  keep_scratch?: boolean;
}

export interface WorkspaceDeleteFailure {
  id: string;
  error: string;
}

export interface DeleteWorkspaceResult {
  ok: boolean;
  error?: string;
  messages?: string[];
  deleted?: string[];
  failed?: WorkspaceDeleteFailure[];
}

/** Atomically delete a workspace. `sessionIds[0]` owns the worktree and is removed last. */
export async function deleteWorkspace(
  sessionIds: string[],
  options: DeleteSessionOptions = {},
): Promise<DeleteWorkspaceResult> {
  try {
    const { ok, status, payload } = await send(
      "/api/workspaces",
      jsonInit("DELETE", { session_ids: sessionIds, ...options }),
    );
    const failed = payload?.failed as WorkspaceDeleteFailure[] | undefined;
    if (!ok) return { ok: false, error: rawMessage(payload) || `Server error (${status})`, failed };
    // Without a `deleted` array, deletion is unconfirmed; keep local state for those sessions.
    if (!Array.isArray(payload?.deleted)) {
      return { ok: false, error: "Server did not confirm which sessions were deleted" };
    }
    return { ok: true, messages: payload.messages as string[] | undefined, deleted: payload.deleted, failed };
  } catch (e) {
    return { ok: false, error: networkError(e) };
  }
}

// --- MCP servers ---

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

const mcpPath = (name: string, action: string) => `/api/mcp/servers/${encodeURIComponent(name)}/${action}`;

export type McpResolveResult = "applied" | "stale" | "error";

export async function resolveMcpConflict(
  name: string,
  agent: string,
  winner: "aoe" | "native",
  fingerprint: string,
): Promise<McpResolveResult> {
  try {
    const res = await fetch(mcpPath(name, "resolve"), jsonInit("POST", { agent, winner, fingerprint }));
    if (res.ok) return "applied";
    return res.status === 409 ? "stale" : "error";
  } catch {
    return "error";
  }
}

export function keepMcpServer(name: string, agent: string): Promise<boolean> {
  return fetchOk(mcpPath(name, "keep"), jsonInit("POST", { agent }));
}

export function dropMcpServer(name: string, agent: string): Promise<boolean> {
  return fetchOk(mcpPath(name, "drop"), jsonInit("POST", { agent }));
}

// --- Skills ---

export type SkillProvenance = { kind: "aoe-managed" } | { kind: "external"; root: string };

export interface SkillSummary {
  directory: string;
  name: string;
  description: string;
  provenance: SkillProvenance;
  provenanceLabel: string;
  writable: boolean;
}

export interface SkillDetail {
  directory: string;
  name: string;
  description: string;
  provenance: SkillProvenance;
  content: string;
}

export interface SkillRoot {
  id: string;
  label: string;
  relativePath: string;
  consumers: string[];
  legacy: boolean;
}

export interface SkillsResponse {
  skills: SkillSummary[];
  roots: SkillRoot[];
}

export interface SkillMutationResult {
  ok: boolean;
  directory?: string;
  error?: string;
  status?: number;
}

export function fetchSkills(): Promise<SkillsResponse | null> {
  return fetchJson<SkillsResponse>("/api/skills");
}

export function fetchSkill(source: string, directory: string): Promise<SkillDetail | null> {
  return fetchJson<SkillDetail>(`/api/skills/${encodeURIComponent(source)}/${encodeURIComponent(directory)}`);
}

async function skillRequest<T extends { ok: boolean; error?: string; status?: number }>(
  url: string,
  init: RequestInit,
  onOk: (payload: Payload | null) => Omit<T, "ok" | "status">,
  failureExtra: Partial<T> = {},
): Promise<T> {
  try {
    const { ok, status, payload } = await send(url, init);
    if (!ok)
      return { ...failureExtra, ok: false, error: rawMessage(payload) ?? `Server error (${status})`, status } as T;
    return { ok: true, ...onOk(payload), status } as T;
  } catch (e) {
    return { ok: false, ...failureExtra, error: networkError(e) } as T;
  }
}

function skillMutation(url: string, method: string, body?: unknown): Promise<SkillMutationResult> {
  return skillRequest<SkillMutationResult>(url, body === undefined ? { method } : jsonInit(method, body), (p) => ({
    directory: (p?.directory as string | null | undefined) ?? undefined,
  }));
}

export function createSkill(directory: string, description?: string): Promise<SkillMutationResult> {
  return skillMutation("/api/skills", "POST", { directory, description });
}

export function updateSkill(directory: string, content: string): Promise<SkillMutationResult> {
  return skillMutation(`/api/skills/${encodeURIComponent(directory)}`, "PUT", { content });
}

export function deleteSkill(directory: string): Promise<SkillMutationResult> {
  return skillMutation(`/api/skills/${encodeURIComponent(directory)}`, "DELETE");
}

export function adoptSkill(source: string, directory: string, destination?: string): Promise<SkillMutationResult> {
  return skillMutation(`/api/skills/${encodeURIComponent(source)}/${encodeURIComponent(directory)}/adopt`, "POST", {
    destination,
  });
}

export type SkillSyncStatus = "created" | "updated" | "unchanged" | "removed" | "conflict" | "error";

/** `message` is set for `conflict` (a user-owned or edited copy was left alone) and `error`. */
export interface SkillSyncOutcome {
  root: string;
  directory: string;
  status: SkillSyncStatus;
  message: string | null;
}

export interface SkillSyncResult {
  ok: boolean;
  outcomes: SkillSyncOutcome[];
  error?: string;
  status?: number;
}

/** Copy AoE-managed skills into agent skill directories, never overwriting anything AoE did not
 *  deploy unchanged. Empty `roots` syncs every root; `replace` names conflicts to overwrite;
 *  non-empty `directories` limits the reconcile (and orphan removal) to those skills. */
export function syncSkills(options?: {
  roots?: string[];
  replace?: string[];
  directories?: string[];
}): Promise<SkillSyncResult> {
  return skillRequest<SkillSyncResult>(
    "/api/skills/sync",
    jsonInit("POST", { roots: options?.roots, replace: options?.replace, directories: options?.directories }),
    (p) => ({ outcomes: (p?.outcomes as SkillSyncOutcome[] | undefined) ?? [] }),
    { outcomes: [] },
  );
}
