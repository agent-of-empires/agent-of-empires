// @vitest-environment jsdom
//
// Vitest coverage for the extracted project add/edit form (#2212), migrated
// from the former ProjectsView test: the add form sends `default_base_branch`
// only when filled, and edit mode PATCHes the registration (including clearing
// the base branch to null).

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

describe("ProjectFormModal", () => {
  it("sends default_base_branch in the create payload when set", async () => {
    mockCreate.mockResolvedValue({ ok: true });
    render(<ProjectFormModal onClose={() => {}} onSaved={() => {}} />);

    fireEvent.change(screen.getByPlaceholderText("/path/to/repo"), { target: { value: "/repo/extra" } });
    fireEvent.change(screen.getByPlaceholderText("blank = inherit global default, then auto-detect"), {
      target: { value: "develop" },
    });
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

  it("prefills and PATCHes the base branch in edit mode", async () => {
    mockUpdate.mockResolvedValue({ ok: true });
    render(
      <ProjectFormModal
        initial={{ name: "extra", path: "/repo/extra", scope: "global", default_base_branch: "develop" }}
        onClose={() => {}}
        onSaved={() => {}}
      />,
    );

    const input = screen.getByPlaceholderText("blank = inherit global default, then auto-detect") as HTMLInputElement;
    expect(input.value).toBe("develop");
    fireEvent.change(input, { target: { value: "release" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(mockUpdate).toHaveBeenCalledWith("extra", "global", "release"));
  });

  it("clears the base branch by saving an empty value in edit mode", async () => {
    mockUpdate.mockResolvedValue({ ok: true });
    render(
      <ProjectFormModal
        initial={{ name: "extra", path: "/repo/extra", scope: "global", default_base_branch: "develop" }}
        onClose={() => {}}
        onSaved={() => {}}
      />,
    );

    fireEvent.change(screen.getByPlaceholderText("blank = inherit global default, then auto-detect"), {
      target: { value: "  " },
    });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(mockUpdate).toHaveBeenCalledWith("extra", "global", null));
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
