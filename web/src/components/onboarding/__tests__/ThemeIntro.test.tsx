// @vitest-environment jsdom
//
// Contract test for the first-run theme welcome modal. Browser behavior (auto
// show, persistence across reload, handoff to the tour) is covered by the
// mocked Playwright spec tests/theme-onboarding.spec.ts; this file drills into
// the click -> persist -> dispatch flow, the persist-then-paint failure path,
// and dismiss.

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

const updateTheme = vi.fn(() => Promise.resolve(true));
vi.mock("../../../lib/api", () => ({
  fetchThemes: vi.fn(() => Promise.resolve(["default", "modus-vivendi", "empire"])),
  // The theme is a global preference, written via the dedicated /api/theme
  // endpoint (not a profile settings PATCH).
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

describe("ThemeIntro", () => {
  it("loads the available themes as options", async () => {
    await mount();
    expect(screen.getAllByRole("option")).toHaveLength(3);
  });

  it("persists the picked theme globally and repaints", async () => {
    await mount();
    fireEvent.click(screen.getByRole("option", { name: "modus-vivendi" }));
    await waitFor(() => expect(updateTheme).toHaveBeenCalledWith({ name: "modus-vivendi" }));
    expect(dispatchSpy).toHaveBeenCalledWith("modus-vivendi");
    expect(screen.getByRole("option", { name: "modus-vivendi" }).getAttribute("aria-selected")).toBe("true");
  });

  it("lets the user re-pick another theme", async () => {
    await mount();
    fireEvent.click(screen.getByRole("option", { name: "modus-vivendi" }));
    await waitFor(() => expect(dispatchSpy).toHaveBeenCalledWith("modus-vivendi"));
    fireEvent.click(screen.getByRole("option", { name: "empire" }));
    await waitFor(() => expect(dispatchSpy).toHaveBeenCalledWith("empire"));
    expect(updateTheme).toHaveBeenCalledTimes(2);
  });

  it("shows an error and does not repaint when the save fails", async () => {
    updateTheme.mockImplementation(() => Promise.resolve(false));
    await mount();
    fireEvent.click(screen.getByRole("option", { name: "empire" }));
    await waitFor(() => expect(screen.getByRole("alert")).toBeTruthy());
    expect(dispatchSpy).not.toHaveBeenCalled();
    // Highlight reverts so the grid never claims an unsaved theme is active.
    expect(screen.getByRole("option", { name: "empire" }).getAttribute("aria-selected")).toBe("false");
  });

  it("dismisses via Continue", async () => {
    const { onDone } = await mount();
    fireEvent.click(screen.getByRole("button", { name: "Continue" }));
    expect(onDone).toHaveBeenCalledTimes(1);
  });

  it("dismisses via Escape", async () => {
    const { onDone } = await mount();
    fireEvent.keyDown(window, { key: "Escape" });
    expect(onDone).toHaveBeenCalledTimes(1);
  });
});
