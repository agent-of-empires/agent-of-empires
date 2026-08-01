// @vitest-environment jsdom
//
// RTL + fetch-spy coverage for the sidebar "Add project" affordance (#3103):
// attaching another repo to a session that already exists, so an agent that
// turns out to need a second repo keeps its conversation instead of the session
// being recreated. Mirrors ForkSessionAction.test.tsx: mount the real SessionRow
// with a stubbed Workspace, open the context menu, and assert the request
// payload and the modal's reporting of the worker outcome.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useMemo, useRef, type ReactNode } from "react";

import { DragSuppressContext, SessionRow, type RowBulkApi } from "../WorkspaceSidebar";
import { useSidebarTriage } from "../../hooks/useSidebarTriage";
import type { SessionResponse, Workspace } from "../../lib/types";

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

function json(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

/** The attach response shape, with the worker outcome the modal reports. */
function attachOk(worker: string, extra: Record<string, unknown> = {}) {
  return {
    session: null,
    attached: {
      name: "frontend",
      branch: "feature/abc",
      branch_created: true,
      moved_to: "/src/feature-abc-workspace-abcd1234",
    },
    warnings: [],
    worker,
    worker_message: null,
    ...extra,
  };
}

beforeEach(() => {
  fetchSpy.mockReset();
  vi.stubGlobal("fetch", fetchSpy);
  fetchSpy.mockImplementation(async (input) => {
    const url = typeof input === "string" ? input : (input as Request).url;
    if (url.includes("/api/projects")) {
      return json([{ name: "frontend", path: "/src/frontend", pinned: false, scope: "global" }]);
    }
    return json(attachOk("restarted"));
  });
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

async function openModal() {
  fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
  fireEvent.click(screen.getByTestId("sidebar-context-menu-add-project"));
  await waitFor(() => expect(screen.queryByTestId("add-project-modal")).not.toBeNull());
}

describe("SessionRow Add project affordance", () => {
  it("is offered on a normal row and hidden in read-only mode", () => {
    const ws = workspace("w1", [session()]);
    render(
      <Wrap>
        <Row ws={ws} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    expect(screen.queryByTestId("sidebar-context-menu-add-project")).not.toBeNull();

    cleanup();
    render(
      <Wrap>
        <Row ws={workspace("w2", [session()])} readOnly />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    expect(screen.queryByTestId("sidebar-context-menu-add-project")).toBeNull();
  });

  it("is hidden for a scratch session, which has no repo to attach to", () => {
    // A scratch session's cwd is a throwaway directory under the app dir, so
    // there is nothing for an attached repo to widen. The server refuses it in
    // `attach_project::plan`; hiding the entry keeps the menu honest instead
    // of offering an action that can only fail.
    render(
      <Wrap>
        <Row ws={workspace("w1", [session({ scratch: true })])} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    expect(screen.queryByTestId("sidebar-context-menu-add-project")).toBeNull();
    // Other entries still render, so this is the scratch gate rather than a
    // menu that failed to open.
    expect(screen.queryByTestId("sidebar-context-menu-rename")).not.toBeNull();
  });

  it.each([
    ["mid-create", { status: "Creating" as const }],
    ["archived", { archived_at: "2025-01-02T00:00:00Z" }],
    ["trashed", { trashed_at: "2025-01-02T00:00:00Z" }],
  ])("is hidden for a %s session", (_label, over) => {
    // All three are refused by `attach_project::plan`, so offering the entry
    // would only produce an error dialog.
    render(
      <Wrap>
        <Row ws={workspace("w1", [session(over)])} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    expect(screen.queryByTestId("sidebar-context-menu-add-project")).toBeNull();
    // The menu itself opened, so this is the per-entry gate rather than a row
    // that refuses to open a menu at all.
    expect(screen.queryByTestId("sidebar-context-menu-rename")).not.toBeNull();
  });

  it("cannot be reached on a mid-delete row, which opens no menu at all", () => {
    // Deleting is the state with teeth: the deletion pass has already read the
    // session's repo list, so a worktree created in that window is orphaned with
    // its record about to be dropped. The row suppresses the whole context menu,
    // and `plan` refuses it server-side for the surfaces that have no menu.
    render(
      <Wrap>
        <Row ws={workspace("w1", [session({ status: "Deleting" })])} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    expect(screen.queryByTestId("sidebar-context-menu-rename")).toBeNull();
    expect(screen.queryByTestId("sidebar-context-menu-add-project")).toBeNull();
  });

  it("stays offered while the agent is Running, which the server decides on the turn probe", () => {
    // Not filtered client-side: a Running session that is merely idle between
    // turns is attachable, and the 409 the server returns mid-turn is surfaced by
    // the modal. Filtering here would be coarser than the event-log probe.
    render(
      <Wrap>
        <Row ws={workspace("w1", [session({ status: "Running" })])} />
      </Wrap>,
    );
    fireEvent.contextMenu(screen.getByTestId("sidebar-session-row"));
    expect(screen.queryByTestId("sidebar-context-menu-add-project")).not.toBeNull();
  });

  it("warns that the working directory moves and the session stops before the user attaches", async () => {
    render(
      <Wrap>
        <Row ws={workspace("w1", [session({ id: "sess-9" })])} />
      </Wrap>,
    );
    await openModal();

    const warning = screen.getByTestId("add-project-modal-restart-warning");
    expect(warning.textContent).toContain("working directory moves");
    expect(warning.textContent).toContain("stopped for the move and started again");
    expect(warning.textContent).toContain("conversation is kept");
  });

  it("posts the project with existing-branch reuse off by default", async () => {
    render(
      <Wrap>
        <Row ws={workspace("w1", [session({ id: "sess-9" })])} />
      </Wrap>,
    );
    await openModal();

    fireEvent.change(screen.getByTestId("add-project-modal-input"), {
      target: { value: "frontend" },
    });
    fireEvent.click(screen.getByTestId("add-project-modal-submit"));

    await waitFor(() => {
      const call = fetchSpy.mock.calls.find(([url]) => String(url).includes("/api/sessions/sess-9/projects"));
      expect(call).toBeDefined();
      const init = call![1] as RequestInit;
      expect(init.method).toBe("POST");
      expect(JSON.parse(init.body as string)).toEqual({
        project: "frontend",
        attach_existing_branch: false,
      });
    });
  });

  it("sends attach_existing_branch when the user opts in", async () => {
    render(
      <Wrap>
        <Row ws={workspace("w1", [session({ id: "sess-9" })])} />
      </Wrap>,
    );
    await openModal();

    fireEvent.change(screen.getByTestId("add-project-modal-input"), {
      target: { value: "/src/frontend" },
    });
    fireEvent.click(screen.getByTestId("add-project-modal-attach-existing-branch"));
    fireEvent.click(screen.getByTestId("add-project-modal-submit"));

    await waitFor(() => {
      const call = fetchSpy.mock.calls.find(([url]) => String(url).includes("/api/sessions/sess-9/projects"));
      expect(JSON.parse((call![1] as RequestInit).body as string).attach_existing_branch).toBe(true);
    });
  });

  it("does not post an empty project", async () => {
    render(
      <Wrap>
        <Row ws={workspace("w1", [session({ id: "sess-9" })])} />
      </Wrap>,
    );
    await openModal();
    fireEvent.click(screen.getByTestId("add-project-modal-submit"));

    await waitFor(() => expect(screen.queryByTestId("add-project-modal-error")).not.toBeNull());
    // Scoped to the session path: the registry fetch that populates the picker
    // is also under /api/projects, so a looser matcher would always match.
    expect(fetchSpy.mock.calls.some(([url]) => String(url).includes("/api/sessions/sess-9/projects"))).toBe(false);
  });

  it("ignores Escape and backdrop clicks while the attach is in flight", async () => {
    // The attach lands and the session restarts regardless, so dismissing
    // mid-POST would throw away the result and the "did not restart" notice.
    let release: (v: Response) => void = () => {};
    fetchSpy.mockImplementation(async (input) => {
      const url = typeof input === "string" ? input : (input as Request).url;
      if (url.includes("/api/projects")) return json([]);
      return new Promise<Response>((resolve) => {
        release = resolve;
      });
    });
    render(
      <Wrap>
        <Row ws={workspace("w1", [session({ id: "sess-9" })])} />
      </Wrap>,
    );
    await openModal();
    fireEvent.change(screen.getByTestId("add-project-modal-input"), {
      target: { value: "frontend" },
    });
    fireEvent.click(screen.getByTestId("add-project-modal-submit"));
    await waitFor(() => expect(screen.getByTestId("add-project-modal-submit").textContent).toContain("Attaching"));

    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByTestId("add-project-modal")).not.toBeNull();
    fireEvent.click(screen.getByTestId("add-project-modal-backdrop"));
    expect(screen.queryByTestId("add-project-modal")).not.toBeNull();

    release(json(attachOk("restarted")));
    await waitFor(() => expect(screen.queryByTestId("add-project-modal-result")).not.toBeNull());

    // Once the result is showing, Escape closes normally again.
    fireEvent.keyDown(document, { key: "Escape" });
    await waitFor(() => expect(screen.queryByTestId("add-project-modal")).toBeNull());
  });

  it("reports a failed restart instead of implying the agent can see the repo", async () => {
    // The server answers 200 here: the repo is attached and durable, only the
    // respawn failed. Closing the modal silently would tell the user the agent
    // picked the repo up when it did not.
    fetchSpy.mockImplementation(async (input) => {
      const url = typeof input === "string" ? input : (input as Request).url;
      if (url.includes("/api/projects")) return json([]);
      return json(attachOk("restart_failed", { worker_message: "worker respawn failed: boom" }));
    });
    render(
      <Wrap>
        <Row ws={workspace("w1", [session({ id: "sess-9" })])} />
      </Wrap>,
    );
    await openModal();
    fireEvent.change(screen.getByTestId("add-project-modal-input"), {
      target: { value: "frontend" },
    });
    fireEvent.click(screen.getByTestId("add-project-modal-submit"));

    const result = await waitFor(() => screen.getByTestId("add-project-modal-result"));
    expect(result.textContent).toContain("frontend");
    expect(screen.getByTestId("add-project-modal").textContent).toContain("did not restart");
  });

  it("reports the new working directory when the attach converted the session", async () => {
    // The one user-visible consequence of the conversion: anything the user had
    // open at the old path, a terminal or an editor, is now looking at a
    // directory the session no longer works in.
    render(
      <Wrap>
        <Row ws={workspace("w1", [session({ id: "sess-9" })])} />
      </Wrap>,
    );
    await openModal();
    fireEvent.change(screen.getByTestId("add-project-modal-input"), {
      target: { value: "frontend" },
    });
    fireEvent.click(screen.getByTestId("add-project-modal-submit"));

    const moved = await waitFor(() => screen.getByTestId("add-project-modal-moved-to"));
    expect(moved.textContent).toContain("/src/feature-abc-workspace-abcd1234");
    expect(moved.textContent).toContain("multi-repo workspace");
  });

  it("surfaces the server's refusal message on a rejected attach", async () => {
    fetchSpy.mockImplementation(async (input) => {
      const url = typeof input === "string" ? input : (input as Request).url;
      if (url.includes("/api/projects")) return json([]);
      return json({ message: "branch 'feature/abc' already exists in the repo being attached" }, 400);
    });
    render(
      <Wrap>
        <Row ws={workspace("w1", [session({ id: "sess-9" })])} />
      </Wrap>,
    );
    await openModal();
    fireEvent.change(screen.getByTestId("add-project-modal-input"), {
      target: { value: "frontend" },
    });
    fireEvent.click(screen.getByTestId("add-project-modal-submit"));

    const err = await waitFor(() => screen.getByTestId("add-project-modal-error"));
    expect(err.textContent).toContain("already exists");
  });
});
