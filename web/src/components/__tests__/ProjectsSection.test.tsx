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

describe("ProjectsSection", () => {
  it("renders a row and the add button when online and writable", () => {
    renderSection();
    expect(screen.getByTestId("sidebar-project-row")).toBeTruthy();
    expect(screen.getByText("alpha")).toBeTruthy();
    expect(screen.getByTestId("sidebar-projects-add")).toBeTruthy();
  });

  it("shows the configured base branch on the row", () => {
    renderSection({
      projects: [
        emptyProject("/work/alpha", {
          registeredProjects: [{ name: "alpha", path: "/work/alpha", scope: "global", default_base_branch: "develop" }],
        }),
      ],
    });
    expect(screen.getByText(/develop/)).toBeTruthy();
  });

  it("calls onAddProject from the header button", () => {
    const h = renderSection();
    fireEvent.click(screen.getByTestId("sidebar-projects-add"));
    expect(h.onAddProject).toHaveBeenCalled();
  });

  it("starts a session when the row is clicked", () => {
    const h = renderSection();
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

    fireEvent.contextMenu(screen.getByTestId("sidebar-project-row"));
    fireEvent.click(screen.getByTestId("sidebar-project-context-menu-remove"));
    expect(h.onRemoveProject).toHaveBeenCalledWith(expect.objectContaining({ repoPath: "/work/alpha" }));
  });

  it("opens the context menu on a long-press (#3460-style touch support)", () => {
    vi.useFakeTimers();
    try {
      renderSection();
      const row = screen.getByTestId("sidebar-project-row");
      fireEvent.touchStart(row, { touches: [{ clientX: 10, clientY: 10 }] });
      act(() => {
        vi.advanceTimersByTime(500);
      });
      expect(screen.getByTestId("sidebar-project-context-menu")).toBeTruthy();
    } finally {
      vi.useRealTimers();
    }
  });

  it("cancels the pending long-press once the finger moves past the touch slop", () => {
    vi.useFakeTimers();
    try {
      renderSection();
      const row = screen.getByTestId("sidebar-project-row");
      fireEvent.touchStart(row, { touches: [{ clientX: 10, clientY: 10 }] });
      // Well past LONG_PRESS_SLOP_PX (8px): a deliberate drag, not a jittery hold.
      fireEvent.touchMove(row, { touches: [{ clientX: 100, clientY: 100 }] });
      act(() => {
        vi.advanceTimersByTime(500);
      });
      expect(screen.queryByTestId("sidebar-project-context-menu")).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("cancels the pending long-press on touchend before the timer fires", () => {
    vi.useFakeTimers();
    try {
      renderSection();
      const row = screen.getByTestId("sidebar-project-row");
      fireEvent.touchStart(row, { touches: [{ clientX: 10, clientY: 10 }] });
      fireEvent.touchEnd(row, { touches: [] });
      act(() => {
        vi.advanceTimersByTime(500);
      });
      expect(screen.queryByTestId("sidebar-project-context-menu")).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("cancels the pending long-press on touchcancel (an OS-interrupted gesture)", () => {
    vi.useFakeTimers();
    try {
      renderSection();
      const row = screen.getByTestId("sidebar-project-row");
      fireEvent.touchStart(row, { touches: [{ clientX: 10, clientY: 10 }] });
      fireEvent.touchCancel(row);
      act(() => {
        vi.advanceTimersByTime(500);
      });
      expect(screen.queryByTestId("sidebar-project-context-menu")).toBeNull();
    } finally {
      vi.useRealTimers();
    }
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

  it("opens the context menu via Shift+F10 keyboard path", () => {
    const h = renderSection();
    const row = screen.getByTestId("sidebar-project-row");
    fireEvent.keyDown(row, { key: "F10", shiftKey: true });
    fireEvent.click(screen.getByTestId("sidebar-project-context-menu-remove"));
    expect(h.onRemoveProject).toHaveBeenCalled();
  });

  it("filters rows by query and shows the no-match hint", () => {
    renderSection({ query: "zzz" });
    expect(screen.queryByTestId("sidebar-project-row")).toBeNull();
    expect(screen.getByText("No matching projects.")).toBeTruthy();
  });

  it("shows the empty hint when there are no projects but add is available", () => {
    renderSection({ projects: [] });
    expect(screen.getByText(/No saved projects/)).toBeTruthy();
  });

  it("renders nothing when there are no projects and no way to add", () => {
    const { container } = render(
      <ProjectsSection
        projects={[]}
        query=""
        readOnly
        offline={false}
        onCreateSession={vi.fn()}
        onAddProject={vi.fn()}
        onEditProject={vi.fn()}
        onRemoveProject={vi.fn()}
      />,
    );
    expect(container.querySelector("[data-testid='sidebar-projects-section']")).toBeNull();
  });
});
