import { useCallback, useMemo, useSyncExternalStore } from "react";

import { confirmFileEditPanes } from "./fileEditGuard";
import { useLatestRef } from "../hooks/useLatestRef";
import { safeGetItem, safeRemoveItem, safeSetItem } from "./safeStorage";
import { BUILTIN_PANES, isTerminalTabId, terminalTabId, type DockLocation } from "./panes";

const LAYOUT_KEY = "aoe-pane-layout-v3";
const V2_KEY = "aoe-pane-layout-v2";
// Earlier layout stores, deleted once v3 is written to free quota.
const REPLACED_KEYS = [V2_KEY, "aoe-pane-layout", "aoe-right-collapsed"];

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

/** Only Sessions whose layout the user changed; the rest follow {@link seedLayout}. */
interface LayoutStore {
  version: 3;
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

/** A dock's groups reduced to what's actually shown: each surviving group keeps its persisted index (so a drop
 *  addresses the right group) and a valid active tab. Groups with no available tab are not rendered. */
export function renderedGroups(
  layout: DockLayout,
  dock: DockLocation,
  tabAvailable: (id: TabId) => boolean,
): { group: number; tabs: TabId[]; active: TabId | null }[] {
  return layout[dock]
    .map((g, group) => {
      const tabs = g.tabs.filter(tabAvailable);
      const active = g.active && tabs.includes(g.active) ? g.active : (tabs[0] ?? null);
      return { group, tabs, active };
    })
    .filter((g) => g.tabs.length > 0);
}

/** Tabs shown before a change that are hidden or remounted after it: Dock instances are keyed by dock and group. */
function leftTabs(before: DockLayout, after: DockLayout, tabAvailable: (id: TabId) => boolean): TabId[] {
  const shown = (layout: DockLayout) =>
    DOCKS.flatMap((dock) =>
      isDockCollapsed(layout, dock)
        ? []
        : renderedGroups(layout, dock, tabAvailable).map((g) => ({ id: g.active, at: `${dock}:${g.group}` })),
    );
  const next = new Set(shown(after).map(({ id, at }) => `${at}:${id}`));
  return shown(before).flatMap(({ id, at }) => (id && !next.has(`${at}:${id}`) ? [id] : []));
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
export function syncPluginTabs(
  layout: DockLayout,
  available: readonly { id: TabId; defaultDock: DockLocation }[],
): DockLayout {
  let next = layout;
  for (const p of available) {
    if (dockOf(next, p.id)) continue;
    if (next.closedPlugins.includes(p.id)) continue;
    // Auto-added plugin tabs don't steal focus.
    next = addTab(next, p.defaultDock, p.id, false);
  }
  return next;
}

export interface SeedPrefs {
  diff: boolean;
  terminal: boolean;
}

/** What a Session shows until the user changes its layout. */
export function seedLayout(prefs: SeedPrefs): DockLayout {
  let l = emptyDockLayout();
  for (const p of BUILTIN_PANES) {
    if (p.id === "diff" && prefs.diff) l = addTab(l, p.defaultDock, "diff", false);
    if (p.id === "terminal" && prefs.terminal) l = addTab(l, p.defaultDock, terminalTabId(0), false);
  }
  return l;
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

function parseSessions(raw: string | null, version: 2 | 3): Record<string, DockLayout> {
  try {
    const parsed = JSON.parse(raw ?? "null") as Record<string, unknown> | null;
    if (parsed?.version !== version) return {};
    const sessions: Record<string, DockLayout> = {};
    for (const [id, layout] of Object.entries((parsed.sessions ?? {}) as Record<string, unknown>)) {
      sessions[id] = normalizeDock(layout);
    }
    return sessions;
  } catch {
    return {};
  }
}

/** v3 if written, else the v2 layouts whose added terminals may still be live shells: keeping their tab ids and
 *  next index stops a new tab from reusing one. Other v2 layouts were seeds or simple toggles and are dropped. */
function loadStore(): LayoutStore {
  const raw = safeGetItem(LAYOUT_KEY);
  if (raw !== null) return { version: 3, sessions: parseSessions(raw, 3) };
  const v2 = Object.entries(parseSessions(safeGetItem(V2_KEY), 2)).filter(([, l]) => l.nextTerminalIndex > 1);
  return { version: 3, sessions: Object.fromEntries(v2) };
}

const listeners = new Set<() => void>();
let cachedSource: string | undefined;
let cachedStore: LayoutStore = { version: 3, sessions: {} };

function storedSource(): string {
  const raw = safeGetItem(LAYOUT_KEY);
  return raw !== null ? `3:${raw}` : `2:${safeGetItem(V2_KEY) ?? ""}`;
}

function readStore(): LayoutStore {
  const source = storedSource();
  if (source !== cachedSource) {
    cachedSource = source;
    cachedStore = loadStore();
  }
  return cachedStore;
}

function updateStore(fn: (s: LayoutStore) => LayoutStore): void {
  const current = readStore();
  const next = fn(current);
  if (next === current) return;
  // A failed write keeps the change in memory rather than re-reading the stale copy.
  safeSetItem(LAYOUT_KEY, JSON.stringify(next));
  cachedStore = next;
  cachedSource = storedSource();
  for (const l of listeners) l();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => void listeners.delete(listener);
}

/** Drops saved layouts of Sessions not in `live`, writes v3 if it was read from v2, then deletes the old stores. */
export function sweepOrphanPaneLayouts(live: ReadonlySet<string>): void {
  const upgrading = safeGetItem(LAYOUT_KEY) === null;
  updateStore((s) => {
    const kept = Object.entries(s.sessions).filter(([id]) => live.has(id));
    const unchanged = kept.length === Object.keys(s.sessions).length;
    return unchanged && !upgrading ? s : { version: 3, sessions: Object.fromEntries(kept) };
  });
  if (safeGetItem(LAYOUT_KEY) !== null) for (const key of REPLACED_KEYS) safeRemoveItem(key);
}

export interface PaneLayoutApi {
  layout: DockLayout;
  /** No-op if already open anywhere. */
  openTab: (tabId: TabId, dock: DockLocation) => void;
  addTerminal: (dock: DockLocation) => void;
  /** Asks first, so an irreversible `before` (a shell kill) never runs for a declined close; closes once it resolves true. */
  closeTab: (tabId: TabId, before?: () => Promise<boolean>) => void;
  activateTab: (dock: DockLocation, tabId: TabId) => void;
  moveTab: (tabId: TabId, toDock: DockLocation) => void;
  placeTab: (tabId: TabId, target: PlaceTarget) => void;
  toggleKind: (kind: "diff" | "terminal" | "agents" | "files", defaultDock: DockLocation) => void;
  togglePlugin: (id: TabId, defaultDock: DockLocation) => void;
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

export type AutoPane = { id: TabId; defaultDock: DockLocation };

/**
 * Shows the saved layout, or the seed, plus `autoPlugins` the user has not closed. Only a user change saves,
 * so the defaults keep reaching every other Session. `outsideRepository` drops Diff from the seed; `null`
 * (not yet known) seeds no Diff.
 */
export function usePaneLayout(
  sessionId: string | null,
  outsideRepository: boolean | null,
  prefs: SeedPrefs,
  autoPlugins: readonly AutoPane[],
  tabAvailable: (id: TabId) => boolean = () => true,
): PaneLayoutApi {
  const store = useSyncExternalStore(subscribe, readStore);
  const seedDiff = prefs.diff && outsideRepository === false;
  const derive = useCallback(
    (saved: DockLayout | undefined) =>
      syncPluginTabs(saved ?? seedLayout({ diff: seedDiff, terminal: prefs.terminal }), autoPlugins),
    [seedDiff, prefs.terminal, autoPlugins],
  );
  const layout = useMemo(
    () => (sessionId ? derive(store.sessions[sessionId]) : emptyDockLayout()),
    [store, sessionId, derive],
  );

  const shownRef = useLatestRef({ layout, tabAvailable });
  const deriveRef = useLatestRef(derive);
  const mutate = useCallback(
    (fn: (l: DockLayout) => DockLayout, derive = deriveRef.current) => {
      if (!sessionId) return;
      updateStore((s) => {
        const current = derive(s.sessions[sessionId]);
        const updated = fn(current);
        if (updated === current) return s;
        return { version: 3, sessions: { ...s.sessions, [sessionId]: updated } };
      });
    },
    [sessionId, deriveRef],
  );

  const confirmLeaving = useCallback(
    (fn: (l: DockLayout) => DockLayout) => {
      const { layout: current, tabAvailable: available } = shownRef.current;
      return confirmFileEditPanes(leftTabs(current, fn(current), available));
    },
    [shownRef],
  );
  const guardedMutate = useCallback(
    (fn: (l: DockLayout) => DockLayout) => {
      if (confirmLeaving(fn)) mutate(fn);
    },
    [mutate, confirmLeaving],
  );
  const openTab = useCallback(
    (tabId: TabId, dock: DockLocation) => guardedMutate((l) => openOrRevealTab(l, tabId, dock)),
    [guardedMutate],
  );
  const addTerminalCb = useCallback(
    (dock: DockLocation) => guardedMutate((l) => revealDock(addTerminal(l, dock).layout, dock)),
    [guardedMutate],
  );
  const closeTab = useCallback(
    (tabId: TabId, before?: () => Promise<boolean>) => {
      const close = (l: DockLayout) => removeTab(l, tabId);
      if (!before) return guardedMutate(close);
      if (!confirmLeaving(close)) return;
      // A switch during the kill must not derive another Session's defaults into this one.
      const derive = deriveRef.current;
      void before().then((ok) => {
        if (ok) mutate(close, derive);
      });
    },
    [mutate, guardedMutate, confirmLeaving, deriveRef],
  );
  const activateTab = useCallback(
    (dock: DockLocation, tabId: TabId) => guardedMutate((l) => activateOrRevealTab(l, dock, tabId)),
    [guardedMutate],
  );
  const moveTabCb = useCallback(
    (tabId: TabId, toDock: DockLocation) =>
      guardedMutate((l) => {
        const next = moveTab(l, tabId, toDock);
        return dockOf(next, tabId) === toDock ? revealDock(next, toDock) : next;
      }),
    [guardedMutate],
  );
  const placeTabCb = useCallback(
    (tabId: TabId, target: PlaceTarget) =>
      guardedMutate((l) => {
        const next = placeTab(l, tabId, target);
        return dockOf(next, tabId) === target.dock ? revealDock(next, target.dock) : next;
      }),
    [guardedMutate],
  );
  const toggleKind = useCallback(
    (kind: "diff" | "terminal" | "agents" | "files", defaultDock: DockLocation) =>
      guardedMutate((l) => {
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
    [guardedMutate],
  );
  const togglePlugin = useCallback(
    (id: TabId, defaultDock: DockLocation) =>
      guardedMutate((l) => {
        const at = findTab(l, id);
        if (at) return isDockCollapsed(l, at.dock) ? openOrRevealTab(l, id, defaultDock) : removeTab(l, id);
        return openOrRevealTab(l, id, defaultDock);
      }),
    [guardedMutate],
  );
  const setDockCollapsedCb = useCallback(
    (dock: DockLocation, collapsed: boolean) => guardedMutate((l) => setDockCollapsed(l, dock, collapsed)),
    [guardedMutate],
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
    setDockCollapsed: setDockCollapsedCb,
  };
}
