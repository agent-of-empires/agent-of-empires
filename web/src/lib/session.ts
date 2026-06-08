import type { SessionResponse, SessionStatus } from "./types";

/** How long a Stop-hooked Idle session keeps the freshness signal active
 *  (animated icon, fresh-idle color, "needs attention" bucketing).
 *  Default is 0: the dashboard does not light up freshly-stopped
 *  sessions, mirroring the Rust default for `Config.theme.idle_decay_minutes`.
 *
 *  Helpers below accept an optional `windowMs` override so a future
 *  client-side fetch of the server's configured value (#874) can opt in
 *  without changing the call sites. Pass a positive number to enable. */
export const IDLE_DECAY_WINDOW_MS = 0;

/** Tailwind class for status dot background color by session status */
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

/** Tailwind class for status text color by session status */
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

/** Milliseconds since this session most recently transitioned into Idle.
 *  Returns null for non-Idle sessions, sessions without an
 *  `idle_entered_at` timestamp (legacy state), or timestamps in the future
 *  (clock skew). */
export function idleAgeMs(
  session: Pick<SessionResponse, "status" | "idle_entered_at">,
): number | null {
  if (session.status !== "Idle") return null;
  if (!session.idle_entered_at) return null;
  const since = Date.parse(session.idle_entered_at);
  if (Number.isNaN(since)) return null;
  const age = Date.now() - since;
  return age >= 0 ? age : null;
}

/** True when the session is Idle and within `windowMs` of the Stop hook.
 *  Treated as "needs attention" alongside Waiting. Defaults to the
 *  module-level `IDLE_DECAY_WINDOW_MS` (0, i.e. off) so the freshness
 *  signal is opt-in across the dashboard. Pass a positive override to
 *  enable for a specific call site (or once #874 lands, fetch the
 *  server's configured value and thread it through). */
export function isFreshIdle(
  session: Pick<SessionResponse, "status" | "idle_entered_at">,
  windowMs: number = IDLE_DECAY_WINDOW_MS,
): boolean {
  if (windowMs <= 0) return false;
  const age = idleAgeMs(session);
  return age !== null && age < windowMs;
}

/** Background-color class for a session's status dot. Returns the standard
 *  status class for non-Idle states; for Idle, picks a fresh / decayed tier
 *  based on `idle_entered_at`. Two tiers (rather than continuous color-mix)
 *  keeps the class set static so Tailwind's JIT picks them up reliably. */
export function getStatusDotClass(
  session: Pick<SessionResponse, "status" | "idle_entered_at">,
  windowMs: number = IDLE_DECAY_WINDOW_MS,
): string {
  if (session.status === "Idle" && isFreshIdle(session, windowMs)) {
    return "bg-status-fresh-idle";
  }
  return STATUS_DOT_CLASS[session.status] ?? "bg-status-idle";
}

/** Text-color class equivalent of `getStatusDotClass`. */
export function getStatusTextClass(
  session: Pick<SessionResponse, "status" | "idle_entered_at">,
  windowMs: number = IDLE_DECAY_WINDOW_MS,
): string {
  if (session.status === "Idle" && isFreshIdle(session, windowMs)) {
    return "text-status-fresh-idle";
  }
  return STATUS_TEXT_CLASS[session.status] ?? "text-status-idle";
}

/** Whether a session status means the agent is actively doing something or
 *  has just finished and is awaiting the user's next prompt. Fresh-idle is
 *  bucketed with active so dashboard counts and filters surface it. */
export function isSessionActive(
  session: Pick<SessionResponse, "status" | "idle_entered_at"> | SessionStatus,
  windowMs: number = IDLE_DECAY_WINDOW_MS,
): boolean {
  if (typeof session === "string") {
    return (
      session === "Running" || session === "Waiting" || session === "Starting"
    );
  }
  return (
    session.status === "Running" ||
    session.status === "Waiting" ||
    session.status === "Starting" ||
    isFreshIdle(session, windowMs)
  );
}
