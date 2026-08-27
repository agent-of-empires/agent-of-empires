// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { Dashboard } from "../Dashboard";
import type { SessionResponse } from "../../lib/types";

afterEach(cleanup);

function session(id: string, lastAccessedAt: string): SessionResponse {
  return {
    id,
    title: `Session ${id}`,
    project_path: `/repo/${id}`,
    main_repo_path: `/repo/${id}`,
    status: "Idle",
    created_at: "2026-01-01T00:00:00Z",
    last_accessed_at: lastAccessedAt,
    idle_entered_at: null,
    dormant: false,
    archived_at: null,
    snoozed_until: null,
    trashed_at: null,
  } as SessionResponse;
}

describe("Dashboard mobile recent sessions", () => {
  it("shows the five newest live sessions in recency order and opens the selected row", () => {
    const onSelectSession = vi.fn();
    const sessions = [
      session("old", "2026-01-02T00:00:00Z"),
      session("whole-second", "2026-01-07T00:00:00+00:00"),
      session("new", "2026-01-07T00:00:00.500+00:00"),
      session("middle", "2026-01-04T00:00:00Z"),
      session("four", "2026-01-05T00:00:00Z"),
      session("five", "2026-01-06T00:00:00Z"),
      session("six", "2026-01-03T00:00:00Z"),
      { ...session("trash", "2026-01-08T00:00:00Z"), trashed_at: "2026-01-08T00:00:00Z" },
    ];

    const { container } = render(
      <Dashboard
        sessions={sessions}
        onSelectSession={onSelectSession}
        onNewSession={vi.fn()}
        onCloneFromUrl={vi.fn()}
        onToggleSidebar={vi.fn()}
      />,
    );

    const recent = screen.getByLabelText("Recent sessions");
    expect(recent.className).toContain("md:hidden");
    expect(withinText(recent)).toEqual([
      "Session new",
      "Session whole-second",
      "Session five",
      "Session four",
      "Session middle",
    ]);
    expect(screen.queryByText("Session old")).toBeNull();
    expect(screen.queryByText("Session trash")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: /Session new/ }));
    expect(onSelectSession).toHaveBeenCalledWith("new");
    expect(container.querySelectorAll("section[aria-labelledby='recent-sessions-heading']")).toHaveLength(1);
  });
});

function withinText(section: HTMLElement): string[] {
  return Array.from(section.querySelectorAll("button span.font-mono.text-sm.font-medium")).map((el) => el.textContent);
}
