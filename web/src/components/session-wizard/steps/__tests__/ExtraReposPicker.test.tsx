// @vitest-environment jsdom
//
// Tests for ExtraReposPicker: the wizard step that lets the user attach
// additional repos to a multi-repo session. Cover the loading -> loaded
// transition, toggling registered projects, free-text add (including the
// dedupe / primary-path guards), removal chips, and the resulting onChange
// payloads.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";

import { ExtraReposPicker } from "../ExtraReposPicker";
import type { ProjectInfo } from "../../../../lib/types";

const fetchProjects = vi.fn();
vi.mock("../../../../lib/api", () => ({
  fetchProjects: () => fetchProjects(),
}));

const PROJECTS: ProjectInfo[] = [
  { name: "primary", path: "/repos/primary", scope: "global" },
  { name: "alpha", path: "/repos/alpha", scope: "global" },
  { name: "beta", path: "/repos/beta", scope: "profile" },
];

function setup(overrides?: { selectedPaths?: string[]; primaryPath?: string }) {
  const onChange = vi.fn();
  const utils = render(
    <ExtraReposPicker
      primaryPath={overrides?.primaryPath ?? "/repos/primary"}
      selectedPaths={overrides?.selectedPaths ?? []}
      onChange={onChange}
    />,
  );
  return { ...utils, onChange };
}

function projectButton(container: HTMLElement, name: string): HTMLButtonElement | undefined {
  return Array.from(container.querySelectorAll("button")).find(
    (b) => b.querySelector(".font-mono")?.textContent === name,
  ) as HTMLButtonElement | undefined;
}

beforeEach(() => {
  fetchProjects.mockReset();
  fetchProjects.mockResolvedValue(PROJECTS);
});

afterEach(() => {
  cleanup();
});

describe("ExtraReposPicker", () => {
  it("hides the primary repo and lists the other registered projects once loaded", async () => {
    const { container } = setup();
    await waitFor(() => expect(container.textContent).toContain("Registered projects"));
    expect(projectButton(container, "alpha")).toBeTruthy();
    expect(projectButton(container, "beta")).toBeTruthy();
    // Primary is filtered out of the pickable list.
    expect(projectButton(container, "primary")).toBeFalsy();
  });

  it("shows the none summary when nothing is selected", () => {
    const { container } = setup({ selectedPaths: [] });
    expect(container.textContent).toContain("none");
  });

  it("selecting a registered project adds its path via onChange", async () => {
    const { container, onChange } = setup({ selectedPaths: [] });
    await waitFor(() => expect(projectButton(container, "alpha")).toBeTruthy());
    fireEvent.click(projectButton(container, "alpha")!);
    expect(onChange).toHaveBeenCalledWith(["/repos/alpha"]);
  });

  it("clicking an already-selected project deselects it", async () => {
    const { container, onChange } = setup({ selectedPaths: ["/repos/alpha"] });
    await waitFor(() => expect(projectButton(container, "alpha")).toBeTruthy());
    // Click the registered-project toggle (title is the full path and it
    // carries a scope label), not the chip's remove button.
    const toggle = Array.from(container.querySelectorAll("button")).find(
      (b) => b.getAttribute("title") === "/repos/alpha" && b.textContent?.includes("alpha"),
    )!;
    fireEvent.click(toggle);
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
    await waitFor(() => expect(container.textContent).toContain("Registered projects"));
    const addBtn = Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.trim() === "Add")!;
    expect(addBtn.disabled).toBe(true);
    const input = container.querySelector<HTMLInputElement>('input[type="text"]')!;
    fireEvent.change(input, { target: { value: "/new/repo" } });
    expect(addBtn.disabled).toBe(false);
  });

  it("free-text Add appends a trimmed path and clears the input", async () => {
    const { container, onChange } = setup({ selectedPaths: ["/repos/alpha"] });
    await waitFor(() => expect(container.textContent).toContain("Registered projects"));
    const input = container.querySelector<HTMLInputElement>('input[type="text"]')!;
    fireEvent.change(input, { target: { value: "  /new/repo  " } });
    const addBtn = Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.trim() === "Add")!;
    fireEvent.click(addBtn);
    expect(onChange).toHaveBeenCalledWith(["/repos/alpha", "/new/repo"]);
    expect(input.value).toBe("");
  });

  it("Enter in the free-text input adds the path", async () => {
    const { container, onChange } = setup({ selectedPaths: [] });
    await waitFor(() => expect(container.textContent).toContain("Registered projects"));
    const input = container.querySelector<HTMLInputElement>('input[type="text"]')!;
    fireEvent.change(input, { target: { value: "/typed/repo" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onChange).toHaveBeenCalledWith(["/typed/repo"]);
  });

  it("ignores a duplicate free-text path but still clears the input", async () => {
    const { container, onChange } = setup({ selectedPaths: ["/repos/alpha"] });
    await waitFor(() => expect(container.textContent).toContain("Registered projects"));
    const input = container.querySelector<HTMLInputElement>('input[type="text"]')!;
    fireEvent.change(input, { target: { value: "/repos/alpha" } });
    const addBtn = Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.trim() === "Add")!;
    fireEvent.click(addBtn);
    expect(onChange).not.toHaveBeenCalled();
    expect(input.value).toBe("");
  });

  it("ignores a free-text path equal to the primary path", async () => {
    const { container, onChange } = setup({ primaryPath: "/repos/primary", selectedPaths: [] });
    await waitFor(() => expect(container.textContent).toContain("Registered projects"));
    const input = container.querySelector<HTMLInputElement>('input[type="text"]')!;
    fireEvent.change(input, { target: { value: "/repos/primary" } });
    const addBtn = Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.trim() === "Add")!;
    fireEvent.click(addBtn);
    expect(onChange).not.toHaveBeenCalled();
    expect(input.value).toBe("");
  });

  it("shows the empty-projects hint when no projects are registered", async () => {
    fetchProjects.mockResolvedValue([]);
    const { container } = setup();
    await waitFor(() => expect(container.textContent).toContain("No registered projects yet"));
    expect(container.textContent).toContain("aoe project add");
  });
});
