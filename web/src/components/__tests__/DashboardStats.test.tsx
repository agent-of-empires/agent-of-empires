// @vitest-environment jsdom
//
// Regression guard for #2489: the dashboard's returning-user summary must not
// count trashed sessions. A session left in an Error state does not matter once
// it is in the trash, so the "N errors" / "N sessions across M projects" line
// has to exclude anything carrying `trashed_at`.
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";

import { Dashboard } from "../Dashboard";
import type { SessionResponse } from "../../lib/types";

afterEach(() => {
  cleanup();
});

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
  it("does not count a trashed session's Error status", () => {
    renderDashboard([session({ id: "a", status: "Error", trashed_at: "2026-07-26T00:00:00Z" })]);
    // The only session is trashed, so the whole summary line is suppressed:
    // no error count and no "N sessions across M projects" tally.
    expect(screen.queryByText(/error/i)).toBeNull();
    expect(screen.queryByText(/across/i)).toBeNull();
  });

  it("counts only live sessions in the totals", () => {
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
    // One live error, one trashed error: the summary reports a single error and
    // a single session across a single project.
    expect(screen.getByText("1 error")).toBeTruthy();
    expect(screen.getByText(/1 session across 1 project/)).toBeTruthy();
  });
});
