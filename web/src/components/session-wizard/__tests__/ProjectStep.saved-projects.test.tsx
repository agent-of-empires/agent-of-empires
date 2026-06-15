// @vitest-environment jsdom
//
// Vitest coverage for saved projects in the wizard Recent tab (#2140).
// Saved projects are a curated registry surfaced alongside the
// session-derived recents. A path present in both sources renders once,
// in the Saved section. The pure-function tests pin the dedup
// (`splitSavedAndRecent`); the render tests pin the two-section layout,
// the section headers, and the path-filter narrowing.
//
// Sits next to ProjectStep.workspace.test.tsx (#1645) and
// ProjectStep.recents-normalize.test.tsx (#1843).

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render } from "@testing-library/react";

import { ProjectStep, splitSavedAndRecent } from "../steps/ProjectStep";
import { initialData } from "../wizardReducer";
import type { ProjectInfo, SessionResponse } from "../../../lib/types";

vi.mock("../../../lib/api", () => ({
  fetchSessions: vi.fn(),
  fetchProjects: vi.fn(),
  cloneRepo: vi.fn(),
}));

import { fetchSessions, fetchProjects } from "../../../lib/api";

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

function savedProject(overrides: Partial<ProjectInfo> = {}): ProjectInfo {
  return {
    name: overrides.name ?? "alpha",
    path: overrides.path ?? "/repo/alpha",
    scope: overrides.scope ?? "global",
    default_base_branch: overrides.default_base_branch,
  };
}

function recentRow(path: string) {
  return {
    path,
    displayName: path.split("/").filter(Boolean).pop() || path,
    lastAccessedAt: null,
    tool: "claude",
    sessionCount: 1,
  };
}

function mockSession(overrides: Partial<SessionResponse> = {}): SessionResponse {
  return {
    id: overrides.id ?? "s1",
    title: "session",
    project_path: overrides.project_path ?? "/repo/beta",
    group_path: "/repo/beta",
    tool: "claude",
    status: "Idle",
    yolo_mode: false,
    created_at: "2025-01-01T00:00:00Z",
    last_accessed_at: overrides.last_accessed_at ?? "2025-09-01T00:00:00Z",
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
    workspace_repos: [],
    scratch: false,
    ...overrides,
  } as SessionResponse;
}

function renderStep(path = "") {
  const onChange = vi.fn();
  const utils = render(
    <ProjectStep data={{ ...initialData, path, extraRepoPaths: [], scratch: false }} onChange={onChange} />,
  );
  return { onChange, ...utils };
}

describe("splitSavedAndRecent (#2140)", () => {
  it("drops a recent whose path is also a saved project, keeping it in Saved only", () => {
    const out = splitSavedAndRecent([savedProject({ path: "/repo/alpha" })], [recentRow("/repo/alpha")]);

    expect(out.saved).toHaveLength(1);
    expect(out.recent).toHaveLength(0);
  });

  it("matches across a trailing-slash difference between the two sources", () => {
    const out = splitSavedAndRecent([savedProject({ path: "/repo/alpha/" })], [recentRow("/repo/alpha")]);

    expect(out.recent).toHaveLength(0);
  });

  it("keeps recents that are not saved projects", () => {
    const out = splitSavedAndRecent([savedProject({ path: "/repo/alpha" })], [recentRow("/repo/beta")]);

    expect(out.saved).toHaveLength(1);
    expect(out.recent.map((r) => r.path)).toEqual(["/repo/beta"]);
  });

  it("returns saved projects untouched", () => {
    const saved = [savedProject({ name: "a", path: "/a" }), savedProject({ name: "b", path: "/b", scope: "profile" })];
    const out = splitSavedAndRecent(saved, []);

    expect(out.saved).toEqual(saved);
    expect(out.recent).toHaveLength(0);
  });
});

describe("ProjectStep saved-projects render (#2140)", () => {
  // The Recent *tab* button always carries the text "Recent"; the Recent
  // *section* header is a styled <p>. Scope header assertions to <p> so
  // they don't collide with the tab button.
  const asHeader = { selector: "p" } as const;

  it("renders both a Saved projects section and a Recent section header", async () => {
    vi.mocked(fetchProjects).mockResolvedValue([savedProject({ name: "alpha", path: "/repo/alpha" })]);
    vi.mocked(fetchSessions).mockResolvedValue({
      sessions: [mockSession({ id: "s-beta", project_path: "/repo/beta" })],
      workspace_ordering: [],
    });

    const { findByText } = renderStep();
    expect(await findByText("Saved projects", asHeader)).toBeTruthy();
    expect(await findByText("Recent", asHeader)).toBeTruthy();
    // Saved project path and the recent session path both render.
    expect(await findByText("/repo/alpha")).toBeTruthy();
    expect(await findByText("/repo/beta")).toBeTruthy();
  });

  it("renders the Saved projects section but no Recent section header when there are no recents", async () => {
    vi.mocked(fetchProjects).mockResolvedValue([savedProject({ name: "solo", path: "/repo/solo" })]);
    vi.mocked(fetchSessions).mockResolvedValue({ sessions: [], workspace_ordering: [] });

    const { findByText, queryByText } = renderStep();
    expect(await findByText("Saved projects", asHeader)).toBeTruthy();
    expect(await findByText("/repo/solo")).toBeTruthy();
    // No recents, so the Recent section header is suppressed (the tab
    // button labeled "Recent" is a <button>, not the <p> header).
    expect(queryByText("Recent", asHeader)).toBeNull();
  });

  it("renders a path shared by saved and recent only once (in the Saved section)", async () => {
    vi.mocked(fetchProjects).mockResolvedValue([savedProject({ name: "dup", path: "/repo/dup" })]);
    vi.mocked(fetchSessions).mockResolvedValue({
      sessions: [mockSession({ id: "s-dup", project_path: "/repo/dup" })],
      workspace_ordering: [],
    });

    const { findByText, findAllByText, queryByText } = renderStep();
    await findByText("Saved projects", asHeader);
    // The shared path appears once (the saved row); the recent section,
    // now empty after dedup, renders no header and no duplicate row.
    expect((await findAllByText("/repo/dup")).length).toBe(1);
    expect(queryByText("Recent", asHeader)).toBeNull();
  });

  it("narrows the Saved projects section to entries matching the path filter", async () => {
    vi.mocked(fetchProjects).mockResolvedValue([
      savedProject({ name: "alpha", path: "/repo/alpha" }),
      savedProject({ name: "zeta", path: "/repo/zeta" }),
    ]);
    vi.mocked(fetchSessions).mockResolvedValue({ sessions: [], workspace_ordering: [] });

    // data.path acts as the filter query against both sections.
    const { findByText, queryByText } = renderStep("zeta");
    expect(await findByText("/repo/zeta")).toBeTruthy();
    expect(queryByText("/repo/alpha")).toBeNull();
  });
});
