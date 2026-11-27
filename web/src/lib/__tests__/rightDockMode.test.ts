import { describe, expect, it } from "vitest";

import { rightDockMode, SHEET_MEDIA_QUERY } from "../rightDockMode";

describe("rightDockMode", () => {
  it.each([
    [390, "picker"],
    [767, "picker"],
    [768, "sheet"],
    [1000, "sheet"],
    [1099, "sheet"],
    [1100, "inline"],
    [1440, "inline"],
  ])("%ipx shows the right dock as a %s", (width, mode) => {
    expect(rightDockMode(width)).toBe(mode);
  });

  it("matches the sheet range in CSS pixels", () => {
    expect(SHEET_MEDIA_QUERY).toBe("(768px <= width < 1100px)");
  });
});
