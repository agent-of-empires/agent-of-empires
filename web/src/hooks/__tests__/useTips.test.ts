// @vitest-environment jsdom
//
// Hook tests for useTips (#2292): fetches the web-surface tips on mount,
// derives the unseen/firstUnseen state the tip-of-the-day modal needs, and
// persists mark-seen and the show-on-startup toggle through the api module
// (mocked here so no network is touched).

import { renderHook, act, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { useTips } from "../useTips";
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
    expect(result.current.firstUnseenIndex).toBe(1);
  });

  it("reports no unseen and index 0 when all tips are seen", async () => {
    mockFetch.mockResolvedValue(resp({ tips: [{ id: "a", title: "A", body: "b", seen: true }] }));
    const { result } = renderHook(() => useTips());

    await waitFor(() => expect(result.current.loaded).toBe(true));
    expect(result.current.hasUnseen).toBe(false);
    expect(result.current.firstUnseenIndex).toBe(0);
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
