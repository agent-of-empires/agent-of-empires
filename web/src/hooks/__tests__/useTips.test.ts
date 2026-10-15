// @vitest-environment jsdom

import { renderHook, act, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { useTips, shouldAutoPopTips, type TipsAutoPopGate } from "../useTips";
import type { TipsResponse } from "../../lib/api";

vi.mock("../../lib/api", () => ({
  fetchTips: vi.fn(),
  markTipSeen: vi.fn(),
  setShowTips: vi.fn(),
}));

import { fetchTips, markTipSeen, setShowTips } from "../../lib/api";

const mockFetch = vi.mocked(fetchTips);
const mockMarkSeen = vi.mocked(markTipSeen);
const mockSetShow = vi.mocked(setShowTips);

afterEach(() => {
  vi.clearAllMocks();
});

function resp(over: Partial<TipsResponse> = {}): TipsResponse {
  return {
    enabled: true,
    tips: [
      { id: "a", title: "A", body: "ba", seen: true },
      { id: "b", title: "B", body: "bb", seen: false },
    ],
    ...over,
  };
}

describe("useTips", () => {
  it.each<[string, TipsResponse | null, boolean, number, boolean]>([
    ["a loaded response", resp(), true, 2, true],
    ["a failed fetch", null, false, 0, false],
    ["tips disabled server-side", resp({ enabled: false }), false, 2, false],
  ])("derives state from %s", async (_label, response, enabled, count, hasUnseen) => {
    mockFetch.mockResolvedValue(response);
    const { result } = renderHook(() => useTips());

    await waitFor(() => expect(result.current.loaded).toBe(true));
    expect(result.current.enabled).toBe(enabled);
    expect(result.current.tips).toHaveLength(count);
    expect(result.current.hasUnseen).toBe(hasUnseen);
    expect(result.current.isOpen).toBe(false);
  });

  it("opens on the first unseen tip, marks it seen, and closes", async () => {
    mockFetch.mockResolvedValue(resp());
    mockMarkSeen.mockResolvedValue(true);
    const { result } = renderHook(() => useTips());
    await waitFor(() => expect(result.current.loaded).toBe(true));

    act(() => result.current.open());
    expect(result.current.isOpen).toBe(true);
    expect(result.current.startIndex).toBe(1); // first unseen
    expect(result.current.tips.find((t) => t.id === "b")?.seen).toBe(true);
    expect(mockMarkSeen).toHaveBeenCalledWith("b");

    act(() => result.current.close());
    expect(result.current.isOpen).toBe(false);
  });

  it("opens at index 0 when every tip is already seen", async () => {
    mockFetch.mockResolvedValue(resp({ tips: [{ id: "a", title: "A", body: "b", seen: true }] }));
    const { result } = renderHook(() => useTips());
    await waitFor(() => expect(result.current.loaded).toBe(true));

    act(() => result.current.open());
    expect(result.current.startIndex).toBe(0);
    expect(mockMarkSeen).not.toHaveBeenCalled();
  });

  it("markSeen and setEnabled flip local state and persist it", async () => {
    mockFetch.mockResolvedValue(resp());
    mockMarkSeen.mockResolvedValue(true);
    mockSetShow.mockResolvedValue(true);
    const { result } = renderHook(() => useTips());
    await waitFor(() => expect(result.current.loaded).toBe(true));

    act(() => result.current.markSeen("b"));
    expect(result.current.tips.find((t) => t.id === "b")?.seen).toBe(true);
    expect(result.current.hasUnseen).toBe(false);
    expect(mockMarkSeen).toHaveBeenCalledWith("b");

    act(() => result.current.setEnabled(false));
    expect(result.current.enabled).toBe(false);
    expect(mockSetShow).toHaveBeenCalledWith(false);
  });
});

describe("shouldAutoPopTips", () => {
  const ready: TipsAutoPopGate = {
    loaded: true,
    hasUnseen: true,
    tourSeenAtLoad: true,
    onboardingReady: true,
    telemetryPending: false,
    tourActive: false,
    automated: false,
  };

  it("returns true when every gate is satisfied", () => {
    expect(shouldAutoPopTips(ready)).toBe(true);
  });

  it("blocks each individual gate", () => {
    expect(shouldAutoPopTips({ ...ready, loaded: false })).toBe(false);
    expect(shouldAutoPopTips({ ...ready, hasUnseen: false })).toBe(false);
    expect(shouldAutoPopTips({ ...ready, tourSeenAtLoad: false })).toBe(false);
    expect(shouldAutoPopTips({ ...ready, tourSeenAtLoad: null })).toBe(false);
    expect(shouldAutoPopTips({ ...ready, onboardingReady: false })).toBe(false);
    expect(shouldAutoPopTips({ ...ready, telemetryPending: true })).toBe(false);
    expect(shouldAutoPopTips({ ...ready, tourActive: true })).toBe(false);
    expect(shouldAutoPopTips({ ...ready, automated: true })).toBe(false);
  });
});
