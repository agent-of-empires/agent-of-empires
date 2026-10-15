import type { LucideIcon } from "lucide-react";

import { usePluginUiEntries } from "./pluginUiContext";
import { lucideIcon, sessionEntries } from "./pluginUi";
import type { PluginUiEntry } from "./api";
import type { DockLocation } from "./panes";

/** Id namespaces plugin and entry so it never collides with built-in panes; `icon` is an allowlisted lucide name. */
export interface PluginPane {
  id: string;
  title: string;
  defaultDock: DockLocation;
  icon: LucideIcon | undefined;
  entry: PluginUiEntry;
}

export const PLUGIN_PANE_PREFIX = "plugin:";

export function isPluginPaneId(id: string): boolean {
  return id.startsWith(PLUGIN_PANE_PREFIX);
}

function paneTitle(entry: PluginUiEntry): string {
  const t = entry.payload["title"];
  return typeof t === "string" && t.length > 0 ? t : entry.plugin_id;
}

function defaultDock(entry: PluginUiEntry): DockLocation {
  return entry.payload["default_location"] === "bottom" ? "bottom" : "right";
}

/** The pane's runtime icon, else the plugin's manifest icon, else undefined for the host fallback. */
export function resolvePaneIcon(
  paneIcon: LucideIcon | undefined,
  manifestIconName: string | undefined,
): LucideIcon | undefined {
  return paneIcon ?? lucideIcon(manifestIconName);
}

/** Stable (plugin_id, entry id) order. */
export function usePluginPanes(sessionId: string | null): PluginPane[] {
  const entries = usePluginUiEntries();
  if (!sessionId) return [];
  return sessionEntries(entries, "pane", sessionId).map((entry) => ({
    id: `${PLUGIN_PANE_PREFIX}${entry.plugin_id}:${entry.id}`,
    title: paneTitle(entry),
    defaultDock: defaultDock(entry),
    icon: lucideIcon(typeof entry.payload["icon"] === "string" ? entry.payload["icon"] : undefined),
    entry,
  }));
}
