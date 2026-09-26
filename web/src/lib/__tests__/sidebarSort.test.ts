// @vitest-environment jsdom

import { beforeEach, describe, expect, it } from "vitest";
import type { RepoGroup, SessionResponse, SessionStatus, Workspace } from "../types";
import {
  SIDEBAR_SORT_MODE_KEY,
  compareWorkspacesByAttention,
  compareWorkspacesByLastActivityDesc,
  compareWorkspacesByPluginSort,
  compareWorkspacesForComputedSortMode,
  repoGroupPluginSortValue,
  workspacePluginSortValue,
  loadSidebarSortMode,
  nextAttentionSessionId,
  repoGroupAttentionRank,
  repoGroupHasLiveWorkspace,
  repoGroupIsUrgent,
  repoGroupLastActivityMs,
  saveSidebarSortMode,
  sessionAttentionRank,
  sessionNeedsAttention,
  snoozeTimestampCloseEnough,
  triageMenuShape,
  triageStateOf,
  workspaceAttentionCount,
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
    cleanup_defaults: { delete_worktree: false, delete_branch: false, delete_sandbox: false },
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

/** A single-session workspace whose session id is derived from `id`. */
const ws1 = (id: string, over: Partial<SessionResponse> = {}) => workspace(id, [session({ id: `${id}-s`, ...over })]);

function repoGroup(workspaces: Workspace[]): RepoGroup {
  return {
    id: "repo",
    repoPath: "repo",
    displayName: "repo",
    defaultDisplayName: "repo",
    alias: null,
    color: null,
    remoteOwner: null,
    workspaces,
    status: "idle",
    collapsed: false,
  };
}

const TS = "2025-06-01T00:00:00Z";
const archived = { archived_at: TS };
const snoozed = { snoozed_until: "2099-01-01T00:00:00Z" };
const noTimestamp = { created_at: "bad", idle_entered_at: null, last_accessed_at: null };
const ids = (list: Workspace[]) => list.map((w) => w.id);

describe("activity timestamps", () => {
  it.each<[string, Partial<SessionResponse>[], number]>([
    [
      "max across the three fields",
      [
        {
          created_at: "2025-01-01T00:00:00Z",
          idle_entered_at: "2025-03-01T00:00:00Z",
          last_accessed_at: "2025-02-01T00:00:00Z",
        },
      ],
      Date.parse("2025-03-01T00:00:00Z"),
    ],
    ["created_at fallback", [{ created_at: "2025-05-01T00:00:00Z" }], Date.parse("2025-05-01T00:00:00Z")],
    [
      "unparseable strings ignored",
      [{ created_at: "2025-04-01T00:00:00Z", idle_entered_at: "not-a-date", last_accessed_at: "also-bad" }],
      Date.parse("2025-04-01T00:00:00Z"),
    ],
    [
      "max across sessions",
      [
        { id: "s1", created_at: "2025-01-01T00:00:00Z" },
        { id: "s2", created_at: "2025-06-01T00:00:00Z" },
      ],
      Date.parse("2025-06-01T00:00:00Z"),
    ],
    ["no usable timestamp", [noTimestamp], Number.NEGATIVE_INFINITY],
  ])("workspaceLastActivityMs: %s", (_name, sessions, expected) => {
    expect(workspaceLastActivityMs(workspace("w", sessions.map(session)))).toBe(expected);
  });

  it("repoGroupLastActivityMs takes the max, or -Infinity for an empty group", () => {
    expect(repoGroupLastActivityMs([ws1("a"), ws1("b", { created_at: "2025-07-01T00:00:00Z" })])).toBe(
      Date.parse("2025-07-01T00:00:00Z"),
    );
    expect(repoGroupLastActivityMs([])).toBe(Number.NEGATIVE_INFINITY);
  });
});

describe("triage", () => {
  it.each<[string, Partial<SessionResponse>[], { pinned: boolean; sunk: boolean; tier: number }]>([
    ["live", [{}], { pinned: false, sunk: false, tier: 1 }],
    ["one pinned session", [{}, { id: "s2", pinned_at: TS }], { pinned: true, sunk: false, tier: 0 }],
    ["pinned beside archived", [{ pinned_at: TS }, { id: "s2", ...archived }], { pinned: true, sunk: false, tier: 0 }],
    ["archived and snoozed", [archived, { id: "s2", ...snoozed }], { pinned: false, sunk: true, tier: 2 }],
    ["one live beside archived", [archived, { id: "s2" }], { pinned: false, sunk: false, tier: 1 }],
    ["empty", [], { pinned: false, sunk: false, tier: 1 }],
  ])("%s workspace", (_name, sessions, { pinned, sunk, tier }) => {
    const ws = workspace("w", sessions.map(session));
    expect(workspaceIsPinned(ws)).toBe(pinned);
    expect(workspaceIsSunk(ws)).toBe(sunk);
    expect(workspaceTriageTier(ws)).toBe(tier);
  });

  it.each([
    [false, false, false, "live"],
    [false, false, true, "snoozed"],
    [false, true, true, "archived"],
    [true, true, false, "pinned"],
  ])("triageStateOf(pinned=%s, archived=%s, snoozed=%s) is %s", (isPinned, isArchived, isSnoozed, expected) => {
    expect(triageStateOf({ isPinned, isArchived, isSnoozed })).toBe(expected);
  });

  it.each([
    ["live", ["showPin", "showArchive", "showSnooze"]],
    ["pinned", ["showUnpin", "showArchive", "showSnooze"]],
    ["archived", ["showUnarchive"]],
    ["snoozed", ["showUnsnooze"]],
  ] as const)("triageMenuShape(%s) offers only %o", (state, shown) => {
    const all = ["showPin", "showUnpin", "showArchive", "showUnarchive", "showSnooze", "showUnsnooze"] as const;
    expect(triageMenuShape(state)).toMatchObject(Object.fromEntries(all.map((k) => [k, shown.includes(k as never)])));
  });

  it.each([
    ["2099-01-01T00:00:00Z", "2099-01-01T00:00:00Z", true],
    ["2099-01-01T00:00:00Z", "2099-01-01T00:02:00Z", true],
    ["2099-01-01T00:00:00Z", "2099-01-01T00:02:00.001Z", false],
    ["not-a-date", "not-a-date", true],
    ["not-a-date", "also-bad", false],
    ["2099-01-01T00:00:00Z", "not-a-date", false],
  ])("snoozeTimestampCloseEnough(%s, %s) is %s", (a, b, expected) => {
    expect(snoozeTimestampCloseEnough(a, b)).toBe(expected);
  });

  it.each<[string, Workspace[], boolean]>([
    ["one live workspace", [ws1("live"), ws1("arch", archived)], true],
    ["all sunk", [ws1("arch", archived), ws1("snz", snoozed)], false],
    ["no workspaces", [], false],
  ])("repoGroupHasLiveWorkspace with %s is %s", (_name, workspaces, expected) => {
    expect(repoGroupHasLiveWorkspace(repoGroup(workspaces))).toBe(expected);
  });
});

describe("compareWorkspacesByLastActivityDesc", () => {
  it.each<[string, Workspace[], string[]]>([
    ["newer first", [ws1("older"), ws1("newer", { created_at: "2025-09-01T00:00:00Z" })], ["newer", "older"]],
    ["id tie-break", [ws1("b"), ws1("a"), ws1("c")], ["a", "b", "c"]],
    ["id tie-break without timestamps", [ws1("b", noTimestamp), ws1("a", noTimestamp)], ["a", "b"]],
    [
      "archived sinks despite activity",
      [ws1("archived", { created_at: "2025-09-01T00:00:00Z", ...archived }), ws1("live")],
      ["live", "archived"],
    ],
    [
      "pinned floats despite activity",
      [ws1("live", { created_at: "2025-09-01T00:00:00Z" }), ws1("pinned", { pinned_at: TS })],
      ["pinned", "live"],
    ],
  ])("%s", (_name, list, expected) => {
    expect(ids([...list].sort(compareWorkspacesByLastActivityDesc))).toEqual(expected);
  });
});

describe("attention sort (#1640)", () => {
  it.each<[SessionStatus, number]>([
    ["Waiting", 0],
    ["Error", 1],
    ["Idle", 2],
    ["Unknown", 3],
    ["Running", 4],
    ["Stopped", 5],
    ["Starting", 6],
  ])("sessionAttentionRank(%s) is %s", (status, rank) => {
    expect(sessionAttentionRank(session({ status }))).toBe(rank);
  });

  it("sinks archived and snoozed sessions to rank 99", () => {
    expect(sessionAttentionRank(session({ status: "Waiting", ...archived }))).toBe(99);
    expect(sessionAttentionRank(session({ status: "Error", ...snoozed }))).toBe(99);
  });

  it("aggregates the best rank, ignoring sunk sessions", () => {
    const statuses = (list: [SessionStatus, Partial<SessionResponse>?][]) =>
      workspace(
        "w",
        list.map(([status, over], i) => session({ id: `s${i}`, status, ...over })),
      );
    expect(workspaceAttentionRank(statuses([["Running"], ["Waiting"], ["Idle"]]))).toBe(0);
    expect(workspaceAttentionRank(statuses([["Running"], ["Waiting", snoozed]]))).toBe(4);
    expect(repoGroupAttentionRank([ws1("a", { status: "Running" }), ws1("b", { status: "Waiting" })])).toBe(0);
  });

  it("urgent and favorited flags aggregate across sessions and workspaces", () => {
    expect(workspaceIsUrgent(workspace("w", [session({ id: "a" }), session({ id: "b", urgent: true })]))).toBe(true);
    expect(workspaceIsUrgent(ws1("w"))).toBe(false);
    expect(workspaceIsFavorited(ws1("w", { favorited: true }))).toBe(true);
    expect(workspaceIsFavorited(ws1("w"))).toBe(false);
    expect(repoGroupIsUrgent([ws1("a"), ws1("b", { urgent: true })])).toBe(true);
    expect(repoGroupIsUrgent([ws1("a")])).toBe(false);
  });

  const status = (id: string, s: SessionStatus, over: Partial<SessionResponse> = {}) =>
    ws1(id, { status: s, created_at: TS, ...over });

  it.each<[string, Workspace[], string[]]>([
    [
      "status rank",
      [
        status("stopped", "Stopped"),
        status("running", "Running"),
        status("idle", "Idle"),
        status("error", "Error"),
        status("waiting", "Waiting"),
      ],
      ["waiting", "error", "idle", "running", "stopped"],
    ],
    [
      "urgent across ranks",
      [status("waiting", "Waiting"), status("urgent", "Running", { urgent: true })],
      ["urgent", "waiting"],
    ],
    ["favorite within a rank", [status("plain", "Idle"), status("fav", "Idle", { favorited: true })], ["fav", "plain"]],
    [
      "newest within a rank",
      [
        status("older", "Idle", { created_at: "2025-01-01T00:00:00Z" }),
        status("newer", "Idle", { created_at: "2025-07-01T00:00:00Z" }),
      ],
      ["newer", "older"],
    ],
    [
      "pinned and sunk tiers",
      [
        status("waiting", "Waiting"),
        status("sunk", "Waiting", archived),
        status("pinned", "Stopped", { pinned_at: TS }),
      ],
      ["pinned", "waiting", "sunk"],
    ],
    ["id without timestamps", [status("b", "Idle", noTimestamp), status("a", "Idle", noTimestamp)], ["a", "b"]],
  ])("orders by %s", (_name, list, expected) => {
    expect(ids([...list].sort(compareWorkspacesByAttention))).toEqual(expected);
  });

  it("uses the attention comparator only for attention mode", () => {
    expect(compareWorkspacesForComputedSortMode("attention")).toBe(compareWorkspacesByAttention);
    expect(compareWorkspacesForComputedSortMode("lastActivity")).toBe(compareWorkspacesByLastActivityDesc);
    expect(compareWorkspacesForComputedSortMode("manual")).toBe(compareWorkspacesByLastActivityDesc);
  });
});

describe("sort mode storage", () => {
  beforeEach(() => window.localStorage.clear());

  it.each(["attention", "manual"] as const)("round-trips %s", (mode) => {
    saveSidebarSortMode(mode === "manual" ? "lastActivity" : "manual");
    saveSidebarSortMode(mode);
    expect(window.localStorage.getItem(SIDEBAR_SORT_MODE_KEY)).toBe(mode);
    expect(loadSidebarSortMode()).toBe(mode);
  });
});

describe("plugin sort (#2401)", () => {
  const wsA = workspace("a", [session({ id: "a1" }), session({ id: "a2" })]);
  const wsB = ws1("b");
  const values = new Map([
    ["a1", 30],
    ["a2", 10],
    ["b-s", 50],
  ]);

  it("takes the best value for the direction", () => {
    expect(workspacePluginSortValue(wsA, { direction: "asc", values })).toBe(10);
    expect(workspacePluginSortValue(wsA, { direction: "desc", values })).toBe(30);
    expect(workspacePluginSortValue(wsA, { direction: "asc", values: new Map() })).toBeUndefined();
    expect(repoGroupPluginSortValue([wsA, wsB], { direction: "asc", values })).toBe(10);
    expect(repoGroupPluginSortValue([wsA, wsB], { direction: "desc", values })).toBe(50);
  });

  it.each<[string, Workspace[], "asc" | "desc", string[]]>([
    ["asc", [wsB, wsA], "asc", ["a", "b"]],
    ["desc", [wsA, wsB], "desc", ["b", "a"]],
    ["unvalued sinks", [ws1("c"), wsA, wsB], "desc", ["b", "a", "c"]],
    ["tier before value", [ws1("b", archived), wsA], "desc", ["a", "b"]],
  ])("orders %s", (_name, list, direction, expected) => {
    expect(ids([...list].sort(compareWorkspacesByPluginSort({ direction, values })))).toEqual(expected);
  });
});

describe("attention badges and jump", () => {
  it.each<[Partial<SessionResponse>, boolean]>([
    [{ status: "Waiting" }, true],
    [{ status: "Error" }, true],
    [{ status: "Running", urgent: true }, true],
    [{ status: "Idle" }, false],
    [{ status: "Running" }, false],
    [{ status: "Waiting", ...archived }, false],
    [{ status: "Error", snoozed_until: "2025-01-01T00:00:00Z" }, false],
    [{ status: "Running", urgent: true, trashed_at: TS }, false],
  ])("sessionNeedsAttention(%o) is %s", (over, expected) => {
    expect(sessionNeedsAttention(session(over))).toBe(expected);
  });

  it("workspaceAttentionCount counts only attention-needing sessions", () => {
    const statuses: Partial<SessionResponse>[] = [
      { status: "Waiting" },
      { status: "Running" },
      { status: "Error" },
      { status: "Waiting", ...archived },
    ];
    expect(
      workspaceAttentionCount(
        workspace(
          "w",
          statuses.map((s, i) => session({ id: `s${i}`, ...s })),
        ),
      ),
    ).toBe(2);
    expect(workspaceAttentionCount(ws1("w"))).toBe(0);
  });

  it.each<[string[], string | null, string | null]>([
    [[], "a", null],
    [["b", "d"], "b", "d"],
    [["a", "c"], "c", "a"],
    [["d"], "b", "d"],
    [["c", "d"], null, "c"],
    [["c"], "zzz", "c"],
    [["b"], "b", "b"],
  ])("nextAttentionSessionId(%o, active=%s) is %s", (attention, active, expected) => {
    expect(nextAttentionSessionId(["a", "b", "c", "d"], new Set(attention), active)).toBe(expected);
  });
});
