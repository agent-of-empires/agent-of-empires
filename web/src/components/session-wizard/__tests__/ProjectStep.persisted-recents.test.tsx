// @vitest-environment jsdom
//
// Render coverage for the persisted recent-projects path in ProjectStep
// (#2141): the step fetches both /api/sessions and /api/recent-projects and
// merges them, so a project whose last session is deleted still shows in the
// Recent tab (with "0 sessions"), and a live project is not duplicated by a
// stale persisted entry. The merge logic itself is unit-tested in
// ProjectStep.merge.test.tsx; this exercises the component's fetch + render.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render } from "@testing-library/react";

import { ProjectStep } from "../steps/ProjectStep";
import { initialData } from "../wizardReducer";
import type { SessionResponse } from "../../../lib/types";
import type { RecentProjectEntry } from "../../../lib/api";

vi.mock("../../../lib/api", () => ({
  fetchSessions: vi.fn(),
  fetchRecentProjects: vi.fn(),
  fetchProjects: vi.fn().mockResolvedValue([]),
  cloneRepo: vi.fn(),
}));

import { fetchSessions, fetchRecentProjects } from "../../../lib/api";

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

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

function entry(overrides: Partial<RecentProjectEntry> = {}): RecentProjectEntry {
  return {
    path: overrides.path ?? "/repo/frontend",
    display_name: overrides.display_name ?? "frontend",
    tool: overrides.tool ?? "claude",
    last_used_at: overrides.last_used_at ?? "2025-09-09T00:00:00+00:00",
  };
}

function renderStep() {
  return render(
    <ProjectStep data={{ ...initialData, path: "", extraRepoPaths: [], scratch: false }} onChange={vi.fn()} />,
  );
}

describe("ProjectStep persisted recent projects (#2141)", () => {
  it("shows a persisted project with no live session in the Recent tab", async () => {
    vi.mocked(fetchSessions).mockResolvedValue({ sessions: [], workspace_ordering: [] });
    vi.mocked(fetchRecentProjects).mockResolvedValue({ projects: [entry()] });

    const { findAllByText } = renderStep();
    expect((await findAllByText("frontend")).length).toBeGreaterThan(0);
    expect((await findAllByText("0 sessions")).length).toBeGreaterThan(0);
  });

  it("does not duplicate a project that has both a live session and a persisted entry", async () => {
    vi.mocked(fetchSessions).mockResolvedValue({
      sessions: [mockSession({ id: "live", project_path: "/repo/frontend", last_accessed_at: "2025-09-10T00:00:00Z" })],
      workspace_ordering: [],
    });
    vi.mocked(fetchRecentProjects).mockResolvedValue({
      projects: [entry({ path: "/repo/frontend", last_used_at: "2025-01-01T00:00:00+00:00" })],
    });

    const { findAllByText, queryByText } = renderStep();
    // Single entry, and the live session count wins (not the persisted "0").
    expect((await findAllByText("frontend")).length).toBe(1);
    expect((await findAllByText("1 session")).length).toBeGreaterThan(0);
    expect(queryByText("0 sessions")).toBeNull();
  });

  it("renders Recent even when the recent-projects fetch returns null", async () => {
    vi.mocked(fetchSessions).mockResolvedValue({
      sessions: [mockSession({ id: "a", project_path: "/repo/alpha", last_accessed_at: "2025-09-01T00:00:00Z" })],
      workspace_ordering: [],
    });
    vi.mocked(fetchRecentProjects).mockResolvedValue(null);

    const { findAllByText } = renderStep();
    expect((await findAllByText("alpha")).length).toBeGreaterThan(0);
  });
});
