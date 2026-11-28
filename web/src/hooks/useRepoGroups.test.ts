// @vitest-environment jsdom

import { renderHook, act } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";

import { useRepoGroups, MULTI_REPO_GROUP_ID, SCRATCH_GROUP_ID } from "./useRepoGroups";
import type { SessionResponse, Workspace } from "../lib/types";
import type { SidebarSort } from "../lib/sidebarView";
import { session, workspace } from "./__tests__/fixtures";

const multiRepos = [
  { name: "repo-a", source_path: "/repo-a", branch: "main" },
  { name: "repo-b", source_path: "/repo-b", branch: "main" },
];

/** A single-session workspace; `path` defaults to `/<id's repo letter>`-style repo paths. */
const ws = (id: string, path: string, over: Partial<SessionResponse> = {}, wsOver: Partial<Workspace> = {}) =>
  workspace(id, path, [session({ id: `s-${id}`, project_path: path, group_path: path, ...over })], wsOver);
const multi = (id = "multi", over: Partial<SessionResponse> = {}) =>
  ws(id, "/repo-a", { workspace_repos: multiRepos, ...over });
const scratch = (id = "sc", over: Partial<SessionResponse> = {}) =>
  ws(id, `/home/u/.agent-of-empires/scratch/${id}`, { scratch: true, ...over });
const newer = (day: number) => ({ created_at: `2025-0${day}-01T00:00:00Z` });

function groupsOf(
  workspaces: Workspace[],
  sort: SidebarSort = "custom",
  workspaceOrdering = workspaces.map((w) => w.id),
  repositoryOrder: string[] = [],
) {
  return renderHook(() => useRepoGroups(workspaces, { sort, workspaceOrdering, repositoryOrder }));
}
const ids = (workspaces: Workspace[], sort?: SidebarSort, repositoryOrder: string[] = []) =>
  groupsOf(workspaces, sort, undefined, repositoryOrder).result.current.groups.map((g) => g.id);

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

describe("useRepoGroups header order", () => {
  it.each<[string, SidebarSort, Workspace[], string[], string[]]>([
    [
      "Custom: the shared repository order",
      "custom",
      [ws("a1", "/repo-a"), ws("b1", "/repo-b")],
      ["/repo-b", "/repo-a"],
      ["/repo-b", "/repo-a"],
    ],
    [
      "Custom: headers never moved come first, newest first",
      "custom",
      [ws("a1", "/repo-a", newer(1)), ws("b1", "/repo-b", newer(2)), ws("c1", "/repo-c", newer(3))],
      ["/repo-a"],
      ["/repo-c", "/repo-b", "/repo-a"],
    ],
    [
      "Name: A to Z by header name, ignoring case",
      "name",
      [ws("b1", "/x/Beta"), ws("a1", "/x/alpha"), ws("c1", "/x/gamma")],
      ["/x/gamma", "/x/Beta"],
      ["/x/alpha", "/x/Beta", "/x/gamma"],
    ],
    [
      "synthetic groups stay at the bottom under every sort",
      "name",
      [scratch("sc"), multi("aaa"), ws("real", "/repo-z")],
      [SCRATCH_GROUP_ID, MULTI_REPO_GROUP_ID],
      ["/repo-z", MULTI_REPO_GROUP_ID, SCRATCH_GROUP_ID],
    ],
  ])("%s", (_label, sort, workspaces, repositoryOrder, expected) => {
    expect(ids(workspaces, sort, repositoryOrder)).toEqual(expected);
  });

  it.each<[SidebarSort, string[]]>([
    ["custom", ["zed", "abe"]],
    ["name", ["abe", "zed"]],
  ])("%s orders workspaces within a group", (sort, expected) => {
    const workspaces = [ws("zed", "/repo-a", { title: "zed" }), ws("abe", "/repo-a", { title: "abe" })];
    expect(groupsOf(workspaces, sort).result.current.groups[0]!.workspaces.map((w) => w.id)).toEqual(expected);
  });
});

describe("useRepoGroups stateful API", () => {
  it("toggleRepoCollapsed persists", () => {
    const { result } = groupsOf([ws("a1", "/repo-a"), ws("b1", "/repo-b")], "custom", undefined, [
      "/repo-a",
      "/repo-b",
    ]);
    const first = () => result.current.groups[0]!;

    act(() => result.current.toggleRepoCollapsed("/repo-a"));
    expect(first().collapsed).toBe(true);
    expect(localStorage.getItem("aoe-repo-collapsed-/repo-a")).toBe("1");
    act(() => result.current.toggleRepoCollapsed("/repo-a"));
    expect(first().collapsed).toBe(false);
    expect(localStorage.getItem("aoe-repo-collapsed-/repo-a")).toBeNull();
  });
});
