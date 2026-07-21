// @vitest-environment jsdom
//
// The hook polls the host snapshot and must toast each notification exactly
// once: the first snapshot's backlog is adopted as already-seen (no replay on
// load), and only strictly-newer seqs toast thereafter.

import { renderHook, act } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { PluginUiState } from "../lib/api";
import { fetchPluginUiState } from "../lib/api";
import { reportError, reportInfo, reportOpenLink } from "../lib/toastBus";
import { usePluginUiState } from "./usePluginUiState";

vi.mock("../lib/api", () => ({ fetchPluginUiState: vi.fn() }));
vi.mock("../lib/toastBus", () => ({
  reportError: vi.fn(),
  reportInfo: vi.fn(),
  reportOpenLink: vi.fn(),
}));

const fetchMock = vi.mocked(fetchPluginUiState);

function snapshot(notifications: PluginUiState["notifications"]): PluginUiState {
  return { entries: [], notifications };
}

beforeEach(() => {
  vi.useFakeTimers();
  fetchMock.mockReset();
  vi.mocked(reportError).mockReset();
  vi.mocked(reportInfo).mockReset();
  vi.mocked(reportOpenLink).mockReset();
});
afterEach(() => {
  vi.useRealTimers();
});

describe("usePluginUiState notifications", () => {
  it("adopts the first backlog silently, then toasts only newer seqs once", async () => {
    fetchMock
      // First poll: a backlog notification already present at load.
      .mockResolvedValueOnce(snapshot([{ seq: 1, plugin_id: "acme.kit", tone: "info", title: "old" }]))
      // Second poll: a new one arrives.
      .mockResolvedValueOnce(
        snapshot([
          { seq: 1, plugin_id: "acme.kit", tone: "info", title: "old" },
          { seq: 2, plugin_id: "acme.kit", tone: "danger", title: "Build failed", body: "tests" },
        ]),
      )
      // Third poll: nothing newer; no repeat toast.
      .mockResolvedValue(
        snapshot([
          { seq: 1, plugin_id: "acme.kit", tone: "info", title: "old" },
          { seq: 2, plugin_id: "acme.kit", tone: "danger", title: "Build failed", body: "tests" },
        ]),
      );

    renderHook(() => usePluginUiState());

    // Flush the mount poll: backlog adopted, nothing toasted.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(reportError).not.toHaveBeenCalled();
    expect(reportInfo).not.toHaveBeenCalled();

    // Next tick: seq 2 toasts once, as an error (danger tone), with body joined.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000);
    });
    expect(reportError).toHaveBeenCalledTimes(1);
    expect(reportError).toHaveBeenCalledWith("Build failed: tests");

    // A further tick with no newer seq does not re-toast.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000);
    });
    expect(reportError).toHaveBeenCalledTimes(1);
  });

  it("re-seeds when the ring resets (daemon restart) so new toasts fire", async () => {
    fetchMock
      // Seed high.
      .mockResolvedValueOnce(snapshot([{ seq: 5, plugin_id: "acme.kit", tone: "info", title: "old" }]))
      // Daemon restarted: ring starts low again, below the watermark.
      .mockResolvedValueOnce(snapshot([{ seq: 1, plugin_id: "acme.kit", tone: "info", title: "after restart" }]))
      // A genuinely new one after the reset must toast.
      .mockResolvedValue(
        snapshot([
          { seq: 1, plugin_id: "acme.kit", tone: "info", title: "after restart" },
          { seq: 2, plugin_id: "acme.kit", tone: "info", title: "fresh" },
        ]),
      );

    renderHook(() => usePluginUiState());

    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(reportInfo).not.toHaveBeenCalled();

    // The lower maxSeq is treated as a fresh ring: re-seed, still no toast.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000);
    });
    expect(reportInfo).not.toHaveBeenCalled();

    // seq 2 now exceeds the re-seeded watermark of 1: toast once.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000);
    });
    expect(reportInfo).toHaveBeenCalledTimes(1);
    expect(reportInfo).toHaveBeenCalledWith("fresh");
  });

  it("routes a notification carrying an href to the click-to-open toast", async () => {
    fetchMock
      .mockResolvedValueOnce(snapshot([{ seq: 1, plugin_id: "acme.kit", tone: "info", title: "seed" }]))
      .mockResolvedValue(
        snapshot([
          { seq: 1, plugin_id: "acme.kit", tone: "info", title: "seed" },
          {
            seq: 2,
            plugin_id: "acme.kit",
            tone: "info",
            title: "Open link",
            href: "https://example.com/pr/1",
          },
        ]),
      );

    renderHook(() => usePluginUiState());
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(3000);
    });
    expect(reportOpenLink).toHaveBeenCalledWith("Open link", "https://example.com/pr/1");
    // An href notification does not also fire a plain info toast.
    expect(reportInfo).not.toHaveBeenCalled();
  });
});

describe("usePluginUiState refresh indicator", () => {
  it("does not flip isRefreshing for a poll that settles before the delay", async () => {
    fetchMock.mockResolvedValue(snapshot([]));
    const { result } = renderHook(() => usePluginUiState());

    // Mount poll resolves on a microtask, before the threshold timer fires.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(result.current.isRefreshing).toBe(false);

    // Past where the threshold would have fired: still false (timer was cleared).
    await act(async () => {
      await vi.advanceTimersByTimeAsync(300);
    });
    expect(result.current.isRefreshing).toBe(false);
  });

  it("flips isRefreshing on once a poll outlasts the delay, off when it settles", async () => {
    let resolve!: (v: PluginUiState | null) => void;
    fetchMock.mockReturnValueOnce(new Promise((r) => (resolve = r)));
    fetchMock.mockResolvedValue(snapshot([]));
    const { result } = renderHook(() => usePluginUiState());

    // Still in flight, before the threshold.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(100);
    });
    expect(result.current.isRefreshing).toBe(false);

    // Crossing the threshold while the poll is still pending shows the indicator.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(200);
    });
    expect(result.current.isRefreshing).toBe(true);

    // Settling clears it.
    await act(async () => {
      resolve(snapshot([]));
    });
    expect(result.current.isRefreshing).toBe(false);
  });

  it("clears isRefreshing even when a slow poll fails (returns null)", async () => {
    let resolve!: (v: PluginUiState | null) => void;
    fetchMock.mockReturnValueOnce(new Promise((r) => (resolve = r)));
    fetchMock.mockResolvedValue(snapshot([]));
    const { result } = renderHook(() => usePluginUiState());

    await act(async () => {
      await vi.advanceTimersByTimeAsync(300);
    });
    expect(result.current.isRefreshing).toBe(true);

    await act(async () => {
      resolve(null);
    });
    expect(result.current.isRefreshing).toBe(false);
  });
});
