// @vitest-environment jsdom
//
// Contract test for the structured view WorkingSpinner force-end-turn gate.
// Verifies the post-#1176 behaviour: the escape hatch is suppressed
// whenever any tool is in flight, the stalled-time label still flips
// so the user sees the wait, and the button only surfaces for a
// silent model with no tool running.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, screen } from "@testing-library/react";

import { WorkingSpinner } from "../StructuredView";
import { THINKING_VERBS } from "../../../lib/acpRattle";

function makeRef(initial: number): React.RefObject<number> {
  return { current: initial } as React.RefObject<number>;
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

function renderSpinner(opts: {
  /** Seconds since the last streaming frame (drives the watchdog). */
  stalledSecs: number;
  /** In-flight tool name, or null for "model is silent". */
  tool: string | null;
  thinking?: boolean;
  /** True once the user clicked Stop and aoe armed escalation (#1727). */
  cancelling?: boolean;
  /** ISO deadline for the escalation countdown, or null. */
  cancelEscalatesAt?: string | null;
}) {
  const now = Date.now();
  const ref = makeRef(now - opts.stalledSecs * 1000);
  const onForceEndTurn = vi.fn().mockResolvedValue(undefined);
  render(
    <WorkingSpinner
      thinking={opts.thinking ?? false}
      tool={opts.tool}
      cancelling={opts.cancelling ?? false}
      cancelEscalatesAt={opts.cancelEscalatesAt ?? null}
      lastActivityRef={ref}
      onForceEndTurn={onForceEndTurn}
    />,
  );
  // One watchdog tick so the 1s interval inside the component picks
  // up the pre-seeded `lastActivityRef` and the label / button states
  // settle.
  act(() => {
    vi.advanceTimersByTime(1100);
  });
  return { onForceEndTurn };
}

describe("WorkingSpinner force-end-turn gate (#1176)", () => {
  it("hides the button while a tool is in flight, even past the stall threshold", () => {
    renderSpinner({ stalledSecs: 60, tool: "Write" });
    expect(screen.queryByRole("button", { name: /force end turn/i })).toBeNull();
    expect(screen.getByText(/waiting on tool…/i)).toBeTruthy();
  });

  it("shows the button when no tool is in flight past the stall threshold", () => {
    renderSpinner({ stalledSecs: 60, tool: null });
    expect(screen.getByRole("button", { name: /force end turn/i })).toBeTruthy();
    expect(screen.getByText(/waiting on model…/i)).toBeTruthy();
  });

  it("hides the button below threshold regardless of tool state", () => {
    renderSpinner({ stalledSecs: 5, tool: null });
    expect(screen.queryByRole("button", { name: /force end turn/i })).toBeNull();
    expect(screen.queryByText(/waiting on (model|tool)…/i)).toBeNull();
  });

  it("hides the button for a long-running Task subagent (original report)", () => {
    renderSpinner({ stalledSecs: 180, tool: "Task" });
    expect(screen.queryByRole("button", { name: /force end turn/i })).toBeNull();
    expect(screen.getByText(/waiting on tool… 3m \d{2}s/i)).toBeTruthy();
  });
});

describe("WorkingSpinner state precedence (#1213)", () => {
  it("shows the tool verb, not a thinking verb, when both thinking and tool are set", () => {
    // The adapter can leave `thinking` latched true through a tool run
    // (it skips ThinkingEnded). Tool is the more specific signal and
    // must win, so the user sees "Dispatching Terminal…" rather than a
    // mystical thinking verb while a shell command is in flight.
    renderSpinner({ stalledSecs: 1, tool: "Terminal", thinking: true });
    expect(screen.getByText(/Terminal…/)).toBeTruthy();
    expect(THINKING_VERBS.some((v) => screen.queryByText(`${v}…`))).toBe(false);
  });
});

describe("WorkingSpinner cancelling / force-stop (#1727)", () => {
  it("shows Stopping… and a Force stop button even while a tool is in flight", () => {
    renderSpinner({ stalledSecs: 2, tool: "Terminal", cancelling: true });
    expect(screen.getByRole("button", { name: /force stop/i })).toBeTruthy();
    expect(screen.getByText(/stopping…/i)).toBeTruthy();
    // The legacy "Force end turn" must not also render while cancelling.
    expect(screen.queryByRole("button", { name: /force end turn/i })).toBeNull();
  });

  it("renders an escalation countdown when a deadline is provided", () => {
    const at = new Date(Date.now() + 8000).toISOString();
    renderSpinner({
      stalledSecs: 1,
      tool: "Terminal",
      cancelling: true,
      cancelEscalatesAt: at,
    });
    expect(screen.getByText(/stopping… \(force in \d+s\)/i)).toBeTruthy();
  });

  it("Force stop invokes the force-end-turn handler", () => {
    const { onForceEndTurn } = renderSpinner({
      stalledSecs: 1,
      tool: "Terminal",
      cancelling: true,
    });
    screen.getByRole("button", { name: /force stop/i }).click();
    expect(onForceEndTurn).toHaveBeenCalledTimes(1);
  });

  it("does not show force controls for a slow tool that is NOT being cancelled (#1176 preserved)", () => {
    renderSpinner({ stalledSecs: 180, tool: "Task", cancelling: false });
    expect(screen.queryByRole("button", { name: /force stop/i })).toBeNull();
    expect(screen.queryByRole("button", { name: /force end turn/i })).toBeNull();
  });
});
