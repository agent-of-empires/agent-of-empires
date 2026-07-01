// Coverage for the trash/restore action loops (#2489): apply each snapshot,
// flag failures via onError, and toast the aggregate result. The api calls
// are mocked so the test exercises only the loop + notify branches.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api", () => ({
  trashSession: vi.fn(),
  restoreSession: vi.fn(),
  deleteSession: vi.fn(),
}));

import { deleteSession, restoreSession, trashSession } from "../api";
import { deleteWorkspaceSessions, restoreSessions, trashedWorkspaceRestoreIds, trashSessions } from "../trashActions";
import type { SessionResponse, Workspace } from "../types";

const snap = (id: string) => ({ id, title: id }) as unknown as SessionResponse;
const ws = (id: string, sessionIds: string[]) =>
  ({ id, sessions: sessionIds.map((sid) => ({ id: sid })) }) as unknown as Workspace;
const trashMock = vi.mocked(trashSession);
const restoreMock = vi.mocked(restoreSession);
const deleteMock = vi.mocked(deleteSession);

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

describe("deleteWorkspaceSessions (#2530, #2539)", () => {
  const ok = (messages?: string[]) => ({ ok: true as const, messages });
  const sessions = (...ids: string[]) => ids.map((id) => ({ id }) as unknown as SessionResponse);
  const deps = () => ({
    setStatus: vi.fn(),
    purgeLocal: vi.fn(),
    navigateHome: vi.fn(),
    notify: { info: vi.fn(), error: vi.fn() },
  });

  it("deletes every session; the primary carries the chosen cleanup, siblings strip worktree/branch", async () => {
    deleteMock.mockResolvedValue(ok());
    const d = deps();

    await deleteWorkspaceSessions(sessions("a", "b", "c"), { delete_worktree: true, delete_branch: true }, null, d);

    expect(deleteMock).toHaveBeenCalledTimes(3);
    expect(deleteMock).toHaveBeenNthCalledWith(1, "a", { delete_worktree: true, delete_branch: true });
    expect(deleteMock).toHaveBeenNthCalledWith(2, "b", { delete_worktree: false, delete_branch: false });
    expect(deleteMock).toHaveBeenNthCalledWith(3, "c", { delete_worktree: false, delete_branch: false });
    expect(d.purgeLocal).toHaveBeenCalledTimes(3);
    expect(d.notify.info).toHaveBeenCalledWith("Sessions deleted");
    expect(d.navigateHome).not.toHaveBeenCalled();
  });

  it("aborts the siblings and does not navigate when the primary delete fails", async () => {
    deleteMock.mockResolvedValueOnce({ ok: false, error: "dirty" });
    const d = deps();

    await deleteWorkspaceSessions(sessions("a", "b"), {}, "b", d);

    expect(deleteMock).toHaveBeenCalledTimes(1);
    expect(d.purgeLocal).not.toHaveBeenCalled();
    expect(d.navigateHome).not.toHaveBeenCalled();
    expect(d.setStatus).toHaveBeenCalledWith("a", "Error");
    expect(d.setStatus).toHaveBeenCalledWith("b", "Error");
    expect(d.notify.error).toHaveBeenCalledWith("dirty");
  });

  it("navigates home only after the open session is actually deleted (primary)", async () => {
    deleteMock.mockResolvedValue(ok());
    const d = deps();

    await deleteWorkspaceSessions(sessions("a", "b"), {}, "a", d);

    expect(d.navigateHome).toHaveBeenCalledTimes(1);
  });

  it("navigates home when the open session is a deleted sibling", async () => {
    deleteMock.mockResolvedValue(ok());
    const d = deps();

    await deleteWorkspaceSessions(sessions("a", "b"), {}, "b", d);

    expect(d.navigateHome).toHaveBeenCalledTimes(1);
  });

  it("reports a partial failure and skips purge for the failed sibling", async () => {
    deleteMock.mockResolvedValueOnce(ok()).mockResolvedValueOnce({ ok: false, error: "boom" });
    const d = deps();

    await deleteWorkspaceSessions(sessions("a", "b"), {}, null, d);

    expect(d.purgeLocal).toHaveBeenCalledTimes(1);
    expect(d.purgeLocal).toHaveBeenCalledWith("a");
    expect(d.setStatus).toHaveBeenCalledWith("b", "Error");
    expect(d.notify.error).toHaveBeenCalledWith("Some sessions could not be deleted");
  });

  it("surfaces a server message and handles a single-session workspace", async () => {
    deleteMock.mockResolvedValue(ok(["Scratch directory kept at: /tmp/x"]));
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
