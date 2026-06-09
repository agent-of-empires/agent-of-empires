// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import type { SessionResponse } from "../../lib/types";

vi.mock("../TerminalView", () => ({
  TerminalView: ({ session, active }: { session: SessionResponse; active: boolean }) => (
    <div data-testid={`terminal-${session.id}`} data-active={String(active)}>
      {session.title}
    </div>
  ),
}));

import { normalizePersistentTerminalLimit } from "../../lib/persistentTerminals";
import { TerminalSessionStack } from "../TerminalSessionStack";

afterEach(() => {
  cleanup();
});

function makeSession(id: string): SessionResponse {
  return {
    id,
    title: id,
    project_path: `/tmp/${id}`,
    group_path: "/tmp",
    tool: "claude",
    status: "Running",
    yolo_mode: false,
    created_at: new Date().toISOString(),
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
  };
}

describe("TerminalSessionStack", () => {
  it("renders only the active session when persistence is disabled", () => {
    const sessions = [makeSession("s1"), makeSession("s2")];
    const { rerender } = render(<TerminalSessionStack activeSessionId="s1" sessions={sessions} persistent={false} />);

    expect(screen.getByTestId("terminal-s1").dataset.active).toBe("true");
    expect(screen.queryByTestId("terminal-s2")).toBeNull();

    rerender(<TerminalSessionStack activeSessionId="s2" sessions={sessions} persistent={false} />);

    expect(screen.queryByTestId("terminal-s1")).toBeNull();
    expect(screen.getByTestId("terminal-s2").dataset.active).toBe("true");
  });

  it("keeps recent inactive sessions mounted when persistence is enabled", async () => {
    const sessions = [makeSession("s1"), makeSession("s2")];
    const { rerender } = render(<TerminalSessionStack activeSessionId="s1" sessions={sessions} persistent={true} />);
    await waitFor(() => {
      expect(screen.getByTestId("terminal-s1").dataset.active).toBe("true");
    });

    rerender(<TerminalSessionStack activeSessionId="s2" sessions={sessions} persistent={true} />);

    await waitFor(() => {
      expect(screen.getByTestId("terminal-s1").dataset.active).toBe("false");
      expect(screen.getByTestId("terminal-s2").dataset.active).toBe("true");
    });
  });

  it("evicts older inactive sessions beyond the configured limit", async () => {
    const sessions = [makeSession("s1"), makeSession("s2"), makeSession("s3")];
    const { rerender } = render(
      <TerminalSessionStack activeSessionId="s1" sessions={sessions} persistent={true} maxPersistentTerminals={2} />,
    );
    await waitFor(() => {
      expect(screen.getByTestId("terminal-s1")).toBeDefined();
    });

    rerender(
      <TerminalSessionStack activeSessionId="s2" sessions={sessions} persistent={true} maxPersistentTerminals={2} />,
    );
    await waitFor(() => {
      expect(screen.getByTestId("terminal-s1")).toBeDefined();
      expect(screen.getByTestId("terminal-s2")).toBeDefined();
    });

    rerender(
      <TerminalSessionStack activeSessionId="s3" sessions={sessions} persistent={true} maxPersistentTerminals={1} />,
    );
    await waitFor(() => {
      expect(screen.queryByTestId("terminal-s1")).toBeNull();
      expect(screen.queryByTestId("terminal-s2")).toBeNull();
      expect(screen.getByTestId("terminal-s3").dataset.active).toBe("true");
    });
  });

  it("counts the configured limit as the total mounted terminal count", async () => {
    const sessions = [makeSession("s1"), makeSession("s2"), makeSession("s3")];
    const { rerender } = render(
      <TerminalSessionStack activeSessionId="s1" sessions={sessions} persistent={true} maxPersistentTerminals={2} />,
    );
    await waitFor(() => {
      expect(screen.getByTestId("terminal-s1")).toBeDefined();
    });

    rerender(
      <TerminalSessionStack activeSessionId="s2" sessions={sessions} persistent={true} maxPersistentTerminals={2} />,
    );
    await waitFor(() => {
      expect(screen.getByTestId("terminal-s1")).toBeDefined();
      expect(screen.getByTestId("terminal-s2")).toBeDefined();
    });

    rerender(
      <TerminalSessionStack activeSessionId="s3" sessions={sessions} persistent={true} maxPersistentTerminals={2} />,
    );
    await waitFor(() => {
      expect(screen.queryByTestId("terminal-s1")).toBeNull();
      expect(screen.getByTestId("terminal-s2").dataset.active).toBe("false");
      expect(screen.getByTestId("terminal-s3").dataset.active).toBe("true");
    });
  });

  it("normalizes configured limits to the supported range", () => {
    expect(normalizePersistentTerminalLimit(0)).toBe(1);
    expect(normalizePersistentTerminalLimit(5.4)).toBe(5);
    expect(normalizePersistentTerminalLimit(99)).toBe(50);
    expect(normalizePersistentTerminalLimit("10")).toBe(5);
  });
});
