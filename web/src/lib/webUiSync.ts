// Mirror a registered set of localStorage UI preferences to the server through the safeStorage write chokepoint, so they follow the single user across browsers.

import { getWebUiState, patchWebUiState } from "./api";
import { configureStorageSync } from "./safeStorage";
import { SETTINGS_INTRO_KEY_PREFIX } from "./settingsIntros";
import { markWebUiStateHydrated } from "./webUiHydration";

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

// Group-collapse keys are per repo/group and Settings intro folds per tab, so match by prefix.
const KEY_PREFIXES = [
  "aoe-repo-collapsed-",
  "aoe-nested-group-collapsed-",
  "aoe-group-collapsed-",
  SETTINGS_INTRO_KEY_PREFIX,
];

export function isSyncedKey(key: string): boolean {
  return EXACT_KEYS.has(key) || KEY_PREFIXES.some((p) => key.startsWith(p));
}

// Debounced so bursts collapse into one PATCH.
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

/** Idempotent. */
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
    // storage disabled or full; non-fatal
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

const HYDRATION_DEADLINE_MS = 15_000;

/** Pull server values into localStorage (server wins), without echoing writes back. */
export async function hydrateWebUiStateFromServer(signal?: AbortSignal): Promise<void> {
  const controller = new AbortController();
  const requestSignal = signal ? AbortSignal.any([signal, controller.signal]) : controller.signal;
  const deadline = setTimeout(() => controller.abort(), HYDRATION_DEADLINE_MS);
  try {
    await applyServerState(requestSignal);
  } finally {
    clearTimeout(deadline);
    // Cancelled mounts must not settle a later mount's hydration.
    if (!signal?.aborted) markWebUiStateHydrated();
  }
}

async function applyServerState(signal: AbortSignal): Promise<void> {
  const server = await getWebUiState(signal);
  if (!server || signal.aborted) return;

  // Seed the server only when it has never stored anything; later a missing key may mean deleted elsewhere.
  if (Object.keys(server).length === 0) {
    const backfill: Record<string, string | null> = {};
    for (const key of enumerateLocalSyncedKeys()) {
      const local = window.localStorage?.getItem(key);
      if (local != null) backfill[key] = local;
    }
    if (Object.keys(backfill).length > 0) void patchWebUiState(backfill);
    return;
  }

  for (const [key, value] of Object.entries(server)) {
    rawSet(key, value);
  }
}
