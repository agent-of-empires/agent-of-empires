// @vitest-environment jsdom

import { act, renderHook } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import type { SessionResponse, Workspace } from "../lib/types";
import { useSidebarTriage } from "./useSidebarTriage";
import { setSessionArchive, setSessionUnread } from "../lib/api";

vi.mock("../lib/api", () => ({
  setSessionArchive: vi.fn(),
  setSessionPin: vi.fn(),
  setSessionSnooze: vi.fn(),
  setSessionUnread: vi.fn(),
}));

const slice = (session: Partial<SessionResponse>): Workspace => ({
  id: "shared",
  branch: null,
  projectPath: "/p",
  displayName: "shared",
  agents: ["claude"],
  primaryAgent: "claude",
  status: "idle",
  sessions: [session] as SessionResponse[],
});

// One workspace rendered as two status slices, so each row's actions target its own session.
const archivedSlice = slice({ id: "a", status: "Stopped", archived_at: null });
const unreadSlice = slice({ id: "b", status: "Running" });
// A stable array: the hook reconciles whenever its `workspaces` identity changes.
const slices = [archivedSlice, unreadSlice];

const accepted = { id: "a" } as SessionResponse;

beforeEach(() => {
  vi.mocked(setSessionArchive).mockReset().mockResolvedValue(accepted);
  vi.mocked(setSessionUnread).mockReset().mockResolvedValue(accepted);
});

describe("useSidebarTriage pending actions", () => {
  it("keeps archive and unread pending on separate slices of one workspace", async () => {
    const { result } = renderHook(() => useSidebarTriage(slices));

    await act(async () => {
      result.current.archiveToggle(archivedSlice, true);
      result.current.unreadToggle(unreadSlice, true);
    });

    expect(result.current.optimisticFor(archivedSlice)).toMatchObject({ archived: true, unread: null, sessionId: "a" });
    expect(result.current.optimisticFor(unreadSlice)).toMatchObject({ archived: null, unread: true, sessionId: "b" });
  });

  it("keeps both pending actions when the slices recombine into one row", async () => {
    const held = () => {
      let resolve!: (value: SessionResponse) => void;
      const promise = new Promise<SessionResponse>((r) => (resolve = r));
      return { promise, resolve };
    };
    const archiveCall = held();
    const unreadCall = held();
    vi.mocked(setSessionArchive).mockReturnValue(archiveCall.promise);
    vi.mocked(setSessionUnread).mockReturnValue(unreadCall.promise);
    // A Repository/Org/Group axis shows the workspace as one row again.
    const combined: Workspace = { ...archivedSlice, sessions: [...archivedSlice.sessions, ...unreadSlice.sessions] };
    const { result, rerender } = renderHook(
      ({ workspaces }: { workspaces: Workspace[] }) => useSidebarTriage(workspaces),
      {
        initialProps: { workspaces: slices },
      },
    );

    act(() => {
      result.current.archiveToggle(archivedSlice, true);
      result.current.unreadToggle(unreadSlice, true);
    });
    rerender({ workspaces: [combined] });

    const row = result.current.optimisticFor(combined);
    expect(row).toMatchObject({ archived: true, unread: true });
    expect(row.bySession?.get("a")?.archived).toBe(true);
    expect(row.bySession?.get("b")?.unread).toBe(true);

    await act(async () => {
      archiveCall.resolve(accepted);
      unreadCall.resolve(accepted);
    });
  });

  it("reverts a failed action without clearing the other session's pending action", async () => {
    vi.mocked(setSessionArchive).mockResolvedValue(null);
    const { result } = renderHook(() => useSidebarTriage(slices));

    await act(async () => {
      result.current.archiveToggle(archivedSlice, true);
      result.current.unreadToggle(unreadSlice, true);
    });

    expect(result.current.optimisticFor(archivedSlice).archived).toBeNull();
    expect(result.current.optimisticFor(unreadSlice).unread).toBe(true);
    expect(result.current.optimisticFor(unreadSlice).archived).toBeNull();
  });
});
