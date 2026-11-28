import type { SessionResponse, SessionStatus } from "./types";

/** Freshness window for Stop-hooked Idle sessions; 0 (off) mirrors the Rust `theme.idle_decay_minutes` default. */
export const IDLE_DECAY_WINDOW_MS = 0;

export const STATUS_DOT_CLASS: Record<SessionStatus, string> = {
  Running: "bg-status-running",
  Waiting: "bg-status-waiting",
  Idle: "bg-status-idle",
  Error: "bg-status-error",
  Starting: "bg-status-starting",
  Stopped: "bg-status-stopped",
  Unknown: "bg-status-idle",
  Deleting: "bg-status-error",
  Creating: "bg-status-starting",
};

export const STATUS_TEXT_CLASS: Record<SessionStatus, string> = {
  Running: "text-status-running",
  Waiting: "text-status-waiting",
  Idle: "text-status-idle",
  Error: "text-status-error",
  Starting: "text-status-starting",
  Stopped: "text-status-stopped",
  Unknown: "text-status-idle",
  Deleting: "text-status-error",
  Creating: "text-status-starting",
};

/** Null unless Idle with a non-future `idle_entered_at`. */
export function idleAgeMs(session: Pick<SessionResponse, "status" | "idle_entered_at">): number | null {
  if (session.status !== "Idle") return null;
  if (!session.idle_entered_at) return null;
  const since = Date.parse(session.idle_entered_at);
  if (Number.isNaN(since)) return null;
  const age = Date.now() - since;
  return age >= 0 ? age : null;
}

/** Idle within `windowMs` of the Stop hook, which counts as needing attention. */
export function isFreshIdle(
  session: Pick<SessionResponse, "status" | "idle_entered_at">,
  windowMs: number = IDLE_DECAY_WINDOW_MS,
): boolean {
  if (windowMs <= 0) return false;
  const age = idleAgeMs(session);
  return age !== null && age < windowMs;
}

/** Idle picks a fresh or decayed tier; static classes keep Tailwind's JIT happy. */
export function getStatusDotClass(
  session: Pick<SessionResponse, "status" | "idle_entered_at" | "dormant">,
  windowMs: number = IDLE_DECAY_WINDOW_MS,
): string {
  // A dormant worker gets its own dim-amber dot; a deliberate Stop is never dormant.
  if (session.dormant) {
    return "bg-status-dormant";
  }
  if (session.status === "Idle" && isFreshIdle(session, windowMs)) {
    return "bg-status-fresh-idle";
  }
  return STATUS_DOT_CLASS[session.status] ?? "bg-status-idle";
}

export function getStatusTextClass(
  session: Pick<SessionResponse, "status" | "idle_entered_at" | "dormant">,
  windowMs: number = IDLE_DECAY_WINDOW_MS,
): string {
  if (session.dormant) {
    return "text-status-dormant";
  }
  if (session.status === "Idle" && isFreshIdle(session, windowMs)) {
    return "text-status-fresh-idle";
  }
  return STATUS_TEXT_CLASS[session.status] ?? "text-status-idle";
}

/** Fresh-idle counts as active. */
export function isSessionActive(
  session: Pick<SessionResponse, "status" | "idle_entered_at"> | SessionStatus,
  windowMs: number = IDLE_DECAY_WINDOW_MS,
): boolean {
  if (typeof session === "string") {
    return session === "Running" || session === "Waiting" || session === "Starting";
  }
  return (
    session.status === "Running" ||
    session.status === "Waiting" ||
    session.status === "Starting" ||
    isFreshIdle(session, windowMs)
  );
}
