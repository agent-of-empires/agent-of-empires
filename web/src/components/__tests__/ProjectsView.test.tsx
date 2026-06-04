// @vitest-environment jsdom
//
// Vitest coverage for the per-project default base branch on the Projects
// view (#1924): the add form sends `default_base_branch` in the create
// payload only when filled, and a configured base branch renders on the
// project row.

import { afterEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";

import { ProjectsView } from "../ProjectsView";

vi.mock("../../lib/api", () => ({
  fetchProjects: vi.fn(),
  createProject: vi.fn(),
  deleteProject: vi.fn(),
}));

import { fetchProjects, createProject } from "../../lib/api";

const mockFetch = fetchProjects as ReturnType<typeof vi.fn>;
const mockCreate = createProject as ReturnType<typeof vi.fn>;

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

async function openAddForm() {
  fireEvent.click(await screen.findByRole("button", { name: "+ Add project" }));
  fireEvent.change(screen.getByPlaceholderText("/path/to/repo"), {
    target: { value: "/repo/extra" },
  });
}

describe("ProjectsView default base branch", () => {
  it("sends default_base_branch in the create payload when set", async () => {
    mockFetch.mockResolvedValue([]);
    mockCreate.mockResolvedValue({ ok: true });

    render(<ProjectsView onClose={() => {}} />);
    await openAddForm();
    fireEvent.change(
      screen.getByPlaceholderText(
        "blank = inherit global default, then auto-detect",
      ),
      { target: { value: "develop" } },
    );
    fireEvent.click(screen.getByRole("button", { name: "Add" }));

    await waitFor(() =>
      expect(mockCreate).toHaveBeenCalledWith(
        expect.objectContaining({
          path: "/repo/extra",
          default_base_branch: "develop",
        }),
      ),
    );
  });

  it("omits default_base_branch when the field is left blank", async () => {
    mockFetch.mockResolvedValue([]);
    mockCreate.mockResolvedValue({ ok: true });

    render(<ProjectsView onClose={() => {}} />);
    await openAddForm();
    fireEvent.click(screen.getByRole("button", { name: "Add" }));

    await waitFor(() => expect(mockCreate).toHaveBeenCalled());
    expect(mockCreate.mock.calls[0][0].default_base_branch).toBeUndefined();
  });

  it("renders a project's configured base branch in the list", async () => {
    mockFetch.mockResolvedValue([
      {
        name: "extra",
        path: "/repo/extra",
        scope: "global",
        default_base_branch: "develop",
      },
    ]);

    render(<ProjectsView onClose={() => {}} />);

    // findByText / getByText throw if the node is absent, so resolving is
    // the assertion (this repo's component tests do not load jest-dom).
    expect(await screen.findByText(/base branch:/i)).toBeTruthy();
    expect(screen.getByText("develop")).toBeTruthy();
  });

  it("does not render a base branch row when none is configured", async () => {
    mockFetch.mockResolvedValue([
      { name: "plain", path: "/repo/plain", scope: "global" },
    ]);

    render(<ProjectsView onClose={() => {}} />);

    expect(await screen.findByText("plain")).toBeTruthy();
    expect(screen.queryByText(/base branch:/i)).toBeNull();
  });

  it("clears the base branch field when the add form is cancelled", async () => {
    mockFetch.mockResolvedValue([]);

    render(<ProjectsView onClose={() => {}} />);
    await openAddForm();
    const baseInput = screen.getByPlaceholderText(
      "blank = inherit global default, then auto-detect",
    );
    fireEvent.change(baseInput, { target: { value: "develop" } });
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));

    // Reopening starts from a clean form (the cancel handler reset state).
    fireEvent.click(screen.getByRole("button", { name: "+ Add project" }));
    expect(
      (
        screen.getByPlaceholderText(
          "blank = inherit global default, then auto-detect",
        ) as HTMLInputElement
      ).value,
    ).toBe("");
  });
});
