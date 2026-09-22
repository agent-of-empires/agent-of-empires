// @vitest-environment jsdom

import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { ProjectFormModal } from "../ProjectFormModal";
import { updateProject } from "../../lib/api";
import type * as Api from "../../lib/api";

vi.mock("../../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof Api>()),
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

  it("pre-selects 'On' for worktree-by-default when the project has that override set", () => {
    render(
      <ProjectFormModal
        initial={{
          name: "extra",
          path: "/repo/extra",
          scope: "global",
          pinned: false,
          overrides: { worktree_enabled: true },
        }}
        onClose={() => {}}
        onSaved={() => {}}
      />,
    );

    const worktreeSelect = screen.getByText("Worktree by default").nextElementSibling as HTMLSelectElement;
    expect(worktreeSelect.value).toBe("on");
  });
});
