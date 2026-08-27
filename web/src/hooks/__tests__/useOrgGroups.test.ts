// @vitest-environment jsdom
//
// Coverage for useOrgGroups: the org axis (#3283), keyed by remote owner. Collapse state is
// keyed on `org:${encodeURIComponent(orgId)}` for org headers and
// `repo:${encodeURIComponent(orgId)}::${encodeURIComponent(repoId)}` for
// member repos, both under the `aoe-org-group-collapsed-` prefix, distinct
// from the repo and nested-subgroup axes. Persistence runs in an effect,
// mirroring useNestedSidebarGroups.

import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { renderHook, act } from "@testing-library/react";

import { useOrgGroups } from "../useOrgGroups";
import type { RepoGroup, SessionResponse, Workspace } from "../../lib/types";

const PREFIX = "aoe-org-group-collapsed-";

function session(): SessionResponse {
  return {
    id: "s1",
    title: "t",
    project_path: "/repo",
    group_path: "",
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
    remote_owner: "acme",
    remote_owner_key: "acme@example.com",
    notify_on_waiting: null,
    notify_on_idle: null,
    notify_on_error: null,
    claude_fullscreen: false,
    workspace_repos: [],
  };
}

function workspace(): Workspace {
  return {
    id: "w1",
    branch: "feat",
    projectPath: "/repo",
    displayName: "feat",
    agents: ["claude"],
    primaryAgent: "claude",
    status: "idle",
    sessions: [session()],
  };
}

function repoGroup(over: Partial<RepoGroup> = {}): RepoGroup {
  return {
    id: "repo-1",
    repoPath: "/repo",
    displayName: "repo",
    defaultDisplayName: "repo",
    alias: null,
    color: null,
    remoteOwner: "acme",
    remoteOwnerKey: "acme@example.com",
    workspaces: [workspace()],
    status: "idle",
    collapsed: false,
    registeredProjects: [],
    ...over,
  };
}

function orgKey(): string {
  return `${PREFIX}org:${encodeURIComponent("acme@example.com")}`;
}

function repoKey(): string {
  return `${PREFIX}repo:${encodeURIComponent("acme@example.com")}::${encodeURIComponent("repo-1")}`;
}

beforeEach(() => localStorage.clear());
afterEach(() => localStorage.clear());

describe("useOrgGroups", () => {
  it("buckets repos under their org", () => {
    const { result } = renderHook(() => useOrgGroups([repoGroup()]));
    expect(result.current.groups).toHaveLength(1);
    expect(result.current.groups[0]!.org.id).toBe("acme@example.com");
    expect(result.current.groups[0]!.repos.map((r) => r.id)).toEqual(["repo-1"]);
  });

  it("reads initial org and repo collapse from their encoded keys", () => {
    localStorage.setItem(orgKey(), "1");
    const { result } = renderHook(() => useOrgGroups([repoGroup()]));
    expect(result.current.groups[0]!.org.collapsed).toBe(true);
    expect(result.current.groups[0]!.repos[0]!.collapsed).toBe(false);
  });

  it("toggles the org header and persists, then clears on toggle back", () => {
    const { result } = renderHook(() => useOrgGroups([repoGroup()]));

    act(() => result.current.toggleOrgCollapsed("acme@example.com"));
    expect(localStorage.getItem(orgKey())).toBe("1");
    expect(result.current.groups[0]!.org.collapsed).toBe(true);

    act(() => result.current.toggleOrgCollapsed("acme@example.com"));
    expect(localStorage.getItem(orgKey())).toBeNull();
    expect(result.current.groups[0]!.org.collapsed).toBe(false);
  });

  it("toggles a repo within an org independently of the org header", () => {
    const { result } = renderHook(() => useOrgGroups([repoGroup(), repoGroup({ id: "repo-2", repoPath: "/repo-2" })]));

    act(() => result.current.toggleRepoCollapsed("acme@example.com", "repo-1"));
    expect(localStorage.getItem(repoKey())).toBe("1");
    const repos = result.current.groups[0]!.repos;
    expect(repos.find((r) => r.id === "repo-1")!.collapsed).toBe(true);
    expect(repos.find((r) => r.id === "repo-2")!.collapsed).toBe(false);
    expect(result.current.groups[0]!.org.collapsed).toBe(false);

    act(() => result.current.toggleRepoCollapsed("acme@example.com", "repo-1"));
    expect(localStorage.getItem(repoKey())).toBeNull();
    expect(result.current.groups[0]!.repos.find((r) => r.id === "repo-1")!.collapsed).toBe(false);
  });
});
