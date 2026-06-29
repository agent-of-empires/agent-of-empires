// @vitest-environment jsdom
//
// Coverage for the WorkspaceSidebar Trash control (#2489, reworked in #2512):
// a workspace whose sessions are all trashed is reachable from a labeled Trash
// control in the sidebar footer next to Settings, which opens a wider panel
// with Open / Restore / Delete actions. Trash is no longer an inline scrolling
// section.
// Also asserts the Projects section renders below "Snoozed & archived" (#2512).
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

describe("WorkspaceSidebar Trash control (#2489, #2512)", () => {
  // The Trash list is owned by the parent (App computes it from the full,
  // unsliced workspace set) and passed in as `trashedWorkspaces`; the sidebar
  // no longer derives it from `groups`. See #2533. Tests pass both: `groups`
  // so the workspace exists for navigation, `trashedWorkspaces` to populate
  // the footer popover.
  function trashedWorkspace(): Workspace {
    return workspace("trashed-ws", [session({ id: "s1", trashed_at: "2026-01-01T00:00:00Z" })]);
  }
  function trashedGroups() {
    return buildSessionGroups([trashedWorkspace()], {
      idleDecayWindowMs: 60_000,
      sortMode: "lastActivity",
      isCollapsed: () => false,
    });
  }
  function renderWithTrash(over: Partial<React.ComponentProps<typeof WorkspaceSidebar>> = {}) {
    return renderSidebar({ groups: trashedGroups(), trashedWorkspaces: [trashedWorkspace()], ...over });
  }

  it("reaches a trashed workspace via the footer Trash panel and exposes its actions", () => {
    const props = renderWithTrash();

    // No inline section in the scrolling list; only the footer toggle.
    expect(screen.queryByTestId("sidebar-trash-section")).toBeNull();
    expect(screen.getByTestId("sidebar-trash-toggle").textContent).toContain("Trash");
    // Closed by default: panel and rows hidden until the footer control is clicked.
    expect(screen.queryByTestId("sidebar-trash-menu")).toBeNull();
    expect(screen.queryByTestId("sidebar-trash-row")).toBeNull();

    fireEvent.click(screen.getByTestId("sidebar-trash-toggle"));
    expect(screen.getByTestId("sidebar-trash-menu")).toBeTruthy();
    expect(screen.getByTestId("sidebar-trash-row")).toBeTruthy();
    expect(screen.getByTestId("sidebar-trash-open").textContent).toContain("Open");
    expect(screen.getByTestId("sidebar-trash-restore").textContent).toContain("Restore");
    expect(screen.getByTestId("sidebar-trash-purge").textContent).toContain("Delete");

    fireEvent.click(screen.getByTestId("sidebar-trash-open"));
    expect(props.onSelect).toHaveBeenCalledWith("trashed-ws");
    expect(screen.queryByTestId("sidebar-trash-menu")).toBeNull();

    fireEvent.click(screen.getByTestId("sidebar-trash-toggle"));
    fireEvent.click(screen.getByTestId("sidebar-trash-restore"));
    expect(props.onRestoreSession).toHaveBeenCalledWith(["s1"]);

    fireEvent.click(screen.getByTestId("sidebar-trash-purge"));
    expect(props.onDeleteSession).toHaveBeenCalledWith("trashed-ws");
  });

  it("closes the Trash popover on Escape", () => {
    renderWithTrash();
    fireEvent.click(screen.getByTestId("sidebar-trash-toggle"));
    expect(screen.getByTestId("sidebar-trash-menu")).toBeTruthy();
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByTestId("sidebar-trash-menu")).toBeNull();
  });

  it("closes the Trash popover on outside click", () => {
    renderWithTrash();
    fireEvent.click(screen.getByTestId("sidebar-trash-toggle"));
    expect(screen.getByTestId("sidebar-trash-menu")).toBeTruthy();
    fireEvent.mouseDown(document.body);
    expect(screen.queryByTestId("sidebar-trash-menu")).toBeNull();
  });

  it("keeps the Trash icon reachable while a filter hides every live row (#2512)", () => {
    // Trash is a global recovery affordance: an active filter that matches no
    // workspace must not strand trashed sessions by hiding the footer icon.
    renderWithTrash();
    fireEvent.click(screen.getByLabelText("Filter sessions"));
    fireEvent.change(screen.getByTestId("sidebar-filter-input"), { target: { value: "zzz-no-match" } });
    expect(screen.getByTestId("sidebar-trash-toggle").textContent).toContain("Trash");
    fireEvent.click(screen.getByTestId("sidebar-trash-toggle"));
    expect(screen.getByTestId("sidebar-trash-row")).toBeTruthy();
  });

  it("hides Restore/Delete actions in read-only mode", () => {
    renderWithTrash({ readOnly: true });
    fireEvent.click(screen.getByTestId("sidebar-trash-toggle"));
    expect(screen.getByTestId("sidebar-trash-open")).toBeTruthy();
    expect(screen.queryByTestId("sidebar-trash-restore")).toBeNull();
    expect(screen.queryByTestId("sidebar-trash-purge")).toBeNull();
  });

  it("omits the Trash icon when nothing is trashed", () => {
    renderSidebar({
      groups: buildSessionGroups([workspace("live-ws", [session({ id: "live", status: "Running" })])], {
        idleDecayWindowMs: 60_000,
        sortMode: "lastActivity",
        isCollapsed: () => false,
      }),
    });
    expect(screen.queryByTestId("sidebar-trash-toggle")).toBeNull();
  });

  it("renders the Projects section below 'Snoozed & archived' (#2512)", () => {
    // An archived (sunk) workspace surfaces the sunk section; the Projects
    // section header always renders when CRUD is available. Assert DOM order.
    const archivedWs = workspace("archived-ws", [session({ id: "a1", archived_at: "2026-01-01T00:00:00Z" })]);
    renderSidebar({
      groups: buildSessionGroups([archivedWs], {
        idleDecayWindowMs: 60_000,
        sortMode: "lastActivity",
        isCollapsed: () => false,
      }),
    });
    const sunk = screen.getByTestId("sidebar-sunk-section");
    const projects = screen.getByTestId("sidebar-projects-section");
    expect(sunk.compareDocumentPosition(projects) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });
});
