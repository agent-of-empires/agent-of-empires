import { describe, expect, it } from "vitest";
import { rowHeight } from "./rowHeight";

describe("rowHeight", () => {
  it.each([
    [14, 17],
    [13, 16],
    [8, 10],
    [6, 7],
    [28, 34],
    // A pinch drives fractional font sizes.
    [14.3, 17],
    [0.2, 1],
  ])("rounds %s px text to a %s px row", (fontSize, want) => {
    expect(rowHeight(fontSize)).toBe(want);
  });

  it("applies a font's own line ratio", () => {
    expect(rowHeight(14, 1.32)).toBe(18);
    expect(rowHeight(13, 1.32)).toBe(17);
  });
});
