// @vitest-environment jsdom

import { renderHook, act } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";

import { useRepoGroups, MULTI_REPO_GROUP_ID, SCRATCH_GROUP_ID } from "./useRepoGroups";
import type { SessionResponse, Workspace } from "../lib/types";
import type { SidebarSortMode } from "../lib/sidebarSort";
import { session, workspace } from "./__tests__/fixtures";

const multiRepos = [
  { name: "repo-a", source_path: "/repo-a", branch: "main" },
  { name: "repo-b", source_path: "/repo-b", branch: "main" },
];
const ORDER_KEY = "aoe-repo-group-order-v1";

/** A single-session workspace; `path` defaults to `/<id's repo letter>`-style repo paths. */
const ws = (id: string, path: string, over: Partial<SessionResponse> = {}, wsOver: Partial<Workspace> = {}) =>
  workspace(id, path, [session({ id: `s-${id}`, project_path: path, group_path: path, ...over })], wsOver);
const multi = (id = "multi", over: Partial<SessionResponse> = {}) =>
  ws(id, "/repo-a", { workspace_repos: multiRepos, ...over });
const scratch = (id = "sc", over: Partial<SessionResponse> = {}) =>
  ws(id, `/home/u/.agent-of-empires/scratch/${id}`, { scratch: true, ...over });
const recent = { last_accessed_at: "2025-09-01T00:00:00Z" };

function groupsOf(workspaces: Workspace[], mode?: SidebarSortMode, ordering = workspaces.map((w) => w.id)) {
  return renderHook(() => useRepoGroups(workspaces, ordering, mode));
}
const ids = (workspaces: Workspace[], mode?: SidebarSortMode, ordering?: string[]) =>
  groupsOf(workspaces, mode, ordering).result.current.groups.map((g) => g.id);

beforeEach(() => localStorage.clear());

describe("useRepoGroups grouping", () => {
  it("groups single-repo workspaces by projectPath and pins multi-repo then scratch to the bottom", () => {
    const { groups } = groupsOf([
      ws("a1", "/repo-a"),
      ws("a2", "/repo-a"),
      ws("b1", "/repo-b"),
      multi(),
      scratch("sc1"),
      scratch("sc2"),
    ]).result.current;
    expect(groups.map((g) => [g.id, g.workspaces.map((w) => w.id)])).toEqual([
      ["/repo-a", ["a1", "a2"]],
      ["/repo-b", ["b1"]],
      [MULTI_REPO_GROUP_ID, ["multi"]],
      [SCRATCH_GROUP_ID, ["sc1", "sc2"]],
    ]);
    expect(groups[3]!.displayName).toBe("Scratch");
  });

  it("derives displayName from the last path segment and rolls up active status", () => {
    const { groups } = groupsOf([
      ws("a1", "/home/user/code/repo-x"),
      ws("a2", "/home/user/code/repo-x", {}, { status: "active" }),
    ]).result.current;
    expect(groups[0]).toMatchObject({ displayName: "repo-x", status: "active" });
  });
});

describe("useRepoGroups sort modes", () => {
  it.each<[string, SidebarSortMode, Workspace[], string[] | undefined, string[]]>([
    [
      "manual: groups by min workspace rank",
      "manual",
      [ws("a1", "/repo-a"), ws("b1", "/repo-b")],
      ["b1", "a1"],
      ["/repo-b", "/repo-a"],
    ],
    [
      "lastActivity: groups by freshest activity",
      "lastActivity",
      [ws("a1", "/repo-a"), ws("b1", "/repo-b", recent)],
      undefined,
      ["/repo-b", "/repo-a"],
    ],
    [
      "lastActivity: ties break on repoPath",
      "lastActivity",
      [ws("b1", "/repo-b"), ws("a1", "/repo-a")],
      undefined,
      ["/repo-a", "/repo-b"],
    ],
    [
      "lastActivity: synthetic groups stay at the bottom",
      "lastActivity",
      [scratch("sc", { last_accessed_at: "2025-12-31T00:00:00Z" }), multi("multi", recent), ws("real", "/repo-a")],
      undefined,
      ["/repo-a", MULTI_REPO_GROUP_ID, SCRATCH_GROUP_ID],
    ],
    [
      "attention: a Waiting group floats up",
      "attention",
      [ws("a1", "/repo-a", { status: "Running" }), ws("b1", "/repo-b", { status: "Waiting" })],
      undefined,
      ["/repo-b", "/repo-a"],
    ],
  ])("%s", (_label, mode, workspaces, ordering, expected) => {
    expect(ids(workspaces, mode, ordering)).toEqual(expected);
  });

  it.each<[SidebarSortMode, Workspace[], string[], string[]]>([
    ["manual", [ws("new", "/repo-a", recent), ws("old", "/repo-a")], ["old", "new"], ["old", "new"]],
    ["lastActivity", [ws("old", "/repo-a"), ws("new", "/repo-a", recent)], ["old", "new"], ["new", "old"]],
    [
      "attention",
      [ws("running", "/repo-a", { status: "Running" }), ws("waiting", "/repo-a", { status: "Waiting" })],
      ["running", "waiting"],
      ["waiting", "running"],
    ],
  ])("%s orders workspaces within a group", (mode, workspaces, ordering, expected) => {
    const { groups } = groupsOf(workspaces, mode, ordering).result.current;
    expect(groups[0]!.workspaces.map((w) => w.id)).toEqual(expected);
  });
});

describe("useRepoGroups manual group order (#1644)", () => {
  it.each<[string, string[], Workspace[], SidebarSortMode, string[]]>([
    [
      "the stored order wins over min rank",
      ["/repo-b", "/repo-a"],
      [ws("a1", "/repo-a"), ws("b1", "/repo-b")],
      "manual",
      ["/repo-b", "/repo-a"],
    ],
    [
      "an unstored group floats above ranked ones",
      ["/repo-b", "/repo-c"],
      [ws("a1", "/repo-a"), ws("b1", "/repo-b"), ws("c1", "/repo-c")],
      "manual",
      ["/repo-a", "/repo-b", "/repo-c"],
    ],
    [
      "untouched synthetic groups sink",
      ["/repo-a"],
      [ws("real", "/repo-a"), multi(), scratch()],
      "manual",
      ["/repo-a", MULTI_REPO_GROUP_ID, SCRATCH_GROUP_ID],
    ],
    [
      "a dragged synthetic group holds its place",
      [SCRATCH_GROUP_ID, "/repo-a"],
      [ws("real", "/repo-a"), scratch()],
      "manual",
      [SCRATCH_GROUP_ID, "/repo-a"],
    ],
    [
      "lastActivity ignores the stored order",
      ["/repo-b", "/repo-a"],
      [ws("a1", "/repo-a", recent), ws("b1", "/repo-b")],
      "lastActivity",
      ["/repo-a", "/repo-b"],
    ],
  ])("%s", (_label, stored, workspaces, mode, expected) => {
    localStorage.setItem(ORDER_KEY, JSON.stringify(stored));
    expect(ids(workspaces, mode)).toEqual(expected);
  });
});

describe("useRepoGroups stateful API", () => {
  it("toggleRepoCollapsed, updateRepoAppearance, and reorderRepoGroups persist", () => {
    const { result } = groupsOf([ws("a1", "/repo-a"), ws("b1", "/repo-b")], "manual");
    const first = () => result.current.groups[0]!;

    act(() => result.current.toggleRepoCollapsed("/repo-a"));
    expect(first().collapsed).toBe(true);
    expect(localStorage.getItem("aoe-repo-collapsed-/repo-a")).toBe("1");
    act(() => result.current.toggleRepoCollapsed("/repo-a"));
    expect(first().collapsed).toBe(false);
    expect(localStorage.getItem("aoe-repo-collapsed-/repo-a")).toBeNull();

    act(() => result.current.updateRepoAppearance("/repo-a", { alias: "pretty-name" }));
    expect(first()).toMatchObject({ displayName: "pretty-name", alias: "pretty-name" });

    act(() => result.current.reorderRepoGroups(["/repo-b", "/repo-a"]));
    expect(result.current.groups.map((g) => g.id)).toEqual(["/repo-b", "/repo-a"]);
    expect(localStorage.getItem(ORDER_KEY)).toBe(JSON.stringify(["/repo-b", "/repo-a"]));
  });
});
