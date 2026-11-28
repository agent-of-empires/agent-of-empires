import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

import { describe, expect, it, vi } from "vitest";

const swPath = fileURLToPath(new URL("../../public/sw.js", import.meta.url));
const swSource = readFileSync(swPath, "utf8");

type Handler = (event: unknown) => void;

function loadSw() {
  const handlers = new Map<string, Handler[]>();
  const showNotification = vi.fn();
  const getNotifications = vi.fn().mockResolvedValue([]);
  const matchAll = vi.fn().mockResolvedValue([]);
  const getSubscription = vi.fn().mockResolvedValue({ endpoint: "https://fcm.googleapis.com/fcm/send/x" });
  const self = {
    addEventListener: (type: string, fn: Handler) => {
      const list = handlers.get(type) ?? [];
      list.push(fn);
      handlers.set(type, list);
    },
    skipWaiting: vi.fn(),
    location: { origin: "https://aoe.test" },
    clients: { matchAll, claim: vi.fn(), openWindow: vi.fn() },
    registration: { showNotification, getNotifications, pushManager: { getSubscription } },
  };
  vm.runInNewContext(swSource, { self, URL, caches: { keys: vi.fn(), delete: vi.fn() } });
  return {
    handlers,
    showNotification,
    getNotifications,
    matchAll,
    getSubscription,
    openWindow: self.clients.openWindow,
  };
}

async function dispatchPush(handlers: Map<string, Handler[]>, payload: unknown) {
  const pending: Promise<unknown>[] = [];
  const event = {
    data: { json: () => payload, text: () => JSON.stringify(payload) },
    waitUntil: (p: Promise<unknown>) => pending.push(Promise.resolve(p)),
  };
  for (const fn of handlers.get("push") ?? []) fn(event);
  await Promise.all(pending);
}

const APPROVAL_TAG = "acp-approval-s1";
const QUESTION_TAG = "acp-question-s1";

describe("service worker push handler (#2491)", () => {
  it("shows a notification for a normal payload and stores tag + seq", async () => {
    const { handlers, showNotification, getNotifications } = loadSw();
    await dispatchPush(handlers, {
      kind: "notify",
      title: "needs approval",
      body: "Bash",
      url: "/sessions/s1/acp",
      tag: APPROVAL_TAG,
      seq: 5,
    });
    expect(showNotification).toHaveBeenCalledTimes(1);
    const [title, options] = showNotification.mock.calls[0];
    expect(title).toBe("needs approval");
    expect(options.tag).toBe(APPROVAL_TAG);
    expect(options.data).toMatchObject({ tag: APPROVAL_TAG, seq: 5, url: "/sessions/s1/acp" });
    expect(getNotifications).not.toHaveBeenCalled();
  });

  // WebKit revokes subscriptions whose pushes show nothing, so Apple endpoints always show.
  it.each<[string, unknown, boolean]>([
    ["an FCM subscription", { endpoint: "https://fcm.googleapis.com/fcm/send/x" }, false],
    ["an Apple subscription", { endpoint: "https://web.push.apple.com/QGuQ" }, true],
    ["no readable subscription", null, true],
  ])("with a focused client and %s", async (_label, sub, shows) => {
    const { handlers, showNotification, matchAll, getSubscription } = loadSw();
    getSubscription.mockResolvedValue(sub);
    const postMessage = vi.fn();
    matchAll.mockResolvedValue([{ visibilityState: "visible", focused: true, postMessage }]);
    const payload = { kind: "notify", title: "t", tag: APPROVAL_TAG, seq: 1 };
    await dispatchPush(handlers, payload);
    expect(showNotification).toHaveBeenCalledTimes(shows ? 1 : 0);
    expect(postMessage.mock.calls).toEqual(shows ? [] : [[{ type: "aoe-push", payload }]]);
  });

  it.each<[string, string, string, boolean]>([
    ["its own session", "/session/s1?focus=1", "s1", false],
    ["another session", "/session/s1?focus=1", "s2", true],
    // The app routes a trailing slash to the same session, so the worker must scope it the same way.
    ["another session, trailing slash", "/session/s1/?focus=1", "s2", true],
    ["its own session, trailing slash", "/session/s1/?focus=1", "s1", false],
  ])("a focused focus-mode window toasts only pushes about its session: %s", async (_label, path, sessionId, shows) => {
    const { handlers, showNotification, matchAll } = loadSw();
    const postMessage = vi.fn();
    matchAll.mockResolvedValue([
      { url: `https://aoe.test${path}`, visibilityState: "visible", focused: true, postMessage },
    ]);
    const payload = { kind: "notify", title: "t", tag: `session-${sessionId}`, session_id: sessionId };
    await dispatchPush(handlers, payload);
    expect(showNotification).toHaveBeenCalledTimes(shows ? 1 : 0);
    expect(postMessage).toHaveBeenCalledTimes(shows ? 0 : 1);
  });

  it("closes only notifications older than the clear's seq, for the cleared tag alone", async () => {
    const { handlers, showNotification, getNotifications, matchAll } = loadSw();
    const closes = [vi.fn(), vi.fn(), vi.fn()];
    getNotifications.mockResolvedValue([5, 6, 20].map((seq, i) => ({ data: { seq }, close: closes[i] })));
    await dispatchPush(handlers, { kind: "clear", tag: APPROVAL_TAG, seq: 10 });
    expect(getNotifications).toHaveBeenCalledWith({ tag: APPROVAL_TAG });
    expect(getNotifications).not.toHaveBeenCalledWith({ tag: QUESTION_TAG });
    expect(closes.map((c) => c.mock.calls.length)).toEqual([1, 1, 0]);
    expect(showNotification).not.toHaveBeenCalled();
    expect(matchAll).not.toHaveBeenCalled();
  });

  it("drops a notify older than the last clear but still shows a newer one", async () => {
    const { handlers, showNotification } = loadSw();
    await dispatchPush(handlers, { kind: "clear", tag: APPROVAL_TAG, seq: 10 });
    await dispatchPush(handlers, { kind: "notify", title: "stale", tag: APPROVAL_TAG, seq: 5 });
    expect(showNotification).not.toHaveBeenCalled();

    await dispatchPush(handlers, { kind: "notify", title: "new", tag: APPROVAL_TAG, seq: 11 });
    expect(showNotification).toHaveBeenCalledTimes(1);
    expect(showNotification.mock.calls[0][1].tag).toBe(APPROVAL_TAG);
  });

  it("on Apple shows a stale notify under its own tag and closes it, so the push is not silent", async () => {
    const { handlers, showNotification, getNotifications, getSubscription } = loadSw();
    getSubscription.mockResolvedValue({ endpoint: "https://web.push.apple.com/QGuQ" });
    const close = vi.fn();
    getNotifications.mockResolvedValue([{ close }]);
    await dispatchPush(handlers, { kind: "notify", title: "new", tag: APPROVAL_TAG, seq: 11 });
    showNotification.mockClear();

    await dispatchPush(handlers, { kind: "notify", title: "stale", tag: APPROVAL_TAG, seq: 5 });
    expect(showNotification).toHaveBeenCalledTimes(1);
    expect(showNotification.mock.calls[0][1].tag).toBe(`${APPROVAL_TAG}:stale`);
    expect(getNotifications).toHaveBeenLastCalledWith({ tag: `${APPROVAL_TAG}:stale` });
    expect(close).toHaveBeenCalled();
  });

  it("ignores a clear with no tag without throwing or showing", async () => {
    const { handlers, showNotification, getNotifications } = loadSw();
    await dispatchPush(handlers, { kind: "clear", seq: 1 });
    expect(getNotifications).not.toHaveBeenCalled();
    expect(showNotification).not.toHaveBeenCalled();
  });
});

describe("service worker notification click with focus-mode windows", () => {
  type FakeClient = { url: string; focus: ReturnType<typeof vi.fn>; navigate: ReturnType<typeof vi.fn> };
  const client = (path: string): FakeClient => ({
    url: `https://aoe.test${path}`,
    focus: vi.fn(),
    navigate: vi.fn().mockResolvedValue(undefined),
  });

  async function click(handlers: Map<string, Handler[]>, url: string) {
    const pending: Promise<unknown>[] = [];
    const event = {
      notification: { close: vi.fn(), data: { url } },
      waitUntil: (p: Promise<unknown>) => pending.push(Promise.resolve(p)),
    };
    for (const fn of handlers.get("notificationclick") ?? []) fn(event);
    await Promise.all(pending);
  }

  it.each<[string, string[], number | "open"]>([
    ["focuses the focus window already showing the session", ["/session/s2?focus=1", "/session/s1?focus=1"], 1],
    ["focuses it through a trailing slash too", ["/session/s2/?focus=1", "/session/s1/?focus=1"], 1],
    ["focuses it through a mixed-case path, as the app routes it", ["/session/s2?focus=1", "/Session/s1?focus=1"], 1],
    ["navigates a regular window, never another session's focus window", ["/session/s2?focus=1", "/"], 1],
    ["opens a window when only other focus windows exist", ["/session/s2?focus=1"], "open"],
  ])("%s", async (_label, paths, expected) => {
    const { handlers, matchAll, openWindow } = loadSw();
    const clients = paths.map(client);
    matchAll.mockResolvedValue(clients);
    await click(handlers, "https://aoe.test/session/s1");

    expect(clients[0]!.navigate).not.toHaveBeenCalled();
    expect(clients[0]!.focus).not.toHaveBeenCalled();
    if (expected === "open") {
      expect(openWindow).toHaveBeenCalledWith("https://aoe.test/session/s1");
    } else {
      expect(clients[expected]!.focus).toHaveBeenCalled();
      expect(openWindow).not.toHaveBeenCalled();
    }
  });
});

describe("service worker offline shell", () => {
  function loadFetchSw(fetchImpl: (request: unknown) => Promise<Response>) {
    const handlers = new Map<string, Handler[]>();
    const stores = new Map<string, Map<string, Response>>();
    const store = (name: string) => {
      if (!stores.has(name)) stores.set(name, new Map());
      const entries = stores.get(name)!;
      const key = (r: unknown) => (typeof r === "string" ? r : new URL((r as { url: string }).url).pathname);
      return {
        match: async (r: unknown) => entries.get(key(r))?.clone(),
        put: async (r: unknown, res: Response) => void entries.set(key(r), res),
        keys: async () => [...entries.keys()],
        delete: async (r: unknown) => entries.delete(key(r)),
      };
    };
    const caches = {
      open: vi.fn(async (name: string) => store(name)),
      match: async (r: string, { cacheName }: { cacheName: string }) => store(cacheName).match(r),
      keys: vi.fn(),
      delete: vi.fn(),
    };
    const fetch = vi.fn(fetchImpl);
    const self = {
      addEventListener: (type: string, fn: Handler) => handlers.set(type, [...(handlers.get(type) ?? []), fn]),
      location: { origin: "https://aoe.test" },
    };
    vm.runInNewContext(swSource, { self, URL, Response, caches, fetch });
    const dispatch = (path: string, init: { mode?: string; destination?: string; method?: string } = {}) => {
      let responded: Promise<Response> | undefined;
      const request = {
        method: "GET",
        mode: "cors",
        destination: "",
        ...init,
        url: new URL(path, "https://aoe.test").href,
      };
      for (const fn of handlers.get("fetch") ?? [])
        fn({ request, respondWith: (p: Promise<Response>) => (responded = p) });
      return responded;
    };
    return { dispatch, fetch, caches, stores };
  }
  const html = (body: string) => new Response(body, { headers: { "content-type": "text/html" } });
  const page = { mode: "navigate", destination: "document" };

  it("keeps the last page and serves it, or the offline page, when the network fails", async () => {
    let online = true;
    const sw = loadFetchSw(async () => (online ? html("app") : Promise.reject(new TypeError("offline"))));
    expect(await (await sw.dispatch("/session/s1", page))!.text()).toBe("app");
    await vi.waitFor(() => expect(sw.stores.get("aoe-shell-v1")?.has("/index.html")).toBe(true));
    online = false;
    expect(await (await sw.dispatch("/", page))!.text()).toBe("app");

    const fresh = loadFetchSw(async () => Promise.reject(new TypeError("offline")));
    const fallback = (await fresh.dispatch("/", page))!;
    expect(fallback.headers.get("content-type")).toContain("text/html");
    expect(await fallback.text()).toContain("Try again");
  });

  it("serves a cached asset without the network, and a Cache Storage failure never fails a fetched one", async () => {
    const sw = loadFetchSw(async () => new Response("js"));
    expect(await (await sw.dispatch("/assets/a.js"))!.text()).toBe("js");
    await vi.waitFor(() => expect(sw.stores.get("aoe-assets-v1")?.has("/assets/a.js")).toBe(true));
    expect(await (await sw.dispatch("/assets/a.js"))!.text()).toBe("js");
    expect(sw.fetch).toHaveBeenCalledTimes(1);

    sw.caches.open.mockRejectedValue(new Error("quota"));
    expect(await (await sw.dispatch("/assets/b.js"))!.text()).toBe("js");
  });

  it.each<[string, string, { mode?: string; destination?: string; method?: string }]>([
    ["a plugin page frame", "/api/plugins/p/pages/x", { mode: "navigate", destination: "iframe" }],
    ["an API page opened as a document", "/api/plugins/p/pages/x", page],
    ["an API request", "/api/sessions", {}],
    ["a non-GET asset request", "/assets/a.js", { method: "POST" }],
    ["another origin's page", "https://elsewhere.test/", page],
  ])("leaves %s to the network", (_label, path, init) => {
    const sw = loadFetchSw(async () => html("x"));
    expect(sw.dispatch(path, init)).toBeUndefined();
  });
});
