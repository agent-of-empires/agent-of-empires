// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { createElement, type ReactNode } from "react";
import TourRunner from "./TourRunner";
import { INSTALL_STEPS, TOUR_STEPS } from "../../lib/tourSteps";
import type { InstallHint } from "../../lib/platform";

const platform = vi.hoisted(() => ({ hint: null as InstallHint | null }));
vi.mock("../../lib/platform", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../lib/platform")>()),
  installHint: () => platform.hint,
}));

// Renders every step's content at once, so a test can see which steps carry the hint.
vi.mock("react-joyride", () => ({
  Joyride: ({ steps }: { steps: { id: string; content: ReactNode }[] }) =>
    createElement(
      "div",
      null,
      steps.map((s) => createElement("section", { key: s.id, "data-step": s.id }, s.content)),
    ),
  EVENTS: {},
  ACTIONS: {},
  STATUS: {},
}));

afterEach(() => {
  cleanup();
  platform.hint = null;
});

const stepText = (id: string) => document.querySelector(`[data-step="${id}"]`)?.textContent ?? "";

describe("TourRunner install hint", () => {
  it.each(["ios", "push", "other"] as const)("adds the %s line to the last step only", (hint) => {
    platform.hint = hint;
    render(<TourRunner run steps={[...TOUR_STEPS]} onFinish={() => {}} />);
    expect(screen.getAllByText(INSTALL_STEPS[hint].tour)).toHaveLength(1);
    expect(stepText("topbar-more")).toContain(INSTALL_STEPS[hint].tour);
  });

  it("shows no install line where the platform needs none", () => {
    render(<TourRunner run steps={[...TOUR_STEPS]} onFinish={() => {}} />);
    for (const { tour: text } of Object.values(INSTALL_STEPS)) expect(screen.queryByText(text)).toBeNull();
    expect(stepText("topbar-more")).toBe(TOUR_STEPS.find((s) => s.id === "topbar-more")?.body);
  });
});
