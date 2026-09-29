// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const createSession = vi.fn();
vi.mock("./api", () => ({ createSession: (...args: unknown[]) => createSession(...args) }));

// Storage writes can fail (quota, disabled storage); the flag breaks them on demand.
const storage = vi.hoisted(() => ({ broken: false }));
vi.mock("./safeStorage", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./safeStorage")>();
  return {
    ...actual,
    safeSetItem: (key: string, value: string) => (storage.broken ? false : actual.safeSetItem(key, value)),
  };
});

const pending = (key: string, since = Date.now()) => ({
  body: { path: "/tmp/p", tool: "claude", idempotency_key: key },
  tool: "claude",
  since,
});

beforeEach(() => {
  storage.broken = false;
  localStorage.clear();
  createSession.mockReset();
  vi.resetModules();
});
afterEach(() => localStorage.clear());

describe("pendingCreates", () => {
  it("resumes a create left by an earlier page under its original key", async () => {
    createSession.mockResolvedValue({ ok: false, error: "offline", network: true });
    const first = await import("./pendingCreates");
    first.registerPendingCreate(pending("k-reload"));
    await vi.waitFor(() => expect(createSession).toHaveBeenCalled());

    // A reload: fresh module state, the same storage.
    vi.resetModules();
    createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
    const onCreated = vi.fn();
    const second = await import("./pendingCreates");
    second.startPendingCreates({ onCreated, onFailed: vi.fn() });
    await vi.waitFor(() => expect(onCreated).toHaveBeenCalledWith({ id: "s1" }, expect.anything()));
    expect(createSession.mock.calls.every(([body]) => body.idempotency_key === "k-reload")).toBe(true);
    expect(second.peekPendingCreate()).toBeNull();
  });

  it("reports a definite failure, and drops a create older than the server's replay window", async () => {
    const { startPendingCreates, registerPendingCreate, peekPendingCreate, PENDING_CREATE_MAX_AGE_MS } =
      await import("./pendingCreates");
    const onFailed = vi.fn();
    startPendingCreates({ onCreated: vi.fn(), onFailed });
    createSession.mockResolvedValue({ ok: false, error: "hook failed" });
    registerPendingCreate(pending("k-fail"));
    await vi.waitFor(() => expect(onFailed).toHaveBeenCalledWith("hook failed", expect.anything()));

    localStorage.setItem(
      "aoe-pending-creates",
      JSON.stringify([pending("k-old", Date.now() - PENDING_CREATE_MAX_AGE_MS - 1)]),
    );
    expect(peekPendingCreate()).toBeNull();
  });

  it("a claimed create stops the owner's retries", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      createSession.mockResolvedValue({ ok: false, error: "offline", network: true });
      const { registerPendingCreate, claimPendingCreate, peekPendingCreate } = await import("./pendingCreates");
      registerPendingCreate(pending("k-claim"));
      await vi.advanceTimersByTimeAsync(5_000);
      const before = createSession.mock.calls.length;
      expect(before).toBeGreaterThan(1);
      claimPendingCreate("k-claim");
      await vi.advanceTimersByTimeAsync(5 * 60_000);
      expect(createSession.mock.calls.length).toBeLessThanOrEqual(before + 1);
      expect(peekPendingCreate()).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("drops corrupt stored entries rather than sending them", async () => {
    const good = pending("k-good");
    localStorage.setItem(
      "aoe-pending-creates",
      JSON.stringify([
        { ...pending("k-string-since"), since: String(Date.now()) },
        { ...pending("k-no-path"), body: { tool: "claude", idempotency_key: "k-no-path" } },
        { body: { idempotency_key: "k-bare" } },
        good,
      ]),
    );
    const { peekPendingCreate, claimPendingCreate } = await import("./pendingCreates");
    expect(peekPendingCreate()?.body.idempotency_key).toBe("k-good");
    claimPendingCreate("k-good");
    expect(peekPendingCreate()).toBeNull();
  });

  it.each([
    ["while its page stays open", true],
    ["while no page was open", false],
  ])("reports a create that ages out %s", async (_label, tracked) => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      createSession.mockResolvedValue({ ok: false, error: "offline", network: true });
      const mod = await import("./pendingCreates");
      const onFailed = vi.fn();
      const nearlyExpired = pending("k-expire", Date.now() - mod.PENDING_CREATE_MAX_AGE_MS + 2_000);
      if (tracked) {
        mod.startPendingCreates({ onCreated: vi.fn(), onFailed });
        mod.registerPendingCreate(nearlyExpired);
      } else {
        localStorage.setItem("aoe-pending-creates", JSON.stringify([nearlyExpired]));
        await vi.advanceTimersByTimeAsync(5_000);
        mod.startPendingCreates({ onCreated: vi.fn(), onFailed });
      }
      await vi.advanceTimersByTimeAsync(10_000);
      expect(onFailed).toHaveBeenCalledWith(mod.PENDING_CREATE_EXPIRED_MESSAGE, expect.anything());
      expect(localStorage.getItem("aoe-pending-creates")).toBe("[]");
    } finally {
      vi.useRealTimers();
    }
  });

  it.each([
    ["adopted by a reopened wizard", false],
    ["sent by a wizard, still in flight", true],
  ])("keeps a create %s across a reload before its response", async (_label, inFlight) => {
    createSession.mockResolvedValue({ ok: false, error: "offline", network: true });
    const first = await import("./pendingCreates");
    if (inFlight) {
      first.registerPendingCreate(pending("k-kept"), { claimed: true });
    } else {
      first.registerPendingCreate(pending("k-kept"));
      first.claimPendingCreate(first.peekPendingCreate()!.body.idempotency_key);
    }

    // The reload comes before any definite answer.
    vi.resetModules();
    createSession.mockReset();
    createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
    const onCreated = vi.fn();
    const second = await import("./pendingCreates");
    second.startPendingCreates({ onCreated, onFailed: vi.fn() });
    await vi.waitFor(() => expect(onCreated).toHaveBeenCalledWith({ id: "s1" }, expect.anything()));
    expect(createSession.mock.calls.map(([body]) => body.idempotency_key)).toEqual(["k-kept"]);
  });

  it("keeps retrying, and stays adoptable, when storage refuses the record", async () => {
    storage.broken = true;
    createSession.mockResolvedValue({ ok: false, error: "offline", network: true });
    const mod = await import("./pendingCreates");
    const onUnsaved = vi.fn();
    const onCreated = vi.fn();
    mod.startPendingCreates({ onCreated, onFailed: vi.fn(), onUnsaved });
    mod.registerPendingCreate(pending("k-unsaved"));
    expect(onUnsaved).toHaveBeenCalledTimes(1);
    expect(mod.peekPendingCreate()?.body.idempotency_key).toBe("k-unsaved");

    createSession.mockResolvedValue({ ok: true, session: { id: "s1" } });
    await vi.waitFor(() => expect(onCreated).toHaveBeenCalledWith({ id: "s1" }, expect.anything()), {
      timeout: 5000,
    });
    // Retried under its own key (loops left by earlier tests' module copies share the mock).
    expect(createSession.mock.calls.filter(([body]) => body.idempotency_key === "k-unsaved").length).toBeGreaterThan(1);
  });
});
