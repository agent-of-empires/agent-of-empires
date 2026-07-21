import { useCallback, useSyncExternalStore } from "react";

import { DEFAULT_PERSISTENT_TERMINALS, normalizePersistentTerminalLimit } from "../lib/persistentTerminals";
import { safeGetItem, safeSetItem } from "../lib/safeStorage";

const STORAGE_KEY = "aoe-web-settings";

export interface WebSettings {
  mobileFontSize: number;
  desktopFontSize: number;
  terminalFontFamily: string;
  autoOpenKeyboard: boolean;
  persistentTerminals: boolean;
  maxPersistentTerminals: number;
  diffViewMode: "flat" | "tree";
  diffViewLayout: "unified" | "split";
  collapsedDiffDirs: string[];
  /** Which edge the session sidebar slides in from on mobile. Client-local;
   *  desktop layout (md:static) is unaffected. See #2244. */
  sidebarSide: "left" | "right";
  /** Auto-open the diff pane in newly opened sessions (#3035). Off keeps it
   *  closed by default; the activity-bar toggle still opens it on demand. */
  autoOpenDiffPane: boolean;
  /** Auto-open a terminal pane in newly opened sessions (#3035). */
  autoOpenTerminalPane: boolean;
  /** Auto-open plugin panes (e.g. the GitHub PR pane) when available (#3035).
   *  Unlike the diff/terminal flags this is an ongoing policy: turning it back
   *  on can add newly available plugin panes to existing sessions too. */
  autoOpenPluginPanes: boolean;
}

function getDefaults(): WebSettings {
  return {
    mobileFontSize: 8,
    desktopFontSize: 14,
    terminalFontFamily: "",
    autoOpenKeyboard: true,
    persistentTerminals: false,
    maxPersistentTerminals: DEFAULT_PERSISTENT_TERMINALS,
    diffViewMode: window.innerWidth < 768 ? "flat" : "tree",
    diffViewLayout: "unified",
    collapsedDiffDirs: [],
    sidebarSide: "left",
    autoOpenDiffPane: true,
    autoOpenTerminalPane: true,
    autoOpenPluginPanes: true,
  };
}

function normalizeBool(value: unknown, fallback: boolean): boolean {
  return typeof value === "boolean" ? value : fallback;
}

function normalizeSnapshot(settings: WebSettings): WebSettings {
  const defaults = getDefaults();
  return {
    ...settings,
    persistentTerminals:
      typeof settings.persistentTerminals === "boolean" ? settings.persistentTerminals : defaults.persistentTerminals,
    maxPersistentTerminals: normalizePersistentTerminalLimit(settings.maxPersistentTerminals),
    // localStorage is user-editable: a corrupted stringy "false" must not read
    // truthy and silently auto-open panes the user disabled.
    autoOpenDiffPane: normalizeBool(settings.autoOpenDiffPane, defaults.autoOpenDiffPane),
    autoOpenTerminalPane: normalizeBool(settings.autoOpenTerminalPane, defaults.autoOpenTerminalPane),
    autoOpenPluginPanes: normalizeBool(settings.autoOpenPluginPanes, defaults.autoOpenPluginPanes),
  };
}

function getSnapshot(): WebSettings {
  const raw = safeGetItem(STORAGE_KEY);
  if (raw) {
    try {
      return normalizeSnapshot({ ...getDefaults(), ...JSON.parse(raw) });
    } catch {
      // malformed JSON; fall through to defaults
    }
  }
  return getDefaults();
}

/** Fresh, normalized settings read outside React. Used by non-reactive code
 *  paths (e.g. the pane-layout `setStore` updater) that must read the latest
 *  prefs synchronously without subscribing, avoiding a stale closure. */
export function getWebSettingsSnapshot(): WebSettings {
  return getSnapshot();
}

// Subscribers for useSyncExternalStore
let listeners: Array<() => void> = [];

function subscribe(listener: () => void) {
  listeners = [...listeners, listener];
  return () => {
    listeners = listeners.filter((l) => l !== listener);
  };
}

function emitChange() {
  for (const l of listeners) l();
}

// Cache snapshot to return stable reference when nothing changed
let cachedRaw: string | null = null;
let cachedSettings: WebSettings = getDefaults();

function getStableSnapshot(): WebSettings {
  const raw = safeGetItem(STORAGE_KEY);
  if (raw !== cachedRaw) {
    cachedRaw = raw;
    cachedSettings = getSnapshot();
  }
  return cachedSettings;
}

export function useWebSettings() {
  const settings = useSyncExternalStore(subscribe, getStableSnapshot);

  const update = useCallback((patch: Partial<WebSettings>) => {
    const current = getSnapshot();
    const next = { ...current, ...patch };
    if (!safeSetItem(STORAGE_KEY, JSON.stringify(next))) {
      console.warn("aoe-web-settings: failed to persist (storage full or disabled)");
    }
    cachedRaw = null;
    emitChange();
  }, []);

  return { settings, update };
}
