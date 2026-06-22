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
  archivableWorkspaces,
  buildNestedSidebarGroups,
  buildSessionGroups,
  nestedSidebarGroupHasLiveWorkspace,
  repoGroupToSidebarGroup,
  sidebarGroupHasLiveWorkspace,
  UNGROUPED_GROUP_ID,
} from "../sidebarGroups";
import { MULTI_REPO_GROUP_ID } from "../../hooks/useRepoGroups";
import type { SidebarSortMode } from "../sidebarSort";
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

function workspace(id: string, sessions: SessionResponse[], over: Partial<Workspace> = {}): Workspace {
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
  sortMode: SidebarSortMode = "lastActivity",
) =>
  buildSessionGroups(workspaces, {
    idleDecayWindowMs: IDLE_DECAY_WINDOW_MS,
    sortMode,
    isCollapsed,
  });

describe("buildSessionGroups", () => {
  it("orders within-group rows by attention when sortMode is attention (#1640)", () => {
    const groups = build(
      [
        workspace("w-running", [session({ id: "r", group_path: "feature", status: "Running" })]),
        workspace("w-waiting", [session({ id: "w", group_path: "feature", status: "Waiting" })]),
        workspace("w-idle", [session({ id: "i", group_path: "feature", status: "Idle" })]),
      ],
      () => false,
      "attention",
    );
    const feature = groups.find((g) => g.id === "feature")!;
    expect(feature.workspaces.map((v) => v.workspace.id)).toEqual(["w-waiting", "w-idle", "w-running"]);
  });

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
      workspace("w1", [session({ id: "a", group_path: "feature" }), session({ id: "b", group_path: "fix" })]),
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
    expect(feature.workspaces[0]!.workspace.sessions.map((s) => s.id)).toEqual(["a"]);
    expect(fix.workspaces[0]!.workspace.sessions.map((s) => s.id)).toEqual(["b"]);
  });

  it("trims and normalizes whitespace-only group_path into Ungrouped", () => {
    const groups = build([workspace("w1", [session({ id: "s1", group_path: "   " })])]);
    expect(groups.map((g) => g.id)).toEqual([UNGROUPED_GROUP_ID]);
  });

  it("buckets paths that differ only by leading/trailing slashes together", () => {
    const groups = build([
      workspace("w1", [session({ id: "a", group_path: "feature" })]),
      workspace("w2", [session({ id: "b", group_path: "feature/" })]),
      workspace("w3", [session({ id: "c", group_path: "/feature" })]),
    ]);
    expect(groups.map((g) => g.id)).toEqual(["feature"]);
    expect(groups[0]!.workspaces.map((v) => v.workspace.id)).toEqual(["w1", "w2", "w3"]);
  });

  it("flattens a nested path into the display name instead of truncating to the leaf", () => {
    const groups = build([workspace("w1", [session({ id: "s1", group_path: "feature/auth" })])]);
    expect(groups[0]!.id).toBe("feature/auth");
    expect(groups[0]!.displayName).toBe("feature / auth");
  });

  it("keeps sibling nested groups distinct instead of colliding on a shared leaf", () => {
    const groups = build([
      workspace("w1", [session({ id: "a", group_path: "pushforward/PRs" })]),
      workspace("w2", [session({ id: "b", group_path: "chargeunpacker/PRs" })]),
    ]);
    expect(groups.map((g) => g.displayName)).toEqual(["chargeunpacker / PRs", "pushforward / PRs"]);
  });

  it("reflects collapse state from the isCollapsed lookup", () => {
    const groups = build([workspace("w1", [session({ id: "s1", group_path: "feature" })])], (id) => id === "feature");
    expect(groups[0]!.collapsed).toBe(true);
  });

  it("session groups expose no repo-only affordances", () => {
    const groups = build([workspace("w1", [session({ id: "s1", group_path: "feature" })])]);
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
      registeredProjects: [],
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
    const sg = repoGroupToSidebarGroup(repoGroup({ id: MULTI_REPO_GROUP_ID, repoPath: MULTI_REPO_GROUP_ID }));
    expect(sg.capabilities.create).toBe("generic");
    expect(sg.capabilities.appearance).toBe(true);
  });
});

describe("buildNestedSidebarGroups", () => {
  function repoGroup(over: Partial<RepoGroup> = {}): RepoGroup {
    return {
      id: "/repo-a",
      repoPath: "/repo-a",
      displayName: "repo-a",
      defaultDisplayName: "repo-a",
      alias: null,
      color: null,
      remoteOwner: null,
      workspaces: [],
      status: "idle",
      collapsed: false,
      registeredProjects: [],
      ...over,
    };
  }

  const buildNested = (
    repoGroups: RepoGroup[],
    isSubgroupCollapsed: (repoId: string, groupPath: string) => boolean = () => false,
    sortMode: SidebarSortMode = "lastActivity",
  ) =>
    buildNestedSidebarGroups(repoGroups, {
      idleDecayWindowMs: IDLE_DECAY_WINDOW_MS,
      sortMode,
      isSubgroupCollapsed,
    });

  it("keeps the repo header and nests its user groups underneath", () => {
    const nested = buildNested([
      repoGroup({
        workspaces: [
          workspace("w1", [session({ id: "a", group_path: "feature" })]),
          workspace("w2", [session({ id: "b", group_path: "fix" })]),
        ],
      }),
    ]);
    expect(nested).toHaveLength(1);
    expect(nested[0]!.repo.kind).toBe("repo");
    expect(nested[0]!.repo.repoPath).toBe("/repo-a");
    expect(nested[0]!.subgroups.map((sg) => sg.id)).toEqual(["feature", "fix"]);
    expect(nested[0]!.subgroups.every((sg) => sg.kind === "sessionGroup")).toBe(true);
  });

  it("drops manual reorder on the repo header (nested axis has no order)", () => {
    const nested = buildNested([
      repoGroup({
        workspaces: [workspace("w1", [session({ id: "a", group_path: "x" })])],
      }),
    ]);
    expect(nested[0]!.repo.capabilities.reorder).toBe(false);
    // Other repo affordances stay intact.
    expect(nested[0]!.repo.capabilities.appearance).toBe(true);
    expect(nested[0]!.repo.capabilities.create).toBe("repo");
  });

  it("puts ungrouped sessions in an Ungrouped subgroup within the repo", () => {
    const nested = buildNested([
      repoGroup({
        workspaces: [
          workspace("w1", [session({ id: "a", group_path: "feature" })]),
          workspace("w2", [session({ id: "b", group_path: "" })]),
        ],
      }),
    ]);
    const ids = nested[0]!.subgroups.map((sg) => sg.id);
    expect(ids).toEqual(["feature", UNGROUPED_GROUP_ID]);
    const ungrouped = nested[0]!.subgroups.find((sg) => sg.id === UNGROUPED_GROUP_ID)!;
    expect(ungrouped.groupPath).toBe("");
  });

  it("slices a split workspace per subgroup with the real id preserved", () => {
    const nested = buildNested([
      repoGroup({
        workspaces: [
          workspace("w1", [session({ id: "a", group_path: "feature" }), session({ id: "b", group_path: "fix" })]),
        ],
      }),
    ]);
    const [feature, fix] = nested[0]!.subgroups;
    expect(feature!.workspaces[0]!.workspace.id).toBe("w1");
    expect(fix!.workspaces[0]!.workspace.id).toBe("w1");
    expect(feature!.workspaces[0]!.key).not.toBe(fix!.workspaces[0]!.key);
    expect(feature!.workspaces[0]!.workspace.sessions.map((s) => s.id)).toEqual(["a"]);
    expect(fix!.workspaces[0]!.workspace.sessions.map((s) => s.id)).toEqual(["b"]);
  });

  it("keys subgroup collapse on (repoId, groupPath), not group id alone", () => {
    const nested = buildNested(
      [
        repoGroup({
          id: "/repo-a",
          repoPath: "/repo-a",
          workspaces: [workspace("w1", [session({ id: "a", group_path: "feature" })])],
        }),
        repoGroup({
          id: "/repo-b",
          repoPath: "/repo-b",
          workspaces: [
            workspace("w2", [session({ id: "b", group_path: "feature" })], {
              projectPath: "/repo-b",
            }),
          ],
        }),
      ],
      (repoId, groupPath) => repoId === "/repo-a" && groupPath === "feature",
    );
    // Same group path in two repos collapses independently.
    expect(nested[0]!.subgroups[0]!.collapsed).toBe(true);
    expect(nested[1]!.subgroups[0]!.collapsed).toBe(false);
  });

  it("reports liveness from any live subgroup row", () => {
    const allSunk = buildNested([
      repoGroup({
        workspaces: [
          workspace("w1", [
            session({
              id: "a",
              group_path: "feature",
              archived_at: "2025-01-02T00:00:00Z",
            }),
          ]),
        ],
      }),
    ]);
    expect(nestedSidebarGroupHasLiveWorkspace(allSunk[0]!)).toBe(false);

    const live = buildNested([
      repoGroup({
        workspaces: [workspace("w1", [session({ id: "a", group_path: "feature" })])],
      }),
    ]);
    expect(nestedSidebarGroupHasLiveWorkspace(live[0]!)).toBe(true);
  });
});

describe("sidebarGroupHasLiveWorkspace", () => {
  it("is false when every workspace is sunk", () => {
    const groups = build([
      workspace("w1", [
        session({
          id: "s1",
          group_path: "feature",
          archived_at: "2025-01-02T00:00:00Z",
        }),
      ]),
    ]);
    expect(sidebarGroupHasLiveWorkspace(groups[0]!)).toBe(false);
  });

  it("is true when at least one workspace is live", () => {
    const groups = build([workspace("w1", [session({ id: "s1", group_path: "feature" })])]);
    expect(sidebarGroupHasLiveWorkspace(groups[0]!)).toBe(true);
  });
});

describe("archivableWorkspaces", () => {
  it("returns members whose primary session is not archived", () => {
    const groups = build([
      workspace("w-live", [session({ id: "a", group_path: "feature" })]),
      workspace("w-archived", [session({ id: "b", group_path: "feature", archived_at: "2025-01-02T00:00:00Z" })]),
    ]);
    const feature = groups.find((g) => g.id === "feature")!;
    expect(archivableWorkspaces(feature).map((ws) => ws.id)).toEqual(["w-live"]);
  });

  it("includes snoozed-but-not-archived members (archive sweeps them in)", () => {
    const groups = build([
      workspace("w-snoozed", [session({ id: "a", group_path: "feature", snoozed_until: "2999-01-01T00:00:00Z" })]),
    ]);
    const feature = groups.find((g) => g.id === "feature")!;
    expect(archivableWorkspaces(feature).map((ws) => ws.id)).toEqual(["w-snoozed"]);
  });

  it("is empty once every member is archived", () => {
    const groups = build([
      workspace("w1", [session({ id: "a", group_path: "feature", archived_at: "2025-01-02T00:00:00Z" })]),
    ]);
    const feature = groups.find((g) => g.id === "feature")!;
    expect(archivableWorkspaces(feature)).toHaveLength(0);
  });

  it("keys off the primary session, ignoring archived siblings", () => {
    // A workspace whose primary session is live is archivable even if a
    // later session is already archived; triage acts on sessions[0].
    const groups = build([
      workspace("w1", [
        session({ id: "a", group_path: "feature" }),
        session({ id: "b", group_path: "feature", archived_at: "2025-01-02T00:00:00Z" }),
      ]),
    ]);
    const feature = groups.find((g) => g.id === "feature")!;
    expect(archivableWorkspaces(feature).map((ws) => ws.id)).toEqual(["w1"]);
  });
});
