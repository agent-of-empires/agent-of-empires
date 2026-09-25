// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { ScratchOverridesModal } from "../ScratchOverridesModal";

vi.mock("../../lib/api", () => ({
  fetchSettings: vi.fn(),
  getProfileSettings: vi.fn(),
  updateSettings: vi.fn(),
  updateProfileSettings: vi.fn(),
}));

import { fetchSettings, getProfileSettings, updateProfileSettings, updateSettings } from "../../lib/api";

const mockFetchGlobal = fetchSettings as ReturnType<typeof vi.fn>;
const mockFetchProfile = getProfileSettings as ReturnType<typeof vi.fn>;
const mockUpdateGlobal = updateSettings as ReturnType<typeof vi.fn>;
const mockUpdateProfile = updateProfileSettings as ReturnType<typeof vi.fn>;

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

function smartRenameSelect() {
  return screen.getByText("Smart session rename").nextElementSibling as HTMLSelectElement;
}

function saveButton() {
  return screen.getByRole("button", { name: "Save" }) as HTMLButtonElement;
}

function renderModal(onClose: () => void = () => {}) {
  return render(<ScratchOverridesModal profile="work" onClose={onClose} />);
}

describe("ScratchOverridesModal", () => {
  it("loads the global scope's current override on open", async () => {
    mockFetchGlobal.mockResolvedValue({ session: { scratch_smart_rename: "off" } });
    renderModal();

    await waitFor(() => expect(mockFetchGlobal).toHaveBeenCalled());
    await waitFor(() => expect(smartRenameSelect().value).toBe("off"));
    expect(mockFetchProfile).not.toHaveBeenCalled();
  });

  it("re-fetches the profile scope's override when the scope toggle is switched", async () => {
    mockFetchGlobal.mockResolvedValue({ session: {} });
    mockFetchProfile.mockResolvedValue({ session: { scratch_smart_rename: "on" } });
    renderModal();
    await waitFor(() => expect(smartRenameSelect().value).toBe("inherit"));

    fireEvent.click(screen.getByRole("button", { name: "Profile-only" }));

    await waitFor(() => expect(mockFetchProfile).toHaveBeenCalledWith("work"));
    await waitFor(() => expect(smartRenameSelect().value).toBe("on"));
  });

  it("saves the selected scope and value via the global settings API, then closes", async () => {
    mockFetchGlobal.mockResolvedValue({ session: {} });
    mockUpdateGlobal.mockResolvedValue(true);
    const onClose = vi.fn();
    renderModal(onClose);
    await waitFor(() => expect(smartRenameSelect().value).toBe("inherit"));

    fireEvent.change(smartRenameSelect(), { target: { value: "on" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(mockUpdateGlobal).toHaveBeenCalledWith({ session: { scratch_smart_rename: "on" } }));
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(mockUpdateProfile).not.toHaveBeenCalled();
  });

  it("saves through the profile settings API when profile scope is selected", async () => {
    mockFetchGlobal.mockResolvedValue({ session: {} });
    mockFetchProfile.mockResolvedValue({ session: {} });
    mockUpdateProfile.mockResolvedValue(true);
    const onClose = vi.fn();
    renderModal(onClose);
    await waitFor(() => expect(smartRenameSelect().value).toBe("inherit"));
    fireEvent.click(screen.getByRole("button", { name: "Profile-only" }));
    await waitFor(() => expect(mockFetchProfile).toHaveBeenCalled());

    fireEvent.change(smartRenameSelect(), { target: { value: "off" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() =>
      expect(mockUpdateProfile).toHaveBeenCalledWith("work", { session: { scratch_smart_rename: "off" } }),
    );
    await waitFor(() => expect(onClose).toHaveBeenCalled());
    expect(mockUpdateGlobal).not.toHaveBeenCalled();
  });

  it("shows the error and stays open when saving fails", async () => {
    mockFetchGlobal.mockResolvedValue({ session: {} });
    mockUpdateGlobal.mockResolvedValue(false);
    const onClose = vi.fn();
    renderModal(onClose);
    await waitFor(() => expect(smartRenameSelect().value).toBe("inherit"));

    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(screen.getByText("Update failed")).toBeTruthy());
    expect(onClose).not.toHaveBeenCalled();
  });

  it("blocks Save and shows an error when the initial load fails, instead of silently defaulting to 'inherit'", async () => {
    mockFetchGlobal.mockResolvedValue(null);
    renderModal();

    await waitFor(() => expect(screen.getByText("Failed to load current setting")).toBeTruthy());
    expect(saveButton().disabled).toBe(true);
    expect(mockUpdateGlobal).not.toHaveBeenCalled();
  });

  it("blocks Save while a scope switch's refetch is still in flight, so it cannot save the old scope's value", async () => {
    mockFetchGlobal.mockResolvedValue({ session: {} });
    let resolveProfileFetch: (v: unknown) => void;
    mockFetchProfile.mockReturnValueOnce(
      new Promise((resolve) => {
        resolveProfileFetch = resolve;
      }),
    );
    renderModal();
    await waitFor(() => expect(smartRenameSelect().value).toBe("inherit"));

    fireEvent.click(screen.getByRole("button", { name: "Profile-only" }));
    expect(saveButton().disabled).toBe(true);

    resolveProfileFetch!({ session: { scratch_smart_rename: "on" } });
    await waitFor(() => expect(smartRenameSelect().value).toBe("on"));
    expect(saveButton().disabled).toBe(false);
  });

  it("exposes dialog semantics and an accessible name for the select", async () => {
    mockFetchGlobal.mockResolvedValue({ session: {} });
    renderModal();
    await waitFor(() => expect(smartRenameSelect().value).toBe("inherit"));

    const dialog = screen.getByRole("dialog");
    expect(dialog.getAttribute("aria-modal")).toBe("true");
    expect(dialog.textContent).toContain("Scratch session settings");
    expect(screen.getByLabelText("Smart session rename")).toBe(smartRenameSelect());
  });

  it("closes on Escape", async () => {
    mockFetchGlobal.mockResolvedValue({ session: {} });
    const onClose = vi.fn();
    renderModal(onClose);
    await waitFor(() => expect(smartRenameSelect().value).toBe("inherit"));

    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).toHaveBeenCalled();
  });

  it("ignores Escape while a save is in flight", async () => {
    mockFetchGlobal.mockResolvedValue({ session: {} });
    mockUpdateGlobal.mockImplementation(() => new Promise(() => {}));
    const onClose = vi.fn();
    renderModal(onClose);
    await waitFor(() => expect(smartRenameSelect().value).toBe("inherit"));

    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).not.toHaveBeenCalled();
  });

  it("guards Enter-to-submit against a load failure, since the select isn't excluded like buttons/inputs are", async () => {
    mockFetchGlobal.mockResolvedValue(null);
    renderModal();
    await waitFor(() => expect(screen.getByText("Failed to load current setting")).toBeTruthy());

    fireEvent.keyDown(smartRenameSelect(), { key: "Enter" });
    expect(mockUpdateGlobal).not.toHaveBeenCalled();
  });

  it("guards Enter-to-submit while a scope switch's refetch is still in flight", async () => {
    mockFetchGlobal.mockResolvedValue({ session: {} });
    mockFetchProfile.mockReturnValueOnce(new Promise(() => {}));
    renderModal();
    await waitFor(() => expect(smartRenameSelect().value).toBe("inherit"));

    fireEvent.click(screen.getByRole("button", { name: "Profile-only" }));
    fireEvent.keyDown(smartRenameSelect(), { key: "Enter" });
    expect(mockUpdateGlobal).not.toHaveBeenCalled();
    expect(mockUpdateProfile).not.toHaveBeenCalled();
  });

  it("loads a profile with no existing override as 'default', distinct from an explicit 'inherit'", async () => {
    mockFetchGlobal.mockResolvedValue({ session: {} });
    mockFetchProfile.mockResolvedValue({ session: {} });
    renderModal();
    await waitFor(() => expect(smartRenameSelect().value).toBe("inherit"));

    fireEvent.click(screen.getByRole("button", { name: "Profile-only" }));

    await waitFor(() => expect(smartRenameSelect().value).toBe("default"));
  });

  it(
    "saving an untouched profile with no existing override clears it (null) instead of writing an explicit " +
      "'inherit', which would silently start overriding the global scratch setting",
    async () => {
      mockFetchGlobal.mockResolvedValue({ session: {} });
      mockFetchProfile.mockResolvedValue({ session: {} });
      mockUpdateProfile.mockResolvedValue(true);
      renderModal();
      fireEvent.click(screen.getByRole("button", { name: "Profile-only" }));
      await waitFor(() => expect(smartRenameSelect().value).toBe("default"));

      fireEvent.click(screen.getByRole("button", { name: "Save" }));

      await waitFor(() =>
        expect(mockUpdateProfile).toHaveBeenCalledWith("work", { session: { scratch_smart_rename: null } }),
      );
    },
  );

  it("preserves an explicit profile-level 'inherit' override as-is, distinct from clearing it", async () => {
    mockFetchGlobal.mockResolvedValue({ session: {} });
    mockFetchProfile.mockResolvedValue({ session: { scratch_smart_rename: "inherit" } });
    mockUpdateProfile.mockResolvedValue(true);
    renderModal();
    fireEvent.click(screen.getByRole("button", { name: "Profile-only" }));
    await waitFor(() => expect(smartRenameSelect().value).toBe("inherit"));

    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() =>
      expect(mockUpdateProfile).toHaveBeenCalledWith("work", { session: { scratch_smart_rename: "inherit" } }),
    );
  });

  it("does not submit on Enter while browsing options in the select, even after a successful load", async () => {
    // "on" is distinct from the "inherit" initial default: a value equal to the default wouldn't
    // prove the load (and the loadedScope it flips) landed at all.
    mockFetchGlobal.mockResolvedValue({ session: { scratch_smart_rename: "on" } });
    const onClose = vi.fn();
    renderModal(onClose);
    await waitFor(() => expect(smartRenameSelect().value).toBe("on"));

    fireEvent.keyDown(smartRenameSelect(), { key: "Enter" });
    expect(mockUpdateGlobal).not.toHaveBeenCalled();
    expect(onClose).not.toHaveBeenCalled();
  });
});
