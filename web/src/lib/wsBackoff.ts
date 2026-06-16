// Reconnect backoff schedule shared by the live-view WebSocket transports.
//
// Tightened from the old exponential curve (1s, 2s, 4s, 8s, 16s, 30s, 30s;
// worst case ~91s) to a fast-start array (200ms, 400ms, 800ms, 1.5s, 3s, 6s,
// 10s; worst case ~22s) to absorb tmux warm-up during first-session-open
// without keeping the client asleep on a 30s timer once the server is finally
// ready. See #1455.
const RETRY_DELAYS_MS = [200, 400, 800, 1500, 3000, 6000, 10000] as const;

export const MAX_RETRIES = RETRY_DELAYS_MS.length;

export const retryDelayMs = (attempt: number): number => {
  const idx = Math.max(1, Math.min(RETRY_DELAYS_MS.length, attempt)) - 1;
  return RETRY_DELAYS_MS[idx]!;
};
