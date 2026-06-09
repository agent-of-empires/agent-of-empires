// @vitest-environment jsdom
//
// RTL coverage for the sidebar "Edit workdir name" flow (#1723): the
// context-menu gating (managed worktree + not running), and the modal's
// request payload (name + rename_branch) plus its error surface. Mirrors
// SessionRowTriage.test.tsx for the SessionRow + DragSuppressContext setup.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { useRef, type ReactNode } from "react";

import { reportError } from "../../lib/toastBus";
import { DragSuppressContext, SessionRow } from "../WorkspaceSidebar";

vi.mock("../../lib/toastBus", () => ({
  reportError: vi.fn(),
  reportInfo: vi.fn(),
}));
import { EMPTY_OPTIMISTIC } from "../../lib/sidebarOptimistic";
import type { SessionResponse, Workspace } from "../../lib/types";

function session(over: Partial<SessionResponse> = {}): SessionResponse {
  return {
    id: "s1",
    title: "row title",
    project_path: "/p/old-name",
    group_path: "/p",
    tool: "claude",
    status: "Idle",
    yolo_mode: false,
    created_at: "2025-01-01T00:00:00Z",
    last_accessed_at: null,
    idle_entered_at: null,
    last_error: null,
    branch: "old-name",
    main_repo_path: "/p",
    is_sandboxed: false,
    favorited: false,
    has_managed_worktree: true,
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
    ...over,
  };
}

function workspace(id: string, sessions: SessionResponse[]): Workspace {
  return {
    id,
    branch: "old-name",
    projectPath: "/p",
    displayName: id,
    agents: ["claude"],
    primaryAgent: "claude",
    status: "idle",
    sessions,
  };
}

function Wrap({ children }: { children: ReactNode }) {
  const ref = useRef(0);
  return (
    <DragSuppressContext.Provider value={ref}>
      {children}
    </DragSuppressContext.Provider>
  );
}

const fetchSpy = vi.fn<typeof fetch>();

beforeEach(() => {
  fetchSpy.mockReset();
  vi.stubGlobal("fetch", fetchSpy);
  fetchSpy.mockImplementation(
    async () =>
      new Response(JSON.stringify({ id: "s1" }), {
        status: 200,
        headers: { "content-type": "application/json" },
      }),
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function openMenu(ws: Workspace) {
  render(
    <Wrap>
      <SessionRow
        workspace={ws}
        isActive={false}
        isSelected={false}
        onActivate={() => {}}
        optimistic={EMPTY_OPTIMISTIC}
        onPinToggle={() => {}}
        onArchiveToggle={() => {}}
        onSnooze={() => {}}
      />
    </Wrap>,
  );
  fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
}

describe("sidebar Edit workdir name", () => {
  it("offers the action for a managed, idle worktree session", () => {
    openMenu(workspace("w", [session()]));
    expect(
      screen.queryByTestId("sidebar-context-menu-edit-workdir"),
    ).not.toBeNull();
  });

  it("hides the action for a non-managed worktree", () => {
    openMenu(workspace("w", [session({ has_managed_worktree: false })]));
    expect(
      screen.queryByTestId("sidebar-context-menu-edit-workdir"),
    ).toBeNull();
  });

  it("hides the action while the session is running", () => {
    openMenu(workspace("w", [session({ status: "Running" })]));
    expect(
      screen.queryByTestId("sidebar-context-menu-edit-workdir"),
    ).toBeNull();
  });

  it("hides the action when the session is tied (#1927)", () => {
    // Tied mode collapses naming into the rename action, so the standalone
    // workdir edit is not offered.
    openMenu(workspace("w", [session({ tie_workdir_to_name: true })]));
    expect(
      screen.queryByTestId("sidebar-context-menu-edit-workdir"),
    ).toBeNull();
  });

  it("PATCHes the worktree-name endpoint with name and rename_branch", async () => {
    openMenu(workspace("w", [session()]));
    fireEvent.click(screen.getByTestId("sidebar-context-menu-edit-workdir"));

    fireEvent.change(screen.getByTestId("workdir-modal-name"), {
      target: { value: "fresh-name" },
    });
    fireEvent.click(screen.getByTestId("workdir-modal-rename-branch"));
    fireEvent.click(screen.getByTestId("workdir-modal-save"));

    await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    const [url, init] = fetchSpy.mock.calls[0];
    expect(url).toBe("/api/sessions/s1/worktree-name");
    expect(init?.method).toBe("PATCH");
    expect(JSON.parse(init?.body as string)).toEqual({
      name: "fresh-name",
      rename_branch: true,
    });
  });

  it("inline rename PATCHes the title endpoint", async () => {
    openMenu(workspace("w", [session()]));
    fireEvent.click(screen.getByTestId("sidebar-context-menu-rename"));
    fireEvent.change(screen.getByTestId("sidebar-rename-input"), {
      target: { value: "new title" },
    });
    fireEvent.keyDown(screen.getByTestId("sidebar-rename-input"), {
      key: "Enter",
    });

    await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    const [url, init] = fetchSpy.mock.calls[0];
    expect(url).toBe("/api/sessions/s1");
    expect(init?.method).toBe("PATCH");
    expect(JSON.parse(init?.body as string)).toEqual({ title: "new title" });
  });

  it("surfaces the server message when a tied rename is rejected (#1927)", async () => {
    fetchSpy.mockImplementation(
      async () =>
        new Response(
          JSON.stringify({
            error: "session_running",
            message: "Stop the session before renaming it.",
          }),
          { status: 409, headers: { "content-type": "application/json" } },
        ),
    );
    openMenu(workspace("w", [session({ tie_workdir_to_name: true })]));
    fireEvent.click(screen.getByTestId("sidebar-context-menu-rename"));
    fireEvent.change(screen.getByTestId("sidebar-rename-input"), {
      target: { value: "blocked" },
    });
    fireEvent.keyDown(screen.getByTestId("sidebar-rename-input"), {
      key: "Enter",
    });

    await vi.waitFor(() =>
      expect(reportError).toHaveBeenCalledWith(
        "Stop the session before renaming it.",
      ),
    );
  });

  it("surfaces the server validation message on failure", async () => {
    fetchSpy.mockImplementation(
      async () =>
        new Response(JSON.stringify({ message: "Branch 'x' already exists" }), {
          status: 409,
          headers: { "content-type": "application/json" },
        }),
    );
    openMenu(workspace("w", [session()]));
    fireEvent.click(screen.getByTestId("sidebar-context-menu-edit-workdir"));
    fireEvent.change(screen.getByTestId("workdir-modal-name"), {
      target: { value: "x" },
    });
    fireEvent.click(screen.getByTestId("workdir-modal-save"));

    await vi.waitFor(() =>
      expect(screen.getByTestId("workdir-modal-error").textContent).toContain(
        "already exists",
      ),
    );
  });
});
