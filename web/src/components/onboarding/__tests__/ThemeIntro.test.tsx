// @vitest-environment jsdom

import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

const updateTheme = vi.fn(() => Promise.resolve("committed" as "committed" | "rejected" | "uncertain"));
const fetchSettings = vi.fn(() => Promise.resolve(null as { theme: { name: string } } | null));
vi.mock("../../../lib/api", () => ({
  fetchSettings: () => fetchSettings(),
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
  fetchSettings.mockClear();
  updateTheme.mockImplementation(() => Promise.resolve("committed"));
});

async function mount(tourFollows = true) {
  const onDone = vi.fn();
  const onSkipTour = vi.fn();
  render(<ThemeIntro onDone={onDone} onSkipTour={onSkipTour} tourFollows={tourFollows} />);
  await waitFor(() => expect(screen.getByRole("option", { name: "modus-vivendi" })).toBeTruthy());
  return { onDone, onSkipTour };
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
    updateTheme.mockImplementation(() => Promise.resolve("rejected"));
    await mount();
    fireEvent.click(option("empire"));
    await waitFor(() => expect(screen.getByRole("alert")).toBeTruthy());
    expect(dispatchSpy).not.toHaveBeenCalled();
    expect(option("empire").getAttribute("aria-selected")).toBe("false");
  });

  it("treats an uncertain save as done only when a fetch shows the pick", async () => {
    updateTheme.mockImplementation(() => Promise.resolve("uncertain"));
    await mount();

    fetchSettings.mockResolvedValueOnce({ theme: { name: "modus-vivendi" } });
    fireEvent.click(option("modus-vivendi"));
    await waitFor(() => expect(dispatchSpy).toHaveBeenCalledWith("modus-vivendi"));

    fetchSettings.mockResolvedValueOnce({ theme: { name: "modus-vivendi" } });
    fireEvent.click(option("empire"));
    await waitFor(() => expect(screen.getByRole("alert")).toBeTruthy());
    expect(dispatchSpy).toHaveBeenCalledTimes(1);
  });

  it("resends an uncertain save past an old fetch, and reports failure only once retries stay unconfirmed", async () => {
    await mount();
    updateTheme.mockImplementation(() => Promise.resolve("uncertain"));
    updateTheme
      .mockImplementationOnce(() => Promise.resolve("uncertain"))
      .mockImplementationOnce(() => Promise.resolve("committed"));
    fetchSettings.mockResolvedValueOnce({ theme: { name: "default" } });
    fireEvent.click(option("modus-vivendi"));
    await waitFor(() => expect(dispatchSpy).toHaveBeenCalledWith("modus-vivendi"));
    expect(updateTheme).toHaveBeenCalledTimes(2);

    updateTheme.mockClear();
    fireEvent.click(option("empire"));
    await waitFor(() => expect(screen.getByRole("alert")).toBeTruthy());
    expect(updateTheme).toHaveBeenCalledTimes(3);
    expect(dispatchSpy).toHaveBeenCalledTimes(1);
  });

  it("Continue hands off to the tour; Escape skips it", async () => {
    const cases: Array<[string, () => void, "onDone" | "onSkipTour"]> = [
      ["Continue", () => fireEvent.click(screen.getByRole("button", { name: "Continue" })), "onDone"],
      ["Escape", () => fireEvent.keyDown(window, { key: "Escape" }), "onSkipTour"],
    ];
    for (const [how, dismiss, expected] of cases) {
      const handlers = await mount();
      dismiss();
      expect(handlers[expected], how).toHaveBeenCalledTimes(1);
      expect(handlers[expected === "onDone" ? "onSkipTour" : "onDone"], how).not.toHaveBeenCalled();
      cleanup();
    }
  });

  it("promises the tour only where it auto-launches", async () => {
    for (const tourFollows of [true, false]) {
      await mount(tourFollows);
      expect(screen.queryByText("Next: a short tour · Esc skips") !== null, `tourFollows=${tourFollows}`).toBe(
        tourFollows,
      );
      cleanup();
    }
  });
});
