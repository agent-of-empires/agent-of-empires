// @vitest-environment jsdom
//
// Component coverage for the sidebar snooze modal added in #1581.
// The modal is the only interactive surface inside WorkspaceSidebar
// that does not require the dnd-kit + AgentProfileProvider plumbing
// the rest of the file needs to mount, so we exercise it directly via
// React Testing Library to lock in the eight preset list, the custom
// duration input + unit converter, the datetime-local "until" path,
// and the validation error surfaces.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import {
  SnoozeModal,
  SNOOZE_PRESETS,
  formatSnoozeRemainingShort,
  makeOptimisticSnoozedUntil,
} from "../WorkspaceSidebar";

afterEach(() => {
  cleanup();
});

describe("SnoozeModal preset buttons", () => {
  it("renders the same 8 presets as the TUI dialog and the matching minutes", () => {
    // Regression: any drift between the web modal and
    // `src/tui/dialogs/snooze_duration.rs` confuses users who switch
    // between surfaces. See #1581.
    const minutes = SNOOZE_PRESETS.map((p) => p.minutes);
    expect(minutes).toEqual([60, 120, 180, 240, 300, 360, 1440, 10080]);
  });

  it("clicks fire onPick with the preset minutes", () => {
    const onPick = vi.fn();
    render(
      <SnoozeModal title="my session" onCancel={() => {}} onPick={onPick} />,
    );
    for (const preset of SNOOZE_PRESETS) {
      fireEvent.click(
        screen.getByTestId(`snooze-modal-preset-${preset.minutes}`),
      );
    }
    expect(onPick).toHaveBeenCalledTimes(SNOOZE_PRESETS.length);
    for (let i = 0; i < SNOOZE_PRESETS.length; i++) {
      expect(onPick).toHaveBeenNthCalledWith(i + 1, SNOOZE_PRESETS[i]!.minutes);
    }
  });
});

describe("SnoozeModal dismissal", () => {
  it("Cancel button calls onCancel", () => {
    const onCancel = vi.fn();
    render(<SnoozeModal title="t" onCancel={onCancel} onPick={() => {}} />);
    fireEvent.click(screen.getByTestId("snooze-modal-cancel"));
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it("Escape dismisses the modal", () => {
    const onCancel = vi.fn();
    render(<SnoozeModal title="t" onCancel={onCancel} onPick={() => {}} />);
    // Modal's useEffect attaches keydown to document, so the event
    // needs to bubble there. document.body is the typical bubble
    // source under jsdom.
    document.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Escape", bubbles: true }),
    );
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it("backdrop click dismisses the modal", () => {
    const onCancel = vi.fn();
    render(<SnoozeModal title="t" onCancel={onCancel} onPick={() => {}} />);
    fireEvent.click(screen.getByTestId("snooze-modal-backdrop"));
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  it("inside-modal click does NOT dismiss", () => {
    const onCancel = vi.fn();
    render(<SnoozeModal title="t" onCancel={onCancel} onPick={() => {}} />);
    fireEvent.click(screen.getByTestId("snooze-modal"));
    expect(onCancel).not.toHaveBeenCalled();
  });
});

describe("SnoozeModal custom duration", () => {
  it("converts hours by default and fires onPick with minutes", () => {
    const onPick = vi.fn();
    render(<SnoozeModal title="t" onCancel={() => {}} onPick={onPick} />);
    fireEvent.change(screen.getByTestId("snooze-modal-custom-value"), {
      target: { value: "3" },
    });
    fireEvent.click(screen.getByTestId("snooze-modal-custom-submit"));
    expect(onPick).toHaveBeenCalledWith(180);
  });

  it("respects the unit selector for minutes", () => {
    const onPick = vi.fn();
    render(<SnoozeModal title="t" onCancel={() => {}} onPick={onPick} />);
    fireEvent.change(screen.getByTestId("snooze-modal-custom-value"), {
      target: { value: "45" },
    });
    fireEvent.change(screen.getByTestId("snooze-modal-custom-unit"), {
      target: { value: "m" },
    });
    fireEvent.click(screen.getByTestId("snooze-modal-custom-submit"));
    expect(onPick).toHaveBeenCalledWith(45);
  });

  it("converts days correctly", () => {
    const onPick = vi.fn();
    render(<SnoozeModal title="t" onCancel={() => {}} onPick={onPick} />);
    fireEvent.change(screen.getByTestId("snooze-modal-custom-value"), {
      target: { value: "2" },
    });
    fireEvent.change(screen.getByTestId("snooze-modal-custom-unit"), {
      target: { value: "d" },
    });
    fireEvent.click(screen.getByTestId("snooze-modal-custom-submit"));
    expect(onPick).toHaveBeenCalledWith(2 * 24 * 60);
  });

  it("converts weeks correctly", () => {
    const onPick = vi.fn();
    render(<SnoozeModal title="t" onCancel={() => {}} onPick={onPick} />);
    fireEvent.change(screen.getByTestId("snooze-modal-custom-value"), {
      target: { value: "1" },
    });
    fireEvent.change(screen.getByTestId("snooze-modal-custom-unit"), {
      target: { value: "w" },
    });
    fireEvent.click(screen.getByTestId("snooze-modal-custom-submit"));
    expect(onPick).toHaveBeenCalledWith(7 * 24 * 60);
  });

  it("rejects 0 and surfaces an inline error", () => {
    const onPick = vi.fn();
    render(<SnoozeModal title="t" onCancel={() => {}} onPick={onPick} />);
    fireEvent.change(screen.getByTestId("snooze-modal-custom-value"), {
      target: { value: "0" },
    });
    fireEvent.click(screen.getByTestId("snooze-modal-custom-submit"));
    expect(onPick).not.toHaveBeenCalled();
    expect(screen.queryByTestId("snooze-modal-custom-error")).not.toBeNull();
  });

  it("rejects > 30 days and surfaces an inline error", () => {
    const onPick = vi.fn();
    render(<SnoozeModal title="t" onCancel={() => {}} onPick={onPick} />);
    fireEvent.change(screen.getByTestId("snooze-modal-custom-value"), {
      target: { value: "5" },
    });
    fireEvent.change(screen.getByTestId("snooze-modal-custom-unit"), {
      target: { value: "w" },
    });
    fireEvent.click(screen.getByTestId("snooze-modal-custom-submit"));
    expect(onPick).not.toHaveBeenCalled();
    expect(screen.queryByTestId("snooze-modal-custom-error")).not.toBeNull();
  });

  it("submits on Enter", () => {
    const onPick = vi.fn();
    render(<SnoozeModal title="t" onCancel={() => {}} onPick={onPick} />);
    const input = screen.getByTestId("snooze-modal-custom-value");
    fireEvent.change(input, { target: { value: "2" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(onPick).toHaveBeenCalledWith(120);
  });
});

describe("SnoozeModal datetime-local 'until' path", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-06-01T12:00:00Z"));
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("converts a future datetime to minutes-from-now and fires onPick", () => {
    const onPick = vi.fn();
    render(<SnoozeModal title="t" onCancel={() => {}} onPick={onPick} />);
    // datetime-local values are wall-clock in the user's timezone.
    // jsdom inherits the host TZ, so use a date 5 days out to make
    // the test robust against any reasonable TZ offset; the bound
    // assertion below has wide enough tolerance.
    fireEvent.change(screen.getByTestId("snooze-modal-until-value"), {
      target: { value: "2026-06-06T12:00" },
    });
    fireEvent.click(screen.getByTestId("snooze-modal-until-submit"));
    expect(onPick).toHaveBeenCalledTimes(1);
    const minutes = onPick.mock.calls[0]![0] as number;
    // 5 days minus up to 14h TZ offset is at least ~4 days of
    // minutes; the upper bound stays under the 30-day server limit.
    expect(minutes).toBeGreaterThan(4 * 24 * 60);
    expect(minutes).toBeLessThan(30 * 24 * 60);
  });

  it("rejects an empty datetime input", () => {
    const onPick = vi.fn();
    render(<SnoozeModal title="t" onCancel={() => {}} onPick={onPick} />);
    fireEvent.click(screen.getByTestId("snooze-modal-until-submit"));
    expect(onPick).not.toHaveBeenCalled();
    expect(screen.queryByTestId("snooze-modal-until-error")).not.toBeNull();
  });

  it("rejects a datetime in the past", () => {
    const onPick = vi.fn();
    render(<SnoozeModal title="t" onCancel={() => {}} onPick={onPick} />);
    fireEvent.change(screen.getByTestId("snooze-modal-until-value"), {
      target: { value: "2026-05-01T12:00" },
    });
    fireEvent.click(screen.getByTestId("snooze-modal-until-submit"));
    expect(onPick).not.toHaveBeenCalled();
    expect(screen.queryByTestId("snooze-modal-until-error")).not.toBeNull();
  });

  it("rejects a datetime more than 30 days from now", () => {
    const onPick = vi.fn();
    render(<SnoozeModal title="t" onCancel={() => {}} onPick={onPick} />);
    fireEvent.change(screen.getByTestId("snooze-modal-until-value"), {
      target: { value: "2099-01-01T00:00" },
    });
    fireEvent.click(screen.getByTestId("snooze-modal-until-submit"));
    expect(onPick).not.toHaveBeenCalled();
    expect(screen.queryByTestId("snooze-modal-until-error")).not.toBeNull();
  });
});

describe("makeOptimisticSnoozedUntil", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-06-01T12:00:00Z"));
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("returns now + minutes as an RFC3339 string", () => {
    expect(makeOptimisticSnoozedUntil(60)).toBe("2026-06-01T13:00:00.000Z");
    expect(makeOptimisticSnoozedUntil(24 * 60)).toBe(
      "2026-06-02T12:00:00.000Z",
    );
  });
});

describe("formatSnoozeRemainingShort", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-06-01T12:00:00Z"));
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("renders sub-minute as '<1m'", () => {
    expect(formatSnoozeRemainingShort("2026-06-01T12:00:30Z")).toBe("<1m");
  });

  it("renders sub-hour as minutes", () => {
    expect(formatSnoozeRemainingShort("2026-06-01T12:30:00Z")).toBe("30m");
  });

  it("renders sub-day as hours (rounded down)", () => {
    expect(formatSnoozeRemainingShort("2026-06-01T15:30:00Z")).toBe("3h");
  });

  it("renders ≥ day as days (rounded down)", () => {
    expect(formatSnoozeRemainingShort("2026-06-04T12:00:00Z")).toBe("3d");
  });

  it("returns 'soon' for an already-expired timestamp", () => {
    // The server gates `snoozed_until` on `is_snoozed()` so we
    // should not normally hit this branch on the wire, but the
    // optimistic state can briefly out-stale the prop.
    expect(formatSnoozeRemainingShort("2026-05-31T12:00:00Z")).toBe("soon");
  });

  it("returns 'snoozed' for an unparseable timestamp (defensive)", () => {
    expect(formatSnoozeRemainingShort("not-a-date")).toBe("snoozed");
  });
});
