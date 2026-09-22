// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

vi.mock("../../../lib/api", () => ({
  fetchProfiles: vi.fn().mockResolvedValue([{ name: "default", is_default: true }]),
  createProfile: vi.fn(),
  renameProfile: vi.fn(),
  deleteProfile: vi.fn(),
}));

import { SettingsHeader } from "../SettingsHeader";

afterEach(() => {
  cleanup();
});

describe("SettingsHeader", () => {
  const baseProps = {
    onClose: () => {},
    saving: false,
    saveError: null as string | null,
    selectedProfile: "default",
    onSelectProfile: () => {},
    schema: [],
    schemaLoading: false,
    onSearchJump: () => {},
  };

  it("renders the title and a Back button that closes", () => {
    const onClose = vi.fn();
    render(<SettingsHeader {...baseProps} onClose={onClose} />);
    expect(screen.getByText("Settings")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /Back/ }));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it.each([
    [false, null],
    [true, null],
    [false, "Save failed: network error"],
    [true, "Save failed: network error"],
  ])("shows saving=%s and saveError=%j independently", (saving, saveError) => {
    render(<SettingsHeader {...baseProps} saving={saving} saveError={saveError} />);
    expect(!!screen.queryByText("Saving...")).toBe(saving);
    expect(screen.queryByTestId("settings-header-save-error")?.textContent ?? null).toBe(saveError);
  });
});
