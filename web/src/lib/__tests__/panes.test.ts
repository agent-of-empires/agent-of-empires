import { describe, expect, it } from "vitest";

import { isTerminalTabId, terminalIndexOf, terminalTabId } from "../panes";

describe("terminal tab ids", () => {
  it("round-trips a valid index", () => {
    expect(terminalTabId(0)).toBe("terminal:0");
    expect(isTerminalTabId("terminal:0")).toBe(true);
    expect(terminalIndexOf("terminal:3")).toBe(3);
  });

  it("rejects malformed ids strictly (no aliasing a real pane index)", () => {
    for (const bad of ["terminal:", "terminal:1junk", "terminal:-0", "terminal: 1", "diff", "plugin:a:b"]) {
      expect(isTerminalTabId(bad)).toBe(false);
      expect(terminalIndexOf(bad)).toBe(0);
    }
  });
});
