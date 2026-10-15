// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";

import { Dashboard } from "../Dashboard";
import type { SessionResponse } from "../../lib/types";

function session(over: Partial<SessionResponse>): SessionResponse {
  return {
    id: "s",
    title: "t",
    project_path: "/repo",
    main_repo_path: "/repo",
    status: "Idle",
    archived_at: null,
    snoozed_until: null,
    trashed_at: null,
    ...over,
  } as SessionResponse;
}

function renderDashboard(sessions: SessionResponse[]) {
  return render(
    <Dashboard
      sessions={sessions}
      onSelectSession={vi.fn()}
      onNewSession={vi.fn()}
      onCloneFromUrl={vi.fn()}
      onToggleSidebar={vi.fn()}
    />,
  );
}

describe("Dashboard summary excludes trashed sessions (#2489)", () => {
  it("counts only live sessions, and suppresses the summary when all are trashed", () => {
    renderDashboard([session({ id: "a", status: "Error", trashed_at: "2026-07-26T00:00:00Z" })]);
    expect(screen.queryByText(/error/i)).toBeNull();
    expect(screen.queryByText(/across/i)).toBeNull();
    cleanup();

    renderDashboard([
      session({ id: "live", status: "Error", project_path: "/repo-a", main_repo_path: "/repo-a" }),
      session({
        id: "gone",
        status: "Error",
        project_path: "/repo-b",
        main_repo_path: "/repo-b",
        trashed_at: "2026-07-26T00:00:00Z",
      }),
    ]);
    expect(screen.getByText("1 error")).toBeTruthy();
    expect(screen.getByText(/1 session across 1 project/)).toBeTruthy();
  });
});
