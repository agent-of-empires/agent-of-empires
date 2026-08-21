// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import type { SessionResponse } from "../../lib/types";

vi.mock("../acp/StructuredView", () => ({
  StructuredView: ({ sessionId, active }: { sessionId: string; active: boolean }) => (
    <div data-testid={`structured-${sessionId}`} data-active={String(active)}>
      {sessionId}
    </div>
  ),
}));

import { normalizePersistentStructuredViewLimit } from "../../lib/persistentStructuredViews";
import { StructuredViewStack } from "../StructuredViewStack";

afterEach(() => {
  cleanup();
});

function makeSession(id: string, view: "structured" | "terminal" = "structured"): SessionResponse {
  return {
    id,
    title: id,
    project_path: `/tmp/${id}`,
    artifact_dir: `/tmp/artifacts/${id}`,
    group_path: "/tmp",
    tool: "claude",
    status: "Running",
    dormant: false,
    yolo_mode: false,
    created_at: new Date().toISOString(),
    last_accessed_at: null,
    idle_entered_at: null,
    last_error: null,
    branch: null,
    main_repo_path: null,
    is_sandboxed: false,
    scratch: false,
    favorited: false,
    has_managed_worktree: false,
    has_terminal: false,
    profile: "default",
    cleanup_defaults: {
      delete_worktree: false,
      delete_branch: false,
      delete_sandbox: false,
    },
    remote_owner: null,
    remote_owner_key: null,
    notify_on_waiting: null,
    notify_on_idle: null,
    notify_on_error: null,
    claude_fullscreen: false,
    workspace_repos: [],
    view,
  };
}

describe("StructuredViewStack", () => {
  it("renders only the active session when persistence is disabled", () => {
    const sessions = [makeSession("s1"), makeSession("s2")];
    const { rerender } = render(
      <StructuredViewStack activeSessionId="s1" sessions={sessions} persistent={false} visible={true} />,
    );

    expect(screen.getByTestId("structured-s1").dataset.active).toBe("true");
    expect(screen.queryByTestId("structured-s2")).toBeNull();

    rerender(<StructuredViewStack activeSessionId="s2" sessions={sessions} persistent={false} visible={true} />);

    expect(screen.queryByTestId("structured-s1")).toBeNull();
    expect(screen.getByTestId("structured-s2").dataset.active).toBe("true");
  });

  it("renders nothing when the active session is terminal", () => {
    const sessions = [makeSession("s1", "terminal")];
    render(<StructuredViewStack activeSessionId="s1" sessions={sessions} persistent={false} visible={true} />);
    expect(screen.queryByTestId("structured-s1")).toBeNull();
  });

  it("keeps recent inactive structured sessions mounted when persistence is enabled", async () => {
    const sessions = [makeSession("s1"), makeSession("s2")];
    const { rerender } = render(
      <StructuredViewStack activeSessionId="s1" sessions={sessions} persistent={true} visible={true} />,
    );
    await waitFor(() => {
      expect(screen.getByTestId("structured-s1").dataset.active).toBe("true");
    });

    rerender(<StructuredViewStack activeSessionId="s2" sessions={sessions} persistent={true} visible={true} />);

    await waitFor(() => {
      expect(screen.getByTestId("structured-s1").dataset.active).toBe("false");
      expect(screen.getByTestId("structured-s2").dataset.active).toBe("true");
    });
  });

  it("marks inactive sessions as not interactive when the stack is not visible", async () => {
    const sessions = [makeSession("s1"), makeSession("s2")];
    const { rerender } = render(
      <StructuredViewStack activeSessionId="s1" sessions={sessions} persistent={true} visible={true} />,
    );
    await waitFor(() => {
      expect(screen.getByTestId("structured-s1").dataset.active).toBe("true");
    });

    rerender(<StructuredViewStack activeSessionId="s2" sessions={sessions} persistent={true} visible={false} />);

    await waitFor(() => {
      expect(screen.getByTestId("structured-s1").dataset.active).toBe("false");
      expect(screen.getByTestId("structured-s2").dataset.active).toBe("false");
    });
  });

  it("does not add terminal sessions to the recent list", async () => {
    const sessions = [makeSession("s1"), makeSession("s2", "terminal")];
    const { rerender } = render(
      <StructuredViewStack activeSessionId="s1" sessions={sessions} persistent={true} visible={true} />,
    );
    await waitFor(() => {
      expect(screen.getByTestId("structured-s1")).toBeDefined();
    });

    rerender(<StructuredViewStack activeSessionId="s2" sessions={sessions} persistent={true} visible={true} />);

    await waitFor(() => {
      expect(screen.queryByTestId("structured-s2")).toBeNull();
    });
  });

  it("evicts older inactive sessions beyond the configured limit", async () => {
    const sessions = [makeSession("s1"), makeSession("s2"), makeSession("s3")];
    const { rerender } = render(
      <StructuredViewStack
        activeSessionId="s1"
        sessions={sessions}
        persistent={true}
        visible={true}
        maxPersistentStructuredViews={2}
      />,
    );
    await waitFor(() => {
      expect(screen.getByTestId("structured-s1")).toBeDefined();
    });

    rerender(
      <StructuredViewStack
        activeSessionId="s2"
        sessions={sessions}
        persistent={true}
        visible={true}
        maxPersistentStructuredViews={2}
      />,
    );
    await waitFor(() => {
      expect(screen.getByTestId("structured-s1")).toBeDefined();
      expect(screen.getByTestId("structured-s2")).toBeDefined();
    });

    rerender(
      <StructuredViewStack
        activeSessionId="s3"
        sessions={sessions}
        persistent={true}
        visible={true}
        maxPersistentStructuredViews={1}
      />,
    );
    await waitFor(() => {
      expect(screen.queryByTestId("structured-s1")).toBeNull();
      expect(screen.queryByTestId("structured-s2")).toBeNull();
      expect(screen.getByTestId("structured-s3").dataset.active).toBe("true");
    });
  });

  it("counts the configured limit as the total mounted structured view count", async () => {
    const sessions = [makeSession("s1"), makeSession("s2"), makeSession("s3")];
    const { rerender } = render(
      <StructuredViewStack
        activeSessionId="s1"
        sessions={sessions}
        persistent={true}
        visible={true}
        maxPersistentStructuredViews={2}
      />,
    );
    await waitFor(() => {
      expect(screen.getByTestId("structured-s1")).toBeDefined();
    });

    rerender(
      <StructuredViewStack
        activeSessionId="s2"
        sessions={sessions}
        persistent={true}
        visible={true}
        maxPersistentStructuredViews={2}
      />,
    );
    await waitFor(() => {
      expect(screen.getByTestId("structured-s1")).toBeDefined();
      expect(screen.getByTestId("structured-s2")).toBeDefined();
    });

    rerender(
      <StructuredViewStack
        activeSessionId="s3"
        sessions={sessions}
        persistent={true}
        visible={true}
        maxPersistentStructuredViews={2}
      />,
    );
    await waitFor(() => {
      expect(screen.queryByTestId("structured-s1")).toBeNull();
      expect(screen.getByTestId("structured-s2").dataset.active).toBe("false");
      expect(screen.getByTestId("structured-s3").dataset.active).toBe("true");
    });
  });

  it("normalizes configured limits to the supported range", () => {
    expect(normalizePersistentStructuredViewLimit(0)).toBe(1);
    expect(normalizePersistentStructuredViewLimit(5.4)).toBe(5);
    expect(normalizePersistentStructuredViewLimit(99)).toBe(5);
    expect(normalizePersistentStructuredViewLimit("10")).toBe(2);
  });
});
