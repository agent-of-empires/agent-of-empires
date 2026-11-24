import type { RepoGroup, SessionResponse, Workspace } from "./types";
import { compareSortValues, type PluginSortValue } from "./pluginUi";
import type { SidebarSort } from "./sidebarView";

function epochOr(ts: string | null | undefined): number {
  if (!ts) return Number.NEGATIVE_INFINITY;
  const n = Date.parse(ts);
  return Number.isFinite(n) ? n : Number.NEGATIVE_INFINITY;
}

/** Latest activity in epoch ms; `Number.NEGATIVE_INFINITY` when no timestamp parses. */
export function workspaceLastActivityMs(ws: Workspace): number {
  let best = Number.NEGATIVE_INFINITY;
  for (const s of ws.sessions) {
    const m = Math.max(epochOr(s.last_accessed_at), epochOr(s.idle_entered_at), epochOr(s.created_at));
    if (m > best) best = m;
  }
  return best;
}

export function repoGroupLastActivityMs(workspaces: readonly Workspace[]): number {
  let best = Number.NEGATIVE_INFINITY;
  for (const ws of workspaces) {
    const m = workspaceLastActivityMs(ws);
    if (m > best) best = m;
  }
  return best;
}

export function workspaceIsPinned(ws: Workspace): boolean {
  return ws.sessions.some((s) => s.pinned_at != null);
}

/** The row title the sidebar shows, which the Name sort orders by. */
export function workspaceLabel(ws: Workspace): string {
  const title = ws.sessions[0]?.title.trim() ?? "";
  return (ws.sessions.length === 1 ? title || ws.branch : ws.branch || title) || "default";
}

/** Newest session creation in epoch ms; a header never moved by hand sorts by it. */
export function workspacesNewestMs(workspaces: readonly Workspace[]): number {
  let best = Number.NEGATIVE_INFINITY;
  for (const ws of workspaces) for (const s of ws.sessions) best = Math.max(best, epochOr(s.created_at));
  return best;
}

export const compareNames = (a: string, b: string) => a.toLowerCase().localeCompare(b.toLowerCase());

/** Tier, then A to Z by title ignoring case, then id. */
export function compareWorkspacesByName(a: Workspace, b: Workspace): number {
  return (
    workspaceTriageTier(a) - workspaceTriageTier(b) ||
    compareNames(workspaceLabel(a), workspaceLabel(b)) ||
    a.id.localeCompare(b.id)
  );
}

/** Tier, then the shared Custom order. A workspace the order does not list yet sits on top, newest first, as the
 *  Daemon places it. */
export function compareWorkspacesByCustomOrder(order: readonly string[]): (a: Workspace, b: Workspace) => number {
  const rank = new Map(order.map((id, i) => [id, i] as const));
  return (a, b) => {
    const tier = workspaceTriageTier(a) - workspaceTriageTier(b);
    if (tier !== 0) return tier;
    const ar = rank.get(a.id);
    const br = rank.get(b.id);
    if (ar === undefined || br === undefined) {
      if (ar !== br) return ar === undefined ? -1 : 1;
      return workspacesNewestMs([b]) - workspacesNewestMs([a]) || a.id.localeCompare(b.id);
    }
    return ar - br;
  };
}

/** Every session archived or snoozed; one live session keeps the workspace out of the footer. Ignored rows stay in the live list: they are not archived. */
export function workspaceIsSunk(ws: Workspace): boolean {
  if (ws.sessions.length === 0) return false;
  return ws.sessions.every((s) => s.archived_at != null || s.snoozed_until != null || s.trashed_at != null);
}

/** Every session trashed. */
export function workspaceIsTrashed(ws: Workspace): boolean {
  if (ws.sessions.length === 0) return false;
  return ws.sessions.every((s) => s.trashed_at != null);
}

export function workspaceTrashedAtMs(ws: Workspace): number {
  let max = 0;
  for (const s of ws.sessions) {
    if (s.trashed_at == null) continue;
    const t = new Date(s.trashed_at).getTime();
    if (t > max) max = t;
  }
  return max;
}

/** Hides a group header whose workspaces all sank into the footer. */
export function repoGroupHasLiveWorkspace(group: RepoGroup): boolean {
  return group.workspaces.some((ws) => !workspaceIsSunk(ws));
}

/** 0 pinned, 1 live, 2 sunk. The server clears sink fields on pin, so any pinned session wins. */
export function workspaceTriageTier(ws: Workspace): 0 | 1 | 2 {
  if (workspaceIsPinned(ws)) return 0;
  if (workspaceIsSunk(ws)) return 2;
  return 1;
}

/** Snooze timestamps within 2 minutes match, absorbing client/server clock skew. */
export function snoozeTimestampCloseEnough(aIso: string, bIso: string): boolean {
  const a = Date.parse(aIso);
  const b = Date.parse(bIso);
  if (!Number.isFinite(a) || !Number.isFinite(b)) return aIso === bIso;
  return Math.abs(a - b) <= 2 * 60_000;
}

/** Optimistic override: `undefined` falls through, `null` means unsnoozed, a string means snoozed until then. */
export function resolveEffectiveSnoozedUntil(
  optimistic: string | null | undefined,
  serverValue: string | null | undefined,
): string | null | undefined {
  if (optimistic === undefined) return serverValue;
  return optimistic;
}

/** The server keeps pinned/archived/snoozed mutually exclusive. */
export type TriageState = "live" | "pinned" | "archived" | "snoozed";

/** If an aggregate surfaces several states: pinned > archived > snoozed > live. */
export interface TriageMenuShape {
  showPin: boolean;
  showUnpin: boolean;
  showArchive: boolean;
  showUnarchive: boolean;
  showSnooze: boolean;
  showUnsnooze: boolean;
}

export function triageStateOf(input: { isPinned: boolean; isArchived: boolean; isSnoozed: boolean }): TriageState {
  if (input.isPinned) return "pinned";
  if (input.isArchived) return "archived";
  if (input.isSnoozed) return "snoozed";
  return "live";
}

export function triageMenuShape(state: TriageState): TriageMenuShape {
  switch (state) {
    case "pinned":
      // Archiving or snoozing a pinned session is valid (the backend clears the pin), matching the TUI.
      return {
        showPin: false,
        showUnpin: true,
        showArchive: true,
        showUnarchive: false,
        showSnooze: true,
        showUnsnooze: false,
      };
    case "archived":
      return {
        showPin: false,
        showUnpin: false,
        showArchive: false,
        showUnarchive: true,
        showSnooze: false,
        showUnsnooze: false,
      };
    case "snoozed":
      return {
        showPin: false,
        showUnpin: false,
        showArchive: false,
        showUnarchive: false,
        showSnooze: false,
        showUnsnooze: true,
      };
    case "live":
      return {
        showPin: true,
        showUnpin: false,
        showArchive: true,
        showUnarchive: false,
        showSnooze: true,
        showUnsnooze: false,
      };
  }
}

/** A live session that is Waiting, in Error, flagged urgent, or carries a plugin danger/warn tone. Shared by badges, markers, and jump-to-next so counts match. */
export function sessionNeedsAttention(s: SessionResponse, pluginAttention?: ReadonlySet<string>): boolean {
  if (s.archived_at != null || s.ignored_at != null || s.snoozed_until != null || s.trashed_at != null) {
    return false;
  }
  return s.status === "Waiting" || s.status === "Error" || s.urgent === true || pluginAttention?.has(s.id) === true;
}

export function workspaceAttentionCount(ws: Workspace, pluginAttention?: ReadonlySet<string>): number {
  let n = 0;
  for (const s of ws.sessions) {
    if (sessionNeedsAttention(s, pluginAttention)) n += 1;
  }
  return n;
}

/** The Error Sessions among those `workspaceAttentionCount` counts. */
export function workspaceErrorCount(ws: Workspace): number {
  return ws.sessions.filter((s) => s.status === "Error" && sessionNeedsAttention(s)).length;
}

/** A Group attention badge's tooltip, naming errors apart from other attention. */
export function attentionBadgeText(attention: number, errors: number): string {
  const sessions = (n: number) => `${n} Session${n === 1 ? "" : "s"}`;
  const haveErrors = (n: number) => (n === 1 ? "has an error" : "have errors");
  if (errors > 0 && errors === attention) return `${sessions(errors)} ${haveErrors(errors)}`;
  const needs = `${sessions(attention)} need${attention === 1 ? "s" : ""} attention`;
  return errors > 0 ? `${needs} · ${errors} ${haveErrors(errors)}` : needs;
}

/** Next attention session after `activeId` in display order, wrapping; returns `activeId` when it is the only one. */
export function nextAttentionSessionId(
  orderedIds: readonly string[],
  attention: ReadonlySet<string>,
  activeId: string | null,
): string | null {
  if (attention.size === 0) return null;
  const activeIdx = activeId == null ? -1 : orderedIds.indexOf(activeId);
  const start = activeIdx < 0 ? 0 : activeIdx + 1;
  const n = orderedIds.length;
  for (let i = 0; i < n; i += 1) {
    const id = orderedIds[(start + i) % n]!;
    if (attention.has(id)) return id;
  }
  return null;
}

/** Plugin sort direction plus `session_id -> sort_value`, built at the component boundary. */
export interface PluginSortContext {
  direction: "asc" | "desc";
  values: Map<string, PluginSortValue>;
}

/** The best value for the direction; `undefined` when no session has one, so the workspace sinks. */
export function workspacePluginSortValue(ws: Workspace, ctx: PluginSortContext): PluginSortValue | undefined {
  let best: PluginSortValue | undefined;
  for (const s of ws.sessions) {
    const v = ctx.values.get(s.id);
    if (v === undefined) continue;
    if (best === undefined || compareSortValues(v, best, ctx.direction) < 0) best = v;
  }
  return best;
}

export function repoGroupPluginSortValue(
  workspaces: readonly Workspace[],
  ctx: PluginSortContext,
): PluginSortValue | undefined {
  let best: PluginSortValue | undefined;
  for (const ws of workspaces) {
    const v = workspacePluginSortValue(ws, ctx);
    if (v === undefined) continue;
    if (best === undefined || compareSortValues(v, best, ctx.direction) < 0) best = v;
  }
  return best;
}

/** Tier, best plugin value (unvalued sink), last activity, then id. */
export function compareWorkspacesByPluginSort(ctx: PluginSortContext): (a: Workspace, b: Workspace) => number {
  return (a, b) => {
    const aTier = workspaceTriageTier(a);
    const bTier = workspaceTriageTier(b);
    if (aTier !== bTier) return aTier - bTier;
    const cmp = compareSortValues(workspacePluginSortValue(a, ctx), workspacePluginSortValue(b, ctx), ctx.direction);
    if (cmp !== 0) return cmp;
    const aMs = workspaceLastActivityMs(a);
    const bMs = workspaceLastActivityMs(b);
    if (aMs < bMs) return 1;
    if (aMs > bMs) return -1;
    return a.id.localeCompare(b.id);
  };
}

/** Row order inside every header: an active plugin sort, else the shared sort. */
export function workspaceComparator(
  sort: SidebarSort,
  customOrder: readonly string[],
  pluginSort?: PluginSortContext,
): (a: Workspace, b: Workspace) => number {
  if (pluginSort) return compareWorkspacesByPluginSort(pluginSort);
  return sort === "name" ? compareWorkspacesByName : compareWorkspacesByCustomOrder(customOrder);
}
