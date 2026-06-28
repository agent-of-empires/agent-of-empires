// @vitest-environment jsdom
//
// Coverage for the WorkspaceSidebar Trash section (#2489): a workspace whose
// sessions are all trashed renders under a collapsible "Trash" group with
// Open / Restore / Delete actions, separate from the live and sunk lists.
// Vitest (accurate per-file V8) rather than Playwright, whose bundle->source
// remap is lossy for this large file.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { WorkspaceSidebar } from "../WorkspaceSidebar";
import { buildSessionGroups } from "../../lib/sidebarGroups";
import type { SessionResponse, Workspace } from "../../lib/types";

function session(over: Partial<SessionResponse> = {}): SessionResponse {
  return {
    id: "s1",
    title: "t",
    project_path: "/repo-a",
    group_path: "",
    tool: "claude",
    status: "Stopped",
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
    cleanup_defaults: { delete_worktree: false, delete_branch: false, delete_sandbox: false },
    remote_owner: null,
    notify_on_waiting: null,
    notify_on_idle: null,
    notify_on_error: null,
    claude_fullscreen: false,
    workspace_repos: [],
    scratch: false,
    archived_at: null,
    snoozed_until: null,
    trashed_at: null,
    ...over,
  } as SessionResponse;
}

function workspace(id: string, sessions: SessionResponse[]): Workspace {
  return {
    id,
    branch: null,
    projectPath: "/repo-a",
    displayName: id,
    agents: ["claude"],
    primaryAgent: "claude",
    status: "idle",
    sessions,
  } as unknown as Workspace;
}

const noop = () => {};

function renderSidebar(over: Partial<React.ComponentProps<typeof WorkspaceSidebar>> = {}) {
  const props: React.ComponentProps<typeof WorkspaceSidebar> = {
    groups: buildSessionGroups([], { idleDecayWindowMs: 60_000, sortMode: "lastActivity", isCollapsed: () => false }),
    nestedGroups: [],
    onToggleSubgroup: noop,
    onReorderWorkspaces: noop,
    onReorderGroups: noop,
    activeId: null,
    open: true,
    onToggle: noop,
    onSelect: vi.fn(),
    onToggleGroup: noop,
    onUpdateRepoAppearance: noop,
    onNew: noop,
    onCreateSession: noop,
    savedProjects: [],
    onAddProject: noop,
    onEditProject: noop,
    onRemoveProject: noop,
    onSettings: noop,
    onRestoreSession: vi.fn(),
    onDeleteSession: vi.fn(),
    sortMode: "lastActivity",
    onSortModeChange: noop,
    pluginSortRef: null,
    onPluginSortChange: noop,
    axis: "group",
    onAxisChange: noop,
    ...over,
  };
  render(<WorkspaceSidebar {...props} />);
  return props;
}

afterEach(cleanup);

describe("WorkspaceSidebar Trash section (#2489)", () => {
  function trashedGroups() {
    const ws = workspace("trashed-ws", [session({ id: "s1", trashed_at: "2026-01-01T00:00:00Z" })]);
    return buildSessionGroups([ws], { idleDecayWindowMs: 60_000, sortMode: "lastActivity", isCollapsed: () => false });
  }

  it("renders a trashed workspace under the Trash section and exposes its actions", () => {
    const props = renderSidebar({ groups: trashedGroups() });

    const section = screen.getByTestId("sidebar-trash-section");
    expect(section).toBeTruthy();
    // Collapsed by default: rows hidden until the toggle is clicked.
    expect(screen.queryByTestId("sidebar-trash-row")).toBeNull();

    fireEvent.click(screen.getByTestId("sidebar-trash-toggle"));
    expect(screen.getByTestId("sidebar-trash-row")).toBeTruthy();

    fireEvent.click(screen.getByTestId("sidebar-trash-open"));
    expect(props.onSelect).toHaveBeenCalledWith("trashed-ws");

    fireEvent.click(screen.getByTestId("sidebar-trash-restore"));
    expect(props.onRestoreSession).toHaveBeenCalledWith(["s1"]);

    fireEvent.click(screen.getByTestId("sidebar-trash-purge"));
    expect(props.onDeleteSession).toHaveBeenCalledWith("trashed-ws");
  });

  it("hides Restore/Delete actions in read-only mode", () => {
    renderSidebar({ groups: trashedGroups(), readOnly: true });
    fireEvent.click(screen.getByTestId("sidebar-trash-toggle"));
    expect(screen.getByTestId("sidebar-trash-open")).toBeTruthy();
    expect(screen.queryByTestId("sidebar-trash-restore")).toBeNull();
    expect(screen.queryByTestId("sidebar-trash-purge")).toBeNull();
  });

  it("omits the Trash section when nothing is trashed", () => {
    renderSidebar({
      groups: buildSessionGroups([workspace("live-ws", [session({ id: "live", status: "Running" })])], {
        idleDecayWindowMs: 60_000,
        sortMode: "lastActivity",
        isCollapsed: () => false,
      }),
    });
    expect(screen.queryByTestId("sidebar-trash-section")).toBeNull();
  });
});
