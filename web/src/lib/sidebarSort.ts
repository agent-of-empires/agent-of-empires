import type { RepoGroup, SessionResponse, Workspace } from "./types";
import { safeGetItem, safeSetItem } from "./safeStorage";
import { compareSortValues, type PluginSortValue } from "./pluginUi";

export type SidebarSortMode = "manual" | "lastActivity" | "attention";

export const SIDEBAR_SORT_MODE_KEY = "aoe-sidebar-sort-mode";

const VALID_MODES: readonly SidebarSortMode[] = ["manual", "lastActivity", "attention"];

export function loadSidebarSortMode(): SidebarSortMode {
  const raw = safeGetItem(SIDEBAR_SORT_MODE_KEY);
  if (raw && (VALID_MODES as readonly string[]).includes(raw)) {
    return raw as SidebarSortMode;
  }
  return "manual";
}

export function saveSidebarSortMode(mode: SidebarSortMode): void {
  safeSetItem(SIDEBAR_SORT_MODE_KEY, mode);
}

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

/** Every session archived or snoozed, so one live session keeps the workspace out of the footer. */
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

/** Tier first, then last activity descending, then id. Compared with `<`/`>` since missing timestamps are -Infinity. */
export function compareWorkspacesByLastActivityDesc(a: Workspace, b: Workspace): number {
  const aTier = workspaceTriageTier(a);
  const bTier = workspaceTriageTier(b);
  if (aTier !== bTier) return aTier - bTier;
  const aMs = workspaceLastActivityMs(a);
  const bMs = workspaceLastActivityMs(b);
  if (aMs < bMs) return 1;
  if (aMs > bMs) return -1;
  return a.id.localeCompare(b.id);
}

/** Mirrors the TUI's tier-99 sink (`attention_tier`, src/session/groups.rs). */
const ATTENTION_SINK_RANK = 99;

/** Lower ranks sort first. Mirrors the TUI `attention_tier`; sunk sessions take the sink rank. */
export function sessionAttentionRank(s: SessionResponse): number {
  if (s.archived_at != null || s.snoozed_until != null) {
    return ATTENTION_SINK_RANK;
  }
  switch (s.status) {
    case "Waiting":
      return 0;
    case "Error":
      return 1;
    case "Idle":
      return 2;
    case "Unknown":
      return 3;
    case "Running":
      return 4;
    case "Stopped":
      return 5;
    case "Starting":
    case "Creating":
    case "Deleting":
      return 6;
    default:
      // Unknown statuses from a newer server rank like Unknown rather than sinking.
      return 3;
  }
}

export function workspaceAttentionRank(ws: Workspace): number {
  let best = ATTENTION_SINK_RANK;
  for (const s of ws.sessions) {
    const rank = sessionAttentionRank(s);
    if (rank < best) best = rank;
  }
  return best;
}

export function workspaceIsFavorited(ws: Workspace): boolean {
  return ws.sessions.some((s) => s.favorited);
}

/** The server clears urgent for sunk sessions, so they never float above live rows. */
export function workspaceIsUrgent(ws: Workspace): boolean {
  return ws.sessions.some((s) => s.urgent === true);
}

/** A live session that is Waiting, in Error, or flagged urgent. Shared by badges, markers, and jump-to-next so counts match. */
export function sessionNeedsAttention(s: SessionResponse): boolean {
  if (s.archived_at != null || s.snoozed_until != null || s.trashed_at != null) {
    return false;
  }
  return s.status === "Waiting" || s.status === "Error" || s.urgent === true;
}

export function workspaceAttentionCount(ws: Workspace): number {
  let n = 0;
  for (const s of ws.sessions) {
    if (sessionNeedsAttention(s)) n += 1;
  }
  return n;
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

/** Tier, urgent, attention rank, favorited, last activity, then id. */
export function compareWorkspacesByAttention(a: Workspace, b: Workspace): number {
  const aTier = workspaceTriageTier(a);
  const bTier = workspaceTriageTier(b);
  if (aTier !== bTier) return aTier - bTier;

  const aUrgent = workspaceIsUrgent(a);
  const bUrgent = workspaceIsUrgent(b);
  if (aUrgent !== bUrgent) return aUrgent ? -1 : 1;

  const aRank = workspaceAttentionRank(a);
  const bRank = workspaceAttentionRank(b);
  if (aRank !== bRank) return aRank - bRank;

  const aFav = workspaceIsFavorited(a);
  const bFav = workspaceIsFavorited(b);
  if (aFav !== bFav) return aFav ? -1 : 1;

  const aMs = workspaceLastActivityMs(a);
  const bMs = workspaceLastActivityMs(b);
  if (aMs < bMs) return 1;
  if (aMs > bMs) return -1;
  return a.id.localeCompare(b.id);
}

/** For axes without a manual order, where `manual` means last activity. */
export function compareWorkspacesForComputedSortMode(mode: SidebarSortMode): (a: Workspace, b: Workspace) => number {
  if (mode === "attention") return compareWorkspacesByAttention;
  return compareWorkspacesByLastActivityDesc;
}

export function repoGroupAttentionRank(workspaces: readonly Workspace[]): number {
  let best = ATTENTION_SINK_RANK;
  for (const ws of workspaces) {
    const rank = workspaceAttentionRank(ws);
    if (rank < best) best = rank;
  }
  return best;
}

export function repoGroupIsUrgent(workspaces: readonly Workspace[]): boolean {
  return workspaces.some(workspaceIsUrgent);
}

export function repoGroupIsFavorited(workspaces: readonly Workspace[]): boolean {
  return workspaces.some(workspaceIsFavorited);
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
