import { describe, it, expect, beforeEach } from "vitest";
import { consumePendingTerminalFocus, setPendingTerminalFocus } from "./terminalFocus";

describe("terminalFocus pending intent", () => {
  beforeEach(() => {
    consumePendingTerminalFocus("agent");
    consumePendingTerminalFocus("paired");
  });

  it("consumePendingTerminalFocus does not match a different target", () => {
    setPendingTerminalFocus("paired");
    expect(consumePendingTerminalFocus("agent")).toBe(false);
    expect(consumePendingTerminalFocus("paired")).toBe(true);
  });

  it("setting a new target overwrites the previous pending intent", () => {
    setPendingTerminalFocus("paired");
    setPendingTerminalFocus("agent");
    expect(consumePendingTerminalFocus("paired")).toBe(false);
    expect(consumePendingTerminalFocus("agent")).toBe(true);
  });
});
