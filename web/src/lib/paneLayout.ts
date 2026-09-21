import { useCallback, useEffect, useMemo, useState } from "react";

import { getWebSettingsSnapshot, useWebSettings } from "../hooks/useWebSettings";
import { safeGetItem, safeSetItem } from "./safeStorage";
import { BUILTIN_PANES, isTerminalTabId, terminalTabId, type DockLocation } from "./panes";

const LAYOUT_KEY = "aoe-pane-layout-v2";
// v1 per-browser layout, read once to seed the v2 per-session template.
const LEGACY_V1_KEY = "aoe-pane-layout";
// The pre-pane single right-column collapse flag.
const LEGACY_COLLAPSED_KEY = "aoe-right-collapsed";

/** `"diff"`, `"terminal:<n>"`, or `"plugin:<plugin>:<entry>"`. */
export type TabId = string;

export interface PaneGroup {
  tabs: TabId[];
  active: TabId | null;
}

/** One session's tab layout. Only one group per dock renders today; the array leaves room for split groups without a storage migration. */
export interface DockLayout {
  right: PaneGroup[];
  bottom: PaneGroup[];
  // Never reused, so a new tab can't alias a just-closed terminal's tmux session.
  nextTerminalIndex: number;
  // Plugin tabs the user closed, so auto-add does not reopen them.
  closedPlugins: TabId[];
  // Collapsed docks keep their tabs.
  collapsed: Record<DockLocation, boolean>;
}

interface LayoutStore {
  version: 2;
  template: DockLayout;
  sessions: Record<string, DockLayout>;
}

const DOCKS: DockLocation[] = ["right", "bottom"];

function emptyDockLayout(): DockLayout {
  return { right: [], bottom: [], nextTerminalIndex: 1, closedPlugins: [], collapsed: { right: false, bottom: false } };
}

export function dockGroups(layout: DockLayout, dock: DockLocation): PaneGroup[] {
  return layout[dock];
}

export function dockTabs(layout: DockLayout, dock: DockLocation): TabId[] {
  return layout[dock].flatMap((g) => g.tabs);
}

export interface TabAddress {
  dock: DockLocation;
  group: number;
  index: number;
}

export function findTab(layout: DockLayout, tabId: TabId): TabAddress | null {
  for (const dock of DOCKS) {
    const groups = layout[dock];
    for (let group = 0; group < groups.length; group++) {
      const index = groups[group]!.tabs.indexOf(tabId);
      if (index >= 0) return { dock, group, index };
    }
  }
  return null;
}

export function isActiveTab(layout: DockLayout, tabId: TabId): boolean {
  const at = findTab(layout, tabId);
  return at ? layout[at.dock][at.group]!.active === tabId : false;
}

export function dockOf(layout: DockLayout, tabId: TabId): DockLocation | null {
  return findTab(layout, tabId)?.dock ?? null;
}

export function isDockCollapsed(layout: DockLayout, dock: DockLocation): boolean {
  return layout.collapsed[dock] === true;
}

function clone(layout: DockLayout): DockLayout {
  return {
    right: layout.right.map((g) => ({ tabs: [...g.tabs], active: g.active })),
    bottom: layout.bottom.map((g) => ({ tabs: [...g.tabs], active: g.active })),
    nextTerminalIndex: layout.nextTerminalIndex,
    closedPlugins: [...layout.closedPlugins],
    collapsed: { ...layout.collapsed },
  };
}

export function setDockCollapsed(layout: DockLayout, dock: DockLocation, collapsed: boolean): DockLayout {
  if (isDockCollapsed(layout, dock) === collapsed) return layout;
  const next = clone(layout);
  next.collapsed[dock] = collapsed;
  return next;
}

function ensureGroup(layout: DockLayout, dock: DockLocation): PaneGroup {
  if (!layout[dock][0]) layout[dock] = [{ tabs: [], active: null }];
  return layout[dock][0]!;
}

/** Prunes on real tab count, so a group holding only an unloaded plugin tab survives. */
function pruneEmpty(layout: DockLayout, dock: DockLocation): void {
  layout[dock] = layout[dock].filter((g) => g.tabs.length > 0);
}

export function addTab(layout: DockLayout, dock: DockLocation, tabId: TabId, activate = true): DockLayout {
  if (dockOf(layout, tabId)) return layout; // already open somewhere
  const next = clone(layout);
  const group = ensureGroup(next, dock);
  group.tabs.push(tabId);
  if (activate || group.active === null) group.active = tabId;
  next.closedPlugins = next.closedPlugins.filter((id) => id !== tabId);
  return next;
}

export function removeTab(layout: DockLayout, tabId: TabId): DockLayout {
  const at = findTab(layout, tabId);
  if (!at) return layout;
  const next = clone(layout);
  const group = next[at.dock][at.group]!;
  group.tabs.splice(at.index, 1);
  if (group.active === tabId) {
    group.active = group.tabs[at.index] ?? group.tabs[group.tabs.length - 1] ?? null;
  }
  if (tabId.startsWith("plugin:") && !next.closedPlugins.includes(tabId)) {
    next.closedPlugins.push(tabId);
  }
  pruneEmpty(next, at.dock);
  return next;
}

export function setActive(layout: DockLayout, dock: DockLocation, tabId: TabId): DockLayout {
  const gi = layout[dock].findIndex((g) => g.tabs.includes(tabId));
  if (gi < 0 || layout[dock][gi]!.active === tabId) return layout;
  const next = clone(layout);
  next[dock][gi]!.active = tabId;
  return next;
}

function clampIndex(index: number, max: number): number {
  return Math.max(0, Math.min(Math.floor(index), max));
}

/** An existing group (`group` + `index`) or, with `newGroup`, a fresh group spliced in at `group`. */
export interface PlaceTarget {
  dock: DockLocation;
  group: number;
  index?: number;
  newGroup?: boolean;
}

/** Move `tabId` to `target`; `index` is measured after removal from the source. A within-group reorder keeps the active tab; moving into another group activates it there. Never marks a plugin tab as closed. */
export function placeTab(layout: DockLayout, tabId: TabId, target: PlaceTarget): DockLayout {
  const src = findTab(layout, tabId);
  if (!src) return layout;
  const next = clone(layout);
  const srcGroup = next[src.dock][src.group]!;
  const srcActive = srcGroup.active;
  srcGroup.tabs.splice(src.index, 1);
  if (srcActive === tabId) {
    srcGroup.active = srcGroup.tabs[src.index] ?? srcGroup.tabs[srcGroup.tabs.length - 1] ?? null;
  }
  let srcPruned = false;
  if (srcGroup.tabs.length === 0) {
    next[src.dock].splice(src.group, 1);
    srcPruned = true;
  }
  // Removing the source group renumbers later groups in the same dock.
  let groupIdx = target.group;
  if (srcPruned && src.dock === target.dock && src.group < groupIdx) groupIdx--;
  const destGroups = next[target.dock];
  if (target.newGroup) {
    destGroups.splice(clampIndex(groupIdx, destGroups.length), 0, { tabs: [tabId], active: tabId });
  } else {
    const dest = destGroups[groupIdx];
    if (!dest) return layout;
    dest.tabs.splice(clampIndex(target.index ?? dest.tabs.length, dest.tabs.length), 0, tabId);
    const sameGroup = !srcPruned && src.dock === target.dock && src.group === groupIdx;
    dest.active = sameGroup ? srcActive : tabId;
  }
  next.closedPlugins = next.closedPlugins.filter((id) => id !== tabId);
  return next;
}

export function moveTab(layout: DockLayout, tabId: TabId, toDock: DockLocation): DockLayout {
  const from = dockOf(layout, tabId);
  if (!from || from === toDock) return layout;
  const groups = layout[toDock];
  if (groups.length === 0) return placeTab(layout, tabId, { dock: toDock, group: 0, newGroup: true });
  const last = groups.length - 1;
  return placeTab(layout, tabId, { dock: toDock, group: last, index: groups[last]!.tabs.length });
}

export function addTerminal(layout: DockLayout, dock: DockLocation): { layout: DockLayout; tabId: TabId } {
  const tabId = terminalTabId(layout.nextTerminalIndex);
  const next = addTab(layout, dock, tabId);
  next.nextTerminalIndex = layout.nextTerminalIndex + 1;
  return { layout: next, tabId };
}

export function removeAllTerminals(layout: DockLayout): DockLayout {
  let next = layout;
  for (const dock of DOCKS) {
    for (const id of [...dockTabs(layout, dock)]) {
      if (isTerminalTabId(id)) next = removeTab(next, id);
    }
  }
  return next;
}

/** Add available plugin panes that are neither open nor closed, leaving open ones in place. */
export function syncPluginTabs(layout: DockLayout, available: { id: TabId; defaultDock: DockLocation }[]): DockLayout {
  let next = layout;
  for (const p of available) {
    if (dockOf(next, p.id)) continue;
    if (next.closedPlugins.includes(p.id)) continue;
    // Auto-added plugin tabs don't steal focus.
    next = addTab(next, p.defaultDock, p.id, false);
  }
  return next;
}

// Sub agents and Files are opt-in, so they never auto-open as empty tabs.
const AUTO_OPEN_PANES = BUILTIN_PANES.filter((p) => p.id !== "agents" && p.id !== "files");

function defaultTemplate(): DockLayout {
  // Narrow viewports start empty and use the mobile picker.
  const open = typeof window !== "undefined" && window.innerWidth >= 768;
  const base = emptyDockLayout();
  if (!open) return base;
  let l: DockLayout = base;
  for (const p of AUTO_OPEN_PANES) {
    const tabId = p.id === "terminal" ? terminalTabId(0) : p.id;
    l = addTab(l, p.defaultDock, tabId, false);
  }
  return l;
}

function migrateTemplate(): DockLayout {
  const v1 = safeGetItem(LEGACY_V1_KEY);
  if (v1) {
    try {
      const parsed = JSON.parse(v1) as Record<string, unknown>;
      let l = emptyDockLayout();
      for (const p of AUTO_OPEN_PANES) {
        const v = parsed[p.id];
        let open = true;
        let dock: DockLocation = p.defaultDock;
        if (typeof v === "boolean") {
          open = v; // phase-1 bare boolean shape
        } else if (v && typeof v === "object") {
          const s = v as Record<string, unknown>;
          if (typeof s.open === "boolean") open = s.open;
          if (s.dock === "right" || s.dock === "bottom") dock = s.dock;
        }
        if (open) {
          const tabId = p.id === "terminal" ? terminalTabId(0) : p.id;
          l = addTab(l, dock, tabId, false);
        }
      }
      return l;
    } catch {
      // Fall through to the collapsed flag and defaults.
    }
  }
  const collapsed = safeGetItem(LEGACY_COLLAPSED_KEY);
  if (collapsed === "1") return emptyDockLayout();
  if (collapsed === "0") {
    let l = emptyDockLayout();
    for (const p of AUTO_OPEN_PANES) {
      const tabId = p.id === "terminal" ? terminalTabId(0) : p.id;
      l = addTab(l, p.defaultDock, tabId, false);
    }
    return l;
  }
  return defaultTemplate();
}

function normalizeGroups(v: unknown): PaneGroup[] {
  if (!Array.isArray(v)) return [];
  const groups: PaneGroup[] = [];
  for (const g of v) {
    if (!g || typeof g !== "object") continue;
    const o = g as Record<string, unknown>;
    const tabs = Array.isArray(o.tabs) ? o.tabs.filter((t): t is string => typeof t === "string") : [];
    if (tabs.length === 0) continue;
    const active = typeof o.active === "string" && tabs.includes(o.active) ? o.active : tabs[0]!;
    groups.push({ tabs, active });
  }
  return groups;
}

/** A corrupted store with one id in two docks would give dnd-kit duplicate sortable ids. */
function dropDuplicates(group: PaneGroup[], seen: Set<TabId>): PaneGroup[] {
  return group
    .map((g) => {
      const tabs = g.tabs.filter((t) => {
        if (seen.has(t)) return false;
        seen.add(t);
        return true;
      });
      const active = g.active && tabs.includes(g.active) ? g.active : (tabs[0] ?? null);
      return { tabs, active };
    })
    .filter((g) => g.tabs.length > 0);
}

function normalizeDock(v: unknown): DockLayout {
  const o = (v && typeof v === "object" ? v : {}) as Record<string, unknown>;
  const seen = new Set<TabId>();
  const collapsed = (o.collapsed && typeof o.collapsed === "object" ? o.collapsed : {}) as Record<string, unknown>;
  return {
    right: dropDuplicates(normalizeGroups(o.right), seen),
    bottom: dropDuplicates(normalizeGroups(o.bottom), seen),
    nextTerminalIndex:
      typeof o.nextTerminalIndex === "number" && o.nextTerminalIndex >= 1 ? Math.floor(o.nextTerminalIndex) : 1,
    closedPlugins: Array.isArray(o.closedPlugins)
      ? o.closedPlugins.filter((t): t is string => typeof t === "string")
      : [],
    collapsed: {
      right: collapsed.right === true,
      bottom: collapsed.bottom === true,
    },
  };
}

function loadStore(): LayoutStore {
  const raw = safeGetItem(LAYOUT_KEY);
  if (raw) {
    try {
      const parsed = JSON.parse(raw) as Record<string, unknown>;
      if (parsed && parsed.version === 2) {
        const sessionsRaw = (parsed.sessions ?? {}) as Record<string, unknown>;
        const sessions: Record<string, DockLayout> = {};
        for (const [id, layout] of Object.entries(sessionsRaw)) {
          sessions[id] = normalizeDock(layout);
        }
        return { version: 2, template: normalizeDock(parsed.template), sessions };
      }
    } catch {
      // Malformed JSON: fall through to migration and defaults.
    }
  }
  return { version: 2, template: migrateTemplate(), sessions: {} };
}

export interface PaneLayoutApi {
  layout: DockLayout;
  /** No-op if already open anywhere. */
  openTab: (tabId: TabId, dock: DockLocation) => void;
  addTerminal: (dock: DockLocation) => void;
  closeTab: (tabId: TabId) => void;
  activateTab: (dock: DockLocation, tabId: TabId) => void;
  moveTab: (tabId: TabId, toDock: DockLocation) => void;
  placeTab: (tabId: TabId, target: PlaceTarget) => void;
  toggleKind: (kind: "diff" | "terminal" | "agents" | "files", defaultDock: DockLocation) => void;
  togglePlugin: (id: TabId, defaultDock: DockLocation) => void;
  syncPlugins: (available: { id: TabId; defaultDock: DockLocation }[]) => void;
  setDockCollapsed: (dock: DockLocation, collapsed: boolean) => void;
}

function revealDock(layout: DockLayout, dock: DockLocation): DockLayout {
  return setDockCollapsed(layout, dock, false);
}

function openOrRevealTab(layout: DockLayout, tabId: TabId, defaultDock: DockLocation): DockLayout {
  const at = findTab(layout, tabId);
  if (at) return revealDock(setActive(layout, at.dock, tabId), at.dock);
  return revealDock(addTab(layout, defaultDock, tabId), defaultDock);
}

function activateOrRevealTab(layout: DockLayout, dock: DockLocation, tabId: TabId): DockLayout {
  if (!layout[dock].some((g) => g.tabs.includes(tabId))) return layout;
  return revealDock(setActive(layout, dock, tabId), dock);
}

function terminalTabs(layout: DockLayout): { id: TabId; dock: DockLocation }[] {
  return DOCKS.flatMap((dock) =>
    dockTabs(layout, dock)
      .filter(isTerminalTabId)
      .map((id) => ({ id, dock })),
  );
}

export interface AutoOpenPanePrefs {
  diff: boolean;
  terminal: boolean;
}

/** Strip auto-open panes from the template at seed time, so the prefs affect existing users but only new sessions. Purely subtractive. */
export function seedLayout(template: DockLayout, prefs: AutoOpenPanePrefs): DockLayout {
  let l = template;
  if (!prefs.diff) l = removeTab(l, "diff");
  if (!prefs.terminal) l = removeAllTerminals(l);
  return l;
}

export function usePaneLayout(sessionId: string | null): PaneLayoutApi {
  const [store, setStore] = useState(loadStore);
  const { settings } = useWebSettings();
  const { autoOpenDiffPane, autoOpenTerminalPane } = settings;
  useEffect(() => {
    safeSetItem(LAYOUT_KEY, JSON.stringify(store));
  }, [store]);

  const layout = useMemo(
    () =>
      sessionId
        ? (store.sessions[sessionId] ??
          seedLayout(store.template, { diff: autoOpenDiffPane, terminal: autoOpenTerminalPane }))
        : emptyDockLayout(),
    [store, sessionId, autoOpenDiffPane, autoOpenTerminalPane],
  );

  const mutate = useCallback(
    (fn: (l: DockLayout) => DockLayout) => {
      if (!sessionId) return;
      setStore((s) => {
        // Read prefs synchronously: depending on `settings` would rebuild the pane API on any setting change.
        const prefs = getWebSettingsSnapshot();
        const current =
          s.sessions[sessionId] ??
          seedLayout(s.template, { diff: prefs.autoOpenDiffPane, terminal: prefs.autoOpenTerminalPane });
        const updated = fn(current);
        if (updated === current && sessionId in s.sessions) return s;
        return { ...s, sessions: { ...s.sessions, [sessionId]: updated } };
      });
    },
    [sessionId],
  );

  const openTab = useCallback(
    (tabId: TabId, dock: DockLocation) => mutate((l) => openOrRevealTab(l, tabId, dock)),
    [mutate],
  );
  const addTerminalCb = useCallback(
    (dock: DockLocation) => mutate((l) => revealDock(addTerminal(l, dock).layout, dock)),
    [mutate],
  );
  const closeTab = useCallback((tabId: TabId) => mutate((l) => removeTab(l, tabId)), [mutate]);
  const activateTab = useCallback(
    (dock: DockLocation, tabId: TabId) => mutate((l) => activateOrRevealTab(l, dock, tabId)),
    [mutate],
  );
  const moveTabCb = useCallback(
    (tabId: TabId, toDock: DockLocation) =>
      mutate((l) => {
        const next = moveTab(l, tabId, toDock);
        return dockOf(next, tabId) === toDock ? revealDock(next, toDock) : next;
      }),
    [mutate],
  );
  const placeTabCb = useCallback(
    (tabId: TabId, target: PlaceTarget) =>
      mutate((l) => {
        const next = placeTab(l, tabId, target);
        return dockOf(next, tabId) === target.dock ? revealDock(next, target.dock) : next;
      }),
    [mutate],
  );
  const toggleKind = useCallback(
    (kind: "diff" | "terminal" | "agents" | "files", defaultDock: DockLocation) =>
      mutate((l) => {
        // Single-instance panes toggle their tab; terminals toggle the whole group.
        if (kind === "diff" || kind === "agents" || kind === "files") {
          const at = findTab(l, kind);
          if (at) return isDockCollapsed(l, at.dock) ? openOrRevealTab(l, kind, defaultDock) : removeTab(l, kind);
          return openOrRevealTab(l, kind, defaultDock);
        }
        const terminals = terminalTabs(l);
        if (terminals.length === 0) return openOrRevealTab(l, terminalTabId(0), defaultDock);
        const visibleTerminal = terminals.find(({ dock }) => !isDockCollapsed(l, dock));
        if (visibleTerminal) return removeAllTerminals(l);
        const first = terminals[0]!;
        return activateOrRevealTab(l, first.dock, first.id);
      }),
    [mutate],
  );
  const togglePlugin = useCallback(
    (id: TabId, defaultDock: DockLocation) =>
      mutate((l) => {
        const at = findTab(l, id);
        if (at) return isDockCollapsed(l, at.dock) ? openOrRevealTab(l, id, defaultDock) : removeTab(l, id);
        return openOrRevealTab(l, id, defaultDock);
      }),
    [mutate],
  );
  const syncPlugins = useCallback(
    (available: { id: TabId; defaultDock: DockLocation }[]) => mutate((l) => syncPluginTabs(l, available)),
    [mutate],
  );
  const setDockCollapsedCb = useCallback(
    (dock: DockLocation, collapsed: boolean) => mutate((l) => setDockCollapsed(l, dock, collapsed)),
    [mutate],
  );

  return {
    layout,
    openTab,
    addTerminal: addTerminalCb,
    closeTab,
    activateTab,
    moveTab: moveTabCb,
    placeTab: placeTabCb,
    toggleKind,
    togglePlugin,
    syncPlugins,
    setDockCollapsed: setDockCollapsedCb,
  };
}
