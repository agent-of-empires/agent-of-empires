// Coverage for the trash/restore action loops (#2489): apply each snapshot,
// flag failures via onError, and toast the aggregate result. The api calls
// are mocked so the test exercises only the loop + notify branches.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api", () => ({
  trashSession: vi.fn(),
  restoreSession: vi.fn(),
  deleteWorkspace: vi.fn(),
}));

import { deleteWorkspace, restoreSession, trashSession } from "../api";
import { deleteWorkspaceSessions, restoreSessions, trashedWorkspaceRestoreIds, trashSessions } from "../trashActions";
import type { SessionResponse, Workspace } from "../types";

const snap = (id: string) => ({ id, title: id }) as unknown as SessionResponse;
const ws = (id: string, sessionIds: string[]) =>
  ({ id, sessions: sessionIds.map((sid) => ({ id: sid })) }) as unknown as Workspace;
const trashMock = vi.mocked(trashSession);
const restoreMock = vi.mocked(restoreSession);
const deleteMock = vi.mocked(deleteWorkspace);

beforeEach(() => {
  trashMock.mockReset();
  restoreMock.mockReset();
  deleteMock.mockReset();
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

describe("trashedWorkspaceRestoreIds (#2593)", () => {
  it("returns every session id in the workspace containing the session", () => {
    const workspaces = [ws("w1", ["a", "b"]), ws("w2", ["c"])];
    expect(trashedWorkspaceRestoreIds(workspaces, "b")).toEqual(["a", "b"]);
  });

  it("falls back to just the session id when no workspace groups it", () => {
    expect(trashedWorkspaceRestoreIds([ws("w2", ["c"])], "orphan")).toEqual(["orphan"]);
  });
});

describe("deleteWorkspaceSessions (#2536)", () => {
  const ok = (over: { deleted?: string[]; failed?: { id: string; error: string }[]; messages?: string[] } = {}) => ({
    ok: true as const,
    ...over,
  });
  const sessions = (...ids: string[]) => ids.map((id) => ({ id }) as unknown as SessionResponse);
  const deps = () => ({
    setStatus: vi.fn(),
    purgeLocal: vi.fn(),
    navigateHome: vi.fn(),
    notify: { info: vi.fn(), error: vi.fn() },
  });

  it("makes ONE workspace call with the full id set in order, and purges every deleted id", async () => {
    deleteMock.mockResolvedValue(ok({ deleted: ["a", "b", "c"] }));
    const d = deps();

    await deleteWorkspaceSessions(sessions("a", "b", "c"), { delete_worktree: true, delete_branch: true }, null, d);

    expect(deleteMock).toHaveBeenCalledTimes(1);
    expect(deleteMock).toHaveBeenCalledWith(["a", "b", "c"], { delete_worktree: true, delete_branch: true });
    expect(d.purgeLocal).toHaveBeenCalledTimes(3);
    expect(d.notify.info).toHaveBeenCalledWith("Sessions deleted");
    expect(d.navigateHome).not.toHaveBeenCalled();
  });

  it("leaves a session that is neither deleted nor failed untouched (kept-restored)", async () => {
    // Server kept sess-b (a concurrent restore won the race): it is reported
    // in neither `deleted` nor `failed`, so we must not purge its local state
    // or flag it Error; the next poll reconciles it.
    deleteMock.mockResolvedValue(ok({ deleted: ["a"], failed: [] }));
    const d = deps();

    await deleteWorkspaceSessions(sessions("a", "b"), {}, null, d);

    expect(d.purgeLocal).toHaveBeenCalledTimes(1);
    expect(d.purgeLocal).toHaveBeenCalledWith("a");
    expect(d.setStatus).not.toHaveBeenCalledWith("b", "Error");
    expect(d.notify.info).toHaveBeenCalledWith("Sessions deleted");
  });

  it("flags every session Error and does not navigate when the call fails", async () => {
    deleteMock.mockResolvedValue({ ok: false, error: "dirty" });
    const d = deps();

    await deleteWorkspaceSessions(sessions("a", "b"), {}, "b", d);

    expect(d.purgeLocal).not.toHaveBeenCalled();
    expect(d.navigateHome).not.toHaveBeenCalled();
    expect(d.setStatus).toHaveBeenCalledWith("a", "Error");
    expect(d.setStatus).toHaveBeenCalledWith("b", "Error");
    expect(d.notify.error).toHaveBeenCalledWith("dirty");
  });

  it("navigates home when the open session is among the deleted", async () => {
    deleteMock.mockResolvedValue(ok({ deleted: ["a", "b"] }));
    const d = deps();

    await deleteWorkspaceSessions(sessions("a", "b"), {}, "b", d);

    expect(d.navigateHome).toHaveBeenCalledTimes(1);
  });

  it("does NOT navigate home when the open session is the one that failed", async () => {
    deleteMock.mockResolvedValue(ok({ deleted: ["a"], failed: [{ id: "b", error: "boom" }] }));
    const d = deps();

    await deleteWorkspaceSessions(sessions("a", "b"), {}, "b", d);

    expect(d.navigateHome).not.toHaveBeenCalled();
  });

  it("reports a partial failure: purges the deleted id, flags the failed one Error", async () => {
    deleteMock.mockResolvedValue(ok({ deleted: ["a"], failed: [{ id: "b", error: "boom" }] }));
    const d = deps();

    await deleteWorkspaceSessions(sessions("a", "b"), {}, null, d);

    expect(d.purgeLocal).toHaveBeenCalledTimes(1);
    expect(d.purgeLocal).toHaveBeenCalledWith("a");
    expect(d.setStatus).toHaveBeenCalledWith("b", "Error");
    expect(d.notify.error).toHaveBeenCalledWith("Some sessions could not be deleted");
  });

  it("surfaces a server message and handles a single-session workspace", async () => {
    deleteMock.mockResolvedValue(ok({ deleted: ["solo"], messages: ["Scratch directory kept at: /tmp/x"] }));
    const d = deps();

    await deleteWorkspaceSessions(sessions("solo"), {}, null, d);

    expect(deleteMock).toHaveBeenCalledTimes(1);
    expect(d.notify.info).toHaveBeenCalledWith("Scratch directory kept at: /tmp/x");
  });

  it("no-ops on an empty workspace", async () => {
    const d = deps();
    await deleteWorkspaceSessions([], {}, null, d);
    expect(deleteMock).not.toHaveBeenCalled();
  });
});
