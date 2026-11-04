import type { SessionResponse, Workspace } from "./types";
import { resolveEffectiveSnoozedUntil, snoozeTimestampCloseEnough } from "./sidebarSort";

/** Wall-clock target for an optimistic snooze: `Date.now() + minutes *
 *  60_000` as an RFC3339 ISO string. Sits outside any component so the
 *  `Date.now()` call doesn't trip `react-hooks/purity`; the event handler
 *  that calls it is itself a closure, not a render. The exact value is
 *  throwaway (the server's response on the next poll is the source of
 *  truth), so a few ms of jitter is harmless. See #1581. */
export function makeOptimisticSnoozedUntil(minutes: number): string {
  return new Date(Date.now() + minutes * 60_000).toISOString();
}

/** Optimistic triage override for one row. `null` falls through to the server value; for `snoozedUntil`, `undefined` falls through and `null` means unsnoozed. */
export interface OptimisticTriage {
  pinned: boolean | null;
  archived: boolean | null;
  /** The session every override here was issued for; only the rows that show it render the entry. */
  sessionId: string | null;
  snoozedUntil: string | null | undefined;
  unread: boolean | null;
}

/** Frozen singleton so rows without an override share one identity and memoized rows don't re-render. */
export const EMPTY_OPTIMISTIC: OptimisticTriage = Object.freeze({
  pinned: null,
  archived: null,
  sessionId: null,
  snoozedUntil: undefined,
  unread: null,
});

/** The same aggregates the row renders with: any-session pin/archive, first session's snooze. */
export function serverTriageOf(ws: Workspace): {
  isPinned: boolean;
  isArchived: boolean;
  snoozedUntil: string | null;
  unread: boolean;
} {
  return {
    isPinned: ws.sessions.some((s) => s.pinned_at != null),
    isArchived: ws.sessions.some((s) => s.archived_at != null),
    snoozedUntil: ws.sessions.find((s) => s.snoozed_until)?.snoozed_until ?? null,
    unread: ws.sessions.some((s) => s.unread === true),
  };
}

export function effectivePinnedOf(optimistic: OptimisticTriage, serverPinned: boolean): boolean {
  return optimistic.pinned ?? serverPinned;
}

export function effectiveArchivedOf(optimistic: OptimisticTriage, serverArchived: boolean): boolean {
  return optimistic.archived ?? serverArchived;
}

export function effectiveSnoozedUntilOf(
  optimistic: OptimisticTriage,
  serverSnoozedUntil: string | null | undefined,
): string | null | undefined {
  return resolveEffectiveSnoozedUntil(optimistic.snoozedUntil, serverSnoozedUntil);
}

export function effectiveUnreadOf(optimistic: OptimisticTriage, serverUnread: boolean): boolean {
  return optimistic.unread ?? serverUnread;
}

/** The override a row may render: a pending action belongs only to the session it was issued for. */
export function optimisticForRow(map: ReadonlyMap<string, OptimisticTriage>, workspace: Workspace): OptimisticTriage {
  const override = map.get(workspace.id);
  if (!override) return EMPTY_OPTIMISTIC;
  if (override.sessionId == null) return override;
  return workspace.sessions.some((s) => s.id === override.sessionId) ? override : EMPTY_OPTIMISTIC;
}

/** A boolean clears once it matches the server; a snooze clears when both are unsnoozed or the deadlines are close enough. */
function fieldCaughtUp<T>(override: T | null, server: T): boolean {
  return override !== null && override === server;
}

function snoozeCaughtUp(override: string | null | undefined, server: string | null): boolean {
  if (override === undefined) return false;
  if (override === null) return server == null;
  return server != null && snoozeTimestampCloseEnough(override, server);
}

/** Drop overrides the server caught up to. Returns the same map when nothing changed, avoiding render loops. */
export function reconcileOptimistic(
  map: ReadonlyMap<string, OptimisticTriage>,
  workspaces: readonly Workspace[],
): Map<string, OptimisticTriage> {
  if (map.size === 0) return map as Map<string, OptimisticTriage>;
  // A workspace can arrive as several slices (one per group or status bucket); the override
  // reconciles against its target session wherever that slice appears.
  const sessionsById = new Map<string, Map<string, SessionResponse>>();
  for (const ws of workspaces) {
    let sessions = sessionsById.get(ws.id);
    if (!sessions) sessionsById.set(ws.id, (sessions = new Map()));
    for (const s of ws.sessions) sessions.set(s.id, s);
  }
  let changed = false;
  const next = new Map<string, OptimisticTriage>();
  for (const [id, override] of map) {
    const sessions = sessionsById.get(id);
    // Keep a vanished workspace's override so a mid-refresh row doesn't flicker back to stale state.
    if (!sessions) {
      next.set(id, override);
      continue;
    }
    const target = override.sessionId == null ? undefined : sessions.get(override.sessionId);
    // Nothing renders an override whose target session left the workspace.
    if (override.sessionId != null && !target) {
      changed = true;
      continue;
    }
    const all = [...sessions.values()];
    const serverPinned = target ? target.pinned_at != null : all.some((s) => s.pinned_at != null);
    const serverArchived = target ? target.archived_at != null : all.some((s) => s.archived_at != null);
    const serverSnoozedUntil = target
      ? (target.snoozed_until ?? null)
      : (all.find((s) => s.snoozed_until)?.snoozed_until ?? null);
    const serverUnread = target ? target.unread === true : all.some((s) => s.unread === true);
    const pinned = fieldCaughtUp(override.pinned, serverPinned) ? null : override.pinned;
    const archived = fieldCaughtUp(override.archived, serverArchived) ? null : override.archived;
    const snoozedUntil = snoozeCaughtUp(override.snoozedUntil, serverSnoozedUntil) ? undefined : override.snoozedUntil;
    const unread = fieldCaughtUp(override.unread, serverUnread) ? null : override.unread;
    if (
      pinned !== override.pinned ||
      archived !== override.archived ||
      snoozedUntil !== override.snoozedUntil ||
      unread !== override.unread
    ) {
      changed = true;
    }
    if (pinned === null && archived === null && snoozedUntil === undefined && unread === null) {
      changed = true;
      continue;
    }
    next.set(id, { pinned, archived, sessionId: override.sessionId, snoozedUntil, unread });
  }
  return changed ? next : (map as Map<string, OptimisticTriage>);
}

export function withOverride(prev: OptimisticTriage | undefined, patch: Partial<OptimisticTriage>): OptimisticTriage {
  return {
    pinned: patch.pinned !== undefined ? patch.pinned : (prev?.pinned ?? null),
    archived: patch.archived !== undefined ? patch.archived : (prev?.archived ?? null),
    sessionId: patch.sessionId !== undefined ? patch.sessionId : (prev?.sessionId ?? null),
    snoozedUntil: "snoozedUntil" in patch ? patch.snoozedUntil : (prev?.snoozedUntil ?? undefined),
    unread: patch.unread !== undefined ? patch.unread : (prev?.unread ?? null),
  };
}
