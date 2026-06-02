// Vitest coverage for trailing-slash normalization in
// `collectRecentProjects` (#1843). Two sessions on the same project that
// differ only by a trailing `/` must collapse to a single Recent entry
// with a summed session count, mirroring the backend's dedup convention
// (`src/cli/add.rs` is_duplicate_session, `src/server/api/sessions.rs`
// workspace_id_for_session, both `trim_end_matches('/')`).
//
// Sits next to ProjectStep.workspace.test.tsx (#1645) and
// ProjectStep.scratch.test.tsx (#1324), which cover the adjacent recents
// filters.

import { describe, expect, it } from "vitest";

import { collectRecentProjects } from "../steps/ProjectStep";
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
    workspace_repos: overrides.workspace_repos ?? [],
    scratch: overrides.scratch ?? false,
    ...overrides,
  } as SessionResponse;
}

describe("collectRecentProjects trailing-slash normalization (#1843)", () => {
  it("collapses paths differing only by a trailing slash into one entry with a summed count", () => {
    const recents = collectRecentProjects([
      mockSession({
        id: "s-slash",
        project_path: "/foo/bar/",
        last_accessed_at: "2025-09-01T00:00:00Z",
      }),
      mockSession({
        id: "s-noslash",
        project_path: "/foo/bar",
        last_accessed_at: "2025-09-02T00:00:00Z",
      }),
    ]);

    expect(recents).toHaveLength(1);
    expect(recents[0].path).toBe("/foo/bar");
    expect(recents[0].displayName).toBe("bar");
    expect(recents[0].sessionCount).toBe(2);
  });

  it("collapses multiple trailing slashes too", () => {
    const recents = collectRecentProjects([
      mockSession({ id: "s-a", project_path: "/foo/bar///" }),
      mockSession({ id: "s-b", project_path: "/foo/bar" }),
    ]);

    expect(recents).toHaveLength(1);
    expect(recents[0].path).toBe("/foo/bar");
    expect(recents[0].sessionCount).toBe(2);
  });

  it("keeps the filesystem root '/' as a single entry rather than normalizing it away", () => {
    const recents = collectRecentProjects([
      mockSession({ id: "s-root", project_path: "/" }),
    ]);

    expect(recents).toHaveLength(1);
    expect(recents[0].path).toBe("/");
    // basename of "/" has no segments; displayName falls back to the path.
    expect(recents[0].displayName).toBe("/");
  });

  it("leaves distinct projects untouched", () => {
    const recents = collectRecentProjects([
      mockSession({
        id: "s-a",
        project_path: "/repo/alpha",
        last_accessed_at: "2025-09-01T00:00:00Z",
      }),
      mockSession({
        id: "s-b",
        project_path: "/repo/beta/",
        last_accessed_at: "2025-09-02T00:00:00Z",
      }),
    ]);

    expect(recents).toHaveLength(2);
    expect(recents.map((r) => r.path).sort()).toEqual([
      "/repo/alpha",
      "/repo/beta",
    ]);
  });
});
