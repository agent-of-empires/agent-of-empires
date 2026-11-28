import { describe, it, expect } from "vitest";
import { sessionRowChromeClass } from "../sessionRowChrome";

describe("sessionRowChromeClass", () => {
  it("fills the open session's row and brightens its text, with no edge bar", () => {
    const chrome = sessionRowChromeClass(true, false);
    expect([chrome.includes("bg-surface-700/70 text-text-primary"), chrome.includes("shadow")]).toEqual([true, false]);
  });

  it("keeps the open row's fill when it is also multi-selected, adding the selection ring", () => {
    const chrome = sessionRowChromeClass(true, true);
    expect([chrome.includes("bg-surface-700/70"), chrome.includes("ring-1")]).toEqual([true, true]);
  });

  it("gives multi-selection a thin ring and a tint", () => {
    expect(sessionRowChromeClass(false, true)).toContain("ring-1 ring-inset ring-brand-500/60 bg-brand-500/10");
  });

  it("withholds hover from the open row and keeps it on every other row", () => {
    expect(
      [sessionRowChromeClass(true, false), sessionRowChromeClass(true, true)].some((c) => c.includes("hover:")),
    ).toBe(false);
    expect(sessionRowChromeClass(false, false)).toContain("hover:bg-surface-800/60");
    expect(sessionRowChromeClass(false, true)).toContain("hover:bg-surface-800/60");
  });
});
