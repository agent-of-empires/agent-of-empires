// @vitest-environment jsdom

import { useState } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { ProjectFormModal } from "../ProjectFormModal";
import type * as Api from "../../lib/api";
import type { ProjectInfo } from "../../lib/types";

vi.mock("../../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof Api>()),
  createProject: vi.fn(),
  updateProject: vi.fn(),
  getSettingsSchema: vi.fn(),
  fetchMachineSettings: vi.fn(),
  resolvePluginOptions: vi.fn(),
}));

function Form({ saved = async () => {} }: { saved?: () => Promise<void> }) {
  const [open, setOpen] = useState(true);
  return open ? (
    <ProjectFormModal
      profile="alpha"
      initial={{ name: "extra", path: "/repo/extra", scope: "profile", default_base_branch: "develop", pinned: true }}
      onClose={() => setOpen(false)}
      onSaved={saved}
    />
  ) : (
    <p>Closed</p>
  );
}
import {
  createProject,
  fetchMachineSettings,
  getSettingsSchema,
  resolvePluginOptions,
  updateProject,
} from "../../lib/api";

const baseField = () =>
  screen.getByPlaceholderText("blank = inherit global default, then auto-detect") as HTMLInputElement;
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
  vi.resetAllMocks();
  vi.unstubAllGlobals();
});

describe("ProjectFormModal", () => {
  it("keeps the edited value and form open when persistence fails", async () => {
    vi.mocked(updateProject).mockResolvedValue({ ok: false, error: "Registry is read-only" });
    render(<Form />);
    expect(baseField().value).toBe("develop");
    fireEvent.change(baseField(), { target: { value: "release" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await screen.findByText("Registry is read-only");
    expect(baseField().value).toBe("release");
    expect(screen.queryByText("Closed")).toBeNull();
  });

  it("stays open until the committed registry refresh finishes", async () => {
    vi.mocked(updateProject).mockResolvedValue({ ok: true });
    let finishRefresh!: () => void;
    const refresh = new Promise<void>((resolve) => {
      finishRefresh = resolve;
    });
    let refreshing = false;
    render(
      <Form
        saved={() => {
          refreshing = true;
          return refresh;
        }}
      />,
    );
    fireEvent.change(baseField(), { target: { value: "release" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(refreshing).toBe(true));
    expect(screen.queryByText("Closed")).toBeNull();
    expect(baseField().value).toBe("release");
    finishRefresh();
    await screen.findByText("Closed");
  });

  it.each([
    ["inherit", undefined],
    ["on", true],
    ["off", false],
  ])("creates with the exact served profile and container-start %s override", async (choice, expected) => {
    vi.mocked(createProject).mockResolvedValue({ ok: true });
    const onSaved = vi.fn();
    const onClose = vi.fn();
    render(<ProjectFormModal profile="served" onClose={onClose} onSaved={onSaved} />);
    fireEvent.change(screen.getByPlaceholderText("/path/to/repo"), { target: { value: "/repo/extra" } });
    fireEvent.change(screen.getByText("Start in container by default").nextElementSibling!, {
      target: { value: choice },
    });
    fireEvent.change(screen.getByText("Worktree by default").nextElementSibling!, { target: { value: "on" } });
    fireEvent.click(screen.getByRole("button", { name: "Profile-only" }));
    fireEvent.click(screen.getByRole("button", { name: "Add" }));
    await waitFor(() =>
      expect(createProject).toHaveBeenCalledWith(
        expect.objectContaining({
          path: "/repo/extra",
          scope: "profile",
          profile: "served",
          overrides: {
            worktree_enabled: true,
            smart_rename: undefined,
            sandbox_enabled: expected,
          },
        }),
      ),
    );
    await waitFor(() => expect(onClose).toHaveBeenCalledOnce());
    expect(onSaved).toHaveBeenCalledOnce();
  });

  it.each([
    ["on", true],
    ["off", false],
    ["inherit", null],
  ])("patches the container-start %s choice at the unchanged registration identity", async (choice, expected) => {
    vi.mocked(updateProject).mockResolvedValue({ ok: true });
    render(
      <ProjectFormModal
        profile="served"
        initial={{
          name: "friendly",
          path: "/repo/extra",
          scope: "profile",
          pinned: false,
          overrides: { worktree_enabled: false, sandbox_enabled: choice === "on" ? false : true },
        }}
        onClose={() => {}}
        onSaved={() => {}}
      />,
    );
    fireEvent.change(screen.getByText("Start in container by default").nextElementSibling!, {
      target: { value: choice },
    });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(updateProject).toHaveBeenCalledWith(
        "/repo/extra",
        { scope: "profile", profile: "served" },
        {
          default_base_branch: null,
          overrides: { worktree_enabled: false, smart_rename: null, sandbox_enabled: expected },
        },
      ),
    );
  });

  it("omits unchanged overrides while updating the base branch", async () => {
    vi.mocked(updateProject).mockResolvedValue({ ok: true });
    render(<Form />);
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(updateProject).toHaveBeenCalledWith(
        "/repo/extra",
        { scope: "profile", profile: "alpha" },
        {
          default_base_branch: "develop",
        },
      ),
    );
  });

  it("pre-selects 'Off' for smart-rename when the project has that override set", () => {
    render(
      <ProjectFormModal
        profile="alpha"
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

    const editProject = (overrides?: ProjectInfo["overrides"], scope: ProjectInfo["scope"] = "global") =>
      render(
        <ProjectFormModal
          profile="served"
          initial={{ name: "extra", path: "/repo/extra", scope, pinned: false, overrides }}
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

    it.each(["global", "profile"] as const)(
      "PATCHes a changed %s project value at the HTTP boundary",
      async (scope) => {
        const actual = await vi.importActual<typeof Api>("../../lib/api");
        mockUpdate.mockImplementation(actual.updateProject);
        const canonical: ProjectInfo = {
          name: "extra",
          path: "/repo/extra",
          scope,
          pinned: false,
          overrides: { worktree_enabled: false, plugin_settings: { "acme.kit": { label: "mine" } } },
        };
        const fetch = vi.fn().mockResolvedValue(
          new Response(JSON.stringify(canonical), {
            headers: {
              "Content-Type": "application/json",
              "aoe-runtime-epoch": "native-test",
              "aoe-runtime-revision": "7",
            },
          }),
        );
        vi.stubGlobal("fetch", fetch);
        editProject({ worktree_enabled: false }, scope);
        const input = await screen.findByDisplayValue("global-label");
        fireEvent.focus(input);
        fireEvent.change(input, { target: { value: "mine" } });
        fireEvent.keyDown(input, { key: "Enter" });
        fireEvent.click(screen.getByRole("button", { name: "Save" }));
        await waitFor(() => expect(fetch).toHaveBeenCalledOnce());
        const [url, init] = fetch.mock.calls[0]!;
        const target = new URL(String(url), "http://dashboard.test");
        expect(target.pathname).toBe("/api/projects/%2Frepo%2Fextra");
        expect(target.searchParams.get("scope")).toBe(scope);
        expect(target.searchParams.get("profile")).toBe(scope === "profile" ? "served" : null);
        expect(init).toMatchObject({ method: "PATCH", headers: { "Content-Type": "application/json" } });
        expect(JSON.parse(String(init.body))).toEqual({
          default_base_branch: null,
          overrides: {
            worktree_enabled: false,
            smart_rename: null,
            sandbox_enabled: null,
            plugin_settings: { "acme.kit": { label: "mine" } },
          },
        });
        await expect(mockUpdate.mock.results[0]!.value).resolves.toEqual({ ok: true, project: canonical });
      },
    );

    it("'Use global' clears only the saved project value with null over HTTP", async () => {
      const actual = await vi.importActual<typeof Api>("../../lib/api");
      mockUpdate.mockImplementation(actual.updateProject);
      const canonical: ProjectInfo = {
        name: "Canonical registration",
        path: "/repo/extra",
        scope: "global",
        pinned: false,
        overrides: { worktree_enabled: false },
      };
      const fetch = vi.fn().mockResolvedValue(
        new Response(JSON.stringify(canonical), {
          headers: {
            "Content-Type": "application/json",
            "aoe-runtime-epoch": "native-test",
            "aoe-runtime-revision": "8",
          },
        }),
      );
      vi.stubGlobal("fetch", fetch);
      editProject({ worktree_enabled: false, plugin_settings: { "acme.kit": { label: "saved" } } });
      await screen.findByDisplayValue("saved");
      fireEvent.click(screen.getByRole("button", { name: "Use global" }));
      expect(screen.getByDisplayValue("global-label")).toBeTruthy();
      fireEvent.click(screen.getByRole("button", { name: "Save" }));
      await waitFor(() => expect(fetch).toHaveBeenCalledOnce());
      const [url, init] = fetch.mock.calls[0]!;
      const target = new URL(String(url), "http://dashboard.test");
      expect(target.pathname).toBe("/api/projects/%2Frepo%2Fextra");
      expect(target.searchParams.get("scope")).toBe("global");
      expect(target.searchParams.has("profile")).toBe(false);
      expect(init).toMatchObject({ method: "PATCH" });
      expect(JSON.parse(String(init.body))).toEqual({
        default_base_branch: null,
        overrides: {
          worktree_enabled: false,
          smart_rename: null,
          sandbox_enabled: null,
          plugin_settings: { "acme.kit": { label: null } },
        },
      });
      await expect(mockUpdate.mock.results[0]!.value).resolves.toEqual({ ok: true, project: canonical });
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
      render(<ProjectFormModal profile="served" onClose={() => {}} onSaved={() => {}} />);
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
