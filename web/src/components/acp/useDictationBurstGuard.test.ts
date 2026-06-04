// @vitest-environment jsdom
//
// Imperative-side tests for useDictationBurstGuard, the iOS-Safari
// dictation glue extracted from Composer.tsx (#1431). The pure
// decideDictationAction matrix is covered separately by
// Composer.dictation.test.ts; this file covers the hook wiring (refs,
// burst timer, setText sink, unmount cleanup) without mounting the
// whole composer + assistant-ui runtime.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { renderHook, act } from "@testing-library/react";

import {
  DICTATION_BURST_TIMEOUT_MS,
  useDictationBurstGuard,
} from "./useDictationBurstGuard";

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

function renderGuard() {
  const setText = vi.fn<(s: string) => void>();
  const { result, unmount, rerender } = renderHook(() =>
    useDictationBurstGuard(setText),
  );
  return { setText, result, unmount, rerender };
}

describe("useDictationBurstGuard (#1431)", () => {
  it("non-replacement input outside a burst is a no-op", () => {
    const { setText, result } = renderGuard();
    act(() => {
      result.current.observeInputType("insertText", 1000);
    });
    expect(result.current.shouldSuppressUpstream("hello")).toBe(false);
    expect(setText).not.toHaveBeenCalled();
  });

  it("insertReplacementText enters a burst and suppresses upstream change", () => {
    const { setText, result } = renderGuard();
    act(() => {
      result.current.observeInputType("insertReplacementText", 1000);
    });
    expect(result.current.shouldSuppressUpstream("open the")).toBe(true);
    expect(setText).not.toHaveBeenCalled();
  });

  it("buffers the latest textarea value across consecutive replacements", () => {
    const { setText, result } = renderGuard();
    act(() => {
      result.current.observeInputType("insertReplacementText", 1000);
    });
    expect(result.current.shouldSuppressUpstream("open")).toBe(true);
    act(() => {
      result.current.observeInputType("insertReplacementText", 1100);
    });
    expect(result.current.shouldSuppressUpstream("open the")).toBe(true);
    act(() => {
      result.current.observeInputType("insertReplacementText", 1300);
    });
    expect(result.current.shouldSuppressUpstream("open the diff viewer")).toBe(
      true,
    );
    expect(setText).not.toHaveBeenCalled();
  });

  it("flushes the buffered text into setText after the burst timeout fires", () => {
    const { setText, result } = renderGuard();
    act(() => {
      result.current.observeInputType("insertReplacementText", 1000);
    });
    result.current.shouldSuppressUpstream("open the diff viewer");
    act(() => {
      vi.advanceTimersByTime(DICTATION_BURST_TIMEOUT_MS + 5);
    });
    expect(setText).toHaveBeenCalledTimes(1);
    expect(setText).toHaveBeenCalledWith("open the diff viewer");
    expect(result.current.shouldSuppressUpstream("any")).toBe(false);
  });

  it("re-arms the burst timer on each replacement so a long utterance does not flush mid-stream", () => {
    const { setText, result } = renderGuard();
    act(() => {
      result.current.observeInputType("insertReplacementText", 1000);
    });
    result.current.shouldSuppressUpstream("open");
    // Half-window passes, then another partial fires; the original
    // timer must be cancelled and replaced.
    act(() => {
      vi.advanceTimersByTime(DICTATION_BURST_TIMEOUT_MS - 100);
    });
    act(() => {
      result.current.observeInputType("insertReplacementText", 2000);
    });
    result.current.shouldSuppressUpstream("open the");
    // Original 1200 ms window has now elapsed since the first event,
    // but the second event reset it; nothing should have flushed yet.
    act(() => {
      vi.advanceTimersByTime(200);
    });
    expect(setText).not.toHaveBeenCalled();
    // Once a full window passes since the second event, the flush fires.
    act(() => {
      vi.advanceTimersByTime(DICTATION_BURST_TIMEOUT_MS);
    });
    expect(setText).toHaveBeenCalledExactlyOnceWith("open the");
  });

  it("blur during an active burst flushes the buffer and clears the timer", () => {
    const { setText, result } = renderGuard();
    act(() => {
      result.current.observeInputType("insertReplacementText", 1000);
    });
    result.current.shouldSuppressUpstream("hello");
    act(() => {
      result.current.flushOnBlur();
    });
    expect(setText).toHaveBeenCalledExactlyOnceWith("hello");
    // The timer should be cleared by the flush; advancing past it must
    // not double-fire setText.
    act(() => {
      vi.advanceTimersByTime(DICTATION_BURST_TIMEOUT_MS + 100);
    });
    expect(setText).toHaveBeenCalledTimes(1);
  });

  it("blur outside a burst is a no-op", () => {
    const { setText, result } = renderGuard();
    act(() => {
      result.current.flushOnBlur();
    });
    expect(setText).not.toHaveBeenCalled();
  });

  it("non-replacement input during a burst flushes, exits the burst, and stops suppressing", () => {
    const { setText, result } = renderGuard();
    act(() => {
      result.current.observeInputType("insertReplacementText", 1000);
    });
    result.current.shouldSuppressUpstream("hello");
    act(() => {
      result.current.observeInputType("insertText", 1500);
    });
    expect(setText).toHaveBeenCalledExactlyOnceWith("hello");
    expect(result.current.shouldSuppressUpstream("hello!")).toBe(false);
  });

  it("does not call setText when the burst ends with an empty buffer (no shouldSuppressUpstream call between burst start and end)", () => {
    const { setText, result } = renderGuard();
    act(() => {
      result.current.observeInputType("insertReplacementText", 1000);
    });
    // Never captured a textarea value via shouldSuppressUpstream; the
    // buffer ref stays null and the flush path skips setText.
    act(() => {
      vi.advanceTimersByTime(DICTATION_BURST_TIMEOUT_MS + 5);
    });
    expect(setText).not.toHaveBeenCalled();
  });

  it("unmount clears any armed timer to avoid a setText on an unmounted parent", () => {
    const { setText, result, unmount } = renderGuard();
    act(() => {
      result.current.observeInputType("insertReplacementText", 1000);
    });
    result.current.shouldSuppressUpstream("hello");
    unmount();
    act(() => {
      vi.advanceTimersByTime(DICTATION_BURST_TIMEOUT_MS + 100);
    });
    expect(setText).not.toHaveBeenCalled();
  });

  it("re-rendering the hook with the same setText reuses the burst state (refs survive re-render)", () => {
    let calls = 0;
    const setText = vi.fn<(s: string) => void>(() => {
      calls += 1;
    });
    const { result, rerender } = renderHook(() =>
      useDictationBurstGuard(setText),
    );
    act(() => {
      result.current.observeInputType("insertReplacementText", 1000);
    });
    result.current.shouldSuppressUpstream("hello");
    rerender();
    // After the rerender the same burst is still active.
    expect(result.current.shouldSuppressUpstream("hello world")).toBe(true);
    act(() => {
      vi.advanceTimersByTime(DICTATION_BURST_TIMEOUT_MS + 5);
    });
    expect(setText).toHaveBeenCalledExactlyOnceWith("hello world");
    expect(calls).toBe(1);
  });
});
