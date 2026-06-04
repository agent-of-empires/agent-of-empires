import { useCallback, useEffect, useState } from "react";

// Minimal end-to-end push hook. Returns the current state and primitives
// for the NotificationSettings UI: enable(), disable(), sendTest(),
// refresh(). Works with the server endpoints /api/push/{status,
// vapid-public-key, subscribe, unsubscribe, test}.
//
// iOS note: Push requires the PWA to have been installed via "Add to
// Home Screen" AND opened standalone. In Safari tabs, `PushManager` is
// present but permission requests silently fail. Detection is in the
// `state.kind === 'unsupported'` path.

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

const isIOS = () =>
  typeof navigator !== "undefined" &&
  /iPad|iPhone|iPod/.test(navigator.userAgent);

const isStandalone = (): boolean => {
  if (typeof window === "undefined") return false;
  // iOS uses navigator.standalone; other platforms use the display-mode
  // media query. Both are worth checking.
  const ios = (window.navigator as unknown as { standalone?: boolean })
    .standalone === true;
  const displayMode = window.matchMedia?.(
    "(display-mode: standalone)",
  ).matches;
  return ios || !!displayMode;
};

const supportsPush = (): boolean =>
  typeof window !== "undefined" &&
  "serviceWorker" in navigator &&
  "PushManager" in window &&
  "Notification" in window;

/** Web Push requires a secure context. Localhost and 127.0.0.1 are
 *  allowed over http for dev, but any LAN IP or hostname must be
 *  served over https. This is especially relevant on mobile where
 *  users hit the dashboard at `http://<laptop-ip>:<port>` and are
 *  surprised push doesn't work. Tunnel mode (aoe serve --remote)
 *  provides https out of the box via Cloudflare. */
const isSecureOrigin = (): boolean => {
  if (typeof window === "undefined") return false;
  if (window.isSecureContext) return true;
  const host = window.location.hostname;
  return host === "localhost" || host === "127.0.0.1" || host === "[::1]";
};

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
    if (!isSecureOrigin()) {
      setState({ kind: "unsupported", reason: "insecure-origin" });
      return;
    }
    if (!supportsPush()) {
      if (isIOS() && !isStandalone()) {
        setState({ kind: "unsupported", reason: "ios-not-standalone" });
      } else {
        setState({ kind: "unsupported", reason: "no-api" });
      }
      return;
    }
    try {
      const resp = await fetch("/api/push/status");
      if (resp.ok) {
        const data = (await resp.json()) as { enabled: boolean };
        if (!data.enabled) {
          setState({ kind: "disabled-by-server" });
          return;
        }
      }
      const perm = Notification.permission;
      if (perm === "denied") {
        setState({ kind: "denied" });
        return;
      }
      const reg = await navigator.serviceWorker.ready;
      const sub = await reg.pushManager.getSubscription();
      if (perm === "granted" && sub) {
        setState({ kind: "enabled" });
      } else {
        setState({ kind: "off" });
      }
    } catch (e) {
      setState({
        kind: "error",
        message: e instanceof Error ? e.message : String(e),
      });
    }
  }, []);

  useEffect(() => {
    const timer = setTimeout(() => {
      void refresh();
    }, 0);
    return () => clearTimeout(timer);
  }, [refresh]);

  const enable = useCallback(async () => {
    if (!isSecureOrigin()) {
      setState({ kind: "unsupported", reason: "insecure-origin" });
      return;
    }
    if (!supportsPush()) {
      if (isIOS() && !isStandalone()) {
        setState({ kind: "unsupported", reason: "ios-not-standalone" });
      } else {
        setState({ kind: "unsupported", reason: "no-api" });
      }
      return;
    }
    setState({ kind: "asking" });
    try {
      const perm = await Notification.requestPermission();
      if (perm !== "granted") {
        if (isIOS() && !isStandalone()) {
          setState({ kind: "unsupported", reason: "ios-not-standalone" });
        } else {
          setState({ kind: "denied" });
        }
        return;
      }
      setState({ kind: "subscribing" });
      const vapidResp = await fetch("/api/push/vapid-public-key");
      if (!vapidResp.ok) {
        setState({
          kind: "error",
          message: `Server returned ${vapidResp.status} for VAPID key`,
        });
        return;
      }
      const { public_key } = (await vapidResp.json()) as {
        public_key: string;
      };
      const reg = await navigator.serviceWorker.ready;
      const sub = await reg.pushManager.subscribe({
        userVisibleOnly: true,
        applicationServerKey: base64UrlToUint8Array(public_key),
      });
      const json = sub.toJSON();
      const subscribeResp = await fetch("/api/push/subscribe", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          endpoint: json.endpoint,
          keys: json.keys,
        }),
      });
      if (!subscribeResp.ok) {
        // Roll back the browser-side subscription so we don't end up with
        // a subscription the server has no record of.
        await sub.unsubscribe().catch(() => {});
        setState({
          kind: "error",
          message: `Server returned ${subscribeResp.status} on subscribe`,
        });
        return;
      }
      setState({ kind: "enabled" });
    } catch (e) {
      setState({
        kind: "error",
        message: e instanceof Error ? e.message : String(e),
      });
    }
  }, []);

  const disable = useCallback(async () => {
    setState({ kind: "disabling" });
    try {
      const reg = await navigator.serviceWorker.ready;
      const sub = await reg.pushManager.getSubscription();
      if (sub) {
        const endpoint = sub.endpoint;
        await sub.unsubscribe().catch(() => {});
        await fetch("/api/push/unsubscribe", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ endpoint }),
        }).catch(() => {});
      }
      setState({ kind: "off" });
    } catch (e) {
      setState({
        kind: "error",
        message: e instanceof Error ? e.message : String(e),
      });
    }
  }, []);

  const sendTest = useCallback(async () => {
    setState({ kind: "sending-test" });
    try {
      const reg = await navigator.serviceWorker.ready;
      const sub = await reg.pushManager.getSubscription();
      if (!sub) {
        setState({ kind: "error", message: "No active subscription" });
        return;
      }
      const resp = await fetch("/api/push/test", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ endpoint: sub.endpoint }),
      });
      if (!resp.ok) {
        setState({
          kind: "error",
          message: `Test failed: server returned ${resp.status}`,
        });
        return;
      }
      setState({ kind: "enabled" });
    } catch (e) {
      setState({
        kind: "error",
        message: e instanceof Error ? e.message : String(e),
      });
    }
  }, []);

  // Re-subscribe: unsubscribe then re-subscribe in a single click. Used
  // by the "Re-subscribe" affordance in NotificationSettings to refresh
  // the server-side `Subscription.origin` field after the user moved
  // the deployment to a different port or origin. Existing subs from
  // before the origin-tracking landed have no recorded origin and get
  // skipped on send (#1188); re-subscribing refreshes the entry.
  const resubscribe = useCallback(async () => {
    await disable();
    await enable();
  }, [disable, enable]);

  return { state, enable, disable, sendTest, refresh, resubscribe };
}
