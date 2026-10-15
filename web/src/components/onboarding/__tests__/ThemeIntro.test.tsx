// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

const updateTheme = vi.fn(() => Promise.resolve(true));
vi.mock("../../../lib/api", () => ({
  fetchThemes: vi.fn(() => Promise.resolve(["default", "modus-vivendi", "empire"])),
  updateTheme: (patch: { name?: string }) => updateTheme(patch),
}));

const dispatchSpy = vi.fn();
vi.mock("../../../hooks/useResolvedTheme", () => ({
  dispatchThemePickerChanged: (name?: string) => dispatchSpy(name),
}));

import { ThemeIntro } from "../ThemeIntro";

afterEach(() => {
  cleanup();
  dispatchSpy.mockClear();
  updateTheme.mockClear();
  updateTheme.mockImplementation(() => Promise.resolve(true));
});

async function mount() {
  const onDone = vi.fn();
  render(<ThemeIntro onDone={onDone} />);
  await waitFor(() => expect(screen.getByRole("option", { name: "modus-vivendi" })).toBeTruthy());
  return { onDone };
}

const option = (name: string) => screen.getByRole("option", { name });

describe("ThemeIntro", () => {
  it("persists each pick globally and repaints, allowing re-picks", async () => {
    await mount();
    expect(screen.getAllByRole("option")).toHaveLength(3);
    fireEvent.click(option("modus-vivendi"));
    await waitFor(() => expect(updateTheme).toHaveBeenCalledWith({ name: "modus-vivendi" }));
    expect(dispatchSpy).toHaveBeenCalledWith("modus-vivendi");
    expect(option("modus-vivendi").getAttribute("aria-selected")).toBe("true");
    fireEvent.click(option("empire"));
    await waitFor(() => expect(dispatchSpy).toHaveBeenCalledWith("empire"));
    expect(updateTheme).toHaveBeenCalledTimes(2);
  });

  it("shows an error, does not repaint, and reverts the highlight when the save fails", async () => {
    updateTheme.mockImplementation(() => Promise.resolve(false));
    await mount();
    fireEvent.click(option("empire"));
    await waitFor(() => expect(screen.getByRole("alert")).toBeTruthy());
    expect(dispatchSpy).not.toHaveBeenCalled();
    expect(option("empire").getAttribute("aria-selected")).toBe("false");
  });

  it("dismisses via Escape", async () => {
    const { onDone } = await mount();
    fireEvent.keyDown(window, { key: "Escape" });
    expect(onDone).toHaveBeenCalledTimes(1);
  });
});
