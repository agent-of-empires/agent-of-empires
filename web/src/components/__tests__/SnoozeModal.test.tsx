// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { SnoozeModal } from "../sidebar/SnoozeModal";
import { SNOOZE_PRESETS, formatSnoozeRemainingShort } from "../sidebar/format";
import { makeOptimisticSnoozedUntil } from "../../lib/sidebarOptimistic";

function setup() {
  const onPick = vi.fn();
  const onCancel = vi.fn();
  render(<SnoozeModal title="t" onCancel={onCancel} onPick={onPick} />);
  return { onPick, onCancel };
}
const change = (id: string, value: string) => fireEvent.change(screen.getByTestId(id), { target: { value } });
const click = (id: string) => fireEvent.click(screen.getByTestId(id));

afterEach(cleanup);

describe("SnoozeModal", () => {
  it("fires onPick with each preset's minutes, matching the TUI presets", () => {
    const { onPick } = setup();
    for (const preset of SNOOZE_PRESETS) click(`snooze-modal-preset-${preset.minutes}`);
    expect(onPick.mock.calls.map(([m]) => m)).toEqual([60, 120, 180, 240, 300, 360, 1440, 10080]);
  });

  it("dismisses on Cancel, Escape, and a backdrop click, but not an inside click", () => {
    const { onCancel } = setup();
    click("snooze-modal");
    expect(onCancel).not.toHaveBeenCalled();
    click("snooze-modal-cancel");
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    click("snooze-modal-backdrop");
    expect(onCancel).toHaveBeenCalledTimes(3);
  });

  it.each([
    ["3", null, 180],
    ["45", "m", 45],
    ["2", "d", 2 * 24 * 60],
    ["1", "w", 7 * 24 * 60],
    ["0", null, null],
    ["5", "w", null],
  ])("custom %s %s picks %s", (value, unit, minutes) => {
    const { onPick } = setup();
    change("snooze-modal-custom-value", value);
    if (unit) change("snooze-modal-custom-unit", unit);
    click("snooze-modal-custom-submit");
    if (minutes == null) {
      expect(onPick).not.toHaveBeenCalled();
      expect(screen.queryByTestId("snooze-modal-custom-error")).not.toBeNull();
    } else {
      expect(onPick).toHaveBeenCalledWith(minutes);
    }
  });

  it("submits a custom duration on Enter", () => {
    const { onPick } = setup();
    change("snooze-modal-custom-value", "2");
    fireEvent.keyDown(screen.getByTestId("snooze-modal-custom-value"), { key: "Enter" });
    expect(onPick).toHaveBeenCalledWith(120);
  });
});

describe("with a fixed clock", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-06-01T12:00:00Z"));
  });
  afterEach(() => vi.useRealTimers());

  it("converts a future datetime to minutes from now", () => {
    const { onPick } = setup();
    // Five days out stays within bounds under any host timezone offset.
    change("snooze-modal-until-value", "2026-06-06T12:00");
    click("snooze-modal-until-submit");
    const minutes = onPick.mock.calls[0]![0] as number;
    expect(minutes).toBeGreaterThan(4 * 24 * 60);
    expect(minutes).toBeLessThan(30 * 24 * 60);
  });

  it.each([
    ["empty", ""],
    ["past", "2026-05-01T12:00"],
    ["beyond 30 days", "2099-01-01T00:00"],
  ])("rejects an %s until value", (_n, value) => {
    const { onPick } = setup();
    if (value) change("snooze-modal-until-value", value);
    click("snooze-modal-until-submit");
    expect(onPick).not.toHaveBeenCalled();
    expect(screen.queryByTestId("snooze-modal-until-error")).not.toBeNull();
  });

  it("makeOptimisticSnoozedUntil returns now plus minutes", () => {
    expect(makeOptimisticSnoozedUntil(60)).toBe("2026-06-01T13:00:00.000Z");
    expect(makeOptimisticSnoozedUntil(24 * 60)).toBe("2026-06-02T12:00:00.000Z");
  });

  it.each([
    ["2026-06-01T12:00:30Z", "<1m"],
    ["2026-06-01T12:30:00Z", "30m"],
    ["2026-06-01T15:30:00Z", "3h"],
    ["2026-06-04T12:00:00Z", "3d"],
    // Optimistic state can briefly outlive the wake time.
    ["2026-05-31T12:00:00Z", "soon"],
    ["not-a-date", "snoozed"],
  ])("formatSnoozeRemainingShort(%s) is %s", (iso, label) => {
    expect(formatSnoozeRemainingShort(iso)).toBe(label);
  });
});
