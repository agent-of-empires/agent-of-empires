// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";

import { SettingsHeader } from "../SettingsHeader";
import { setHeaderPageSlot } from "../../../lib/headerSlot";

afterEach(() => {
  cleanup();
});

describe("SettingsHeader", () => {
  const baseProps = {
    onClose: () => {},
    saving: false,
    saveError: null as string | null,
    schema: [],
    schemaLoading: false,
    onSearchJump: () => {},
    sectionLabel: "Theme",
  };

  it("Back closes Settings from the phone's row", () => {
    const onClose = vi.fn();
    render(<SettingsHeader {...baseProps} onClose={onClose} />);
    fireEvent.click(screen.getByRole("button", { name: "Back" }));
    expect(onClose).toHaveBeenCalledTimes(1);
    // The section list's phone title is Settings itself.
    expect(screen.getByRole("heading", { level: 1 }).textContent).toBe("Settings");
  });

  it("on a tab page, the mobile Back returns to the section list", () => {
    const onClose = vi.fn();
    const onBackToList = vi.fn();
    render(<SettingsHeader {...baseProps} onClose={onClose} onBackToList={onBackToList} />);
    fireEvent.click(screen.getByRole("button", { name: /All settings/ }));
    expect(onBackToList).toHaveBeenCalledTimes(1);
    expect(onClose).not.toHaveBeenCalled();
    // A section page titles itself.
    expect(screen.getByRole("heading", { level: 1 }).textContent).toBe("Theme");
  });

  it.each([
    [false, null],
    [true, null],
    [false, "Save failed: network error"],
    [true, "Save failed: network error"],
  ])("shows saving=%s and saveError=%j independently", (saving, saveError) => {
    render(<SettingsHeader {...baseProps} saving={saving} saveError={saveError} />);
    expect(!!screen.queryByText("Saving…")).toBe(saving);
    expect(screen.queryByTestId("settings-header-save-error")?.textContent ?? null).toBe(saveError);
  });

  it("on a wide screen fills the app header's page slot instead of a row of its own", () => {
    const slot = document.createElement("div");
    document.body.appendChild(slot);
    setHeaderPageSlot(slot);
    try {
      render(<SettingsHeader {...baseProps} saving />);
      expect(screen.queryByTestId("settings-header")).toBeNull();
      expect(slot.textContent).toContain("Saving…");
    } finally {
      act(() => setHeaderPageSlot(null));
      slot.remove();
    }
  });
});
