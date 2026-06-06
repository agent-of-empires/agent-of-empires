// Browser-side error capture + batched relay to /api/client-log.
//
// Captures window.onerror, unhandledrejection, React ErrorBoundary
// (via reportError), and explicit reportError() calls.
//
// Throttling: token-bucket (10 cap, 10/s refill) on entries. Batches
// flush every 2s, on size threshold, on visibilitychange=hidden, and
// on pagehide. Unload-time flush uses navigator.sendBeacon with a JSON
// Blob since sendBeacon can't carry the Authorization header; the
// cookie-mode auth still works in that path.
//
// URL sanitization: never log `window.location.href` raw, because we
// embed the auth token in `?token=` and don't want it on disk.

export type ClientLogLevel = "error" | "warn" | "info" | "debug";

export interface ClientLogEntry {
  level: ClientLogLevel;
  message: string;
  stack?: string;
  componentStack?: string;
  target?: string;
  sessionId?: string;
  path: string;
  userAgent: string;
  ts: number;
  dropped?: number;
}

const ENDPOINT = "/api/client-log";
const RATE_CAP = 10;
const RATE_REFILL_PER_SEC = 10;
const FLUSH_INTERVAL_MS = 2000;
const MAX_BATCH = 20;
const MAX_BATCH_BYTES = 48 * 1024;

let installed = false;
let queue: ClientLogEntry[] = [];
let dropped = 0;
let tokens = RATE_CAP;
let lastRefill = Date.now();
let isReporting = false;

function sanitizedPath(): string {
  try {
    const u = new URL(window.location.href);
    u.searchParams.delete("token");
    return `${u.pathname}${u.search}${u.hash}`;
  } catch {
    return "/";
  }
}

function refillTokens(): void {
  const now = Date.now();
  const delta = (now - lastRefill) / 1000;
  if (delta <= 0) return;
  tokens = Math.min(RATE_CAP, tokens + delta * RATE_REFILL_PER_SEC);
  lastRefill = now;
}

function tryConsumeToken(): boolean {
  refillTokens();
  if (tokens >= 1) {
    tokens -= 1;
    return true;
  }
  return false;
}

function normalizeError(err: unknown): { message: string; stack?: string } {
  if (err instanceof Error) {
    return { message: err.message || String(err), stack: err.stack };
  }
  if (typeof err === "string") return { message: err };
  try {
    return { message: JSON.stringify(err) };
  } catch {
    return { message: String(err) };
  }
}

function enqueue(entry: ClientLogEntry): void {
  if (isReporting) return;
  if (!tryConsumeToken()) {
    dropped += 1;
    return;
  }
  queue.push(entry);
  if (queue.length >= MAX_BATCH) {
    void flush(false);
  }
}

async function flush(viaBeacon: boolean): Promise<void> {
  if (queue.length === 0 && dropped === 0) return;
  if (isReporting) return;
  isReporting = true;
  try {
    const batch = queue;
    queue = [];
    if (dropped > 0) {
      batch.push({
        level: "warn",
        message: `log relay dropped ${dropped} entries (rate-limited)`,
        target: "logger.relay",
        path: sanitizedPath(),
        userAgent: navigator.userAgent,
        ts: Date.now(),
        dropped,
      });
      dropped = 0;
    }
    // Trim oversized payloads to fit the keepalive budget.
    const body = trimToBudget(batch);
    const json = JSON.stringify({ entries: body });

    if (viaBeacon && typeof navigator.sendBeacon === "function") {
      const blob = new Blob([json], { type: "application/json" });
      navigator.sendBeacon(ENDPOINT, blob);
      return;
    }
    await fetch(ENDPOINT, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: json,
      keepalive: true,
      credentials: "include",
    });
  } catch {
    // Drop the batch on failure; don't recurse into the logger.
  } finally {
    isReporting = false;
  }
}

function trimToBudget(batch: ClientLogEntry[]): ClientLogEntry[] {
  let totalBytes = 0;
  const out: ClientLogEntry[] = [];
  for (const entry of batch) {
    const size = JSON.stringify(entry).length;
    if (out.length >= MAX_BATCH || totalBytes + size > MAX_BATCH_BYTES) {
      dropped += batch.length - out.length;
      break;
    }
    totalBytes += size;
    out.push(entry);
  }
  return out;
}

export function reportError(err: unknown, ctx?: Partial<ClientLogEntry>): void {
  const { message, stack } = normalizeError(err);
  enqueue({
    level: ctx?.level ?? "error",
    message,
    stack: ctx?.stack ?? stack,
    componentStack: ctx?.componentStack,
    target: ctx?.target,
    sessionId: ctx?.sessionId,
    path: sanitizedPath(),
    userAgent: navigator.userAgent,
    ts: Date.now(),
  });
}

export function installClientLogger(): void {
  if (installed) return;
  installed = true;

  window.addEventListener("error", (e) => {
    reportError(e.error ?? e.message, { target: "window.onerror" });
  });

  window.addEventListener("unhandledrejection", (e) => {
    reportError(e.reason, { target: "window.unhandledrejection" });
  });

  setInterval(() => void flush(false), FLUSH_INTERVAL_MS);

  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden") {
      void flush(true);
    }
  });

  window.addEventListener("pagehide", () => {
    void flush(true);
  });
}
