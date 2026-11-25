import { describe, expect, it } from "vitest";
import { segStyle } from "./segStyle";

const FG = "var(--term-fg, #e4e4e7)";
const BG = "var(--term-bg, #1c1c1f)";

describe("segStyle", () => {
  it.each([
    { name: "default colours", style: { dim: true }, color: `color-mix(in srgb, ${FG} 60%, ${BG})`, bg: undefined },
    {
      name: "an explicit fg over a cell background",
      style: { dim: true, fg: "#ff0000", bg: "#303030" },
      color: "color-mix(in srgb, #ff0000 60%, #303030)",
      bg: "#303030",
    },
    {
      name: "inverse, toward the swapped background",
      style: { dim: true, inverse: true },
      color: `color-mix(in srgb, ${BG} 60%, ${FG})`,
      bg: FG,
    },
  ])("dims $name by blending the glyph colour, leaving the background opaque", ({ style, color, bg }) => {
    const css = segStyle(style);
    expect(css?.color).toBe(color);
    expect(css?.backgroundColor).toBe(bg);
    expect(css?.opacity).toBeUndefined();
  });
});
