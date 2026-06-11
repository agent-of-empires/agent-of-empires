// Server-side sync for the web dashboard's UI preferences.
//
// Single-tenant: there is one user, so preferences that today live in
// per-browser localStorage (sidebar sort/axis, tool density, repo
// appearance/order, group collapse, last-used tool, welcome-seen, the web
// settings blob) should follow the user across browsers and devices. Rather
// than rewrite every store, we keep their localStorage API and mirror a
// registered set of keys to a server-side blob through the `safeStorage`
// write chokepoint, then hydrate localStorage from the server before first
// paint. Keys NOT listed here (layout dimensions tied to screen size,
// per-session drafts, caches, the cold-load theme cache) stay purely local.

import { getWebUiState, patchWebUiState } from "./api";
import { configureStorageSync } from "./safeStorage";

// Exact localStorage keys that sync to the server.
const EXACT_KEYS = new Set<string>([
  "aoe-welcome-seen", // theme welcome modal seen (mirrors the server-side tour flag)
  "aoe.acp.toolDensity.v1", // compact/detailed tool display
  "aoe-sidebar-sort-mode",
  "aoe-sidebar-axis",
  "aoe-sidebar-sunk-expanded",
  "aoe-repo-appearance-v1", // repo colors/aliases
  "aoe-repo-group-order-v1", // manual repo-group order
  "aoe-acp-last-tool", // last agent picked in the wizard
  "aoe-last-browse-dir", // last dir browsed (paths are identical across devices)
  "aoe-web-settings", // dashboard prefs (persistent terminals, auto-open keyboard, fonts)
]);

// Group-collapse keys are dynamic (one per repo/group), so match by prefix.
const KEY_PREFIXES = ["aoe-repo-collapsed-", "aoe-nested-group-collapsed-", "aoe-group-collapsed-"];

export function isSyncedKey(key: string): boolean {
  return EXACT_KEYS.has(key) || KEY_PREFIXES.some((p) => key.startsWith(p));
}

// Debounce writes so a burst (dragging the sort handle, toggling several
// groups) collapses into a single PATCH.
let pending: Record<string, string | null> = {};
let timer: ReturnType<typeof setTimeout> | null = null;

function flush(): void {
  timer = null;
  const batch = pending;
  pending = {};
  if (Object.keys(batch).length > 0) void patchWebUiState(batch);
}

function scheduleWrite(key: string, value: string | null): void {
  pending[key] = value;
  if (timer) clearTimeout(timer);
  timer = setTimeout(flush, 400);
}

let initialized = false;

/** Wire the localStorage write chokepoint to the server. Idempotent. */
export function initWebUiSync(): void {
  if (initialized) return;
  initialized = true;
  configureStorageSync(isSyncedKey, scheduleWrite);
}

function rawSet(key: string, value: string): void {
  try {
    // Bare localStorage on purpose: safeSetItem would re-enter the sync
    // chokepoint and PATCH the value we just pulled FROM the server back to it.
    // eslint-disable-next-line no-restricted-syntax
    window.localStorage?.setItem(key, value);
  } catch {
    // storage disabled / quota; non-fatal
  }
}

function enumerateLocalSyncedKeys(): string[] {
  const out: string[] = [];
  try {
    const ls = window.localStorage;
    if (!ls) return out;
    for (let i = 0; i < ls.length; i++) {
      const k = ls.key(i);
      if (k && isSyncedKey(k)) out.push(k);
    }
  } catch {
    // ignore
  }
  return out;
}

/**
 * Pull the server's UI-state blob into localStorage before the app renders so
 * stores read the synced (cross-device) values on first paint. Server wins on
 * conflict; any synced key present only locally is backfilled to the server so
 * an existing browser's prefs propagate on first run. Best-effort: on fetch
 * failure the local cache is left untouched. Uses raw localStorage writes so
 * applying server values does not echo back through the sync chokepoint.
 */
export async function hydrateWebUiStateFromServer(): Promise<void> {
  const server = await getWebUiState();
  if (!server) return;

  // One-time migration: only seed the server from this browser's existing
  // localStorage when the server has NEVER stored anything. Backfilling once
  // the server is non-empty would resurrect keys another device legitimately
  // deleted (a local key absent from the server can mean "deleted elsewhere",
  // not "never synced"), so we don't.
  if (Object.keys(server).length === 0) {
    const backfill: Record<string, string | null> = {};
    for (const key of enumerateLocalSyncedKeys()) {
      const local = window.localStorage?.getItem(key);
      if (local != null) backfill[key] = local;
    }
    if (Object.keys(backfill).length > 0) void patchWebUiState(backfill);
    return;
  }

  // Server is the source of truth: apply its values locally.
  for (const [key, value] of Object.entries(server)) {
    rawSet(key, value);
  }
}
