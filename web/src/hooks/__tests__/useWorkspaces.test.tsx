// @vitest-environment jsdom
//
// Unit coverage for useWorkspaces: the session -> workspace grouping. Worktree
// sessions (non-null branch) collapse one-row-per-(repo, branch); plain
// sessions each get their own row (#956). Display-name derivation and the
// active/idle rollup are exercised here without mounting the sidebar.

import { describe, expect, it } from "vitest";
import { renderHook } from "@testing-library/react";

import { useWorkspaces } from "../useWorkspaces";
import type { SessionResponse } from "../../lib/types";

function session(over: Partial<SessionResponse> = {}): SessionResponse {
  return {
    id: "s1",
    title: "row title",
    project_path: "/repo",
    group_path: "/repo",
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
    ...over,
  } as SessionResponse;
}

describe("useWorkspaces", () => {
  it("returns an empty list for no sessions", () => {
    const { result } = renderHook(() => useWorkspaces([]));
    expect(result.current).toEqual([]);
  });

  it("collapses sessions on the same repo+branch into one workspace", () => {
    const { result } = renderHook(() =>
      useWorkspaces([
        session({ id: "a", branch: "feat", main_repo_path: "/repo/" }),
        session({ id: "b", branch: "feat", main_repo_path: "/repo", tool: "codex" }),
      ]),
    );
    expect(result.current).toHaveLength(1);
    const ws = result.current[0]!;
    expect(ws.sessions.map((s) => s.id).sort()).toEqual(["a", "b"]);
    // Trailing slash normalized so both sessions land in the same group.
    expect(ws.projectPath).toBe("/repo");
    expect(ws.branch).toBe("feat");
    // Dedup of agents, primaryAgent is the first.
    expect(ws.agents).toEqual(["claude", "codex"]);
    expect(ws.primaryAgent).toBe("claude");
    // Multi-session group uses branch as the display name, not the title.
    expect(ws.displayName).toBe("feat");
  });

  it("gives each branch-less session its own workspace (#956)", () => {
    const { result } = renderHook(() =>
      useWorkspaces([
        session({ id: "a", branch: null, main_repo_path: "/repo" }),
        session({ id: "b", branch: null, main_repo_path: "/repo" }),
      ]),
    );
    expect(result.current).toHaveLength(2);
  });

  it("uses the trimmed title for a single-session workspace, else falls back to repo name", () => {
    const { result } = renderHook(() =>
      useWorkspaces([session({ id: "a", branch: null, title: "  My Task  ", project_path: "/x/myrepo" })]),
    );
    expect(result.current[0]!.displayName).toBe("My Task");

    const { result: r2 } = renderHook(() =>
      useWorkspaces([session({ id: "a", branch: null, title: "   ", project_path: "/x/myrepo" })]),
    );
    expect(r2.current[0]!.displayName).toBe("myrepo");
  });

  it("rolls up to active when any session is active, else idle", () => {
    const { result } = renderHook(() =>
      useWorkspaces([
        session({ id: "a", branch: "b", status: "Idle" }),
        session({ id: "c", branch: "b", status: "Running" }),
      ]),
    );
    expect(result.current[0]!.status).toBe("active");

    const { result: r2 } = renderHook(() => useWorkspaces([session({ id: "a", branch: "b", status: "Idle" })]));
    expect(r2.current[0]!.status).toBe("idle");
  });
});
