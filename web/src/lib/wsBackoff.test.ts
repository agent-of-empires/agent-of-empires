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

import { expect, it } from "vitest";
import { retryDelayMs } from "./wsBackoff";

it("retryDelayMs starts fast, caps at 10s, and clamps non-positive attempts", () => {
  const schedule: [number, number][] = [
    [-1, 200],
    [0, 200],
    [1, 200],
    [2, 400],
    [3, 800],
    [4, 1500],
    [5, 3000],
    [6, 6000],
    [7, 10000],
    [20, 10000],
  ];
  for (const [attempt, delay] of schedule) expect(retryDelayMs(attempt), `attempt ${attempt}`).toBe(delay);
});
