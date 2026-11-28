// @vitest-environment node

import { describe, expect, it } from "vitest";

import {
  archivableWorkspaces,
  buildNestedSidebarGroups,
  buildSessionGroups,
  nestedSidebarGroupHasLiveWorkspace,
  repoGroupToSidebarGroup,
  sidebarGroupHasLiveWorkspace,
  UNGROUPED_GROUP_ID,
  type SidebarGroup,
} from "../sidebarGroups";
import { MULTI_REPO_GROUP_ID, SCRATCH_GROUP_ID } from "../../hooks/useRepoGroups";
import { compareWorkspacesByCustomOrder, compareWorkspacesByName } from "../sidebarSort";
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
    lifecycle_generation: 0,
    created_at: "2025-01-01T00:00:00Z",
    last_accessed_at: null,
    idle_entered_at: null,
    last_error: null,
    branch: null,
    main_repo_path: null,
    is_sandboxed: false,
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
    scratch: false,
    ...over,
  };
}

function workspace(id: string, sessions: SessionResponse[]): Workspace {
  return {
    id,
    branch: null,
    projectPath: "/repo-a",
    displayName: id,
    agents: ["claude"],
    primaryAgent: "claude",
    status: "idle",
    sessions,
  };
}

/** A one-session workspace in `group_path`. */
const ws = (id: string, group_path: string, over: Partial<SessionResponse> = {}) =>
  workspace(id, [session({ id: `${id}-s`, group_path, ...over })]);

function repoGroup(id: string, over: Partial<RepoGroup> = {}): RepoGroup {
  return {
    id,
    repoPath: id,
    displayName: id,
    remoteOwner: null,
    remoteOwnerKey: over.remoteOwner ? `${over.remoteOwner}@example.com` : null,
    workspaces: [ws(`${id}-w`, "")],
    status: "idle",
    collapsed: false,
    registeredProjects: [],
    ...over,
  };
}

const archived = { archived_at: "2025-01-02T00:00:00Z" };
const wsIds = (group: SidebarGroup) => group.workspaces.map((v) => v.workspace.id);

// Insertion order stands in for the shared Custom order.
const inInputOrder = (workspaces: Workspace[]) => compareWorkspacesByCustomOrder(workspaces.map((w) => w.id));

const build = (
  workspaces: Workspace[],
  isCollapsed: (id: string) => boolean = () => false,
  compareWorkspace = inInputOrder(workspaces),
) => buildSessionGroups(workspaces, { idleDecayWindowMs: IDLE_DECAY_WINDOW_MS, compareWorkspace, isCollapsed });

describe("buildSessionGroups", () => {
  it("orders rows by the comparator it is given", () => {
    const workspaces = [ws("w-b", "feature", { title: "beta" }), ws("w-a", "feature", { title: "Alpha" })];
    expect(wsIds(build(workspaces)[0]!)).toEqual(["w-b", "w-a"]);
    expect(wsIds(build(workspaces, () => false, compareWorkspacesByName)[0]!)).toEqual(["w-a", "w-b"]);
  });

  it("buckets by group_path, A to Z ignoring case, with Ungrouped first as in the TUI", () => {
    const groups = build([ws("w1", "refactor"), ws("w0", ""), ws("w2", "Feature"), ws("w3", "   ")]);
    expect(groups.map((g) => g.id)).toEqual([UNGROUPED_GROUP_ID, "Feature", "refactor"]);
    expect(groups.every((g) => g.kind === "sessionGroup")).toBe(true);
    expect(groups[1]!.groupPath).toBe("Feature");
    expect(groups[0]).toMatchObject({ displayName: "Ungrouped", groupPath: "" });
    expect(wsIds(groups[0]!)).toEqual(["w0", "w3"]);
  });

  it("splits a workspace whose sessions span groups, keeping its real id", () => {
    const [feature, fix] = build([
      workspace("w1", [session({ id: "a", group_path: "feature" }), session({ id: "b", group_path: "fix" })]),
    ]);
    expect([feature!.id, fix!.id]).toEqual(["feature", "fix"]);
    expect([wsIds(feature!), wsIds(fix!)]).toEqual([["w1"], ["w1"]]);
    expect(feature!.workspaces[0]!.key).not.toBe(fix!.workspaces[0]!.key);
    expect(feature!.workspaces[0]!.workspace.sessions.map((s) => s.id)).toEqual(["a"]);
    expect(fix!.workspaces[0]!.workspace.sessions.map((s) => s.id)).toEqual(["b"]);
  });

  it("buckets paths differing only by surrounding slashes together", () => {
    const groups = build([ws("w1", "feature"), ws("w2", "feature/"), ws("w3", "/feature")]);
    expect(groups.map((g) => g.id)).toEqual(["feature"]);
    expect(wsIds(groups[0]!)).toEqual(["w1", "w2", "w3"]);
  });

  it("shows the full nested path so sibling leaves stay distinct", () => {
    const groups = build([ws("w1", "pushforward/PRs"), ws("w2", "chargeunpacker/PRs")]);
    expect(groups.map((g) => [g.id, g.displayName])).toEqual([
      ["chargeunpacker/PRs", "chargeunpacker / PRs"],
      ["pushforward/PRs", "pushforward / PRs"],
    ]);
  });

  it("reflects collapse state and exposes no repo-only affordances", () => {
    const [group] = build([ws("w1", "feature")], (id) => id === "feature");
    expect(group!.collapsed).toBe(true);
    expect(group!.capabilities).toEqual({ create: "generic" });
  });

  it("reports liveness", () => {
    expect(sidebarGroupHasLiveWorkspace(build([ws("w1", "feature", archived)])[0]!)).toBe(false);
    expect(sidebarGroupHasLiveWorkspace(build([ws("w1", "feature")])[0]!)).toBe(true);
  });
});

describe("repoGroupToSidebarGroup", () => {
  it("maps a real repo with repo capabilities and id keys", () => {
    const sg = repoGroupToSidebarGroup(repoGroup("/repo-a"));
    expect(sg).toMatchObject({ kind: "repo", repoPath: "/repo-a" });
    expect(sg.capabilities).toEqual({ create: "repo" });
    expect(sg.workspaces[0]).toMatchObject({ key: "/repo-a-w", workspace: { id: "/repo-a-w" } });
  });

  it("gives synthetic buckets a generic create action", () => {
    expect(repoGroupToSidebarGroup(repoGroup(MULTI_REPO_GROUP_ID)).capabilities).toMatchObject({
      create: "generic",
    });
  });
});

describe("buildNestedSidebarGroups", () => {
  const buildNested = (
    repoGroups: RepoGroup[],
    isSubgroupCollapsed: (repoId: string, groupPath: string) => boolean = () => false,
  ) =>
    buildNestedSidebarGroups(repoGroups, {
      idleDecayWindowMs: IDLE_DECAY_WINDOW_MS,
      compareWorkspace: compareWorkspacesByName,
      isSubgroupCollapsed,
    });

  it("nests user groups under the repo header, ungrouped rows first", () => {
    const [nested] = buildNested([
      repoGroup("/repo-a", { workspaces: [ws("w1", "feature"), ws("w2", ""), ws("w3", "fix")] }),
    ]);
    expect(nested!.repo).toMatchObject({ kind: "repo", repoPath: "/repo-a" });
    expect(nested!.repo.capabilities).toEqual({ create: "repo" });
    expect(nested!.subgroups.map((sg) => [sg.id, sg.kind, sg.groupPath])).toEqual([
      [UNGROUPED_GROUP_ID, "sessionGroup", ""],
      ["feature", "sessionGroup", "feature"],
      ["fix", "sessionGroup", "fix"],
    ]);
  });

  it("keeps Scratch sessions out of Groups, as the TUI does", () => {
    const [scratch] = buildNested([repoGroup(SCRATCH_GROUP_ID, { workspaces: [ws("w1", "feature"), ws("w2", "")] })]);
    expect(scratch!.subgroups.map((sg) => [sg.id, wsIds(sg)])).toEqual([[UNGROUPED_GROUP_ID, ["w1", "w2"]]]);
  });

  it("keys subgroup collapse on (repoId, groupPath)", () => {
    const nested = buildNested(
      [
        repoGroup("/repo-a", { workspaces: [ws("w1", "feature")] }),
        repoGroup("/repo-b", { workspaces: [ws("w2", "feature")] }),
      ],
      (repoId, groupPath) => repoId === "/repo-a" && groupPath === "feature",
    );
    expect(nested.map((n) => n.subgroups[0]!.collapsed)).toEqual([true, false]);
  });

  it("reports liveness from any live subgroup row", () => {
    const [sunk, live] = buildNested([
      repoGroup("/a", { workspaces: [ws("w1", "feature", archived)] }),
      repoGroup("/b", { workspaces: [ws("w2", "feature")] }),
    ]);
    expect(nestedSidebarGroupHasLiveWorkspace(sunk!)).toBe(false);
    expect(nestedSidebarGroupHasLiveWorkspace(live!)).toBe(true);
  });
});

describe("archivableWorkspaces", () => {
  it.each<[string, Workspace[], string[]]>([
    ["skips archived primaries", [ws("w-live", "feature"), ws("w-archived", "feature", archived)], ["w-live"]],
    [
      "includes snoozed members",
      [ws("w-snoozed", "feature", { snoozed_until: "2999-01-01T00:00:00Z" })],
      ["w-snoozed"],
    ],
    [
      "keys off the primary session",
      [
        workspace("w1", [
          session({ id: "a", group_path: "feature" }),
          session({ id: "b", group_path: "feature", ...archived }),
        ]),
      ],
      ["w1"],
    ],
  ])("%s", (_name, workspaces, expected) => {
    expect(archivableWorkspaces(build(workspaces)[0]!).map((w) => w.id)).toEqual(expected);
  });
});
