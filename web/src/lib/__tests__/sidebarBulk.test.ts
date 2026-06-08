import { describe, expect, it } from "vitest";

import { bucketSelectionForBulk, summarizeBulkResults } from "../sidebarBulk";
import { EMPTY_OPTIMISTIC, withOverride } from "../sidebarOptimistic";
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
    expect(b.archivable.map((w) => w.id)).toEqual(["live"]);
    expect(b.snoozable.map((w) => w.id)).toEqual(["live"]);
    expect(b.unpinnable.map((w) => w.id)).toEqual(["pinned"]);
    expect(b.unarchivable.map((w) => w.id)).toEqual(["archived"]);
    expect(b.unsnoozable.map((w) => w.id)).toEqual(["snoozed"]);
  });

  it("respects an optimistic override over the server value", () => {
    // Server says live, but a pending optimistic pin makes it eligible only
    // for Unpin, not Pin.
    const workspaces = [ws("w", {})];
    const overlay = new Map([
      ["w", withOverride(EMPTY_OPTIMISTIC, { pinned: true })],
    ]);
    const b = bucketSelectionForBulk(
      workspaces,
      (id) => overlay.get(id) ?? EMPTY_OPTIMISTIC,
    );
    expect(b.pinnable).toHaveLength(0);
    expect(b.unpinnable.map((w) => w.id)).toEqual(["w"]);
  });
});

describe("summarizeBulkResults", () => {
  it("reports successes, failures, and skips", () => {
    expect(
      summarizeBulkResults("Archived", [
        { ok: true },
        { ok: true },
        { ok: false },
        { ok: false, skipped: true },
      ]),
    ).toBe("Archived 2 sessions. 1 failed. 1 skipped.");
  });

  it("uses the singular noun and omits zero counts", () => {
    expect(summarizeBulkResults("Pinned", [{ ok: true }])).toBe(
      "Pinned 1 session.",
    );
  });
});
