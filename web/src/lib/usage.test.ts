import { describe, expect, it } from "vitest";

import { formatDurationShort, parseUsageOverlayEnabled, usageLines, type UsageSummary } from "./usage";

const base: UsageSummary = {
  resets: 0,
  clears: 0,
  compactions: 0,
  compactionsAuto: 0,
  compactionsManual: 0,
  resumes: 0,
  prompts: 0,
  turns: 0,
  turnErrors: 0,
  contextStartedAt: null,
  contextPrompts: 0,
  contextTurns: 0,
  trackedSince: null,
  lastEventAt: null,
  tracked: false,
};

describe("usage overlay text", () => {
  it("matches the TUI duration format", () => {
    const min = 60_000;
    expect([20_000, 42 * min, 125 * min, 76 * 60 * min, -5_000].map(formatDurationShort)).toEqual([
      "<1m",
      "42m",
      "2h5m",
      "3d4h",
      "<1m",
    ]);
  });

  it("builds the breakdown lines", () => {
    const now = Date.parse("2026-09-29T12:00:00Z");
    const summary = {
      ...base,
      resets: 17,
      clears: 12,
      compactions: 3,
      compactionsAuto: 2,
      compactionsManual: 1,
      resumes: 2,
      contextStartedAt: new Date(now - 42 * 60_000).toISOString(),
      contextPrompts: 18,
      contextTurns: 17,
    };
    const created = new Date(now - 76 * 3_600_000).toISOString();
    expect(usageLines(summary, created, now)).toEqual(["clr 12 cmp 3/2a rsm 2", "42m 18p 17t age 3d4h"]);
  });

  it("defaults the overlay on", () => {
    expect(parseUsageOverlayEnabled(null)).toBe(true);
    expect(parseUsageOverlayEnabled({ session: { show_usage_overlay: false } })).toBe(false);
  });
});
