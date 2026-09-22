// @vitest-environment jsdom

import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { usePushSubscription, type PushState } from "./usePushSubscription";

type Hook = ReturnType<typeof usePushSubscription>;

function makeSubscription(endpoint = "https://push.example/abc") {
  return {
    endpoint,
    toJSON: () => ({ endpoint, keys: { p256dh: "key", auth: "auth" } }),
    unsubscribe: vi.fn(async () => true),
  };
}
type FakeSubscription = ReturnType<typeof makeSubscription>;

let currentSub: FakeSubscription | null;
let subscribeImpl: () => Promise<FakeSubscription>;
let calls: string[];

const IOS_UA = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)";
const DISABLED_BY_SERVER = { status: { ok: true, body: { enabled: false } } };

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
  vapid?: number;
  subscribe?: number;
  test?: number;
}

function installFetch(overrides: FetchOverrides = {}) {
  calls = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      calls.push(url);
      if (url.includes("/status")) {
        const o = overrides.status ?? { ok: true, body: { enabled: true } };
        return new Response(JSON.stringify(o.body), { status: o.ok ? 200 : 500 });
      }
      if (url.includes("/vapid-public-key")) {
        return new Response(JSON.stringify({ public_key: "QUJD" }), { status: overrides.vapid ?? 200 });
      }
      const status = url.includes("/subscribe") ? overrides.subscribe : url.includes("/test") ? overrides.test : 200;
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
    ["a failing auto-heal re-register", () => installFetch({ subscribe: 500 }), { kind: "enabled" }],
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
    ["PushManager disappears", removePushManager, unsupported("no-api")],
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

describe("usePushSubscription refresh() and resubscribe()", () => {
  it("refresh re-evaluates state on demand", async () => {
    noSubscription();
    const { result } = await mountAndSettle();
    expect(result.current.state).toEqual({ kind: "off" });
    currentSub = makeSubscription();
    expect(await act_(result, "refresh")).toEqual({ kind: "enabled" });
  });

  it("resubscribe disables then enables", async () => {
    const { result } = await mountAndSettle();
    calls.length = 0;
    expect(await act_(result, "resubscribe")).toEqual({ kind: "enabled" });
    expect(called("/api/push/unsubscribe") && called("/api/push/subscribe")).toBe(true);
  });
});
