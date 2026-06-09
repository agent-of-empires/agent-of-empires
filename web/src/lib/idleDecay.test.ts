import { describe, expect, it } from "vitest";

import { parseIdleDecayWindowMs } from "./idleDecay";

describe("parseIdleDecayWindowMs", () => {
  it("converts configured minutes to milliseconds", () => {
    expect(parseIdleDecayWindowMs({ theme: { idle_decay_minutes: 5 } })).toBe(5 * 60 * 1000);
  });

  it("falls back to the default when the value is missing", () => {
    expect(parseIdleDecayWindowMs({ theme: {} })).toBe(0);
    expect(parseIdleDecayWindowMs(null)).toBe(0);
  });

  it("treats zero, negative, and non-numeric values as disabled", () => {
    expect(parseIdleDecayWindowMs({ theme: { idle_decay_minutes: 0 } })).toBe(0);
    expect(parseIdleDecayWindowMs({ theme: { idle_decay_minutes: -1 } })).toBe(0);
    expect(parseIdleDecayWindowMs({ theme: { idle_decay_minutes: "5" } as never })).toBe(0);
  });
});
