// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { SessionNotice } from "../../lib/acpTypes";
import { AcpPrefsProvider, type AcpPrefs } from "../../lib/acpPrefs";
import { SessionNoticesStrip } from "./PromptStrips";

afterEach(() => {
  cleanup();
});

function mk(id: string, severity: string, title: string, description?: string): SessionNotice {
  return { id, severity, title, description: description ?? null };
}

describe("SessionNoticesStrip", () => {
  it("renders nothing when every notice is dismissed", () => {
    const { container } = render(<SessionNoticesStrip notices={[]} onDismiss={() => {}} />);
    expect(container.innerHTML).toBe("");
  });

  it("shows each notice with its description and dismisses the one that was clicked", () => {
    const onDismiss = vi.fn();
    render(
      <SessionNoticesStrip
        notices={[
          mk("notice-1", "warning", "Model fallback", "Switched to Sonnet."),
          mk("notice-2", "info", "Fast mode turned off"),
        ]}
        onDismiss={onDismiss}
      />,
    );

    expect(screen.getByText("Model fallback")).toBeTruthy();
    expect(screen.getByText("Switched to Sonnet.")).toBeTruthy();
    expect(screen.getByText("Fast mode turned off")).toBeTruthy();

    fireEvent.click(screen.getByLabelText("Dismiss notice: Fast mode turned off"));
    expect(onDismiss).toHaveBeenCalledTimes(1);
    expect(onDismiss).toHaveBeenCalledWith("notice-2");
  });

  // An unknown future ACP level must still render, toned as advisory.
  it("renders an unrecognized severity rather than dropping it", () => {
    render(<SessionNoticesStrip notices={[mk("notice-1", "critical", "Quota exhausted")]} onDismiss={() => {}} />);
    expect(screen.getByText("Quota exhausted")).toBeTruthy();
  });
});

describe("SessionNoticesStrip notice display pref", () => {
  const prefs = (noticeDisplay: AcpPrefs["noticeDisplay"]): AcpPrefs => ({
    showToolDurations: true,
    wrapToolOutput: false,
    replayEvents: 0,
    compactionReminder: false,
    compactionReminderPercent: 75,
    noticeDisplay,
    noticeTimeoutSecs: 10,
  });
  const notices = [mk("n-info", "info", "Fast mode off"), mk("n-error", "error", "Compaction failed")];

  afterEach(() => {
    vi.useRealTimers();
  });

  it("hides advisories but keeps errors when hidden", () => {
    render(
      <AcpPrefsProvider value={prefs("hidden")}>
        <SessionNoticesStrip notices={notices} onDismiss={() => {}} />
      </AcpPrefsProvider>,
    );
    expect(screen.queryByText("Fast mode off")).toBeNull();
    expect(screen.getByText("Compaction failed")).toBeTruthy();
  });

  it("auto-dismisses advisories after the timeout but never errors", () => {
    vi.useFakeTimers();
    const onDismiss = vi.fn();
    render(
      <AcpPrefsProvider value={prefs("timeout")}>
        <SessionNoticesStrip notices={notices} onDismiss={onDismiss} />
      </AcpPrefsProvider>,
    );

    act(() => vi.advanceTimersByTime(9_999));
    expect(onDismiss).not.toHaveBeenCalled();
    act(() => vi.advanceTimersByTime(1));
    expect(onDismiss).toHaveBeenCalledTimes(1);
    expect(onDismiss).toHaveBeenCalledWith("n-info");
    act(() => vi.advanceTimersByTime(60_000));
    expect(onDismiss).toHaveBeenCalledTimes(1);
  });

  it("keeps a notice whose timeout is past the browser timer limit (#4475)", () => {
    vi.useFakeTimers();
    const onDismiss = vi.fn();
    render(
      <AcpPrefsProvider value={{ ...prefs("timeout"), noticeTimeoutSecs: 2_147_484 }}>
        <SessionNoticesStrip notices={notices} onDismiss={onDismiss} />
      </AcpPrefsProvider>,
    );

    act(() => vi.advanceTimersByTime(2_147_483_646));
    expect(onDismiss).not.toHaveBeenCalled();
    act(() => vi.advanceTimersByTime(1));
    expect(onDismiss).toHaveBeenCalledTimes(1);
    expect(onDismiss).toHaveBeenCalledWith("n-info");
  });

  it("keeps notices until dismissed by default", () => {
    vi.useFakeTimers();
    const onDismiss = vi.fn();
    render(<SessionNoticesStrip notices={notices} onDismiss={onDismiss} />);
    act(() => vi.advanceTimersByTime(60_000));
    expect(onDismiss).not.toHaveBeenCalled();
  });
});
