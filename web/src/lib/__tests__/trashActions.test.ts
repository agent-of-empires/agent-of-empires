import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../api", () => ({
  trashSession: vi.fn(),
  restoreSession: vi.fn(),
  deleteWorkspace: vi.fn(),
}));

import { deleteWorkspace, restoreSession, trashSession } from "../api";
import {
  deleteWorkspaceSessions,
  restoreSessions,
  sessionsSharingWorktree,
  trashedWorkspaceRestoreIds,
  trashSessions,
  workspaceCleanupDefaults,
} from "../trashActions";
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

describe("trash and restore loops (#2489)", () => {
  const notifier = () => ({ info: vi.fn(), error: vi.fn() });

  it("trashSessions applies snapshots, flags failures, and toasts the aggregate", async () => {
    trashMock.mockImplementation(async (id: string) => (id === "bad" ? null : snap(id)));
    const applySession = vi.fn();
    const onError = vi.fn();
    const notify = notifier();
    expect(await trashSessions(["a", "b"], { applySession, onError, notify })).toBe(true);
    expect(applySession).toHaveBeenCalledTimes(2);
    expect(notify.info).toHaveBeenCalledWith("Moved to trash");
    expect(await trashSessions(["good", "bad"], { applySession, onError, notify })).toBe(false);
    expect(applySession).toHaveBeenCalledTimes(3);
    expect(onError).toHaveBeenCalledExactlyOnceWith("bad");
    expect(notify.error).toHaveBeenCalledWith("Failed to move session to trash");
  });

  it("restoreSessions applies snapshots and toasts the aggregate", async () => {
    restoreMock.mockImplementation(async (id: string) => (id === "bad" ? null : snap(id)));
    const applySession = vi.fn();
    const notify = notifier();
    expect(await restoreSessions(["a", "b"], { applySession, notify })).toBe(true);
    expect(applySession).toHaveBeenCalledTimes(2);
    expect(notify.info).toHaveBeenCalledWith("Session restored");
    expect(await restoreSessions(["bad"], { applySession, notify })).toBe(false);
    expect(notify.error).toHaveBeenCalledWith("Failed to restore session");
  });

  it("tolerates a null notifier", async () => {
    trashMock.mockResolvedValue(snap("a"));
    restoreMock.mockResolvedValue(snap("a"));
    await expect(trashSessions(["a"], { applySession: vi.fn(), onError: vi.fn(), notify: null })).resolves.toBe(true);
    await expect(restoreSessions(["a"], { applySession: vi.fn(), notify: null })).resolves.toBe(true);
  });
});

describe("trashedWorkspaceRestoreIds (#2593)", () => {
  it("returns every session id in the containing workspace, else just the session id", () => {
    const workspaces = [ws("w1", ["a", "b"]), ws("w2", ["c"])];
    expect(trashedWorkspaceRestoreIds(workspaces, "b")).toEqual(["a", "b"]);
    expect(trashedWorkspaceRestoreIds(workspaces, "orphan")).toEqual(["orphan"]);
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

  it("navigates home only when the open session was deleted, not when it failed", async () => {
    for (const [result, navigations] of [
      [ok({ deleted: ["a", "b"] }), 1],
      [ok({ deleted: ["a"], failed: [{ id: "b", error: "boom" }] }), 0],
    ] as const) {
      deleteMock.mockResolvedValue(result);
      const d = deps();
      await deleteWorkspaceSessions(sessions("a", "b"), {}, "b", d);
      expect(d.navigateHome).toHaveBeenCalledTimes(navigations);
    }
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

describe("workspaceCleanupDefaults (#3167)", () => {
  const s = (over: Partial<SessionResponse>, worktree = false, branch = false, sandbox = false): SessionResponse =>
    ({
      has_cleanable_worktree: false,
      is_sandboxed: false,
      cleanup_defaults: { delete_worktree: worktree, delete_branch: branch, delete_sandbox: sandbox },
      ...over,
    }) as unknown as SessionResponse;
  const flags = (delete_worktree: boolean, delete_branch: boolean, delete_sandbox: boolean) => ({
    delete_worktree,
    delete_branch,
    delete_sandbox,
  });

  it.each<[string, SessionResponse[], ReturnType<typeof flags>]>([
    ["empty", [], flags(false, false, false)],
    ["no cleanable worktree suppresses worktree/branch", [s({}, true, true)], flags(false, false, false)],
    [
      "a cleanable worktree honors the defaults",
      [s({ has_cleanable_worktree: true }, true, true)],
      flags(true, true, false),
    ],
    ["sandbox gates on is_sandboxed", [s({ is_sandboxed: true }, false, false, true)], flags(false, false, true)],
    [
      "any session opting in flips the flag",
      [s({ has_cleanable_worktree: true }, true), s({ is_sandboxed: true }, false, false, true)],
      flags(true, false, true),
    ],
  ])("%s", (_name, sessions, expected) => {
    expect(workspaceCleanupDefaults(sessions)).toEqual(expected);
  });
});

describe("sessionsSharingWorktree (#4084)", () => {
  const at = (id: string, project_path: string, has_cleanable_worktree = false) =>
    ({ id, project_path, has_cleanable_worktree }) as unknown as SessionResponse;

  it("finds unselected sessions in or under a worktree the selection would clean up", () => {
    const owner = at("owner", "/wt/feat/", true);
    const all = [
      owner,
      at("same", "/wt/feat"),
      at("nested", "/wt/feat/sub"),
      at("prefix", "/wt/feature"),
      at("other", "/repo"),
    ];
    expect(sessionsSharingWorktree([owner], all).map((s) => s.id)).toEqual(["same", "nested"]);
    expect(sessionsSharingWorktree([owner, all[1]!, all[2]!], all)).toEqual([]);
    expect(sessionsSharingWorktree([at("plain", "/wt/feat")], all)).toEqual([]);
  });
});
