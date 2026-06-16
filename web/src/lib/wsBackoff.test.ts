// @vitest-environment jsdom
//
// Unit tests for retryDelayMs. The function drives the WS reconnect
// backoff schedule. A regression here would either hammer a dead server
// or stretch the first retry past the user-perceptible threshold.
//
// Schedule was tightened from the old exponential curve (1s, 2s, 4s, 8s,
// 16s, 30s, 30s; worst case ~91s) to a fast-start array (200ms, 400ms,
// 800ms, 1.5s, 3s, 6s, 10s; worst case ~22s) to absorb tmux warm-up
// during first-session-open without keeping the client asleep on a 30s
// timer once the server is finally ready. See #1455.

import { describe, expect, it } from "vitest";
import { retryDelayMs } from "./wsBackoff";

describe("retryDelayMs", () => {
  it("uses the fast-start schedule for the first attempts", () => {
    expect(retryDelayMs(1)).toBe(200);
    expect(retryDelayMs(2)).toBe(400);
    expect(retryDelayMs(3)).toBe(800);
    expect(retryDelayMs(4)).toBe(1500);
    expect(retryDelayMs(5)).toBe(3000);
  });

  it("caps at 10s for the tail of the backoff", () => {
    expect(retryDelayMs(6)).toBe(6000);
    expect(retryDelayMs(7)).toBe(10000);
    // Defense against an off-by-one: even an out-of-range attempt
    // never exceeds the tail value, so the retry handler can't
    // accidentally schedule a 30s+ timeout if MAX_RETRIES creeps up.
    expect(retryDelayMs(20)).toBe(10000);
  });

  it("clamps non-positive attempts to the first delay", () => {
    // Defensive: the call site always passes attempt >= 1, but a future
    // change to retry-state machine arithmetic shouldn't drop a 0 into
    // retryDelayMs and produce undefined.
    expect(retryDelayMs(0)).toBe(200);
    expect(retryDelayMs(-1)).toBe(200);
  });
});
