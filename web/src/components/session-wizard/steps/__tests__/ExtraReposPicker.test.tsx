// @vitest-environment jsdom
//
// Tests for ExtraReposPicker: the wizard step that lets the user attach
// additional repos to a multi-repo session. Cover the loading -> loaded
// transition, the shared search-over-saved-and-recent picker (#3743),
// free-text add (including the dedupe / primary-path guards), removal
// chips, and the resulting onChange payloads.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { ExtraReposPicker } from "../ExtraReposPicker";
import type { ProjectInfo } from "../../../../lib/types";
import type { RecentProjectEntry } from "../../../../lib/api";

const fetchProjects = vi.fn();
const fetchSessions = vi.fn();
const fetchRecentProjects = vi.fn();
const fetchBranches = vi.fn();
vi.mock("../../../../lib/api", () => ({
  fetchProjects: () => fetchProjects(),
  fetchSessions: () => fetchSessions(),
  fetchRecentProjects: () => fetchRecentProjects(),
  fetchBranches: (...args: unknown[]) => fetchBranches(...args),
}));

const PROJECTS: ProjectInfo[] = [
  { name: "primary", path: "/repos/primary", scope: "global", pinned: false },
  { name: "alpha", path: "/repos/alpha", scope: "global", pinned: false },
  { name: "beta", path: "/repos/beta", scope: "profile", pinned: false },
];

// `basesEnabled` defaults to false so the per-repo base inputs (#3329) stay out
// of the way of this file's concern, repo selection. The free-text path field
// is looked up by placeholder, not position, because the shared search box
// (#3743) also renders a text input above it.
function setup(overrides?: { selectedPaths?: string[]; primaryPath?: string; basesEnabled?: boolean }) {
  const onChange = vi.fn();
  const onRepoBasesChange = vi.fn();
  const utils = render(
    <ExtraReposPicker
      primaryPath={overrides?.primaryPath ?? "/repos/primary"}
      selectedPaths={overrides?.selectedPaths ?? []}
      onChange={onChange}
      repoBases={{}}
      onRepoBasesChange={onRepoBasesChange}
      basesEnabled={overrides?.basesEnabled ?? false}
    />,
  );
  return { ...utils, onChange, onRepoBasesChange };
}

function projectRow(container: HTMLElement, name: string): HTMLButtonElement | undefined {
  return Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.includes(name)) as
    | HTMLButtonElement
    | undefined;
}

function freeTextInput(): HTMLInputElement {
  return screen.getByPlaceholderText("/path/to/another/repo");
}

beforeEach(() => {
  fetchProjects.mockReset();
  fetchProjects.mockResolvedValue(PROJECTS);
  fetchSessions.mockReset();
  fetchSessions.mockResolvedValue({ sessions: [], workspace_ordering: [] });
  fetchRecentProjects.mockReset();
  fetchRecentProjects.mockResolvedValue({ projects: [] });
});

afterEach(() => {
  cleanup();
});

describe("ExtraReposPicker", () => {
  it("hides the primary repo and lists the other saved projects once loaded", async () => {
    const { container } = setup();
    await waitFor(() => expect(container.textContent).toContain("Saved projects"));
    expect(projectRow(container, "alpha")).toBeTruthy();
    expect(projectRow(container, "beta")).toBeTruthy();
    // Primary is filtered out of the pickable list.
    expect(projectRow(container, "primary")).toBeFalsy();
  });

  it("shows the none summary when nothing is selected", () => {
    const { container } = setup({ selectedPaths: [] });
    expect(container.textContent).toContain("none");
  });

  it("selecting a saved project adds its path via onChange", async () => {
    const { container, onChange } = setup({ selectedPaths: [] });
    await waitFor(() => expect(projectRow(container, "alpha")).toBeTruthy());
    fireEvent.click(projectRow(container, "alpha")!);
    expect(onChange).toHaveBeenCalledWith(["/repos/alpha"]);
  });

  it("clicking an already-selected project deselects it", async () => {
    const { container, onChange } = setup({ selectedPaths: ["/repos/alpha"] });
    await waitFor(() => expect(projectRow(container, "alpha")).toBeTruthy());
    fireEvent.click(projectRow(container, "alpha")!);
    expect(onChange).toHaveBeenCalledWith([]);
  });

  it("renders a chip + selected count for each selected path", async () => {
    const { container } = setup({ selectedPaths: ["/repos/alpha", "/repos/beta"] });
    await waitFor(() => expect(container.textContent).toContain("2 selected"));
    expect(container.querySelector('button[aria-label="Remove alpha"]')).toBeTruthy();
    expect(container.querySelector('button[aria-label="Remove beta"]')).toBeTruthy();
  });

  it("removing a chip drops that path via onChange", async () => {
    const { container, onChange } = setup({ selectedPaths: ["/repos/alpha", "/repos/beta"] });
    await waitFor(() => expect(container.querySelector('button[aria-label="Remove alpha"]')).toBeTruthy());
    fireEvent.click(container.querySelector<HTMLButtonElement>('button[aria-label="Remove alpha"]')!);
    expect(onChange).toHaveBeenCalledWith(["/repos/beta"]);
  });

  it("labels a chip for an unknown (free-text) path by its basename", async () => {
    const { container } = setup({ selectedPaths: ["/some/other/repo"] });
    await waitFor(() => expect(container.querySelector('button[aria-label="Remove repo"]')).toBeTruthy());
  });

  it("Add button is disabled until the free-text input has content", async () => {
    const { container } = setup();
    await waitFor(() => expect(container.textContent).toContain("Saved projects"));
    const addBtn = Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.trim() === "Add")!;
    expect(addBtn.disabled).toBe(true);
    fireEvent.change(freeTextInput(), { target: { value: "/new/repo" } });
    expect(addBtn.disabled).toBe(false);
  });

  it("free-text Add appends a trimmed path and clears the input", async () => {
    const { container, onChange } = setup({ selectedPaths: ["/repos/alpha"] });
    await waitFor(() => expect(container.textContent).toContain("Saved projects"));
    const input = freeTextInput();
    fireEvent.change(input, { target: { value: "  /new/repo  " } });
    const addBtn = Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.trim() === "Add")!;
    fireEvent.click(addBtn);
    expect(onChange).toHaveBeenCalledWith(["/repos/alpha", "/new/repo"]);
    expect(input.value).toBe("");
  });

  it("Enter in the free-text input adds the path", async () => {
    const { container, onChange } = setup({ selectedPaths: [] });
    await waitFor(() => expect(container.textContent).toContain("Saved projects"));
    const input = freeTextInput();
    fireEvent.change(input, { target: { value: "/typed/repo" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onChange).toHaveBeenCalledWith(["/typed/repo"]);
  });

  it("ignores a duplicate free-text path but still clears the input", async () => {
    const { container, onChange } = setup({ selectedPaths: ["/repos/alpha"] });
    await waitFor(() => expect(container.textContent).toContain("Saved projects"));
    const input = freeTextInput();
    fireEvent.change(input, { target: { value: "/repos/alpha" } });
    const addBtn = Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.trim() === "Add")!;
    fireEvent.click(addBtn);
    expect(onChange).not.toHaveBeenCalled();
    expect(input.value).toBe("");
  });

  it("ignores a free-text path equal to the primary path", async () => {
    const { container, onChange } = setup({ primaryPath: "/repos/primary", selectedPaths: [] });
    await waitFor(() => expect(container.textContent).toContain("Saved projects"));
    const input = freeTextInput();
    fireEvent.change(input, { target: { value: "/repos/primary" } });
    const addBtn = Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.trim() === "Add")!;
    fireEvent.click(addBtn);
    expect(onChange).not.toHaveBeenCalled();
    expect(input.value).toBe("");
  });

  it("shows the empty-projects hint when there is nothing saved or recent", async () => {
    fetchProjects.mockResolvedValue([]);
    const { container } = setup();
    await waitFor(() => expect(container.textContent).toContain("No registered projects yet"));
    expect(container.textContent).toContain("aoe project add");
  });

  // #3743: extra repos should be just as searchable as the main project step.
  it("filters the saved list by name via the shared search box", async () => {
    const { container } = setup();
    await waitFor(() => expect(projectRow(container, "alpha")).toBeTruthy());
    const search = screen.getByLabelText("Search projects");
    fireEvent.change(search, { target: { value: "bet" } });
    expect(projectRow(container, "beta")).toBeTruthy();
    expect(projectRow(container, "alpha")).toBeFalsy();
  });

  it("lists recent projects alongside saved projects and can select one", async () => {
    const recents: RecentProjectEntry[] = [
      { path: "/repos/gamma", display_name: "gamma", tool: "claude", last_used_at: "2025-09-20T00:00:00+00:00" },
    ];
    fetchRecentProjects.mockResolvedValue({ projects: recents });
    const { container, onChange } = setup();
    await waitFor(() => expect(projectRow(container, "gamma")).toBeTruthy());
    fireEvent.click(projectRow(container, "gamma")!);
    expect(onChange).toHaveBeenCalledWith(["/repos/gamma"]);
  });
});
