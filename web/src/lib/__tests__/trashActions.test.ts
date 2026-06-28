// Coverage for the trash/restore action loops (#2489): apply each snapshot,
// flag failures via onError, and toast the aggregate result. The api calls
// are mocked so the test exercises only the loop + notify branches.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api", () => ({
  trashSession: vi.fn(),
  restoreSession: vi.fn(),
}));

import { restoreSession, trashSession } from "../api";
import { restoreSessions, trashSessions } from "../trashActions";
import type { SessionResponse } from "../types";

const snap = (id: string) => ({ id, title: id }) as unknown as SessionResponse;
const trashMock = vi.mocked(trashSession);
const restoreMock = vi.mocked(restoreSession);

beforeEach(() => {
  trashMock.mockReset();
  restoreMock.mockReset();
});

afterEach(() => vi.clearAllMocks());

describe("trashSessions (#2489)", () => {
  it("applies every snapshot and toasts success when all succeed", async () => {
    trashMock.mockImplementation(async (id: string) => snap(id));
    const applySession = vi.fn();
    const onError = vi.fn();
    const notify = { info: vi.fn(), error: vi.fn() };

    const ok = await trashSessions(["a", "b"], { applySession, onError, notify });

    expect(ok).toBe(true);
    expect(applySession).toHaveBeenCalledTimes(2);
    expect(onError).not.toHaveBeenCalled();
    expect(notify.info).toHaveBeenCalledWith("Moved to trash");
    expect(notify.error).not.toHaveBeenCalled();
  });

  it("flags failures, toasts error, and returns false", async () => {
    trashMock.mockImplementation(async (id: string) => (id === "bad" ? null : snap(id)));
    const applySession = vi.fn();
    const onError = vi.fn();
    const notify = { info: vi.fn(), error: vi.fn() };

    const ok = await trashSessions(["good", "bad"], { applySession, onError, notify });

    expect(ok).toBe(false);
    expect(applySession).toHaveBeenCalledTimes(1);
    expect(onError).toHaveBeenCalledWith("bad");
    expect(notify.error).toHaveBeenCalledWith("Failed to move session to trash");
  });

  it("tolerates a null notifier", async () => {
    trashMock.mockResolvedValue(snap("a"));
    await expect(trashSessions(["a"], { applySession: vi.fn(), onError: vi.fn(), notify: null })).resolves.toBe(true);
  });
});

describe("restoreSessions (#2489)", () => {
  it("applies every snapshot and toasts success", async () => {
    restoreMock.mockImplementation(async (id: string) => snap(id));
    const applySession = vi.fn();
    const notify = { info: vi.fn(), error: vi.fn() };

    const ok = await restoreSessions(["a", "b"], { applySession, notify });

    expect(ok).toBe(true);
    expect(applySession).toHaveBeenCalledTimes(2);
    expect(notify.info).toHaveBeenCalledWith("Session restored");
  });

  it("toasts error and returns false when any restore fails", async () => {
    restoreMock.mockResolvedValue(null);
    const notify = { info: vi.fn(), error: vi.fn() };

    const ok = await restoreSessions(["a"], { applySession: vi.fn(), notify });

    expect(ok).toBe(false);
    expect(notify.error).toHaveBeenCalledWith("Failed to restore session");
  });

  it("tolerates a null notifier", async () => {
    restoreMock.mockResolvedValue(snap("a"));
    await expect(restoreSessions(["a"], { applySession: vi.fn(), notify: null })).resolves.toBe(true);
  });
});
