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
  const self = {
    addEventListener: (type: string, fn: Handler) => {
      const list = handlers.get(type) ?? [];
      list.push(fn);
      handlers.set(type, list);
    },
    skipWaiting: vi.fn(),
    location: { origin: "https://aoe.test" },
    clients: { matchAll, claim: vi.fn(), openWindow: vi.fn() },
    registration: { showNotification, getNotifications },
  };
  vm.runInNewContext(swSource, { self, caches: { keys: vi.fn(), delete: vi.fn() } });
  return { handlers, showNotification, getNotifications, matchAll };
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

  it("forwards a normal payload to a focused client instead of showing", async () => {
    const { handlers, showNotification, matchAll } = loadSw();
    const postMessage = vi.fn();
    matchAll.mockResolvedValue([{ visibilityState: "visible", focused: true, postMessage }]);
    const payload = { kind: "notify", title: "t", tag: APPROVAL_TAG, seq: 1 };
    await dispatchPush(handlers, payload);
    expect(postMessage).toHaveBeenCalledWith({ type: "aoe-push", payload });
    expect(showNotification).not.toHaveBeenCalled();
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

  it("ignores a clear with no tag without throwing or showing", async () => {
    const { handlers, showNotification, getNotifications } = loadSw();
    await dispatchPush(handlers, { kind: "clear", seq: 1 });
    expect(getNotifications).not.toHaveBeenCalled();
    expect(showNotification).not.toHaveBeenCalled();
  });
});
