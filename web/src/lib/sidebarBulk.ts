import {
  effectiveArchivedOf,
  effectivePinnedOf,
  effectiveSnoozedUntilOf,
  serverTriageOf,
  type OptimisticTriage,
} from "./sidebarOptimistic";
import { triageStateOf } from "./sidebarSort";
import type { Workspace } from "./types";

/** Eligible subsets of a multi-selection for each bulk triage action.
 *  Mixed selections (live + pinned + archived + snoozed) split into
 *  count-labelled buckets so the bulk bar can offer "Pin 3" / "Unpin 2"
 *  rather than one ambiguous toggle, mirroring the single-row menu's
 *  state machine (triageMenuShape). A workspace's eligibility is computed
 *  from its effective triage state (server value overlaid with any pending
 *  optimistic override). See #1724. */
export interface BulkTriageBuckets {
  /** Live rows can be pinned, archived, or snoozed. */
  pinnable: Workspace[];
  archivable: Workspace[];
  snoozable: Workspace[];
  /** Already-triaged rows offer only their inverse. */
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
        // Pinned rows can be unpinned, and also archived or snoozed
        // directly: the backend clears the pin on either transition,
        // matching the single-row menu (triageMenuShape) and the TUI.
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

/** One-line summary toast for a completed bulk action, e.g.
 *  "Archived 12 workspaces. 2 failed." Skipped rows (no session) are folded
 *  in only when present. */
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
