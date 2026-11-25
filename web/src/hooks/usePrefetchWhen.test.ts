// @vitest-environment jsdom

import { renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { usePrefetchWhen } from "./usePrefetchWhen";

describe("usePrefetchWhen", () => {
  it("loads only once the condition is true", () => {
    const load = vi.fn(() => Promise.resolve());
    const { rerender } = renderHook(({ when }) => usePrefetchWhen(when, load), {
      initialProps: { when: false },
    });
    expect(load).not.toHaveBeenCalled();
    rerender({ when: true });
    expect(load).toHaveBeenCalledTimes(1);
  });

  it("swallows a failed prefetch", async () => {
    const onUnhandled = vi.fn();
    process.on("unhandledRejection", onUnhandled);
    try {
      renderHook(() => usePrefetchWhen(true, () => Promise.reject(new Error("chunk failed"))));
      await new Promise((resolve) => setTimeout(resolve, 0));
      expect(onUnhandled).not.toHaveBeenCalled();
    } finally {
      process.off("unhandledRejection", onUnhandled);
    }
  });
});
