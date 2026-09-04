// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";

import { loadScrollState, restoredScrollTop, saveScrollState } from "./acpScrollState";

afterEach(() => window.localStorage.clear());

describe("acpScrollState", () => {
  it("restores pinned intent without overriding a later user scroll", () => {
    expect(restoredScrollTop(null, true, 900, 300)).toBe(900);
    expect(restoredScrollTop({ stuck: true, top: 0 }, false, 900, 300)).toBeNull();
  });

  it("clamps a saved reader position to the current viewport", () => {
    expect(restoredScrollTop({ stuck: false, top: 700 }, false, 900, 300)).toBe(600);
  });

  it("round-trips a saved state per session", () => {
    saveScrollState("s1", { stuck: false, top: 420 });
    expect(loadScrollState("s1")).toEqual({ stuck: false, top: 420 });
    // Distinct sessions do not bleed into each other.
    expect(loadScrollState("s2")).toBeNull();
  });

  it("returns null for missing, malformed, or wrong-typed entries", () => {
    expect(loadScrollState("missing")).toBeNull();
    window.localStorage.setItem("aoe:acp-scroll:v1:bad", "{not json");
    expect(loadScrollState("bad")).toBeNull();
    // Wrong types must not crash a restore.
    window.localStorage.setItem("aoe:acp-scroll:v1:typed", JSON.stringify({ stuck: "yes", top: "x" }));
    expect(loadScrollState("typed")).toBeNull();
  });
});
