// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { SettingsHeader } from "../SettingsHeader";

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
  };

  it("Back closes", () => {
    const onClose = vi.fn();
    render(<SettingsHeader {...baseProps} onClose={onClose} />);
    fireEvent.click(screen.getByRole("button", { name: /Back/ }));
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("on a tab page, the mobile Back returns to the section list", () => {
    const onClose = vi.fn();
    const onBackToList = vi.fn();
    render(<SettingsHeader {...baseProps} onClose={onClose} onBackToList={onBackToList} />);
    fireEvent.click(screen.getByRole("button", { name: /All settings/ }));
    expect(onBackToList).toHaveBeenCalledTimes(1);
    expect(onClose).not.toHaveBeenCalled();
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
