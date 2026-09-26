// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";

import { ProjectsSection } from "../ProjectsSection";
import type { ProjectInfo, RepoGroup } from "../../lib/types";

afterEach(cleanup);

function emptyProject(repoPath: string, over: Partial<RepoGroup> = {}): RepoGroup {
  const name = repoPath.split("/").pop() ?? repoPath;
  const registration: ProjectInfo = { name, path: repoPath, scope: "global" };
  return {
    id: repoPath,
    repoPath,
    displayName: name,
    defaultDisplayName: name,
    alias: null,
    color: null,
    remoteOwner: null,
    workspaces: [],
    status: "idle",
    collapsed: false,
    registeredProjects: [registration],
    ...over,
  };
}

function renderSection(props: Partial<Parameters<typeof ProjectsSection>[0]> = {}) {
  const handlers = {
    onCreateSession: vi.fn(),
    onAddProject: vi.fn(),
    onEditProject: vi.fn(),
    onRemoveProject: vi.fn(),
  };
  render(
    <ProjectsSection projects={[emptyProject("/work/alpha")]} query="" offline={false} {...handlers} {...props} />,
  );
  return handlers;
}

function longPress(release: (row: HTMLElement) => void) {
  vi.useFakeTimers();
  try {
    renderSection();
    const row = screen.getByTestId("sidebar-project-row");
    fireEvent.touchStart(row, { touches: [{ clientX: 10, clientY: 10 }] });
    release(row);
    act(() => {
      vi.advanceTimersByTime(500);
    });
    return screen.queryByTestId("sidebar-project-context-menu");
  } finally {
    vi.useRealTimers();
    cleanup();
  }
}

describe("ProjectsSection", () => {
  it("renders the row with its base branch, adds a project, and starts a session", () => {
    const h = renderSection({
      projects: [
        emptyProject("/work/alpha", {
          registeredProjects: [{ name: "alpha", path: "/work/alpha", scope: "global", default_base_branch: "develop" }],
        }),
      ],
    });
    expect(screen.getByText("alpha")).toBeTruthy();
    expect(screen.getByText(/develop/)).toBeTruthy();
    fireEvent.click(screen.getByTestId("sidebar-projects-add"));
    expect(h.onAddProject).toHaveBeenCalled();
    fireEvent.click(screen.getByTitle("New session in alpha"));
    expect(h.onCreateSession).toHaveBeenCalledWith("/work/alpha");
  });

  it("hides add and disables create for read-only viewers", () => {
    const h = renderSection({ readOnly: true });
    expect(screen.queryByTestId("sidebar-projects-add")).toBeNull();
    fireEvent.click(screen.getByTitle("New session in alpha"));
    expect(h.onCreateSession).not.toHaveBeenCalled();
  });

  it("opens the context menu on right-click and fires edit / remove", () => {
    const h = renderSection();
    fireEvent.contextMenu(screen.getByTestId("sidebar-project-row"));
    fireEvent.click(screen.getByTestId("sidebar-project-context-menu-edit"));
    expect(h.onEditProject).toHaveBeenCalledWith(expect.objectContaining({ path: "/work/alpha" }));

    fireEvent.keyDown(screen.getByTestId("sidebar-project-row"), { key: "F10", shiftKey: true });
    fireEvent.click(screen.getByTestId("sidebar-project-context-menu-remove"));
    expect(h.onRemoveProject).toHaveBeenCalledWith(expect.objectContaining({ repoPath: "/work/alpha" }));
  });

  it("opens the context menu on a long-press unless the gesture ends or moves first", () => {
    expect(longPress(() => {})).not.toBeNull();
    // Well past LONG_PRESS_SLOP_PX (8px): a deliberate drag, not a jittery hold.
    expect(longPress((row) => fireEvent.touchMove(row, { touches: [{ clientX: 100, clientY: 100 }] }))).toBeNull();
    expect(longPress((row) => fireEvent.touchEnd(row, { touches: [] }))).toBeNull();
    expect(longPress((row) => fireEvent.touchCancel(row))).toBeNull();
  });

  it("caps the context menu with the dynamic viewport so its tail scrolls on iOS (#2870)", () => {
    renderSection();
    fireEvent.contextMenu(screen.getByTestId("sidebar-project-row"));
    const menu = screen.getByTestId("sidebar-project-context-menu");
    // `100vh` overshoots iOS Safari's visible viewport (dynamic toolbar), so the menu would never exceed its own
    // max-height and overflow-y-auto would never engage.
    expect(menu.style.maxHeight).toContain("dvh");
    expect(menu.className).toContain("overflow-y-auto");
  });

  it("shows the no-match and empty hints, and nothing when there is no way to add", () => {
    renderSection({ query: "zzz" });
    expect(screen.queryByTestId("sidebar-project-row")).toBeNull();
    expect(screen.getByText("No matching projects.")).toBeTruthy();
    cleanup();
    renderSection({ projects: [] });
    expect(screen.getByText(/No saved projects/)).toBeTruthy();
    cleanup();
    renderSection({ projects: [], readOnly: true });
    expect(screen.queryByTestId("sidebar-projects-section")).toBeNull();
  });
});
