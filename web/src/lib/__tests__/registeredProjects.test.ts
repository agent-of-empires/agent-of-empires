// @vitest-environment node
//
// Unit tests for the registry merge that brings the TUI's project-pin
// feature to the web sidebar (#2047): populated repo groups gain their
// registry entries, and registered repos with no live group are appended as
// pinned-but-empty headers, deduped by normalized path.

import { describe, expect, it } from "vitest";

import { mergeRegisteredProjects, normalizeProjectPathKey } from "../registeredProjects";
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
  return { name: path.split("/").pop() ?? path, path, scope: "global", ...over };
}

describe("normalizeProjectPathKey", () => {
  it("trims and strips trailing slashes without lowercasing", () => {
    expect(normalizeProjectPathKey("/work/Foo/ ".trim())).toBe("/work/Foo");
    expect(normalizeProjectPathKey("/work/foo/")).toBe("/work/foo");
    expect(normalizeProjectPathKey("/work/foo")).toBe("/work/foo");
    // Case is preserved: distinct repos on a case-sensitive filesystem.
    expect(normalizeProjectPathKey("/work/Foo")).not.toBe(normalizeProjectPathKey("/work/foo"));
  });
});

describe("mergeRegisteredProjects", () => {
  it("attaches the registry entry to a matching populated group (no duplicate header)", () => {
    const groups = [repoGroup("/work/alpha")];
    const merged = mergeRegisteredProjects(groups, [project("/work/alpha")]);
    expect(merged).toHaveLength(1);
    expect(merged[0]!.registeredProjects).toHaveLength(1);
    expect(merged[0]!.workspaces).toHaveLength(1);
  });

  it("matches across a trailing-slash difference", () => {
    const groups = [repoGroup("/work/alpha")];
    const merged = mergeRegisteredProjects(groups, [project("/work/alpha/")]);
    expect(merged).toHaveLength(1);
    expect(merged[0]!.registeredProjects).toHaveLength(1);
  });

  it("appends a zero-workspace group for a registered repo with no live group", () => {
    const merged = mergeRegisteredProjects([], [project("/work/beta")]);
    expect(merged).toHaveLength(1);
    expect(merged[0]!.repoPath).toBe("/work/beta");
    expect(merged[0]!.displayName).toBe("beta");
    expect(merged[0]!.workspaces).toHaveLength(0);
    expect(merged[0]!.registeredProjects).toHaveLength(1);
  });

  it("collapses a path registered under both global and profile into one group", () => {
    const merged = mergeRegisteredProjects(
      [],
      [project("/work/beta", { scope: "global" }), project("/work/beta", { scope: "profile" })],
    );
    expect(merged).toHaveLength(1);
    expect(merged[0]!.registeredProjects.map((p) => p.scope)).toEqual(["global", "profile"]);
  });

  it("leaves an unregistered populated repo untouched", () => {
    const merged = mergeRegisteredProjects([repoGroup("/work/gamma")], [project("/work/beta")]);
    const gamma = merged.find((g) => g.repoPath === "/work/gamma")!;
    expect(gamma.registeredProjects).toHaveLength(0);
  });

  it("never pins synthetic Multi-repo / Scratch buckets", () => {
    const synthetic = repoGroup(MULTI_REPO_GROUP_ID, { id: MULTI_REPO_GROUP_ID });
    const merged = mergeRegisteredProjects([synthetic], [project(MULTI_REPO_GROUP_ID)]);
    const found = merged.find((g) => g.id === MULTI_REPO_GROUP_ID)!;
    expect(found.registeredProjects).toHaveLength(0);
  });

  it("applies resolved alias/color/collapse to appended empty groups", () => {
    const merged = mergeRegisteredProjects([], [project("/work/beta")], {
      alias: () => "Beta",
      color: () => "teal",
      collapsed: () => true,
    });
    expect(merged[0]!.displayName).toBe("Beta");
    expect(merged[0]!.alias).toBe("Beta");
    expect(merged[0]!.color).toBe("teal");
    expect(merged[0]!.collapsed).toBe(true);
  });
});

describe("SidebarGroup pin derivation + render gating", () => {
  it("marks a populated registered repo pinned but not pinnedEmpty, and renders it", () => {
    const [g] = mergeRegisteredProjects([repoGroup("/work/alpha")], [project("/work/alpha")]);
    const sg = repoGroupToSidebarGroup(g!);
    expect(sg.pinned).toBe(true);
    expect(sg.pinnedEmpty).toBe(false);
    expect(sidebarGroupShouldRender(sg)).toBe(true);
  });

  it("marks an empty registered repo pinnedEmpty and renders it despite no live rows", () => {
    const [g] = mergeRegisteredProjects([], [project("/work/beta")]);
    const sg = repoGroupToSidebarGroup(g!);
    expect(sg.pinned).toBe(true);
    expect(sg.pinnedEmpty).toBe(true);
    expect(sidebarGroupShouldRender(sg)).toBe(true);
  });

  it("leaves an unregistered repo unpinned", () => {
    const sg = repoGroupToSidebarGroup(repoGroup("/work/gamma"));
    expect(sg.pinned).toBe(false);
    expect(sg.pinnedEmpty).toBe(false);
  });
});
