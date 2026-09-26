// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { ExtraReposPicker } from "../steps/ExtraReposPicker";
import type { ProjectInfo } from "../../../lib/types";

const fetchProjects = vi.fn();
const fetchRecentProjects = vi.fn();
const fetchBranches = vi.fn();
vi.mock("../../../lib/api", () => ({
  fetchProjects: () => fetchProjects(),
  fetchSessions: () => Promise.resolve({ sessions: [], workspace_ordering: [] }),
  fetchRecentProjects: () => fetchRecentProjects(),
  fetchBranches: (...args: unknown[]) => fetchBranches(...args),
}));

const project = (name: string): ProjectInfo => ({ name, path: `/repos/${name}`, scope: "global", pinned: false });

beforeEach(() => {
  fetchProjects.mockResolvedValue([project("primary"), project("alpha"), project("beta")]);
  fetchRecentProjects.mockResolvedValue({ projects: [] });
  fetchBranches.mockResolvedValue([]);
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

function setup(over: { selectedPaths?: string[]; repoBases?: Record<string, string>; basesEnabled?: boolean } = {}) {
  const onChange = vi.fn();
  const onRepoBasesChange = vi.fn();
  const { container } = render(
    <ExtraReposPicker
      primaryPath="/repos/primary"
      selectedPaths={over.selectedPaths ?? []}
      onChange={onChange}
      repoBases={over.repoBases ?? {}}
      onRepoBasesChange={onRepoBasesChange}
      basesEnabled={over.basesEnabled ?? false}
    />,
  );
  return { container, onChange, onRepoBasesChange };
}

const row = (container: HTMLElement, name: string) =>
  Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.includes(name));
const loaded = (container: HTMLElement) => waitFor(() => expect(container.textContent).toContain("Saved projects"));
const addFreeText = (value: string) => {
  const input = screen.getByPlaceholderText("/path/to/another/repo") as HTMLInputElement;
  fireEvent.change(input, { target: { value } });
  fireEvent.click(screen.getByRole("button", { name: "Add" }));
  return input;
};

describe("ExtraReposPicker selection", () => {
  it("lists saved projects except the primary, with a none summary", async () => {
    const { container } = setup();
    expect(container.textContent).toContain("none");
    await loaded(container);
    expect(row(container, "alpha")).toBeTruthy();
    expect(row(container, "beta")).toBeTruthy();
    expect(row(container, "primary")).toBeFalsy();
  });

  it.each([
    [[], ["/repos/alpha"]],
    [["/repos/alpha"], []],
  ])("clicking alpha with %j selected emits %j", async (selectedPaths, expected) => {
    const { container, onChange } = setup({ selectedPaths });
    await waitFor(() => expect(row(container, "alpha")).toBeTruthy());
    fireEvent.click(row(container, "alpha")!);
    expect(onChange).toHaveBeenCalledWith(expected);
  });

  it("renders removable chips, naming unknown paths by basename", async () => {
    const { onChange } = setup({ selectedPaths: ["/repos/alpha", "/some/other/repo"] });
    expect(await screen.findByText("2 selected")).toBeTruthy();
    expect(screen.getByLabelText("Remove repo")).toBeTruthy();
    fireEvent.click(screen.getByLabelText("Remove alpha"));
    expect(onChange).toHaveBeenCalledWith(["/some/other/repo"]);
  });

  it("adds a trimmed free-text path via Add or Enter, and Add waits for input", async () => {
    const { container, onChange } = setup({ selectedPaths: ["/repos/alpha"] });
    await loaded(container);
    const add = screen.getByRole("button", { name: "Add" }) as HTMLButtonElement;
    expect(add.disabled).toBe(true);
    const input = addFreeText("  /new/repo  ");
    expect(onChange).toHaveBeenCalledWith(["/repos/alpha", "/new/repo"]);
    expect(input.value).toBe("");
    fireEvent.change(input, { target: { value: "/typed/repo" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onChange).toHaveBeenCalledWith(["/repos/alpha", "/typed/repo"]);
  });

  it.each(["/repos/alpha", "/repos/primary"])("ignores free-text %s but clears the input", async (path) => {
    const { container, onChange } = setup({ selectedPaths: ["/repos/alpha"] });
    await loaded(container);
    expect(addFreeText(path).value).toBe("");
    expect(onChange).not.toHaveBeenCalled();
  });

  it("searches saved projects and selects recents", async () => {
    fetchRecentProjects.mockResolvedValue({
      projects: [
        { path: "/repos/gamma", display_name: "gamma", tool: "claude", last_used_at: "2025-09-20T00:00:00+00:00" },
      ],
    });
    const { container, onChange } = setup();
    await waitFor(() => expect(row(container, "gamma")).toBeTruthy());
    fireEvent.click(row(container, "gamma")!);
    expect(onChange).toHaveBeenCalledWith(["/repos/gamma"]);
    fireEvent.change(screen.getByLabelText("Search projects"), { target: { value: "bet" } });
    expect(row(container, "beta")).toBeTruthy();
    expect(row(container, "alpha")).toBeFalsy();
  });
});

describe("ExtraReposPicker per-repo base branch", () => {
  const selectedPaths = ["/src/api", "/src/web"];

  it.each([
    [{ "/src/api": "epic/checkout" }, "web", "develop", { "/src/api": "epic/checkout", "/src/web": "develop" }],
    [{ "/src/api": "epic/checkout", "/src/web": "develop" }, "api", "", { "/src/web": "develop" }],
  ])("with %j, typing into %s emits the merged map", (repoBases, repo, value, expected) => {
    const { onRepoBasesChange } = setup({ selectedPaths, repoBases, basesEnabled: true });
    fireEvent.change(screen.getByLabelText(`Base branch for ${repo}`), { target: { value } });
    expect(onRepoBasesChange).toHaveBeenCalledWith(expected);
  });

  it("drops a removed repo's base so it cannot be submitted", () => {
    const { onChange, onRepoBasesChange } = setup({
      selectedPaths,
      repoBases: { "/src/api": "epic/checkout" },
      basesEnabled: true,
    });
    fireEvent.click(screen.getByLabelText("Remove api"));
    expect(onChange).toHaveBeenCalledWith(["/src/web"]);
    expect(onRepoBasesChange).toHaveBeenCalledWith({});
  });

  it("suggests branches from the repo being edited", async () => {
    fetchBranches.mockResolvedValue([{ name: "epic/checkout", is_current: false }]);
    setup({ selectedPaths, basesEnabled: true });
    fireEvent.focus(screen.getByLabelText("Base branch for web"));
    await waitFor(() => expect(fetchBranches).toHaveBeenCalledWith("/src/web", true));
    expect(fetchBranches).not.toHaveBeenCalledWith("/repos/primary", true);
    fireEvent.mouseDown(await screen.findByText("epic/checkout"));
  });

  it("hides base inputs when bases are disabled but keeps repos removable", () => {
    setup({ selectedPaths });
    expect(screen.queryByLabelText("Base branch for api")).toBeNull();
    expect(screen.getByLabelText("Remove api")).toBeTruthy();
  });
});
