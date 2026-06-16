// Vitest coverage for `mergeRecentProjects` (#2141): folding the persisted
// recent-projects store (projects whose last session was deleted) into the
// live session-derived list. Session-derived entries win on a normalized-path
// collision; persisted-only projects are appended with a zero session count.
//
// Sits next to ProjectStep.recents-normalize.test.tsx (#1843), which covers
// the session-derived collection itself.

import { describe, expect, it } from "vitest";

import { collectRecentProjects, mergeRecentProjects } from "../steps/ProjectStep";
import type { RecentProjectEntry } from "../../../lib/api";
import type { SessionResponse } from "../../../lib/types";

function mockSession(overrides: Partial<SessionResponse> = {}): SessionResponse {
  return {
    id: overrides.id ?? "s1",
    title: overrides.title ?? "session",
    project_path: overrides.project_path ?? "/repo/alpha",
    group_path: overrides.group_path ?? "/repo/alpha",
    tool: overrides.tool ?? "claude",
    status: overrides.status ?? "Idle",
    yolo_mode: false,
    created_at: "2025-01-01T00:00:00Z",
    last_accessed_at: overrides.last_accessed_at ?? null,
    idle_entered_at: null,
    last_error: null,
    branch: null,
    main_repo_path: overrides.main_repo_path ?? null,
    is_sandboxed: false,
    favorited: false,
    has_managed_worktree: false,
    has_terminal: true,
    profile: "default",
    cleanup_defaults: { delete_worktree: false, delete_branch: false, delete_sandbox: false },
    remote_owner: null,
    notify_on_waiting: null,
    notify_on_idle: null,
    notify_on_error: null,
    claude_fullscreen: false,
    workspace_repos: overrides.workspace_repos ?? [],
    scratch: overrides.scratch ?? false,
    ...overrides,
  } as SessionResponse;
}

function persisted(overrides: Partial<RecentProjectEntry> = {}): RecentProjectEntry {
  return {
    path: overrides.path ?? "/repo/gamma",
    display_name: overrides.display_name ?? "gamma",
    tool: overrides.tool ?? "claude",
    last_used_at: overrides.last_used_at ?? "2025-09-01T00:00:00+00:00",
  };
}

describe("mergeRecentProjects (#2141)", () => {
  it("appends a persisted-only project with a zero session count", () => {
    const merged = mergeRecentProjects([], [persisted({ path: "/repo/frontend", display_name: "frontend" })]);

    expect(merged).toHaveLength(1);
    expect(merged[0].path).toBe("/repo/frontend");
    expect(merged[0].displayName).toBe("frontend");
    expect(merged[0].sessionCount).toBe(0);
  });

  it("derives the display name from the path when the persisted entry has none", () => {
    const merged = mergeRecentProjects([], [persisted({ path: "/repo/backend", display_name: "" })]);

    expect(merged).toHaveLength(1);
    expect(merged[0].displayName).toBe("backend");
  });

  it("falls back to the raw path for a root-only persisted entry with no name", () => {
    const merged = mergeRecentProjects([], [persisted({ path: "/", display_name: "" })]);

    expect(merged).toHaveLength(1);
    expect(merged[0].displayName).toBe("/");
  });

  it("lets the session-derived entry win on a path collision (keeps the real count)", () => {
    const sessionDerived = collectRecentProjects([
      mockSession({ id: "a", project_path: "/repo/frontend", last_accessed_at: "2025-09-05T00:00:00Z" }),
    ]);
    const merged = mergeRecentProjects(sessionDerived, [
      persisted({ path: "/repo/frontend", last_used_at: "2025-01-01T00:00:00+00:00" }),
    ]);

    expect(merged).toHaveLength(1);
    expect(merged[0].sessionCount).toBe(1);
    expect(merged[0].lastAccessedAt).toBe("2025-09-05T00:00:00Z");
  });

  it("dedupes a persisted path that differs only by a trailing slash", () => {
    const sessionDerived = collectRecentProjects([mockSession({ id: "a", project_path: "/repo/frontend" })]);
    const merged = mergeRecentProjects(sessionDerived, [persisted({ path: "/repo/frontend/" })]);

    expect(merged).toHaveLength(1);
    expect(merged[0].sessionCount).toBe(1);
  });

  it("sorts the combined list newest-first", () => {
    const sessionDerived = collectRecentProjects([
      mockSession({ id: "a", project_path: "/repo/live", last_accessed_at: "2025-09-10T00:00:00Z" }),
    ]);
    const merged = mergeRecentProjects(sessionDerived, [
      persisted({ path: "/repo/old", last_used_at: "2025-01-01T00:00:00+00:00" }),
      persisted({ path: "/repo/recent", last_used_at: "2025-12-01T00:00:00+00:00" }),
    ]);

    expect(merged.map((r) => r.path)).toEqual(["/repo/recent", "/repo/live", "/repo/old"]);
  });
});
