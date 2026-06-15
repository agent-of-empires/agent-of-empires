// @vitest-environment jsdom
//
// Coverage tests for usePushSubscription: the end-to-end Web Push hook
// backing NotificationSettings. The hook leans entirely on browser
// globals (navigator.serviceWorker, Notification, matchMedia,
// isSecureContext, atob) and the /api/push/* endpoints, so each test
// stubs those globals and a fetch router, then drives the returned
// callbacks through act().

import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { usePushSubscription } from "./usePushSubscription";

interface FakeSubscription {
  endpoint: string;
  toJSON: () => { endpoint: string; keys: Record<string, string> };
  unsubscribe: () => Promise<boolean>;
}

function makeSubscription(endpoint = "https://push.example/abc"): FakeSubscription {
  return {
    endpoint,
    toJSON: () => ({ endpoint, keys: { p256dh: "key", auth: "auth" } }),
    unsubscribe: vi.fn(async () => true),
  };
}

// Mutable handles the per-test setup configures.
let currentSubscription: FakeSubscription | null;
let subscribeImpl: () => Promise<FakeSubscription>;
let getSubscriptionImpl: () => Promise<FakeSubscription | null>;
let serviceWorkerReady: Promise<unknown>;

function installServiceWorker() {
  const pushManager = {
    getSubscription: vi.fn(() => getSubscriptionImpl()),
    subscribe: vi.fn(() => subscribeImpl()),
  };
  const registration = { pushManager };
  serviceWorkerReady = Promise.resolve(registration);
  Object.defineProperty(navigator, "serviceWorker", {
    configurable: true,
    value: { ready: serviceWorkerReady },
  });
}

// Default fetch router: every /api/push/* endpoint returns ok.
function installFetch(
  overrides: Partial<{
    status: { ok: boolean; body: unknown };
    vapid: { ok: boolean; status?: number; body?: unknown };
    subscribe: { ok: boolean; status?: number };
    test: { ok: boolean; status?: number };
    unsubscribe: { ok: boolean };
  }> = {},
) {
  const calls: string[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      const url = typeof input === "string" ? input : input.toString();
      calls.push(url);
      if (url.includes("/api/push/status")) {
        const o = overrides.status ?? { ok: true, body: { enabled: true } };
        return new Response(JSON.stringify(o.body ?? { enabled: true }), {
          status: o.ok ? 200 : 500,
        });
      }
      if (url.includes("/api/push/vapid-public-key")) {
        const o = overrides.vapid ?? { ok: true };
        return new Response(JSON.stringify(o.body ?? { public_key: "QUJD" }), {
          status: o.ok ? 200 : (o.status ?? 500),
        });
      }
      if (url.includes("/api/push/subscribe")) {
        const o = overrides.subscribe ?? { ok: true };
        return new Response("{}", { status: o.ok ? 200 : (o.status ?? 500) });
      }
      if (url.includes("/api/push/test")) {
        const o = overrides.test ?? { ok: true };
        return new Response("{}", { status: o.ok ? 200 : (o.status ?? 500) });
      }
      if (url.includes("/api/push/unsubscribe")) {
        const o = overrides.unsubscribe ?? { ok: true };
        return new Response("{}", { status: o.ok ? 200 : 500 });
      }
      return new Response("{}", { status: 200 });
    }),
  );
  return calls;
}

function setNotificationPermission(perm: NotificationPermission) {
  // The hook only reads Notification.permission and calls
  // Notification.requestPermission; a minimal stub is enough.
  vi.stubGlobal(
    "Notification",
    Object.assign(vi.fn() as unknown as typeof Notification, {
      permission: perm,
      requestPermission: vi.fn(async () => perm),
    }),
  );
}

function setUserAgent(ua: string) {
  Object.defineProperty(navigator, "userAgent", {
    configurable: true,
    value: ua,
  });
}

const originalDescriptors = {
  serviceWorker: Object.getOwnPropertyDescriptor(navigator, "serviceWorker"),
  userAgent: Object.getOwnPropertyDescriptor(navigator, "userAgent"),
};

beforeEach(() => {
  currentSubscription = makeSubscription();
  getSubscriptionImpl = async () => currentSubscription;
  subscribeImpl = async () => {
    currentSubscription = makeSubscription();
    return currentSubscription;
  };

  installServiceWorker();
  installFetch();
  setNotificationPermission("granted");
  setUserAgent("Mozilla/5.0 (Macintosh)");

  // PushManager + matchMedia on window for supportsPush / isStandalone.
  vi.stubGlobal("PushManager", function PushManager() {});
  vi.stubGlobal(
    "matchMedia",
    vi.fn(() => ({ matches: false })),
  );
  Object.defineProperty(window, "isSecureContext", {
    configurable: true,
    value: true,
  });
  // atob for base64UrlToUint8Array (jsdom provides it, but be explicit).
  vi.stubGlobal("atob", (s: string) => Buffer.from(s, "base64").toString("binary"));
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  if (originalDescriptors.serviceWorker) {
    Object.defineProperty(navigator, "serviceWorker", originalDescriptors.serviceWorker);
  }
  if (originalDescriptors.userAgent) {
    Object.defineProperty(navigator, "userAgent", originalDescriptors.userAgent);
  }
});

// The mount effect schedules refresh() via setTimeout(0). Flush the
// timer and the resulting microtasks under act so the initial state
// settles deterministically.
async function mountAndSettle() {
  const rendered = renderHook(() => usePushSubscription());
  await act(async () => {
    await new Promise((r) => setTimeout(r, 0));
    for (let i = 0; i < 10; i++) await Promise.resolve();
  });
  return rendered;
}

describe("usePushSubscription initial refresh", () => {
  it("starts loading then resolves to enabled when permission granted and a sub exists", async () => {
    const { result } = await mountAndSettle();
    expect(result.current.state).toEqual({ kind: "enabled" });
  });

  it("resolves to off when granted but no active subscription", async () => {
    getSubscriptionImpl = async () => null;
    currentSubscription = null;
    const { result } = await mountAndSettle();
    expect(result.current.state).toEqual({ kind: "off" });
  });

  it("resolves to denied when Notification.permission is denied", async () => {
    setNotificationPermission("denied");
    const { result } = await mountAndSettle();
    expect(result.current.state).toEqual({ kind: "denied" });
  });

  it("resolves to disabled-by-server when /api/push/status reports disabled", async () => {
    installFetch({ status: { ok: true, body: { enabled: false } } });
    const { result } = await mountAndSettle();
    expect(result.current.state).toEqual({ kind: "disabled-by-server" });
  });

  it("resolves to error when serviceWorker.ready rejects", async () => {
    const rejected = Promise.reject(new Error("sw boom"));
    // Pre-attach a noop catch so the rejection is never "unhandled" at
    // the microtask level (the hook awaits it on a later tick).
    rejected.catch(() => {});
    Object.defineProperty(navigator, "serviceWorker", {
      configurable: true,
      value: { ready: rejected },
    });
    const { result } = await mountAndSettle();
    expect(result.current.state).toEqual({ kind: "error", message: "sw boom" });
  });

  it("tolerates a non-ok status response and still falls through to permission/sub check", async () => {
    installFetch({ status: { ok: false, body: {} } });
    const { result } = await mountAndSettle();
    // status not ok -> skip disabled-by-server branch, granted + sub -> enabled.
    expect(result.current.state).toEqual({ kind: "enabled" });
  });
});

describe("usePushSubscription unsupported / insecure paths", () => {
  it("reports insecure-origin when not a secure context and host is a LAN IP", async () => {
    Object.defineProperty(window, "isSecureContext", {
      configurable: true,
      value: false,
    });
    Object.defineProperty(window, "location", {
      configurable: true,
      value: { hostname: "192.168.1.5" },
    });
    const { result } = await mountAndSettle();
    expect(result.current.state).toEqual({
      kind: "unsupported",
      reason: "insecure-origin",
    });
  });

  it("treats localhost over http as secure", async () => {
    Object.defineProperty(window, "isSecureContext", {
      configurable: true,
      value: false,
    });
    Object.defineProperty(window, "location", {
      configurable: true,
      value: { hostname: "localhost" },
    });
    const { result } = await mountAndSettle();
    expect(result.current.state).toEqual({ kind: "enabled" });
  });

  it("reports unsupported no-api when PushManager is absent", async () => {
    // Remove PushManager so supportsPush() ("PushManager" in window) is
    // false. Deleting the key, not setting it undefined. Non-iOS UA.
    delete (window as unknown as { PushManager?: unknown }).PushManager;
    const { result } = await mountAndSettle();
    expect(result.current.state).toEqual({
      kind: "unsupported",
      reason: "no-api",
    });
  });

  it("reports ios-not-standalone on iOS Safari tab without PushManager", async () => {
    delete (window as unknown as { PushManager?: unknown }).PushManager;
    setUserAgent("Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)");
    // matchMedia standalone = false and navigator.standalone unset.
    const { result } = await mountAndSettle();
    expect(result.current.state).toEqual({
      kind: "unsupported",
      reason: "ios-not-standalone",
    });
  });
});

describe("usePushSubscription enable()", () => {
  it("enables: requests permission, fetches vapid key, subscribes, posts to server", async () => {
    getSubscriptionImpl = async () => null;
    currentSubscription = null;
    const calls = installFetch();
    const { result } = await mountAndSettle();
    expect(result.current.state).toEqual({ kind: "off" });

    await act(async () => {
      await result.current.enable();
    });
    expect(result.current.state).toEqual({ kind: "enabled" });
    expect(calls.some((u) => u.includes("/api/push/vapid-public-key"))).toBe(true);
    expect(calls.some((u) => u.includes("/api/push/subscribe"))).toBe(true);
  });

  it("goes denied when requestPermission is not granted (non-iOS)", async () => {
    setNotificationPermission("denied");
    const { result } = await mountAndSettle();
    await act(async () => {
      await result.current.enable();
    });
    expect(result.current.state).toEqual({ kind: "denied" });
  });

  it("goes ios-not-standalone when permission denied on an iOS tab", async () => {
    setNotificationPermission("denied");
    setUserAgent("Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)");
    const { result } = await mountAndSettle();
    await act(async () => {
      await result.current.enable();
    });
    expect(result.current.state).toEqual({
      kind: "unsupported",
      reason: "ios-not-standalone",
    });
  });

  it("errors when the vapid-public-key endpoint fails", async () => {
    installFetch({ vapid: { ok: false, status: 500 } });
    const { result } = await mountAndSettle();
    await act(async () => {
      await result.current.enable();
    });
    expect(result.current.state).toEqual({
      kind: "error",
      message: "Server returned 500 for VAPID key",
    });
  });

  it("rolls back the subscription and errors when /api/push/subscribe fails", async () => {
    const sub = makeSubscription();
    subscribeImpl = async () => sub;
    installFetch({ subscribe: { ok: false, status: 422 } });
    const { result } = await mountAndSettle();
    await act(async () => {
      await result.current.enable();
    });
    expect(result.current.state).toEqual({
      kind: "error",
      message: "Server returned 422 on subscribe",
    });
    expect(sub.unsubscribe).toHaveBeenCalled();
  });

  it("reports insecure-origin from enable() when context is not secure", async () => {
    const { result } = await mountAndSettle();
    Object.defineProperty(window, "isSecureContext", {
      configurable: true,
      value: false,
    });
    Object.defineProperty(window, "location", {
      configurable: true,
      value: { hostname: "10.0.0.4" },
    });
    await act(async () => {
      await result.current.enable();
    });
    expect(result.current.state).toEqual({
      kind: "unsupported",
      reason: "insecure-origin",
    });
  });

  it("reports unsupported no-api from enable() when PushManager is absent", async () => {
    const { result } = await mountAndSettle();
    delete (window as unknown as { PushManager?: unknown }).PushManager;
    await act(async () => {
      await result.current.enable();
    });
    expect(result.current.state).toEqual({
      kind: "unsupported",
      reason: "no-api",
    });
  });

  it("errors when subscribe() throws", async () => {
    getSubscriptionImpl = async () => null;
    subscribeImpl = async () => {
      throw new Error("subscribe failed");
    };
    const { result } = await mountAndSettle();
    await act(async () => {
      await result.current.enable();
    });
    expect(result.current.state).toEqual({
      kind: "error",
      message: "subscribe failed",
    });
  });
});

describe("usePushSubscription disable()", () => {
  it("unsubscribes, posts to server, and lands off", async () => {
    const sub = makeSubscription();
    currentSubscription = sub;
    getSubscriptionImpl = async () => sub;
    const calls = installFetch();
    const { result } = await mountAndSettle();
    await act(async () => {
      await result.current.disable();
    });
    expect(result.current.state).toEqual({ kind: "off" });
    expect(sub.unsubscribe).toHaveBeenCalled();
    expect(calls.some((u) => u.includes("/api/push/unsubscribe"))).toBe(true);
  });

  it("lands off even when there is no active subscription", async () => {
    getSubscriptionImpl = async () => null;
    currentSubscription = null;
    const { result } = await mountAndSettle();
    await act(async () => {
      await result.current.disable();
    });
    expect(result.current.state).toEqual({ kind: "off" });
  });

  it("errors when serviceWorker.ready rejects during disable", async () => {
    const { result } = await mountAndSettle();
    const rejected = Promise.reject(new Error("no sw"));
    rejected.catch(() => {});
    Object.defineProperty(navigator, "serviceWorker", {
      configurable: true,
      value: { ready: rejected },
    });
    await act(async () => {
      await result.current.disable();
    });
    expect(result.current.state).toEqual({ kind: "error", message: "no sw" });
  });
});

describe("usePushSubscription sendTest()", () => {
  it("posts a test and returns to enabled on success", async () => {
    const calls = installFetch();
    const { result } = await mountAndSettle();
    await act(async () => {
      await result.current.sendTest();
    });
    expect(result.current.state).toEqual({ kind: "enabled" });
    expect(calls.some((u) => u.includes("/api/push/test"))).toBe(true);
  });

  it("errors when there is no active subscription", async () => {
    getSubscriptionImpl = async () => null;
    currentSubscription = null;
    const { result } = await mountAndSettle();
    await act(async () => {
      await result.current.sendTest();
    });
    expect(result.current.state).toEqual({
      kind: "error",
      message: "No active subscription",
    });
  });

  it("errors when the test endpoint returns non-ok", async () => {
    installFetch({ test: { ok: false, status: 503 } });
    const { result } = await mountAndSettle();
    await act(async () => {
      await result.current.sendTest();
    });
    expect(result.current.state).toEqual({
      kind: "error",
      message: "Test failed: server returned 503",
    });
  });
});

describe("usePushSubscription refresh() and resubscribe()", () => {
  it("refresh() re-evaluates state on demand", async () => {
    getSubscriptionImpl = async () => null;
    currentSubscription = null;
    const { result } = await mountAndSettle();
    expect(result.current.state).toEqual({ kind: "off" });

    // Flip to having a subscription, then refresh.
    const sub = makeSubscription();
    currentSubscription = sub;
    getSubscriptionImpl = async () => sub;
    await act(async () => {
      await result.current.refresh();
    });
    expect(result.current.state).toEqual({ kind: "enabled" });
  });

  it("resubscribe() runs disable then enable, ending enabled", async () => {
    const sub = makeSubscription();
    currentSubscription = sub;
    getSubscriptionImpl = async () => currentSubscription;
    subscribeImpl = async () => {
      currentSubscription = makeSubscription("https://push.example/new");
      return currentSubscription;
    };
    const calls = installFetch();
    const { result } = await mountAndSettle();
    await act(async () => {
      await result.current.resubscribe();
    });
    expect(result.current.state).toEqual({ kind: "enabled" });
    expect(calls.some((u) => u.includes("/api/push/unsubscribe"))).toBe(true);
    expect(calls.some((u) => u.includes("/api/push/subscribe"))).toBe(true);
  });
});
