import { useCallback, useEffect, useState } from "react";
import { isIOS, isStandalone } from "../lib/platform";

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

  const refresh = useCallback(async () => {
    const unsupported = unsupportedState();
    if (unsupported) return setState(unsupported);
    try {
      const resp = await fetch("/api/push/status");
      if (resp.ok && !((await resp.json()) as { enabled: boolean }).enabled) {
        return setState({ kind: "disabled-by-server" });
      }
      const perm = Notification.permission;
      if (perm === "denied") return setState({ kind: "denied" });
      const sub = await currentSubscription();
      if (perm === "granted" && sub) {
        // Re-register on every open so the server re-binds the sub to the current token (#3386).
        await postPush("subscribe", subscribeBody(sub)).catch(() => {});
        setState({ kind: "enabled" });
      } else {
        setState({ kind: "off" });
      }
    } catch (e) {
      setState(errorState(e));
    }
  }, []);

  useEffect(() => {
    const timer = setTimeout(() => {
      void refresh();
    }, 0);
    return () => clearTimeout(timer);
  }, [refresh]);

  const enable = useCallback(async () => {
    const unsupported = unsupportedState();
    if (unsupported) return setState(unsupported);
    setState({ kind: "asking" });
    try {
      if ((await Notification.requestPermission()) !== "granted") {
        return setState(iosTab() ? { kind: "unsupported", reason: "ios-not-standalone" } : { kind: "denied" });
      }
      setState({ kind: "subscribing" });
      const vapidResp = await fetch("/api/push/vapid-public-key");
      if (!vapidResp.ok) {
        return setState({ kind: "error", message: `Server returned ${vapidResp.status} for VAPID key` });
      }
      const { public_key } = (await vapidResp.json()) as { public_key: string };
      const reg = await navigator.serviceWorker.ready;
      const sub = await reg.pushManager.subscribe({
        userVisibleOnly: true,
        applicationServerKey: base64UrlToUint8Array(public_key),
      });
      const subscribeResp = await postPush("subscribe", subscribeBody(sub));
      if (!subscribeResp.ok) {
        // Don't keep a browser subscription the server has no record of.
        await sub.unsubscribe().catch(() => {});
        return setState({ kind: "error", message: `Server returned ${subscribeResp.status} on subscribe` });
      }
      setState({ kind: "enabled" });
    } catch (e) {
      setState(errorState(e));
    }
  }, []);

  const disable = useCallback(async () => {
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
    }
  }, []);

  const sendTest = useCallback(async () => {
    setState({ kind: "sending-test" });
    try {
      const sub = await currentSubscription();
      if (!sub) return setState({ kind: "error", message: "No active subscription" });
      const resp = await postPush("test", { endpoint: sub.endpoint });
      if (!resp.ok) return setState({ kind: "error", message: `Test failed: server returned ${resp.status}` });
      setState({ kind: "enabled" });
    } catch (e) {
      setState(errorState(e));
    }
  }, []);

  // Refreshes the server-side subscription origin after the dashboard moved (#1188).
  const resubscribe = useCallback(async () => {
    await disable();
    await enable();
  }, [disable, enable]);

  return { state, enable, disable, sendTest, refresh, resubscribe };
}
