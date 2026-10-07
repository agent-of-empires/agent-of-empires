import { describe, expect, it } from "vitest";

import { getAttentionBadgeColors } from "../attentionBadgeColors";
import type { ResolvedTheme } from "../theme";

function theme(cssVars: Record<string, string>): ResolvedTheme {
  return {
    name: "test",
    source: "builtin",
    appearance: "dark",
    web: { cssVars },
    terminal: { cssVars: {} },
    syntax: { shikiTheme: "test" },
  };
}

describe("getAttentionBadgeColors", () => {
  it("falls back to the index.css defaults when there is no resolved theme yet", () => {
    expect(getAttentionBadgeColors(null)).toEqual({
      unreadBg: "#38bdf8",
      unreadFg: "#000000",
      waitingBg: "#fbbf24",
      waitingFg: "#000000",
    });
  });

  it("reads the resolved theme's unread/waiting accents", () => {
    const colors = getAttentionBadgeColors(
      theme({ "--color-status-unread": "#7287fd", "--color-status-waiting": "#fe640b" }),
    );
    expect(colors.unreadBg).toBe("#7287fd");
    expect(colors.waitingBg).toBe("#fe640b");
  });

  it("picks a white foreground for a dark custom-theme accent", () => {
    const colors = getAttentionBadgeColors(
      theme({ "--color-status-unread": "#0f0f11", "--color-status-waiting": "#0f0f11" }),
    );
    expect(colors.unreadFg).toBe("#ffffff");
    expect(colors.waitingFg).toBe("#ffffff");
  });
});
