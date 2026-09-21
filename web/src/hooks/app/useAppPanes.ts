// Dock pane model for the active session: layout, plugin panes, and tab actions.

import { useCallback, useEffect, useMemo, useState } from "react";
import { Puzzle } from "lucide-react";
import type { PaneDisplay } from "../../components/Dock";
import type { DockGroupView } from "../../components/DockGroups";
import { visibleToFullIndex, type DropTarget } from "../../components/paneDnd";
import { fetchPlugins, killTerminal } from "../../lib/api";
import { dockGroups, dockOf, dockTabs, isDockCollapsed, usePaneLayout } from "../../lib/paneLayout";
import { BUILTIN_PANES, isTerminalTabId, terminalIndexOf, type DockLocation } from "../../lib/panes";
import { isPluginPaneId, resolvePaneIcon, usePluginPanes, type PluginPane } from "../../lib/pluginPanes";

const DOCKS: DockLocation[] = ["right", "bottom"];

export function useAppPanes(activeSessionId: string | null, autoOpenPluginPanes: boolean) {
  const layoutApi = usePaneLayout(activeSessionId);
  const {
    layout: paneLayout,
    closeTab,
    activateTab,
    moveTab,
    placeTab,
    toggleKind,
    togglePlugin,
    syncPlugins,
  } = layoutApi;
  const pluginPanes = usePluginPanes(activeSessionId);
  const pluginPaneById = useMemo(() => new Map<string, PluginPane>(pluginPanes.map((p) => [p.id, p])), [pluginPanes]);

  useEffect(() => {
    syncPlugins(autoOpenPluginPanes ? pluginPanes.map((p) => ({ id: p.id, defaultDock: p.defaultDock })) : []);
  }, [pluginPanes, syncPlugins, autoOpenPluginPanes]);

  const [pluginIdentityById, setPluginIdentityById] = useState<
    Record<string, { icon?: string; iconAssetUrl?: string }>
  >({});
  useEffect(() => {
    void fetchPlugins().then((res) => {
      if (!res) return;
      setPluginIdentityById(
        Object.fromEntries(
          res.plugins.map((p) => [p.id, { icon: p.icon ?? undefined, iconAssetUrl: p.icon_asset_url ?? undefined }]),
        ),
      );
    });
  }, []);

  const paneDescriptor = useCallback(
    (id: string): PaneDisplay => {
      const plugin = pluginPaneById.get(id);
      if (plugin) {
        const identity = pluginIdentityById[plugin.entry.plugin_id];
        const icon = resolvePaneIcon(plugin.icon, identity?.icon) ?? Puzzle;
        return { title: plugin.title, icon, iconAssetUrl: identity?.iconAssetUrl };
      }
      if (isTerminalTabId(id)) {
        const idx = terminalIndexOf(id);
        const term = BUILTIN_PANES.find((p) => p.id === "terminal")!;
        return { title: idx === 0 ? term.title : `${term.title} ${idx + 1}`, icon: term.icon };
      }
      const d = BUILTIN_PANES.find((p) => p.id === id)!;
      return { title: d.title, icon: d.icon };
    },
    [pluginPaneById, pluginIdentityById],
  );

  // A plugin tab whose pane is no longer offered stays in the layout but is not rendered.
  const tabAvailable = useCallback(
    (id: string) => !id.startsWith("plugin:") || pluginPaneById.has(id),
    [pluginPaneById],
  );
  const renderGroups = useCallback(
    (dock: DockLocation): DockGroupView[] =>
      dockGroups(paneLayout, dock)
        .map((g, group) => {
          const tabs = g.tabs.filter(tabAvailable);
          const active = g.active && tabs.includes(g.active) ? g.active : (tabs[0] ?? null);
          return { group, tabs, active };
        })
        .filter((g) => g.tabs.length > 0),
    [paneLayout, tabAvailable],
  );

  const availableRightGroups = useMemo(() => renderGroups("right"), [renderGroups]);
  const rightDockExplicitlyCollapsed = isDockCollapsed(paneLayout, "right");
  const rightGroups = useMemo(
    () => (rightDockExplicitlyCollapsed ? [] : availableRightGroups),
    [rightDockExplicitlyCollapsed, availableRightGroups],
  );
  const bottomGroups = useMemo(() => renderGroups("bottom"), [renderGroups]);
  const groupsByDock = useMemo(
    () => ({
      right: rightGroups.map((g) => ({ group: g.group, tabs: g.tabs })),
      bottom: bottomGroups.map((g) => ({ group: g.group, tabs: g.tabs })),
    }),
    [rightGroups, bottomGroups],
  );

  const isPaneOpen = (kind: string): boolean => {
    if (kind === "terminal") {
      return DOCKS.some((d) => !isDockCollapsed(paneLayout, d) && dockTabs(paneLayout, d).some(isTerminalTabId));
    }
    const dock = dockOf(paneLayout, kind);
    return dock !== null && !isDockCollapsed(paneLayout, dock);
  };

  const togglePaneAny = useCallback(
    (kind: string) => {
      const defaultDock: DockLocation =
        pluginPaneById.get(kind)?.defaultDock ?? BUILTIN_PANES.find((p) => p.id === kind)?.defaultDock ?? "right";
      if (isPluginPaneId(kind)) togglePlugin(kind, defaultDock);
      else toggleKind(kind as "diff" | "terminal" | "agents" | "files", defaultDock);
    },
    [toggleKind, togglePlugin, pluginPaneById],
  );

  const openAgentsPane = useCallback(() => {
    const dock = dockOf(paneLayout, "agents");
    if (dock) activateTab(dock, "agents");
    else toggleKind("agents", "right");
  }, [paneLayout, activateTab, toggleKind]);

  // The first terminal is the session's own; extra terminals are killed server-side before closing.
  const closePaneAny = useCallback(
    (id: string) => {
      const idx = isTerminalTabId(id) ? terminalIndexOf(id) : 0;
      if (idx < 1) {
        closeTab(id);
      } else if (activeSessionId) {
        void killTerminal(activeSessionId, idx).then((ok) => {
          if (ok) closeTab(id);
        });
      }
    },
    [closeTab, activeSessionId],
  );

  const placeVisibleTab = useCallback(
    (id: string, target: DropTarget) => {
      if (target.newGroup) {
        placeTab(id, { dock: target.dock, group: target.group, newGroup: true });
        return;
      }
      const fullBase = (dockGroups(paneLayout, target.dock)[target.group]?.tabs ?? []).filter((tab) => tab !== id);
      const index = visibleToFullIndex(fullBase, target.index ?? fullBase.length, tabAvailable);
      placeTab(id, { dock: target.dock, group: target.group, index });
    },
    [paneLayout, placeTab, tabAvailable],
  );

  return {
    ...layoutApi,
    pluginPanes,
    pluginPaneById,
    paneDescriptor,
    availableRightGroups,
    rightDockCollapsed: rightDockExplicitlyCollapsed || availableRightGroups.length === 0,
    rightGroups,
    bottomGroups,
    groupsByDock,
    isPaneOpen,
    togglePaneAny,
    openAgentsPane,
    closePaneAny,
    movePaneAny: moveTab,
    placeVisibleTab,
  };
}
