// @vitest-environment jsdom
//
// Coverage for useNestedSidebarGroups: the (repo, subgroup) axis. Collapse
// state is keyed on encodeURIComponent(repoId)::encodeURIComponent(groupPath)
// under the `aoe-nested-group-collapsed-` prefix, distinct from both the repo
// and flat-group axes (#1720). Persistence runs in an effect, mirroring
// useSessionGroups.

import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { renderHook, act } from "@testing-library/react";

import { useNestedSidebarGroups } from "../useNestedSidebarGroups";
import type { RepoGroup, SessionResponse, Workspace } from "../../lib/types";

const PREFIX = "aoe-nested-group-collapsed-";

function session(group_path: string): SessionResponse {
  return {
    id: "s1",
    title: "t",
    project_path: "/repo",
    group_path,
    tool: "claude",
    status: "Idle",
    yolo_mode: false,
    created_at: "2025-01-01T00:00:00Z",
    last_accessed_at: null,
    idle_entered_at: null,
    last_error: null,
    branch: "feat",
    main_repo_path: "/repo",
    is_sandboxed: false,
    favorited: false,
    scratch: false,
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
  } as SessionResponse;
}

function workspace(group_path: string): Workspace {
  return {
    id: "w1",
    branch: "feat",
    projectPath: "/repo",
    displayName: "feat",
    agents: ["claude"],
    primaryAgent: "claude",
    status: "idle",
    sessions: [session(group_path)],
  } as Workspace;
}

function repoGroup(): RepoGroup {
  return {
    id: "repo-1",
    repoPath: "/repo",
    displayName: "repo",
    defaultDisplayName: "repo",
    alias: null,
    color: null,
    remoteOwner: null,
    workspaces: [workspace("team-a")],
    status: "idle",
    collapsed: false,
    registeredProjects: [],
  };
}

function key(groupPath: string): string {
  return `${PREFIX}${encodeURIComponent("repo-1")}::${encodeURIComponent(groupPath)}`;
}

beforeEach(() => localStorage.clear());
afterEach(() => localStorage.clear());

describe("useNestedSidebarGroups", () => {
  it("nests subgroups under each repo", () => {
    const { result } = renderHook(() => useNestedSidebarGroups([repoGroup()], "lastActivity"));
    expect(result.current.groups).toHaveLength(1);
    expect(result.current.groups[0]!.repo.id).toBe("repo-1");
    expect(result.current.groups[0]!.subgroups.length).toBeGreaterThan(0);
    // Repo manual reorder is dropped on the nested axis.
    expect(result.current.groups[0]!.repo.capabilities.reorder).toBe(false);
  });

  it("reads initial subgroup collapse from the encoded key", () => {
    localStorage.setItem(key("team-a"), "1");
    const { result } = renderHook(() => useNestedSidebarGroups([repoGroup()], "lastActivity"));
    expect(result.current.groups[0]!.subgroups[0]!.collapsed).toBe(true);
  });

  it("toggles a subgroup and persists, then clears on toggle back", () => {
    const { result } = renderHook(() => useNestedSidebarGroups([repoGroup()], "lastActivity"));

    act(() => result.current.toggleSubgroupCollapsed("repo-1", "team-a"));
    expect(localStorage.getItem(key("team-a"))).toBe("1");
    expect(result.current.groups[0]!.subgroups[0]!.collapsed).toBe(true);

    act(() => result.current.toggleSubgroupCollapsed("repo-1", "team-a"));
    expect(localStorage.getItem(key("team-a"))).toBeNull();
    expect(result.current.groups[0]!.subgroups[0]!.collapsed).toBe(false);
  });
});
