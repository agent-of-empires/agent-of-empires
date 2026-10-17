import { useEffect, useSyncExternalStore } from "react";
import { isIOS, isStandalone } from "../lib/platform";
import {
  classifyPushHealth,
  hasFreshGoneFailure,
  readPushWanted,
  writePushWanted,
  type PushHealth,
  type ServerSubscriptionStatus,
} from "../lib/pushHealth";

export type PushState =
  | { kind: "loading" }
  | { kind: "off" }
  | { kind: "asking" }
  | { kind: "subscribing" }
  | { kind: "enabled" }
  | { kind: "sending-test" }
  | { kind: "disabling" }
  | { kind: "denied" }
  | {
      kind: "unsupported";
      reason: "no-api" | "ios-not-standalone" | "insecure-origin";
    }
  | { kind: "disabled-by-server" }
  | { kind: "error"; message: string };

const supportsPush = (): boolean =>
  typeof window !== "undefined" && "serviceWorker" in navigator && "PushManager" in window && "Notification" in window;

// Push needs a secure context; plain http is allowed only on loopback.
const isSecureOrigin = (): boolean => {
  if (typeof window === "undefined") return false;
  if (window.isSecureContext) return true;
  const host = window.location.hostname;
  return host === "localhost" || host === "127.0.0.1" || host === "[::1]";
};

const iosTab = () => isIOS() && !isStandalone();

/** Null when push can be used; otherwise the reason it can't. */
function unsupportedState(): PushState | null {
  if (!isSecureOrigin()) return { kind: "unsupported", reason: "insecure-origin" };
  if (supportsPush()) return null;
  return { kind: "unsupported", reason: iosTab() ? "ios-not-standalone" : "no-api" };
}

const errorState = (e: unknown): PushState => ({ kind: "error", message: e instanceof Error ? e.message : String(e) });

function postPush(path: string, body: unknown): Promise<Response> {
  return fetch(`/api/push/${path}`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
}

function subscribeBody(sub: PushSubscription) {
  const json = sub.toJSON();
  return { endpoint: json.endpoint, keys: json.keys };
}

async function currentSubscription(): Promise<PushSubscription | null> {
  const reg = await navigator.serviceWorker.ready;
  return reg.pushManager.getSubscription();
}

interface PushStatus {
  enabled: boolean;
  public_key?: string;
  subscription?: ServerSubscriptionStatus;
}

async function fetchStatus(endpoint: string | undefined): Promise<PushStatus> {
  const query = endpoint ? `?endpoint=${encodeURIComponent(endpoint)}` : "";
  const resp = await fetch(`/api/push/status${query}`);
  if (!resp.ok) throw new Error(`Could not fetch push status (${resp.status})`);
  return (await resp.json()) as PushStatus;
}

/** Null when the browser does not expose the key the subscription was made with. */
function keyMatches(sub: PushSubscription, publicKey: string): boolean | null {
  const key = sub.options?.applicationServerKey;
  if (!key) return null;
  const have = new Uint8Array(key);
  const want = base64UrlToUint8Array(publicKey);
  return have.length === want.length && have.every((b, i) => b === want[i]);
}

async function testFailureMessage(resp: Response): Promise<string | null> {
  const result = (await resp.json().catch(() => ({}))) as { reason?: string | null };
  switch (result.reason) {
    case null:
    case undefined:
      return null;
    case "key-mismatch":
      return "The push service rejected this device's key. Enable notifications again.";
    case "gone":
      return "This device's subscription has expired. Enable notifications again.";
    default:
      return `The push service did not deliver the test (${result.reason}).`;
  }
}

function base64UrlToUint8Array(b64: string): Uint8Array<ArrayBuffer> {
  const padding = "=".repeat((4 - (b64.length % 4)) % 4);
  const raw = atob((b64 + padding).replace(/-/g, "+").replace(/_/g, "/"));
  const buffer = new ArrayBuffer(raw.length);
  const out = new Uint8Array(buffer);
  for (let i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i);
  return out;
}

interface Snapshot {
  state: PushState;
  health: PushHealth;
}

const INITIAL: Snapshot = { state: { kind: "loading" }, health: "unknown" };

// One store shared by every mounted consumer, so the banner and the settings
// panel converge without a remount. It resets when the last consumer unmounts,
// or when the action running at that moment settles.
let snapshot = INITIAL;
const listeners = new Set<() => void>();
// A refresh must not run over an action in flight, and actions do not overlap.
let busy = false;
// Bumped by every refresh and action; writes from an older generation are dropped.
let generation = 0;
// Every pending refresh re-registration POST; actions wait for all so none lands after them.
let registration: Promise<unknown> = Promise.resolve();

function reset() {
  generation++;
  busy = false;
  snapshot = INITIAL;
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
    if (listeners.size === 0 && !busy) reset();
  };
}

function publish(gen: number, next: Partial<Snapshot>) {
  if (gen !== generation) return;
  snapshot = { ...snapshot, ...next };
  listeners.forEach((l) => l());
}

async function refresh() {
  if (busy) return;
  const gen = ++generation;
  const live = () => gen === generation;
  const unsupported = unsupportedState();
  if (unsupported) return publish(gen, { state: unsupported, health: "unknown" });
  try {
    const perm = Notification.permission;
    const sub = await currentSubscription();
    if (!live()) return;
    let status: PushStatus;
    try {
      status = await fetchStatus(sub?.endpoint);
    } catch {
      if (!live()) return;
      if (perm === "granted" && sub) writePushWanted(true);
      if (perm === "denied") publish(gen, { state: { kind: "denied" } });
      else publish(gen, { state: perm === "granted" && sub ? { kind: "enabled" } : { kind: "off" } });
      return;
    }
    if (!live()) return;
    if (!status.enabled) return publish(gen, { state: { kind: "disabled-by-server" }, health: "unknown" });
    // Subscriptions made before intent was recorded count as wanted.
    const wanted = readPushWanted() || (perm === "granted" && !!sub);
    if (wanted) writePushWanted(true);
    let next = classifyPushHealth({
      wanted,
      permission: perm,
      subscribed: !!sub,
      keyMatches: sub && status.public_key ? keyMatches(sub, status.public_key) : null,
      server: status.subscription ?? null,
    });
    // Re-binds the sub to the current token (#3386) or restores one the server dropped.
    if (sub && perm === "granted" && (next === "server-forgot" || (next === "healthy" && !status.subscription))) {
      const post = postPush("subscribe", subscribeBody(sub)).catch(() => null);
      registration = Promise.all([registration, post]);
      if ((await post)?.ok) next = "healthy";
    }
    if (perm === "denied") publish(gen, { health: next, state: { kind: "denied" } });
    else publish(gen, { health: next, state: perm === "granted" && sub ? { kind: "enabled" } : { kind: "off" } });
  } catch (e) {
    publish(gen, { state: errorState(e) });
  }
}

/** Starts an exclusive action; null when another one is already running. */
function beginAction(): number | null {
  if (busy) return null;
  busy = true;
  return ++generation;
}

function endAction(gen: number) {
  if (gen !== generation) return;
  busy = false;
  if (listeners.size === 0) reset();
}

// Also the repair path: iOS only allows requestPermission and a first subscribe inside a
// user gesture, so the permission request stays the first await of a click handler.
async function enable() {
  const unsupported = unsupportedState();
  if (unsupported) return publish(generation, { state: unsupported });
  const gen = beginAction();
  if (gen === null) return;
  publish(gen, { state: { kind: "asking" } });
  try {
    // WebKit resolves "denied" for a gestureless request even when already granted.
    if (Notification.permission !== "granted" && (await Notification.requestPermission()) !== "granted") {
      return publish(gen, {
        state: iosTab() ? { kind: "unsupported", reason: "ios-not-standalone" } : { kind: "denied" },
      });
    }
    await registration;
    publish(gen, { state: { kind: "subscribing" } });
    const vapidResp = await fetch("/api/push/vapid-public-key");
    if (!vapidResp.ok) {
      return publish(gen, { state: { kind: "error", message: `Server returned ${vapidResp.status} for VAPID key` } });
    }
    const { public_key } = (await vapidResp.json()) as { public_key: string };
    const reg = await navigator.serviceWorker.ready;
    let sub = await reg.pushManager.getSubscription();
    // A 404/410 permanently retires a browser endpoint. Re-subscribing the same
    // endpoint only stores a dead subscription again, so renew it before posting.
    const status = sub ? await fetchStatus(sub.endpoint) : null;
    const keyMismatch = sub && keyMatches(sub, public_key) !== true;
    const gone = hasFreshGoneFailure(status?.subscription ?? null);
    if (sub && (keyMismatch || gone)) {
      const stale = sub.endpoint;
      const unsubscribed = await sub.unsubscribe().catch(() => false);
      if (!unsubscribed && (await reg.pushManager.getSubscription())?.endpoint === stale) {
        throw new Error("Could not unsubscribe the expired notification subscription");
      }
      // Browser ownership is already retired; a failed server cleanup must
      // not block registering the replacement endpoint.
      await postPush("unsubscribe", { endpoint: stale }).catch(() => {});
      sub = null;
    }
    sub ??= await reg.pushManager.subscribe({
      userVisibleOnly: true,
      applicationServerKey: base64UrlToUint8Array(public_key),
    });
    // Also refreshes the server-side origin after the dashboard moved (#1188).
    const subscribeResp = await postPush("subscribe", subscribeBody(sub));
    if (!subscribeResp.ok) {
      // Don't keep a browser subscription the server has no record of.
      await sub.unsubscribe().catch(() => {});
      return publish(gen, {
        state: { kind: "error", message: `Server returned ${subscribeResp.status} on subscribe` },
      });
    }
    writePushWanted(true);
    publish(gen, { health: "healthy", state: { kind: "enabled" } });
  } catch (e) {
    publish(gen, { state: errorState(e) });
  } finally {
    endAction(gen);
  }
}

async function disable() {
  const gen = beginAction();
  if (gen === null) return;
  writePushWanted(false);
  publish(gen, { health: "not-wanted", state: { kind: "disabling" } });
  try {
    await registration;
    const sub = await currentSubscription();
    if (sub) {
      const endpoint = sub.endpoint;
      await sub.unsubscribe().catch(() => {});
      await postPush("unsubscribe", { endpoint }).catch(() => {});
    }
    publish(gen, { state: { kind: "off" } });
  } catch (e) {
    publish(gen, { state: errorState(e) });
  } finally {
    endAction(gen);
  }
}

async function sendTest() {
  const gen = beginAction();
  if (gen === null) return;
  publish(gen, { state: { kind: "sending-test" } });
  try {
    await registration;
    const sub = await currentSubscription();
    if (!sub) return publish(gen, { state: { kind: "error", message: "No active subscription" } });
    const resp = await postPush("test", { endpoint: sub.endpoint });
    if (!resp.ok)
      return publish(gen, { state: { kind: "error", message: `Test failed: server returned ${resp.status}` } });
    const failure = await testFailureMessage(resp);
    if (failure) return publish(gen, { health: "delivery-failed", state: { kind: "error", message: failure } });
    publish(gen, { state: { kind: "enabled" } });
  } catch (e) {
    publish(gen, { state: errorState(e) });
  } finally {
    endAction(gen);
  }
}

export function usePushSubscription() {
  const { state, health } = useSyncExternalStore(subscribe, () => snapshot);

  useEffect(() => {
    const timer = setTimeout(() => {
      void refresh();
    }, 0);
    const onVisibility = () => {
      if (document.visibilityState === "visible") void refresh();
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      clearTimeout(timer);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, []);

  return { state, health, enable, disable, sendTest, refresh };
}
