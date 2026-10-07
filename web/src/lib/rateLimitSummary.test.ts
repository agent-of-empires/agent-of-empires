import { describe, expect, it } from "vitest";

import { summarizeRateLimits } from "./rateLimitSummary";

const limited = (resets_at: string | null) => ({
  rate_limit: { status: "limited", resets_at, kind: "usage" },
});

describe("summarizeRateLimits (#3514)", () => {
  it("is null when no session is parked, whatever a browser mirror once held", () => {
    expect(summarizeRateLimits([])).toBeNull();
    expect(summarizeRateLimits([{ rate_limit: null }, {}])).toBeNull();
  });

  it("counts parked sessions and picks the soonest reported reset", () => {
    expect(
      summarizeRateLimits([
        limited("2026-06-01T12:00:00Z"),
        { rate_limit: null },
        limited("2026-06-01T09:00:00Z"),
        limited(null),
      ]),
    ).toEqual({ count: 3, resetsAt: "2026-06-01T09:00:00Z" });
  });

  it("reports a park with no reset time as rate-limited without a time", () => {
    expect(summarizeRateLimits([limited(null)])).toEqual({ count: 1, resetsAt: null });
  });
});
