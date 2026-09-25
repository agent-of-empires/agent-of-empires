// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import {
  ScheduledWakeupBanner,
  SnoozedWorkerStoppedBanner,
  TrashedWorkerStoppedBanner,
  WorkerRestartingBanner,
} from "../SessionBanners";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe("SnoozedWorkerStoppedBanner", () => {
  it("renders the wake time, or the raw value instead of Invalid Date", () => {
    for (const [snoozedUntil, wake] of [
      ["2099-01-01T00:00:00Z", /2099|2098/],
      ["not-a-date", /not-a-date/],
    ] as const) {
      render(<SnoozedWorkerStoppedBanner sessionId="abc-123" snoozedUntil={snoozedUntil} />);
      const text = screen.getByTestId("acp-snoozed-banner-abc-123").textContent;
      expect(text).toMatch(wake);
      expect(text).not.toContain("Invalid Date");
      cleanup();
    }
  });
});

describe("TrashedWorkerStoppedBanner", () => {
  it("renders the read-only trash notice without Restore when onRestore is omitted", () => {
    render(<TrashedWorkerStoppedBanner sessionId="sess-9" />);
    expect(screen.getByTestId("acp-trashed-banner-sess-9")).toBeTruthy();
    expect(screen.getByText("Session in trash")).toBeTruthy();
    expect(screen.getByText(/read-only/)).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Restore" })).toBeNull();
  });

  it("disables Restore while pending so a double-click cannot fire twice", () => {
    const onRestore = vi.fn(() => new Promise<boolean>(() => {}));
    render(<TrashedWorkerStoppedBanner sessionId="sess-9" onRestore={onRestore} />);
    fireEvent.click(screen.getByRole("button", { name: "Restore" }));
    const pending = screen.getByRole("button", { name: "Restoring…" }) as HTMLButtonElement;
    expect(pending.disabled).toBe(true);
    fireEvent.click(pending);
    expect(onRestore).toHaveBeenCalledTimes(1);
  });

  it("resets the pending state when restore resolves false or rejects", async () => {
    for (const impl of [() => Promise.resolve(false), () => Promise.reject(new Error("boom"))]) {
      render(<TrashedWorkerStoppedBanner sessionId="sess-9" onRestore={vi.fn(impl)} />);
      await act(async () => {
        fireEvent.click(screen.getByRole("button", { name: "Restore" }));
      });
      await waitFor(() =>
        expect((screen.getByRole("button", { name: "Restore" }) as HTMLButtonElement).disabled).toBe(false),
      );
      cleanup();
    }
  });
});

describe("WorkerRestartingBanner", () => {
  const GENERIC = "Restarting structured view worker";
  const UNRESPONSIVE = "Agent stopped responding to cancel";
  const ORPHANED = "Agent finished but didn't notify the daemon";
  it("explains the restart cause, preferring orphaned", () => {
    const cases = [
      [false, false, GENERIC],
      [true, false, UNRESPONSIVE],
      [false, true, ORPHANED],
      // Both can briefly be set during a cancel-escalation race; orphaned wins.
      [true, true, ORPHANED],
    ] as const;
    for (const [agentUnresponsive, agentOrphaned, expected] of cases) {
      const { container } = render(
        <WorkerRestartingBanner agentUnresponsive={agentUnresponsive} agentOrphaned={agentOrphaned} />,
      );
      for (const copy of [GENERIC, UNRESPONSIVE, ORPHANED]) {
        expect(container.textContent?.includes(copy), `${agentUnresponsive}/${agentOrphaned}`).toBe(copy === expected);
      }
      cleanup();
    }
  });
});

describe("ScheduledWakeupBanner", () => {
  it("shows Waking… once fired, then self-dismisses after the grace window", () => {
    vi.useFakeTimers();
    const { container } = render(
      <ScheduledWakeupBanner wakeAt={new Date(Date.now() - 1_000).toISOString()} reason="fallback" />,
    );
    expect(container.textContent).toContain("Waking…");
    act(() => {
      vi.advanceTimersByTime(10_000);
    });
    expect(container.textContent).toBe("");
  });

  it("keeps the countdown while the wake is still in the future", () => {
    vi.useFakeTimers();
    const { container } = render(
      <ScheduledWakeupBanner wakeAt={new Date(Date.now() + 120_000).toISOString()} reason="fallback" />,
    );
    act(() => {
      vi.advanceTimersByTime(10_000);
    });
    expect(container.textContent).toContain("Asleep until");
  });
});
