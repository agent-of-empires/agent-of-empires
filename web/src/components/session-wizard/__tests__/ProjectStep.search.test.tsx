// @vitest-environment jsdom
//
// #3461: the Recent tab has a search box that filters across the whole
// saved + recent list. With an empty query the recents stay capped at
// RECENT_CAP (6) so the tab reads as a short "jump back in" list; typing
// searches the full merged list, so a project below the cap is reachable
// without falling back to the Browse tab.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { ProjectStep } from "../steps/ProjectStep";
import { initialData } from "../wizardReducer";
import type { ProjectInfo } from "../../../lib/types";
import type { RecentProjectEntry } from "../../../lib/api";

vi.mock("../../../lib/api", () => ({
  fetchSessions: vi.fn().mockResolvedValue({ sessions: [], workspace_ordering: [] }),
  fetchRecentProjects: vi.fn(),
  fetchProjects: vi.fn(),
  cloneRepo: vi.fn(),
}));

import { fetchRecentProjects, fetchProjects } from "../../../lib/api";

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

// Eight recents, newest first, so `zebra` lands at index 7 and is below the
// six-row cap. Names are distinct enough that a substring hits exactly one.
const NAMES = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "zebra"];

function recents(): RecentProjectEntry[] {
  return NAMES.map((name, i) => ({
    path: `/repo/${name}`,
    display_name: name,
    tool: "claude",
    // Descending timestamps keep the array order after the merge sort.
    last_used_at: `2025-09-${String(20 - i).padStart(2, "0")}T00:00:00+00:00`,
  }));
}

function savedProject(): ProjectInfo {
  return { name: "saved-yankee", path: "/repo/saved-yankee", scope: "global" } as ProjectInfo;
}

function renderStep() {
  return render(
    <ProjectStep data={{ ...initialData, path: "", extraRepoPaths: [], scratch: false }} onChange={vi.fn()} />,
  );
}

describe("ProjectStep project search (#3461)", () => {
  it("caps recents with an empty query and finds a below-the-cap project by typing", async () => {
    vi.mocked(fetchRecentProjects).mockResolvedValue({ projects: recents() });
    vi.mocked(fetchProjects).mockResolvedValue([]);

    renderStep();
    const box = await screen.findByLabelText("Search projects");

    // Only the first six recents render before any query.
    expect(await screen.findByText("alpha")).toBeTruthy();
    expect(screen.getByText("foxtrot")).toBeTruthy();
    expect(screen.queryByText("golf")).toBeNull();
    expect(screen.queryByText("zebra")).toBeNull();

    // Typing reaches past the cap.
    fireEvent.change(box, { target: { value: "zeb" } });
    expect(await screen.findByText("zebra")).toBeTruthy();
    expect(screen.queryByText("alpha")).toBeNull();

    // Clearing restores the capped list.
    fireEvent.change(box, { target: { value: "" } });
    expect(await screen.findByText("alpha")).toBeTruthy();
    expect(screen.queryByText("zebra")).toBeNull();
  });

  it("matches on path as well as display name", async () => {
    vi.mocked(fetchRecentProjects).mockResolvedValue({ projects: recents() });
    vi.mocked(fetchProjects).mockResolvedValue([]);

    renderStep();
    const box = await screen.findByLabelText("Search projects");

    fireEvent.change(box, { target: { value: "/repo/golf" } });
    expect(await screen.findByText("golf")).toBeTruthy();
    expect(screen.queryByText("alpha")).toBeNull();
  });

  it("filters saved projects too", async () => {
    vi.mocked(fetchRecentProjects).mockResolvedValue({ projects: recents() });
    vi.mocked(fetchProjects).mockResolvedValue([savedProject()]);

    renderStep();
    const box = await screen.findByLabelText("Search projects");
    expect(await screen.findByText("saved-yankee")).toBeTruthy();

    fireEvent.change(box, { target: { value: "yank" } });
    expect(screen.getByText("saved-yankee")).toBeTruthy();
    expect(screen.queryByText("alpha")).toBeNull();

    fireEvent.change(box, { target: { value: "alpha" } });
    expect(await screen.findByText("alpha")).toBeTruthy();
    expect(screen.queryByText("saved-yankee")).toBeNull();
  });

  it("shows an empty state when nothing matches", async () => {
    vi.mocked(fetchRecentProjects).mockResolvedValue({ projects: recents() });
    vi.mocked(fetchProjects).mockResolvedValue([]);

    renderStep();
    const box = await screen.findByLabelText("Search projects");

    fireEvent.change(box, { target: { value: "nothing-here" } });
    expect(await screen.findByText(/No projects match that search/)).toBeTruthy();
    expect(screen.queryByText("alpha")).toBeNull();
  });
});
