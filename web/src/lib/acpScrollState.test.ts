// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";

import { loadScrollState, saveScrollState } from "./acpScrollState";

afterEach(() => window.localStorage.clear());

describe("acpScrollState", () => {
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
