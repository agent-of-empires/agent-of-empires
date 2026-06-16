// @vitest-environment jsdom
//
// Coverage for useSessionGroups: it adapts the user-group axis from
// buildSessionGroups and owns the collapse toggle. The toggle updater stays
// pure (no storage IO inside the setState callback, for the StrictMode
// double-invoke reason documented on the hook); persistence runs in an effect
// against the `aoe-group-collapsed-` key prefix.

import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { renderHook, act } from "@testing-library/react";

import { useSessionGroups } from "../useSessionGroups";
import type { SessionResponse, Workspace } from "../../lib/types";

const PREFIX = "aoe-group-collapsed-";

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

function workspace(group_path = "team-a"): Workspace {
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

beforeEach(() => localStorage.clear());
afterEach(() => localStorage.clear());

describe("useSessionGroups", () => {
  it("builds a group per user group_path", () => {
    const { result } = renderHook(() => useSessionGroups([workspace("team-a")], "lastActivity"));
    expect(result.current.groups).toHaveLength(1);
    expect(result.current.groups[0]!.collapsed).toBe(false);
  });

  it("reads initial collapsed state from storage", () => {
    const { result: probe } = renderHook(() => useSessionGroups([workspace("team-a")], "lastActivity"));
    const id = probe.current.groups[0]!.id;
    localStorage.setItem(`${PREFIX}${id}`, "1");

    const { result } = renderHook(() => useSessionGroups([workspace("team-a")], "lastActivity"));
    expect(result.current.groups[0]!.collapsed).toBe(true);
  });

  it("toggles collapse and persists to localStorage, then clears on toggle back", () => {
    const { result } = renderHook(() => useSessionGroups([workspace("team-a")], "lastActivity"));
    const id = result.current.groups[0]!.id;

    act(() => result.current.toggleGroupCollapsed(id));
    expect(result.current.groups[0]!.collapsed).toBe(true);
    expect(localStorage.getItem(`${PREFIX}${id}`)).toBe("1");

    act(() => result.current.toggleGroupCollapsed(id));
    expect(result.current.groups[0]!.collapsed).toBe(false);
    expect(localStorage.getItem(`${PREFIX}${id}`)).toBeNull();
  });
});
