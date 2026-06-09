// State-machine tests for the structured view composer's iOS-dictation burst
// detector added in #1431. The pure helper lets the burst matrix be
// exercised without mounting the composer + the WebKit dictation
// engine; the imperative wiring (refs, timers, composerRuntime.setText)
// is in the component and exercised by the live Playwright spec.

import { describe, expect, it } from "vitest";

import { DICTATION_BURST_TIMEOUT_MS, decideDictationAction, type DictationBurstState } from "./Composer";

const inactive: DictationBurstState = { active: false };

describe("decideDictationAction (#1431)", () => {
  it("inactive + insertReplacementText -> enter burst, suppress upstream, arm timeout", () => {
    const d = decideDictationAction(inactive, {
      kind: "input",
      inputType: "insertReplacementText",
      nowMs: 1000,
    });
    expect(d.next).toEqual({ active: true, sinceMs: 1000 });
    expect(d.suppressUpstreamChange).toBe(true);
    expect(d.flushPending).toBe(false);
    expect(d.armTimeoutMs).toBe(DICTATION_BURST_TIMEOUT_MS);
  });

  it("active + insertReplacementText -> stay in burst, suppress upstream, re-arm timeout", () => {
    const prev: DictationBurstState = { active: true, sinceMs: 1000 };
    const d = decideDictationAction(prev, {
      kind: "input",
      inputType: "insertReplacementText",
      nowMs: 1300,
    });
    expect(d.next).toEqual({ active: true, sinceMs: 1300 });
    expect(d.suppressUpstreamChange).toBe(true);
    expect(d.flushPending).toBe(false);
    expect(d.armTimeoutMs).toBe(DICTATION_BURST_TIMEOUT_MS);
  });

  it("active + timeout -> exit burst, flush pending", () => {
    const prev: DictationBurstState = { active: true, sinceMs: 1000 };
    const d = decideDictationAction(prev, {
      kind: "timeout",
      nowMs: 2300,
    });
    expect(d.next).toEqual({ active: false });
    expect(d.suppressUpstreamChange).toBe(false);
    expect(d.flushPending).toBe(true);
    expect(d.armTimeoutMs).toBeNull();
  });

  it("active + blur -> exit burst, flush pending (Send tap fires blur first)", () => {
    const prev: DictationBurstState = { active: true, sinceMs: 1000 };
    const d = decideDictationAction(prev, { kind: "blur" });
    expect(d.next).toEqual({ active: false });
    expect(d.suppressUpstreamChange).toBe(false);
    expect(d.flushPending).toBe(true);
    expect(d.armTimeoutMs).toBeNull();
  });

  it("active + non-replacement input -> exit burst, flush, do NOT suppress (let event through)", () => {
    const prev: DictationBurstState = { active: true, sinceMs: 1000 };
    const d = decideDictationAction(prev, {
      kind: "input",
      inputType: "insertText",
      nowMs: 1100,
    });
    expect(d.next).toEqual({ active: false });
    expect(d.suppressUpstreamChange).toBe(false);
    expect(d.flushPending).toBe(true);
    expect(d.armTimeoutMs).toBeNull();
  });

  it("active + backspace -> exit burst, flush, do NOT suppress", () => {
    const prev: DictationBurstState = { active: true, sinceMs: 1000 };
    const d = decideDictationAction(prev, {
      kind: "input",
      inputType: "deleteContentBackward",
      nowMs: 1100,
    });
    expect(d.next).toEqual({ active: false });
    expect(d.flushPending).toBe(true);
    expect(d.suppressUpstreamChange).toBe(false);
  });

  it("inactive + regular insertText -> stay inactive, no-op", () => {
    const d = decideDictationAction(inactive, {
      kind: "input",
      inputType: "insertText",
      nowMs: 1000,
    });
    expect(d.next).toEqual({ active: false });
    expect(d.suppressUpstreamChange).toBe(false);
    expect(d.flushPending).toBe(false);
    expect(d.armTimeoutMs).toBeNull();
  });

  it("inactive + blur -> stay inactive, no flush", () => {
    const d = decideDictationAction(inactive, { kind: "blur" });
    expect(d.next).toEqual({ active: false });
    expect(d.flushPending).toBe(false);
  });

  it("inactive + timeout -> stay inactive, no flush (stale timer fired after blur)", () => {
    const d = decideDictationAction(inactive, {
      kind: "timeout",
      nowMs: 5000,
    });
    expect(d.next).toEqual({ active: false });
    expect(d.flushPending).toBe(false);
  });

  it("burst timeout is at least one second (must span typical breath pause)", () => {
    expect(DICTATION_BURST_TIMEOUT_MS).toBeGreaterThanOrEqual(1000);
  });
});
