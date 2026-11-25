import { describe, expect, it } from "vitest";

import { bindUnboundTerminalTabId, emptyPanelPane, isTerminalTabId, parseTerminalTabId, terminalTabId } from "../panes";

describe("terminal tab ids", () => {
  it("round-trips a Session's shell index", () => {
    expect(terminalTabId("ab12cd34ef", 3)).toBe("terminal:ab12cd34ef:3");
    expect(parseTerminalTabId("terminal:ab12cd34ef:3")).toEqual({ sessionId: "ab12cd34ef", index: 3 });
  });

  it("rejects malformed ids strictly (no aliasing a real pane index)", () => {
    for (const bad of [
      "terminal:",
      "terminal:0",
      "terminal:s:",
      "terminal:s:1junk",
      "terminal:s:-0",
      "terminal:s: 1",
      "terminal::1",
      "diff",
      "plugin:a:b",
    ]) {
      expect(isTerminalTabId(bad)).toBe(false);
      expect(parseTerminalTabId(bad)).toBeNull();
    }
  });

  it("binds only an unbound terminal id", () => {
    expect(bindUnboundTerminalTabId("terminal:2", "s1")).toBe("terminal:s1:2");
    for (const kept of ["terminal:s0:1", "diff", "plugin:p:a", "terminal:x"]) {
      expect(bindUnboundTerminalTabId(kept, "s1")).toBe(kept);
    }
  });
});

describe("emptyPanelPane", () => {
  it("opens Diff when available, else the first pane, else nothing", () => {
    expect([emptyPanelPane(["files", "diff"]), emptyPanelPane(["agents", "plugin:p:a"]), emptyPanelPane([])]).toEqual([
      "diff",
      "agents",
      undefined,
    ]);
  });
});
