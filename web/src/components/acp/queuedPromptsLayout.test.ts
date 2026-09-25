import { describe, expect, it } from "vitest";

import { isQueuedPromptLong, queuedStripLayout } from "./queuedPromptsLayout";

describe("queuedStripLayout", () => {
  it.each([
    // count, mobile, expanded -> visible, hidden, label, collapsed
    [1, false, false, 1, 0, null, false],
    [2, false, false, 2, 0, null, false],
    [5, false, false, 2, 3, "Show 3 more", true],
    [5, false, true, 5, 0, "Show less", false],
    [1, true, false, 1, 0, null, false],
    [4, true, false, 1, 3, "Show 3 more", true],
    // A drained queue drops the toggle while `expanded` stays harmlessly true.
    [1, false, true, 1, 0, null, false],
  ])(
    "count=%i mobile=%s expanded=%s",
    (queuedCount, isMobile, expanded, visibleCount, hiddenCount, toggleLabel, collapsed) => {
      expect(queuedStripLayout({ queuedCount, isMobile, expanded })).toMatchObject({
        visibleCount,
        hiddenCount,
        toggleLabel,
        collapsed,
      });
    },
  );
});

describe("isQueuedPromptLong", () => {
  it.each([
    ["fix the spinner", false],
    ["line 1\nline 2", false],
    ["line 1\nline 2\nline 3", true],
    ["x".repeat(161), true],
    ["x".repeat(160), false],
  ])("%j -> %s", (text, expected) => {
    expect(isQueuedPromptLong(text)).toBe(expected);
  });
});
