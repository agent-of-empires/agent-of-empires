// @vitest-environment jsdom
//
// Vitest coverage for the ProjectStep recents filter excluding
// multi-repo workspace sessions (#1645). A workspace session collapses
// to its `main_repo_path` in the Recent list, so picking it would start
// a plain single-repo session and silently drop the other repos. The
// guard in `collectRecentProjects` keeps workspaces out of the list
// entirely (single path cannot reconstruct a workspace).
//
// Sits next to ProjectStep.scratch.test.tsx (#1324), which covers the
// adjacent scratch-session skip.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render } from "@testing-library/react";

import { ProjectStep } from "../steps/ProjectStep";
import { initialData } from "../wizardReducer";
import type { SessionResponse, WorkspaceRepoSummary } from "../../../lib/types";

vi.mock("../../../lib/api", () => ({
  fetchSessions: vi.fn(),
  cloneRepo: vi.fn(),
}));

import { fetchSessions } from "../../../lib/api";

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

function workspaceRepos(): WorkspaceRepoSummary[] {
  return [
    { name: "gamma", source_path: "/repo/gamma", branch: "main" },
    { name: "delta", source_path: "/repo/delta", branch: "main" },
  ];
}

function renderStep() {
  const onChange = vi.fn();
  const utils = render(
    <ProjectStep
      data={{
        ...initialData,
        path: "",
        extraRepoPaths: [],
        scratch: false,
      }}
      onChange={onChange}
    />,
  );
  return { onChange, ...utils };
}

describe("ProjectStep recents workspace filter (#1645)", () => {
  it("does not surface multi-repo workspace sessions in the Recent tab", async () => {
    // One single-repo session plus one workspace session. The workspace
    // keys by its main_repo_path ("/repo/gamma"); without the guard it
    // would render as a phantom single-repo recent entry that, when
    // picked, drops the other repos. Only the real single-repo path
    // ("alpha") must appear.
    vi.mocked(fetchSessions).mockResolvedValue({
      sessions: [
        mockSession({
          id: "s-single",
          project_path: "/repo/alpha",
          last_accessed_at: "2025-09-01T00:00:00Z",
        }),
        mockSession({
          id: "s-workspace",
          project_path: "/repo/gamma",
          main_repo_path: "/repo/gamma",
          workspace_repos: workspaceRepos(),
          last_accessed_at: "2025-09-02T00:00:00Z",
        }),
      ],
      workspace_ordering: [],
    });

    const { findAllByText, queryByText } = renderStep();
    // The single-repo project renders; the workspace basename never does.
    expect((await findAllByText("alpha")).length).toBeGreaterThan(0);
    expect(queryByText("gamma")).toBeNull();
  });

  it("still surfaces single-repo sessions unchanged when no workspaces are present", async () => {
    vi.mocked(fetchSessions).mockResolvedValue({
      sessions: [
        mockSession({
          id: "s-a",
          project_path: "/repo/alpha",
          last_accessed_at: "2025-09-01T00:00:00Z",
        }),
        mockSession({
          id: "s-b",
          project_path: "/repo/beta",
          last_accessed_at: "2025-09-02T00:00:00Z",
        }),
      ],
      workspace_ordering: [],
    });

    const { findAllByText } = renderStep();
    expect((await findAllByText("alpha")).length).toBeGreaterThan(0);
    expect((await findAllByText("beta")).length).toBeGreaterThan(0);
  });
});
