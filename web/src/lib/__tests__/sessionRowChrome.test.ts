import { describe, it, expect } from "vitest";
import { sessionRowChromeClass } from "../sessionRowChrome";

const BAR = "shadow-[inset_2px_0_0_0_var(--color-session-active)]";

describe("sessionRowChromeClass", () => {
  it("fills the open session's row and edges it in the theme's active-session accent", () => {
    expect(sessionRowChromeClass(true, false)).toContain(`bg-surface-800 text-text-primary ${BAR}`);
  });

  it("keeps the open row's fill and bar when it is also multi-selected, adding the selection ring", () => {
    const chrome = sessionRowChromeClass(true, true);
    expect([chrome.includes(BAR), chrome.includes("ring-1")]).toEqual([true, true]);
  });

  it("gives multi-selection a thin ring and no active bar", () => {
    const chrome = sessionRowChromeClass(false, true);
    expect([chrome.includes("ring-1"), chrome.includes("session-active")]).toEqual([true, false]);
  });

  it("withholds hover from the open row and keeps it on every other row", () => {
    expect(
      [sessionRowChromeClass(true, false), sessionRowChromeClass(true, true)].some((c) => c.includes("hover:")),
    ).toBe(false);
    expect(sessionRowChromeClass(false, false)).toContain("hover:bg-surface-800/60");
    expect(sessionRowChromeClass(false, true)).toContain("hover:bg-surface-800/60");
  });
});
