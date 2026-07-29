import { describe, expect, it } from "vitest";

import { parseSessionColorsEnabled } from "../sessionColors";

// The parser encodes a compatibility contract: only a literal `false` turns
// session colors off, so a daemon predating `session.show_session_colors`
// keeps rendering them. A careless rewrite into a truthiness check would
// silently disable colors for every older server, hence the explicit cases.
describe("parseSessionColorsEnabled", () => {
  it("defaults to enabled when there are no settings at all", () => {
    expect(parseSessionColorsEnabled(undefined)).toBe(true);
    expect(parseSessionColorsEnabled(null)).toBe(true);
  });

  it("defaults to enabled when `session` is missing or not an object", () => {
    expect(parseSessionColorsEnabled({})).toBe(true);
    expect(parseSessionColorsEnabled({ session: "nope" })).toBe(true);
    expect(parseSessionColorsEnabled({ session: null })).toBe(true);
  });

  it("defaults to enabled when the field is absent or malformed", () => {
    expect(parseSessionColorsEnabled({ session: { row_tag: "branch" } })).toBe(true);
    expect(parseSessionColorsEnabled({ session: { show_session_colors: "false" } })).toBe(true);
    expect(parseSessionColorsEnabled({ session: { show_session_colors: 0 } })).toBe(true);
  });

  it("disables only on an explicit boolean false", () => {
    expect(parseSessionColorsEnabled({ session: { show_session_colors: false } })).toBe(false);
    expect(parseSessionColorsEnabled({ session: { show_session_colors: true } })).toBe(true);
  });
});
