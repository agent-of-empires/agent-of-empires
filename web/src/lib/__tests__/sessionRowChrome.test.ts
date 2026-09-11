import { describe, it, expect } from "vitest";
import { sessionRowChromeClass } from "../sessionRowChrome";

describe("sessionRowChromeClass", () => {
  it("frames the open session in the theme's active-session accent", () => {
    expect(sessionRowChromeClass(true, false)).toContain("ring-2 ring-inset ring-session-active");
  });

  it("keeps the active frame when the open session is also multi-selected", () => {
    // Both states want a ring; the active frame has to win deterministically
    // instead of leaving Tailwind to settle ring-1 vs ring-2 by source order.
    const chrome = sessionRowChromeClass(true, true);
    expect(chrome).toContain("ring-2 ring-inset ring-session-active");
    expect(chrome).not.toContain("ring-1");
  });

  it("gives multi-selection a distinct, thinner ring", () => {
    const chrome = sessionRowChromeClass(false, true);
    expect(chrome).toContain("ring-1");
    expect(chrome).not.toContain("ring-session-active");
  });

  it("withholds hover from the open row so it cannot repaint over the frame", () => {
    expect(sessionRowChromeClass(true, false)).not.toContain("hover:");
    expect(sessionRowChromeClass(true, true)).not.toContain("hover:");
  });

  it("keeps hover on every row that is not the open one", () => {
    expect(sessionRowChromeClass(false, false)).toContain("hover:bg-surface-700/40");
    expect(sessionRowChromeClass(false, true)).toContain("hover:bg-surface-700/40");
  });
});
