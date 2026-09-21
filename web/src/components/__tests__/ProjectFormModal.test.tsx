// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { ProjectFormModal } from "../ProjectFormModal";

vi.mock("../../lib/api", () => ({
  createProject: vi.fn(),
  updateProject: vi.fn(),
}));

import { createProject, updateProject } from "../../lib/api";

const mockCreate = createProject as ReturnType<typeof vi.fn>;
const mockUpdate = updateProject as ReturnType<typeof vi.fn>;

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

const BRANCH_PLACEHOLDER = "blank = inherit global default, then auto-detect";

function branchInput() {
  return screen.getByPlaceholderText(BRANCH_PLACEHOLDER) as HTMLInputElement;
}

function renderEdit() {
  render(
    <ProjectFormModal
      initial={{ name: "extra", path: "/repo/extra", scope: "global", default_base_branch: "develop" }}
      onClose={() => {}}
      onSaved={() => {}}
    />,
  );
  return branchInput();
}

describe("ProjectFormModal", () => {
  it("sends default_base_branch in the create payload when set", async () => {
    mockCreate.mockResolvedValue({ ok: true });
    render(<ProjectFormModal onClose={() => {}} onSaved={() => {}} />);

    fireEvent.change(screen.getByPlaceholderText("/path/to/repo"), { target: { value: "/repo/extra" } });
    fireEvent.change(branchInput(), { target: { value: "develop" } });
    fireEvent.click(screen.getByRole("button", { name: "Add" }));

    await waitFor(() =>
      expect(mockCreate).toHaveBeenCalledWith(
        expect.objectContaining({ path: "/repo/extra", default_base_branch: "develop" }),
      ),
    );
  });

  it("omits default_base_branch when the field is left blank", async () => {
    mockCreate.mockResolvedValue({ ok: true });
    render(<ProjectFormModal onClose={() => {}} onSaved={() => {}} />);

    fireEvent.change(screen.getByPlaceholderText("/path/to/repo"), { target: { value: "/repo/extra" } });
    fireEvent.click(screen.getByRole("button", { name: "Add" }));

    await waitFor(() => expect(mockCreate).toHaveBeenCalled());
    expect(mockCreate.mock.calls[0]![0].default_base_branch).toBeUndefined();
  });

  it.each([
    ["release", "release"],
    ["  ", null],
  ] as [string, string | null][])("edit mode saves the typed base branch (%j)", async (typed, sent) => {
    mockUpdate.mockResolvedValue({ ok: true });
    const input = renderEdit();
    expect(input.value).toBe("develop");

    fireEvent.change(input, { target: { value: typed } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(mockUpdate).toHaveBeenCalledWith("extra", "global", sent));
  });

  it("invokes onSaved and onClose after a successful create", async () => {
    mockCreate.mockResolvedValue({ ok: true });
    const onSaved = vi.fn();
    const onClose = vi.fn();
    render(<ProjectFormModal onClose={onClose} onSaved={onSaved} />);

    fireEvent.change(screen.getByPlaceholderText("/path/to/repo"), { target: { value: "/repo/extra" } });
    fireEvent.click(screen.getByRole("button", { name: "Add" }));

    await waitFor(() => expect(onSaved).toHaveBeenCalled());
    expect(onClose).toHaveBeenCalled();
  });
});
