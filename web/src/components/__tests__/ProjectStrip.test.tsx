// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@testing-library/react";
import { ProjectStrip } from "../ProjectStrip";
import type { RepoGroup, SessionResponse } from "../../lib/types";

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

describe("ProjectStrip", () => {
  it("renders project tabs and selects the first workspace in the group", () => {
    const onSelectWorkspace = vi.fn();
    const { getByRole } = render(
      <ProjectStrip
        groups={[
          group("Alpha", "/tmp/alpha", "Running"),
          group("Beta", "/tmp/beta", "Idle"),
        ]}
        activeSessionId="Alpha-session"
        activeWorkspaceId="Alpha-workspace"
        onSelectWorkspace={onSelectWorkspace}
        onSelectSession={vi.fn()}
        onCreateSession={vi.fn()}
      />,
    );

    const beta = getByRole("tab", { name: /Beta/i });
    expect(beta.getAttribute("aria-selected")).toBe("false");

    fireEvent.click(beta);
    expect(onSelectWorkspace).toHaveBeenCalledWith("Beta-workspace");
  });

  it("marks the active project tab", () => {
    const { getByRole } = render(
      <ProjectStrip
        groups={[group("Alpha", "/tmp/alpha", "Running")]}
        activeSessionId="Alpha-session"
        activeWorkspaceId="Alpha-workspace"
        onSelectWorkspace={vi.fn()}
        onSelectSession={vi.fn()}
        onCreateSession={vi.fn()}
      />,
    );

    expect(
      getByRole("tab", { name: /Alpha/i }).getAttribute("aria-selected"),
    ).toBe("true");
  });

  it("renders selected project sessions and selects a specific session", () => {
    const onSelectSession = vi.fn();
    const alpha = group("Alpha", "/tmp/alpha", "Running");
    alpha.workspaces[0]!.sessions.push(session("Alpha-second", "/tmp/alpha", "Waiting"));

    const { getByRole } = render(
      <ProjectStrip
        groups={[alpha]}
        activeSessionId="Alpha-session"
        activeWorkspaceId="Alpha-workspace"
        onSelectWorkspace={vi.fn()}
        onSelectSession={onSelectSession}
        onCreateSession={vi.fn()}
      />,
    );

    fireEvent.click(getByRole("button", { name: /Alpha-second/i }));
    expect(onSelectSession).toHaveBeenCalledWith("Alpha-second");
  });

  it("filters projects by project and session details", () => {
    const beta = group("Beta", "/tmp/beta", "Idle");
    beta.workspaces[0]!.sessions[0]!.branch = "feature/searchable";
    beta.workspaces[0]!.sessions[0]!.tool = "codex";

    const { getByLabelText, getByRole, queryByRole } = render(
      <ProjectStrip
        groups={[group("Alpha", "/tmp/alpha", "Running"), beta]}
        activeSessionId="Alpha-session"
        activeWorkspaceId="Alpha-workspace"
        onSelectWorkspace={vi.fn()}
        onSelectSession={vi.fn()}
        onCreateSession={vi.fn()}
      />,
    );

    fireEvent.change(getByLabelText("Filter project strip"), {
      target: { value: "searchable" },
    });

    expect(queryByRole("tab", { name: /Alpha/i })).toBeNull();
    expect(getByRole("tab", { name: /Beta/i })).toBeTruthy();
  });

  it("starts a new session from the project row without selecting it", () => {
    const onCreateSession = vi.fn();
    const onSelectWorkspace = vi.fn();

    const { getByLabelText } = render(
      <ProjectStrip
        groups={[group("Alpha", "/tmp/alpha", "Running")]}
        activeSessionId="Alpha-session"
        activeWorkspaceId="Alpha-workspace"
        onSelectWorkspace={onSelectWorkspace}
        onSelectSession={vi.fn()}
        onCreateSession={onCreateSession}
      />,
    );

    fireEvent.click(getByLabelText("New session in Alpha"));

    expect(onCreateSession).toHaveBeenCalledWith("/tmp/alpha");
    expect(onSelectWorkspace).not.toHaveBeenCalled();
  });
});
