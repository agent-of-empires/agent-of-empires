import { useCallback, useSyncExternalStore } from "react";

import { DEFAULT_CONVERSATION_FONT_SIZE, normalizeConversationFontSize } from "../lib/conversationFontSize";
import { DEFAULT_PERSISTENT_TERMINALS, normalizePersistentTerminalLimit } from "../lib/persistentTerminals";
import { safeGetItem, safeSetItem } from "../lib/safeStorage";

const STORAGE_KEY = "aoe-web-settings";

export interface WebSettings {
  mobileFontSize: number;
  desktopFontSize: number;
  structuredMobileFontSize: number;
  structuredDesktopFontSize: number;
  terminalFontFamily: string;
  autoOpenKeyboard: boolean;
  persistentTerminals: boolean;
  maxPersistentTerminals: number;
  diffViewMode: "flat" | "tree";
  diffViewLayout: "unified" | "split";
  markdownPreview: "rendered" | "raw";
  collapsedDiffDirs: string[];
  sidebarSide: "left" | "right";
  sidebarCompact: boolean;
  autoOpenDiffPane: boolean;
  autoOpenTerminalPane: boolean;
  autoOpenPluginPanes: boolean;
}

function getDefaults(): WebSettings {
  return {
    mobileFontSize: 8,
    desktopFontSize: 14,
    structuredMobileFontSize: DEFAULT_CONVERSATION_FONT_SIZE,
    structuredDesktopFontSize: DEFAULT_CONVERSATION_FONT_SIZE,
    terminalFontFamily: "",
    autoOpenKeyboard: true,
    persistentTerminals: false,
    maxPersistentTerminals: DEFAULT_PERSISTENT_TERMINALS,
    diffViewMode: window.innerWidth < 768 ? "flat" : "tree",
    diffViewLayout: "unified",
    markdownPreview: "rendered",
    collapsedDiffDirs: [],
    sidebarSide: "left",
    sidebarCompact: false,
    autoOpenDiffPane: true,
    autoOpenTerminalPane: true,
    autoOpenPluginPanes: false,
  };
}

function normalizeBool(value: unknown, fallback: boolean): boolean {
  return typeof value === "boolean" ? value : fallback;
}

function normalizeSnapshot(settings: WebSettings): WebSettings {
  const defaults = getDefaults();
  return {
    ...settings,
    persistentTerminals: normalizeBool(settings.persistentTerminals, defaults.persistentTerminals),
    maxPersistentTerminals: normalizePersistentTerminalLimit(settings.maxPersistentTerminals),
    structuredMobileFontSize: normalizeConversationFontSize(settings.structuredMobileFontSize),
    structuredDesktopFontSize: normalizeConversationFontSize(settings.structuredDesktopFontSize),
    sidebarCompact: normalizeBool(settings.sidebarCompact, defaults.sidebarCompact),
    autoOpenDiffPane: normalizeBool(settings.autoOpenDiffPane, defaults.autoOpenDiffPane),
    autoOpenTerminalPane: normalizeBool(settings.autoOpenTerminalPane, defaults.autoOpenTerminalPane),
    autoOpenPluginPanes: normalizeBool(settings.autoOpenPluginPanes, defaults.autoOpenPluginPanes),
    markdownPreview:
      settings.markdownPreview === "rendered" || settings.markdownPreview === "raw"
        ? settings.markdownPreview
        : defaults.markdownPreview,
  };
}

function getSnapshot(): WebSettings {
  const raw = safeGetItem(STORAGE_KEY);
  if (raw) {
    try {
      return normalizeSnapshot({ ...getDefaults(), ...JSON.parse(raw) });
    } catch {
      // Malformed JSON; use defaults.
    }
  }
  return getDefaults();
}

export { getSnapshot as getWebSettingsSnapshot };

const listeners = new Set<() => void>();

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => void listeners.delete(listener);
}

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
    for (const l of listeners) l();
  }, []);

  return { settings, update };
}
