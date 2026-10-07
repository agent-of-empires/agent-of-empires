// @vitest-environment node
//
// Unit tests for the registry merge that brings the TUI's project-pin
// feature to the web sidebar (#2047): populated repo groups gain their
// registry entries, and registered repos with no live group are appended as
// pinned-but-empty headers, deduped by normalized path.

import { describe, expect, it } from "vitest";

import { mergeRegisteredProjects, normalizeProjectPathKey, unpinnedSavedProjects } from "../registeredProjects";
import { repoGroupToSidebarGroup, sidebarGroupShouldRender } from "../sidebarGroups";
import { MULTI_REPO_GROUP_ID } from "../../hooks/useRepoGroups";
import type { ProjectInfo, RepoGroup, Workspace } from "../types";

function workspace(repoPath: string): Workspace {
  return {
    id: `${repoPath}::w`,
    branch: null,
    projectPath: repoPath,
    displayName: "w",
    agents: ["claude"],
    primaryAgent: "claude",
    status: "idle",
    sessions: [],
  };
}

function repoGroup(repoPath: string, over: Partial<RepoGroup> = {}): RepoGroup {
  return {
    id: repoPath,
    repoPath,
    displayName: repoPath.split("/").pop() ?? repoPath,
    defaultDisplayName: repoPath.split("/").pop() ?? repoPath,
    alias: null,
    color: null,
    remoteOwner: null,
    workspaces: [workspace(repoPath)],
    status: "idle",
    collapsed: false,
    registeredProjects: [],
    ...over,
  };
}

function project(path: string, over: Partial<ProjectInfo> = {}): ProjectInfo {
  return { name: path.split("/").pop() ?? path, path, scope: "global", pinned: true, ...over };
}

it("normalizeProjectPathKey trims and strips trailing slashes without lowercasing", () => {
  expect(["/work/foo/", "/work/foo", " /work/Foo "].map(normalizeProjectPathKey)).toEqual([
    "/work/foo",
    "/work/foo",
    "/work/Foo",
  ]);
});

const summary = (groups: RepoGroup[]) =>
  groups.map((g) => [g.repoPath, g.workspaces.length, g.registeredProjects.map((p) => p.scope)]);

describe("mergeRegisteredProjects", () => {
  it.each<[string, RepoGroup[], ProjectInfo[], unknown[]]>([
    [
      "attaches to a matching populated group",
      [repoGroup("/work/alpha")],
      [project("/work/alpha")],
      [["/work/alpha", 1, ["global"]]],
    ],
    [
      "matches across a trailing slash",
      [repoGroup("/work/alpha")],
      [project("/work/alpha/")],
      [["/work/alpha", 1, ["global"]]],
    ],
    ["appends an empty group for a pinned repo", [], [project("/work/beta")], [["/work/beta", 0, ["global"]]]],
    ["appends nothing for an unpinned repo", [], [project("/work/saved", { pinned: false })], []],
    [
      "appends when any registration is pinned, collapsing scopes",
      [],
      [project("/work/beta", { pinned: false }), project("/work/beta", { scope: "profile" })],
      [["/work/beta", 0, ["global", "profile"]]],
    ],
    [
      "attaches an unpinned entry to a populated group",
      [repoGroup("/work/alpha")],
      [project("/work/alpha", { pinned: false })],
      [["/work/alpha", 1, ["global"]]],
    ],
    [
      "leaves an unregistered repo untouched",
      [repoGroup("/work/gamma")],
      [project("/work/beta")],
      [
        ["/work/gamma", 1, []],
        ["/work/beta", 0, ["global"]],
      ],
    ],
  ])("%s", (_name, groups, projects, expected) => {
    expect(summary(mergeRegisteredProjects(groups, projects))).toEqual(expected);
  });

  it("never attaches registrations to synthetic buckets", () => {
    const merged = mergeRegisteredProjects([repoGroup(MULTI_REPO_GROUP_ID)], [project(MULTI_REPO_GROUP_ID)]);
    expect(merged.find((g) => g.id === MULTI_REPO_GROUP_ID)!.registeredProjects).toEqual([]);
  });

  it("applies resolved alias/color/collapse to appended groups", () => {
    const [g] = mergeRegisteredProjects([], [project("/work/beta")], {
      alias: () => "Beta",
      color: () => "teal",
      collapsed: () => true,
    });
    expect(g).toMatchObject({ displayName: "Beta", alias: "Beta", color: "teal", collapsed: true });
  });
});

it.each<[string, RepoGroup[], ProjectInfo[], boolean, boolean]>([
  ["a populated pinned repo", [repoGroup("/work/alpha")], [project("/work/alpha")], true, false],
  ["an empty pinned repo", [], [project("/work/beta")], true, true],
  ["an unregistered repo", [repoGroup("/work/gamma")], [], false, false],
  ["a populated unpinned repo", [repoGroup("/work/alpha")], [project("/work/alpha", { pinned: false })], false, false],
])("SidebarGroup pin state for %s", (_name, groups, projects, pinned, pinnedEmpty) => {
  const sg = repoGroupToSidebarGroup(mergeRegisteredProjects(groups, projects)[0]!);
  expect([sg.pinned, sg.pinnedEmpty, sidebarGroupShouldRender(sg)]).toEqual([pinned, pinnedEmpty, true]);
});

describe("unpinnedSavedProjects (#2212)", () => {
  const unpinned = (path: string, scope: ProjectInfo["scope"] = "global") => project(path, { pinned: false, scope });
  const allSunk = repoGroup("/work/alpha", {
    workspaces: [
      {
        ...workspace("/work/alpha"),
        sessions: [{ archived_at: "2026-01-01T00:00:00Z" } as Workspace["sessions"][number]],
      },
    ],
  });

  it.each<[string, RepoGroup[], ProjectInfo[], unknown[]]>([
    ["includes an unpinned project without a group", [], [unpinned("/work/saved")], [["/work/saved", 0, ["global"]]]],
    ["excludes a pinned project", [], [project("/work/pinned")], []],
    ["excludes a project with a live session", [repoGroup("/work/alpha")], [unpinned("/work/alpha")], []],
    [
      "includes a project whose sessions are all sunk",
      [allSunk],
      [unpinned("/work/alpha")],
      [["/work/alpha", 0, ["global"]]],
    ],
    [
      "collapses scopes and sorts by name",
      [],
      [unpinned("/work/zeta"), unpinned("/work/alpha"), unpinned("/work/alpha", "profile")],
      [
        ["/work/alpha", 0, ["global", "profile"]],
        ["/work/zeta", 0, ["global"]],
      ],
    ],
  ])("%s", (_name, groups, projects, expected) => {
    expect(summary(unpinnedSavedProjects(groups, projects))).toEqual(expected);
  });

  it("applies resolved alias/color", () => {
    const [g] = unpinnedSavedProjects([], [unpinned("/work/beta")], { alias: () => "Beta", color: () => "teal" });
    expect(g).toMatchObject({ displayName: "Beta", color: "teal" });
  });
});
