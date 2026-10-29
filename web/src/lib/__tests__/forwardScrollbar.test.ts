import { describe, expect, it } from "vitest";
import { forwardPageNotches, forwardScrollRate } from "../forwardScrollbar";

describe("forwardScrollRate", () => {
  it.each([
    ["centre", 0, 40, 0],
    ["inside the dead zone", 4, 40, 0],
    ["invalid travel", 10, 0, 0],
    ["non-finite offset", Number.NaN, 40, 0],
  ])("%s scrolls nothing", (_name, offset, travel, expected) => {
    expect(forwardScrollRate(offset, travel)).toBe(expected);
  });

  it("is signed by direction and symmetric", () => {
    const down = forwardScrollRate(20, 40);
    expect(down).toBeGreaterThan(0);
    expect(forwardScrollRate(-20, 40)).toBe(-down);
  });

  it("grows with distance and clamps at the maximum", () => {
    expect(forwardScrollRate(30, 40)).toBeGreaterThan(forwardScrollRate(15, 40));
    expect(forwardScrollRate(400, 40)).toBe(forwardScrollRate(40, 40));
    expect(forwardScrollRate(40, 40)).toBe(24);
  });
});

describe("forwardPageNotches", () => {
  it.each([
    [40, 39],
    [2, 1],
    [1, 1],
    [0, 1],
  ])("%i rows is %i notches", (rows, expected) => {
    expect(forwardPageNotches(rows)).toBe(expected);
  });
});
