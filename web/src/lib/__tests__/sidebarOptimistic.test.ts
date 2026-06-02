import { describe, expect, it } from "vitest";

import {
  EMPTY_OPTIMISTIC,
  effectiveArchivedOf,
  effectivePinnedOf,
  effectiveSnoozedUntilOf,
  reconcileOptimistic,
  serverTriageOf,
  withOverride,
  type OptimisticTriage,
} from "../sidebarOptimistic";
import type { SessionResponse, Workspace } from "../types";

function ws(id: string, sessions: Partial<SessionResponse>[]): Workspace {
  return {
    id,
    branch: null,
    projectPath: "/p",
    displayName: id,
    agents: ["claude"],
    primaryAgent: "claude",
    status: "idle",
    // serverTriageOf only reads the triage timestamps, so a partial cast is
    // enough here and avoids restating the full SessionResponse shape.
    sessions: sessions as SessionResponse[],
  };
}

function override(over: Partial<OptimisticTriage>): OptimisticTriage {
  return withOverride(EMPTY_OPTIMISTIC, over);
}

describe("effective resolvers", () => {
  it("pin/archive override wins, null falls through to the server value", () => {
    expect(effectivePinnedOf(override({ pinned: true }), false)).toBe(true);
    expect(effectivePinnedOf(override({ pinned: false }), true)).toBe(false);
    expect(effectivePinnedOf(EMPTY_OPTIMISTIC, true)).toBe(true);
    expect(effectiveArchivedOf(override({ archived: true }), false)).toBe(true);
    expect(effectiveArchivedOf(EMPTY_OPTIMISTIC, true)).toBe(true);
  });

  it("snooze override: undefined falls through, null and string win", () => {
    expect(effectiveSnoozedUntilOf(EMPTY_OPTIMISTIC, "2099-01-01T00:00:00Z")).toBe(
      "2099-01-01T00:00:00Z",
    );
    expect(effectiveSnoozedUntilOf(override({ snoozedUntil: null }), "x")).toBeNull();
    expect(
      effectiveSnoozedUntilOf(override({ snoozedUntil: "2099-01-01T00:00:00Z" }), null),
    ).toBe("2099-01-01T00:00:00Z");
  });
});

describe("serverTriageOf", () => {
  it("aggregates pin/archive with `.some` and snooze with the first match", () => {
    const w = ws("w", [
      { pinned_at: null, archived_at: null, snoozed_until: null },
      { pinned_at: "2026-01-01T00:00:00Z", archived_at: null, snoozed_until: "2099-01-01T00:00:00Z" },
    ]);
    expect(serverTriageOf(w)).toEqual({
      isPinned: true,
      isArchived: false,
      snoozedUntil: "2099-01-01T00:00:00Z",
    });
  });
});

describe("withOverride", () => {
  it("merges a patch, preserving unmentioned fields", () => {
    const base = override({ pinned: true });
    expect(withOverride(base, { archived: true })).toEqual({
      pinned: true,
      archived: true,
      snoozedUntil: undefined,
    });
  });

  it("applies an explicit null (clear) and an explicit undefined snooze", () => {
    const base = override({ pinned: true, snoozedUntil: "2099-01-01T00:00:00Z" });
    expect(withOverride(base, { pinned: null }).pinned).toBeNull();
    expect(withOverride(base, { snoozedUntil: undefined }).snoozedUntil).toBeUndefined();
  });
});

describe("reconcileOptimistic", () => {
  it("returns the same reference when the map is empty", () => {
    const map = new Map<string, OptimisticTriage>();
    expect(reconcileOptimistic(map, [])).toBe(map);
  });

  it("drops a pin override the server has caught up to", () => {
    const map = new Map([["w", override({ pinned: true })]]);
    const next = reconcileOptimistic(map, [ws("w", [{ pinned_at: "t" }])]);
    expect(next.has("w")).toBe(false);
  });

  it("keeps a pin override the server has not caught up to", () => {
    const map = new Map([["w", override({ pinned: true })]]);
    const next = reconcileOptimistic(map, [ws("w", [{ pinned_at: null }])]);
    expect(next.get("w")?.pinned).toBe(true);
    // No change means the same reference is returned.
    expect(next).toBe(map);
  });

  it("drops an unsnooze override once the server reports no snooze", () => {
    const map = new Map([["w", override({ snoozedUntil: null })]]);
    const next = reconcileOptimistic(map, [ws("w", [{ snoozed_until: null }])]);
    expect(next.has("w")).toBe(false);
  });

  it("drops a snooze override once the server deadline matches (within tolerance)", () => {
    const target = "2099-01-01T00:00:00.000Z";
    const near = "2099-01-01T00:00:30.000Z"; // 30s skew, within the 2min window
    const map = new Map([["w", override({ snoozedUntil: target })]]);
    const next = reconcileOptimistic(map, [ws("w", [{ snoozed_until: near }])]);
    expect(next.has("w")).toBe(false);
  });

  it("keeps an override for a workspace that vanished from the tree", () => {
    const map = new Map([["w", override({ archived: true })]]);
    const next = reconcileOptimistic(map, [ws("other", [{}])]);
    expect(next.get("w")?.archived).toBe(true);
  });

  it("clears only the caught-up field, keeping the rest of the entry", () => {
    const map = new Map([["w", override({ pinned: true, archived: false })]]);
    // Server caught up to the archived=false override (no archive), but not
    // the pin. The pin override survives; the archived override is dropped.
    const next = reconcileOptimistic(map, [ws("w", [{ pinned_at: null, archived_at: null }])]);
    expect(next.get("w")).toEqual({
      pinned: true,
      archived: null,
      snoozedUntil: undefined,
    });
  });
});
