// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const createSession = vi.fn();
vi.mock("./api", () => ({ createSession: (...args: unknown[]) => createSession(...args) }));

const pending = (key: string, since = Date.now()) => ({
  body: { path: "/tmp/p", tool: "claude", idempotency_key: key },
  tool: "claude",
  since,
});

beforeEach(() => {
  localStorage.clear();
  createSession.mockReset();
  vi.resetModules();
});
afterEach(() => localStorage.clear());

describe("pendingCreates", () => {
  it("resumes a create left by an earlier page under its original key", async () => {
    createSession.mockResolvedValue({ ok: false, error: "offline", network: true });
    const first = await import("./pendingCreates");
    first.trackPendingCreate(pending("k-reload"));
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
    const { startPendingCreates, trackPendingCreate, peekPendingCreate, PENDING_CREATE_MAX_AGE_MS } =
      await import("./pendingCreates");
    const onFailed = vi.fn();
    startPendingCreates({ onCreated: vi.fn(), onFailed });
    createSession.mockResolvedValue({ ok: false, error: "hook failed" });
    trackPendingCreate(pending("k-fail"));
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
      const { trackPendingCreate, claimPendingCreate, peekPendingCreate } = await import("./pendingCreates");
      trackPendingCreate(pending("k-claim"));
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
        mod.trackPendingCreate(nearlyExpired);
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
});
