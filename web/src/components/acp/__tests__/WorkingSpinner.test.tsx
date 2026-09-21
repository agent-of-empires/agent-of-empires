// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, screen } from "@testing-library/react";

import { WorkingSpinner } from "../WorkingSpinner";
import { THINKING_VERBS } from "../../../lib/acpRattle";

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

interface SpinnerOpts {
  stalledSecs: number;
  tool: string | null;
  thinking?: boolean;
  cancelling?: boolean;
  cancelEscalatesAt?: string | null;
  compacting?: boolean;
}

function renderSpinner(opts: SpinnerOpts) {
  const ref = { current: Date.now() - opts.stalledSecs * 1000 } as React.RefObject<number>;
  const onForceEndTurn = vi.fn().mockResolvedValue(undefined);
  render(
    <WorkingSpinner
      thinking={opts.thinking ?? false}
      tool={opts.tool}
      cancelling={opts.cancelling ?? false}
      cancelEscalatesAt={opts.cancelEscalatesAt ?? null}
      compacting={opts.compacting ?? false}
      lastActivityRef={ref}
      onForceEndTurn={onForceEndTurn}
    />,
  );
  // One watchdog tick so the label and buttons settle.
  act(() => {
    vi.advanceTimersByTime(1100);
  });
  return { onForceEndTurn };
}

const button = (name: RegExp) => screen.queryByRole("button", { name });

describe("WorkingSpinner", () => {
  // label: expected text (null = neither waiting label); button: the only force button shown.
  it.each<[string, SpinnerOpts, RegExp | null, RegExp | null]>([
    // A tool in flight never offers Force end turn; a long Task gap is normal.
    ["tool past threshold", { stalledSecs: 60, tool: "Write" }, /waiting on tool…/i, null],
    ["long Task subagent", { stalledSecs: 180, tool: "Task" }, /waiting on tool… 3m \d{2}s/i, null],
    ["silent model past threshold", { stalledSecs: 60, tool: null }, /waiting on model…/i, /force end turn/i],
    ["below threshold", { stalledSecs: 5, tool: null }, null, null],
    // A /compact is silent for minutes; name it from the first tick and never offer the abort.
    [
      "compaction past threshold",
      { stalledSecs: 85, tool: null, compacting: true },
      /compaction in progress… 1m \d{2}s/i,
      null,
    ],
    ["compaction early", { stalledSecs: 3, tool: null, compacting: true }, /compaction in progress… \ds/i, null],
    [
      "cancel during compaction",
      { stalledSecs: 85, tool: null, compacting: true, cancelling: true },
      /stopping…/i,
      /force stop/i,
    ],
    // Force stop shows even with a tool in flight: a runaway loop is one.
    ["cancel with tool in flight", { stalledSecs: 2, tool: "Terminal", cancelling: true }, /stopping…/i, /force stop/i],
  ])("%s", (_label, opts, label, forceButton) => {
    renderSpinner(opts);
    if (label) expect(screen.getByText(label)).toBeTruthy();
    else expect(screen.queryByText(/waiting on (model|tool)…/i)).toBeNull();
    if (!opts.compacting || opts.cancelling) expect(screen.queryByText(/compaction in progress…/i)).toBeNull();
    for (const name of [/force end turn/i, /force stop/i]) {
      expect(button(name) !== null).toBe(String(name) === String(forceButton));
    }
  });

  it("shows the tool verb, not a thinking verb, when thinking stays latched through a tool", () => {
    renderSpinner({ stalledSecs: 1, tool: "Terminal", thinking: true });
    expect(screen.getByText(/Terminal…/)).toBeTruthy();
    expect(THINKING_VERBS.some((v) => screen.queryByText(`${v}…`))).toBe(false);
  });

  it("renders an escalation countdown and Force stop invokes the handler", () => {
    const { onForceEndTurn } = renderSpinner({
      stalledSecs: 1,
      tool: "Terminal",
      cancelling: true,
      cancelEscalatesAt: new Date(Date.now() + 8000).toISOString(),
    });
    expect(screen.getByText(/stopping… \(force in \d+s\)/i)).toBeTruthy();
    screen.getByRole("button", { name: /force stop/i }).click();
    expect(onForceEndTurn).toHaveBeenCalledTimes(1);
  });
});
