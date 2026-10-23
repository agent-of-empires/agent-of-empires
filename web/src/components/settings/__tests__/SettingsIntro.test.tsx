// @vitest-environment jsdom

import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";

import { SettingsIntro, SettingsIntroReopen } from "../SettingsIntro";

afterEach(cleanup);

describe("SettingsIntro", () => {
  // resolved.rs holds text-primary and text-subtle at AA on surface-850 and -900 in every
  // built-in; the brand ramp and text-secondary fall below it in Latte and Deep Ocean.
  it("keeps its controls on the foregrounds held at AA text contrast", () => {
    render(
      <div>
        <SettingsIntro id="worktree" onFold={() => {}} />
        <SettingsIntroReopen onReopen={() => {}} />
      </div>,
    );
    const cases: [string, string][] = [
      ["Details", "text-text-primary"],
      ["Got it", "text-text-primary"],
      ["About this tab", "text-text-subtle"],
    ];
    for (const [name, token] of cases) {
      const { className } = screen.getByRole("button", { name: new RegExp(name) });
      expect(className.split(/\s+/), name).toContain(token);
      expect(className, name).not.toMatch(/(?:^|\s)(?:text-brand-|text-text-secondary)/);
    }
  });
});
