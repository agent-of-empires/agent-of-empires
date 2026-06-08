// @vitest-environment jsdom
//
// Vitest coverage for the ProjectStep scratch-mode rendering and
// recents filter (#1324). Lifts coverage on:
//   - the `Skip project folder` Toggle and label double-toggle guard
//   - the scratch confirmation card swap (tabs / DirectoryBrowser
//     hidden when `data.scratch === true`)
//   - `collectRecentProjects` excluding `scratch === true` sessions
//
// Live Playwright (`wizard-scratch-*.spec.ts`) exercises the
// end-to-end create flow; this file isolates the pure-render bits so
// the bundle of branches under `data.scratch` does not depend on a
// real `aoe serve`.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, fireEvent } from "@testing-library/react";

import { ProjectStep } from "../steps/ProjectStep";
import { initialData } from "../wizardReducer";
import type { SessionResponse } from "../../../lib/types";

vi.mock("../../../lib/api", () => ({
  fetchSessions: vi.fn(),
  cloneRepo: vi.fn(),
  // The Browse tab mounts DirectoryBrowser, which probes the filesystem on
  // mount (getHomePath -> browseFilesystem). Stub both so the tab renders
  // without hitting the network. ok:false makes navigate() bail cleanly.
  getHomePath: vi.fn().mockResolvedValue(null),
  browseFilesystem: vi.fn().mockResolvedValue({ ok: false, entries: [] }),
}));

import { fetchSessions } from "../../../lib/api";

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

function mockSession(
  overrides: Partial<SessionResponse> = {},
): SessionResponse {
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
    workspace_repos: [],
    scratch: overrides.scratch ?? false,
    ...overrides,
  } as SessionResponse;
}

function renderStep(overrides: { scratch?: boolean } = {}) {
  const onChange = vi.fn();
  const utils = render(
    <ProjectStep
      data={{
        ...initialData,
        path: "",
        extraRepoPaths: [],
        scratch: overrides.scratch ?? false,
      }}
      onChange={onChange}
    />,
  );
  return { onChange, ...utils };
}

describe("ProjectStep scratch toggle (#1324)", () => {
  beforeEach(() => {
    // Most tests don't care about recents; default to an empty
    // envelope so the loading skeleton resolves immediately.
    // `fetchSessions` returns `Promise<SessionsEnvelope | null>` and
    // the component reads `envelope.sessions` so the shape must
    // match.
    vi.mocked(fetchSessions).mockResolvedValue({
      sessions: [],
      workspace_ordering: [],
    });
  });

  it("renders the Skip project folder toggle in the off position by default", () => {
    const { getByRole } = renderStep({ scratch: false });
    const toggle = getByRole("switch", { name: "Skip project folder" });
    expect(toggle.getAttribute("aria-checked")).toBe("false");
  });

  it("clicking the toggle calls onChange with scratch=true", () => {
    const { onChange, getByRole } = renderStep({ scratch: false });
    fireEvent.click(getByRole("switch", { name: "Skip project folder" }));
    expect(onChange).toHaveBeenCalledWith("scratch", true);
  });

  it("clicking the toggle switch does not double-fire via the label", () => {
    // The label wraps the toggle and has its own onClick that toggles
    // scratch. Clicks landing on the inner switch button must NOT also
    // run the label handler (would flip twice and land on the
    // original value). The guard at ProjectStep.tsx:192 implements
    // `closest('button[role="switch"]')` to skip the label handler.
    const { onChange, getByRole } = renderStep({ scratch: false });
    fireEvent.click(getByRole("switch", { name: "Skip project folder" }));
    // Exactly one onChange call, not two.
    expect(onChange).toHaveBeenCalledTimes(1);
  });

  it("with scratch=true, the tabs and DirectoryBrowser are hidden", () => {
    const { queryByRole, queryByText } = renderStep({ scratch: true });
    // Tab strip: "Browse" tab button is absent when scratch is on.
    expect(queryByRole("button", { name: "Browse", exact: true })).toBeNull();
    expect(queryByRole("button", { name: "Clone URL" })).toBeNull();
    // The confirmation card replaces the picker.
    expect(
      queryByText(/A fresh scratch directory under your AoE app data folder/),
    ).toBeTruthy();
  });

  it("with scratch=false, the Browse tab is rendered", async () => {
    const { findByRole } = renderStep({ scratch: false });
    // Browse tab always exists; Recent only renders when fetchSessions
    // returns something. The await is required because the component
    // fetches recents on mount and the loading skeleton must resolve.
    expect(
      await findByRole("button", { name: "Browse", exact: true }),
    ).toBeTruthy();
  });
});

describe("ProjectStep recents filter (#1324)", () => {
  it("does not surface scratch sessions in the Recent tab", async () => {
    // fetchSessions mock has to be set inside the test itself, not in
    // beforeEach: the parent describe's afterEach calls
    // `vi.clearAllMocks` which wipes the per-suite default and resets
    // the mock back to returning undefined.
    // Mix one scratch session in with two real-repo sessions. Only
    // the real-repo paths must appear in the Recent list; the scratch
    // session would otherwise be a re-selectable phantom "project"
    // pointing at a directory that gets deleted with the session.
    vi.mocked(fetchSessions).mockResolvedValue({
      sessions: [
        mockSession({
          id: "s-real-a",
          project_path: "/repo/alpha",
          last_accessed_at: "2025-09-01T00:00:00Z",
        }),
        mockSession({
          id: "s-real-b",
          project_path: "/repo/beta",
          last_accessed_at: "2025-09-02T00:00:00Z",
        }),
        mockSession({
          id: "s-scratch",
          project_path: "/home/u/.agent-of-empires/scratch/aaa",
          scratch: true,
          last_accessed_at: "2025-09-03T00:00:00Z",
        }),
      ],
      workspace_ordering: [],
    });

    const { findAllByText, queryByText } = renderStep({ scratch: false });
    // alpha and beta come from real projects; both render in the
    // Recent tab. Each recent row shows the basename ("alpha") AND
    // the full path ("/repo/alpha") so the substring regex would
    // match twice; query by exact displayName to scope to the title
    // span. The scratch dir basename ("aaa") never appears.
    expect((await findAllByText("alpha")).length).toBeGreaterThan(0);
    expect((await findAllByText("beta")).length).toBeGreaterThan(0);
    expect(queryByText("aaa")).toBeNull();
  });
});
