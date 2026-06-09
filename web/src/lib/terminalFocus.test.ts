import { describe, it, expect, beforeEach } from "vitest";
import { consumePendingTerminalFocus, setPendingTerminalFocus } from "./terminalFocus";

describe("terminalFocus pending intent", () => {
  beforeEach(() => {
    // Drain any leftover pending intent between tests.
    consumePendingTerminalFocus("agent");
    consumePendingTerminalFocus("paired");
  });

  it("consumePendingTerminalFocus only fires once per set", () => {
    setPendingTerminalFocus("paired");
    expect(consumePendingTerminalFocus("paired")).toBe(true);
    expect(consumePendingTerminalFocus("paired")).toBe(false);
  });

  it("consumePendingTerminalFocus does not match a different target", () => {
    setPendingTerminalFocus("paired");
    expect(consumePendingTerminalFocus("agent")).toBe(false);
    // The pending intent is still there for the right target.
    expect(consumePendingTerminalFocus("paired")).toBe(true);
  });

  it("setting a new target overwrites the previous pending intent", () => {
    setPendingTerminalFocus("paired");
    setPendingTerminalFocus("agent");
    expect(consumePendingTerminalFocus("paired")).toBe(false);
    expect(consumePendingTerminalFocus("agent")).toBe(true);
  });
});
