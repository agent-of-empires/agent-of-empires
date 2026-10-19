// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { ProjectFormModal } from "../ProjectFormModal";

vi.mock("../../lib/api", () => ({
  createProject: vi.fn(),
  updateProject: vi.fn(),
  getSettingsSchema: vi.fn(),
  fetchMachineSettings: vi.fn(),
  resolvePluginOptions: vi.fn(),
}));

import {
  createProject,
  fetchMachineSettings,
  getSettingsSchema,
  resolvePluginOptions,
  updateProject,
} from "../../lib/api";

const mockCreate = createProject as ReturnType<typeof vi.fn>;
const mockUpdate = updateProject as ReturnType<typeof vi.fn>;
const mockSchema = getSettingsSchema as ReturnType<typeof vi.fn>;
const mockSettings = fetchMachineSettings as ReturnType<typeof vi.fn>;
const mockResolve = resolvePluginOptions as ReturnType<typeof vi.fn>;

beforeEach(() => {
  mockSchema.mockResolvedValue([]);
  mockSettings.mockResolvedValue(null);
  mockResolve.mockResolvedValue([]);
});

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

  it("sends overrides in the create payload when a tri-state select is set to On", async () => {
    mockCreate.mockResolvedValue({ ok: true });
    render(<ProjectFormModal onClose={() => {}} onSaved={() => {}} />);

    fireEvent.change(screen.getByPlaceholderText("/path/to/repo"), { target: { value: "/repo/extra" } });
    const worktreeSelect = screen.getByText("Worktree by default").nextElementSibling as HTMLSelectElement;
    fireEvent.change(worktreeSelect, { target: { value: "on" } });
    fireEvent.click(screen.getByRole("button", { name: "Add" }));

    await waitFor(() =>
      expect(mockCreate).toHaveBeenCalledWith(
        expect.objectContaining({ overrides: expect.objectContaining({ worktree_enabled: true }) }),
      ),
    );
  });

  it("sends the container-start override in the create payload only when it is set", async () => {
    mockCreate.mockResolvedValue({ ok: true });
    render(<ProjectFormModal onClose={() => {}} onSaved={() => {}} />);

    fireEvent.change(screen.getByPlaceholderText("/path/to/repo"), { target: { value: "/repo/extra" } });
    const sandboxSelect = screen.getByText("Start in container by default").nextElementSibling as HTMLSelectElement;
    fireEvent.change(sandboxSelect, { target: { value: "off" } });
    fireEvent.click(screen.getByRole("button", { name: "Add" }));

    await waitFor(() =>
      expect(mockCreate).toHaveBeenCalledWith(
        expect.objectContaining({ overrides: expect.objectContaining({ sandbox_enabled: false }) }),
      ),
    );
  });

  it("PATCHes with exactly 3 arguments when overrides are left at 'Use global default' in edit mode", async () => {
    mockUpdate.mockResolvedValue({ ok: true });
    render(
      <ProjectFormModal
        initial={{ name: "extra", path: "/repo/extra", scope: "global", default_base_branch: "develop", pinned: false }}
        onClose={() => {}}
        onSaved={() => {}}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(mockUpdate).toHaveBeenCalledWith("extra", "global", "develop"));
    expect(mockUpdate.mock.calls[0]!.length).toBe(3);
  });

  it("PATCHes overrides when worktree-by-default is switched to Off in edit mode", async () => {
    mockUpdate.mockResolvedValue({ ok: true });
    render(
      <ProjectFormModal
        initial={{ name: "extra", path: "/repo/extra", scope: "global", default_base_branch: "develop", pinned: false }}
        onClose={() => {}}
        onSaved={() => {}}
      />,
    );

    const worktreeSelect = screen.getByText("Worktree by default").nextElementSibling as HTMLSelectElement;
    fireEvent.change(worktreeSelect, { target: { value: "off" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() =>
      expect(mockUpdate).toHaveBeenCalledWith("extra", "global", "develop", {
        worktree_enabled: false,
        smart_rename: null,
        sandbox_enabled: null,
      }),
    );
  });

  it("PATCHes the sandbox override when start-in-container is switched to On in edit mode", async () => {
    mockUpdate.mockResolvedValue({ ok: true });
    render(
      <ProjectFormModal
        initial={{ name: "extra", path: "/repo/extra", scope: "global", pinned: false }}
        onClose={() => {}}
        onSaved={() => {}}
      />,
    );

    const sandboxSelect = screen.getByText("Start in container by default").nextElementSibling as HTMLSelectElement;
    fireEvent.change(sandboxSelect, { target: { value: "on" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() =>
      expect(mockUpdate).toHaveBeenCalledWith("extra", "global", null, {
        worktree_enabled: null,
        smart_rename: null,
        sandbox_enabled: true,
      }),
    );
  });

  it("pre-selects 'Off' for smart-rename when the project has that override set", () => {
    render(
      <ProjectFormModal
        initial={{
          name: "extra",
          path: "/repo/extra",
          scope: "global",
          pinned: false,
          overrides: { smart_rename: false },
        }}
        onClose={() => {}}
        onSaved={() => {}}
      />,
    );

    const smartRenameSelect = screen.getByText("Smart session rename").nextElementSibling as HTMLSelectElement;
    expect(smartRenameSelect.value).toBe("off");
  });

  describe("per-project plugin settings", () => {
    const field = {
      section: "plugin:acme.kit",
      field: "label",
      category: "Plugins",
      label: "Label",
      description: "",
      widget: { kind: "text" },
      web_write: { policy: "allow" },
      profile_overridable: false,
      validation: { rule: "none" },
      advanced: false,
      per_project: true,
    };
    const globalOnly = { ...field, field: "other", label: "Other", per_project: undefined };

    beforeEach(() => {
      mockSchema.mockResolvedValue([field, globalOnly]);
      mockSettings.mockResolvedValue({ plugins: { "acme.kit": { settings: { label: "global-label" } } } });
    });

    const editProject = (overrides?: object) =>
      render(
        <ProjectFormModal
          initial={{ name: "extra", path: "/repo/extra", scope: "global", pinned: false, overrides }}
          onClose={() => {}}
          onSaved={() => {}}
        />,
      );

    it("lists only per_project fields, seeded with the global value", async () => {
      editProject();
      await screen.findByDisplayValue("global-label");
      expect(screen.queryByText("Other")).toBeNull();
      expect(screen.getByText("Using the global value")).toBeTruthy();
    });

    it("PATCHes a changed project value under overrides.plugin_settings", async () => {
      mockUpdate.mockResolvedValue({ ok: true });
      editProject();
      const input = await screen.findByDisplayValue("global-label");
      fireEvent.focus(input);
      fireEvent.change(input, { target: { value: "mine" } });
      fireEvent.keyDown(input, { key: "Enter" });
      fireEvent.click(screen.getByRole("button", { name: "Save" }));

      await waitFor(() =>
        expect(mockUpdate).toHaveBeenCalledWith("extra", "global", null, {
          worktree_enabled: null,
          smart_rename: null,
          sandbox_enabled: null,
          plugin_settings: { "acme.kit": { label: "mine" } },
        }),
      );
    });

    it("'Use global' clears a saved project value with null", async () => {
      mockUpdate.mockResolvedValue({ ok: true });
      editProject({ plugin_settings: { "acme.kit": { label: "saved" } } });
      await screen.findByDisplayValue("saved");
      fireEvent.click(screen.getByRole("button", { name: "Use global" }));
      expect(screen.getByDisplayValue("global-label")).toBeTruthy();
      fireEvent.click(screen.getByRole("button", { name: "Save" }));

      await waitFor(() =>
        expect(mockUpdate).toHaveBeenCalledWith(
          "extra",
          "global",
          null,
          expect.objectContaining({ plugin_settings: { "acme.kit": { label: null } } }),
        ),
      );
    });

    it("resolves a dependent control from the effective sibling values, global-only ones included", async () => {
      const agent = { ...field, field: "agent", label: "Agent" };
      const mode = { ...field, field: "mode", label: "Mode", per_project: undefined };
      const model = {
        ...field,
        field: "model",
        label: "Model",
        widget: { kind: "dynamic_select", source: "acp.models", depends_on: ["agent", "mode"] },
      };
      mockSchema.mockResolvedValue([agent, mode, model]);
      mockSettings.mockResolvedValue({ plugins: { "acme.kit": { settings: { agent: "claude", mode: "fast" } } } });
      editProject();

      const input = await screen.findByDisplayValue("claude");
      await waitFor(() => expect(mockResolve).toHaveBeenLastCalledWith("acme.kit", "acp.models", ["claude", "fast"]));

      fireEvent.focus(input);
      fireEvent.change(input, { target: { value: "codex" } });
      fireEvent.keyDown(input, { key: "Enter" });
      await waitFor(() => expect(mockResolve).toHaveBeenLastCalledWith("acme.kit", "acp.models", ["codex", "fast"]));

      fireEvent.click(screen.getAllByRole("button", { name: "Use global" })[0]!);
      await waitFor(() => expect(mockResolve).toHaveBeenLastCalledWith("acme.kit", "acp.models", ["claude", "fast"]));
    });

    it("sends the value in the create payload", async () => {
      mockCreate.mockResolvedValue({ ok: true });
      render(<ProjectFormModal onClose={() => {}} onSaved={() => {}} />);
      const input = await screen.findByDisplayValue("global-label");
      fireEvent.change(screen.getByPlaceholderText("/path/to/repo"), { target: { value: "/repo/extra" } });
      fireEvent.focus(input);
      fireEvent.change(input, { target: { value: "mine" } });
      fireEvent.keyDown(input, { key: "Enter" });
      fireEvent.click(screen.getByRole("button", { name: "Add" }));

      await waitFor(() =>
        expect(mockCreate).toHaveBeenCalledWith(
          expect.objectContaining({
            overrides: expect.objectContaining({ plugin_settings: { "acme.kit": { label: "mine" } } }),
          }),
        ),
      );
    });
  });
});
