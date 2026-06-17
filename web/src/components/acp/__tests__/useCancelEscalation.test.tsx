// @vitest-environment jsdom

import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { useCancelEscalation } from "../useCancelEscalation";

// Regression guard for #2237: the Stop button must escalate to force-end on a
// second press without depending on the server-confirmed `cancelling` flag,
// and the local intent must reset on turn end (turnSeq bump) and on session
// switch so it never leaks across turns or sessions.
describe("useCancelEscalation (#2237)", () => {
  function setup(initial: { sessionId?: string; turnSeq?: number; cancelling?: boolean } = {}) {
    const cancelPrompt = vi.fn().mockResolvedValue(undefined);
    const forceEndTurn = vi.fn().mockResolvedValue(undefined);
    const { result, rerender } = renderHook(
      ({ sessionId, turnSeq, cancelling }) =>
        useCancelEscalation(sessionId, turnSeq, cancelling, cancelPrompt, forceEndTurn),
      {
        initialProps: {
          sessionId: initial.sessionId ?? "s-1",
          turnSeq: initial.turnSeq ?? 1,
          cancelling: initial.cancelling ?? false,
        },
      },
    );
    return { result, rerender, cancelPrompt, forceEndTurn };
  }

  it("first press sends a graceful cancel, second press force-ends", async () => {
    const { result, cancelPrompt, forceEndTurn } = setup();
    await act(async () => {
      await result.current();
    });
    expect(cancelPrompt).toHaveBeenCalledTimes(1);
    expect(forceEndTurn).not.toHaveBeenCalled();

    await act(async () => {
      await result.current();
    });
    expect(forceEndTurn).toHaveBeenCalledTimes(1);
    expect(cancelPrompt).toHaveBeenCalledTimes(1);
  });

  it("escalates to force on the first press when the server already confirmed a cancel", async () => {
    const { result, cancelPrompt, forceEndTurn } = setup({ cancelling: true });
    await act(async () => {
      await result.current();
    });
    expect(forceEndTurn).toHaveBeenCalledTimes(1);
    expect(cancelPrompt).not.toHaveBeenCalled();
  });

  it("resets the local intent when a new turn starts (turnSeq bumps)", async () => {
    const { result, rerender, cancelPrompt, forceEndTurn } = setup({ turnSeq: 1 });
    await act(async () => {
      await result.current();
    });
    // Next turn: pendingUserPromptSeq advances.
    rerender({ sessionId: "s-1", turnSeq: 2, cancelling: false });
    await act(async () => {
      await result.current();
    });
    // Back to graceful cancel: the stale "already requested" intent does not
    // carry into the new turn.
    expect(cancelPrompt).toHaveBeenCalledTimes(2);
    expect(forceEndTurn).not.toHaveBeenCalled();
  });

  it("resets the local intent on a session switch without unmount", async () => {
    const { result, rerender, cancelPrompt, forceEndTurn } = setup({ sessionId: "s-1", turnSeq: 5 });
    await act(async () => {
      await result.current();
    });
    // Switch to a different session that happens to share the same turnSeq.
    rerender({ sessionId: "s-2", turnSeq: 5, cancelling: false });
    await act(async () => {
      await result.current();
    });
    // First press in the new session must be a graceful cancel, not a force.
    expect(cancelPrompt).toHaveBeenCalledTimes(2);
    expect(forceEndTurn).not.toHaveBeenCalled();
  });
});
