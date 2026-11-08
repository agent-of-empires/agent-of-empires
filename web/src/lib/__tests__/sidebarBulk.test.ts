import { describe, expect, it } from "vitest";

import { bucketSelectionForBulk, summarizeBulkResults } from "../sidebarBulk";
import { EMPTY_OPTIMISTIC, optimisticForRow, optimisticKey, withOverride } from "../sidebarOptimistic";
import type { SessionResponse, Workspace } from "../types";

function ws(id: string, over: Partial<SessionResponse>): Workspace {
  return {
    id,
    branch: null,
    projectPath: "/p",
    displayName: id,
    agents: ["claude"],
    primaryAgent: "claude",
    status: "idle",
    sessions: [over] as SessionResponse[],
  };
}

const noOverride = () => EMPTY_OPTIMISTIC;

describe("bucketSelectionForBulk", () => {
  it("splits a mixed selection into per-action eligible subsets", () => {
    const workspaces = [
      ws("live", {}),
      ws("pinned", { pinned_at: "t" }),
      ws("archived", { archived_at: "t" }),
      ws("snoozed", { snoozed_until: "2099-01-01T00:00:00Z" }),
    ];
    const b = bucketSelectionForBulk(workspaces, noOverride);
    expect(b.pinnable.map((w) => w.id)).toEqual(["live"]);
    expect(b.archivable.map((w) => w.id)).toEqual(["live", "pinned"]);
    expect(b.snoozable.map((w) => w.id)).toEqual(["live", "pinned"]);
    expect(b.unpinnable.map((w) => w.id)).toEqual(["pinned"]);
    expect(b.unarchivable.map((w) => w.id)).toEqual(["archived"]);
    expect(b.unsnoozable.map((w) => w.id)).toEqual(["snoozed"]);
  });

  it("respects an optimistic override over the server value", () => {
    const workspaces = [ws("w", {})];
    const overlay = new Map([[optimisticKey("w", null), withOverride(EMPTY_OPTIMISTIC, { pinned: true })]]);
    const b = bucketSelectionForBulk(workspaces, (workspace) => optimisticForRow(overlay, workspace));
    expect(b.pinnable).toHaveLength(0);
    expect(b.unpinnable.map((w) => w.id)).toEqual(["w"]);
  });

  it("keeps a sibling row eligible when an override targets the other session", () => {
    const snoozed = ws("shared", { id: "a", snoozed_until: "2099-01-01T00:00:00Z" });
    const live = ws("shared", { id: "b" });
    const overlay = new Map([
      [
        optimisticKey("shared", "a"),
        withOverride(EMPTY_OPTIMISTIC, { snoozedUntil: "2099-01-01T00:00:00Z", sessionId: "a" }),
      ],
    ]);
    const buckets = bucketSelectionForBulk([snoozed, live], (workspace) => optimisticForRow(overlay, workspace));
    expect(buckets.unsnoozable.map((w) => w.sessions[0]!.id)).toEqual(["a"]);
    expect(buckets.archivable.map((w) => w.sessions[0]!.id)).toEqual(["b"]);
  });

  it("buckets a recombined row by every pending action on it", () => {
    const combined = ws("shared", { id: "a", status: "Idle", archived_at: null });
    combined.sessions = [
      { id: "a", status: "Idle", archived_at: null },
      { id: "b", status: "Stopped", archived_at: null },
    ] as SessionResponse[];
    const overlay = new Map([
      [optimisticKey("shared", "a"), withOverride(EMPTY_OPTIMISTIC, { unread: true, sessionId: "a" })],
      [optimisticKey("shared", "b"), withOverride(EMPTY_OPTIMISTIC, { archived: true, sessionId: "b" })],
    ]);
    const buckets = bucketSelectionForBulk([combined], (workspace) => optimisticForRow(overlay, workspace));
    expect(buckets.unarchivable.map((w) => w.id)).toEqual(["shared"]);
    expect(buckets.archivable).toEqual([]);
  });

  it("buckets each slice of one workspace by its own pending action", () => {
    const archivedSlice = ws("shared", { id: "a", archived_at: null });
    const liveSlice = ws("shared", { id: "b", unread: false });
    const overlay = new Map([
      [optimisticKey("shared", "a"), withOverride(EMPTY_OPTIMISTIC, { archived: true, sessionId: "a" })],
      [optimisticKey("shared", "b"), withOverride(EMPTY_OPTIMISTIC, { unread: true, sessionId: "b" })],
    ]);
    const buckets = bucketSelectionForBulk([archivedSlice, liveSlice], (workspace) =>
      optimisticForRow(overlay, workspace),
    );
    expect(buckets.unarchivable.map((w) => w.sessions[0]!.id)).toEqual(["a"]);
    expect(buckets.archivable.map((w) => w.sessions[0]!.id)).toEqual(["b"]);
    expect(buckets.snoozable.map((w) => w.sessions[0]!.id)).toEqual(["b"]);
  });
});

describe("summarizeBulkResults", () => {
  it("reports successes, failures, and skips", () => {
    expect(
      summarizeBulkResults("Archived", [{ ok: true }, { ok: true }, { ok: false }, { ok: false, skipped: true }]),
    ).toBe("Archived 2 sessions. 1 failed. 1 skipped.");
  });

  it("uses the singular noun and omits zero counts", () => {
    expect(summarizeBulkResults("Pinned", [{ ok: true }])).toBe("Pinned 1 session.");
  });
});
