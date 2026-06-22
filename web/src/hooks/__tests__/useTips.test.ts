// @vitest-environment jsdom
//
// Tests for useTips and shouldAutoPopTips (#2292): the hook fetches the
// web-surface tips, derives unseen state, owns the modal open/close + the tip
// it opens on, and persists mark-seen and the show-on-startup toggle through
// the api module (mocked). shouldAutoPopTips is the pure startup-auto-pop gate.

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
  it("loads tips and derives unseen state", async () => {
    mockFetch.mockResolvedValue(resp());
    const { result } = renderHook(() => useTips());

    await waitFor(() => expect(result.current.loaded).toBe(true));
    expect(result.current.enabled).toBe(true);
    expect(result.current.tips).toHaveLength(2);
    expect(result.current.hasUnseen).toBe(true);
    expect(result.current.isOpen).toBe(false);
  });

  it("treats a failed fetch as loaded with no tips", async () => {
    mockFetch.mockResolvedValue(null);
    const { result } = renderHook(() => useTips());

    await waitFor(() => expect(result.current.loaded).toBe(true));
    expect(result.current.enabled).toBe(false);
    expect(result.current.tips).toEqual([]);
    expect(result.current.hasUnseen).toBe(false);
  });

  it("hasUnseen is false when tips are disabled even with unseen entries", async () => {
    mockFetch.mockResolvedValue(resp({ enabled: false }));
    const { result } = renderHook(() => useTips());

    await waitFor(() => expect(result.current.loaded).toBe(true));
    expect(result.current.hasUnseen).toBe(false);
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

  it("markSeen flips the tip locally and persists it", async () => {
    mockFetch.mockResolvedValue(resp());
    mockMarkSeen.mockResolvedValue(true);
    const { result } = renderHook(() => useTips());
    await waitFor(() => expect(result.current.loaded).toBe(true));

    act(() => result.current.markSeen("b"));
    expect(result.current.tips.find((t) => t.id === "b")?.seen).toBe(true);
    expect(result.current.hasUnseen).toBe(false);
    expect(mockMarkSeen).toHaveBeenCalledWith("b");
  });

  it("setEnabled flips enabled locally and persists it", async () => {
    mockFetch.mockResolvedValue(resp());
    mockSetShow.mockResolvedValue(true);
    const { result } = renderHook(() => useTips());
    await waitFor(() => expect(result.current.loaded).toBe(true));

    act(() => result.current.setEnabled(false));
    expect(result.current.enabled).toBe(false);
    expect(result.current.hasUnseen).toBe(false);
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
