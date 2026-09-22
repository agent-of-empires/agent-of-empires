// @vitest-environment jsdom

import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { nextCancelAction, useCancelEscalation } from "../useCancelEscalation";

// A second Stop must force-end without the server's `cancelling` confirmation,
// which never arrives for an orphaned turn; the intent resets per turn and session.
describe("nextCancelAction", () => {
  it.each([
    [false, false, "cancel"],
    [false, true, "force"],
    [true, false, "force"],
    [true, true, "force"],
  ])("cancelling=%s alreadyRequested=%s -> %s", (cancelling, alreadyRequested, expected) => {
    expect(nextCancelAction(cancelling, alreadyRequested)).toBe(expected);
  });
});

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
    rerender({ sessionId: "s-1", turnSeq: 2, cancelling: false });
    await act(async () => {
      await result.current();
    });
    expect(cancelPrompt).toHaveBeenCalledTimes(2);
    expect(forceEndTurn).not.toHaveBeenCalled();
  });

  it("resets the local intent on a session switch without unmount", async () => {
    const { result, rerender, cancelPrompt, forceEndTurn } = setup({ sessionId: "s-1", turnSeq: 5 });
    await act(async () => {
      await result.current();
    });
    rerender({ sessionId: "s-2", turnSeq: 5, cancelling: false });
    await act(async () => {
      await result.current();
    });
    expect(cancelPrompt).toHaveBeenCalledTimes(2);
    expect(forceEndTurn).not.toHaveBeenCalled();
  });
});
