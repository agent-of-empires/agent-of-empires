import { describe, expect, it } from "vitest";

import { isQueuedPromptLong, queuedStripLayout } from "./queuedPromptsLayout";

describe("queuedPromptsLayout", () => {
  it("queuedStripLayout collapses past the desktop and mobile thresholds", () => {
    const cases = [
      // count, mobile, expanded -> visible, hidden, label, collapsed
      [1, false, false, 1, 0, null, false],
      [2, false, false, 2, 0, null, false],
      [5, false, false, 2, 3, "Show 3 more", true],
      [5, false, true, 5, 0, "Show less", false],
      [1, true, false, 1, 0, null, false],
      [4, true, false, 1, 3, "Show 3 more", true],
      // A drained queue drops the toggle while `expanded` stays harmlessly true.
      [1, false, true, 1, 0, null, false],
    ] as const;
    for (const [queuedCount, isMobile, expanded, visibleCount, hiddenCount, toggleLabel, collapsed] of cases) {
      expect(
        queuedStripLayout({ queuedCount, isMobile, expanded }),
        `${queuedCount}/${isMobile}/${expanded}`,
      ).toMatchObject({ visibleCount, hiddenCount, toggleLabel, collapsed });
    }
  });

  it("isQueuedPromptLong trips on a third line or past 160 chars", () => {
    const cases: [string, boolean][] = [
      ["fix the spinner", false],
      ["line 1\nline 2", false],
      ["line 1\nline 2\nline 3", true],
      ["x".repeat(161), true],
      ["x".repeat(160), false],
    ];
    for (const [text, expected] of cases) expect(isQueuedPromptLong(text), JSON.stringify(text)).toBe(expected);
  });
});
