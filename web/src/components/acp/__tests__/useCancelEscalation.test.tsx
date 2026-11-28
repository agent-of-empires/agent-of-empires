// @vitest-environment jsdom

import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { nextCancelAction, useCancelEscalation } from "../useCancelEscalation";

// Only a confirmed or admitted cancellation permits the explicit local escape.
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
    const cancelPrompt = vi.fn().mockResolvedValue(true);
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
      await result.current.onCancel();
    });
    expect(cancelPrompt).toHaveBeenCalledTimes(1);
    expect(forceEndTurn).not.toHaveBeenCalled();

    await act(async () => {
      await result.current.onCancel();
    });
    expect(forceEndTurn).toHaveBeenCalledTimes(1);
    expect(cancelPrompt).toHaveBeenCalledTimes(1);
  });

  it("escalates to force on the first press when the server already confirmed a cancel", async () => {
    const { result, cancelPrompt, forceEndTurn } = setup({ cancelling: true });
    await act(async () => {
      await result.current.onCancel();
    });
    expect(forceEndTurn).toHaveBeenCalledTimes(1);
    expect(cancelPrompt).not.toHaveBeenCalled();
  });

  it("resets the local intent when a new turn starts (turnSeq bumps)", async () => {
    const { result, rerender, cancelPrompt, forceEndTurn } = setup({ turnSeq: 1 });
    await act(async () => {
      await result.current.onCancel();
    });
    rerender({ sessionId: "s-1", turnSeq: 2, cancelling: false });
    await act(async () => {
      await result.current.onCancel();
    });
    expect(cancelPrompt).toHaveBeenCalledTimes(2);
    expect(forceEndTurn).not.toHaveBeenCalled();
  });

  it("resets the local intent on a session switch without unmount", async () => {
    const { result, rerender, cancelPrompt, forceEndTurn } = setup({ sessionId: "s-1", turnSeq: 5 });
    await act(async () => {
      await result.current.onCancel();
    });
    rerender({ sessionId: "s-2", turnSeq: 5, cancelling: false });
    await act(async () => {
      await result.current.onCancel();
    });
    expect(cancelPrompt).toHaveBeenCalledTimes(2);
    expect(forceEndTurn).not.toHaveBeenCalled();
  });
  it("keeps repeated refused cancellations graceful and never arms local force", async () => {
    const { result, cancelPrompt, forceEndTurn } = setup();
    cancelPrompt.mockResolvedValue(false);
    await act(async () => {
      await result.current.onCancel();
    });
    expect(result.current.canForceEndTurn).toBe(false);
    await act(async () => {
      await result.current.onCancel();
    });
    expect(cancelPrompt).toHaveBeenCalledTimes(2);
    expect(forceEndTurn).not.toHaveBeenCalled();
    expect(result.current.canForceEndTurn).toBe(false);
  });

  it("coalesces gestures while admission is pending, then exposes accepted force reactively", async () => {
    const { result, cancelPrompt, forceEndTurn } = setup();
    const admission = Promise.withResolvers<boolean>();
    cancelPrompt.mockReturnValueOnce(admission.promise);
    let first!: Promise<void>;
    let overlapping!: Promise<void>;
    act(() => {
      first = result.current.onCancel();
      overlapping = result.current.onCancel();
    });
    expect(cancelPrompt).toHaveBeenCalledTimes(1);
    expect(forceEndTurn).not.toHaveBeenCalled();
    expect(result.current.canForceEndTurn).toBe(false);
    await act(async () => {
      admission.resolve(true);
      await Promise.all([first, overlapping]);
    });
    expect(result.current.canForceEndTurn).toBe(true);
    await act(async () => {
      await result.current.onCancel();
    });
    expect(forceEndTurn).toHaveBeenCalledTimes(1);
    expect(cancelPrompt).toHaveBeenCalledTimes(1);
  });

  it("honors server-confirmed cancellation before the first HTTP response settles", async () => {
    const { result, rerender, cancelPrompt, forceEndTurn } = setup();
    const admission = Promise.withResolvers<boolean>();
    cancelPrompt.mockReturnValueOnce(admission.promise);
    const stop = result.current.onCancel;
    let first!: Promise<void>;
    let second!: Promise<void>;
    act(() => {
      first = stop();
    });
    expect(cancelPrompt).toHaveBeenCalledTimes(1);
    rerender({ sessionId: "s-1", turnSeq: 1, cancelling: true });
    expect(result.current.canForceEndTurn).toBe(true);
    act(() => {
      second = stop();
    });
    expect(forceEndTurn).toHaveBeenCalledTimes(1);
    expect(cancelPrompt).toHaveBeenCalledTimes(1);
    await act(async () => {
      admission.resolve(true);
      await Promise.all([first, second]);
    });
  });

  it.each([
    ["session", false],
    ["session", true],
    ["turn", false],
    ["turn", true],
  ] as const)("late %s result cannot grant or erase a newer admission (%s)", async (changed, newerAccepted) => {
    const { result, rerender, cancelPrompt, forceEndTurn } = setup();
    const oldAdmission = Promise.withResolvers<boolean>();
    cancelPrompt.mockReturnValueOnce(oldAdmission.promise);
    let old!: Promise<void>;
    act(() => {
      old = result.current.onCancel();
    });
    rerender({
      sessionId: changed === "session" ? "s-2" : "s-1",
      turnSeq: changed === "turn" ? 2 : 1,
      cancelling: false,
    });
    expect(result.current.canForceEndTurn).toBe(false);
    if (newerAccepted)
      await act(async () => {
        await result.current.onCancel();
      });
    await act(async () => {
      oldAdmission.resolve(true);
      await old;
    });
    expect(result.current.canForceEndTurn).toBe(newerAccepted);
    await act(async () => {
      await result.current.onCancel();
    });
    expect(forceEndTurn).toHaveBeenCalledTimes(Number(newerAccepted));
    expect(cancelPrompt).toHaveBeenCalledTimes(2);
  });

  it.each([
    ["session", false],
    ["session", true],
    ["turn", false],
    ["turn", true],
  ] as const)("committed %s A-to-B-to-A does not reuse an old admission (%s)", async (changed, newerAccepted) => {
    const { result, rerender, cancelPrompt, forceEndTurn } = setup();
    const oldAdmission = Promise.withResolvers<boolean>();
    cancelPrompt.mockReturnValueOnce(oldAdmission.promise);
    const oldStop = result.current.onCancel;
    let old!: Promise<void>;
    act(() => {
      old = oldStop();
    });
    rerender({
      sessionId: changed === "session" ? "s-2" : "s-1",
      turnSeq: changed === "turn" ? 2 : 1,
      cancelling: false,
    });
    rerender({ sessionId: "s-1", turnSeq: 1, cancelling: false });
    expect(result.current.canForceEndTurn).toBe(false);
    if (newerAccepted) {
      let current!: Promise<void>;
      act(() => {
        current = result.current.onCancel();
      });
      expect(cancelPrompt).toHaveBeenCalledTimes(2);
      await act(async () => {
        await current;
      });
    }
    await act(async () => {
      oldAdmission.resolve(true);
      await old;
    });
    expect(result.current.canForceEndTurn).toBe(newerAccepted);
    await act(async () => {
      await oldStop();
    });
    expect(forceEndTurn).not.toHaveBeenCalled();
    expect(cancelPrompt).toHaveBeenCalledTimes(newerAccepted ? 2 : 1);
    await act(async () => {
      await result.current.onCancel();
    });
    expect(forceEndTurn).toHaveBeenCalledTimes(Number(newerAccepted));
    expect(cancelPrompt).toHaveBeenCalledTimes(2);
  });
});
