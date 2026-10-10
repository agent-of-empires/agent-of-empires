import { useCallback, useEffect, useRef, useState } from "react";
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

export function usePushSubscription() {
  const [state, setState] = useState<PushState>({ kind: "loading" });
  const [health, setHealth] = useState<PushHealth>("unknown");
  // A visibility refresh must not overwrite the state of an action in flight.
  const busy = useRef(false);

  const refresh = useCallback(async () => {
    const unsupported = unsupportedState();
    if (unsupported) {
      setHealth("unknown");
      return setState(unsupported);
    }
    try {
      const perm = Notification.permission;
      const sub = await currentSubscription();
      let status: PushStatus;
      try {
        status = await fetchStatus(sub?.endpoint);
      } catch {
        if (busy.current) return;
        if (perm === "granted" && sub) writePushWanted(true);
        if (perm === "denied") setState({ kind: "denied" });
        else setState(perm === "granted" && sub ? { kind: "enabled" } : { kind: "off" });
        return;
      }
      if (!status.enabled) {
        setHealth("unknown");
        return setState({ kind: "disabled-by-server" });
      }
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
        const resp = await postPush("subscribe", subscribeBody(sub)).catch(() => null);
        if (resp?.ok) next = "healthy";
      }
      if (busy.current) return;
      setHealth(next);
      if (perm === "denied") setState({ kind: "denied" });
      else setState(perm === "granted" && sub ? { kind: "enabled" } : { kind: "off" });
    } catch (e) {
      if (!busy.current) setState(errorState(e));
    }
  }, []);

  useEffect(() => {
    const timer = setTimeout(() => {
      void refresh();
    }, 0);
    const onVisibility = () => {
      if (document.visibilityState === "visible" && !busy.current) void refresh();
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      clearTimeout(timer);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [refresh]);

  // Also the repair path: iOS only allows requestPermission and a first subscribe inside a
  // user gesture, so the permission request stays the first await of a click handler.
  const enable = useCallback(async () => {
    const unsupported = unsupportedState();
    if (unsupported) return setState(unsupported);
    busy.current = true;
    setState({ kind: "asking" });
    try {
      // WebKit resolves "denied" for a gestureless request even when already granted.
      if (Notification.permission !== "granted" && (await Notification.requestPermission()) !== "granted") {
        return setState(iosTab() ? { kind: "unsupported", reason: "ios-not-standalone" } : { kind: "denied" });
      }
      setState({ kind: "subscribing" });
      const vapidResp = await fetch("/api/push/vapid-public-key");
      if (!vapidResp.ok) {
        return setState({ kind: "error", message: `Server returned ${vapidResp.status} for VAPID key` });
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
        return setState({ kind: "error", message: `Server returned ${subscribeResp.status} on subscribe` });
      }
      writePushWanted(true);
      setHealth("healthy");
      setState({ kind: "enabled" });
    } catch (e) {
      setState(errorState(e));
    } finally {
      busy.current = false;
    }
  }, []);

  const disable = useCallback(async () => {
    busy.current = true;
    writePushWanted(false);
    setHealth("not-wanted");
    setState({ kind: "disabling" });
    try {
      const sub = await currentSubscription();
      if (sub) {
        const endpoint = sub.endpoint;
        await sub.unsubscribe().catch(() => {});
        await postPush("unsubscribe", { endpoint }).catch(() => {});
      }
      setState({ kind: "off" });
    } catch (e) {
      setState(errorState(e));
    } finally {
      busy.current = false;
    }
  }, []);

  const sendTest = useCallback(async () => {
    busy.current = true;
    setState({ kind: "sending-test" });
    try {
      const sub = await currentSubscription();
      if (!sub) return setState({ kind: "error", message: "No active subscription" });
      const resp = await postPush("test", { endpoint: sub.endpoint });
      if (!resp.ok) return setState({ kind: "error", message: `Test failed: server returned ${resp.status}` });
      const failure = await testFailureMessage(resp);
      if (failure) {
        setHealth("delivery-failed");
        return setState({ kind: "error", message: failure });
      }
      setState({ kind: "enabled" });
    } catch (e) {
      setState(errorState(e));
    } finally {
      busy.current = false;
    }
  }, []);

  return { state, health, enable, disable, sendTest, refresh };
}
