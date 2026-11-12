// @vitest-environment jsdom

import { act, renderHook } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../../lib/api", () => ({ fetchAllSessions: vi.fn() }));
vi.mock("../../lib/acpDrafts", () => ({ sweepOrphanDrafts: vi.fn() }));
vi.mock("../../components/diff/comments/storage", () => ({ sweepOrphanComments: vi.fn() }));

import { fetchAllSessions, type SessionsEnvelope } from "../../lib/api";
import { sweepOrphanDrafts } from "../../lib/acpDrafts";
import { sweepOrphanComments } from "../../components/diff/comments/storage";
import type { SessionResponse } from "../../lib/types";
import { useOrphanSweep } from "../useOrphanSweep";

const rows = (...ids: string[]) => ids.map((id) => ({ id }) as SessionResponse);
const snapshot = (...ids: string[]): SessionsEnvelope => ({
  sessions: rows(...ids),
  workspace_ordering: [],
  cursor: { epoch: "boot", revision: 1n },
});
const swept = () => [...vi.mocked(sweepOrphanDrafts).mock.calls[0]![0]].sort();

beforeEach(() => vi.clearAllMocks());

describe("useOrphanSweep", () => {
  it("sweeps with the live ids alone when the trash is empty", () => {
    renderHook(() => useOrphanSweep(rows("a", "b"), true, 0));
    expect(swept()).toEqual(["a", "b"]);
    expect(sweepOrphanComments).toHaveBeenCalledWith(new Set(["a", "b"]));
    expect(fetchAllSessions).not.toHaveBeenCalled();
  });

  it("waits for the first list", () => {
    renderHook(() => useOrphanSweep([], false, 0));
    expect(sweepOrphanDrafts).not.toHaveBeenCalled();
  });

  it("takes the whole inventory from one snapshot, plus sessions created while it was in flight", async () => {
    let resolve: (value: SessionsEnvelope | null) => void = () => {};
    vi.mocked(fetchAllSessions).mockReturnValue(new Promise((r) => (resolve = r)));
    const { rerender } = renderHook(({ sessions }) => useOrphanSweep(sessions, true, 1), {
      initialProps: { sessions: rows("a") },
    });
    expect(sweepOrphanDrafts).not.toHaveBeenCalled();

    rerender({ sessions: rows("a", "created-meanwhile") });
    // `restored` left the trash after the list was read; only the all-scope snapshot still has it.
    await act(async () => resolve(snapshot("a", "restored", "t")));

    expect(swept()).toEqual(["a", "created-meanwhile", "restored", "t"]);
    expect(vi.mocked(sweepOrphanComments).mock.calls[0]![0]).toEqual(
      new Set(["a", "created-meanwhile", "restored", "t"]),
    );
    expect(fetchAllSessions).toHaveBeenCalledTimes(1);
  });

  it("skips the sweep when the inventory cannot be read, and sweeps only once", async () => {
    vi.mocked(fetchAllSessions).mockResolvedValue(null);
    const { rerender } = renderHook(({ sessions }) => useOrphanSweep(sessions, true, 1), {
      initialProps: { sessions: rows("a") },
    });
    await act(async () => {});
    rerender({ sessions: rows("a", "b") });
    await act(async () => {});
    expect(sweepOrphanDrafts).not.toHaveBeenCalled();
    expect(fetchAllSessions).toHaveBeenCalledTimes(1);
  });
});
