import type { Workspace } from "./types";
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
  snoozedUntil: string | null | undefined;
  unread: boolean | null;
}

/** Frozen singleton so rows without an override share one identity and memoized rows don't re-render. */
export const EMPTY_OPTIMISTIC: OptimisticTriage = Object.freeze({
  pinned: null,
  archived: null,
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
  const serverById = new Map<string, ReturnType<typeof serverTriageOf>>();
  for (const ws of workspaces) {
    if (!serverById.has(ws.id)) serverById.set(ws.id, serverTriageOf(ws));
  }
  let changed = false;
  const next = new Map<string, OptimisticTriage>();
  for (const [id, override] of map) {
    const server = serverById.get(id);
    // Keep a vanished workspace's override so a mid-refresh row doesn't flicker back to stale state.
    if (!server) {
      next.set(id, override);
      continue;
    }
    const pinned = fieldCaughtUp(override.pinned, server.isPinned) ? null : override.pinned;
    const archived = fieldCaughtUp(override.archived, server.isArchived) ? null : override.archived;
    const snoozedUntil = snoozeCaughtUp(override.snoozedUntil, server.snoozedUntil) ? undefined : override.snoozedUntil;
    const unread = fieldCaughtUp(override.unread, server.unread) ? null : override.unread;
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
    next.set(id, { pinned, archived, snoozedUntil, unread });
  }
  return changed ? next : (map as Map<string, OptimisticTriage>);
}

export function withOverride(prev: OptimisticTriage | undefined, patch: Partial<OptimisticTriage>): OptimisticTriage {
  return {
    pinned: patch.pinned !== undefined ? patch.pinned : (prev?.pinned ?? null),
    archived: patch.archived !== undefined ? patch.archived : (prev?.archived ?? null),
    snoozedUntil: "snoozedUntil" in patch ? patch.snoozedUntil : (prev?.snoozedUntil ?? undefined),
    unread: patch.unread !== undefined ? patch.unread : (prev?.unread ?? null),
  };
}
