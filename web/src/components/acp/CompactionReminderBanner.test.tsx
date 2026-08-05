// @vitest-environment jsdom
//
// User stories (#3253):
//   - With the reminder off (the default), context usage passing any
//     threshold changes nothing.
//   - With it on and the threshold at 80, a snapshot at or past 80% of the
//     window shows a dismissable banner above the composer offering
//     /compact.
//
// The gate itself is `isCompactionReminderDue`, exercised here as a table so
// each condition has a case without a test function per row. The rendered
// assertions cover the parts a pure gate cannot: the action sends /compact
// rather than prefilling, and dismiss reports back to the reducer.

import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";

import { CompactionReminderBanner } from "./CompactionReminderBanner";
import { AcpPrefsProvider, type AcpPrefs } from "../../lib/acpPrefs";
import { isCompactionReminderDue, type AcpState, type SessionUsage } from "../../lib/acpTypes";

const COMPACT_COMMAND = { name: "compact", description: "compact the conversation", accepts_input: false };

type GateState = Pick<AcpState, "sessionUsage" | "compacting" | "compactionReminderDismissed" | "availableCommands">;

function usage(used: number, size: number): SessionUsage {
  return { used, size, cost: null };
}

function gateState(over: Partial<GateState> = {}): GateState {
  return {
    sessionUsage: usage(160_000, 200_000),
    compacting: false,
    compactionReminderDismissed: null,
    availableCommands: [COMPACT_COMMAND],
    ...over,
  };
}

function prefs(over: Partial<AcpPrefs> = {}): AcpPrefs {
  return {
    showToolDurations: true,
    replayEvents: 0,
    compactionReminder: true,
    compactionReminderPercent: 80,
    ...over,
  };
}

function renderBanner(state: GateState, p: AcpPrefs, onCompact = vi.fn(), onDismiss = vi.fn()) {
  render(
    <AcpPrefsProvider value={p}>
      <CompactionReminderBanner state={state} onCompact={onCompact} onDismiss={onDismiss} />
    </AcpPrefsProvider>,
  );
  return { onCompact, onDismiss };
}

describe("isCompactionReminderDue", () => {
  it("gates on every condition", () => {
    const cases: [string, GateState, AcpPrefs, boolean][] = [
      // Off by default: no banner however full the window is.
      ["disabled", gateState({ sessionUsage: usage(199_000, 200_000) }), prefs({ compactionReminder: false }), false],
      // Equality counts, so a threshold of exactly 80% fires at 80%.
      ["at threshold", gateState({ sessionUsage: usage(160_000, 200_000) }), prefs(), true],
      ["past threshold", gateState({ sessionUsage: usage(190_000, 200_000) }), prefs(), true],
      ["below threshold", gateState({ sessionUsage: usage(150_000, 200_000) }), prefs(), false],
      // Some agents report used > size transiently; that is past any
      // legal threshold, not a rendering glitch to swallow.
      ["over-full window", gateState({ sessionUsage: usage(210_000, 200_000) }), prefs(), true],
      // No window reported yet means there is no percentage to compare.
      ["zero size", gateState({ sessionUsage: usage(0, 0) }), prefs(), false],
      ["no snapshot", gateState({ sessionUsage: null }), prefs(), false],
      // Telling the user to compact while a compaction runs names the job
      // they are already waiting on.
      ["compacting", gateState({ compacting: true }), prefs(), false],
      ["dismissed", gateState({ compactionReminderDismissed: usage(160_000, 200_000) }), prefs(), false],
      // Suggesting a command the agent never advertised is noise.
      ["no compact command", gateState({ availableCommands: [] }), prefs(), false],
    ];
    for (const [label, state, p, expected] of cases) {
      expect(isCompactionReminderDue(state, p), label).toBe(expected);
    }
  });
});

describe("CompactionReminderBanner", () => {
  it("renders nothing while the reminder is off", () => {
    renderBanner(gateState(), prefs({ compactionReminder: false }));
    expect(screen.queryByTestId("compaction-reminder")).toBeNull();
  });

  it("reports the current percentage once the threshold is crossed", () => {
    renderBanner(gateState({ sessionUsage: usage(170_000, 200_000) }), prefs());
    expect(screen.getByTestId("compaction-reminder").textContent).toContain("85%");
  });

  it("sends /compact rather than prefilling, and dismisses on the close control", () => {
    const { onCompact, onDismiss } = renderBanner(gateState(), prefs());
    screen.getByRole("button", { name: "Compact now" }).click();
    expect(onCompact).toHaveBeenCalledTimes(1);
    expect(onDismiss).not.toHaveBeenCalled();
    screen.getByRole("button", { name: "Dismiss compaction reminder" }).click();
    expect(onDismiss).toHaveBeenCalledTimes(1);
  });
});
