import type { AgentLifecycleInfo } from "./agentProfiles";
import { notifySettingsChanged } from "./settingsEvents";
import { clientFormFactor } from "./formFactor";
import type { SchemaSettings } from "./generated/settingsKeys";
import type {
  SessionResponse,
  RichDiffFilesResponse,
  RichFileContentsResponse,
  AgentInfo,
  ProfileInfo,
  ProfileSettingsResponse,
  HooksOverride,
  BrowseResponse,
  GroupInfo,
  ProjectInfo,
  ProjectOverrides,
  DockerStatusResponse,
  CreateProgress,
  CreateSessionRequest,
  CreationTrustFingerprint,
  ImportableSession,
  SettingsFieldDescriptor,
  SuggestionMapping,
  SuggestionPrefs,
  SuggestionPrepared,
  SuggestionTemplate,
  SuggestionsSnapshot,
} from "./types";
import type { ConfigOptionDescriptor } from "./acpTypes";
import { clearDeviceBindingSecret, getOrCreateDeviceBindingSecret } from "./deviceBinding";

type Payload = Record<string, unknown>;

interface Reply {
  ok: boolean;
  status: number;
  /** Parsed JSON body, or null when the body is not JSON. */
  payload: Payload | null;
}

type Failure = { kind: "error"; message: string };

async function send(url: string, init?: RequestInit): Promise<Reply> {
  const res = await fetch(url, init);
  const payload = (await res.json().catch(() => null)) as Payload | null;
  return { ok: res.ok, status: res.status, payload };
}

async function fetchOk(url: string, init?: RequestInit): Promise<boolean> {
  try {
    return (await fetch(url, init)).ok;
  } catch {
    return false;
  }
}

const jsonInit = (method: string, body?: unknown, headers: Record<string, string> = {}): RequestInit => ({
  method,
  headers: { "Content-Type": "application/json", ...headers },
  body: JSON.stringify(body),
});

const ACCEPT_JSON = { Accept: "application/json" };

const stringField = (payload: Payload | null | undefined, key: string): string | undefined =>
  typeof payload?.[key] === "string" ? (payload[key] as string) : undefined;

const rawMessage = (payload: Payload | null | undefined) => payload?.message as string | undefined;

const networkError = (e: unknown) => `Network error: ${e instanceof Error ? e.message : "connection failed"}`;

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

async function okWithMessage(url: string, init: RequestInit): Promise<{ ok: boolean; message?: string }> {
  try {
    const res = await fetch(url, init);
    if (res.ok) return { ok: true };
    const payload = (await res.json().catch(() => null)) as Payload | null;
    return { ok: false, message: stringField(payload, "message") };
  } catch {
    return { ok: false };
  }
}

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

async function skillRequest<T extends { ok: boolean; error?: string; status?: number }>(
  url: string,
  init: RequestInit,
  onOk: (payload: Payload) => Omit<T, "ok" | "status">,
  failureExtra: Partial<T> = {},
): Promise<T> {
  try {
    const res = await fetch(url, init);
    const payload = (await res.json().catch(() => ({}))) as Payload;
    if (!res.ok) {
      return {
        ...failureExtra,
        ok: false,
        error: (payload.message as string | undefined) ?? `Server error (${res.status})`,
        status: res.status,
      } as T;
    }
    return { ok: true, ...onOk(payload), status: res.status } as T;
  } catch (e) {
    return { ok: false, ...failureExtra, error: networkError(e) } as T;
  }
}

const queuePath = (sessionId: string, promptId?: string) =>
  `/api/sessions/${encodeURIComponent(sessionId)}/queue${promptId === undefined ? "" : `/${encodeURIComponent(promptId)}`}`;

const sessionUpdate = (id: string, action: string, init: RequestInit) =>
  fetchJson<SessionResponse>(`/api/sessions/${encodeURIComponent(id)}/${action}`, init);

/** Folds a degraded worktree move's warning from the mutation `outcome` into `warnings`. */
function withMoveWarning(
  session: SessionResponse | null,
  pick: (outcome: Payload) => string | undefined,
): SessionResponse | null {
  const outcome = (session as (SessionResponse & { outcome?: Payload }) | null)?.outcome;
  const warning = outcome && pick(outcome);
  return session && warning ? { ...session, warnings: [...(session.warnings ?? []), warning] } : session;
}

const mcpPath = (name: string, action: string) => `/api/mcp/servers/${encodeURIComponent(name)}/${action}`;

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

export interface RuntimeCursor {
  epoch: string;
  revision: bigint;
}

export interface SessionReceipt {
  id: string;
  cursor: RuntimeCursor;
}

export interface SessionMutation {
  session: SessionResponse;
  cursor: RuntimeCursor;
}

export type SessionSettingOutcome =
  | { outcome: "committed"; mutation: SessionMutation }
  | { outcome: Exclude<SaveOutcome, "committed"> };

export interface SessionsEnvelope {
  sessions: SessionResponse[];
  workspace_ordering: string[];
  cursor: RuntimeCursor;
}

function runtimeCursor(res: Response): RuntimeCursor | null {
  const epoch = res.headers.get("aoe-runtime-epoch");
  const revision = res.headers.get("aoe-runtime-revision");
  if (!epoch || revision === null || !/^\d+$/.test(revision)) return null;
  return { epoch, revision: BigInt(revision) };
}

async function sessionMutation(res: Response): Promise<SessionMutation | null> {
  if (!res.ok) return null;
  const cursor = runtimeCursor(res);
  if (!cursor) return null;
  return { session: (await res.json()) as SessionResponse, cursor };
}

async function sessionSettingOutcome(res: Response, id: string, expectedEpoch: string): Promise<SessionSettingOutcome> {
  if (!res.ok) return { outcome: res.status >= 500 ? "uncertain" : "rejected" };
  const mutation = await sessionMutation(res);
  if (
    !mutation ||
    mutation.session?.id !== id ||
    mutation.cursor.epoch !== expectedEpoch ||
    mutation.cursor.revision === 0n
  ) {
    return { outcome: "uncertain" };
  }
  return { outcome: "committed", mutation };
}

export async function fetchSessions(signal?: AbortSignal): Promise<SessionsEnvelope | null> {
  try {
    const res = await fetch("/api/sessions", { signal });
    if (!res.ok) return null;
    const cursor = runtimeCursor(res);
    if (!cursor) return null;
    return { ...((await res.json()) as Omit<SessionsEnvelope, "cursor">), cursor };
  } catch {
    return null;
  }
}

export interface ConversationSearchHit {
  session_id: string;
  seq: number;
  kind: string;
  snippet: string;
  match_count: number;
}

// Full-text search over session conversation content. Returns one hit per
// matching session, newest first. `signal` lets the caller abort a stale
// in-flight search when the query changes.
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

// Persisted recent projects (newest first), so a project stays in the wizard
// Recent tab after its last session is deleted. Merged with the live
// session-derived list in ProjectStep.
export function fetchRecentProjects(): Promise<RecentProjectsEnvelope | null> {
  return fetchJson<RecentProjectsEnvelope>("/api/recent-projects");
}

export async function updateWorkspaceOrdering(order: string[]): Promise<boolean> {
  try {
    return await fetchOk("/api/workspace-ordering", jsonInit("PUT", { order }));
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
      message: typeof body.message === "string" ? body.message : undefined,
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

export async function ensureTerminal(id: string, index = 0, container = false): Promise<EnsureSessionResult> {
  const path = container ? "container-terminal" : "terminal";
  try {
    const res = await fetch(`/api/sessions/${id}/${path}?index=${index}`, {
      method: "POST",
    });
    if (res.ok) return { ok: true };
    const body = await res.json().catch(() => ({}));
    return {
      ok: false,
      error: typeof body.error === "string" ? body.error : undefined,
      message: typeof body.message === "string" ? body.message : `Server error (${res.status})`,
    };
  } catch (e) {
    return { ok: false, message: e instanceof Error ? e.message : "Network error" };
  }
}

function fileToBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(reader.error ?? new Error("read failed"));
    reader.onload = () => {
      const result = reader.result as string;
      // Drop the `data:<mime>;base64,` prefix; the server wants raw base64.
      const comma = result.indexOf(",");
      resolve(comma >= 0 ? result.slice(comma + 1) : result);
    };
    reader.readAsDataURL(file);
  });
}

/**
 * Upload a clipboard image pasted into the live terminal. The host writes it
 * into the session worktree and returns the path the tmux pane can read, so
 * the CLI agent (e.g. Claude Code) attaches it. Returns null on any failure.
 * See #2678.
 */
export async function pasteImage(id: string, file: File, canUpload: () => boolean): Promise<string | null> {
  try {
    if (!canUpload()) return null;
    const data = await fileToBase64(file);
    if (!canUpload()) return null;
    const body = await fetchJson<Payload>(
      `/api/sessions/${id}/paste-image`,
      jsonInit("POST", { mime_type: file.type, data }),
    );
    return stringField(body, "path") ?? null;
  } catch {
    return null;
  }
}

/** Kill an extra terminal tab's host + container shells (index >= 1). Index 0
 *  is the primary terminal shared with the native TUI and cannot be killed
 *  from the web UI (the server rejects it); closing that tab only hides it. */
export async function killTerminal(id: string, index: number): Promise<boolean> {
  try {
    return await fetchOk(`/api/sessions/${id}/terminal?index=${index}`, { method: "DELETE" });
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

/**
 * Read a session file for the file viewer (#3088). Path may be project-relative
 * or an absolute path the agent touched this session; the server enforces
 * provenance confinement. See `GET /api/sessions/{id}/file`.
 */
export function getSessionFile(id: string, filePath: string): Promise<SessionFileResponse | null> {
  const params = new URLSearchParams({ path: filePath });
  return fetchJson<SessionFileResponse>(`/api/sessions/${id}/file?${params.toString()}`);
}

/** URL of a session file's raw bytes, confined like {@link getSessionFile}, for opening in a new tab. */
export function sessionRawFileUrl(id: string, filePath: string): string {
  const params = new URLSearchParams({ path: filePath });
  return `/api/sessions/${id}/file/raw?${params.toString()}`;
}

// --- Settings ---

/** Sections and fields come from the server schema (`SchemaSettings`, generated), so a name the
 *  server lacks fails to compile. `app_state`, `hooks` and plugin sections are outside the schema. */
export type SettingsResponse = SchemaSettings & {
  app_state?: {
    has_seen_web_tour?: boolean;
  };
  hooks?: HooksOverride;
} & { [K in `plugin:${string}`]?: Record<string, unknown> };

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

/** Settings as they apply: the served profile's overrides over the
 *  machine-wide values, or `profile`'s when named. */
export function fetchSettings(profile?: string): Promise<SettingsResponse | null> {
  const params = profile ? `?profile=${encodeURIComponent(profile)}` : "";
  return fetchJson<SettingsResponse>(`/api/settings${params}`);
}

/** The machine-wide layer alone, for editors that show or set the value a
 *  profile inherits. Anything honoring a setting reads `fetchSettings`. */
export function fetchMachineSettings(): Promise<SettingsResponse | null> {
  return fetchJson<SettingsResponse>("/api/settings?layer=machine");
}

async function announceSave(save: Promise<boolean>): Promise<boolean> {
  const ok = await save;
  if (ok) notifySettingsChanged();
  return ok;
}

/** `uncertain`: the write may have landed (network error, 5xx, or a failure after the server persisted). */
export type SaveOutcome = "committed" | "rejected" | "uncertain";

const POST_PERSIST_ERRORS = new Set(["partially_saved", "load_failed"]);

// Stamps each settings PATCH so the server can drop one that commits after a newer edit of the same field.
const writeTab = globalThis.crypto?.randomUUID?.() ?? Math.random().toString(36).slice(2);
let writeSeq = 0;

async function sendSave(url: string, init: RequestInit): Promise<SaveOutcome> {
  const headers = { ...(init.headers as Record<string, string>), "X-Aoe-Write-Stamp": `${writeTab}.${++writeSeq}` };
  try {
    const { ok, status, payload } = await send(url, { ...init, headers });
    if (ok) return "committed";
    if (status >= 500 || POST_PERSIST_ERRORS.has(stringField(payload, "error") ?? "")) return "uncertain";
    return "rejected";
  } catch {
    return "uncertain";
  }
}

async function announceOutcome(save: Promise<SaveOutcome>): Promise<SaveOutcome> {
  const outcome = await save;
  if (outcome !== "rejected") notifySettingsChanged();
  return outcome;
}

/** Fetch this install's CityHall config bundle as TOML text (settings +
 *  projects), for an admin to hand to CityHall. Throws with the server's
 *  message so the Settings page can show why an export failed. */
export async function fetchCityHallBundle(): Promise<string> {
  const res = await fetch("/api/cityhall/bundle");
  if (!res.ok) {
    // The failure body is JSON (`{error, message}`); fall back to the status
    // when it is not, e.g. the 403 CityHall client mode returns.
    const detail = await res.json().catch(() => null);
    throw new Error(detail?.message ?? `Export failed (HTTP ${res.status})`);
  }
  return res.text();
}

export interface LogsSnapshot {
  status: "available" | "missing" | "stdout" | "unavailable";
  /** Redacted server-side. */
  content: string;
  lineCount: number;
  truncated: boolean;
}

/** Redacted tail of the active log file; throws with the server's message. */
export async function fetchLogs(lines: number, signal?: AbortSignal): Promise<LogsSnapshot> {
  const res = await fetch(`/api/system/logs?lines=${lines}`, { signal });
  if (!res.ok) {
    const detail = await res.json().catch(() => null);
    throw new Error(detail?.message ?? `Could not load logs (HTTP ${res.status})`);
  }
  return (await res.json()) as LogsSnapshot;
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

// --- Plugins ---

export interface PluginView {
  id: string;
  name: string;
  version: string;
  description: string;
  /** Lucide kebab-case identity icon name, straight from the manifest. */
  icon: string | null;
  /** Resolved URL for the manifest's `icon_asset` (served from the plugin's
   *  install directory), null for a builtin or a plugin with no icon_asset. */
  icon_asset_url: string | null;
  enabled: boolean;
  builtin: boolean;
  /** Validation provenance: "builtin" | "featured" | "community" | "local". */
  validation: string;
  source: string | null;
  capabilities: string[];
  /** UI slots the plugin declares it will render into (#2366), disclosed so the
   *  user sees the plugin modifies the dashboard. Not a capability (needs no
   *  grant). `slot` is the kebab-case slot name. */
  ui_contributions: { slot: string; id: string }[];
  granted: boolean;
  needs_reapproval: boolean;
}

export interface PluginListResponse {
  plugins: PluginView[];
  load_errors: string[];
}

/** Outcome of a plugin enable/disable toggle. On success the server returns
 *  the refreshed list; on failure it returns the error JSON (403 read_only /
 *  elevation_required, or 400 plugin_error). A 403 `elevation_required` is
 *  handled globally by the fetch interceptor, which pops the passphrase
 *  prompt, the same as any other elevated request. */
export type PluginToggleResult = { kind: "ok"; data: PluginListResponse } | { kind: "error"; message: string };

export function fetchPlugins(): Promise<PluginListResponse | null> {
  return fetchJson<PluginListResponse>("/api/plugins");
}

/** A command's client-executed action (`api_version >= 6`). `open-ui-link` opens
 *  the `href` from the plugin's own per-session UI-state entry at `(slot, id)`. */
export type PluginClientAction = { kind: "open-ui-link"; slot: PluginUiSlot; id: string };

/** One active plugin command (`GET /api/plugins/commands`), normalized for the
 *  command palette and keymap. */
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

/** One plugin's update status (`GET /api/plugins/updates`). An on-demand
 *  network check, kept off the always-on plugin list. `available` is a short
 *  commit for an outdated GitHub source, "modified" for a changed local tree,
 *  or null when current. `error` is set when the check could not run. */
export interface PluginUpdateStatus {
  id: string;
  source: string;
  current: string;
  available: string | null;
  needs_update: boolean;
  error: string | null;
}

export type PluginUpdatesResult = { kind: "ok"; updates: PluginUpdateStatus[] } | { kind: "error"; message: string };

/** Check installed plugins for updates. Returns a discriminated result (like
 *  discoverPlugins) so this explicit action can report why it failed instead of
 *  silently doing nothing on an HTTP/network/JSON error. */
export async function fetchPluginUpdates(): Promise<PluginUpdatesResult> {
  try {
    return await kindRequest(
      "/api/plugins/updates",
      { headers: ACCEPT_JSON },
      (p) => (Array.isArray(p.updates) ? { kind: "ok" as const, updates: p.updates as PluginUpdateStatus[] } : null),
      "Update check failed",
    );
  } catch {
    return { kind: "error", message: "Network error." };
  }
}

/** One discovery result (`GET /api/plugins/discover`). Repo-level: the dashboard
 *  has no install path, so it shows `install_command` for the user to copy. */
export interface PluginDiscoveryResult {
  slug: string;
  html_url: string;
  description: string | null;
  stars: number;
  badge: "installed" | "featured" | "unvetted";
  /** The featured index marks the manifest as declaring `[suggestions]`. */
  suggestions: boolean;
  install_command: string;
  /** The repo owner's GitHub avatar; a source-identity affordance, NOT the
   *  plugin's own icon (unknown until a manifest fetch, which discovery
   *  results never do). */
  source_avatar_url: string;
}

export type DiscoverResult = { kind: "ok"; results: PluginDiscoveryResult[] } | { kind: "error"; message: string };

/** Search the `aoe-plugin` GitHub topic. Returns the error message (notably the
 *  unauthenticated rate limit) so the UI can show it rather than failing
 *  silently. */
export async function discoverPlugins(query: string): Promise<DiscoverResult> {
  const qs = query.trim() ? `?q=${encodeURIComponent(query.trim())}` : "";
  try {
    return await kindRequest(
      `/api/plugins/discover${qs}`,
      { headers: ACCEPT_JSON },
      (p) => (Array.isArray(p.results) ? { kind: "ok" as const, results: p.results as PluginDiscoveryResult[] } : null),
      "Discovery failed",
    );
  } catch {
    return { kind: "error", message: "Network error." };
  }
}

/** A plugin's manifest fields as shown in the detail modal (parsed leniently
 *  server-side, so a plugin targeting a newer api_version still renders). */
export interface PluginDetailManifest {
  id: string;
  name: string;
  version: string;
  description: string;
  api_version: number;
  capabilities: string[];
  ui_contributions: { slot: string; id: string }[];
  /** Screenshot/GIF previews, each resolved server-side to a raw.githubusercontent.com URL. */
  screenshots: { src: string; alt: string; caption: string }[];
  /** Lucide kebab-case identity icon name. */
  icon: string | null;
  /** The manifest's `icon_asset`, resolved server-side to a raw.githubusercontent.com URL. */
  icon_asset_url: string | null;
}

/** On-demand detail for one plugin source (`GET /api/plugins/details`): manifest
 *  fields plus the repo's published release tags (the available versions). */
export interface PluginDetail {
  source: string;
  manifest: PluginDetailManifest | null;
  manifest_error: string | null;
  release_tags: string[];
}

export type PluginDetailResult = { kind: "ok"; detail: PluginDetail } | { kind: "error"; message: string };

/** Fetch a plugin source's detail (manifest + release tags) for the modal.
 *  Only gh:owner/repo sources are supported server-side. */
export async function fetchPluginDetails(source: string): Promise<PluginDetailResult> {
  try {
    return await kindRequest(
      `/api/plugins/details?source=${encodeURIComponent(source)}`,
      { headers: ACCEPT_JSON },
      (p) => (typeof p.source === "string" ? { kind: "ok" as const, detail: p as unknown as PluginDetail } : null),
      "Details failed",
    );
  } catch {
    return { kind: "error", message: "Network error." };
  }
}

/** One UI slot disclosed in an update consent (mirrors the engine `UiView`). */
export interface PluginUpdateUiView {
  slot: string;
  id: string;
}

/** One changelog item between the installed and target version: a release's
 *  notes, or a single commit subject. Mirrors Rust `ChangelogEntry`. */
export type PluginChangelogEntry =
  | { kind: "release"; tag: string; body: string | null; published_at: string | null }
  | { kind: "commit"; sha: string; subject: string; url: string | null };

/** What changed between the installed version and the update target, mirroring
 *  Rust `UpdateChangelog`. `unavailable_reason` distinguishes "could not load"
 *  from "no entries" (both leave `entries` empty); `truncated` flags that more
 *  existed than are shown. */
export interface PluginUpdateChangelog {
  entries: PluginChangelogEntry[];
  truncated: boolean;
  unavailable_reason: string | null;
  /** GitHub URL for the full history (releases page or compare view), shown when
   *  the changelog is truncated. */
  more_url: string | null;
}

/** Structured disclosure for a capability-expanding plugin update, mirroring the
 *  Rust `UpdateConsent`. Drives the consent modal. */
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

/** Result of `GET /api/plugins/{id}/update/preview`: a tagged union mirroring the
 *  Rust `UpdatePreview`. */
export type PluginUpdatePreview =
  | { kind: "no_update" }
  | { kind: "safe_update"; to_version: string; fingerprint: string; changelog: PluginUpdateChangelog }
  | { kind: "consent_required"; consent: PluginUpdateConsent; dismissed: boolean };

export type PluginUpdatePreviewResult =
  | { kind: "ok"; preview: PluginUpdatePreview }
  | { kind: "error"; message: string };

/** Validate a preview payload against the discriminated union, so a drifted
 *  server response is rejected rather than passed on: a safe_update must carry a
 *  string fingerprint (else the apply would send no pin) and a consent_required
 *  must carry a consent object (else the modal path would blow up). */
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

/** Classify the available update for one installed plugin (no install happens).
 *  When consent is required the payload carries the full disclosure. */
export async function previewPluginUpdate(id: string): Promise<PluginUpdatePreviewResult> {
  try {
    return await kindRequest(
      `/api/plugins/${encodeURIComponent(id)}/update/preview`,
      { headers: ACCEPT_JSON },
      (p) => (isValidPreview(p) ? { kind: "ok" as const, preview: p } : null),
      "Update preview failed",
    );
  } catch {
    return { kind: "error", message: "Network error." };
  }
}

/** Start a host-side update job, pinned to the fingerprint the user saw. The
 *  update runs as a job (like install/uninstall) so its build is observable;
 *  poll the returned job id with `fetchPluginJob`. A fingerprint mismatch (the
 *  remote moved since the preview) surfaces as a failed job, which the UI
 *  recovers from by re-previewing. */
export async function applyPluginUpdate(id: string, expectedFingerprint: string | null): Promise<PluginJobStartResult> {
  return startPluginJob(`/api/plugins/${encodeURIComponent(id)}/update/apply`, {
    expected_fingerprint: expectedFingerprint,
  });
}

/** Structured install disclosure (`POST /api/plugins/install/preview`),
 *  mirroring the Rust `InstallConsent`. Drives the install consent modal. */
export interface PluginInstallConsent {
  id: string;
  version: string;
  source: string;
  /** One line stating what is being installed (resolved release, ref, etc). */
  notice: string;
  /** The source is off the audited-release default path; warn on it. */
  unverified: boolean;
  /** Trust class: "featured" | "community" | "local". */
  validation: string;
  capabilities: string[];
  ui: PluginUpdateUiView[];
  build_steps: string[];
  fingerprint: string;
}

export type PluginInstallPreviewResult =
  | { kind: "ok"; consent: PluginInstallConsent }
  | { kind: "error"; message: string };

/** Classify a gh: install candidate and return its disclosure, without
 *  installing. Backs the install consent modal. */
export async function previewPluginInstall(source: string): Promise<PluginInstallPreviewResult> {
  try {
    return await kindRequest(
      "/api/plugins/install/preview",
      jsonInit("POST", { source }, ACCEPT_JSON),
      (p) =>
        typeof p.fingerprint === "string"
          ? { kind: "ok" as const, consent: p as unknown as PluginInstallConsent }
          : null,
      "Install preview failed",
    );
  } catch {
    return { kind: "error", message: "Network error." };
  }
}

/** Outcome of starting a lifecycle job: a job id to poll, or an error (403
 *  read_only / elevation_required is handled by the fetch interceptor). */
export type PluginJobStartResult = { kind: "ok"; jobId: string } | { kind: "error"; message: string };

async function startPluginJob(url: string, body: unknown): Promise<PluginJobStartResult> {
  try {
    return await kindRequest(
      url,
      jsonInit("POST", body, ACCEPT_JSON),
      (p) => (typeof p.job_id === "string" ? { kind: "ok" as const, jobId: p.job_id } : null),
      "Request failed",
    );
  } catch {
    return { kind: "error", message: "Network error." };
  }
}

/** Start a host-side install job for an approved gh: source. Poll the returned
 *  job id with `fetchPluginJob`. */
export async function startPluginInstall(source: string, expectedFingerprint: string): Promise<PluginJobStartResult> {
  return startPluginJob("/api/plugins/install", { source, expected_fingerprint: expectedFingerprint });
}

/** Start a host-side uninstall job for an installed external plugin. */
export async function startPluginUninstall(id: string): Promise<PluginJobStartResult> {
  return startPluginJob(`/api/plugins/${encodeURIComponent(id)}/uninstall`, {});
}

/** A lifecycle job's status, mirroring the Rust `PluginJobStatus` tag. */
export type PluginJobState = { state: "running" } | { state: "succeeded" } | { state: "failed"; error: string };

/** A lifecycle job plus a bounded tail of its host-side log
 *  (`GET /api/plugins/jobs/{id}`). */
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

/** Fetch a lifecycle job's status plus a tail of its log. Polled by the
 *  progress modal until the job reaches a terminal state. The HTTP `status` is
 *  preserved so the caller can tell a terminal 404 (the job is gone, e.g. after
 *  a daemon restart) from a transient failure worth retrying. */
export async function fetchPluginJob(jobId: string, tail = 200): Promise<PluginJobResult> {
  try {
    const reply = await send(`/api/plugins/jobs/${encodeURIComponent(jobId)}?tail=${tail}`, {
      headers: ACCEPT_JSON,
    }).catch(() => null);
    if (!reply) return { kind: "error", status: 0, message: "Network error." };
    const { ok, status, payload } = reply;
    if (ok && typeof payload?.job === "object" && payload.job !== null) {
      return { kind: "ok", job: payload as unknown as PluginJob };
    }
    return { kind: "error", status, message: stringField(payload, "message") ?? `Job status failed (HTTP ${status}).` };
  } catch {
    return { kind: "error", status: 0, message: "Network error." };
  }
}

export type PluginDismissResult = { kind: "ok" } | { kind: "error"; message: string };

/** Record an in-app decline of an available update so it stops nagging until the
 *  next version. */
export async function dismissPluginUpdate(id: string, fingerprint: string): Promise<PluginDismissResult> {
  try {
    const res = await fetch(`/api/plugins/${encodeURIComponent(id)}/update/dismiss`, jsonInit("POST", { fingerprint }));
    if (res.ok) return { kind: "ok" };
    const payload = (await res.json().catch(() => null)) as Payload | null;
    return { kind: "error", message: stringField(payload, "message") ?? `Dismiss failed (HTTP ${res.status}).` };
  } catch {
    return { kind: "error", message: "Network error." };
  }
}

// Plugin UI extension points (#2366).

/** Display tone a plugin attaches to a slot entry or notification. The host
 *  validates it to this closed set; each surface maps it to a color. */
export type PluginUiTone = "neutral" | "info" | "success" | "warn" | "danger";

/** The host-rendered slots, kebab-case as the host serializes them. */
export type PluginUiSlot =
  | "status-bar"
  | "header-start"
  | "row-badge"
  | "row-column"
  | "sort-key"
  | "filter-facet"
  | "card"
  | "pane"
  | "composer-action"
  | "detail-badge"
  | "home-pane"
  | "page"
  | "notification";

/** One piece of UI state a worker pushed. `payload` shape is determined by
 *  `slot`; the dashboard renders it (no plugin code runs here). */
export interface PluginUiEntry {
  plugin_id: string;
  slot: PluginUiSlot;
  id: string;
  session_id?: string;
  payload: Record<string, unknown>;
}

/** A notification pushed via `ui.notify`. `seq` is monotonic so the client
 *  toasts each one exactly once. */
export interface PluginUiNotification {
  seq: number;
  plugin_id: string;
  tone: PluginUiTone;
  title: string;
  body?: string;
  session_id?: string;
  /** A URL a worker asked the surface to open (`ui.open_url`). When present the
   *  toast is rendered as click-to-open: a browser blocks `window.open` from an
   *  async push, so the open happens on the user's click. Always http/https. */
  href?: string;
}

/** A page an active plugin declares; `url` serves its HTML to a sandboxed frame. */
export interface PluginPage {
  plugin_id: string;
  id: string;
  title: string;
  icon: string | null;
  url: string;
  /** The plugin's granted capabilities; the page bridge gates on them. */
  capabilities: string[];
}

export interface PluginUiState {
  entries: PluginUiEntry[];
  notifications: PluginUiNotification[];
  /** Absent on older daemons. */
  pages?: PluginPage[];
  /** Mutation counter per plugin, then per scope (session id, or `""` for global). Absent on older daemons. */
  revisions?: Record<string, Record<string, number>>;
}

/** A plugin's sessions in its worker's `sessions.list` row shape; null unless it declares `session.read`. */
export function fetchPluginSessions(pluginId: string): Promise<{ sessions: Record<string, unknown>[] } | null> {
  return fetchJson(`/api/plugins/${encodeURIComponent(pluginId)}/sessions`);
}

/** The host's aggregated UI-state snapshot. Returns an empty state (not null)
 *  shape on the server when no plugin host is running. */
export function fetchPluginUiState(): Promise<PluginUiState | null> {
  return fetchJson<PluginUiState>("/api/plugins/ui-state");
}

/** Lets the next frame load of a page's `url` authenticate when device binding is on; the ticket rides an HttpOnly cookie. */
export async function mintPluginPageTicket(url: string): Promise<boolean> {
  const reply = await send(`${url}/ticket`, { method: "POST" }).catch(() => null);
  return reply?.ok ?? false;
}

export async function setPluginEnabled(id: string, enabled: boolean): Promise<PluginToggleResult> {
  try {
    return await kindRequest(
      `/api/plugins/${encodeURIComponent(id)}/enabled`,
      jsonInit("POST", { enabled }),
      (p) =>
        Array.isArray(p.plugins) && Array.isArray(p.load_errors)
          ? { kind: "ok" as const, data: p as unknown as PluginListResponse }
          : null,
      (status) => `Failed to ${enabled ? "enable" : "disable"} plugin (${status}).`,
    );
  } catch {
    return { kind: "error", message: "Network error." };
  }
}

/** A worker accepted an action. `baselineRevision` is the scope's UI mutation
 *  counter the host read before forwarding; the UI action can hold its spinner
 *  until the polled counter moves off this value. `null` means the daemon did
 *  not report a baseline (older daemon), so the caller skips the wait and just
 *  clears when the POST settles. */
export interface PluginActionAccepted {
  baselineRevision: number | null;
}

/**
 * Forward a plugin UI action (e.g. a "Refresh" or composer button) to the
 * plugin's worker. Fire-and-forget at the worker: the worker runs the named
 * method and re-pushes its UI state, which a later ui-state poll renders.
 * `sessionId` scopes the baseline revision to the firing UI's session. Returns the
 * accepted baseline (or null if the daemon omitted one), or null on read-only
 * (403), no running worker (404), or network failure.
 */
export async function invokePluginAction(
  pluginId: string,
  method: string,
  sessionId?: string,
  params: Record<string, unknown> = {},
): Promise<PluginActionAccepted | null> {
  try {
    const res = await fetch(
      `/api/plugins/${encodeURIComponent(pluginId)}/action`,
      jsonInit("POST", { method, params, session_id: sessionId ?? null }),
    );
    if (!res.ok) return null;
    const payload = (await res.json().catch(() => null)) as Payload | null;
    // A missing baseline is a sentinel, not revision 0, which would wedge the spinner.
    const revision = payload?.baseline_revision;
    return { baselineRevision: typeof revision === "number" ? revision : null };
  } catch {
    return null;
  }
}

/**
 * Invoke an action-less plugin command (`POST /api/plugins/commands/{fqid}/invoke`).
 * These commands (e.g. the GitHub plugin's `status` / `refresh`) carry no client
 * `action`, so the host dispatches a fixed `plugin.command.invoke` notification
 * to the worker. Fire-and-forget: `true` means the daemon accepted and
 * dispatched it, `false` on read-only, unknown command/session, no worker, or
 * network failure.
 */
export async function invokePluginCommand(fqid: string, sessionId: string): Promise<boolean> {
  try {
    return await fetchOk(
      `/api/plugins/commands/${encodeURIComponent(fqid)}/invoke`,
      jsonInit("POST", { session_id: sessionId }),
    );
  } catch {
    return false;
  }
}

/** Save settings; the server puts each field in the layer `fetchSettings`
 *  reads it from: `profile` (default: the served one) where it may override,
 *  machine-wide otherwise. */
export function updateSettings(updates: Record<string, unknown>, profile?: string): Promise<SaveOutcome> {
  const params = profile ? `?profile=${encodeURIComponent(profile)}` : "";
  return announceOutcome(sendSave(`/api/settings${params}`, jsonInit("PATCH", updates)));
}

/** Set the machine-wide value of every field in `updates`, for editors that
 *  set the value a profile inherits. */
export function updateMachineSettings(updates: Record<string, unknown>): Promise<SaveOutcome> {
  return announceOutcome(sendSave("/api/settings?layer=machine", jsonInit("PATCH", updates)));
}

/**
 * Sets the global theme (name and/or color mode). Dedicated endpoint, not
 * `PATCH /api/settings`: the theme is a global preference but cosmetic, so it
 * must not trip the passphrase/elevation wall the general settings surface
 * carries. `rejected` on read-only servers (403); `uncertain` on a network
 * failure or 5xx, where the write may still have landed.
 */
export function updateTheme(patch: { name?: string; color_mode?: string }): Promise<SaveOutcome> {
  return announceOutcome(sendSave("/api/theme", jsonInit("PATCH", patch)));
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
    return await fetchOk("/api/app-state/web-tour-seen", { method: "POST" });
  } catch {
    return false;
  }
}

// --- Tips ---

export interface TipDto {
  id: string;
  title: string;
  body: string;
  seen: boolean;
}

export interface TipsResponse {
  /** Mirror of `session.show_tips`; the badge and panel hide when false. */
  enabled: boolean;
  /** Web-eligible tips in catalog order, each flagged with its seen state. */
  tips: TipDto[];
}

/** Fetch the web-surface tips and whether tips are enabled. Null on failure so
 *  the caller simply shows no badge. */
export function fetchTips(): Promise<TipsResponse | null> {
  return fetchJson<TipsResponse>("/api/tips");
}

/** Mark one tip seen (mark-seen-on-view). Best-effort; returns success. Mirrors
 *  {@link markWebTourSeen}: off the elevation wall, blocked on read-only. */
export async function markTipSeen(id: string): Promise<boolean> {
  try {
    return await fetchOk("/api/app-state/tip-seen", jsonInit("POST", { id }));
  } catch {
    return false;
  }
}

/** Set "Show tips on startup" (`session.show_tips`). Dedicated endpoint, not
 *  `PATCH /api/settings`, so this cosmetic toggle stays off the passphrase/
 *  elevation wall. Returns false on read-only (403) or network failure. */
export async function setShowTips(enabled: boolean): Promise<boolean> {
  try {
    return await fetchOk("/api/tips/show", jsonInit("POST", { enabled }));
  } catch {
    return false;
  }
}

// --- Web UI state sync (server-side mirror of synced localStorage keys) ---

/** Fetch the server-side UI-state blob: a flat map of localStorage key ->
 *  stored string value. Null on failure so the caller falls back to its local
 *  cache. Single-tenant: this is the one user's synced preferences. */
export async function getWebUiState(): Promise<Record<string, string> | null> {
  return fetchJson<Record<string, string>>("/api/app-state/web-ui-state");
}

/** Merge a partial update into the server UI-state blob. Values are strings to
 *  set, or `null` to delete the key. Best-effort; returns success. */
export async function patchWebUiState(patch: Record<string, string | null>): Promise<boolean> {
  try {
    return await fetchOk("/api/app-state/web-ui-state", jsonInit("PATCH", patch));
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
    return await fetchJson<VolumeIgnoresPreviewResponse>(`/api/sandbox/volume-ignores-preview?${params.toString()}`);
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
    return await fetchOk("/api/app-state/volume-ignores-globs-acknowledged", { method: "POST" });
  } catch {
    return false;
  }
}

// --- Profile management ---

export async function createProfile(name: string): Promise<boolean> {
  try {
    return await fetchOk("/api/profiles", jsonInit("POST", { name }));
  } catch {
    return false;
  }
}

export async function deleteProfile(name: string): Promise<boolean> {
  try {
    return await fetchOk(`/api/profiles/${encodeURIComponent(name)}`, { method: "DELETE" });
  } catch {
    return false;
  }
}

export async function renameProfile(name: string, newName: string): Promise<boolean> {
  try {
    return await fetchOk(`/api/profiles/${encodeURIComponent(name)}/rename`, jsonInit("PATCH", { new_name: newName }));
  } catch {
    return false;
  }
}

export function setDefaultProfile(name: string): Promise<boolean> {
  // Without `--profile` the server serves the default, so its settings change too.
  return announceSave(fetchOk("/api/default-profile", jsonInit("PATCH", { name })));
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
 *  rejects (`validate_patch` in src/session/config/settings_schema/policy.rs); we
 *  reject them client side too as defense in depth. Because both sides read the
 *  same schema, there is no hand-kept list to keep in sync. */
export function profileWritableSections(schema: SettingsFieldDescriptor[]): Set<string> {
  const sections = new Set(schema.map((d) => d.section));
  sections.add("description");
  return sections;
}

/** PATCH profile settings, refusing blocked sections. Without a schema, defers to the server's guard. */
export async function updateProfileSettings(name: string, updates: Record<string, unknown>): Promise<SaveOutcome> {
  const schema = await getSettingsSchema();
  // With the schema in hand, refuse a blocked section before sending. If the
  // schema fetch failed, defer to the server's authoritative guard rather than
  // blocking a legitimate save on a transient network error.
  if (schema) {
    const writable = profileWritableSections(schema);
    const blocked = Object.keys(updates).find((key) => !writable.has(key));
    if (blocked !== undefined) {
      console.error(`updateProfileSettings: refusing to send blocked profile section "${blocked}"`);
      return "rejected";
    }
  }
  return announceOutcome(sendSave(`/api/profiles/${encodeURIComponent(name)}/settings`, jsonInit("PATCH", updates)));
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
  /** CityHall client mode (`AOE_CITYHALL_MODE`). Locks the dashboard
   *  down to a composer + structured-view end-user client: name-only
   *  session creation, theme-only settings, no terminal / diff /
   *  project-management. Enforced server-side too. See #7. */
  cityhall_mode: boolean;
  profile: string;
  /** Resolved `acp.show_tool_durations` from the active profile's
   *  config. Drives the per-tool elapsed-time label in the acp
   *  web UI; cross-device since it lives in config.toml. */
  acp_show_tool_durations: boolean;
  acp_wrap_tool_output: boolean;
  /** Active-profile event retention cap; 0 is unlimited. Also bounds client activity. */
  acp_replay_events: number;
  /** Resolved `acp.compaction_reminder` from the active profile's
   *  config; gates the structured view's compaction reminder (#3253). */
  acp_compaction_reminder: boolean;
  /** Resolved `acp.compaction_reminder_percent` from the active
   *  profile's config. */
  acp_compaction_reminder_percent: number;
  acp_notice_display: "persistent" | "timeout" | "hidden";
  acp_notice_timeout_secs: number;
  build_flavor: "debug" | "release";
  /** Entry bundle name, compared against this page's own to detect a stale client. */
  web_build_id?: string | null;
  /** Read-only runtime state of the daemon's sleep-inhibit reconciler.
   *  Always present: `get_about` emits it unconditionally
   *  (`SleepInhibitStatus`, not `Option`). Informational only; no dashboard
   *  flow consumes it yet. */
  sleep_inhibit: {
    /** The `session.prevent_sleep_when_active` toggle as the reconciler
     *  last read it: the raw config toggle, not whether an assertion is
     *  held. */
    prevent_sleep_enabled: boolean;
    /** Whether the daemon holds an OS sleep assertion as of the last
     *  reconcile; can trail the backing child's death by up to one poll
     *  interval. */
    currently_held: boolean;
    /** Whether a real OS backend is still believed able to hold the
     *  assertion. Optimistic: `true` means no failure latched yet, not
     *  verified working. */
    backend_available: boolean;
  };
  /** This daemon run's id; a create's retries send it back as `retry_origin`. */
  create_boot_id?: string;
}

export function fetchAbout(signal?: AbortSignal): Promise<ServerAbout | null> {
  return fetchJson<ServerAbout>("/api/about", { signal });
}

/** The current daemon run's id, read fresh right before a create's first send. */
export async function fetchCreateBootId(): Promise<string | null> {
  return (await fetchAbout())?.create_boot_id ?? null;
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
    return await fetchJson<TelemetryStatus>("/api/telemetry/consent", jsonInit("POST", { enabled }));
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
  void fetch("/api/telemetry/seen", jsonInit("POST", { surface, form_factor: clientFormFactor() })).catch(() => {});
}

/// Report a browser ACP interaction for the daemon's next opt-in snapshot.
/// Best-effort; the daemon only sends counts when the user is opted in.
export function reportAcpInteraction(kind: "prompt_queued"): void {
  void fetch("/api/telemetry/structured-interaction", jsonInit("POST", { kind })).catch(() => {});
}

/** Runtime helper around `ServerAbout.build_flavor`. See #1055. */
export function isDebugBuild(about: ServerAbout | null | undefined): boolean {
  if (!about) return false;
  return about.build_flavor === "debug";
}
export type UpdateCheckMode = "auto" | "notify" | "off";

/** How the Daemon's own binary was installed. */
export type InstallMethod = "homebrew" | "release" | "cargo" | "nix" | "unknown";

export interface UpdateStatus {
  update_check_mode: UpdateCheckMode;
  current_version: string;
  latest_version: string | null;
  update_available: boolean;
  release_url: string | null;
  error: string | null;
  /** Version the user already dismissed the banner for, persisted server-side
   *  in app_state so the acknowledgement is once-per-account, not per device. */
  dismissed_version: string | null;
  /** Absent from an older Daemon; treated as "unknown". */
  install_method?: InstallMethod;
  /** What to run on the Daemon's machine to update; absent alongside `install_method`. */
  update_command?: string;
}

export function fetchUpdateStatus(): Promise<UpdateStatus | null> {
  return fetchJson<UpdateStatus>("/api/system/update-status");
}

/** Persist that the update banner was dismissed for `version`, server-side, so
 *  it stays dismissed across devices (and matches the TUI). Returns true on
 *  success; the banner optimistically hides regardless. */
export async function dismissUpdate(version: string): Promise<boolean> {
  try {
    return await fetchOk("/api/app-state/dismiss-update", jsonInit("POST", { version }));
  } catch {
    return false;
  }
}

// --- Sandbox image ---

/** The Daemon's last pull of an image, kept until the next one starts. */
export type SandboxImagePull = { state: "idle" | "running" | "done" } | { state: "failed"; error: string };

export type SandboxImageState =
  | { state: "available"; remote_digest: string }
  | { state: "current" }
  /** `pinned`, `not_pulled`, `no_local_digest`, `runtime_unavailable`, `unparseable` or `checks_off`: the Daemon can't tell. */
  | { state: "unknown"; reason: string }
  | { state: "check_failed"; error: string };

export type SandboxImageStatus = SandboxImageState & {
  image: string;
  dismissed: boolean;
  checks_enabled: boolean;
  sandbox_in_use: boolean;
  pull: SandboxImagePull;
  /** Why the dashboard can't pull; `pull_command` is then what to run instead. */
  pull_blocked: "read_only" | "cityhall" | "disabled" | null;
  pull_command: string;
  runtime: string;
};

/** The status, `{ error }` when the Daemon refuses (`config_unreadable`, for one), or null offline. */
export async function fetchSandboxImageStatus(): Promise<SandboxImageStatus | { error: string } | null> {
  const reply = await send("/api/sandbox/image-status").catch(() => null);
  if (!reply) return null;
  if (reply.ok) return reply.payload as SandboxImageStatus | null;
  return { error: stringField(reply.payload, "error") ?? `http_${reply.status}` };
}

/** `message` explains a refusal: the gate is off, read-only, or the configuration is unreadable. */
export function pullSandboxImage(): Promise<{ ok: boolean; message?: string }> {
  return okWithMessage("/api/sandbox/image-pull", { method: "POST" });
}

export function dismissSandboxImage(digest: string): Promise<boolean> {
  return fetchOk("/api/sandbox/image-dismiss", jsonInit("POST", { digest }));
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

export interface WorktreeInfo {
  branch: string;
  path: string;
}

export function fetchWorktrees(path: string): Promise<WorktreeInfo[] | null> {
  return fetchJson<WorktreeInfo[]>(`/api/git/worktrees?${new URLSearchParams({ path }).toString()}`);
}

/** Whether `path` is a git repository, per the same gate the session builder
 *  enforces. Returns the boolean, or null on a transient failure so callers
 *  can stay optimistic rather than misreport a repo as a non-repo. Used by the
 *  new-session wizard to disable the worktree toggle for a plain folder. */
export async function fetchIsGitRepo(path: string): Promise<boolean | null> {
  const params = new URLSearchParams({ path });
  const res = await fetchJson<{ is_git_repo: boolean }>(`/api/git/is-repo?${params.toString()}`);
  return res ? res.is_git_repo : null;
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
  /** Registry lifecycle state, same contract as `/api/agents`: omitted
   *  while Active so older daemons and existing consumers read as active.
   *  Mirrors `AgentLifecycle` in src/agents.rs. */
  lifecycle?: AgentLifecycleInfo;
}

/** List ACP registry entries the acp supervisor knows about.
 *  Distinct from `/api/agents` (session-tool agents for the wizard);
 *  this is the *acp* registry used by the rate-limit recovery
 *  modal to populate the handoff target list. See #1282. */
export async function fetchAcpAgents(): Promise<AcpAgentInfo[]> {
  return (await fetchJson<AcpAgentInfo[]>("/api/acp/agents")) ?? [];
}

/** One agent's last-observed config options, from the recall cache. */
export interface AgentOptionEntry {
  /** RFC 3339 timestamp of the last observation, for freshness display. */
  updated_at: string;
  options: ConfigOptionDescriptor[];
}

/** Recall cache of the config options each agent last advertised, keyed by
 *  agent name. Feeds the per-agent defaults settings page dropdowns without a
 *  live session; empty until an agent has run at least once. See #2631. */
export interface AcpOptionCatalog {
  version: number;
  agents: Record<string, AgentOptionEntry>;
}

export async function fetchAcpOptionCatalog(): Promise<AcpOptionCatalog> {
  return (await fetchJson<AcpOptionCatalog>("/api/acp/option-catalog")) ?? { version: 1, agents: {} };
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
  return fetchJson<SwitchAgentResponse>(
    `/api/sessions/${encodeURIComponent(sessionId)}/acp/switch-agent`,
    jsonInit("POST", body),
  );
}

export interface SwitchProviderResponse {
  session_id: string;
  provider: string;
  /** Whether a model pick was replaced by the new provider's default. */
  model_cleared: boolean;
  status: string;
}

/** Re-route a Claude session to `provider`, keeping the transcript. Throws with
 *  the server's reason rather than collapsing to null: the refusals worth
 *  reading (an agent that does not route through a provider, a respawn that
 *  failed after the worker stopped) exist only in the body, and the second kind
 *  leaves the session changed, so the caller must not read a failure as a
 *  no-op. */
export async function switchAcpProvider(sessionId: string, provider: string): Promise<SwitchProviderResponse> {
  const res = await fetch(
    `/api/sessions/${encodeURIComponent(sessionId)}/acp/switch-provider`,
    jsonInit("POST", { provider }),
  );
  const body = await res.text();
  if (!res.ok) {
    // Structured refusals carry `message`; the respawn failures are plain text.
    let message: string | undefined;
    try {
      message = stringField(JSON.parse(body) as Payload, "message");
    } catch {
      message = undefined;
    }
    throw new Error(message || body.slice(0, 200) || `request failed (${res.status})`);
  }
  return JSON.parse(body) as SwitchProviderResponse;
}

/** Response from the view-switch endpoints. `view` is the session's view
 *  after the swap. */
export interface ViewSwitchResponse {
  session_id: string;
  view?: "structured" | "terminal";
}

/** Switch a session into structured view (POST /acp/enable). For a claude
 *  session with a resumable transcript the conversation is carried over; other
 *  agents restart fresh. Resolves with the updated view or null on non-2xx. */
export async function acpEnable(sessionId: string): Promise<ViewSwitchResponse | null> {
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

// The daemon owns the structured-view prompt queue; /queue reads it on demand.
// Queued follow-ups survive client reloads and drain server-side.

/** Metadata-only view of one queued-prompt attachment. The bytes live
 *  server-side (the pending-attachment store) and are delivered on drain, so
 *  the client only receives id/kind/mime/name/size for display. */
export interface ServerQueuedAttachmentRef {
  id: string;
  kind: "image" | "audio" | "resource";
  mime_type: string;
  name?: string | null;
  size: number;
}

/** One entry of a session's server-owned queue, as the API returns it. */
export interface ServerQueuedPrompt {
  id: string;
  seq: number;
  text: string;
  attachments?: ServerQueuedAttachmentRef[];
  created_at: string;
  origin_device?: string | null;
}

/** Attachment upload shape for the queue enqueue, matching `/acp/prompt`'s
 *  `PromptAttachmentUpload` (base64 `data`, no `data:` prefix). */
export interface QueueAttachmentUpload {
  kind: "image" | "audio" | "resource";
  mimeType: string;
  name?: string;
  dataB64: string;
}

const stringList = (value: unknown) => (Array.isArray(value) ? value : []);

/** Enqueue a prompt server-side (POST /queue). `id` is the client-minted stable
 *  id so an optimistic row reconciles against the returned entry; re-posting the
 *  same id updates it in place rather than duplicating. Returns the stored entry
 *  (with its assigned `seq`) or null on non-2xx. */
export async function enqueueServerPrompt(
  sessionId: string,
  prompt: {
    id: string;
    text: string;
    originDevice?: string;
    attachments?: QueueAttachmentUpload[];
  },
): Promise<ServerQueuedPrompt | null> {
  return fetchJson<ServerQueuedPrompt>(
    queuePath(sessionId),
    jsonInit("POST", {
      id: prompt.id,
      text: prompt.text,
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

/** The session's server queue, ordered by `seq` (GET /queue). Always returns an
 *  array: a non-array body (error page, unexpected shape) yields `[]` so callers
 *  can `.map` without guarding. */
export async function listServerQueue(sessionId: string): Promise<ServerQueuedPrompt[]> {
  const rows = await fetchJson<ServerQueuedPrompt[]>(queuePath(sessionId));
  return Array.isArray(rows) ? rows : [];
}

/** Replace a queued prompt's text (PATCH /queue/{id}). */
export async function editServerQueuedPrompt(sessionId: string, promptId: string, text: string): Promise<boolean> {
  return fetchOk(queuePath(sessionId, promptId), jsonInit("PATCH", { text }));
}

/** Remove one queued prompt (DELETE /queue/{id}). */
export async function removeServerQueuedPrompt(sessionId: string, promptId: string): Promise<boolean> {
  return fetchOk(queuePath(sessionId, promptId), { method: "DELETE" });
}

/** Drop the whole server queue for a session (DELETE /queue). */
export async function clearServerQueue(sessionId: string): Promise<boolean> {
  return fetchOk(queuePath(sessionId), { method: "DELETE" });
}

// --- Acp install agent (Tier 2 of #2109) ---

export interface InstallAgentResponse {
  session_id: string;
  package: string;
  success: boolean;
  exit_code: number | null;
  stdout: string;
  stderr: string;
  /** Other sessions blocked on the same adapter that were queued for an
   *  automatic respawn (the install is global). See #2109. */
  recovered_sessions: number;
}

/** A refused install; `code` is the server's error code, such as `npm_missing`. */
export class InstallAgentError extends Error {
  readonly code: string | undefined;
  constructor(message: string, code: string | undefined) {
    super(message);
    this.code = code;
  }
}

/** Run `npm install -g` for the session's agent where it runs; throws `InstallAgentError` when refused. */
export async function installAcpAgent(sessionId: string): Promise<InstallAgentResponse> {
  const { ok, status, payload } = await send(`/api/sessions/${encodeURIComponent(sessionId)}/acp/install-agent`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
  });
  if (!ok) {
    const code = stringField(payload, "error");
    throw new InstallAgentError(rawMessage(payload) || code || `Server returned ${status}`, code);
  }
  if (!payload) throw new Error("Server returned an invalid or empty response");
  return payload as unknown as InstallAgentResponse;
}

/** How to install a session's ACP adapter, from `GET /acp/install-info`. */
export interface AgentInstallInfo {
  agent: string;
  display_name: string;
  install_command: string | null;
  npm_installable: boolean;
  /** Whether `install-agent` would accept a request now. */
  install_allowed: boolean;
  /** Why it wouldn't: read-only and CityHall dashboards also can't respawn. */
  install_blocked: "read_only" | "cityhall" | "setting_off" | "container_not_running" | null;
  sandboxed: boolean;
  /** `<runtime> exec <container> npm install -g <pkg>` for a sandboxed npm adapter. */
  container_install_command: string | null;
}

export function fetchAgentInstallInfo(sessionId: string): Promise<AgentInstallInfo | null> {
  return fetchJson<AgentInstallInfo>(`/api/sessions/${encodeURIComponent(sessionId)}/acp/install-info`);
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
    return await fetchOk(`/api/login/sessions/${encodeURIComponent(sessionId)}`, { method: "DELETE" });
  } catch {
    return false;
  }
}

/** Sign every device out (the escape hatch that replaces "restart logs
 *  everyone out"). Ends this session too. Elevation-gated. */
export async function signOutAllDevices(): Promise<boolean> {
  try {
    return await fetchOk("/api/login/logout-all", { method: "POST" });
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

/** The profile's groups, sorted by path; the served profile when omitted. */
export async function fetchGroups(profile?: string): Promise<GroupInfo[]> {
  const query = profile ? `?profile=${encodeURIComponent(profile)}` : "";
  return (await fetchJson<GroupInfo[]>(`/api/groups${query}`)) ?? [];
}

/** Resolve a plugin `dynamic_select` widget's options from the host (#2897).
 *  `depends` carries the current values of the widget's `depends_on` siblings
 *  in declaration order (for acp_models/acp_modes the first entry is the
 *  selected agent). Returns [] on any failure so the picker degrades to an
 *  empty list rather than throwing. */
export async function resolvePluginOptions(
  pluginId: string,
  source: string,
  depends: string[],
): Promise<{ value: string; label: string }[]> {
  try {
    const body = await fetchJson<{ options?: { value: string; label: string }[] }>(
      `/api/plugins/${encodeURIComponent(pluginId)}/settings/options/resolve`,
      jsonInit("POST", { source, depends }),
    );
    return body?.options ?? [];
  } catch {
    return [];
  }
}

export type ProjectTarget = { scope: "global" } | { scope: "profile"; profile: string };
export type ProjectReadContext = ProjectTarget | { scope?: never; profile?: string };

export function projectTarget(scope: ProjectInfo["scope"], profile: string): ProjectTarget {
  return scope === "profile" ? { scope, profile } : { scope };
}

function projectQuery(context: ProjectReadContext): string {
  const query = new URLSearchParams();
  if (context.scope) query.set("scope", context.scope);
  if ("profile" in context && context.profile) query.set("profile", context.profile);
  return query.toString();
}

export async function fetchProjects(context: ProjectReadContext, signal?: AbortSignal): Promise<ProjectInfo[] | null> {
  return fetchJson<ProjectInfo[]>(`/api/projects?${projectQuery(context)}`, { signal });
}

export type ImportableSessionsResult =
  | { ok: true; sessions: ImportableSession[]; truncated: boolean }
  | { ok: false; error: string; message: string };

/** Native sessions `agent` can import under `profile`, newest first, for the import picker. */
export async function listImportableSessions(agent: string, profile?: string): Promise<ImportableSessionsResult> {
  const params = new URLSearchParams({ agent });
  if (profile) params.set("profile", profile);
  const reply = await send(`/api/importable-sessions?${params}`).catch(() => null);
  if (!reply) return { ok: false, error: "network", message: "Network error" };
  const payload = reply.payload ?? {};
  if (!reply.ok) {
    return {
      ok: false,
      error: typeof payload.error === "string" ? payload.error : "server_error",
      message: typeof payload.message === "string" ? payload.message : `Server error (${reply.status})`,
    };
  }
  return {
    ok: true,
    sessions: (payload.sessions as ImportableSession[] | undefined) ?? [],
    truncated: payload.truncated === true,
  };
}

/** Error bodies may be JSON `{message}` or plain text. */
function parseApiErrorText(text: string, status: number): string {
  try {
    return JSON.parse(text).message || `Server error (${status})`;
  } catch {
    return text || `Server error (${status})`;
  }
}

export async function createProject(body: {
  path: string;
  name?: string;
  scope: "global" | "profile";
  profile: string;
  allow_override?: boolean;
  default_base_branch?: string;
  pinned?: boolean;
  overrides?: ProjectOverrides;
}): Promise<{ ok: boolean; error?: string; project?: ProjectInfo }> {
  try {
    const res = await fetch("/api/projects", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    if (!res.ok) {
      return { ok: false, error: parseApiErrorText(await res.text(), res.status) };
    }
    const project = (await res.json()) as ProjectInfo;
    return { ok: true, project };
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

export async function deleteProject(name: string, target: ProjectTarget): Promise<{ ok: boolean; error?: string }> {
  try {
    const res = await fetch(`/api/projects/${encodeURIComponent(name)}?${projectQuery(target)}`, { method: "DELETE" });
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

/** Omitted fields are preserved. Null clears a base branch or individual override. */
export async function updateProject(
  name: string,
  target: ProjectTarget,
  patch: {
    default_base_branch?: string | null;
    pinned?: boolean;
    overrides?: { [K in keyof ProjectOverrides]?: ProjectOverrides[K] | null };
  },
): Promise<{ ok: boolean; error?: string; project?: ProjectInfo }> {
  try {
    const res = await fetch(`/api/projects/${encodeURIComponent(name)}?${projectQuery(target)}`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(patch),
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
  return (await fetchJson<DockerStatusResponse>("/api/docker/status")) ?? { available: false, runtime: null };
}

/** The repo's hooks need approval before this session can be created
 *  (#2066). The `hooks_need_trust` 403 triggers a canonical review; approval
 *  then carries that review's exact fingerprint. */
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

export interface CreationTrustRequest {
  path: string;
  profile?: string;
  scratch?: boolean;
}

export interface CreationTrustReview {
  fingerprint: CreationTrustFingerprint;
  merged_hooks: {
    on_create?: string[];
    on_launch?: string[];
    on_destroy?: string[];
  };
  repo_hooks: {
    on_create?: string[];
    on_launch?: string[];
    on_destroy?: string[];
  };
  mcp_summaries: string[];
  hooks_need_trust: boolean;
  mcp_need_trust: boolean;
}

function parseCreationTrustReview(value: unknown): CreationTrustReview {
  const invalid = () => {
    throw new Error("Invalid creation trust review");
  };
  if (!value || typeof value !== "object") invalid();
  const review = value as Record<string, unknown>;
  const fingerprint = review.fingerprint;
  if (!fingerprint || typeof fingerprint !== "object") invalid();
  const fp = fingerprint as Record<string, unknown>;
  if (
    typeof fp.project_path !== "string" ||
    typeof fp.base_hooks_hash !== "string" ||
    (fp.hooks_hash !== null && typeof fp.hooks_hash !== "string") ||
    (fp.mcp_hash !== null && typeof fp.mcp_hash !== "string") ||
    typeof review.hooks_need_trust !== "boolean" ||
    typeof review.mcp_need_trust !== "boolean" ||
    !Array.isArray(review.mcp_summaries) ||
    !review.mcp_summaries.every((summary) => typeof summary === "string")
  ) {
    invalid();
  }
  for (const field of ["merged_hooks", "repo_hooks"]) {
    const hooks = review[field];
    if (!hooks || typeof hooks !== "object") invalid();
    for (const hook of ["on_create", "on_launch", "on_destroy"]) {
      const commands = (hooks as Record<string, unknown>)[hook];
      if (
        commands !== undefined &&
        (!Array.isArray(commands) || !commands.every((command) => typeof command === "string"))
      ) {
        invalid();
      }
    }
  }
  return value as CreationTrustReview;
}

export async function reviewCreationTrust(
  body: CreationTrustRequest,
): Promise<{ ok: true; review: CreationTrustReview } | { ok: false; error: string }> {
  try {
    const res = await fetch("/api/sessions/creation-trust", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    const text = await res.text();
    if (!res.ok) {
      return { ok: false, error: text || `Server error (${res.status})` };
    }
    return { ok: true, review: parseCreationTrustReview(JSON.parse(text) as unknown) };
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

export async function createSession(body: CreateSessionRequest): Promise<{
  ok: boolean;
  error?: string;
  session?: SessionResponse;
  hooksNeedTrust?: HooksNeedTrust;
  /** The repository's hook configuration changed since the trust review; review it again. */
  trustChanged?: boolean;
  /** No definite answer (dropped request, or a proxy timeout page), so the create may still be running. */
  network?: boolean;
  /** A restarted daemon cannot tell whether the first attempt ran; retrying stops here. */
  outcomeUnknown?: boolean;
}> {
  try {
    const res = await fetch("/api/sessions", jsonInit("POST", body));
    if (res.status === 409 && res.headers.get("aoe-error-code") === "creation_trust_changed") {
      return {
        ok: false,
        error: "Repository hook configuration changed; review it again",
        trustChanged: true,
      };
    }
    if (res.ok) return { ok: true, session: await res.json() };
    const text = await res.text();
    let data: { error?: unknown; message?: string; [k: string]: unknown } | null = null;
    try {
      data = JSON.parse(text);
    } catch {
      // Not AoE's JSON; classified below.
    }
    // Only AoE's typed error body is a verdict. A reverse proxy's timeout or bad-gateway
    // page says only that the upstream reply went missing, and the create may still finish.
    if (typeof data?.error !== "string" && (res.status === 408 || res.status >= 500)) {
      return { ok: false, error: `No answer from the server (${res.status})`, network: true };
    }
    if (!data) return { ok: false, error: `Server error (${res.status}): ${text.slice(0, 200)}` };
    if (data.error === "create_outcome_unknown") {
      return { ok: false, error: data.message || "Whether the session was created is unknown.", outcomeUnknown: true };
    }
    if (data.error !== "hooks_need_trust") return { ok: false, error: data.message || `Server error (${res.status})` };
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
  } catch (e) {
    return { ok: false, error: networkError(e), network: true };
  }
}

/** Progress of an in-flight create sent with `key`; null once it has finished. */
export async function fetchCreateProgress(key: string): Promise<CreateProgress | null> {
  try {
    const res = await fetch(`/api/sessions/create-progress/${encodeURIComponent(key)}`);
    return res.ok ? ((await res.json()) as CreateProgress) : null;
  } catch {
    return null;
  }
}

// --- Clone ---

export async function cloneRepo(
  url: string,
  opts?: { destination?: string; shallow?: boolean; bare?: boolean },
): Promise<{ ok: boolean; path?: string; error?: string }> {
  try {
    const body: Record<string, unknown> = { url };
    if (opts?.destination) body.destination = opts.destination;
    if (opts?.shallow) body.shallow = true;
    if (opts?.bare) body.bare = true;
    const res = await fetch("/api/git/clone", jsonInit("POST", body));
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
  /** This origin has an enrolled passkey. */
  passkey_available?: boolean;
}

export async function loginStatus(): Promise<LoginStatus> {
  return (
    (await fetchJson<LoginStatus>(`/api/login/status?origin=${encodeURIComponent(window.location.origin)}`)) ?? {
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
    return await fetchOk("/api/login/status");
  } catch {
    return false;
  }
}

export async function login(passphrase: string): Promise<{ ok: boolean; error?: string }> {
  return withBindingSecret("Could not create device binding for this browser", async (secret) => {
    const res = await fetch("/api/login", jsonInit("POST", { passphrase, device_binding_secret: secret }));
    if (res.ok) return { ok: true };
    const payload = (await res.json().catch(() => null)) as Payload | null;
    return { ok: false, error: rawMessage(payload) ?? `Login failed (${res.status})` };
  });
}

/** The browser can parse and serialize WebAuthn JSON, which the passkey calls rely on. */
export function passkeysSupported(): boolean {
  return (
    typeof PublicKeyCredential !== "undefined" &&
    typeof PublicKeyCredential.parseRequestOptionsFromJSON === "function" &&
    typeof PublicKeyCredential.parseCreationOptionsFromJSON === "function"
  );
}

interface PasskeyCeremony<Options> {
  transaction_id: string;
  options: { publicKey: Options };
}

type PasskeyResult = { ok: boolean; error?: string; cancelled?: boolean };

/** Start a ceremony, let the browser run it, and finish it. `finishExtra` adds fields to the finish request. */
async function runPasskeyCeremony<Options>(
  startUrl: string,
  startBody: unknown,
  finishUrl: string,
  finishExtra: Record<string, unknown>,
  prompt: (options: Options) => Promise<Credential | null>,
  failure: string,
): Promise<PasskeyResult> {
  const start = await send(startUrl, jsonInit("POST", startBody));
  if (!start.ok) return { ok: false, error: rawMessage(start.payload) ?? `${failure} (${start.status})` };
  const ceremony = start.payload as unknown as PasskeyCeremony<Options>;
  let credential: unknown;
  try {
    const result = await prompt(ceremony.options.publicKey);
    if (!result) return { ok: false, cancelled: true };
    credential = (result as PublicKeyCredential).toJSON();
  } catch (err) {
    if (err instanceof DOMException && err.name === "NotAllowedError") return { ok: false, cancelled: true };
    return { ok: false, error: err instanceof Error ? err.message : failure };
  }
  const finish = await send(
    finishUrl,
    jsonInit("POST", { transaction_id: ceremony.transaction_id, credential, ...finishExtra }),
  );
  return finish.ok ? { ok: true } : { ok: false, error: rawMessage(finish.payload) ?? `${failure} (${finish.status})` };
}

/** Sign in with an enrolled passkey. `cancelled` means the user dismissed the browser prompt. */
export function loginWithPasskey(): Promise<PasskeyResult> {
  return withBindingSecret("Could not create device binding for this browser", (secret) =>
    runPasskeyCeremony<PublicKeyCredentialRequestOptionsJSON>(
      "/api/login/passkey/start",
      undefined,
      "/api/login/passkey/finish",
      { device_binding_secret: secret },
      (options) => navigator.credentials.get({ publicKey: PublicKeyCredential.parseRequestOptionsFromJSON(options) }),
      "Passkey sign-in failed",
    ).catch(() => ({ ok: false as const, error: "Network error" })),
  );
}

/** Enrol a passkey for this origin. Elevation-gated; a 403 pops the passphrase prompt (fetch interceptor). */
export function registerPasskey(label: string): Promise<PasskeyResult> {
  return runPasskeyCeremony<PublicKeyCredentialCreationOptionsJSON>(
    "/api/passkeys/register/start",
    { label },
    "/api/passkeys/register/finish",
    {},
    (options) => navigator.credentials.create({ publicKey: PublicKeyCredential.parseCreationOptionsFromJSON(options) }),
    "Could not add passkey",
  ).catch(() => ({ ok: false, error: "Network error" }));
}

export interface PasskeyInfo {
  credential_id: string;
  label: string;
  origin: string;
  created_at_ms: number;
  last_used_at_ms: number | null;
}

export function fetchPasskeys(): Promise<PasskeyInfo[] | null> {
  return fetchJson<PasskeyInfo[]>("/api/passkeys");
}

/** Elevation-gated; resolves false when the interceptor pops the passphrase prompt. */
export function removePasskey(credentialId: string): Promise<boolean> {
  return fetchOk(`/api/passkeys/${encodeURIComponent(credentialId)}`, { method: "DELETE" });
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
      const { clearSoundCache } = await import("./soundPlayer");
      clearSoundCache();
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
export type SessionEditResult =
  | (SessionMutation & { ok: true; warnings?: string[] })
  | { ok: false; message?: string; warnings?: never; session?: never; cursor?: never };

export async function renameSession(id: string, title: string): Promise<SessionEditResult> {
  try {
    const res = await fetch("/api/sessions/" + id, jsonInit("PATCH", { title }));
    if (!res.ok) {
      const payload = (await res.json().catch(() => null)) as Payload | null;
      return { ok: false, message: stringField(payload, "message") };
    }
    const mutation = await sessionMutation(res);
    if (!mutation)
      return { ok: false, message: "Session rename acknowledgment is unavailable; its outcome may be unknown." };
    const { outcome, ...session } = mutation.session as SessionResponse & { outcome?: Payload };
    const receipt = { session, cursor: mutation.cursor };
    if (!outcome) return { ok: false, message: "Session rename outcome is unavailable; its outcome may be unknown." };
    const warnings = stringList(outcome.warnings).filter((warning): warning is string => typeof warning === "string");
    return warnings.length > 0 ? { ok: true, ...receipt, warnings } : { ok: true, ...receipt };
  } catch {
    return { ok: false };
  }
}

/**
 * Manually re-run smart rename ("Auto-name now") for a still-default-named
 * structured-view session whose automatic rename never landed. Best-effort and
 * async: a 202 means the one-shot was re-triggered, not that the title changed.
 * Returns the server message on failure (409 when the session already has a
 * custom name or has no prompt yet) so the caller can surface it.
 */
export async function smartRenameSession(id: string): Promise<{ ok: boolean; message?: string }> {
  try {
    return await okWithMessage(`/api/sessions/${encodeURIComponent(id)}/smart-rename`, { method: "POST" });
  } catch {
    return { ok: false };
  }
}

/**
 * Request an on-demand "summary of the conversation so far" for a
 * structured-view session (see #2808). Best-effort: a 202 means the summary
 * one-shot started, and the result arrives later as a ConversationSummary
 * event over the structured-view WS. Returns the server's message on a gate
 * failure (not structured, no one-shot agent, sandboxed) so the caller can
 * surface it.
 */
export async function summarizeSession(id: string): Promise<{ ok: boolean; message?: string }> {
  try {
    return await okWithMessage(`/api/sessions/${encodeURIComponent(id)}/summarize`, { method: "POST" });
  } catch {
    return { ok: false };
  }
}

/** Restarts a sandboxed session on a new container from the current local image. */
export async function recreateSessionContainer(id: string): Promise<{ ok: boolean; message?: string }> {
  return okWithMessage(`/api/sessions/${encodeURIComponent(id)}/recreate-container`, { method: "POST" });
}

/** The server re-checks eligibility; a 409 means the session got busy or is no longer out of date. */
export async function restartSessionForUpdate(id: string): Promise<{ ok: boolean; message?: string }> {
  try {
    return await okWithMessage(`/api/sessions/${encodeURIComponent(id)}/restart-for-update`, { method: "POST" });
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
export async function setWorktreeName(id: string, name: string, renameBranch: boolean): Promise<SessionEditResult> {
  try {
    const res = await fetch(
      "/api/sessions/" + id + "/worktree-name",
      jsonInit("PATCH", { name, rename_branch: renameBranch }),
    );
    if (!res.ok) {
      const payload = (await res.json().catch(() => null)) as Payload | null;
      return { ok: false, message: stringField(payload, "message") };
    }
    const mutation = await sessionMutation(res);
    if (!mutation)
      return { ok: false, message: "Worktree edit acknowledgment is unavailable; its outcome may be unknown." };
    const { outcome, ...session } = mutation.session as SessionResponse & { outcome?: Payload };
    const receipt = { session, cursor: mutation.cursor };
    if (!outcome) return { ok: false, message: "Worktree edit outcome is unavailable; its outcome may be unknown." };
    const warnings = stringList(outcome.warnings).filter((warning): warning is string => typeof warning === "string");
    return warnings.length > 0 ? { ok: true, ...receipt, warnings } : { ok: true, ...receipt };
  } catch {
    return { ok: false };
  }
}

/** What happened to the session's agent after a repo was attached. */
export type AttachProjectWorker = "restarted" | "not_running" | "restart_failed";

export interface AttachProjectResult {
  ok: boolean;
  /** Canonical session and cursor are present on every acknowledged success. */
  session?: SessionResponse;
  cursor?: RuntimeCursor;
  /** Server validation message on failure, or the worker message on a failed restart. */
  message?: string;
  worker?: AttachProjectWorker;
  /** Directory leaf the repo was attached under. */
  name?: string;
  branch?: string;
  /** False when aoe checked out a branch the repo already had. */
  branchCreated?: boolean;
  /** New working directory when the attach converted the session into a
   *  workspace; absent when it already was one and nothing moved. */
  movedTo?: string;
  warnings?: string[];
}

/**
 * Attach another repo to a session that already exists, so an agent that turns
 * out to need a second repo keeps its conversation instead of the session being
 * recreated. Converts the session into a multi-repo workspace, which moves its
 * working directory unless it already was one, and restarts it there. See #3103.
 *
 * `project` is a path or the name of a registered project.
 *
 * A 200 with `outcome.worker.status: "restart_failed"` means the repo is attached and durable
 * but the session did not come back, so the caller must surface that rather than
 * treating the call as a plain success.
 */
export async function attachSessionProject(
  id: string,
  project: string,
  opts: { epoch: string; attachExistingBranch?: boolean },
): Promise<AttachProjectResult> {
  try {
    const res = await fetch(
      "/api/sessions/" + id + "/projects",
      jsonInit(
        "POST",
        { project, attach_existing_branch: opts.attachExistingBranch ?? false },
        { "aoe-runtime-epoch": opts.epoch },
      ),
    );
    if (!res.ok) {
      const payload = (await res.json().catch(() => null)) as Payload | null;
      return { ok: false, message: stringField(payload, "message") };
    }
    const mutation = await sessionMutation(res);
    if (!mutation)
      return { ok: false, message: "Project attachment acknowledgment is unavailable; its outcome may be unknown." };
    const { outcome, ...session } = mutation.session as SessionResponse & { outcome?: Payload };
    const receipt = { session, cursor: mutation.cursor };
    if (!outcome)
      return { ok: false, message: "Project attachment outcome is unavailable; its outcome may be unknown." };
    const attached = outcome.attached as Payload | undefined;
    const worker = outcome.worker as Payload | undefined;
    const status = worker?.status;
    if (!attached || !worker || (status !== "restarted" && status !== "not_running" && status !== "restart_failed")) {
      return { ok: false, message: "Project attachment outcome is invalid; its outcome may be unknown." };
    }
    return {
      ok: true,
      ...receipt,
      worker: status,
      message: stringField(worker, "message"),
      name: stringField(attached, "name"),
      branch: stringField(attached, "branch"),
      branchCreated: typeof attached.branch_created === "boolean" ? attached.branch_created : undefined,
      movedTo: stringField(attached, "moved_to"),
      warnings: Array.isArray(outcome.warnings) ? (outcome.warnings as string[]) : undefined,
    };
  } catch {
    return { ok: false };
  }
}

/** Move an existing session to another group, create a new group by
 *  passing a path that does not exist yet, or clear the group with an
 *  empty string (the ungroup sentinel, matching session creation and the
 *  TUI). Hits the dedicated `PATCH /api/sessions/:id/group` sub-route. */
export async function updateSessionGroup(id: string, group: string): Promise<SessionMutation | null> {
  try {
    const res = await fetch(`/api/sessions/${encodeURIComponent(id)}/group`, jsonInit("PATCH", { group }));
    return await sessionMutation(res);
  } catch {
    return null;
  }
}

/** Three-preset helper for the sidebar context menu:
 *  - "off":     set all three overrides to false (silence this session)
 *  - "default": clear all three overrides (inherit server defaults)
 *  - "all":     set all three overrides to true (notify on any event)
 *  Sends all three fields in one PATCH to avoid multi-request ordering. */
export async function setSessionNotifications(
  id: string,
  preset: "off" | "default" | "all",
  expectedEpoch: string,
): Promise<SessionSettingOutcome> {
  const value = preset === "off" ? false : preset === "all" ? true : null;
  try {
    const res = await fetch(
      `/api/sessions/${id}/notifications`,
      jsonInit(
        "PATCH",
        { notify_on_waiting: value, notify_on_idle: value, notify_on_error: value },
        { "aoe-runtime-epoch": expectedEpoch },
      ),
    );
    return await sessionSettingOutcome(res, id, expectedEpoch);
  } catch {
    return { outcome: "uncertain" };
  }
}

/** Set or clear (`null`) one repo's diff base; omit `repo` for a single-repo session. */
export async function setSessionDiffBase(
  id: string,
  baseBranch: string | null,
  repo?: string | null,
): Promise<SessionResponse | null> {
  try {
    return await sessionUpdate(
      id,
      "diff-base",
      jsonInit("PATCH", repo ? { base_branch: baseBranch, repo } : { base_branch: baseBranch }),
    );
  } catch {
    return null;
  }
}

/** Toggle the web-only "pin" marker on a session. Pinned workspaces sink
 *  to the top of the sidebar in all sort modes (manual and lastActivity).
 *  Distinct from the TUI favorite signal. See #1581. */
export async function setSessionPin(id: string, pinned: boolean): Promise<SessionResponse | null> {
  try {
    return await sessionUpdate(id, "pin", jsonInit("PATCH", { pinned }));
  } catch {
    return null;
  }
}

/** Set (or clear, with `null`) a session's color label. Rendered as a colored
 *  status dot in the sidebar; the palette is `red` / `amber` / `green`. Also
 *  settable from the CLI via `aoe session color`. See #2383. */
export async function setSessionColor(
  id: string,
  color: string | null,
  expectedEpoch: string,
): Promise<SessionSettingOutcome> {
  try {
    const res = await fetch(
      `/api/sessions/${id}/color`,
      jsonInit("PATCH", { color }, { "aoe-runtime-epoch": expectedEpoch }),
    );
    return await sessionSettingOutcome(res, id, expectedEpoch);
  } catch {
    return { outcome: "uncertain" };
  }
}

/** Archive or unarchive a session. On archive (with `killPane` true or
 *  omitted), the server tears down all tmux sessions and shuts down the
 *  ACP worker for acp-mode sessions. Sending a message auto-unarchives.
 *  See #1581, #1868. */
export async function setSessionArchive(
  id: string,
  archived: boolean,
  killPane = true,
): Promise<SessionResponse | null> {
  try {
    return await sessionUpdate(id, "archive", jsonInit("PATCH", { archived, kill_pane: killPane }));
  } catch {
    return null;
  }
}

/** Move a session to the trash (#2489): stops the live session (ACP
 *  shutdown, which preserves the transcript, plus optional tmux teardown)
 *  and hides it from the normal list while keeping every durable artifact so
 *  it can be restored. NOT a permanent delete; use `deleteWorkspace` for that. */
export async function trashSession(id: string, killPane = true): Promise<SessionResponse | null> {
  try {
    return withMoveWarning(await sessionUpdate(id, "trash", jsonInit("POST", { kill_pane: killPane })), (o) =>
      stringField(o.relocation as Payload | undefined, "warning"),
    );
  } catch {
    return null;
  }
}

/** Restore a trashed session (#2489): clears `trashed_at`, returning it to
 *  its prior bucket with its transcript and metadata intact. */
export async function restoreSession(id: string): Promise<SessionResponse | null> {
  try {
    return withMoveWarning(await sessionUpdate(id, "restore", jsonInit("POST")), (o) => stringField(o, "warning"));
  } catch {
    return null;
  }
}

/** Stop a session, matching the TUI's `x` keybind: kills the tmux pane and
 *  stops (but does not remove) the Docker container for plain sessions, or
 *  shuts down the worker for structured-view sessions. The session record is
 *  preserved with status `Stopped` and can be resumed later. NOT a delete. */
export async function stopSession(id: string): Promise<SessionResponse | null> {
  try {
    return await sessionUpdate(id, "stop", jsonInit("POST"));
  } catch {
    return null;
  }
}

/** A 409 code for a start refused because the session is archived or trashed. */
export const isStartRefusal = (code: string | undefined) => code === "session_archived" || code === "session_trashed";

export type StartSessionResult =
  | { ok: true; session: SessionResponse }
  | { ok: false; refused: boolean; message?: string };

/**
 * Start (resume) a stopped session, the inverse of stopSession: restarts a
 * plain session's pane or un-parks a structured session so its worker
 * respawns. `refused` marks a 409 for an archived or trashed session, which
 * the server left untouched.
 */
export async function startSession(id: string): Promise<StartSessionResult> {
  try {
    const res = await fetch(`/api/sessions/${id}/start`, { method: "POST" });
    if (res.ok) return { ok: true, session: (await res.json()) as SessionResponse };
    const body = await res.json().catch(() => ({}));
    return {
      ok: false,
      refused: isStartRefusal(typeof body.error === "string" ? body.error : undefined),
      message: typeof body.message === "string" ? body.message : undefined,
    };
  } catch (e) {
    return { ok: false, refused: false, message: e instanceof Error ? e.message : "Network error" };
  }
}

/** Snooze or unsnooze a session. Pass `null` to unsnooze, or a positive
 *  number of minutes between 1 and 43200 (30 days) to snooze. The server
 *  validates against the shared `validate_snooze_duration` so the bounds
 *  match the TUI dialog presets and the CLI's `aoe session snooze`. See
 *  #1581. */
export async function setSessionSnooze(id: string, minutes: number | null): Promise<SessionResponse | null> {
  try {
    return await sessionUpdate(id, "snooze", jsonInit("PATCH", { minutes }));
  } catch {
    return null;
  }
}

/** Flag a session manually unread (`true`) or mark it read (`false`, clearing
 *  both auto and manual markers). Mirrors the TUI `u` toggle; the caller
 *  computes the target from the current state so an optimistic update stays in
 *  sync with the server. */
export async function setSessionUnread(id: string, unread: boolean): Promise<SessionResponse | null> {
  try {
    return await sessionUpdate(id, "unread", jsonInit("PATCH", { unread }));
  } catch {
    return null;
  }
}

export interface TrashPurgeConsent {
  expected_trashed_at: string;
  expected_trash_lifecycle_id: string;
  expected_generation: number;
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
  expected_trash?: boolean;
  trash_consents?: Record<string, TrashPurgeConsent>;
}

export interface WorkspaceDeleteFailure {
  id: string;
  error: string;
}

/** Another lifecycle operation holds the session; nothing changed, retry later. */
export type WorkspaceDeleteBusy = WorkspaceDeleteFailure;

export interface DeleteWorkspaceResult {
  ok: boolean;
  error?: string;
  messages?: string[];
  /** Ids the server actually deleted. */
  deleted?: string[];
  kept?: string[];
  /** Sessions that could not be deleted, with their error. */
  failed?: WorkspaceDeleteFailure[];
  /** Sessions skipped because another operation holds them. */
  busy?: WorkspaceDeleteBusy[];
}

/** Delete siblings before their shared-worktree owner under the selected consent. */
export async function deleteWorkspace(
  sessionIds: string[],
  options: DeleteSessionOptions = {},
): Promise<DeleteWorkspaceResult> {
  try {
    const res = await fetch(`/api/workspaces`, jsonInit("DELETE", { session_ids: sessionIds, ...options }));
    const data = (await res.json().catch(() => ({}))) as {
      message?: string;
      messages?: string[];
      deleted?: string[];
      kept?: string[];
      failed?: WorkspaceDeleteFailure[];
      busy?: WorkspaceDeleteBusy[];
    };
    if (!res.ok) {
      return {
        ok: false,
        error: data.message || `Server error (${res.status})`,
        failed: data.failed,
        busy: data.busy,
      };
    }
    // The endpoint always reports which sessions it removed. A 2xx without a
    // `deleted` array is not a confirmed deletion, so treat it as a failure
    // rather than dropping local state (drafts, caches) for sessions the
    // server may not have touched.
    if (!Array.isArray(data.deleted)) {
      return { ok: false, error: "Server did not confirm which sessions were deleted" };
    }
    return {
      ok: true,
      messages: data.messages,
      deleted: data.deleted,
      kept: data.kept,
      failed: data.failed,
      busy: data.busy,
    };
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

export type McpResolveResult = "applied" | "stale" | "error";

export async function resolveMcpConflict(
  name: string,
  agent: string,
  winner: "aoe" | "native",
  fingerprint: string,
): Promise<McpResolveResult> {
  const url = mcpPath(name, "resolve");
  try {
    const res = await fetch(url, jsonInit("POST", { agent, winner, fingerprint }));
    if (res.ok) return "applied";
    return res.status === 409 ? "stale" : "error";
  } catch {
    return "error";
  }
}

export async function keepMcpServer(name: string, agent: string): Promise<boolean> {
  return fetchOk(mcpPath(name, "keep"), jsonInit("POST", { agent }));
}

export async function dropMcpServer(name: string, agent: string): Promise<boolean> {
  return fetchOk(mcpPath(name, "drop"), jsonInit("POST", { agent }));
}

/** `value` omitted keeps the stored secret on edit. */
export interface McpSecretEntry {
  name: string;
  value?: string;
}

export type McpServerInput =
  | { transport: "stdio"; command: string; args: string[]; env: McpSecretEntry[] }
  | { transport: "http" | "sse"; url: string; headers: McpSecretEntry[] };

export interface McpMutationResult {
  ok: boolean;
  error?: string;
  status?: number;
}

function mcpMutation(url: string, method: string, body?: unknown): Promise<McpMutationResult> {
  return skillRequest<McpMutationResult>(url, body === undefined ? { method } : jsonInit(method, body), () => ({}));
}

export function createMcpServer(name: string, input: McpServerInput): Promise<McpMutationResult> {
  return mcpMutation("/api/mcp/servers", "POST", { name, ...input });
}

export function updateMcpServer(name: string, input: McpServerInput): Promise<McpMutationResult> {
  return mcpMutation(`/api/mcp/servers/${encodeURIComponent(name)}`, "PUT", input);
}

export function deleteMcpServer(name: string): Promise<McpMutationResult> {
  return mcpMutation(`/api/mcp/servers/${encodeURIComponent(name)}`, "DELETE");
}

// --- Skills (#3050) ---

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

async function skillMutation(url: string, method: string, body?: unknown): Promise<SkillMutationResult> {
  try {
    return await skillRequest<SkillMutationResult>(
      url,
      body === undefined ? { method } : jsonInit(method, body),
      (p) => ({
        directory: (p.directory as string | null | undefined) ?? undefined,
      }),
    );
  } catch (error) {
    return {
      ok: false,
      error: `Network error: ${error instanceof Error ? error.message : "connection failed"}`,
    };
  }
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

/** One skill's sync outcome for one agent root. `message` is populated for
 *  `conflict` (the user's own skill, or an edited propagated copy, was left
 *  alone) and `error`. */
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

/** Copy AoE-managed skills into each agent's own skills directory
 *  (`POST /api/skills/sync`). Omitting `roots` (or passing an empty array)
 *  syncs every root. Never overwrites or deletes anything AoE did not itself
 *  deploy and that is not still byte-identical to what AoE deployed; such
 *  cases come back as a `conflict` outcome instead. `replace` names skills
 *  the user has explicitly asked AoE to take over, so a conflict for that
 *  directory is overwritten instead of left alone; omitting it (or passing an
 *  empty array) overwrites nothing. `directories`, when non-empty, reconciles
 *  only those skills and skips orphan removal for everything else, so a
 *  single-skill share cannot touch unrelated skills. Distinct shape from
 *  {@link skillMutation} (outcomes array, not a single directory), so this is
 *  a sibling rather than a reuse. */
export async function syncSkills(options?: {
  roots?: string[];
  replace?: string[];
  directories?: string[];
}): Promise<SkillSyncResult> {
  try {
    return await skillRequest<SkillSyncResult>(
      "/api/skills/sync",
      jsonInit("POST", { roots: options?.roots, replace: options?.replace, directories: options?.directories }),
      (p) => ({ outcomes: (p.outcomes as SkillSyncOutcome[] | undefined) ?? [] }),
      { outcomes: [] },
    );
  } catch (error) {
    return {
      ok: false,
      outcomes: [],
      error: `Network error: ${error instanceof Error ? error.message : "connection failed"}`,
    };
  }
}

// --- Suggested sessions ---

export type SuggestionResult<T = undefined> = { ok: true; data: T } | { ok: false; error: string; stale: boolean };

export function fetchSuggestions(): Promise<SuggestionsSnapshot | null> {
  return fetchJson<SuggestionsSnapshot>("/api/suggestions");
}

async function suggestionRequest<T>(url: string, method: string, body?: unknown): Promise<SuggestionResult<T>> {
  try {
    const res = await fetch(url, jsonInit(method, body));
    const text = await res.text();
    if (!res.ok) return { ok: false, error: parseApiErrorText(text, res.status), stale: res.status === 409 };
    return { ok: true, data: (text ? JSON.parse(text) : undefined) as T };
  } catch (error) {
    return {
      ok: false,
      error: `Network error: ${error instanceof Error ? error.message : "connection failed"}`,
      stale: false,
    };
  }
}

export const refreshSuggestions = () => suggestionRequest("/api/suggestions/refresh", "POST");

export const runSuggestionFix = (providerId: string) =>
  suggestionRequest("/api/suggestions/fix", "POST", { provider_id: providerId });

/** Asks every provider to re-run its setup checks; results arrive on the next snapshot. */
export const runSuggestionChecks = () => suggestionRequest("/api/suggestions/checks/run", "POST");

export const dismissSuggestion = (providerId: string, id: string, dismissed: boolean) =>
  suggestionRequest(`/api/suggestions/${dismissed ? "dismiss" : "undismiss"}`, "POST", { provider_id: providerId, id });

export const updateSuggestionSource = (providerId: string, criterion: string, enabled: boolean) =>
  suggestionRequest("/api/suggestions/sources", "PATCH", { provider_id: providerId, criterion, enabled });

/** Resolves the wizard prefill. Creates nothing. */
export const prepareSuggestion = (providerId: string, id: string) =>
  suggestionRequest<SuggestionPrepared>("/api/suggestions/prepare", "POST", { provider_id: providerId, id });

export type SuggestionPrefsPatch = Partial<Omit<SuggestionPrefs, "provider_order">> & {
  provider_order?: string[];
  provider_enabled?: Record<string, boolean>;
};

export const updateSuggestionPrefs = (patch: SuggestionPrefsPatch) =>
  suggestionRequest("/api/suggestions/prefs", "PATCH", patch);

export async function fetchSuggestionTemplates(): Promise<SuggestionTemplate[] | null> {
  const body = await fetchJson<{ templates: SuggestionTemplate[] }>("/api/suggestions/templates");
  return body?.templates ?? null;
}

export const putSuggestionTemplate = (providerId: string, criterion: string, template: string) =>
  suggestionRequest("/api/suggestions/templates", "PUT", { provider_id: providerId, criterion, template });

/** Drops the override, so the provider's own prompt applies again. */
export const resetSuggestionTemplate = (providerId: string, criterion: string) =>
  suggestionRequest("/api/suggestions/templates", "DELETE", { provider_id: providerId, criterion });

export async function fetchSuggestionMappings(): Promise<SuggestionMapping[] | null> {
  const body = await fetchJson<{ mappings: SuggestionMapping[] }>("/api/suggestions/mappings");
  return body?.mappings ?? null;
}

export const putSuggestionMapping = (providerId: string, mappingKey: string, projects: string[]) =>
  suggestionRequest("/api/suggestions/mappings", "PUT", {
    provider_id: providerId,
    mapping_key: mappingKey,
    projects,
  });

export const deleteSuggestionMapping = (providerId: string, mappingKey: string) =>
  suggestionRequest(
    `/api/suggestions/mappings?${new URLSearchParams({ provider_id: providerId, mapping_key: mappingKey })}`,
    "DELETE",
  );
