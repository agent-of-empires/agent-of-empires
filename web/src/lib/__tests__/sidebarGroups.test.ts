// @vitest-environment node
//
// Unit tests for the sidebar group view-model (#1234). The render path
// consumes SidebarGroup; these tests pin the two builders that produce it:
// repoGroupToSidebarGroup (repo axis adapter) and buildSessionGroups (the
// user-group axis). The load-bearing case is the per-session split: a
// workspace whose sessions span groups must render once per group with a
// sliced session set, a distinct render key, and the real workspace id
// preserved for actions.

import { describe, expect, it } from "vitest";

import {
  buildSessionGroups,
  repoGroupToSidebarGroup,
  sidebarGroupHasLiveWorkspace,
  UNGROUPED_GROUP_ID,
} from "../sidebarGroups";
import { MULTI_REPO_GROUP_ID } from "../../hooks/useRepoGroups";
import { IDLE_DECAY_WINDOW_MS } from "../session";
import type { RepoGroup, SessionResponse, Workspace } from "../types";

function session(over: Partial<SessionResponse> = {}): SessionResponse {
  return {
    id: "s1",
    title: "t",
    project_path: "/repo-a",
    group_path: "",
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
    scratch: false,
    ...over,
  };
}

function workspace(
  id: string,
  sessions: SessionResponse[],
  over: Partial<Workspace> = {},
): Workspace {
  return {
    id,
    branch: null,
    projectPath: "/repo-a",
    displayName: id,
    agents: ["claude"],
    primaryAgent: "claude",
    status: "idle",
    sessions,
    ...over,
  };
}

const build = (
  workspaces: Workspace[],
  isCollapsed: (id: string) => boolean = () => false,
) =>
  buildSessionGroups(workspaces, {
    idleDecayWindowMs: IDLE_DECAY_WINDOW_MS,
    isCollapsed,
  });

describe("buildSessionGroups", () => {
  it("buckets workspaces by group_path, named groups alphabetical", () => {
    const groups = build([
      workspace("w1", [session({ id: "s1", group_path: "refactor" })]),
      workspace("w2", [session({ id: "s2", group_path: "feature" })]),
    ]);
    expect(groups.map((g) => g.id)).toEqual(["feature", "refactor"]);
    expect(groups.every((g) => g.kind === "sessionGroup")).toBe(true);
    expect(groups[0]!.groupPath).toBe("feature");
  });

  it("collects empty group_path into Ungrouped, pinned to the bottom", () => {
    const groups = build([
      workspace("w1", [session({ id: "s1", group_path: "" })]),
      workspace("w2", [session({ id: "s2", group_path: "feature" })]),
    ]);
    expect(groups.map((g) => g.id)).toEqual(["feature", UNGROUPED_GROUP_ID]);
    const ungrouped = groups.find((g) => g.id === UNGROUPED_GROUP_ID)!;
    expect(ungrouped.displayName).toBe("Ungrouped");
    expect(ungrouped.groupPath).toBe("");
  });

  it("splits a workspace whose sessions span groups, slicing sessions", () => {
    const groups = build([
      workspace("w1", [
        session({ id: "a", group_path: "feature" }),
        session({ id: "b", group_path: "fix" }),
      ]),
    ]);
    expect(groups.map((g) => g.id)).toEqual(["feature", "fix"]);

    const feature = groups.find((g) => g.id === "feature")!;
    const fix = groups.find((g) => g.id === "fix")!;
    expect(feature.workspaces).toHaveLength(1);
    expect(fix.workspaces).toHaveLength(1);

    // Real workspace id preserved for actions; render keys distinct.
    expect(feature.workspaces[0]!.workspace.id).toBe("w1");
    expect(fix.workspaces[0]!.workspace.id).toBe("w1");
    expect(feature.workspaces[0]!.key).not.toBe(fix.workspaces[0]!.key);

    // Each view carries only its group's sessions.
    expect(feature.workspaces[0]!.workspace.sessions.map((s) => s.id)).toEqual([
      "a",
    ]);
    expect(fix.workspaces[0]!.workspace.sessions.map((s) => s.id)).toEqual([
      "b",
    ]);
  });

  it("trims and normalizes whitespace-only group_path into Ungrouped", () => {
    const groups = build([
      workspace("w1", [session({ id: "s1", group_path: "   " })]),
    ]);
    expect(groups.map((g) => g.id)).toEqual([UNGROUPED_GROUP_ID]);
  });

  it("buckets paths that differ only by leading/trailing slashes together", () => {
    const groups = build([
      workspace("w1", [session({ id: "a", group_path: "feature" })]),
      workspace("w2", [session({ id: "b", group_path: "feature/" })]),
      workspace("w3", [session({ id: "c", group_path: "/feature" })]),
    ]);
    expect(groups.map((g) => g.id)).toEqual(["feature"]);
    expect(groups[0]!.workspaces.map((v) => v.workspace.id)).toEqual([
      "w1",
      "w2",
      "w3",
    ]);
  });

  it("uses the leaf segment of a nested path as the display name", () => {
    const groups = build([
      workspace("w1", [session({ id: "s1", group_path: "feature/auth" })]),
    ]);
    expect(groups[0]!.id).toBe("feature/auth");
    expect(groups[0]!.displayName).toBe("auth");
  });

  it("reflects collapse state from the isCollapsed lookup", () => {
    const groups = build(
      [workspace("w1", [session({ id: "s1", group_path: "feature" })])],
      (id) => id === "feature",
    );
    expect(groups[0]!.collapsed).toBe(true);
  });

  it("session groups expose no repo-only affordances", () => {
    const groups = build([
      workspace("w1", [session({ id: "s1", group_path: "feature" })]),
    ]);
    expect(groups[0]!.capabilities).toEqual({
      appearance: false,
      reorder: false,
      create: "generic",
    });
  });
});

describe("repoGroupToSidebarGroup", () => {
  function repoGroup(over: Partial<RepoGroup> = {}): RepoGroup {
    return {
      id: "/repo-a",
      repoPath: "/repo-a",
      displayName: "repo-a",
      defaultDisplayName: "repo-a",
      alias: null,
      color: null,
      remoteOwner: null,
      workspaces: [workspace("w1", [session({ id: "s1" })])],
      status: "idle",
      collapsed: false,
      ...over,
    };
  }

  it("maps a real repo group with repo capabilities and id-based keys", () => {
    const sg = repoGroupToSidebarGroup(repoGroup());
    expect(sg.kind).toBe("repo");
    expect(sg.repoPath).toBe("/repo-a");
    expect(sg.capabilities).toEqual({
      appearance: true,
      reorder: true,
      create: "repo",
    });
    expect(sg.workspaces[0]!.key).toBe("w1");
    expect(sg.workspaces[0]!.workspace.id).toBe("w1");
  });

  it("gives synthetic repo buckets a generic create action", () => {
    const sg = repoGroupToSidebarGroup(
      repoGroup({ id: MULTI_REPO_GROUP_ID, repoPath: MULTI_REPO_GROUP_ID }),
    );
    expect(sg.capabilities.create).toBe("generic");
    expect(sg.capabilities.appearance).toBe(true);
  });
});

describe("sidebarGroupHasLiveWorkspace", () => {
  it("is false when every workspace is sunk", () => {
    const groups = build([
      workspace("w1", [
        session({ id: "s1", group_path: "feature", archived_at: "2025-01-02T00:00:00Z" }),
      ]),
    ]);
    expect(sidebarGroupHasLiveWorkspace(groups[0]!)).toBe(false);
  });

  it("is true when at least one workspace is live", () => {
    const groups = build([
      workspace("w1", [session({ id: "s1", group_path: "feature" })]),
    ]);
    expect(sidebarGroupHasLiveWorkspace(groups[0]!)).toBe(true);
  });
});
