// Layout tests for the structured view's QueuedPromptsStrip. The pure
// helpers in `queuedPromptsLayout.ts` let us check the
// visible-count / toggle-label decisions across mobile / desktop /
// expanded / collapsed states without mounting the strip + assistant-ui
// runtime. See #1232.

import { describe, expect, it } from "vitest";

import { isQueuedPromptLong, queuedStripLayout } from "./queuedPromptsLayout";

describe("queuedStripLayout (#1232)", () => {
  describe("desktop default N=2", () => {
    it("renders everything when queue is 1 row", () => {
      const out = queuedStripLayout({
        queuedCount: 1,
        isMobile: false,
        expanded: false,
      });
      expect(out.visibleCount).toBe(1);
      expect(out.hiddenCount).toBe(0);
      expect(out.toggleLabel).toBeNull();
      expect(out.collapsed).toBe(false);
    });

    it("renders everything at threshold (2 rows)", () => {
      const out = queuedStripLayout({
        queuedCount: 2,
        isMobile: false,
        expanded: false,
      });
      expect(out.visibleCount).toBe(2);
      expect(out.toggleLabel).toBeNull();
    });

    it("collapses to 2 + 'Show N more' once queue exceeds threshold", () => {
      const out = queuedStripLayout({
        queuedCount: 5,
        isMobile: false,
        expanded: false,
      });
      expect(out.visibleCount).toBe(2);
      expect(out.hiddenCount).toBe(3);
      expect(out.toggleLabel).toBe("Show 3 more");
      expect(out.collapsed).toBe(true);
    });

    it("renders all rows when expanded, with 'Show less' label", () => {
      const out = queuedStripLayout({
        queuedCount: 5,
        isMobile: false,
        expanded: true,
      });
      expect(out.visibleCount).toBe(5);
      expect(out.hiddenCount).toBe(0);
      expect(out.toggleLabel).toBe("Show less");
      expect(out.collapsed).toBe(false);
    });
  });

  describe("mobile default N=1", () => {
    it("renders everything when queue is 1 row", () => {
      const out = queuedStripLayout({
        queuedCount: 1,
        isMobile: true,
        expanded: false,
      });
      expect(out.visibleCount).toBe(1);
      expect(out.toggleLabel).toBeNull();
    });

    it("collapses to 1 + 'Show N more' at 2+ rows", () => {
      const out = queuedStripLayout({
        queuedCount: 4,
        isMobile: true,
        expanded: false,
      });
      expect(out.visibleCount).toBe(1);
      expect(out.hiddenCount).toBe(3);
      expect(out.toggleLabel).toBe("Show 3 more");
    });
  });

  it("returns no toggle when queue drains below threshold even with expanded=true", () => {
    // Edge case: user expanded, then queue drained. Toggle disappears
    // entirely; `expanded` stays harmlessly true so a future overflow
    // doesn't surprise-collapse the rows the user just expanded.
    const out = queuedStripLayout({
      queuedCount: 1,
      isMobile: false,
      expanded: true,
    });
    expect(out.visibleCount).toBe(1);
    expect(out.toggleLabel).toBeNull();
    expect(out.collapsed).toBe(false);
  });
});

describe("isQueuedPromptLong (#1232)", () => {
  it("returns false for short single-line prompts", () => {
    expect(isQueuedPromptLong("fix the spinner")).toBe(false);
  });

  it("returns false for short two-line prompts", () => {
    expect(isQueuedPromptLong("line 1\nline 2")).toBe(false);
  });

  it("returns true for multi-line prompts with 3+ lines", () => {
    expect(isQueuedPromptLong("line 1\nline 2\nline 3")).toBe(true);
  });

  it("returns true for a single very-long line over 160 chars", () => {
    expect(isQueuedPromptLong("x".repeat(161))).toBe(true);
  });

  it("returns false at exactly 160 chars", () => {
    expect(isQueuedPromptLong("x".repeat(160))).toBe(false);
  });
});
