import { useCallback, useSyncExternalStore } from "react";

import {
  DEFAULT_PERSISTENT_TERMINALS,
  normalizePersistentTerminalLimit,
} from "../lib/persistentTerminals";
import { safeGetItem, safeSetItem } from "../lib/safeStorage";

const STORAGE_KEY = "aoe-web-settings";

export type ProjectStripShortcut =
  | "alt-hl"
  | "ctrl-alt-hl"
  | "ctrl-hl"
  | "disabled";

export interface WebSettings {
  mobileFontSize: number;
  desktopFontSize: number;
  autoOpenKeyboard: boolean;
  projectStrip: boolean;
  projectStripShortcut: ProjectStripShortcut;
  persistentTerminals: boolean;
  maxPersistentTerminals: number;
  diffViewMode: "flat" | "tree";
  collapsedDiffDirs: string[];
}

function getDefaults(): WebSettings {
  return {
    mobileFontSize: 8,
    desktopFontSize: 14,
    autoOpenKeyboard: true,
    projectStrip: false,
    projectStripShortcut: "ctrl-alt-hl",
    persistentTerminals: false,
    maxPersistentTerminals: DEFAULT_PERSISTENT_TERMINALS,
    diffViewMode: window.innerWidth < 768 ? "flat" : "tree",
    collapsedDiffDirs: [],
  };
}

function isProjectStripShortcut(value: unknown): value is ProjectStripShortcut {
  return (
    value === "alt-hl" ||
    value === "ctrl-alt-hl" ||
    value === "ctrl-hl" ||
    value === "disabled"
  );
}

function normalizeSnapshot(settings: WebSettings): WebSettings {
  const defaults = getDefaults();
  return {
    ...settings,
    persistentTerminals:
      typeof settings.persistentTerminals === "boolean"
        ? settings.persistentTerminals
        : defaults.persistentTerminals,
    projectStrip:
      typeof settings.projectStrip === "boolean"
        ? settings.projectStrip
        : defaults.projectStrip,
    projectStripShortcut: isProjectStripShortcut(settings.projectStripShortcut)
      ? settings.projectStripShortcut
      : defaults.projectStripShortcut,
    maxPersistentTerminals: normalizePersistentTerminalLimit(
      settings.maxPersistentTerminals,
    ),
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
