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
});
