// @vitest-environment jsdom

import { beforeEach, describe, expect, it } from "vitest";
import type { RepoGroup, SessionResponse, Workspace } from "../types";
import {
  SIDEBAR_SORT_MODE_KEY,
  compareWorkspacesByAttention,
  compareWorkspacesByLastActivityDesc,
  compareWorkspacesForComputedSortMode,
  loadSidebarSortMode,
  repoGroupAttentionRank,
  repoGroupHasLiveWorkspace,
  repoGroupIsUrgent,
  repoGroupLastActivityMs,
  resolveEffectiveSnoozedUntil,
  saveSidebarSortMode,
  sessionAttentionRank,
  snoozeTimestampCloseEnough,
  triageMenuShape,
  triageStateOf,
  workspaceAttentionRank,
  workspaceIsFavorited,
  workspaceIsPinned,
  workspaceIsSunk,
  workspaceIsUrgent,
  workspaceLastActivityMs,
  workspaceTriageTier,
} from "../sidebarSort";

function session(over: Partial<SessionResponse> = {}): SessionResponse {
  return {
    id: "s1",
    title: "t",
    project_path: "/p",
    group_path: "/p",
    tool: "claude",
    status: "Idle",
    yolo_mode: false,
    created_at: "2025-01-01T00:00:00Z",
    last_accessed_at: null,
    idle_entered_at: null,
    last_error: null,
    branch: null,
    main_repo_path: null,
    is_sandboxed: false,
    favorited: false,
    has_managed_worktree: false,
    has_terminal: true,
    profile: "default",
    cleanup_defaults: {
      delete_worktree: false,
      delete_branch: false,
      delete_sandbox: false,
    },
    remote_owner: null,
    notify_on_waiting: null,
    notify_on_idle: null,
    notify_on_error: null,
    claude_fullscreen: false,
    workspace_repos: [],
    ...over,
  };
}

function workspace(id: string, sessions: SessionResponse[]): Workspace {
  return {
    id,
    branch: null,
    projectPath: "/p",
    displayName: id,
    agents: ["claude"],
    primaryAgent: "claude",
    status: "idle",
    sessions,
  };
}

describe("workspaceLastActivityMs", () => {
  it("returns max across last_accessed_at, idle_entered_at, created_at", () => {
    const ws = workspace("w", [
      session({
        created_at: "2025-01-01T00:00:00Z",
        idle_entered_at: "2025-03-01T00:00:00Z",
        last_accessed_at: "2025-02-01T00:00:00Z",
      }),
    ]);
    expect(workspaceLastActivityMs(ws)).toBe(
      Date.parse("2025-03-01T00:00:00Z"),
    );
  });

  it("ignores null timestamps; created_at fallback works", () => {
    const ws = workspace("w", [
      session({
        created_at: "2025-05-01T00:00:00Z",
        idle_entered_at: null,
        last_accessed_at: null,
      }),
    ]);
    expect(workspaceLastActivityMs(ws)).toBe(
      Date.parse("2025-05-01T00:00:00Z"),
    );
  });

  it("ignores unparseable strings (no NaN poison)", () => {
    const ws = workspace("w", [
      session({
        created_at: "2025-04-01T00:00:00Z",
        idle_entered_at: "not-a-date",
        last_accessed_at: "also-bad",
      }),
    ]);
    expect(workspaceLastActivityMs(ws)).toBe(
      Date.parse("2025-04-01T00:00:00Z"),
    );
  });

  it("returns max across every session in a multi-session workspace", () => {
    const ws = workspace("w", [
      session({ id: "s1", created_at: "2025-01-01T00:00:00Z" }),
      session({ id: "s2", created_at: "2025-06-01T00:00:00Z" }),
      session({ id: "s3", created_at: "2025-03-01T00:00:00Z" }),
    ]);
    expect(workspaceLastActivityMs(ws)).toBe(
      Date.parse("2025-06-01T00:00:00Z"),
    );
  });

  it("returns NEGATIVE_INFINITY when no usable timestamp exists", () => {
    const ws = workspace("w", [
      session({
        created_at: "bad",
        idle_entered_at: null,
        last_accessed_at: null,
      }),
    ]);
    expect(workspaceLastActivityMs(ws)).toBe(Number.NEGATIVE_INFINITY);
  });
});

describe("compareWorkspacesByLastActivityDesc", () => {
  it("orders newer activity first", () => {
    const older = workspace("older", [
      session({ id: "s1", created_at: "2025-01-01T00:00:00Z" }),
    ]);
    const newer = workspace("newer", [
      session({ id: "s2", created_at: "2025-09-01T00:00:00Z" }),
    ]);
    const list = [older, newer].sort(compareWorkspacesByLastActivityDesc);
    expect(list.map((w) => w.id)).toEqual(["newer", "older"]);
  });

  it("breaks ties by workspace id ascending (deterministic)", () => {
    const ts = "2025-01-01T00:00:00Z";
    const wsB = workspace("b", [session({ id: "sb", created_at: ts })]);
    const wsA = workspace("a", [session({ id: "sa", created_at: ts })]);
    const wsC = workspace("c", [session({ id: "sc", created_at: ts })]);
    const list = [wsB, wsA, wsC].sort(compareWorkspacesByLastActivityDesc);
    expect(list.map((w) => w.id)).toEqual(["a", "b", "c"]);
  });

  it("breaks ties by id when both sides have no usable timestamp", () => {
    // Both workspaces return NEGATIVE_INFINITY from
    // workspaceLastActivityMs. Subtracting two -Infinity values yields
    // NaN, which Array.sort treats like equality and skips the
    // tie-break, so this case would silently flake without the
    // explicit `<` / `>` comparison in the comparator.
    const bad = {
      created_at: "bad",
      idle_entered_at: null,
      last_accessed_at: null,
    };
    const wsB = workspace("b", [session({ id: "sb", ...bad })]);
    const wsA = workspace("a", [session({ id: "sa", ...bad })]);
    const list = [wsB, wsA].sort(compareWorkspacesByLastActivityDesc);
    expect(list.map((w) => w.id)).toEqual(["a", "b"]);
  });
});

describe("repoGroupLastActivityMs", () => {
  it("returns the max across workspaces in the group", () => {
    const a = workspace("a", [
      session({ id: "sa", created_at: "2025-01-01T00:00:00Z" }),
    ]);
    const b = workspace("b", [
      session({ id: "sb", created_at: "2025-07-01T00:00:00Z" }),
    ]);
    expect(repoGroupLastActivityMs([a, b])).toBe(
      Date.parse("2025-07-01T00:00:00Z"),
    );
  });

  it("returns NEGATIVE_INFINITY on an empty group", () => {
    expect(repoGroupLastActivityMs([])).toBe(Number.NEGATIVE_INFINITY);
  });
});

describe("workspaceIsPinned", () => {
  it("returns true when any session has pinned_at set", () => {
    const ws = workspace("w", [
      session({ id: "s1" }),
      session({ id: "s2", pinned_at: "2025-01-01T00:00:00Z" }),
    ]);
    expect(workspaceIsPinned(ws)).toBe(true);
  });

  it("returns false when no session is pinned", () => {
    const ws = workspace("w", [session({ id: "s1" }), session({ id: "s2" })]);
    expect(workspaceIsPinned(ws)).toBe(false);
  });
});

describe("workspaceIsSunk", () => {
  it("returns true when every session is archived or snoozed", () => {
    const ws = workspace("w", [
      session({ id: "s1", archived_at: "2025-01-01T00:00:00Z" }),
      session({ id: "s2", snoozed_until: "2026-01-01T00:00:00Z" }),
    ]);
    expect(workspaceIsSunk(ws)).toBe(true);
  });

  it("returns false when even one session is live", () => {
    // A multi-session workspace with one live session must stay in the
    // active tier rather than sinking the whole group out of sight.
    const ws = workspace("w", [
      session({ id: "s1", archived_at: "2025-01-01T00:00:00Z" }),
      session({ id: "s2" }),
    ]);
    expect(workspaceIsSunk(ws)).toBe(false);
  });

  it("returns false on an empty workspace", () => {
    const ws = workspace("w", []);
    expect(workspaceIsSunk(ws)).toBe(false);
  });

  it("returns true when every session is archived-only (no snooze)", () => {
    // Branch coverage on the lambda: `archived_at != null` true side
    // alone, no snoozed_until contribution.
    const ws = workspace("w", [
      session({ id: "s1", archived_at: "2025-01-01T00:00:00Z" }),
      session({ id: "s2", archived_at: "2025-02-01T00:00:00Z" }),
    ]);
    expect(workspaceIsSunk(ws)).toBe(true);
  });

  it("returns true when every session is snoozed-only (no archive)", () => {
    // Branch coverage on the lambda: `snoozed_until != null` true
    // side alone, after archived_at false short-circuits the `||`.
    const ws = workspace("w", [
      session({ id: "s1", snoozed_until: "2099-01-01T00:00:00Z" }),
      session({ id: "s2", snoozed_until: "2099-02-01T00:00:00Z" }),
    ]);
    expect(workspaceIsSunk(ws)).toBe(true);
  });

  it("returns false when one session has neither flag", () => {
    // Branch coverage: the lambda's `archived_at != null` false side
    // AND `snoozed_until != null` false side both fire, then `every`
    // short-circuits with false.
    const ws = workspace("w", [
      session({ id: "s1", archived_at: "2025-01-01T00:00:00Z" }),
      session({ id: "s2" }), // live session breaks the every().
    ]);
    expect(workspaceIsSunk(ws)).toBe(false);
  });
});

describe("workspaceTriageTier", () => {
  it("returns 0 (pinned) for any pinned session, overriding sink fields", () => {
    // Pin clears archive/snooze server-side, but a sibling session in
    // the same workspace could still be archived. Any-pinned wins.
    const ws = workspace("w", [
      session({ id: "s1", pinned_at: "2025-01-01T00:00:00Z" }),
      session({ id: "s2", archived_at: "2025-01-01T00:00:00Z" }),
    ]);
    expect(workspaceTriageTier(ws)).toBe(0);
  });

  it("returns 1 (live) when neither pinned nor fully sunk", () => {
    const ws = workspace("w", [session({ id: "s1" })]);
    expect(workspaceTriageTier(ws)).toBe(1);
  });

  it("returns 2 (sunk) when every session is archived or snoozed", () => {
    const ws = workspace("w", [
      session({ id: "s1", archived_at: "2025-01-01T00:00:00Z" }),
    ]);
    expect(workspaceTriageTier(ws)).toBe(2);
  });
});

describe("resolveEffectiveSnoozedUntil", () => {
  it("returns the server value when no optimistic override is set", () => {
    // Regression: snooze had no optimistic state; the chip waited
    // for the next sessions-poll to flip, which read laggy compared
    // to pin / archive's instant feedback. See #1581 CodeRabbit
    // review. `undefined` on the optimistic side means "no override,
    // fall through to the server value."
    expect(resolveEffectiveSnoozedUntil(undefined, null)).toBeNull();
    expect(
      resolveEffectiveSnoozedUntil(undefined, "2099-01-01T00:00:00Z"),
    ).toBe("2099-01-01T00:00:00Z");
  });

  it("uses an optimistic string to render the snooze chip pre-PATCH", () => {
    // Regression: clicking a preset must flip the chip immediately,
    // not wait for the round-trip. Optimistic value wins over the
    // (still-null) server prop until the next poll mirrors it.
    expect(resolveEffectiveSnoozedUntil("2099-01-01T00:00:00Z", null)).toBe(
      "2099-01-01T00:00:00Z",
    );
  });

  it("uses an explicit null override to hide the chip pre-PATCH on unsnooze", () => {
    // Clicking Unsnooze flips the chip away while the PATCH is in
    // flight. The optimistic null wins until the server prop also
    // returns null and the clear-on-prop-sync effect drops the
    // override.
    expect(
      resolveEffectiveSnoozedUntil(null, "2099-01-01T00:00:00Z"),
    ).toBeNull();
  });
});

describe("snoozeTimestampCloseEnough", () => {
  it("treats equal timestamps as a match", () => {
    expect(
      snoozeTimestampCloseEnough(
        "2099-01-01T00:00:00Z",
        "2099-01-01T00:00:00Z",
      ),
    ).toBe(true);
  });

  it("tolerates a 30-second skew", () => {
    expect(
      snoozeTimestampCloseEnough(
        "2099-01-01T00:00:00Z",
        "2099-01-01T00:00:30Z",
      ),
    ).toBe(true);
  });

  it("rejects a 5-minute skew (re-snooze case)", () => {
    // Regression: re-snoozing an already-snoozed row used to drop
    // the optimistic override because the prop and override were
    // both non-null. The new helper compares actual timestamps so
    // a 1h re-snooze on a row already sitting on a 1h snooze
    // (where the server hasn't acked the new duration yet) keeps
    // the optimistic chip visible. See #1581 CodeRabbit review.
    expect(
      snoozeTimestampCloseEnough(
        "2099-01-01T00:00:00Z",
        "2099-01-01T00:05:00Z",
      ),
    ).toBe(false);
  });

  it("falls back to literal equality for unparseable strings", () => {
    expect(snoozeTimestampCloseEnough("not-a-date", "not-a-date")).toBe(true);
    expect(snoozeTimestampCloseEnough("not-a-date", "also-bad")).toBe(false);
  });

  it("covers both `||` short-circuit branches when only one side is unparseable", () => {
    // Branch coverage: the `!Number.isFinite(a) || !Number.isFinite(b)`
    // check has 4 outcomes (a/b each parseable or not). The other
    // cases above hit both-finite and both-unparseable; these two
    // exercise the mixed cases so v8 sees every branch leg.
    expect(
      snoozeTimestampCloseEnough("2099-01-01T00:00:00Z", "not-a-date"),
    ).toBe(false);
    expect(
      snoozeTimestampCloseEnough("not-a-date", "2099-01-01T00:00:00Z"),
    ).toBe(false);
  });

  it("rejects exactly at the 2-minute tolerance boundary", () => {
    // Inclusive boundary at 2 minutes (= 120_000 ms). Just-over
    // counts as different snoozes; just-under counts as the same.
    expect(
      snoozeTimestampCloseEnough(
        "2099-01-01T00:00:00Z",
        "2099-01-01T00:02:00Z",
      ),
    ).toBe(true);
    expect(
      snoozeTimestampCloseEnough(
        "2099-01-01T00:00:00Z",
        "2099-01-01T00:02:00.001Z",
      ),
    ).toBe(false);
  });
});

describe("rank-based comparator with two unranked workspaces", () => {
  it("compares with `<`/`>` instead of subtraction to avoid NaN", () => {
    // Regression: two workspaces missing from the persisted ordering
    // both resolve to `Infinity`; `Infinity - Infinity` is `NaN`,
    // which `Array.prototype.sort` treats like equality and silently
    // skips the id tie-break, leaving order at the mercy of input
    // order. This test simulates the same rank-based comparator used
    // by `useRepoGroups.sortByRank` and asserts deterministic order
    // by id ascending. See #1581 CodeRabbit review.
    const rank = new Map<string, number>();
    const rankOf = (id: string) => rank.get(id) ?? Infinity;
    const cmp = (a: Workspace, b: Workspace) => {
      const ar = rankOf(a.id);
      const br = rankOf(b.id);
      if (ar < br) return -1;
      if (ar > br) return 1;
      return a.id.localeCompare(b.id);
    };
    const wsZ = workspace("z", [session({ id: "sz" })]);
    const wsA = workspace("a", [session({ id: "sa" })]);
    const wsM = workspace("m", [session({ id: "sm" })]);
    const sorted = [wsZ, wsA, wsM].sort(cmp);
    expect(sorted.map((w) => w.id)).toEqual(["a", "m", "z"]);
  });
});

describe("compareWorkspacesByLastActivityDesc triage tier", () => {
  it("sinks fully-archived workspaces below live ones regardless of activity", () => {
    // The archived workspace has the most recent activity timestamp; if
    // the comparator naively used activity only it would sort first.
    // Triage tier forces it to the bottom.
    const archived = workspace("archived-newer", [
      session({
        id: "sa",
        created_at: "2025-09-01T00:00:00Z",
        archived_at: "2025-09-02T00:00:00Z",
      }),
    ]);
    const live = workspace("live-older", [
      session({ id: "sl", created_at: "2025-01-01T00:00:00Z" }),
    ]);
    const list = [archived, live].sort(compareWorkspacesByLastActivityDesc);
    expect(list.map((w) => w.id)).toEqual(["live-older", "archived-newer"]);
  });

  it("lifts pinned workspaces above live ones regardless of activity", () => {
    // The pinned workspace has the older activity timestamp; without
    // the tier prefix the live one would sort first.
    const pinned = workspace("pinned-older", [
      session({
        id: "sp",
        created_at: "2025-01-01T00:00:00Z",
        pinned_at: "2025-01-02T00:00:00Z",
      }),
    ]);
    const live = workspace("live-newer", [
      session({ id: "sl", created_at: "2025-09-01T00:00:00Z" }),
    ]);
    const list = [live, pinned].sort(compareWorkspacesByLastActivityDesc);
    expect(list.map((w) => w.id)).toEqual(["pinned-older", "live-newer"]);
  });

  it("preserves activity order within the same tier", () => {
    const livesA = workspace("live-a", [
      session({ id: "sa", created_at: "2025-09-01T00:00:00Z" }),
    ]);
    const livesB = workspace("live-b", [
      session({ id: "sb", created_at: "2025-01-01T00:00:00Z" }),
    ]);
    const list = [livesB, livesA].sort(compareWorkspacesByLastActivityDesc);
    expect(list.map((w) => w.id)).toEqual(["live-a", "live-b"]);
  });
});

describe("triageStateOf", () => {
  it("returns 'live' when no flag is set", () => {
    expect(
      triageStateOf({ isPinned: false, isArchived: false, isSnoozed: false }),
    ).toBe("live");
  });

  it("returns 'pinned' when isPinned is set", () => {
    expect(
      triageStateOf({ isPinned: true, isArchived: false, isSnoozed: false }),
    ).toBe("pinned");
  });

  it("returns 'archived' when only archived is set", () => {
    expect(
      triageStateOf({ isPinned: false, isArchived: true, isSnoozed: false }),
    ).toBe("archived");
  });

  it("returns 'snoozed' when only snoozed is set", () => {
    expect(
      triageStateOf({ isPinned: false, isArchived: false, isSnoozed: true }),
    ).toBe("snoozed");
  });

  it("returns 'archived' when archived + snoozed are both set (no pin)", () => {
    // Branch coverage: the archived/snoozed branch combo without
    // pin. Archive wins because the data layer makes it impossible
    // for a session to be both at once, but workspace aggregators
    // can surface both via different sessions. Archive is the
    // stronger sink so the menu picks it.
    expect(
      triageStateOf({ isPinned: false, isArchived: true, isSnoozed: true }),
    ).toBe("archived");
  });

  it("prefers pinned over archived and snoozed (defensive priority)", () => {
    // The server's XOR rules make pinned + archived impossible at the
    // session level, but a multi-session workspace can still surface
    // both via the any-pinned / any-archived aggregators. The state
    // function picks pinned so the menu does not show contradictory
    // toggles.
    expect(
      triageStateOf({ isPinned: true, isArchived: true, isSnoozed: false }),
    ).toBe("pinned");
    expect(
      triageStateOf({ isPinned: true, isArchived: false, isSnoozed: true }),
    ).toBe("pinned");
  });
});

describe("triageMenuShape", () => {
  it("a live row offers Pin / Archive / Snooze and no 'Un…' toggles", () => {
    const shape = triageMenuShape("live");
    expect(shape.showPin).toBe(true);
    expect(shape.showArchive).toBe(true);
    expect(shape.showSnooze).toBe(true);
    expect(shape.showUnpin).toBe(false);
    expect(shape.showUnarchive).toBe(false);
    expect(shape.showUnsnooze).toBe(false);
  });

  it("a pinned row only offers Unpin", () => {
    // Regression: a pinned row used to show Archive and Snooze
    // alongside Unpin, letting the user trigger contradictory
    // transitions from the menu. See #1581.
    const shape = triageMenuShape("pinned");
    expect(shape.showUnpin).toBe(true);
    expect(shape.showPin).toBe(false);
    expect(shape.showArchive).toBe(false);
    expect(shape.showUnarchive).toBe(false);
    expect(shape.showSnooze).toBe(false);
    expect(shape.showUnsnooze).toBe(false);
  });

  it("an archived row only offers Unarchive", () => {
    // Regression: an archived row used to show Pin and Snooze in
    // the same menu. See #1581.
    const shape = triageMenuShape("archived");
    expect(shape.showUnarchive).toBe(true);
    expect(shape.showPin).toBe(false);
    expect(shape.showUnpin).toBe(false);
    expect(shape.showArchive).toBe(false);
    expect(shape.showSnooze).toBe(false);
    expect(shape.showUnsnooze).toBe(false);
  });

  it("a snoozed row only offers Unsnooze", () => {
    const shape = triageMenuShape("snoozed");
    expect(shape.showUnsnooze).toBe(true);
    expect(shape.showPin).toBe(false);
    expect(shape.showUnpin).toBe(false);
    expect(shape.showArchive).toBe(false);
    expect(shape.showUnarchive).toBe(false);
    expect(shape.showSnooze).toBe(false);
  });
});

describe("loadSidebarSortMode / saveSidebarSortMode", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });

  it("defaults to 'manual' when localStorage is empty", () => {
    expect(loadSidebarSortMode()).toBe("manual");
  });

  it("returns 'manual' for an unrecognised stored value", () => {
    window.localStorage.setItem(SIDEBAR_SORT_MODE_KEY, "nonsense");
    expect(loadSidebarSortMode()).toBe("manual");
  });

  it("round-trips 'lastActivity'", () => {
    saveSidebarSortMode("lastActivity");
    expect(window.localStorage.getItem(SIDEBAR_SORT_MODE_KEY)).toBe(
      "lastActivity",
    );
    expect(loadSidebarSortMode()).toBe("lastActivity");
  });

  it("round-trips 'manual'", () => {
    saveSidebarSortMode("lastActivity");
    saveSidebarSortMode("manual");
    expect(loadSidebarSortMode()).toBe("manual");
  });
});

function repoGroup(id: string, workspaces: Workspace[]): RepoGroup {
  return {
    id,
    repoPath: id,
    displayName: id,
    defaultDisplayName: id,
    alias: null,
    color: null,
    remoteOwner: null,
    workspaces,
    status: "idle",
    collapsed: false,
  };
}

describe("repoGroupHasLiveWorkspace", () => {
  it("returns true when at least one workspace is live", () => {
    const g = repoGroup("repo-a", [
      workspace("w-live", [session({})]),
      workspace("w-archived", [
        session({ id: "s-arch", archived_at: "2026-01-01T00:00:00Z" }),
      ]),
    ]);
    expect(repoGroupHasLiveWorkspace(g)).toBe(true);
  });

  it("returns false when every workspace is sunk", () => {
    const g = repoGroup("repo-sunk", [
      workspace("w-archived", [
        session({ id: "s1", archived_at: "2026-01-01T00:00:00Z" }),
      ]),
      workspace("w-snoozed", [
        session({ id: "s2", snoozed_until: "2099-01-01T00:00:00Z" }),
      ]),
    ]);
    expect(repoGroupHasLiveWorkspace(g)).toBe(false);
  });

  it("returns false for an empty workspace list", () => {
    const g = repoGroup("repo-empty", []);
    expect(repoGroupHasLiveWorkspace(g)).toBe(false);
  });

  it("flips back to live when a sunk session is unsnoozed/unarchived", () => {
    const g = repoGroup("repo-flip", [
      workspace("w", [
        session({ id: "s", archived_at: "2026-01-01T00:00:00Z" }),
      ]),
    ]);
    expect(repoGroupHasLiveWorkspace(g)).toBe(false);
    const revived: RepoGroup = {
      ...g,
      workspaces: [workspace("w", [session({ id: "s" })])],
    };
    expect(repoGroupHasLiveWorkspace(revived)).toBe(true);
  });

  it("treats a multi-session workspace with one live session as live", () => {
    const g = repoGroup("repo-mixed", [
      workspace("w-mixed", [
        session({ id: "s-live" }),
        session({ id: "s-arch", archived_at: "2026-01-01T00:00:00Z" }),
      ]),
    ]);
    expect(repoGroupHasLiveWorkspace(g)).toBe(true);
  });
});

describe("sessionAttentionRank (#1640)", () => {
  it("ranks live statuses in the TUI attention_tier order", () => {
    const rank = (status: string) =>
      sessionAttentionRank(session({ status: status as never }));
    expect(rank("Waiting")).toBe(0);
    expect(rank("Error")).toBe(1);
    expect(rank("Idle")).toBe(2);
    expect(rank("Unknown")).toBe(3);
    expect(rank("Running")).toBe(4);
    expect(rank("Stopped")).toBe(5);
    expect(rank("Starting")).toBe(6);
    expect(rank("Creating")).toBe(6);
    expect(rank("Deleting")).toBe(6);
  });

  it("sinks archived and snoozed sessions to rank 99 regardless of status", () => {
    expect(
      sessionAttentionRank(
        session({ status: "Waiting", archived_at: "2026-01-01T00:00:00Z" }),
      ),
    ).toBe(99);
    expect(
      sessionAttentionRank(
        session({ status: "Error", snoozed_until: "2999-01-01T00:00:00Z" }),
      ),
    ).toBe(99);
  });
});

describe("workspaceAttentionRank (#1640)", () => {
  it("returns the best (lowest) rank across sessions", () => {
    const ws = workspace("w", [
      session({ id: "a", status: "Running" }),
      session({ id: "b", status: "Waiting" }),
      session({ id: "c", status: "Idle" }),
    ]);
    expect(workspaceAttentionRank(ws)).toBe(0);
  });

  it("ignores a snoozed Waiting session so it does not falsely lift the workspace", () => {
    const ws = workspace("w", [
      session({ id: "live", status: "Running" }),
      session({
        id: "snoozed",
        status: "Waiting",
        snoozed_until: "2999-01-01T00:00:00Z",
      }),
    ]);
    // Running (4) wins over the sunk snoozed Waiting (99).
    expect(workspaceAttentionRank(ws)).toBe(4);
  });
});

describe("workspaceIsUrgent / workspaceIsFavorited (#1640)", () => {
  it("is urgent when any session carries the flag", () => {
    expect(
      workspaceIsUrgent(
        workspace("w", [
          session({ id: "a" }),
          session({ id: "b", urgent: true }),
        ]),
      ),
    ).toBe(true);
    expect(workspaceIsUrgent(workspace("w", [session({ id: "a" })]))).toBe(
      false,
    );
  });

  it("is favorited when any session is favorited", () => {
    expect(
      workspaceIsFavorited(
        workspace("w", [session({ id: "a", favorited: true })]),
      ),
    ).toBe(true);
    expect(workspaceIsFavorited(workspace("w", [session({ id: "a" })]))).toBe(
      false,
    );
  });
});

describe("compareWorkspacesByAttention (#1640)", () => {
  // Shared activity timestamp so the status rank, not the within-tier
  // recency key, drives ordering in the pure-rank cases.
  const TS = "2025-06-01T00:00:00Z";
  const wsStatus = (id: string, status: string) =>
    workspace(id, [
      session({ id: `${id}-s`, status: status as never, created_at: TS }),
    ]);

  function order(list: Workspace[]): string[] {
    return [...list].sort(compareWorkspacesByAttention).map((w) => w.id);
  }

  it("floats Waiting and Error above Idle, Running, and Stopped", () => {
    const list = [
      wsStatus("stopped", "Stopped"),
      wsStatus("running", "Running"),
      wsStatus("idle", "Idle"),
      wsStatus("error", "Error"),
      wsStatus("waiting", "Waiting"),
    ];
    expect(order(list)).toEqual([
      "waiting",
      "error",
      "idle",
      "running",
      "stopped",
    ]);
  });

  it("promotes an urgent workspace above all non-urgent regardless of status rank", () => {
    const urgentRunning = workspace("urgent", [
      session({ id: "u", status: "Running", urgent: true, created_at: TS }),
    ]);
    const plainWaiting = wsStatus("waiting", "Waiting");
    expect(order([plainWaiting, urgentRunning])).toEqual(["urgent", "waiting"]);
  });

  it("breaks ties within a rank by favorite, then newest activity, then id", () => {
    const favorited = workspace("fav", [
      session({ id: "f", status: "Idle", favorited: true, created_at: TS }),
    ]);
    const plain = wsStatus("plain", "Idle");
    expect(order([plain, favorited])).toEqual(["fav", "plain"]);

    const newer = workspace("newer", [
      session({ id: "n", status: "Idle", created_at: "2025-07-01T00:00:00Z" }),
    ]);
    const older = workspace("older", [
      session({ id: "o", status: "Idle", created_at: "2025-01-01T00:00:00Z" }),
    ]);
    expect(order([older, newer])).toEqual(["newer", "older"]);
  });

  it("keeps pinned at the top tier and sunk at the bottom tier", () => {
    const pinned = workspace("pinned", [
      session({ id: "p", status: "Stopped", pinned_at: TS, created_at: TS }),
    ]);
    const liveWaiting = wsStatus("waiting", "Waiting");
    const sunk = workspace("sunk", [
      session({ id: "s", status: "Waiting", archived_at: TS, created_at: TS }),
    ]);
    expect(order([liveWaiting, sunk, pinned])).toEqual([
      "pinned",
      "waiting",
      "sunk",
    ]);
  });

  it("is deterministic when both workspaces have no usable timestamp", () => {
    const a = workspace("a", [
      session({
        id: "a-s",
        status: "Idle",
        created_at: "bad",
        idle_entered_at: null,
        last_accessed_at: null,
      }),
    ]);
    const b = workspace("b", [
      session({
        id: "b-s",
        status: "Idle",
        created_at: "bad",
        idle_entered_at: null,
        last_accessed_at: null,
      }),
    ]);
    // -Infinity vs -Infinity must not poison the comparator into skipping
    // the id tie-break; "a" sorts before "b".
    expect(order([b, a])).toEqual(["a", "b"]);
  });
});

describe("compareWorkspacesForComputedSortMode (#1640)", () => {
  it("returns the attention comparator only for attention mode", () => {
    expect(compareWorkspacesForComputedSortMode("attention")).toBe(
      compareWorkspacesByAttention,
    );
    expect(compareWorkspacesForComputedSortMode("lastActivity")).toBe(
      compareWorkspacesByLastActivityDesc,
    );
    // The group/nested axes have no manual order, so manual falls back to
    // last-activity there.
    expect(compareWorkspacesForComputedSortMode("manual")).toBe(
      compareWorkspacesByLastActivityDesc,
    );
  });
});

describe("repoGroup attention helpers (#1640)", () => {
  it("repoGroupAttentionRank takes the best rank across the group", () => {
    const workspaces = [
      workspace("w1", [session({ id: "a", status: "Running" })]),
      workspace("w2", [session({ id: "b", status: "Waiting" })]),
    ];
    expect(repoGroupAttentionRank(workspaces)).toBe(0);
  });

  it("repoGroupIsUrgent is true when any workspace is urgent", () => {
    expect(
      repoGroupIsUrgent([
        workspace("w1", [session({ id: "a" })]),
        workspace("w2", [session({ id: "b", urgent: true })]),
      ]),
    ).toBe(true);
    expect(repoGroupIsUrgent([workspace("w1", [session({ id: "a" })])])).toBe(
      false,
    );
  });
});

describe("loadSidebarSortMode accepts attention (#1640)", () => {
  beforeEach(() => window.localStorage.clear());

  it("round-trips the attention mode through storage", () => {
    saveSidebarSortMode("attention");
    expect(loadSidebarSortMode()).toBe("attention");
  });

  it("falls back to manual for an unknown stored value", () => {
    window.localStorage.setItem(SIDEBAR_SORT_MODE_KEY, "bogus");
    expect(loadSidebarSortMode()).toBe("manual");
  });
});
