// @vitest-environment jsdom
//
// RTL coverage for the reworked project header row (#2207): the per-project
// session count, the icon (owner avatar vs Folder fallback), the data-draggable
// hook, and the drag-release click suppression. The suppression branches
// (window open while dragging, click swallowed row-wide) are timing/pointer
// dependent and flaky to hit through Playwright, so they are pinned down here
// where `dragHandle.isDragging` can be controlled directly.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { SidebarGroupHeader } from "../WorkspaceSidebar";
import type { SidebarGroup } from "../../lib/sidebarGroups";
import type { SessionResponse, Workspace } from "../../lib/types";

function session(id: string): SessionResponse {
  return {
    id,
    title: id,
    project_path: "/p",
    group_path: "/p",
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
  };
}

function workspace(id: string, count: number): Workspace {
  return {
    id,
    branch: null,
    projectPath: "/p",
    displayName: id,
    agents: ["claude"],
    primaryAgent: "claude",
    status: "idle",
    sessions: Array.from({ length: count }, (_, i) => session(`${id}-s${i}`)),
  };
}

// A workspace whose every session is sunk (archived or snoozed) is "sunk"
// per workspaceIsSunk, drops out of the live row list, and must not count
// toward the header badge. See #2372.
function sunkWorkspace(id: string, count: number, kind: "archived" | "snoozed"): Workspace {
  const ws = workspace(id, count);
  ws.sessions = ws.sessions.map((s) =>
    kind === "archived"
      ? { ...s, archived_at: "2025-01-02T00:00:00Z" }
      : { ...s, snoozed_until: "2099-01-01T00:00:00Z" },
  );
  return ws;
}

function group(over: Partial<SidebarGroup> = {}): SidebarGroup {
  const workspaces = over.workspaces ?? [{ key: "w1", workspace: workspace("w1", 3) }];
  return {
    id: "g1",
    kind: "repo",
    displayName: "my-project",
    defaultDisplayName: "my-project",
    alias: null,
    color: null,
    remoteOwner: null,
    workspaces,
    status: "idle",
    collapsed: false,
    capabilities: { appearance: true, reorder: true, create: "repo" },
    repoPath: "/p",
    registeredProjects: [],
    pinned: false,
    pinnedEmpty: false,
    ...over,
  };
}

function dragHandle(isDragging: boolean) {
  return { setActivatorNodeRef: () => {}, attributes: {}, listeners: {}, isDragging };
}

function renderHeader(props: Partial<Parameters<typeof SidebarGroupHeader>[0]> = {}) {
  const onClick = vi.fn();
  const onNewSession = vi.fn();
  render(
    <SidebarGroupHeader
      group={group()}
      hasActiveChild={false}
      onClick={onClick}
      onNewSession={onNewSession}
      onUpdateAppearance={() => {}}
      offline={false}
      {...props}
    />,
  );
  return { onClick, onNewSession };
}

afterEach(() => cleanup());

describe("SidebarGroupHeader", () => {
  it("counts live (non-sunk) workspaces, matching the rows rendered below", () => {
    renderHeader({
      group: group({
        workspaces: [
          { key: "a", workspace: workspace("a", 2) },
          { key: "b", workspace: workspace("b", 3) },
        ],
      }),
    });
    // Two live workspaces, both shown as rows, so the badge reads (2).
    expect(screen.getByTestId("sidebar-group-session-count").textContent).toBe("(2)");
  });

  it("excludes archived and snoozed workspaces from the count (#2372)", () => {
    renderHeader({
      group: group({
        workspaces: [
          { key: "live", workspace: workspace("live", 1) },
          { key: "arch", workspace: sunkWorkspace("arch", 1, "archived") },
          { key: "snoozed", workspace: sunkWorkspace("snoozed", 1, "snoozed") },
        ],
      }),
    });
    // Only the live workspace is a visible row; sunk ones drop to the footer.
    expect(screen.getByTestId("sidebar-group-session-count").textContent).toBe("(1)");
  });

  it("reads (0) when every workspace is sunk", () => {
    renderHeader({
      group: group({
        workspaces: [
          { key: "arch", workspace: sunkWorkspace("arch", 2, "archived") },
          { key: "snoozed", workspace: sunkWorkspace("snoozed", 1, "snoozed") },
        ],
      }),
    });
    expect(screen.getByTestId("sidebar-group-session-count").textContent).toBe("(0)");
  });

  it("renders the Folder fallback icon when the project has no remote owner", () => {
    renderHeader({ group: group({ remoteOwner: null }) });
    // The owner avatar is an <img alt={owner}>; with no owner it is absent.
    expect(screen.queryByRole("img")).toBeNull();
    expect(screen.getByTestId("sidebar-group-icon")).not.toBeNull();
  });

  it("renders the owner avatar when the project has a remote owner", () => {
    renderHeader({ group: group({ remoteOwner: "octocat" }) });
    const img = screen.getByRole("img") as HTMLImageElement;
    expect(img.getAttribute("alt")).toBe("octocat");
  });

  it("marks the header draggable only when a drag handle is provided", () => {
    renderHeader({ dragHandle: dragHandle(false) });
    expect(screen.getByTestId("sidebar-group-header").getAttribute("data-draggable")).toBe("true");
    cleanup();
    renderHeader();
    expect(screen.getByTestId("sidebar-group-header").getAttribute("data-draggable")).toBeNull();
  });

  it("toggles collapse on a normal click (no drag in flight)", () => {
    const { onClick } = renderHeader({ dragHandle: dragHandle(false) });
    fireEvent.click(screen.getByText("my-project"));
    expect(onClick).toHaveBeenCalledTimes(1);
  });

  it("swallows clicks on every control while a drag is in flight", () => {
    const { onClick, onNewSession } = renderHeader({ dragHandle: dragHandle(true) });
    // Drag active: the row-level capture handler must suppress both the
    // toggle and the New Session button, not just the toggle.
    fireEvent.click(screen.getByText("my-project"));
    fireEvent.click(screen.getByLabelText("New session in my-project"));
    expect(onClick).not.toHaveBeenCalled();
    expect(onNewSession).not.toHaveBeenCalled();
  });

  it("keeps suppressing briefly after the drag ends, then releases", () => {
    vi.useFakeTimers();
    try {
      const onClick = vi.fn();
      const { rerender } = render(
        <SidebarGroupHeader
          group={group()}
          hasActiveChild={false}
          onClick={onClick}
          onNewSession={() => {}}
          onUpdateAppearance={() => {}}
          offline={false}
          dragHandle={dragHandle(true)}
        />,
      );
      // Drag ends: window collapses from Infinity to a short tail.
      rerender(
        <SidebarGroupHeader
          group={group()}
          hasActiveChild={false}
          onClick={onClick}
          onNewSession={() => {}}
          onUpdateAppearance={() => {}}
          offline={false}
          dragHandle={dragHandle(false)}
        />,
      );
      fireEvent.click(screen.getByText("my-project"));
      expect(onClick).not.toHaveBeenCalled();

      // After the tail expires a click toggles again.
      vi.advanceTimersByTime(300);
      fireEvent.click(screen.getByText("my-project"));
      expect(onClick).toHaveBeenCalledTimes(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it("shows an attention badge summing Waiting/Error sessions across the group", () => {
    const wsA = workspace("a", 2);
    wsA.sessions = [
      { ...wsA.sessions[0]!, status: "Waiting" },
      { ...wsA.sessions[1]!, status: "Running" },
    ];
    const wsB = workspace("b", 1);
    wsB.sessions = [{ ...wsB.sessions[0]!, status: "Error" }];
    renderHeader({
      group: group({
        workspaces: [
          { key: "a", workspace: wsA },
          { key: "b", workspace: wsB },
        ],
      }),
    });
    expect(screen.getByTestId("sidebar-group-attention-badge").textContent).toBe("2");
  });

  it("hides the attention badge when nothing needs attention", () => {
    renderHeader({ group: group({ workspaces: [{ key: "w1", workspace: workspace("w1", 3) }] }) });
    expect(screen.queryByTestId("sidebar-group-attention-badge")).toBeNull();
  });

  it("does not count sunk sessions toward the attention badge", () => {
    // Archived but Waiting: the sunk short-circuit must win, so no badge.
    const ws = sunkWorkspace("arch", 2, "archived");
    ws.sessions = ws.sessions.map((s) => ({ ...s, status: "Waiting" }));
    renderHeader({ group: group({ workspaces: [{ key: "arch", workspace: ws }] }) });
    expect(screen.queryByTestId("sidebar-group-attention-badge")).toBeNull();
  });
});
