// @vitest-environment jsdom
//
// Vitest coverage for the sidebar Projects section (#2212): row rendering,
// read-only gating, the add affordance, the empty state, and the row context
// menu (edit / remove). Drives the keyboard + create paths CodeRabbit flagged
// as uncovered.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

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
