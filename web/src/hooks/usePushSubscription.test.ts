// @vitest-environment jsdom

import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { usePushSubscription, type PushState } from "./usePushSubscription";

type Hook = ReturnType<typeof usePushSubscription>;

// SERVER_KEY is base64url for the bytes of "ABC".
const SERVER_KEY = "QUJD";
const keyBytes = (s: string) => new TextEncoder().encode(s).buffer;

function makeSubscription(endpoint = "https://push.example/abc", key: ArrayBuffer | null = null) {
  const sub = {
    endpoint,
    options: { applicationServerKey: key },
    toJSON: () => ({ endpoint, keys: { p256dh: "key", auth: "auth" } }),
    unsubscribe: vi.fn(async () => true),
  };
  sub.unsubscribe.mockImplementation(async () => {
    if (currentSub === sub) currentSub = null;
    return true;
  });
  return sub;
}
type FakeSubscription = ReturnType<typeof makeSubscription>;

let currentSub: FakeSubscription | null;
let subscribeImpl: () => Promise<FakeSubscription>;
let calls: string[];

const IOS_UA = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)";
const DISABLED_BY_SERVER = { status: { ok: true, body: { enabled: false } } };

type StubStatusResponse = { ok: boolean; body: unknown } | Error;
type StubUnsubscribeResponse = number | Error;

function setServiceWorkerReady(ready: Promise<unknown>) {
  Object.defineProperty(navigator, "serviceWorker", { configurable: true, value: { ready } });
}

function rejectServiceWorker(message: string) {
  const rejected = Promise.reject(new Error(message));
  rejected.catch(() => {});
  setServiceWorkerReady(rejected);
}

interface FetchOverrides {
  status?: { ok: boolean; body: unknown };
  statusResponses?: StubStatusResponse[] | (() => StubStatusResponse);
  statusError?: Error;
  vapid?: number;
  subscribe?: number;
  unsubscribe?: number;
  unsubscribeResponses?: StubUnsubscribeResponse[] | (() => StubUnsubscribeResponse);
  test?: number;
  testBody?: unknown;
}

function installFetch(overrides: FetchOverrides = {}) {
  calls = [];
  let statusIndex = 0;
  let unsubscribeIndex = 0;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      calls.push(url);
      if (url.includes("/status")) {
        const responses = overrides.statusResponses;
        const response = typeof responses === "function" ? responses() : responses?.[statusIndex++];
        if (response instanceof Error) throw response;
        if (overrides.statusError) throw overrides.statusError;
        const o = response ?? overrides.status ?? { ok: true, body: { enabled: true } };
        return new Response(JSON.stringify(o.body), { status: o.ok ? 200 : 500 });
      }
      if (url.includes("/vapid-public-key")) {
        return new Response(JSON.stringify({ public_key: SERVER_KEY }), { status: overrides.vapid ?? 200 });
      }
      if (url.includes("/test")) {
        return new Response(JSON.stringify(overrides.testBody ?? {}), { status: overrides.test ?? 200 });
      }
      if (url.includes("/unsubscribe")) {
        const responses = overrides.unsubscribeResponses;
        const response = typeof responses === "function" ? responses() : responses?.[unsubscribeIndex++];
        if (response instanceof Error) throw response;
        return new Response("{}", { status: response ?? overrides.unsubscribe ?? 200 });
      }
      const status = url.includes("/subscribe") ? overrides.subscribe : 200;
      return new Response("{}", { status: status ?? 200 });
    }),
  );
}

function setPermission(perm: NotificationPermission) {
  vi.stubGlobal(
    "Notification",
    Object.assign(vi.fn(), { permission: perm, requestPermission: vi.fn(async () => perm) }),
  );
}

function setUserAgent(ua: string) {
  Object.defineProperty(navigator, "userAgent", { configurable: true, value: ua });
}

function setInsecureHost(hostname: string) {
  Object.defineProperty(window, "isSecureContext", { configurable: true, value: false });
  Object.defineProperty(window, "location", { configurable: true, value: { hostname } });
}

const removePushManager = () => delete (window as unknown as { PushManager?: unknown }).PushManager;
const noSubscription = () => {
  currentSub = null;
};
const called = (fragment: string) => calls.some((u) => u.includes(fragment));

const originalDescriptors = {
  serviceWorker: Object.getOwnPropertyDescriptor(navigator, "serviceWorker"),
  userAgent: Object.getOwnPropertyDescriptor(navigator, "userAgent"),
};

beforeEach(() => {
  localStorage.clear();
  currentSub = makeSubscription();
  subscribeImpl = async () => (currentSub = makeSubscription());
  const pushManager = { getSubscription: vi.fn(async () => currentSub), subscribe: vi.fn(() => subscribeImpl()) };
  setServiceWorkerReady(Promise.resolve({ pushManager }));
  installFetch();
  setPermission("granted");
  setUserAgent("Mozilla/5.0 (Macintosh)");
  vi.stubGlobal("PushManager", function PushManager() {});
  vi.stubGlobal(
    "matchMedia",
    vi.fn(() => ({ matches: false })),
  );
  Object.defineProperty(window, "isSecureContext", { configurable: true, value: true });
  vi.stubGlobal("atob", (s: string) => Buffer.from(s, "base64").toString("binary"));
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  for (const [key, descriptor] of Object.entries(originalDescriptors)) {
    if (descriptor) Object.defineProperty(navigator, key, descriptor);
  }
});

async function mountAndSettle() {
  const rendered = renderHook(() => usePushSubscription());
  await act(async () => {
    await new Promise((r) => setTimeout(r, 0));
    for (let i = 0; i < 10; i++) await Promise.resolve();
  });
  return rendered;
}

const unsupported = (reason: string): PushState => ({ kind: "unsupported", reason }) as PushState;
const error = (message: string): PushState => ({ kind: "error", message });

describe("usePushSubscription initial refresh", () => {
  it.each<[string, () => void, PushState]>([
    ["granted with a subscription", () => {}, { kind: "enabled" }],
    ["granted with no subscription", noSubscription, { kind: "off" }],
    ["denied permission", () => setPermission("denied"), { kind: "denied" }],
    ["push disabled on the server", () => installFetch(DISABLED_BY_SERVER), { kind: "disabled-by-server" }],
    ["a failing status endpoint", () => installFetch({ status: { ok: false, body: {} } }), { kind: "enabled" }],
    ["a rejected serviceWorker.ready", () => rejectServiceWorker("sw boom"), error("sw boom")],
    ["an insecure LAN origin", () => setInsecureHost("192.168.1.5"), unsupported("insecure-origin")],
    ["localhost over http", () => setInsecureHost("localhost"), { kind: "enabled" }],
    ["no PushManager", removePushManager, unsupported("no-api")],
    [
      "an iOS Safari tab",
      () => {
        removePushManager();
        setUserAgent(IOS_UA);
      },
      unsupported("ios-not-standalone"),
    ],
  ])("with %s", async (_label, arrange, expected) => {
    arrange();
    const { result } = await mountAndSettle();
    expect(result.current.state).toEqual(expected);
  });

  it("re-registers an existing subscription with the server on open (#3386)", async () => {
    await mountAndSettle();
    expect(called("/api/push/subscribe")).toBe(true);
  });

  it.each<[string, FetchOverrides]>([
    ["a non-OK response", { status: { ok: false, body: {} } }],
    ["a network error", { statusError: new Error("status unavailable") }],
  ])("keeps the local enabled state after %s during refresh", async (_label, overrides) => {
    const { result } = await mountAndSettle();
    expect(result.current.state).toEqual({ kind: "enabled" });
    const health = result.current.health;

    installFetch(overrides);
    await act_(result, "refresh");

    expect(result.current.state).toEqual({ kind: "enabled" });
    expect(result.current.health).toBe(health);
    expect(calls).toEqual(["/api/push/status?endpoint=https%3A%2F%2Fpush.example%2Fabc"]);
  });

  it("retains existing subscription intent when the initial status request fails", async () => {
    installFetch({ statusError: new Error("status unavailable") });
    const { result } = await mountAndSettle();
    expect(result.current.state).toEqual({ kind: "enabled" });
    expect(result.current.health).toBe("unknown");
    expect(localStorage.getItem("aoe.push.wanted")).toBe("1");
    expect(called("/api/push/subscribe")).toBe(false);

    noSubscription();
    installFetch(statusWith(serverSub({ registered: false, owned: false })));
    await act_(result, "refresh");
    expect(result.current.state).toEqual({ kind: "off" });
    expect(result.current.health).toBe("revoked");
    expect(called("/api/push/subscribe")).toBe(false);
  });

  it.each<[NotificationPermission, PushState]>([
    ["granted", { kind: "off" }],
    ["denied", { kind: "denied" }],
  ])("retains %s permission without a subscription when status fails", async (permission, expected) => {
    noSubscription();
    setPermission(permission);
    installFetch({ status: { ok: false, body: {} } });
    const { result } = await mountAndSettle();
    expect(result.current.state).toEqual(expected);
    expect(result.current.health).toBe("unknown");
    expect(localStorage.getItem("aoe.push.wanted")).toBeNull();
    expect(calls).toEqual(["/api/push/status"]);
  });
});

async function act_(result: { current: Hook }, action: keyof Omit<Hook, "state">) {
  await act(async () => {
    await result.current[action]();
  });
  return result.current.state;
}

describe("usePushSubscription enable()", () => {
  it("requests permission, fetches the VAPID key, subscribes, and registers", async () => {
    noSubscription();
    const { result } = await mountAndSettle();
    expect(result.current.state).toEqual({ kind: "off" });
    expect(await act_(result, "enable")).toEqual({ kind: "enabled" });
    expect(called("/api/push/vapid-public-key") && called("/api/push/subscribe")).toBe(true);
  });

  it.each<[string, () => void, PushState]>([
    ["permission is refused", () => setPermission("denied"), { kind: "denied" }],
    [
      "permission is refused on an iOS tab",
      () => {
        setPermission("denied");
        setUserAgent(IOS_UA);
      },
      unsupported("ios-not-standalone"),
    ],
    ["the VAPID endpoint fails", () => installFetch({ vapid: 500 }), error("Server returned 500 for VAPID key")],
    ["the context turns insecure", () => setInsecureHost("10.0.0.4"), unsupported("insecure-origin")],
    [
      "subscribe() throws",
      () => {
        subscribeImpl = async () => {
          throw new Error("subscribe failed");
        };
      },
      error("subscribe failed"),
    ],
  ])("lands in the right state when %s", async (_label, arrange, expected) => {
    const { result } = await mountAndSettle();
    arrange();
    expect(await act_(result, "enable")).toEqual(expected);
  });

  it.each<[string, FetchOverrides, PushState]>([
    ["the status request fails", { statusError: new Error("status unavailable") }, error("status unavailable")],
    ["the status response is not OK", { status: { ok: false, body: {} } }, error("Could not fetch push status (500)")],
  ])("does not renew a subscription when %s", async (_label, overrides, expected) => {
    const existing = currentSub!;
    const subscribe = vi.fn(async () => makeSubscription("https://push.example/new", keyBytes("ABC")));
    subscribeImpl = subscribe;
    const { result } = await mountAndSettle();
    installFetch(overrides);

    expect(await act_(result, "enable")).toEqual(expected);
    expect(existing.unsubscribe).not.toHaveBeenCalled();
    expect(subscribe).not.toHaveBeenCalled();
    expect(currentSub).toBe(existing);
  });

  it("rolls back the browser subscription when the server rejects it", async () => {
    const sub = makeSubscription();
    subscribeImpl = async () => sub;
    const { result } = await mountAndSettle();
    installFetch({ subscribe: 422 });
    expect(await act_(result, "enable")).toEqual(error("Server returned 422 on subscribe"));
    expect(sub.unsubscribe).toHaveBeenCalled();
  });
});

describe("usePushSubscription disable() and sendTest()", () => {
  it("disable unsubscribes, tells the server, and lands off", async () => {
    const sub = currentSub!;
    const { result } = await mountAndSettle();
    expect(await act_(result, "disable")).toEqual({ kind: "off" });
    expect(sub.unsubscribe).toHaveBeenCalled();
    expect(called("/api/push/unsubscribe")).toBe(true);
  });

  it.each<[string, keyof Omit<Hook, "state">, () => void, PushState]>([
    ["disable with no subscription", "disable", noSubscription, { kind: "off" }],
    ["disable with a rejected serviceWorker", "disable", () => rejectServiceWorker("no sw"), error("no sw")],
    ["sendTest on success", "sendTest", () => {}, { kind: "enabled" }],
    ["sendTest with no subscription", "sendTest", noSubscription, error("No active subscription")],
    [
      "sendTest when the server fails",
      "sendTest",
      () => installFetch({ test: 503 }),
      error("Test failed: server returned 503"),
    ],
  ])("%s", async (_label, action, arrange, expected) => {
    const { result } = await mountAndSettle();
    arrange();
    expect(await act_(result, action)).toEqual(expected);
    if (action === "sendTest" && expected.kind === "enabled") expect(called("/api/push/test")).toBe(true);
  });
});

describe("usePushSubscription enable() with an existing subscription", () => {
  it.each<[string, ArrayBuffer | null, boolean]>([
    ["keeps one bound to the server key", keyBytes("ABC"), false],
    ["replaces one bound to another key", keyBytes("XYZ"), true],
    ["replaces one whose key the browser hides", null, true],
  ])("%s", async (_label, key, replaced) => {
    const existing = makeSubscription("https://push.example/old", key);
    currentSub = existing;
    const { result } = await mountAndSettle();
    calls.length = 0;
    expect(await act_(result, "enable")).toEqual({ kind: "enabled" });
    expect(existing.unsubscribe).toHaveBeenCalledTimes(replaced ? 1 : 0);
    expect(called("/api/push/unsubscribe")).toBe(replaced);
    expect(called("/api/push/subscribe")).toBe(true);
  });

  it("skips requestPermission when already granted, since WebKit denies a gestureless request", async () => {
    const { result } = await mountAndSettle();
    await act_(result, "enable");
    expect(Notification.requestPermission).not.toHaveBeenCalled();
  });

  it("renews an endpoint after a newer gone delivery failure", async () => {
    const existing = makeSubscription("https://push.example/old", keyBytes("ABC"));
    const replacement = makeSubscription("https://push.example/new", keyBytes("ABC"));
    currentSub = existing;
    subscribeImpl = async () => (currentSub = replacement);
    installFetch(
      statusWith(
        serverSub({
          registered: false,
          owned: false,
          last_failure: "gone",
          last_failure_at: "2026-09-02T10:00:00Z",
        }),
      ),
    );
    const { result } = await mountAndSettle();
    expect(result.current.health).toBe("delivery-failed");
    calls.length = 0;

    expect(await act_(result, "enable")).toEqual({ kind: "enabled" });
    expect(existing.unsubscribe).toHaveBeenCalledTimes(1);
    expect(replacement.unsubscribe).not.toHaveBeenCalled();
    expect(currentSub?.endpoint).toBe("https://push.example/new");
    expect(called("/api/push/subscribe")).toBe(true);
  });

  it("keeps a healthy same-key endpoint", async () => {
    const existing = makeSubscription("https://push.example/healthy", keyBytes("ABC"));
    currentSub = existing;
    installFetch(statusWith(serverSub()));
    const { result } = await mountAndSettle();

    expect(await act_(result, "enable")).toEqual({ kind: "enabled" });
    expect(existing.unsubscribe).not.toHaveBeenCalled();
    expect(currentSub).toBe(existing);
  });

  it.each<[string, boolean]>([
    ["a server error", false],
    ["a lost response after server removal", true],
  ])("continues renewal after %s during best-effort cleanup", async (_label, removed) => {
    const existing = makeSubscription("https://push.example/expired", keyBytes("ABC"));
    const replacement = makeSubscription("https://push.example/replacement", keyBytes("ABC"));
    currentSub = existing;
    const subscribe = vi.fn(async () => (currentSub = replacement));
    subscribeImpl = subscribe;
    let serverRegistered = true;
    installFetch({
      ...statusWith(
        serverSub({
          registered: true,
          owned: true,
          last_failure: "gone",
          last_failure_at: "2026-09-02T10:00:00Z",
        }),
      ),
      unsubscribeResponses: () => {
        if (removed) serverRegistered = false;
        return removed ? new Error("removal response lost") : 500;
      },
    });
    const { result } = await mountAndSettle();
    calls.length = 0;

    expect(await act_(result, "enable")).toEqual({ kind: "enabled" });
    expect(existing.unsubscribe).toHaveBeenCalledTimes(1);
    expect(subscribe).toHaveBeenCalledTimes(1);
    expect(currentSub).toBe(replacement);
    const removalIndex = calls.findLastIndex((url) => url.includes("/api/push/unsubscribe"));
    const registrationIndex = calls.findIndex((url) => url.includes("/api/push/subscribe"));
    expect(removalIndex).toBeGreaterThanOrEqual(0);
    expect(registrationIndex).toBeGreaterThan(removalIndex);
    expect(serverRegistered).toBe(!removed);
  });

  it("does not let a forbidden best-effort cleanup block renewal", async () => {
    const existing = makeSubscription("https://push.example/expired", keyBytes("ABC"));
    const replacement = makeSubscription("https://push.example/replacement", keyBytes("ABC"));
    currentSub = existing;
    subscribeImpl = vi.fn(async () => (currentSub = replacement));
    const expiredStatus = statusWith(
      serverSub({
        last_failure: "gone",
        last_failure_at: "2026-09-02T10:00:00Z",
      }),
    );
    installFetch({
      statusResponses: [expiredStatus.status, expiredStatus.status],
      unsubscribeResponses: [403],
    });
    const { result } = await mountAndSettle();
    calls.length = 0;

    expect(await act_(result, "enable")).toEqual({ kind: "enabled" });
    expect(existing.unsubscribe).toHaveBeenCalledTimes(1);
    expect(subscribeImpl).toHaveBeenCalledTimes(1);
    expect(currentSub).toBe(replacement);
    expect(calls.filter((url) => url.includes("/api/push/unsubscribe"))).toHaveLength(1);
    expect(calls.filter((url) => url.includes("/api/push/status?endpoint="))).toHaveLength(1);
    expect(calls.at(-1)).toContain("/api/push/subscribe");
  });

  it("does not replace an endpoint when browser unsubscribe fails", async () => {
    const existing = makeSubscription("https://push.example/expired", keyBytes("ABC"));
    existing.unsubscribe.mockResolvedValue(false);
    currentSub = existing;
    subscribeImpl = vi.fn(async () => makeSubscription("https://push.example/should-not-exist", keyBytes("ABC")));
    installFetch(
      statusWith(
        serverSub({
          registered: false,
          owned: false,
          last_failure: "gone",
          last_failure_at: "2026-09-02T10:00:00Z",
        }),
      ),
    );
    const { result } = await mountAndSettle();

    expect(await act_(result, "enable")).toEqual({
      kind: "error",
      message: "Could not unsubscribe the expired notification subscription",
    });
    expect(subscribeImpl).not.toHaveBeenCalled();
  });

  it("replaces an endpoint when unsubscribe reports it was already inactive", async () => {
    const existing = makeSubscription("https://push.example/expired", keyBytes("ABC"));
    const replacement = makeSubscription("https://push.example/replacement", keyBytes("ABC"));
    existing.unsubscribe.mockImplementation(async () => {
      currentSub = null;
      return false;
    });
    currentSub = existing;
    subscribeImpl = vi.fn(async () => (currentSub = replacement));
    installFetch(
      statusWith(
        serverSub({
          registered: false,
          owned: false,
          last_failure: "gone",
          last_failure_at: "2026-09-02T10:00:00Z",
        }),
      ),
    );
    const { result } = await mountAndSettle();

    expect(await act_(result, "enable")).toEqual({ kind: "enabled" });
    expect(existing.unsubscribe).toHaveBeenCalledTimes(1);
    expect(subscribeImpl).toHaveBeenCalledTimes(1);
    expect(currentSub).toBe(replacement);
  });
});

const statusWith = (subscription: unknown) => ({
  status: { ok: true, body: { enabled: true, public_key: SERVER_KEY, subscription } },
});
const serverSub = (over: object = {}) => ({
  registered: true,
  owned: true,
  last_success_at: null,
  last_failure_at: null,
  last_failure: null,
  ...over,
});

describe("usePushSubscription health", () => {
  it.each<[string, () => void, string, boolean]>([
    ["a healthy subscription", () => installFetch(statusWith(serverSub())), "healthy", false],
    [
      "a subscription the server forgot, re-posted silently",
      () => installFetch(statusWith(serverSub({ registered: false }))),
      "healthy",
      true,
    ],
    [
      "a revoked subscription the user still wants",
      () => {
        localStorage.setItem("aoe.push.wanted", "1");
        noSubscription();
      },
      "revoked",
      false,
    ],
    ["no subscription and no intent", noSubscription, "not-wanted", false],
    [
      "a subscription bound to an old server key",
      () => {
        currentSub = makeSubscription(undefined, keyBytes("XYZ"));
        installFetch(statusWith(serverSub()));
      },
      "key-mismatch",
      false,
    ],
    [
      "a push the service rejected",
      () => installFetch(statusWith(serverSub({ last_failure: "rejected", last_failure_at: "2026-09-01T10:00:00Z" }))),
      "delivery-failed",
      false,
    ],
  ])("classifies %s", async (_label, arrange, health, reposted) => {
    arrange();
    const { result } = await mountAndSettle();
    expect(result.current.health).toBe(health);
    expect(called("/api/push/subscribe")).toBe(reposted);
    expect(called(`/api/push/status?endpoint=${encodeURIComponent("https://push.example/abc")}`)).toBe(
      currentSub?.endpoint === "https://push.example/abc",
    );
  });

  it("records intent on enable, clears it on disable, and treats an existing subscription as wanted", async () => {
    const { result } = await mountAndSettle();
    expect(localStorage.getItem("aoe.push.wanted")).toBe("1");
    await act_(result, "disable");
    expect(localStorage.getItem("aoe.push.wanted")).toBeNull();
    expect(result.current.health).toBe("not-wanted");
    await act_(result, "enable");
    expect(localStorage.getItem("aoe.push.wanted")).toBe("1");
    expect(result.current.health).toBe("healthy");
  });

  it("re-checks when the app becomes visible", async () => {
    localStorage.setItem("aoe.push.wanted", "1");
    const { result } = await mountAndSettle();
    expect(result.current.health).toBe("healthy");
    noSubscription();
    await act(async () => {
      document.dispatchEvent(new Event("visibilitychange"));
      for (let i = 0; i < 10; i++) await Promise.resolve();
    });
    expect(result.current.health).toBe("revoked");
  });

  it.each([
    ["key-mismatch", "The push service rejected this device's key. Enable notifications again."],
    ["gone", "This device's subscription has expired. Enable notifications again."],
    ["rejected", "The push service did not deliver the test (rejected)."],
  ])("sendTest surfaces an undelivered %s test", async (reason, message) => {
    const { result } = await mountAndSettle();
    installFetch({ testBody: { delivered: 0, reason } });
    expect(await act_(result, "sendTest")).toEqual(error(message));
    expect(result.current.health).toBe("delivery-failed");
  });
});
