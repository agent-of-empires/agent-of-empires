import type { LucideIcon } from "lucide-react";

import { usePluginUiEntries } from "./pluginUiContext";
import { lucideIcon, sessionEntries } from "./pluginUi";
import type { PluginUiEntry } from "./api";
import type { DockLocation } from "./panes";

/** A dockable pane contributed by a plugin via the `pane` slot, resolved for
 *  the active session. The id namespaces the plugin + entry so it never
 *  collides with the built-in "diff" / "terminal" pane ids. `icon` is the
 *  plugin's chosen lucide icon (allowlisted), or undefined to fall back to the
 *  generic plugin icon. */
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

/** Fallback chain for a plugin pane's activity-bar/tab icon: the pane's own
 *  runtime payload icon (set by the plugin's worker for this specific pane),
 *  else the plugin's manifest identity icon (a static, per-plugin lucide
 *  name), else undefined so the caller applies its own generic fallback
 *  (the host's `Puzzle` icon). Kept as a pure function, separate from
 *  `usePluginPanes`, so the fallback chain is unit-testable without mounting
 *  the app or the plugin UI-state context. */
export function resolvePaneIcon(
  paneIcon: LucideIcon | undefined,
  manifestIconName: string | undefined,
): LucideIcon | undefined {
  return paneIcon ?? lucideIcon(manifestIconName);
}

/** Plugin panes for the given session, in stable (plugin_id, entry id) order. */
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
