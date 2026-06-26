import { usePluginUiEntries } from "./pluginUiContext";
import { sessionEntries } from "./pluginUi";
import type { PluginUiEntry } from "./api";
import type { DockLocation } from "./panes";

/** A dockable pane contributed by a plugin via the `pane` slot, resolved for
 *  the active session. The id namespaces the plugin + entry so it never
 *  collides with the built-in "diff" / "terminal" pane ids. */
export interface PluginPane {
  id: string;
  title: string;
  defaultDock: DockLocation;
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

/** Plugin panes for the given session, in stable (plugin_id, entry id) order. */
export function usePluginPanes(sessionId: string | null): PluginPane[] {
  const entries = usePluginUiEntries();
  if (!sessionId) return [];
  return sessionEntries(entries, "pane", sessionId).map((entry) => ({
    id: `${PLUGIN_PANE_PREFIX}${entry.plugin_id}:${entry.id}`,
    title: paneTitle(entry),
    defaultDock: defaultDock(entry),
    entry,
  }));
}
