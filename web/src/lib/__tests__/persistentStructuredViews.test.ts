import { describe, expect, it } from "vitest";
import { normalizePersistentStructuredViewLimit } from "../persistentStructuredViews";

describe("normalizePersistentStructuredViewLimit", () => {
  it.each([
    ["3", 3],
    [null, 3],
    [undefined, 3],
    [Number.NaN, 3],
    [Number.POSITIVE_INFINITY, 3],
    [0, 1],
    [99, 5],
    [2.6, 3],
  ])("maps %j to %i", (value, expected) => {
    expect(normalizePersistentStructuredViewLimit(value)).toBe(expected);
  });
});
