// @vitest-environment jsdom
//
// Unit tests for requestSessionInputFocus (#1454): the pure dispatch +
// pending-latch helper the sidebar select handlers delegate to. Covers the
// coarse-pointer suppression and the structured view / non-structured view target choice,
// asserting both the dispatched event and the stashed latch.

import { afterEach, describe, expect, it, vi } from "vitest";
import {
  FOCUS_TERMINAL_EVENT,
  consumePendingTerminalFocus,
  requestSessionInputFocus,
  type FocusTerminalDetail,
} from "../terminalFocus";

function captureDispatch(): () => FocusTerminalDetail[] {
  const seen: FocusTerminalDetail[] = [];
  const handler = (e: Event) => {
    seen.push((e as CustomEvent<FocusTerminalDetail>).detail);
  };
  window.addEventListener(FOCUS_TERMINAL_EVENT, handler);
  return () => {
    window.removeEventListener(FOCUS_TERMINAL_EVENT, handler);
    return seen;
  };
}

afterEach(() => {
  // Drain any latch left over so tests stay independent.
  consumePendingTerminalFocus("agent");
  consumePendingTerminalFocus("composer");
  consumePendingTerminalFocus("paired");
  vi.restoreAllMocks();
});

describe("requestSessionInputFocus", () => {
  it("does nothing on a coarse pointer", () => {
    const done = captureDispatch();
    requestSessionInputFocus({ view: "structured" }, true);
    requestSessionInputFocus({ view: "terminal" }, true);
    const events = done();
    expect(events).toHaveLength(0);
    expect(consumePendingTerminalFocus("composer")).toBe(false);
    expect(consumePendingTerminalFocus("agent")).toBe(false);
  });

  it("does nothing when there is no session", () => {
    const done = captureDispatch();
    requestSessionInputFocus(undefined, false);
    expect(done()).toHaveLength(0);
    expect(consumePendingTerminalFocus("agent")).toBe(false);
  });

  it("targets the composer for structured view sessions on a fine pointer", () => {
    const done = captureDispatch();
    requestSessionInputFocus({ view: "structured" }, false);
    const events = done();
    expect(events).toEqual([{ target: "composer" }]);
    // Latch was set for the not-yet-mounted case.
    expect(consumePendingTerminalFocus("composer")).toBe(true);
    expect(consumePendingTerminalFocus("composer")).toBe(false);
  });

  it("targets the agent terminal for non-structured view sessions on a fine pointer", () => {
    const done = captureDispatch();
    requestSessionInputFocus({ view: "terminal" }, false);
    const events = done();
    expect(events).toEqual([{ target: "agent" }]);
    expect(consumePendingTerminalFocus("agent")).toBe(true);
  });
});
