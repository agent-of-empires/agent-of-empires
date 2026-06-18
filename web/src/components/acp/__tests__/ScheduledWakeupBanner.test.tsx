// @vitest-environment jsdom
//
// The structured-view "Waking…" banner must self-dismiss shortly after
// the wake fires. A fallback ScheduleWakeup superseded by its primary
// signal leaves `nextWakeupAt` set with nothing to clear it, so without
// the grace timeout the banner stuck on "Waking…" forever.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, render } from "@testing-library/react";

import { ScheduledWakeupBanner } from "../StructuredView";

describe("ScheduledWakeupBanner self-dismiss", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("shows Waking… once fired, then unmounts after the grace window", () => {
    const past = new Date(Date.now() - 1_000).toISOString();
    const { container } = render(<ScheduledWakeupBanner wakeAt={past} reason="fallback" />);
    expect(container.textContent).toContain("Waking…");
    act(() => {
      vi.advanceTimersByTime(10_000);
    });
    expect(container.textContent).toBe("");
  });

  it("keeps showing the countdown while the wake is still in the future", () => {
    const future = new Date(Date.now() + 120_000).toISOString();
    const { container } = render(<ScheduledWakeupBanner wakeAt={future} reason="fallback" />);
    expect(container.textContent).toContain("Asleep until");
    act(() => {
      vi.advanceTimersByTime(10_000);
    });
    expect(container.textContent).toContain("Asleep until");
  });
});
