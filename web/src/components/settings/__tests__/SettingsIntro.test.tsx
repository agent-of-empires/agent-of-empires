// @vitest-environment jsdom

import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";

import { SettingsIntro, SettingsIntroReopen } from "../SettingsIntro";

// Catppuccin Latte as `/api/themes/catppuccin-latte` resolves it, the built-in light theme where
// the brand accent and text-secondary fall below AA. A token missing here is one not checked.
const LATTE: Record<string, string> = {
  "text-primary": "#4c4f69",
  "text-subtle": "#56585f",
  "text-secondary": "#209fb5",
  "surface-850": "#f5f6f9",
  "surface-900": "#eff1f5",
};

function contrast(a: string, b: string): number {
  const luminance = (hex: string) => {
    const [r, g, bl] = [1, 3, 5]
      .map((i) => parseInt(hex.slice(i, i + 2), 16) / 255)
      .map((c) => (c <= 0.03928 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4)));
    return 0.2126 * r! + 0.7152 * g! + 0.0722 * bl!;
  };
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi! + 0.05) / (lo! + 0.05);
}

const restingText = (el: HTMLElement) => /(?:^|\s)text-text-([a-z-]+)/.exec(el.className)?.[1];

afterEach(cleanup);

describe("SettingsIntro", () => {
  it("keeps its controls at AA text contrast in the light theme", () => {
    render(
      <div>
        <SettingsIntro id="worktree" onFold={() => {}} />
        <SettingsIntroReopen onReopen={() => {}} />
      </div>,
    );
    // The intro card is surface-850; the About reopen sits on Settings' surface-900.
    const cases: [string, string][] = [
      ["Details", "surface-850"],
      ["Got it", "surface-850"],
      ["About this tab", "surface-900"],
    ];
    for (const [name, surface] of cases) {
      const control = screen.getByRole("button", { name: new RegExp(name) });
      const token = restingText(control);
      const fg = token && LATTE[`text-${token}`];
      expect(fg, `${name}: ${control.className}`).toBeTruthy();
      expect(contrast(fg!, LATTE[surface]!), name).toBeGreaterThanOrEqual(4.5);
    }
  });
});
