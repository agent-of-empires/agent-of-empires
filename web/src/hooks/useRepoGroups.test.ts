// @vitest-environment jsdom
//
// Hook tests for useRepoGroups (#1418). The hook owns three orthogonal
// concerns:
//   1. Grouping workspaces by repo (and pulling multi-repo workspaces
//      into a synthetic group).
//   2. Sorting workspaces inside each group, plus sorting groups across
//      the sidebar. Branches on sortMode.
//   3. Collapsed + appearance state plumbed through localStorage.
//
// The sort comparator's edge cases (null timestamps, tie-breakers) are
// covered by sidebarSort.test.ts. These tests assert the integration:
// manual mode preserves the #1171 rank-driven order, lastActivity mode
// flips it, the multi-repo group stays pinned in both modes, and the
// stateful API (toggleRepoCollapsed, updateRepoAppearance) round-trips
// through localStorage.

import { renderHook, act } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";

import {
  useRepoGroups,
  MULTI_REPO_GROUP_ID,
  SCRATCH_GROUP_ID,
} from "./useRepoGroups";
import type {
  SessionResponse,
  Workspace,
  WorkspaceRepoSummary,
} from "../lib/types";

function session(over: Partial<SessionResponse> = {}): SessionResponse {
  return {
    id: "s1",
    title: "t",
    project_path: "/repo-a",
    group_path: "/repo-a",
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
  projectPath: string,
  sessions: SessionResponse[],
  over: Partial<Workspace> = {},
): Workspace {
  return {
    id,
    branch: null,
    projectPath,
    displayName: id,
    agents: ["claude"],
    primaryAgent: "claude",
    status: "idle",
    sessions,
    ...over,
  };
}

const multiRepos: WorkspaceRepoSummary[] = [
  { name: "repo-a", source_path: "/repo-a", branch: "main" },
  { name: "repo-b", source_path: "/repo-b", branch: "main" },
];

beforeEach(() => {
  window.localStorage.clear();
});

describe("useRepoGroups grouping", () => {
  it("groups single-repo workspaces by projectPath and pins multi-repo to the bottom", () => {
    const wA1 = workspace("a1", "/repo-a", [
      session({ id: "s-a1", created_at: "2025-01-01T00:00:00Z" }),
    ]);
    const wA2 = workspace("a2", "/repo-a", [
      session({ id: "s-a2", created_at: "2025-02-01T00:00:00Z" }),
    ]);
    const wB1 = workspace("b1", "/repo-b", [
      session({ id: "s-b1", created_at: "2025-03-01T00:00:00Z" }),
    ]);
    const wMulti = workspace("multi", "/repo-a", [
      session({
        id: "s-multi",
        created_at: "2025-04-01T00:00:00Z",
        workspace_repos: multiRepos,
      }),
    ]);

    const { result } = renderHook(() =>
      useRepoGroups([wA1, wA2, wB1, wMulti], ["a1", "a2", "b1", "multi"]),
    );

    const groups = result.current.groups;
    expect(groups.map((g) => g.id)).toEqual([
      "/repo-a",
      "/repo-b",
      MULTI_REPO_GROUP_ID,
    ]);
    expect(groups[0].workspaces.map((w) => w.id)).toEqual(["a1", "a2"]);
    expect(groups[1].workspaces.map((w) => w.id)).toEqual(["b1"]);
    expect(groups[2].workspaces.map((w) => w.id)).toEqual(["multi"]);
  });

  it("derives group displayName from the last path segment", () => {
    const w = workspace("a1", "/home/user/code/repo-x", [session()]);
    const { result } = renderHook(() => useRepoGroups([w], ["a1"]));
    expect(result.current.groups[0].displayName).toBe("repo-x");
  });

  it("buckets scratch workspaces into a synthetic Scratch group pinned below multi-repo", () => {
    // Three scratch sessions land in three different scratch dirs
    // (`<app_dir>/scratch/<id>`); without grouping each would render
    // as its own one-session group. Assert they collapse into a
    // single Scratch bucket, and that ordering is: real → multi-repo
    // → scratch.
    const wReal = workspace("real", "/repo-a", [session({ id: "s-real" })]);
    const wMulti = workspace("multi", "/repo-a", [
      session({
        id: "s-multi",
        workspace_repos: multiRepos,
      }),
    ]);
    const wScratch1 = workspace("sc1", "/home/u/.agent-of-empires/scratch/aaa", [
      session({ id: "s-sc1", scratch: true }),
    ]);
    const wScratch2 = workspace("sc2", "/home/u/.agent-of-empires/scratch/bbb", [
      session({ id: "s-sc2", scratch: true }),
    ]);

    const { result } = renderHook(() =>
      useRepoGroups(
        [wReal, wMulti, wScratch1, wScratch2],
        ["real", "multi", "sc1", "sc2"],
      ),
    );

    const groups = result.current.groups;
    expect(groups.map((g) => g.id)).toEqual([
      "/repo-a",
      MULTI_REPO_GROUP_ID,
      SCRATCH_GROUP_ID,
    ]);
    expect(groups[2].displayName).toBe("Scratch");
    expect(groups[2].workspaces.map((w) => w.id)).toEqual(["sc1", "sc2"]);
  });

  it("marks a group active when any workspace is active", () => {
    const wIdle = workspace(
      "a1",
      "/repo-a",
      [session({ id: "s1" })],
      { status: "idle" },
    );
    const wActive = workspace(
      "a2",
      "/repo-a",
      [session({ id: "s2" })],
      { status: "active" },
    );
    const { result } = renderHook(() =>
      useRepoGroups([wIdle, wActive], ["a1", "a2"]),
    );
    expect(result.current.groups[0].status).toBe("active");
  });
});

describe("useRepoGroups sortMode = manual (#1171 behaviour)", () => {
  it("orders workspaces inside a group by their position in workspaceOrdering", () => {
    const wNew = workspace("new", "/repo-a", [
      session({ id: "s-new", created_at: "2025-09-01T00:00:00Z" }),
    ]);
    const wOld = workspace("old", "/repo-a", [
      session({ id: "s-old", created_at: "2025-01-01T00:00:00Z" }),
    ]);
    // Server pins old first even though new is newer.
    const { result } = renderHook(() =>
      useRepoGroups([wNew, wOld], ["old", "new"], "manual"),
    );
    expect(result.current.groups[0].workspaces.map((w) => w.id)).toEqual([
      "old",
      "new",
    ]);
  });

  it("orders groups by the minimum rank of their workspaces", () => {
    const wA = workspace("a1", "/repo-a", [session({ id: "s-a" })]);
    const wB = workspace("b1", "/repo-b", [session({ id: "s-b" })]);
    // b1 has the lower rank, so /repo-b group renders first.
    const { result } = renderHook(() =>
      useRepoGroups([wA, wB], ["b1", "a1"], "manual"),
    );
    expect(result.current.groups.map((g) => g.id)).toEqual([
      "/repo-b",
      "/repo-a",
    ]);
  });
});

describe("useRepoGroups sortMode = lastActivity (#1418)", () => {
  it("orders workspaces inside a group by recency descending, ignoring workspaceOrdering", () => {
    const wOld = workspace("old", "/repo-a", [
      session({ id: "s-old", created_at: "2025-01-01T00:00:00Z" }),
    ]);
    const wNew = workspace("new", "/repo-a", [
      session({
        id: "s-new",
        created_at: "2025-03-01T00:00:00Z",
        last_accessed_at: "2025-09-01T00:00:00Z",
      }),
    ]);
    // Server ordering pins old first; lastActivity overrides.
    const { result } = renderHook(() =>
      useRepoGroups([wOld, wNew], ["old", "new"], "lastActivity"),
    );
    expect(result.current.groups[0].workspaces.map((w) => w.id)).toEqual([
      "new",
      "old",
    ]);
  });

  it("orders groups by the most-recent activity timestamp across each group's workspaces", () => {
    const wA = workspace("a1", "/repo-a", [
      session({ id: "s-a", created_at: "2025-01-01T00:00:00Z" }),
    ]);
    const wB = workspace("b1", "/repo-b", [
      session({
        id: "s-b",
        created_at: "2025-01-01T00:00:00Z",
        last_accessed_at: "2025-09-01T00:00:00Z",
      }),
    ]);
    const { result } = renderHook(() =>
      useRepoGroups([wA, wB], ["a1", "b1"], "lastActivity"),
    );
    // /repo-b's max activity is newer, so it floats above /repo-a.
    expect(result.current.groups.map((g) => g.id)).toEqual([
      "/repo-b",
      "/repo-a",
    ]);
  });

  it("falls back to repoPath alphabetical when two groups have identical activity", () => {
    const ts = "2025-05-01T00:00:00Z";
    const wA = workspace("a1", "/repo-a", [
      session({ id: "s-a", created_at: ts }),
    ]);
    const wB = workspace("b1", "/repo-b", [
      session({ id: "s-b", created_at: ts }),
    ]);
    const { result } = renderHook(() =>
      useRepoGroups([wB, wA], ["b1", "a1"], "lastActivity"),
    );
    expect(result.current.groups.map((g) => g.id)).toEqual([
      "/repo-a",
      "/repo-b",
    ]);
  });

  it("keeps the scratch group pinned at the very bottom (below multi-repo) regardless of recency", () => {
    const wReal = workspace("real", "/repo-a", [
      session({ id: "s-real", created_at: "2025-01-01T00:00:00Z" }),
    ]);
    const wMulti = workspace("multi", "/repo-a", [
      session({
        id: "s-multi",
        created_at: "2025-01-01T00:00:00Z",
        workspace_repos: multiRepos,
      }),
    ]);
    const wScratch = workspace("sc", "/home/u/.agent-of-empires/scratch/aaa", [
      session({
        id: "s-sc",
        created_at: "2025-01-01T00:00:00Z",
        last_accessed_at: "2025-12-31T00:00:00Z",
        scratch: true,
      }),
    ]);

    const { result } = renderHook(() =>
      useRepoGroups(
        [wScratch, wMulti, wReal],
        ["sc", "multi", "real"],
        "lastActivity",
      ),
    );

    expect(result.current.groups.map((g) => g.id)).toEqual([
      "/repo-a",
      MULTI_REPO_GROUP_ID,
      SCRATCH_GROUP_ID,
    ]);
  });

  it("keeps the multi-repo group pinned at the bottom even when its activity is the freshest", () => {
    const wSingle = workspace("single", "/repo-a", [
      session({ id: "s-single", created_at: "2025-01-01T00:00:00Z" }),
    ]);
    const wMulti = workspace("multi", "/repo-a", [
      session({
        id: "s-multi",
        created_at: "2025-01-01T00:00:00Z",
        last_accessed_at: "2025-12-31T00:00:00Z",
        workspace_repos: multiRepos,
      }),
    ]);
    const { result } = renderHook(() =>
      useRepoGroups([wMulti, wSingle], ["multi", "single"], "lastActivity"),
    );
    expect(result.current.groups.map((g) => g.id)).toEqual([
      "/repo-a",
      MULTI_REPO_GROUP_ID,
    ]);
  });
});

describe("useRepoGroups stateful API", () => {
  it("toggleRepoCollapsed flips collapsed and round-trips to localStorage", () => {
    const w = workspace("a1", "/repo-a", [session()]);
    const { result, rerender } = renderHook(() =>
      useRepoGroups([w], ["a1"]),
    );
    expect(result.current.groups[0].collapsed).toBe(false);

    act(() => {
      result.current.toggleRepoCollapsed("/repo-a");
    });
    rerender();
    expect(result.current.groups[0].collapsed).toBe(true);
    expect(window.localStorage.getItem("aoe-repo-collapsed-/repo-a")).toBe(
      "1",
    );

    act(() => {
      result.current.toggleRepoCollapsed("/repo-a");
    });
    rerender();
    expect(result.current.groups[0].collapsed).toBe(false);
    expect(
      window.localStorage.getItem("aoe-repo-collapsed-/repo-a"),
    ).toBeNull();
  });

  it("updateRepoAppearance applies an alias and surfaces it in displayName", () => {
    const w = workspace("a1", "/repo-a", [session()]);
    const { result, rerender } = renderHook(() =>
      useRepoGroups([w], ["a1"]),
    );
    expect(result.current.groups[0].displayName).toBe("repo-a");

    act(() => {
      result.current.updateRepoAppearance("/repo-a", { alias: "pretty-name" });
    });
    rerender();

    expect(result.current.groups[0].displayName).toBe("pretty-name");
    expect(result.current.groups[0].alias).toBe("pretty-name");
  });
});

describe("useRepoGroups manual group order (#1644)", () => {
  const ORDER_KEY = "aoe-repo-group-order-v1";

  it("orders groups by the persisted group order ahead of the min-rank fallback", () => {
    const wA = workspace("a1", "/repo-a", [session({ id: "s-a" })]);
    const wB = workspace("b1", "/repo-b", [session({ id: "s-b" })]);
    // Min-rank would put /repo-a first (a1 ranked before b1); the stored
    // group order flips it.
    window.localStorage.setItem(
      ORDER_KEY,
      JSON.stringify(["/repo-b", "/repo-a"]),
    );
    const { result } = renderHook(() =>
      useRepoGroups([wA, wB], ["a1", "b1"], "manual"),
    );
    expect(result.current.groups.map((g) => g.id)).toEqual([
      "/repo-b",
      "/repo-a",
    ]);
  });

  it("floats a group absent from the stored order above ranked groups", () => {
    const wA = workspace("a1", "/repo-a", [session({ id: "s-a" })]);
    const wB = workspace("b1", "/repo-b", [session({ id: "s-b" })]);
    const wC = workspace("c1", "/repo-c", [session({ id: "s-c" })]);
    // /repo-a is unranked in the stored order and a1 has the worst rank,
    // so only "unknown sorts first" can put it at the top.
    window.localStorage.setItem(
      ORDER_KEY,
      JSON.stringify(["/repo-b", "/repo-c"]),
    );
    const { result } = renderHook(() =>
      useRepoGroups([wA, wB, wC], ["b1", "c1", "a1"], "manual"),
    );
    expect(result.current.groups.map((g) => g.id)).toEqual([
      "/repo-a",
      "/repo-b",
      "/repo-c",
    ]);
  });

  it("defaults untouched synthetic groups to the bottom (multi-repo above scratch)", () => {
    const wReal = workspace("real", "/repo-a", [session({ id: "s-real" })]);
    const wMulti = workspace("multi", "/repo-a", [
      session({ id: "s-multi", workspace_repos: multiRepos }),
    ]);
    const wScratch = workspace("sc", "/home/u/.agent-of-empires/scratch/aaa", [
      session({ id: "s-sc", scratch: true }),
    ]);
    // Stored order only ranks the real group; the synthetic groups have
    // no stored position and sink to their default bottom.
    window.localStorage.setItem(ORDER_KEY, JSON.stringify(["/repo-a"]));
    const { result } = renderHook(() =>
      useRepoGroups([wReal, wMulti, wScratch], ["real", "multi", "sc"], "manual"),
    );
    expect(result.current.groups.map((g) => g.id)).toEqual([
      "/repo-a",
      MULTI_REPO_GROUP_ID,
      SCRATCH_GROUP_ID,
    ]);
  });

  it("lets a synthetic group hold an explicit dragged position above a real group", () => {
    const wReal = workspace("real", "/repo-a", [session({ id: "s-real" })]);
    const wScratch = workspace("sc", "/home/u/.agent-of-empires/scratch/aaa", [
      session({ id: "s-sc", scratch: true }),
    ]);
    // User dragged scratch above the real group; the stored order ranks
    // it first, so it must render first rather than snapping to bottom.
    window.localStorage.setItem(
      ORDER_KEY,
      JSON.stringify([SCRATCH_GROUP_ID, "/repo-a"]),
    );
    const { result } = renderHook(() =>
      useRepoGroups([wReal, wScratch], ["real", "sc"], "manual"),
    );
    expect(result.current.groups.map((g) => g.id)).toEqual([
      SCRATCH_GROUP_ID,
      "/repo-a",
    ]);
  });

  it("ignores the stored group order in lastActivity mode", () => {
    const wA = workspace("a1", "/repo-a", [
      session({
        id: "s-a",
        created_at: "2025-01-01T00:00:00Z",
        last_accessed_at: "2025-09-01T00:00:00Z",
      }),
    ]);
    const wB = workspace("b1", "/repo-b", [
      session({ id: "s-b", created_at: "2025-01-01T00:00:00Z" }),
    ]);
    // Stored order asks for /repo-b first; lastActivity must override and
    // float /repo-a (fresher) to the top.
    window.localStorage.setItem(
      ORDER_KEY,
      JSON.stringify(["/repo-b", "/repo-a"]),
    );
    const { result } = renderHook(() =>
      useRepoGroups([wA, wB], ["a1", "b1"], "lastActivity"),
    );
    expect(result.current.groups.map((g) => g.id)).toEqual([
      "/repo-a",
      "/repo-b",
    ]);
  });

  it("reorderRepoGroups applies the new order and round-trips to localStorage", () => {
    const wA = workspace("a1", "/repo-a", [session({ id: "s-a" })]);
    const wB = workspace("b1", "/repo-b", [session({ id: "s-b" })]);
    const { result, rerender } = renderHook(() =>
      useRepoGroups([wA, wB], ["a1", "b1"], "manual"),
    );
    expect(result.current.groups.map((g) => g.id)).toEqual([
      "/repo-a",
      "/repo-b",
    ]);

    act(() => {
      result.current.reorderRepoGroups(["/repo-b", "/repo-a"]);
    });
    rerender();

    expect(result.current.groups.map((g) => g.id)).toEqual([
      "/repo-b",
      "/repo-a",
    ]);
    expect(window.localStorage.getItem(ORDER_KEY)).toBe(
      JSON.stringify(["/repo-b", "/repo-a"]),
    );
  });
});

describe("useRepoGroups sortMode = attention (#1640)", () => {
  it("orders workspaces inside a group by status attention, ignoring workspaceOrdering", () => {
    const wRunning = workspace("running", "/repo-a", [
      session({ id: "s-run", status: "Running" }),
    ]);
    const wWaiting = workspace("waiting", "/repo-a", [
      session({ id: "s-wait", status: "Waiting" }),
    ]);
    // Server pins running first; attention floats the Waiting workspace.
    const { result } = renderHook(() =>
      useRepoGroups([wRunning, wWaiting], ["running", "waiting"], "attention"),
    );
    expect(result.current.groups[0].workspaces.map((w) => w.id)).toEqual([
      "waiting",
      "running",
    ]);
  });

  it("floats a group holding a Waiting session above one whose best is Running", () => {
    const wA = workspace("a1", "/repo-a", [
      session({ id: "s-a", status: "Running" }),
    ]);
    const wB = workspace("b1", "/repo-b", [
      session({ id: "s-b", status: "Waiting" }),
    ]);
    const { result } = renderHook(() =>
      useRepoGroups([wA, wB], ["a1", "b1"], "attention"),
    );
    expect(result.current.groups.map((g) => g.id)).toEqual([
      "/repo-b",
      "/repo-a",
    ]);
  });

  it("keeps synthetic groups pinned at the bottom even with an urgent session", () => {
    const wReal = workspace("real", "/repo-a", [
      session({ id: "s-real", status: "Idle" }),
    ]);
    const wScratch = workspace(
      "sc",
      "/home/u/.agent-of-empires/scratch/aaa",
      [session({ id: "s-sc", status: "Waiting", urgent: true, scratch: true })],
    );
    const { result } = renderHook(() =>
      useRepoGroups([wScratch, wReal], ["sc", "real"], "attention"),
    );
    // Scratch stays bottom for stable cross-toggle placement, matching
    // lastActivity, even though it holds an urgent Waiting session.
    expect(result.current.groups.map((g) => g.id)).toEqual([
      "/repo-a",
      SCRATCH_GROUP_ID,
    ]);
  });
});
