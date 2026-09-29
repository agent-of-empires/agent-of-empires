// Web Push health: whether a device that asked for notifications still gets them.

import { isIOS } from "./platform";
import { safeGetItem, safeRemoveItem, safeSetItem } from "./safeStorage";

export type PushHealth =
  | "unknown"
  | "not-wanted"
  | "healthy"
  // Wanted, but the browser holds no subscription (WebKit revokes after silent pushes).
  | "revoked"
  // The subscription is bound to a VAPID key the server no longer signs with.
  | "key-mismatch"
  // The server lost or re-owned the subscription; re-posting it fixes this silently.
  | "server-forgot"
  | "permission-denied"
  // The push service refused the most recent send for this subscription.
  | "delivery-failed";

/** What `/api/push/status?endpoint=` reports about one subscription. */
export interface ServerSubscriptionStatus {
  registered: boolean;
  owned: boolean;
  last_success_at: string | null;
  last_failure_at: string | null;
  last_failure: string | null;
}

export interface PushHealthInput {
  wanted: boolean;
  permission: NotificationPermission;
  subscribed: boolean;
  /** Null when the browser does not expose the subscription's key. */
  keyMatches: boolean | null;
  server: ServerSubscriptionStatus | null;
}

// Transient failures (timeouts, 429, 5xx) are not worth alarming the user over.
const PERMANENT_FAILURES = new Set(["gone", "key-mismatch", "rejected"]);

function lastSendFailed(s: ServerSubscriptionStatus): boolean {
  if (!s.last_failure || !PERMANENT_FAILURES.has(s.last_failure) || !s.last_failure_at) return false;
  return !s.last_success_at || Date.parse(s.last_failure_at) > Date.parse(s.last_success_at);
}

export function classifyPushHealth(input: PushHealthInput): PushHealth {
  if (!input.wanted) return "not-wanted";
  if (input.permission === "denied") return "permission-denied";
  if (!input.subscribed) return "revoked";
  if (input.keyMatches === false) return "key-mismatch";
  const server = input.server;
  if (server && lastSendFailed(server)) {
    return server.last_failure === "key-mismatch" ? "key-mismatch" : "delivery-failed";
  }
  if (server && (!server.registered || !server.owned)) return "server-forgot";
  return "healthy";
}

/** Health states that need the user to act. */
export function needsAttention(health: PushHealth): boolean {
  return (
    health === "revoked" || health === "key-mismatch" || health === "delivery-failed" || health === "permission-denied"
  );
}

/** Blocked-permission guidance; a page cannot re-prompt once permission is denied. */
export function deniedGuidance(): string {
  return isIOS()
    ? "Notifications are blocked. Turn them on in iOS Settings > Notifications > Agent of Empires."
    : "Notifications are blocked. Allow them in this site's browser settings.";
}

// Per device: the key is deliberately not in webUiSync's synced set.
const WANTED_KEY = "aoe.push.wanted";

export function readPushWanted(): boolean {
  return safeGetItem(WANTED_KEY) === "1";
}

export function writePushWanted(wanted: boolean): void {
  if (wanted) safeSetItem(WANTED_KEY, "1");
  else safeRemoveItem(WANTED_KEY);
}
