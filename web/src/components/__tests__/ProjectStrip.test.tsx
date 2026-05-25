// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, waitFor } from "@testing-library/react";
import { ProjectStrip } from "../ProjectStrip";
import type { RepoGroup, SessionResponse } from "../../lib/types";
import type { RepoAppearanceUpdate } from "../../lib/repoAppearance";

afterEach(() => {
  vi.restoreAllMocks();
});

function session(
  id: string,
  projectPath: string,
  status: SessionResponse["status"] = "Idle",
): SessionResponse {
  return {
    id,
    title: id,
    project_path: projectPath,
    group_path: projectPath,
    tool: "claude",
    status,
    yolo_mode: false,
    created_at: "2026-05-25T00:00:00Z",
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
    workspace_repos: [],
    claude_fullscreen: false,
  };
}

function group(name: string, path: string, status: SessionResponse["status"]): RepoGroup {
  const s = session(`${name}-session`, path, status);
  return {
    id: path,
    repoPath: path,
    displayName: name,
    defaultDisplayName: name,
    alias: null,
    color: null,
    remoteOwner: null,
    status: status === "Running" ? "active" : "idle",
    collapsed: false,
    workspaces: [
      {
        id: `${name}-workspace`,
        branch: null,
        projectPath: path,
        displayName: name,
        agents: ["claude"],
        primaryAgent: "claude",
        status: status === "Running" ? "active" : "idle",
        sessions: [s],
      },
    ],
  };
}

function renderProjectStrip({
  groups,
  activeSessionId = "Alpha-session",
  activeWorkspaceId = "Alpha-workspace",
  onSelectWorkspace = vi.fn(),
  onSelectSession = vi.fn(),
  onCreateSession = vi.fn(),
  onDeleteSession = vi.fn(),
  onReorderWorkspaces = vi.fn(),
  onUpdateAppearance = vi.fn(),
}: {
  groups: RepoGroup[];
  activeSessionId?: string | null;
  activeWorkspaceId?: string | null;
  onSelectWorkspace?: (workspaceId: string) => void;
  onSelectSession?: (sessionId: string) => void;
  onCreateSession?: (repoPath: string) => void;
  onDeleteSession?: (workspaceId: string) => void;
  onReorderWorkspaces?: (newOrder: string[]) => void;
  onUpdateAppearance?: (repoId: string, update: RepoAppearanceUpdate) => void;
}) {
  return render(
    <ProjectStrip
      groups={groups}
      activeSessionId={activeSessionId}
      activeWorkspaceId={activeWorkspaceId}
      onSelectWorkspace={onSelectWorkspace}
      onSelectSession={onSelectSession}
      onCreateSession={onCreateSession}
      onDeleteSession={onDeleteSession}
      onReorderWorkspaces={onReorderWorkspaces}
      onUpdateAppearance={onUpdateAppearance}
    />,
  );
}

describe("ProjectStrip", () => {
  it("renders project tabs and selects the first workspace in the group", () => {
    const onSelectWorkspace = vi.fn();
    const { getAllByTestId } = renderProjectStrip({
      groups: [
        group("Alpha", "/tmp/alpha", "Running"),
        group("Beta", "/tmp/beta", "Idle"),
      ],
      onSelectWorkspace,
    });

    const beta = getAllByTestId("project-strip-tab").find((tab) =>
      tab.textContent?.includes("Beta"),
    );
    expect(beta).toBeTruthy();
    expect(beta?.getAttribute("aria-current")).toBeNull();

    fireEvent.click(beta!);
    expect(onSelectWorkspace).toHaveBeenCalledWith("Beta-workspace");
  });

  it("marks the active project tab", () => {
    const { getAllByTestId } = renderProjectStrip({
      groups: [group("Alpha", "/tmp/alpha", "Running")],
    });

    const alpha = getAllByTestId("project-strip-tab").find((tab) =>
      tab.textContent?.includes("Alpha"),
    );
    expect(alpha?.getAttribute("aria-current")).toBe("page");
  });

  it("renders selected project sessions and selects a specific session", () => {
    const onSelectSession = vi.fn();
    const alpha = group("Alpha", "/tmp/alpha", "Running");
    alpha.workspaces[0]!.sessions.push(session("Alpha-second", "/tmp/alpha", "Waiting"));

    const { getByRole } = renderProjectStrip({
      groups: [alpha],
      onSelectSession,
    });

    fireEvent.click(getByRole("button", { name: /Alpha-second/i }));
    expect(onSelectSession).toHaveBeenCalledWith("Alpha-second");
  });

  it("keeps unknown statuses behind known statuses when choosing the project status", () => {
    const alpha = group("Alpha", "/tmp/alpha", "Idle");
    alpha.workspaces[0]!.sessions = [
      {
        ...session("Alpha-unknown", "/tmp/alpha", "Unknown"),
        status: "FutureStatus" as SessionResponse["status"],
      },
      session("Alpha-waiting", "/tmp/alpha", "Waiting"),
    ];

    const { getByTestId } = renderProjectStrip({
      groups: [alpha],
      activeSessionId: "Alpha-waiting",
    });

    expect(getByTestId("project-strip-tab").getAttribute("title")).toContain(
      "Waiting",
    );
  });

  it("marks projects with recently finished sessions", () => {
    const alpha = group("Alpha", "/tmp/alpha", "Idle");
    alpha.workspaces[0]!.sessions[0]!.idle_entered_at = new Date().toISOString();

    const { getByLabelText } = renderProjectStrip({ groups: [alpha] });

    expect(getByLabelText("Recently finished session in project")).toBeTruthy();
  });

  it("marks projects with running sessions distinctly", () => {
    const { getByLabelText } = renderProjectStrip({
      groups: [group("Alpha", "/tmp/alpha", "Running")],
    });

    expect(getByLabelText("Running session in project")).toBeTruthy();
  });

  it("deduplicates repeated sessions in the selected project session row", () => {
    const alpha = group("Alpha", "/tmp/alpha", "Running");
    const duplicate = alpha.workspaces[0]!.sessions[0]!;
    alpha.workspaces.push({
      id: "Alpha-workspace-duplicate",
      branch: "feature/dup",
      projectPath: "/tmp/alpha",
      displayName: "feature/dup",
      agents: ["claude"],
      primaryAgent: "claude",
      status: "active",
      sessions: [duplicate],
    });

    const { getAllByTestId } = renderProjectStrip({ groups: [alpha] });

    expect(getAllByTestId("project-strip-session")).toHaveLength(1);
  });

  it("keeps selected project session chips to one visible label", () => {
    const alpha = group("Alpha", "/tmp/alpha", "Running");
    alpha.workspaces[0]!.branch = "feature/alpha";
    alpha.workspaces[0]!.displayName = "feature/alpha";
    alpha.workspaces[0]!.sessions[0]!.title = "Build UI";

    const { getByTestId, queryByText } = renderProjectStrip({ groups: [alpha] });

    expect(getByTestId("project-strip-session").textContent).toContain("Build UI");
    expect(queryByText("feature/alpha")).toBeNull();
  });

  it("starts a new session from the project options menu without selecting it", () => {
    const onCreateSession = vi.fn();
    const onSelectWorkspace = vi.fn();

    const { getByRole, getByTestId } = renderProjectStrip({
      groups: [group("Alpha", "/tmp/alpha", "Running")],
      onCreateSession,
      onSelectWorkspace,
    });

    fireEvent.doubleClick(getByTestId("project-strip-tab"));
    fireEvent.click(getByRole("menuitem", { name: /New session/i }));

    expect(onCreateSession).toHaveBeenCalledWith("/tmp/alpha");
    expect(onSelectWorkspace).not.toHaveBeenCalled();
  });

  it("renames a project from the project options menu", () => {
    const onUpdateAppearance = vi.fn();
    const { getByRole, getByTestId } = renderProjectStrip({
      groups: [group("Alpha", "/tmp/alpha", "Running")],
      onUpdateAppearance,
    });

    fireEvent.doubleClick(getByTestId("project-strip-tab"));
    fireEvent.click(getByRole("menuitem", { name: /Rename project/i }));
    const input = getByTestId("project-strip-rename-input");
    fireEvent.change(input, { target: { value: "Client Alpha" } });
    fireEvent.keyDown(input, { key: "Enter" });

    expect(onUpdateAppearance).toHaveBeenCalledWith("/tmp/alpha", {
      alias: "Client Alpha",
    });
  });

  it("changes project color from the project options menu", () => {
    const onUpdateAppearance = vi.fn();
    const { getByTestId } = renderProjectStrip({
      groups: [group("Alpha", "/tmp/alpha", "Running")],
      onUpdateAppearance,
    });

    fireEvent.doubleClick(getByTestId("project-strip-tab"));
    fireEvent.click(getByTestId("project-strip-color-amber"));

    expect(onUpdateAppearance).toHaveBeenCalledWith("/tmp/alpha", {
      color: "amber",
    });
  });

  it("opens the delete flow from the project options menu", () => {
    const onDeleteSession = vi.fn();
    const { getByRole, getByTestId } = renderProjectStrip({
      groups: [group("Alpha", "/tmp/alpha", "Running")],
      onDeleteSession,
    });

    fireEvent.doubleClick(getByTestId("project-strip-tab"));
    fireEvent.click(getByRole("menuitem", { name: /Delete current session/i }));

    expect(onDeleteSession).toHaveBeenCalledWith("Alpha-workspace");
  });

  it("renames a session from the selected project session menu", async () => {
    const fetchMock = vi
      .spyOn(globalThis, "fetch")
      .mockResolvedValue({ ok: true } as Response);
    const { getByRole, getByTestId } = renderProjectStrip({
      groups: [group("Alpha", "/tmp/alpha", "Running")],
    });

    fireEvent.contextMenu(getByTestId("project-strip-session"));
    fireEvent.click(getByRole("menuitem", { name: "Rename" }));
    const input = getByTestId("project-strip-session-rename-input");
    fireEvent.change(input, { target: { value: "Review patch" } });
    fireEvent.keyDown(input, { key: "Enter" });

    expect(fetchMock).toHaveBeenCalledWith(
      "/api/sessions/Alpha-session",
      expect.objectContaining({
        method: "PATCH",
        body: JSON.stringify({ title: "Review patch" }),
      }),
    );
    await waitFor(() =>
      expect(getByTestId("project-strip-session").textContent).toContain(
        "Review patch",
      ),
    );
  });

  it("updates session notification state from the selected project session menu", async () => {
    const fetchMock = vi
      .spyOn(globalThis, "fetch")
      .mockResolvedValue({ ok: true } as Response);
    const { getByRole, getByTestId } = renderProjectStrip({
      groups: [group("Alpha", "/tmp/alpha", "Running")],
    });

    fireEvent.contextMenu(getByTestId("project-strip-session"));
    fireEvent.click(getByRole("menuitem", { name: "All events" }));

    expect(fetchMock).toHaveBeenCalledWith(
      "/api/sessions/Alpha-session/notifications",
      expect.objectContaining({
        method: "PATCH",
        body: JSON.stringify({
          notify_on_waiting: true,
          notify_on_idle: true,
          notify_on_error: true,
        }),
      }),
    );

    fireEvent.contextMenu(getByTestId("project-strip-session"));
    expect(getByRole("menuitem", { name: /All events/ })).toBeTruthy();
  });

  it("does not mutate project or session actions in read-only mode", () => {
    const onUpdateAppearance = vi.fn();
    const { getByRole, getByTestId, queryByTestId } = render(
      <ProjectStrip
        groups={[group("ReadOnly", "/tmp/read-only", "Running")]}
        activeSessionId="ReadOnly-session"
        activeWorkspaceId="ReadOnly-workspace"
        onSelectWorkspace={vi.fn()}
        onSelectSession={vi.fn()}
        onCreateSession={vi.fn()}
        onDeleteSession={vi.fn()}
        onReorderWorkspaces={vi.fn()}
        onUpdateAppearance={onUpdateAppearance}
        readOnly
      />,
    );

    fireEvent.doubleClick(getByTestId("project-strip-tab"));
    fireEvent.click(getByRole("menuitem", { name: /Rename project/i }));
    expect(queryByTestId("project-strip-rename-input")).toBeNull();
    fireEvent.click(getByTestId("project-strip-color-amber"));
    expect(onUpdateAppearance).not.toHaveBeenCalled();

    fireEvent.contextMenu(getByTestId("project-strip-session"));
    fireEvent.click(getByRole("menuitem", { name: "Rename" }));
    expect(queryByTestId("project-strip-session-rename-input")).toBeNull();
  });

  it("shows sidebar-equivalent session menu actions", () => {
    const onDeleteSession = vi.fn();
    const { getByRole, getByTestId } = renderProjectStrip({
      groups: [group("Alpha", "/tmp/alpha", "Running")],
      onDeleteSession,
    });

    fireEvent.contextMenu(getByTestId("project-strip-session"));

    expect(getByRole("menuitem", { name: "Rename" })).toBeTruthy();
    expect(getByRole("menuitem", { name: "Off" })).toBeTruthy();
    expect(getByRole("menuitem", { name: /Default/ })).toBeTruthy();
    expect(getByRole("menuitem", { name: "All events" })).toBeTruthy();
    fireEvent.click(getByTestId("project-strip-session-menu-delete"));
    expect(onDeleteSession).toHaveBeenCalledWith("Alpha-workspace");
  });

  it("closes the project options menu when Escape is pressed", async () => {
    const { getByTestId, queryByTestId } = renderProjectStrip({
      groups: [group("Alpha", "/tmp/alpha", "Running")],
    });

    fireEvent.doubleClick(getByTestId("project-strip-tab"));
    expect(queryByTestId("project-strip-menu")).toBeTruthy();
    await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
    fireEvent.keyDown(document, { key: "Escape" });
    expect(queryByTestId("project-strip-menu")).toBeNull();
  });

  it("keeps project tabs focused on names instead of summary metadata", () => {
    const { queryByText, getByTestId, queryByLabelText } = renderProjectStrip({
      groups: [group("Alpha", "/tmp/alpha", "Running")],
    });

    expect(queryByText(/projects/i)).toBeNull();
    expect(queryByText(/sessions/i)).toBeNull();
    expect(queryByLabelText("Filter project strip")).toBeNull();
    expect(getByTestId("project-strip-tab").textContent).toBe("Alpha");
  });

  it("does not render agent tool names in the compact strip", () => {
    const alpha = group("Alpha", "/tmp/alpha", "Running");
    alpha.workspaces[0]!.primaryAgent = "cursor";
    alpha.workspaces[0]!.sessions[0]!.tool = "codex";

    const { queryByText } = renderProjectStrip({ groups: [alpha] });

    expect(queryByText(/cursor/i)).toBeNull();
    expect(queryByText(/codex/i)).toBeNull();
  });
});
