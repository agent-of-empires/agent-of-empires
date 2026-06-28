// Unit coverage for the push/clear contract in web/public/sw.js (#2491).
// The service worker is a plain classic worker file, not a module, so we
// load its source into a fresh VM context with the small slice of the SW
// global surface it touches (self, registration, clients) stubbed, then
// dispatch synthetic push events and await their waitUntil promises.

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
  // install/activate handlers register here but only run if dispatched; we
  // only dispatch "push", so caches/skipWaiting are never exercised.
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

  it("closes matching notifications on a clear and shows nothing", async () => {
    const { handlers, showNotification, getNotifications, matchAll } = loadSw();
    const closeA = vi.fn();
    const closeB = vi.fn();
    getNotifications.mockResolvedValue([
      { data: { seq: 5 }, close: closeA },
      { data: { seq: 6 }, close: closeB },
    ]);
    await dispatchPush(handlers, { kind: "clear", tag: APPROVAL_TAG, seq: 10 });
    expect(getNotifications).toHaveBeenCalledWith({ tag: APPROVAL_TAG });
    expect(closeA).toHaveBeenCalled();
    expect(closeB).toHaveBeenCalled();
    expect(showNotification).not.toHaveBeenCalled();
    // clear is not focus-gated, so it never inspects clients.
    expect(matchAll).not.toHaveBeenCalled();
  });

  it("does not close a newer notification than the clear's seq", async () => {
    const { handlers, getNotifications } = loadSw();
    const closeNewer = vi.fn();
    const closeOlder = vi.fn();
    getNotifications.mockResolvedValue([
      { data: { seq: 20 }, close: closeNewer },
      { data: { seq: 5 }, close: closeOlder },
    ]);
    await dispatchPush(handlers, { kind: "clear", tag: APPROVAL_TAG, seq: 10 });
    expect(closeOlder).toHaveBeenCalled();
    expect(closeNewer).not.toHaveBeenCalled();
  });

  it("still shows a fresh notification after a clear for the same tag", async () => {
    const { handlers, showNotification, getNotifications } = loadSw();
    getNotifications.mockResolvedValue([{ data: { seq: 1 }, close: vi.fn() }]);
    await dispatchPush(handlers, { kind: "clear", tag: APPROVAL_TAG, seq: 1 });
    await dispatchPush(handlers, { kind: "notify", title: "new", tag: APPROVAL_TAG, seq: 2 });
    expect(showNotification).toHaveBeenCalledTimes(1);
    expect(showNotification.mock.calls[0][1].tag).toBe(APPROVAL_TAG);
  });

  it("clears only the targeted (approval) tag, not the question tag", async () => {
    const { handlers, getNotifications } = loadSw();
    await dispatchPush(handlers, { kind: "clear", tag: APPROVAL_TAG, seq: 1 });
    expect(getNotifications).toHaveBeenCalledWith({ tag: APPROVAL_TAG });
    expect(getNotifications).not.toHaveBeenCalledWith({ tag: QUESTION_TAG });
  });

  it("drops an older notify delivered after a newer clear for the same tag", async () => {
    // Out-of-order delivery: the clear arrives first, so getNotifications sees
    // nothing to close, then the stale notify lands. Without a high-water mark
    // it would resurrect a handled request's notification. See #2491.
    const { handlers, showNotification } = loadSw();
    await dispatchPush(handlers, { kind: "clear", tag: APPROVAL_TAG, seq: 10 });
    await dispatchPush(handlers, {
      kind: "notify",
      title: "stale",
      tag: APPROVAL_TAG,
      seq: 5,
    });
    expect(showNotification).not.toHaveBeenCalled();
  });

  it("ignores a clear with no tag without throwing or showing", async () => {
    const { handlers, showNotification, getNotifications } = loadSw();
    await dispatchPush(handlers, { kind: "clear", seq: 1 });
    expect(getNotifications).not.toHaveBeenCalled();
    expect(showNotification).not.toHaveBeenCalled();
  });
});
