// @vitest-environment jsdom
//
// RTL + fetch-spy coverage for the sidebar "Fork session" affordance. Fork is
// offered only on a structured row that has captured an `acp_session_id` (the
// value the server forks from); clicking it POSTs a structured create with
// `fork_from` set to that id. Mirrors SessionRowTriage.test.tsx: mount the real
// SessionRow with a stubbed Workspace, open the context menu, and assert the
// createSession request payload.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { useMemo, useRef, type ReactNode } from "react";

import { DragSuppressContext, SessionRow, type RowBulkApi } from "../WorkspaceSidebar";
import { useSidebarTriage } from "../../hooks/useSidebarTriage";
import type { SessionResponse, Workspace } from "../../lib/types";

// Single-row stub for the bulk-triage bridge: these tests mount one unselected
// row, so the context menu is always single-scope.
const SINGLE_BULK_API: RowBulkApi = {
  prepareScope: () => ({ kind: "single" }),
  pin: () => {},
  archive: () => {},
  snooze: () => {},
};

function session(over: Partial<SessionResponse> = {}): SessionResponse {
  return {
    id: "s1",
    title: "row title",
    project_path: "/repo",
    group_path: "/repo",
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
    branch: null,
    projectPath: "/repo",
    displayName: id,
    agents: ["claude"],
    primaryAgent: "claude",
    status: "idle",
    sessions,
  };
}

function Wrap({ children }: { children: ReactNode }) {
  const ref = useRef(0);
  return <DragSuppressContext.Provider value={ref}>{children}</DragSuppressContext.Provider>;
}

function Row({ ws, readOnly }: { ws: Workspace; readOnly?: boolean }) {
  const workspaces = useMemo(() => [ws], [ws]);
  const triage = useSidebarTriage(workspaces);
  return (
    <SessionRow
      workspace={ws}
      isActive={false}
      isSelected={false}
      onActivate={() => {}}
      readOnly={readOnly}
      optimistic={triage.optimisticFor(ws.id)}
      onPinToggle={triage.pinToggle}
      onArchiveToggle={triage.archiveToggle}
      onSnooze={triage.snooze}
      onUnreadToggle={triage.unreadToggle}
      bulkApi={SINGLE_BULK_API}
    />
  );
}

const fetchSpy = vi.fn<typeof fetch>();

beforeEach(() => {
  fetchSpy.mockReset();
  vi.stubGlobal("fetch", fetchSpy);
  fetchSpy.mockImplementation(
    async () =>
      new Response(JSON.stringify({ id: "forked-1" }), {
        status: 200,
        headers: { "content-type": "application/json" },
      }),
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("SessionRow Fork affordance gating", () => {
  it("shows Fork on a structured, fork-capable row with a captured acp_session_id", () => {
    const ws = workspace("w-fork", [session({ view: "structured", acp_session_id: "acp-parent", acp_can_fork: true })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    expect(screen.queryByTestId("sidebar-context-menu-fork")).not.toBeNull();
  });

  it("hides Fork on a structured row with no captured acp_session_id", () => {
    const ws = workspace("w-no-id", [session({ view: "structured", acp_can_fork: true })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    expect(screen.queryByTestId("sidebar-context-menu-fork")).toBeNull();
  });

  it("hides Fork on a resume-only structured row (captured id but not fork-capable)", () => {
    // e.g. the bundled aoe-agent: advertises loadSession and mints an
    // acp_session_id, but does not advertise session/fork. Gating on the id
    // alone would offer a dead-end button that fails at the handshake.
    const ws = workspace("w-resume-only", [
      session({ view: "structured", acp_session_id: "acp-parent", acp_can_fork: false }),
    ]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    expect(screen.queryByTestId("sidebar-context-menu-fork")).toBeNull();
  });

  it("hides Fork on a terminal (tmux) row", () => {
    const ws = workspace("w-tmux", [session({ view: "terminal", acp_session_id: undefined })]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    expect(screen.queryByTestId("sidebar-context-menu-fork")).toBeNull();
  });

  it("hides Fork in read-only mode even for a forkable row", () => {
    const ws = workspace("w-ro", [session({ view: "structured", acp_session_id: "acp-parent", acp_can_fork: true })]);
    render(
      <Wrap>
        <Row ws={ws} readOnly />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    expect(screen.queryByTestId("sidebar-context-menu-fork")).toBeNull();
  });
});

describe("SessionRow Fork action payload", () => {
  it("Fork click POSTs createSession with fork_from and view: structured", async () => {
    const ws = workspace("w-fork", [
      session({
        id: "sess-fork-it",
        view: "structured",
        tool: "claude",
        project_path: "/repo",
        profile: "work",
        acp_session_id: "acp-parent-42",
        acp_can_fork: true,
      }),
    ]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    fireEvent.click(screen.getByTestId("sidebar-context-menu-fork"));

    await vi.waitFor(() => expect(fetchSpy).toHaveBeenCalled());
    const [url, init] = fetchSpy.mock.calls[0]!;
    expect(url).toBe("/api/sessions");
    expect(init?.method).toBe("POST");
    expect(JSON.parse(init!.body as string)).toEqual({
      path: "/repo",
      tool: "claude",
      view: "structured",
      profile: "work",
      fork_from: "acp-parent-42",
    });
  });
});
