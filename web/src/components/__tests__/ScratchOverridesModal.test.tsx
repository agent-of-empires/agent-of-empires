// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { ScratchOverridesModal } from "../ScratchOverridesModal";

vi.mock("../../lib/api", () => ({
  fetchScratchOverrides: vi.fn(),
  updateScratchOverrides: vi.fn(),
}));

import { fetchScratchOverrides, updateScratchOverrides } from "../../lib/api";

const mockFetch = fetchScratchOverrides as ReturnType<typeof vi.fn>;
const mockUpdate = updateScratchOverrides as ReturnType<typeof vi.fn>;

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

function smartRenameSelect() {
  return screen.getByText("Smart session rename").nextElementSibling as HTMLSelectElement;
}

describe("ScratchOverridesModal", () => {
  it("loads the global scope's current override on open", async () => {
    mockFetch.mockResolvedValue({ scope: "global", smart_rename: false });
    render(<ScratchOverridesModal onClose={() => {}} />);

    await waitFor(() => expect(mockFetch).toHaveBeenCalledWith("global"));
    await waitFor(() => expect(smartRenameSelect().value).toBe("off"));
  });

  it("re-fetches the profile scope's override when the scope toggle is switched", async () => {
    mockFetch.mockResolvedValueOnce({ scope: "global", smart_rename: undefined });
    mockFetch.mockResolvedValueOnce({ scope: "profile", smart_rename: true });
    render(<ScratchOverridesModal onClose={() => {}} />);
    await waitFor(() => expect(smartRenameSelect().value).toBe("inherit"));

    fireEvent.click(screen.getByRole("button", { name: "Profile-only" }));

    await waitFor(() => expect(mockFetch).toHaveBeenCalledWith("profile"));
    await waitFor(() => expect(smartRenameSelect().value).toBe("on"));
  });

  it("saves the selected scope and value, then closes", async () => {
    mockFetch.mockResolvedValue({ scope: "global", smart_rename: undefined });
    mockUpdate.mockResolvedValue({ ok: true });
    const onClose = vi.fn();
    render(<ScratchOverridesModal onClose={onClose} />);
    await waitFor(() => expect(smartRenameSelect().value).toBe("inherit"));

    fireEvent.change(smartRenameSelect(), { target: { value: "on" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(mockUpdate).toHaveBeenCalledWith("global", true));
    await waitFor(() => expect(onClose).toHaveBeenCalled());
  });

  it("sends null to clear the override back to 'Use global default'", async () => {
    mockFetch.mockResolvedValue({ scope: "global", smart_rename: true });
    mockUpdate.mockResolvedValue({ ok: true });
    render(<ScratchOverridesModal onClose={() => {}} />);
    await waitFor(() => expect(smartRenameSelect().value).toBe("on"));

    fireEvent.change(smartRenameSelect(), { target: { value: "inherit" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(mockUpdate).toHaveBeenCalledWith("global", null));
  });

  it("shows the error and stays open when saving fails", async () => {
    mockFetch.mockResolvedValue({ scope: "global", smart_rename: undefined });
    mockUpdate.mockResolvedValue({ ok: false, error: "boom" });
    const onClose = vi.fn();
    render(<ScratchOverridesModal onClose={onClose} />);
    await waitFor(() => expect(smartRenameSelect().value).toBe("inherit"));

    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(screen.getByText("boom")).toBeTruthy());
    expect(onClose).not.toHaveBeenCalled();
  });

  function saveButton() {
    return screen.getByRole("button", { name: "Save" }) as HTMLButtonElement;
  }

  it("blocks Save and shows an error when the initial load fails, instead of silently defaulting to 'inherit'", async () => {
    mockFetch.mockResolvedValue(null);
    render(<ScratchOverridesModal onClose={() => {}} />);

    await waitFor(() => expect(screen.getByText("Failed to load current setting")).toBeTruthy());
    expect(saveButton().disabled).toBe(true);
    expect(mockUpdate).not.toHaveBeenCalled();
  });

  it("blocks Save while a scope switch's refetch is still in flight, so it cannot save the old scope's value", async () => {
    mockFetch.mockResolvedValueOnce({ scope: "global", smart_rename: undefined });
    let resolveProfileFetch: (v: { scope: string; smart_rename?: boolean }) => void;
    mockFetch.mockReturnValueOnce(
      new Promise((resolve) => {
        resolveProfileFetch = resolve;
      }),
    );
    render(<ScratchOverridesModal onClose={() => {}} />);
    await waitFor(() => expect(smartRenameSelect().value).toBe("inherit"));

    fireEvent.click(screen.getByRole("button", { name: "Profile-only" }));
    expect(saveButton().disabled).toBe(true);

    resolveProfileFetch!({ scope: "profile", smart_rename: true });
    await waitFor(() => expect(smartRenameSelect().value).toBe("on"));
    expect(saveButton().disabled).toBe(false);
  });

  it("exposes dialog semantics and an accessible name for the select", async () => {
    mockFetch.mockResolvedValue({ scope: "global", smart_rename: undefined });
    render(<ScratchOverridesModal onClose={() => {}} />);
    await waitFor(() => expect(smartRenameSelect().value).toBe("inherit"));

    const dialog = screen.getByRole("dialog");
    expect(dialog.getAttribute("aria-modal")).toBe("true");
    expect(dialog.textContent).toContain("Scratch session settings");
    expect(screen.getByLabelText("Smart session rename")).toBe(smartRenameSelect());
  });

  it("closes on Escape", async () => {
    mockFetch.mockResolvedValue({ scope: "global", smart_rename: undefined });
    const onClose = vi.fn();
    render(<ScratchOverridesModal onClose={onClose} />);
    await waitFor(() => expect(smartRenameSelect().value).toBe("inherit"));

    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).toHaveBeenCalled();
  });

  it("ignores Escape while a save is in flight", async () => {
    mockFetch.mockResolvedValue({ scope: "global", smart_rename: undefined });
    mockUpdate.mockImplementation(() => new Promise(() => {}));
    const onClose = vi.fn();
    render(<ScratchOverridesModal onClose={onClose} />);
    await waitFor(() => expect(smartRenameSelect().value).toBe("inherit"));

    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).not.toHaveBeenCalled();
  });

  it("guards Enter-to-submit against a load failure, since the select isn't excluded like buttons/inputs are", async () => {
    mockFetch.mockResolvedValue(null);
    render(<ScratchOverridesModal onClose={() => {}} />);
    await waitFor(() => expect(screen.getByText("Failed to load current setting")).toBeTruthy());

    fireEvent.keyDown(smartRenameSelect(), { key: "Enter" });
    expect(mockUpdate).not.toHaveBeenCalled();
  });

  it("guards Enter-to-submit while a scope switch's refetch is still in flight", async () => {
    mockFetch.mockResolvedValueOnce({ scope: "global", smart_rename: undefined });
    mockFetch.mockReturnValueOnce(new Promise(() => {}));
    render(<ScratchOverridesModal onClose={() => {}} />);
    await waitFor(() => expect(smartRenameSelect().value).toBe("inherit"));

    fireEvent.click(screen.getByRole("button", { name: "Profile-only" }));
    fireEvent.keyDown(smartRenameSelect(), { key: "Enter" });
    expect(mockUpdate).not.toHaveBeenCalled();
  });

  it("does not submit on Enter while browsing options in the select, even after a successful load", async () => {
    // smart_rename: true maps to "on", distinct from the "inherit" initial default: a value
    // equal to the default wouldn't prove the load (and the loadedScope it flips) landed at all.
    mockFetch.mockResolvedValue({ scope: "global", smart_rename: true });
    const onClose = vi.fn();
    render(<ScratchOverridesModal onClose={onClose} />);
    await waitFor(() => expect(smartRenameSelect().value).toBe("on"));

    fireEvent.keyDown(smartRenameSelect(), { key: "Enter" });
    expect(mockUpdate).not.toHaveBeenCalled();
    expect(onClose).not.toHaveBeenCalled();
  });
});
