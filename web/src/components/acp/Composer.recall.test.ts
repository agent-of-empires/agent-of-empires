// Decision-matrix tests for the composer's ArrowUp/ArrowDown queue
// recall (#2147). The pure helper lets us exercise the hijack guard and
// browse routing without mounting the composer + assistant-ui runtime.

import { describe, expect, it } from "vitest";

import { decideArrowRecall } from "./Composer";

const up = {
  key: "ArrowUp",
  shiftKey: false,
  ctrlKey: false,
  metaKey: false,
  altKey: false,
  isComposing: false,
};
const down = { ...up, key: "ArrowDown" };

describe("decideArrowRecall (#2147)", () => {
  it("ArrowUp at the caret origin with a queue enters recall", () => {
    expect(decideArrowRecall(up, { caretAtStart: true, browsing: false, queueLen: 2 })).toBe("older");
  });

  it("ArrowUp not at the origin moves the caret, never recalls", () => {
    expect(decideArrowRecall(up, { caretAtStart: false, browsing: false, queueLen: 2 })).toBe("default");
  });

  it("ArrowUp with an empty queue does not recall", () => {
    expect(decideArrowRecall(up, { caretAtStart: true, browsing: false, queueLen: 0 })).toBe("default");
  });

  it("while browsing, both arrows own navigation regardless of caret", () => {
    expect(decideArrowRecall(up, { caretAtStart: false, browsing: true, queueLen: 2 })).toBe("older");
    expect(decideArrowRecall(down, { caretAtStart: false, browsing: true, queueLen: 2 })).toBe("newer");
  });

  it("ArrowDown does not recall when not browsing", () => {
    expect(decideArrowRecall(down, { caretAtStart: true, browsing: false, queueLen: 2 })).toBe("default");
  });

  it("modifiers and IME composition pass through to the caret", () => {
    for (const mod of [
      { shiftKey: true },
      { ctrlKey: true },
      { metaKey: true },
      { altKey: true },
      { isComposing: true },
    ]) {
      expect(decideArrowRecall({ ...up, ...mod }, { caretAtStart: true, browsing: true, queueLen: 2 })).toBe("default");
    }
  });

  it("non-arrow keys pass through", () => {
    expect(decideArrowRecall({ ...up, key: "a" }, { caretAtStart: true, browsing: true, queueLen: 2 })).toBe("default");
  });
});
