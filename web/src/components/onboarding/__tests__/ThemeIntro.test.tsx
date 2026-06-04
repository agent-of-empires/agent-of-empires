// @vitest-environment jsdom
//
// Contract test for the first-run theme welcome modal. Live behavior (auto
// show, persistence across reload, handoff to the tour) is covered by
// tests/live/theme-onboarding.spec.ts; this file drills into the click ->
// persist -> dispatch flow, the persist-then-paint failure path, and dismiss.

import { afterEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";

const updateProfileSettings = vi.fn(() => Promise.resolve(true));
vi.mock("../../../lib/api", () => ({
  fetchThemes: vi.fn(() =>
    Promise.resolve(["default", "modus-vivendi", "empire"]),
  ),
  fetchProfiles: vi.fn(() =>
    Promise.resolve([
      { name: "work", is_default: false },
      { name: "default", is_default: true },
    ]),
  ),
  updateProfileSettings: (name: string, updates: Record<string, unknown>) =>
    updateProfileSettings(name, updates),
}));

const dispatchSpy = vi.fn();
vi.mock("../../../hooks/useResolvedTheme", () => ({
  dispatchThemePickerChanged: (name?: string) => dispatchSpy(name),
}));

import { ThemeIntro } from "../ThemeIntro";

afterEach(() => {
  cleanup();
  dispatchSpy.mockClear();
  updateProfileSettings.mockClear();
  updateProfileSettings.mockImplementation(() => Promise.resolve(true));
});

async function mount() {
  const onDone = vi.fn();
  render(<ThemeIntro onDone={onDone} />);
  await waitFor(() =>
    expect(screen.getByRole("option", { name: "modus-vivendi" })).toBeTruthy(),
  );
  return { onDone };
}

describe("ThemeIntro", () => {
  it("loads the available themes as options", async () => {
    await mount();
    expect(screen.getAllByRole("option")).toHaveLength(3);
  });

  it("persists the picked theme to the default profile and repaints", async () => {
    await mount();
    fireEvent.click(screen.getByRole("option", { name: "modus-vivendi" }));
    await waitFor(() =>
      expect(updateProfileSettings).toHaveBeenCalledWith("default", {
        theme: { name: "modus-vivendi" },
      }),
    );
    expect(dispatchSpy).toHaveBeenCalledWith("modus-vivendi");
    expect(
      screen.getByRole("option", { name: "modus-vivendi" }).getAttribute(
        "aria-selected",
      ),
    ).toBe("true");
  });

  it("lets the user re-pick another theme", async () => {
    await mount();
    fireEvent.click(screen.getByRole("option", { name: "modus-vivendi" }));
    await waitFor(() => expect(dispatchSpy).toHaveBeenCalledWith("modus-vivendi"));
    fireEvent.click(screen.getByRole("option", { name: "empire" }));
    await waitFor(() => expect(dispatchSpy).toHaveBeenCalledWith("empire"));
    expect(updateProfileSettings).toHaveBeenCalledTimes(2);
  });

  it("shows an error and does not repaint when the save fails", async () => {
    updateProfileSettings.mockImplementation(() => Promise.resolve(false));
    await mount();
    fireEvent.click(screen.getByRole("option", { name: "empire" }));
    await waitFor(() => expect(screen.getByRole("alert")).toBeTruthy());
    expect(dispatchSpy).not.toHaveBeenCalled();
    // Highlight reverts so the grid never claims an unsaved theme is active.
    expect(
      screen.getByRole("option", { name: "empire" }).getAttribute(
        "aria-selected",
      ),
    ).toBe("false");
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
