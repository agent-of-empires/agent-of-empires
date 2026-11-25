import { useCallback, useMemo, useSyncExternalStore } from "react";

import { confirmFileEditPanes } from "./fileEditGuard";
import { useLatestRef } from "../hooks/useLatestRef";
import { safeGetItem, safeRemoveItem, safeSetItem } from "./safeStorage";
import {
  BUILTIN_PANES,
  bindUnboundTerminalTabId,
  isTerminalTabId,
  paneRank,
  parseTerminalTabId,
  terminalTabId,
  TERMINAL_DOCK,
  type DockLocation,
} from "./panes";

const LAYOUT_KEY = "aoe-pane-layout-v4";
const V3_KEY = "aoe-pane-layout-v3";
const V2_KEY = "aoe-pane-layout-v2";
// Earlier layout stores, deleted once v4 is written to free quota.
const REPLACED_KEYS = [V3_KEY, V2_KEY, "aoe-pane-layout", "aoe-right-collapsed"];

/** `"diff"`, `"terminal:<sessionId>:<n>"`, or `"plugin:<plugin>:<entry>"`. */
export type TabId = string;

export interface PaneGroup {
  tabs: TabId[];
  active: TabId | null;
}

/** One session's tab layout. Its terminal tabs follow {@link LayoutStore.terminals}. Only one group per dock renders today; the array leaves room for split groups without a storage migration. */
export interface DockLayout {
  right: PaneGroup[];
  bottom: PaneGroup[];
  // This Session's next shell index, never reused, so a new tab can't alias a just-closed terminal's tmux session.
  nextTerminalIndex: number;
  // Plugin tabs the user closed, so auto-add does not reopen them.
  closedPlugins: TabId[];
  // Collapsed docks keep their tabs.
  collapsed: Record<DockLocation, boolean>;
}

export interface TerminalPlacement {
  id: TabId;
  dock: DockLocation;
}

/** Only Sessions whose layout the user changed; the rest follow {@link seedLayout}. */
interface LayoutStore {
  version: 4;
  sessions: Record<string, DockLayout>;
  /** Terminal tabs every Session shows, as the last changed layout placed them; null until a change after an
   *  upgrade, so each Session keeps the terminals it had. */
  terminals: TerminalPlacement[] | null;
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
  const rank = paneRank(tabId);
  const at = group.tabs.findIndex((t) => paneRank(t) > rank);
  group.tabs.splice(at < 0 ? group.tabs.length : at, 0, tabId);
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

/** Opens a new shell of `sessionId`, the layout's own Session, above `minIndex` (shells already live). */
export function addTerminal(
  layout: DockLayout,
  dock: DockLocation,
  sessionId: string,
  minIndex = 1,
): { layout: DockLayout; tabId: TabId } {
  const index = Math.max(layout.nextTerminalIndex, minIndex);
  const tabId = terminalTabId(sessionId, index);
  const next = addTab(layout, dock, tabId);
  if (next === layout) return { layout, tabId };
  next.nextTerminalIndex = index + 1;
  return { layout: next, tabId };
}

export function terminalPlacements(layout: DockLayout): TerminalPlacement[] {
  return DOCKS.flatMap((dock) =>
    dockTabs(layout, dock)
      .filter(isTerminalTabId)
      .map((id) => ({ id, dock })),
  );
}

/** `layout` showing the open terminal tabs: others close, missing ones join their dock without taking focus. */
export function withTerminals(layout: DockLayout, terminals: readonly TerminalPlacement[]): DockLayout {
  const open = new Set(terminals.map((t) => t.id));
  let next = layout;
  for (const { id } of terminalPlacements(layout)) {
    if (!open.has(id)) next = removeTab(next, id);
  }
  for (const t of terminals) {
    if (!dockOf(next, t.id)) next = addTab(next, t.dock, t.id, false);
  }
  return next;
}

/** The terminal tabs bound to `sessionId`'s shells. */
export function sessionTerminals(layout: DockLayout, sessionId: string): TerminalPlacement[] {
  return terminalPlacements(layout).filter((t) => parseTerminalTabId(t.id)?.sessionId === sessionId);
}

export function removeSessionTerminals(layout: DockLayout, sessionId: string): DockLayout {
  return sessionTerminals(layout, sessionId).reduce((l, t) => removeTab(l, t.id), layout);
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

/** What `sessionId` shows until the user changes its layout. */
export function seedLayout(prefs: SeedPrefs, sessionId: string): DockLayout {
  let l = emptyDockLayout();
  for (const p of BUILTIN_PANES) {
    if (p.id === "diff" && prefs.diff) l = addTab(l, p.defaultDock, "diff", false);
    if (p.id === "terminal" && prefs.terminal) l = addTab(l, p.defaultDock, terminalTabId(sessionId, 0), false);
  }
  return l;
}

/** The layout `sessionId` shows: its saved one or seed, with the terminal tabs open across Sessions. The seeded
 *  terminal shows only while no other terminal is open, so the default never stacks onto a kept tab. */
export function deriveLayout(
  sessionId: string,
  saved: DockLayout | undefined,
  terminals: readonly TerminalPlacement[] | null,
  prefs: SeedPrefs,
): DockLayout {
  if (saved) return terminals ? withTerminals(saved, terminals) : saved;
  if (!terminals) return seedLayout(prefs, sessionId);
  const seeded = withTerminals(seedLayout({ ...prefs, terminal: false }, sessionId), terminals);
  if (!prefs.terminal || terminals.length > 0) return seeded;
  return addTab(seeded, TERMINAL_DOCK, terminalTabId(sessionId, 0), false);
}

function normalizeGroups(v: unknown, sessionId: string): PaneGroup[] {
  if (!Array.isArray(v)) return [];
  const groups: PaneGroup[] = [];
  for (const g of v) {
    if (!g || typeof g !== "object") continue;
    const o = g as Record<string, unknown>;
    const tabs = Array.isArray(o.tabs)
      ? o.tabs.filter((t): t is string => typeof t === "string").map((t) => bindUnboundTerminalTabId(t, sessionId))
      : [];
    if (tabs.length === 0) continue;
    const active =
      typeof o.active === "string" && tabs.includes(bindUnboundTerminalTabId(o.active, sessionId))
        ? bindUnboundTerminalTabId(o.active, sessionId)
        : tabs[0]!;
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

/** Binds a pre-v4 layout's `terminal:<n>` tabs to `sessionId`, whose shells they showed. */
function normalizeDock(v: unknown, sessionId: string): DockLayout {
  const o = (v && typeof v === "object" ? v : {}) as Record<string, unknown>;
  const seen = new Set<TabId>();
  const collapsed = (o.collapsed && typeof o.collapsed === "object" ? o.collapsed : {}) as Record<string, unknown>;
  return {
    right: dropDuplicates(normalizeGroups(o.right, sessionId), seen),
    bottom: dropDuplicates(normalizeGroups(o.bottom, sessionId), seen),
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

function parseJson(raw: string | null, version: 2 | 3 | 4): Record<string, unknown> | null {
  try {
    const parsed = JSON.parse(raw ?? "null") as Record<string, unknown> | null;
    return parsed?.version === version ? parsed : null;
  } catch {
    return null;
  }
}

function parseSessions(parsed: Record<string, unknown> | null): Record<string, DockLayout> {
  const sessions: Record<string, DockLayout> = {};
  for (const [id, layout] of Object.entries((parsed?.sessions ?? {}) as Record<string, unknown>)) {
    sessions[id] = normalizeDock(layout, id);
  }
  return sessions;
}

function parseTerminals(v: unknown): TerminalPlacement[] | null {
  if (!Array.isArray(v)) return null;
  const seen = new Set<TabId>();
  return v.flatMap((t) => {
    const o = (t && typeof t === "object" ? t : {}) as Record<string, unknown>;
    if (typeof o.id !== "string" || !isTerminalTabId(o.id) || seen.has(o.id)) return [];
    if (o.dock !== "right" && o.dock !== "bottom") return [];
    seen.add(o.id);
    return [{ id: o.id, dock: o.dock }];
  });
}

/** v4 if written, else v3, else the v2 layouts whose added terminals may still be live shells: keeping their tab
 *  ids and next index stops a new tab from reusing one. Other v2 layouts were seeds or simple toggles and are
 *  dropped. */
function loadStore(): LayoutStore {
  const raw = safeGetItem(LAYOUT_KEY);
  if (raw !== null) {
    const parsed = parseJson(raw, 4);
    return { version: 4, sessions: parseSessions(parsed), terminals: parseTerminals(parsed?.terminals) };
  }
  const v3 = safeGetItem(V3_KEY);
  if (v3 !== null) return { version: 4, sessions: parseSessions(parseJson(v3, 3)), terminals: null };
  const v2 = Object.entries(parseSessions(parseJson(safeGetItem(V2_KEY), 2))).filter(
    ([, l]) => l.nextTerminalIndex > 1,
  );
  return { version: 4, sessions: Object.fromEntries(v2), terminals: null };
}

const listeners = new Set<() => void>();
let cachedSource: string | undefined;
let cachedStore: LayoutStore = { version: 4, sessions: {}, terminals: null };

function storedSource(): string {
  const raw = safeGetItem(LAYOUT_KEY);
  if (raw !== null) return `4:${raw}`;
  const v3 = safeGetItem(V3_KEY);
  return v3 !== null ? `3:${v3}` : `2:${safeGetItem(V2_KEY) ?? ""}`;
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

function ownerLive(live: ReadonlySet<string>) {
  return (id: TabId) => {
    const ref = parseTerminalTabId(id);
    return !ref || live.has(ref.sessionId);
  };
}

/** Drops saved layouts of Sessions not in `live` and terminal tabs of their shells, writes v4 if it was read from
 *  an older store, then deletes the old stores. */
export function sweepOrphanPaneLayouts(live: ReadonlySet<string>): void {
  const upgrading = safeGetItem(LAYOUT_KEY) === null;
  const kept = ownerLive(live);
  updateStore((s) => {
    let changed = upgrading;
    const sessions: Record<string, DockLayout> = {};
    for (const [id, layout] of Object.entries(s.sessions)) {
      if (!live.has(id)) {
        changed = true;
        continue;
      }
      const orphans = terminalPlacements(layout).filter((t) => !kept(t.id));
      sessions[id] = orphans.reduce((l, t) => removeTab(l, t.id), layout);
      changed ||= orphans.length > 0;
    }
    const terminals = s.terminals?.filter((t) => kept(t.id)) ?? null;
    changed ||= (terminals?.length ?? 0) !== (s.terminals?.length ?? 0);
    return changed ? { version: 4, sessions, terminals } : s;
  });
  if (safeGetItem(LAYOUT_KEY) !== null) for (const key of REPLACED_KEYS) safeRemoveItem(key);
}

export interface PaneLayoutApi {
  layout: DockLayout;
  /** No-op if already open anywhere. */
  openTab: (tabId: TabId, dock: DockLocation) => void;
  /** A new shell of the layout's Session, above `minIndex`. */
  addTerminal: (dock: DockLocation, minIndex?: number) => void;
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
    (s: LayoutStore) =>
      sessionId
        ? syncPluginTabs(
            deriveLayout(sessionId, s.sessions[sessionId], s.terminals, { diff: seedDiff, terminal: prefs.terminal }),
            autoPlugins,
          )
        : emptyDockLayout(),
    [sessionId, seedDiff, prefs.terminal, autoPlugins],
  );
  const layout = useMemo(() => derive(store), [store, derive]);

  const shownRef = useLatestRef({ layout, tabAvailable });
  const deriveRef = useLatestRef(derive);
  const mutate = useCallback(
    (fn: (l: DockLayout) => DockLayout, derive = deriveRef.current) => {
      if (!sessionId) return;
      updateStore((s) => {
        const current = derive(s);
        const updated = fn(current);
        if (updated === current) return s;
        return {
          version: 4,
          sessions: { ...s.sessions, [sessionId]: updated },
          terminals: terminalPlacements(updated),
        };
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
    (dock: DockLocation, minIndex?: number) => {
      if (sessionId) guardedMutate((l) => revealDock(addTerminal(l, dock, sessionId, minIndex).layout, dock));
    },
    [guardedMutate, sessionId],
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
        // Single-instance panes toggle their tab; terminals toggle this Session's, leaving other Sessions' tabs.
        if (kind === "diff" || kind === "agents" || kind === "files") {
          const at = findTab(l, kind);
          if (at) return isDockCollapsed(l, at.dock) ? openOrRevealTab(l, kind, defaultDock) : removeTab(l, kind);
          return openOrRevealTab(l, kind, defaultDock);
        }
        if (!sessionId) return l;
        const terminals = sessionTerminals(l, sessionId);
        if (terminals.length === 0) return openOrRevealTab(l, terminalTabId(sessionId, 0), defaultDock);
        if (terminals.some(({ dock }) => !isDockCollapsed(l, dock))) return removeSessionTerminals(l, sessionId);
        const first = terminals[0]!;
        return activateOrRevealTab(l, first.dock, first.id);
      }),
    [guardedMutate, sessionId],
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
