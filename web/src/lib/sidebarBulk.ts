import {
  effectiveArchivedOf,
  effectivePinnedOf,
  effectiveSnoozedUntilOf,
  serverTriageOf,
  type OptimisticTriage,
} from "./sidebarOptimistic";
import { triageStateOf } from "./sidebarSort";
import type { Workspace } from "./types";

/** Selection split per bulk action from each workspace's effective (optimistic) triage state, so the bar can offer "Pin 3" / "Unpin 2". */
export interface BulkTriageBuckets {
  pinnable: Workspace[];
  archivable: Workspace[];
  snoozable: Workspace[];
  unpinnable: Workspace[];
  unarchivable: Workspace[];
  unsnoozable: Workspace[];
}

export function bucketSelectionForBulk(
  workspaces: readonly Workspace[],
  optimisticFor: (workspaceId: string) => OptimisticTriage,
): BulkTriageBuckets {
  const buckets: BulkTriageBuckets = {
    pinnable: [],
    archivable: [],
    snoozable: [],
    unpinnable: [],
    unarchivable: [],
    unsnoozable: [],
  };
  for (const ws of workspaces) {
    const o = optimisticFor(ws.id);
    const server = serverTriageOf(ws);
    const state = triageStateOf({
      isPinned: effectivePinnedOf(o, server.isPinned),
      isArchived: effectiveArchivedOf(o, server.isArchived),
      isSnoozed: effectiveSnoozedUntilOf(o, server.snoozedUntil) != null,
    });
    switch (state) {
      case "live":
        buckets.pinnable.push(ws);
        buckets.archivable.push(ws);
        buckets.snoozable.push(ws);
        break;
      case "pinned":
        // Pinned rows can also be archived or snoozed directly; the backend clears the pin.
        buckets.unpinnable.push(ws);
        buckets.archivable.push(ws);
        buckets.snoozable.push(ws);
        break;
      case "archived":
        buckets.unarchivable.push(ws);
        break;
      case "snoozed":
        buckets.unsnoozable.push(ws);
        break;
    }
  }
  return buckets;
}

/** E.g. "Archived 12 workspaces. 2 failed." */
export function summarizeBulkResults(verb: string, results: readonly { ok: boolean; skipped?: boolean }[]): string {
  const ok = results.filter((r) => r.ok).length;
  const skipped = results.filter((r) => r.skipped).length;
  const failed = results.filter((r) => !r.ok && !r.skipped).length;
  const noun = ok === 1 ? "session" : "sessions";
  let msg = `${verb} ${ok} ${noun}.`;
  if (failed > 0) msg += ` ${failed} failed.`;
  if (skipped > 0) msg += ` ${skipped} skipped.`;
  return msg;
}
