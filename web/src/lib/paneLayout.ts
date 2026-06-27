import { useCallback, useEffect, useMemo, useState } from "react";

import { safeGetItem, safeSetItem } from "./safeStorage";
import { BUILTIN_PANES, isTerminalTabId, terminalTabId, type DockLocation } from "./panes";

const LAYOUT_KEY = "aoe-pane-layout-v2";
// v1 (#2432): a flat `Record<paneId, {open, dock}>`, browser-global. Read once
// to seed the v2 per-session template so an upgrading user keeps their open
// panes, then superseded by LAYOUT_KEY.
const LEGACY_V1_KEY = "aoe-pane-layout";
// The pre-pane single right-column collapse flag (#2405 and earlier).
const LEGACY_COLLAPSED_KEY = "aoe-right-collapsed";

/** A tab id: `"diff"`, `"terminal:<n>"`, or `"plugin:<plugin>:<entry>"`. */
export type TabId = string;

/** An ordered set of tabs sharing one strip, with the active one shown. */
export interface PaneGroup {
  tabs: TabId[];
  active: TabId | null;
}

/** The tab layout for one session.
 *
 *  Each dock holds an array of groups, but only one group per dock is rendered
 *  today (a single tab strip). The array shape is deliberate: the drag-and-drop
 *  follow-up (#2437's sibling) adds split groups within a dock without another
 *  storage migration. */
export interface DockLayout {
  right: PaneGroup[];
  bottom: PaneGroup[];
  // Monotonic terminal-index allocator. Never reused, so a freshly opened tab
  // can't alias a just-closed terminal's tmux session.
  nextTerminalIndex: number;
  // Plugin tabs the user explicitly closed, so the auto-add pass does not
  // immediately re-add them on the next render.
  closedPlugins: TabId[];
}

interface LayoutStore {
  version: 2;
  template: DockLayout;
  sessions: Record<string, DockLayout>;
}

const DOCKS: DockLocation[] = ["right", "bottom"];

function emptyDockLayout(): DockLayout {
  return { right: [], bottom: [], nextTerminalIndex: 1, closedPlugins: [] };
}

/** First (and currently only) group of a dock, or undefined when empty. */
function firstGroup(layout: DockLayout, dock: DockLocation): PaneGroup | undefined {
  return layout[dock][0];
}

export function dockTabs(layout: DockLayout, dock: DockLocation): TabId[] {
  return firstGroup(layout, dock)?.tabs ?? [];
}

export function dockActive(layout: DockLayout, dock: DockLocation): TabId | null {
  return firstGroup(layout, dock)?.active ?? null;
}

/** Which dock a tab currently lives in, or null if it is closed. */
export function dockOf(layout: DockLayout, tabId: TabId): DockLocation | null {
  for (const dock of DOCKS) {
    if (dockTabs(layout, dock).includes(tabId)) return dock;
  }
  return null;
}

function clone(layout: DockLayout): DockLayout {
  return {
    right: layout.right.map((g) => ({ tabs: [...g.tabs], active: g.active })),
    bottom: layout.bottom.map((g) => ({ tabs: [...g.tabs], active: g.active })),
    nextTerminalIndex: layout.nextTerminalIndex,
    closedPlugins: [...layout.closedPlugins],
  };
}

function ensureGroup(layout: DockLayout, dock: DockLocation): PaneGroup {
  if (!layout[dock][0]) layout[dock] = [{ tabs: [], active: null }];
  return layout[dock][0]!;
}

/** Drop the dock's group entirely once its last tab closes, so the parent can
 *  hide an empty dock (it keys off a zero-length groups array). */
function pruneEmpty(layout: DockLayout, dock: DockLocation): void {
  const g = layout[dock][0];
  if (g && g.tabs.length === 0) layout[dock] = [];
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
  const dock = dockOf(layout, tabId);
  if (!dock) return layout;
  const next = clone(layout);
  const group = next[dock][0]!;
  const idx = group.tabs.indexOf(tabId);
  group.tabs.splice(idx, 1);
  if (group.active === tabId) {
    // Prefer the tab that shifted into this slot, else the new last tab.
    group.active = group.tabs[idx] ?? group.tabs[group.tabs.length - 1] ?? null;
  }
  if (tabId.startsWith("plugin:") && !next.closedPlugins.includes(tabId)) {
    next.closedPlugins.push(tabId);
  }
  pruneEmpty(next, dock);
  return next;
}

export function setActive(layout: DockLayout, dock: DockLocation, tabId: TabId): DockLayout {
  const group = firstGroup(layout, dock);
  if (!group || !group.tabs.includes(tabId) || group.active === tabId) return layout;
  const next = clone(layout);
  next[dock][0]!.active = tabId;
  return next;
}

function clampIndex(index: number, max: number): number {
  return Math.max(0, Math.min(Math.floor(index), max));
}

/** Move `tabId` to position `toIndex` in `toDock`. Subsumes both within-dock
 *  reorder (toDock equals the source dock) and cross-dock move. `toIndex` is the
 *  index in the destination group *after* the tab is removed from its source.
 *
 *  Active-tab rule: a within-dock reorder keeps whatever was active (including
 *  the dragged tab itself), so dragging a background tab never steals focus; a
 *  cross-dock move activates the tab in its destination, since it would
 *  otherwise land hidden behind the destination's active tab. Implemented
 *  directly rather than via removeTab so a move never marks a plugin tab as
 *  explicitly closed. */
export function placeTab(layout: DockLayout, tabId: TabId, toDock: DockLocation, toIndex: number): DockLayout {
  const fromDock = dockOf(layout, tabId);
  if (!fromDock) return layout;
  const next = clone(layout);
  const source = next[fromDock][0]!;
  const fromIndex = source.tabs.indexOf(tabId);
  if (fromIndex < 0) return layout;
  const wasActive = source.active === tabId;
  source.tabs.splice(fromIndex, 1);
  if (wasActive) {
    // Source loses its active tab: prefer the tab that shifted into the slot,
    // else the new last tab. (Overridden below for a within-dock reorder.)
    source.active = source.tabs[fromIndex] ?? source.tabs[source.tabs.length - 1] ?? null;
  }
  pruneEmpty(next, fromDock);
  const dest = ensureGroup(next, toDock);
  dest.tabs.splice(clampIndex(toIndex, dest.tabs.length), 0, tabId);
  if (fromDock !== toDock || wasActive || dest.active === null) dest.active = tabId;
  next.closedPlugins = next.closedPlugins.filter((id) => id !== tabId);
  return next;
}

export function moveTab(layout: DockLayout, tabId: TabId, toDock: DockLocation): DockLayout {
  const from = dockOf(layout, tabId);
  if (!from || from === toDock) return layout;
  return placeTab(layout, tabId, toDock, dockTabs(layout, toDock).length);
}

/** Allocate a fresh terminal tab in `dock` and return its id + new layout. */
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

/** Add any available plugin pane that is neither open nor explicitly closed to
 *  its default dock, in the order given. Keeps already-open plugins and their
 *  position untouched. */
export function syncPluginTabs(layout: DockLayout, available: { id: TabId; defaultDock: DockLocation }[]): DockLayout {
  let next = layout;
  for (const p of available) {
    if (dockOf(next, p.id)) continue;
    if (next.closedPlugins.includes(p.id)) continue;
    // Auto-added plugin tabs don't steal focus from the active tab.
    next = addTab(next, p.defaultDock, p.id, false);
  }
  return next;
}

// --- persistence + migration ---

// Built-in panes that open by default / on a v1 migration. The Sub agents
// pane is intentionally excluded: it is opt-in, opened on demand via its
// ActivityBar toggle or by clicking an inline sub-agent card, so it never
// auto-opens as an empty tab. A v1 layout predates it, so it can never
// have been "open" there either.
const AUTO_OPEN_PANES = BUILTIN_PANES.filter((p) => p.id !== "agents");

function defaultTemplate(): DockLayout {
  // Desktop opens diff + terminal in the right dock (matches the historical
  // expanded right column); narrow viewports start empty and drive the surface
  // via the mobile picker instead.
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
      // fall through to collapsed flag / defaults
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
  // Collapse to a single group for now; DnD will lift this cap.
  if (groups.length <= 1) return groups;
  const merged: PaneGroup = { tabs: groups.flatMap((g) => g.tabs), active: groups[0]!.active };
  return [merged];
}

/** Drop from `group` any tab id already claimed by an earlier dock. A tab must
 *  live in exactly one dock; a corrupted store with the same id in both would
 *  hand dnd-kit duplicate sortable ids and break dragging. */
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
  return {
    right: dropDuplicates(normalizeGroups(o.right), seen),
    bottom: dropDuplicates(normalizeGroups(o.bottom), seen),
    nextTerminalIndex:
      typeof o.nextTerminalIndex === "number" && o.nextTerminalIndex >= 1 ? Math.floor(o.nextTerminalIndex) : 1,
    closedPlugins: Array.isArray(o.closedPlugins)
      ? o.closedPlugins.filter((t): t is string => typeof t === "string")
      : [],
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
      // Malformed JSON: fall through to migration / defaults.
    }
  }
  return { version: 2, template: migrateTemplate(), sessions: {} };
}

export interface PaneLayoutApi {
  /** The active session's layout (the template if the session is unseen). */
  layout: DockLayout;
  /** Open a specific tab id in `dock` (no-op if already open anywhere). */
  openTab: (tabId: TabId, dock: DockLocation) => void;
  addTerminal: (dock: DockLocation) => void;
  closeTab: (tabId: TabId) => void;
  activateTab: (dock: DockLocation, tabId: TabId) => void;
  moveTab: (tabId: TabId, toDock: DockLocation) => void;
  /** Reorder within a dock or move across docks, landing at `toIndex`. */
  placeTab: (tabId: TabId, toDock: DockLocation, toIndex: number) => void;
  /** Activity-bar toggle for a built-in kind ("diff" or "terminal"). */
  toggleKind: (kind: "diff" | "terminal" | "agents", defaultDock: DockLocation) => void;
  /** Add/remove a plugin pane tab (activity-bar toggle). */
  togglePlugin: (id: TabId, defaultDock: DockLocation) => void;
  syncPlugins: (available: { id: TabId; defaultDock: DockLocation }[]) => void;
}

export function usePaneLayout(sessionId: string | null): PaneLayoutApi {
  const [store, setStore] = useState(loadStore);
  useEffect(() => {
    safeSetItem(LAYOUT_KEY, JSON.stringify(store));
  }, [store]);

  const layout = useMemo(
    () => (sessionId ? (store.sessions[sessionId] ?? store.template) : emptyDockLayout()),
    [store, sessionId],
  );

  // Apply a pure transform to the active session's layout, seeding it from the
  // template the first time the session is touched.
  const mutate = useCallback(
    (fn: (l: DockLayout) => DockLayout) => {
      if (!sessionId) return;
      setStore((s) => {
        const current = s.sessions[sessionId] ?? s.template;
        const updated = fn(current);
        if (updated === current && sessionId in s.sessions) return s;
        return { ...s, sessions: { ...s.sessions, [sessionId]: updated } };
      });
    },
    [sessionId],
  );

  const openTab = useCallback((tabId: TabId, dock: DockLocation) => mutate((l) => addTab(l, dock, tabId)), [mutate]);
  const addTerminalCb = useCallback((dock: DockLocation) => mutate((l) => addTerminal(l, dock).layout), [mutate]);
  const closeTab = useCallback((tabId: TabId) => mutate((l) => removeTab(l, tabId)), [mutate]);
  const activateTab = useCallback(
    (dock: DockLocation, tabId: TabId) => mutate((l) => setActive(l, dock, tabId)),
    [mutate],
  );
  const moveTabCb = useCallback(
    (tabId: TabId, toDock: DockLocation) => mutate((l) => moveTab(l, tabId, toDock)),
    [mutate],
  );
  const placeTabCb = useCallback(
    (tabId: TabId, toDock: DockLocation, toIndex: number) => mutate((l) => placeTab(l, tabId, toDock, toIndex)),
    [mutate],
  );
  const toggleKind = useCallback(
    (kind: "diff" | "terminal" | "agents", defaultDock: DockLocation) =>
      mutate((l) => {
        // Single-instance panes (diff, agents) toggle their one tab; the
        // terminal kind is multi-instance and toggles the whole group.
        if (kind === "diff" || kind === "agents") {
          return dockOf(l, kind) ? removeTab(l, kind) : addTab(l, defaultDock, kind);
        }
        const hasTerminal = DOCKS.some((d) => dockTabs(l, d).some(isTerminalTabId));
        return hasTerminal ? removeAllTerminals(l) : addTab(l, defaultDock, terminalTabId(0));
      }),
    [mutate],
  );
  const togglePlugin = useCallback(
    (id: TabId, defaultDock: DockLocation) =>
      mutate((l) => (dockOf(l, id) ? removeTab(l, id) : addTab(l, defaultDock, id))),
    [mutate],
  );
  const syncPlugins = useCallback(
    (available: { id: TabId; defaultDock: DockLocation }[]) => mutate((l) => syncPluginTabs(l, available)),
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
  };
}
