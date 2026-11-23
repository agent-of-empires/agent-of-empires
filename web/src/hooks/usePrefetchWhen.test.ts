// @vitest-environment jsdom

import { renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { usePrefetchWhen } from "./usePrefetchWhen";

describe("usePrefetchWhen", () => {
  it("loads only once the condition is true", () => {
    const load = vi.fn();
    const { rerender } = renderHook(({ when }) => usePrefetchWhen(when, load), {
      initialProps: { when: false },
    });
    expect(load).not.toHaveBeenCalled();
    rerender({ when: true });
    expect(load).toHaveBeenCalledTimes(1);
  });
});
