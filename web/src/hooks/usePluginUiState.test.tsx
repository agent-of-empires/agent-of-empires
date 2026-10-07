// @vitest-environment jsdom

import { renderHook, act } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { PluginUiNotification, PluginUiState } from "../lib/api";
import { fetchPluginUiState } from "../lib/api";
import { reportError, reportInfo, reportOpenLink } from "../lib/toastBus";
import { usePluginUiState } from "./usePluginUiState";

vi.mock("../lib/api", () => ({ fetchPluginUiState: vi.fn() }));
vi.mock("../lib/toastBus", () => ({ reportError: vi.fn(), reportInfo: vi.fn(), reportOpenLink: vi.fn() }));

const fetchMock = vi.mocked(fetchPluginUiState);

const note = (seq: number, title: string, over: Partial<PluginUiNotification> = {}): PluginUiNotification => ({
  seq,
  plugin_id: "acme.kit",
  tone: "info",
  title,
  ...over,
});
const snapshot = (...notifications: PluginUiNotification[]): PluginUiState => ({ entries: [], notifications });
const tick = (ms: number) => act(() => vi.advanceTimersByTimeAsync(ms));

/** Serve `polls` in order (the last one repeats) and mount the hook. */
function mountPolling(...polls: PluginUiState[]) {
  for (const poll of polls.slice(0, -1)) fetchMock.mockResolvedValueOnce(poll);
  fetchMock.mockResolvedValue(polls[polls.length - 1]!);
  return renderHook(() => usePluginUiState());
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.resetAllMocks();
});
afterEach(() => vi.useRealTimers());

describe("usePluginUiState notifications", () => {
  it("adopts the first backlog silently, then toasts only newer seqs once", async () => {
    const failed = note(2, "Build failed", { tone: "danger", body: "tests" });
    mountPolling(snapshot(note(1, "old")), snapshot(note(1, "old"), failed));
    await tick(0);
    expect(reportError).not.toHaveBeenCalled();
    expect(reportInfo).not.toHaveBeenCalled();
    await tick(3000);
    await tick(3000);
    expect(vi.mocked(reportError).mock.calls).toEqual([["Build failed: tests"]]);
  });

  it("re-seeds when the ring resets (daemon restart) so new toasts fire", async () => {
    const restarted = note(1, "after restart");
    mountPolling(snapshot(note(5, "old")), snapshot(restarted), snapshot(restarted, note(2, "fresh")));
    await tick(0);
    await tick(3000);
    expect(reportInfo).not.toHaveBeenCalled();
    await tick(3000);
    expect(vi.mocked(reportInfo).mock.calls).toEqual([["fresh"]]);
  });

  it("routes a notification carrying an href to the click-to-open toast", async () => {
    const link = note(2, "Open link", { href: "https://example.com/pr/1" });
    mountPolling(snapshot(note(1, "seed")), snapshot(note(1, "seed"), link));
    await tick(0);
    await tick(3000);
    expect(reportOpenLink).toHaveBeenCalledWith("Open link", "https://example.com/pr/1");
    expect(reportInfo).not.toHaveBeenCalled();
  });
});

describe("usePluginUiState refresh indicator", () => {
  it("does not flip isRefreshing for a poll that settles before the delay", async () => {
    const { result } = mountPolling(snapshot());
    await tick(0);
    await tick(300);
    expect(result.current.isRefreshing).toBe(false);
  });

  it.each([snapshot(), null])("shows only for a slow poll and clears when it settles with %j", async (settled) => {
    let resolve!: (v: PluginUiState | null) => void;
    fetchMock.mockReturnValueOnce(new Promise((r) => (resolve = r)));
    fetchMock.mockResolvedValue(snapshot());
    const { result } = renderHook(() => usePluginUiState());
    await tick(100);
    expect(result.current.isRefreshing).toBe(false);
    await tick(200);
    expect(result.current.isRefreshing).toBe(true);
    await act(async () => resolve(settled));
    expect(result.current.isRefreshing).toBe(false);
  });
});
