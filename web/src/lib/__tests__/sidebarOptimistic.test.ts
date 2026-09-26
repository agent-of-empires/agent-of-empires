import { describe, expect, it } from "vitest";

import {
  EMPTY_OPTIMISTIC,
  effectiveArchivedOf,
  effectivePinnedOf,
  effectiveSnoozedUntilOf,
  effectiveUnreadOf,
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
    sessions: sessions as SessionResponse[],
  };
}

function override(over: Partial<OptimisticTriage>): OptimisticTriage {
  return withOverride(EMPTY_OPTIMISTIC, over);
}

it("effective resolvers let a defined override win and fall through to the server value", () => {
  expect(effectivePinnedOf(override({ pinned: true }), false)).toBe(true);
  expect(effectivePinnedOf(override({ pinned: false }), true)).toBe(false);
  expect(effectivePinnedOf(EMPTY_OPTIMISTIC, true)).toBe(true);
  expect(effectiveArchivedOf(override({ archived: true }), false)).toBe(true);
  expect(effectiveArchivedOf(EMPTY_OPTIMISTIC, true)).toBe(true);
  expect(effectiveSnoozedUntilOf(EMPTY_OPTIMISTIC, "2099-01-01T00:00:00Z")).toBe("2099-01-01T00:00:00Z");
  expect(effectiveSnoozedUntilOf(override({ snoozedUntil: null }), "x")).toBeNull();
  expect(effectiveSnoozedUntilOf(override({ snoozedUntil: "2099-01-01T00:00:00Z" }), null)).toBe(
    "2099-01-01T00:00:00Z",
  );
  expect(effectiveUnreadOf(EMPTY_OPTIMISTIC, true)).toBe(true);
  expect(effectiveUnreadOf(override({ unread: false }), true)).toBe(false);
  expect(effectiveUnreadOf(override({ unread: true }), false)).toBe(true);
});

it("serverTriageOf aggregates pin/archive/unread with `.some` and snooze with the first match", () => {
  const w = ws("w", [
    { pinned_at: null, archived_at: null, snoozed_until: null, unread: true },
    { pinned_at: "2026-01-01T00:00:00Z", archived_at: null, snoozed_until: "2099-01-01T00:00:00Z" },
  ]);
  expect(serverTriageOf(w)).toEqual({
    isPinned: true,
    isArchived: false,
    snoozedUntil: "2099-01-01T00:00:00Z",
    unread: true,
  });
});

it("withOverride merges a patch, applying explicit null and undefined", () => {
  const base = override({ pinned: true, snoozedUntil: "2099-01-01T00:00:00Z" });
  expect(withOverride(override({ pinned: true }), { archived: true })).toEqual({
    pinned: true,
    archived: true,
    snoozedUntil: undefined,
    unread: null,
  });
  expect(withOverride(base, { pinned: null }).pinned).toBeNull();
  expect(withOverride(base, { snoozedUntil: undefined }).snoozedUntil).toBeUndefined();
});

describe("reconcileOptimistic", () => {
  it("returns the same reference when nothing changes", () => {
    const empty = new Map<string, OptimisticTriage>();
    expect(reconcileOptimistic(empty, [])).toBe(empty);
    const pending = new Map([["w", override({ pinned: true })]]);
    expect(reconcileOptimistic(pending, [ws("w", [{ pinned_at: null }])])).toBe(pending);
  });

  it("drops an override once the server catches up, and keeps it until then", () => {
    const cases: [string, Partial<OptimisticTriage>, Workspace, boolean][] = [
      ["pin caught up", { pinned: true }, ws("w", [{ pinned_at: "t" }]), false],
      ["pin pending", { pinned: true }, ws("w", [{ pinned_at: null }]), true],
      ["unsnooze caught up", { snoozedUntil: null }, ws("w", [{ snoozed_until: null }]), false],
      // 30s skew is within the 2min tolerance.
      [
        "snooze within tolerance",
        { snoozedUntil: "2099-01-01T00:00:00.000Z" },
        ws("w", [{ snoozed_until: "2099-01-01T00:00:30.000Z" }]),
        false,
      ],
      ["workspace vanished", { archived: true }, ws("other", [{}]), true],
      ["mark-read caught up", { unread: false }, ws("w", [{ unread: false }]), false],
      ["mark-unread pending", { unread: true }, ws("w", [{ unread: false }]), true],
      ["mark-unread caught up", { unread: true }, ws("w", [{ unread: true }]), false],
    ];
    for (const [name, over, tree, kept] of cases) {
      expect(reconcileOptimistic(new Map([["w", override(over)]]), [tree]).has("w"), name).toBe(kept);
    }
  });

  it("clears only the caught-up field, keeping the rest of the entry", () => {
    const map = new Map([["w", override({ pinned: true, archived: false })]]);
    const next = reconcileOptimistic(map, [ws("w", [{ pinned_at: null, archived_at: null }])]);
    expect(next.get("w")).toEqual({ pinned: true, archived: null, snoozedUntil: undefined, unread: null });
  });
});
