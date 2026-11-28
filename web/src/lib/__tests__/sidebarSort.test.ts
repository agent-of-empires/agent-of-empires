// @vitest-environment jsdom

import { describe, expect, it } from "vitest";
import type { RepoGroup, SessionResponse, SessionStatus, Workspace } from "../types";
import {
  compareWorkspacesByCustomOrder,
  compareWorkspacesByName,
  compareWorkspacesByPluginSort,
  repoGroupPluginSortValue,
  workspacePluginSortValue,
  nextAttentionSessionId,
  repoGroupHasLiveWorkspace,
  repoGroupLastActivityMs,
  sessionNeedsAttention,
  snoozeTimestampCloseEnough,
  triageMenuShape,
  triageStateOf,
  workspaceAttentionCount,
  workspaceIsPinned,
  workspaceIsSunk,
  workspaceLastActivityMs,
  workspaceTriageTier,
} from "../sidebarSort";
import type { PluginUiEntry } from "../api";
import { pluginAttentionSessionIds } from "../pluginUi";

function session(over: Partial<SessionResponse> = {}): SessionResponse {
  return {
    id: "s1",
    title: "t",
    project_path: "/p",
    group_path: "/p",
    tool: "claude",
    status: "Idle",
    yolo_mode: false,
    lifecycle_generation: 0,
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
    remoteOwner: null,
    workspaces,
    status: "idle",
    collapsed: false,
  };
}

const TS = "2025-06-01T00:00:00Z";
const archived = { archived_at: TS };
const snoozed = { snoozed_until: "2099-01-01T00:00:00Z" };
const ignored = { ignored_at: TS };
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
    ["ignored", [ignored], { pinned: false, sunk: false, tier: 1 }],
    ["one live beside archived", [archived, { id: "s2" }], { pinned: false, sunk: false, tier: 1 }],
    ["one live beside ignored", [ignored, { id: "s2" }], { pinned: false, sunk: false, tier: 1 }],
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

describe("Name sort", () => {
  const titled = (id: string, title: string, over: Partial<SessionResponse> = {}) => ws1(id, { title, ...over });
  it.each<[string, Workspace[], string[]]>([
    ["A to Z ignoring case", [titled("1", "beta"), titled("2", "Alpha"), titled("3", "gamma")], ["2", "1", "3"]],
    ["id when titles tie", [titled("b", "same"), titled("a", "Same")], ["a", "b"]],
    [
      "pinned first, sunk last",
      [titled("z", "zeta", { pinned_at: TS }), titled("a", "alpha", archived), titled("m", "mu")],
      ["z", "m", "a"],
    ],
  ])("orders %s", (_name, list, expected) => {
    expect(ids([...list].sort(compareWorkspacesByName))).toEqual(expected);
  });

  it("orders a multi-session workspace by its branch", () => {
    const multi = { ...workspace("m", [session({ id: "m1", title: "zzz" }), session({ id: "m2" })]), branch: "aaa" };
    expect(ids([titled("b", "bbb"), multi].sort(compareWorkspacesByName))).toEqual(["m", "b"]);
  });
});

describe("Custom sort", () => {
  it("follows the shared order, with workspaces it does not list yet on top, newest first", () => {
    const list = [
      ws1("ranked-b"),
      ws1("ranked-a"),
      ws1("new-old", { created_at: "2025-01-01T00:00:00Z" }),
      ws1("new-new", { created_at: "2025-09-01T00:00:00Z" }),
      ws1("pinned", { pinned_at: TS }),
    ];
    const compare = compareWorkspacesByCustomOrder(["ranked-a", "pinned", "ranked-b"]);
    expect(ids([...list].sort(compare))).toEqual(["pinned", "new-new", "new-old", "ranked-a", "ranked-b"]);
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
    [{ status: "Idle", unread: true }, true],
    [{ status: "Idle", unread: true, ...archived }, false],
  ])("sessionNeedsAttention(%o) is %s with the unread indicator on", (over, expected) => {
    expect(sessionNeedsAttention(session(over), undefined, true)).toBe(expected);
  });

  it("leaves an unread session out while the unread indicator is off", () => {
    expect(sessionNeedsAttention(session({ status: "Idle", unread: true }))).toBe(false);
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

describe("plugin tone attention (#4446)", () => {
  const entry = (sessionId: string | undefined, tone: unknown, slot: PluginUiEntry["slot"] = "row-column") =>
    ({
      plugin_id: "gh",
      slot,
      id: "pr",
      session_id: sessionId,
      payload: tone === undefined ? { text: "PR" } : { text: "PR", tone },
    }) as PluginUiEntry;
  const attentionOf = (...entries: PluginUiEntry[]) => pluginAttentionSessionIds(entries);

  it.each<[string, unknown, SessionStatus, Partial<SessionResponse>, boolean]>([
    ["danger on an idle session", "danger", "Idle", {}, true],
    ["warn on an idle session", "warn", "Idle", {}, true],
    ["danger on a running session", "danger", "Running", {}, true],
    ["info", "info", "Idle", {}, false],
    ["success", "success", "Idle", {}, false],
    ["neutral", "neutral", "Idle", {}, false],
    ["a missing tone", undefined, "Idle", {}, false],
    ["an unknown tone", "purple", "Idle", {}, false],
    ["danger but archived", "danger", "Idle", archived, false],
    ["warn but snoozed", "warn", "Running", snoozed, false],
  ])("plugin tone: %s", (_name, tone, status, over, needs) => {
    const s = session({ id: "s1", status, ...over });
    expect(sessionNeedsAttention(s, attentionOf(entry("s1", tone)))).toBe(needs);
  });

  it("only counts a session-scoped row-column entry for that session", () => {
    const attention = attentionOf(
      entry("other", "danger"),
      entry("s1", "danger", "row-badge"),
      entry("s1", "danger", "detail-badge"),
      entry(undefined, "danger"),
    );
    expect([...attention]).toEqual(["other"]);
    expect(sessionNeedsAttention(session({ id: "s1" }), attention)).toBe(false);
  });

  it("counts any plugin and column id, not one plugin", () => {
    const other = { ...entry("s1", "warn"), plugin_id: "other", id: "ci" };
    expect(sessionNeedsAttention(session({ id: "s1" }), attentionOf(other))).toBe(true);
  });

  it("keeps one danger entry even when another entry on the session is calm", () => {
    const attention = attentionOf(entry("s1", "success"), { ...entry("s1", "danger"), id: "ci" });
    expect(sessionNeedsAttention(session({ id: "s1" }), attention)).toBe(true);
  });

  it("counts toned sessions in the workspace badge, skipping sunk and trashed ones", () => {
    const sessions = [
      session({ id: "a" }),
      session({ id: "b", status: "Running" }),
      session({ id: "c", ...archived }),
      session({ id: "d" }),
      session({ id: "e", trashed_at: TS }),
    ];
    const attention = attentionOf(
      entry("a", "danger"),
      entry("b", "warn"),
      entry("c", "danger"),
      entry("d", "info"),
      entry("e", "danger"),
    );
    expect(workspaceAttentionCount(workspace("w", sessions), attention)).toBe(2);
    expect(workspaceAttentionCount(workspace("w", sessions))).toBe(0);
  });
});
