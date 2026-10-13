// @vitest-environment jsdom

import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { ProjectFormModal } from "../ProjectFormModal";
import { createProject, updateProject } from "../../lib/api";
import type * as Api from "../../lib/api";

vi.mock("../../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof Api>()),
  createProject: vi.fn(),
  updateProject: vi.fn(),
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

const baseField = () =>
  screen.getByPlaceholderText("blank = inherit global default, then auto-detect") as HTMLInputElement;

afterEach(() => {
  cleanup();
  vi.resetAllMocks();
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
});
