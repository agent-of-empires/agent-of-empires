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

/** Optimistic triage override for a single sidebar row, keyed by workspace
 *  id in the sidebar's overlay map. Each field mirrors the three pieces of
 *  per-row state that used to live inside `SessionRow` as `useState`:
 *  `pinned` / `archived` use `boolean | null` where `null` means "no
 *  override, fall through to the server value"; `snoozedUntil` uses
 *  `string | null | undefined` where `undefined` means "no override",
 *  `null` means "pretend the server already unsnoozed", and a string means
 *  "pretend the server already snoozed until then". Lifting this out of the
 *  row is what lets a bulk action drive many rows from one place instead of
 *  reaching into N independent row components. See #1724. */
export interface OptimisticTriage {
  pinned: boolean | null;
  archived: boolean | null;
  snoozedUntil: string | null | undefined;
}

/** A row with no optimistic override. Shared frozen singleton so rows that
 *  are not mid-mutation all read the same object identity (keeps the
 *  memoized `SessionRow` from re-rendering on unrelated overlay changes). */
export const EMPTY_OPTIMISTIC: OptimisticTriage = Object.freeze({
  pinned: null,
  archived: null,
  snoozedUntil: undefined,
});

/** Server-truth triage aggregates for a workspace, matching the exact
 *  per-row aggregators the sidebar renders with: pin/archive use `.some`
 *  across the workspace's sessions, snooze takes the first session that
 *  carries a `snoozed_until`. Kept here so the overlay reconciler and the
 *  row render agree on the same baseline. */
export function serverTriageOf(ws: Workspace): {
  isPinned: boolean;
  isArchived: boolean;
  snoozedUntil: string | null;
} {
  return {
    isPinned: ws.sessions.some((s) => s.pinned_at != null),
    isArchived: ws.sessions.some((s) => s.archived_at != null),
    snoozedUntil: ws.sessions.find((s) => s.snoozed_until)?.snoozed_until ?? null,
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

/** True when an override has been caught up to by the server and can be
 *  dropped. Mirrors the three per-field reconciliation effects that used to
 *  live in `SessionRow`: a boolean override clears once it equals the
 *  server value; a snooze override clears when both sides are unsnoozed or
 *  both point at the same deadline (within the close-enough tolerance). */
function fieldCaughtUp<T>(override: T | null, server: T): boolean {
  return override !== null && override === server;
}

function snoozeCaughtUp(override: string | null | undefined, server: string | null): boolean {
  if (override === undefined) return false;
  if (override === null) return server == null;
  return server != null && snoozeTimestampCloseEnough(override, server);
}

/** Given the current overlay map and the latest workspaces, return a new map
 *  with any override the server has caught up to removed, dropping rows whose
 *  every field has reconciled. Returns the SAME map reference when nothing
 *  changed so callers can skip a state update (avoids a render loop when used
 *  from an effect). Pure: no React, fully unit-testable. */
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
    // Workspace vanished from the tree: keep the override until it either
    // reappears (and reconciles) or the consumer prunes it; dropping it
    // here would make a row flicker back to a stale state mid-refresh.
    if (!server) {
      next.set(id, override);
      continue;
    }
    const pinned = fieldCaughtUp(override.pinned, server.isPinned) ? null : override.pinned;
    const archived = fieldCaughtUp(override.archived, server.isArchived) ? null : override.archived;
    const snoozedUntil = snoozeCaughtUp(override.snoozedUntil, server.snoozedUntil) ? undefined : override.snoozedUntil;
    if (pinned !== override.pinned || archived !== override.archived || snoozedUntil !== override.snoozedUntil) {
      changed = true;
    }
    if (pinned === null && archived === null && snoozedUntil === undefined) {
      changed = true;
      continue;
    }
    next.set(id, { pinned, archived, snoozedUntil });
  }
  return changed ? next : (map as Map<string, OptimisticTriage>);
}

/** Merge a partial override into an existing entry, preserving the fields the
 *  patch does not mention. Used by both single-row and bulk mutations to set
 *  the optimistic state before the request lands. */
export function withOverride(prev: OptimisticTriage | undefined, patch: Partial<OptimisticTriage>): OptimisticTriage {
  return {
    pinned: patch.pinned !== undefined ? patch.pinned : (prev?.pinned ?? null),
    archived: patch.archived !== undefined ? patch.archived : (prev?.archived ?? null),
    snoozedUntil: "snoozedUntil" in patch ? patch.snoozedUntil : (prev?.snoozedUntil ?? undefined),
  };
}
