// @vitest-environment jsdom
import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { RuntimeCursor, SessionReceipt } from "../../lib/api";
import { usePendingSetting } from "./usePendingSetting";

const cursor = (revision: bigint, epoch = "boot"): RuntimeCursor => ({ epoch, revision });
const receipts = (revision: bigint, id = "A"): SessionReceipt[] => [{ id, cursor: cursor(revision) }];
afterEach(cleanup);

function setup(initial: string | null = "x") {
  const saves: ((result: SessionReceipt[] | null) => void)[] = [];
  const save = vi.fn(() => new Promise<SessionReceipt[] | null>((resolve) => saves.push(resolve)));
  const onError = vi.fn();
  const hook = renderHook(
    ({ server, observedById, identity }) => usePendingSetting(server, save, onError, observedById, identity),
    {
      initialProps: {
        server: initial as string | null,
        observedById: { A: cursor(10n) } as Record<string, RuntimeCursor>,
        identity: "A:boot",
      },
    },
  );
  const value = () => hook.result.current[0];
  const pick = async (next: string | null) => {
    await act(async () => hook.result.current[1](next));
  };
  const poll = (
    server: string | null,
    observedById: Record<string, RuntimeCursor> = { A: cursor(10n) },
    identity = "A:boot",
  ) => hook.rerender({ server, observedById, identity });
  const settle = async (index: number, result: SessionReceipt[] | null) => {
    await act(async () => saves[index]!(result));
  };
  return { value, pick, poll, settle, save, onError, unmount: hook.unmount };
}

describe("usePendingSetting", () => {
  it("holds a pick through stale polls and permits a distinct other writer", async () => {
    const h = setup();
    await h.pick("a");
    h.poll("x");
    expect(h.value()).toBe("a");
    h.poll("b", { A: cursor(11n) });
    expect(h.value()).toBe("b");
    await h.settle(0, receipts(12n));
    expect(h.value()).toBe("b");
  });

  it("serializes saves while showing the latest pick immediately; an older failure cannot erase it", async () => {
    const h = setup();
    await h.pick("a");
    await h.pick("b");
    expect(h.save).toHaveBeenCalledTimes(1);
    expect(h.value()).toBe("b");
    await h.settle(0, null);
    expect(h.save).toHaveBeenCalledTimes(2);
    expect(h.value()).toBe("b");
    expect(h.onError).not.toHaveBeenCalled();
    await h.settle(1, null);
    expect(h.value()).toBe("x");
    expect(h.onError).toHaveBeenCalledOnce();
  });

  it.each([null, "default"])(
    "does not resurrect an old pick after cancellation to %s, and releases without any value-changing poll",
    async (initial) => {
      const h = setup(initial);
      await h.pick("red");
      await h.pick(initial);
      h.poll("red", { A: cursor(11n) });
      expect(h.value()).toBe(initial);
      await h.settle(0, receipts(11n));
      expect(h.value()).toBe(initial);
      await h.settle(1, receipts(12n));
      expect(h.value()).toBe(initial);
      h.poll(initial, { A: cursor(12n) });
      expect(h.value()).toBe(initial);
      // Same historical value is a real writer after the latest receipt was observed.
      h.poll("red", { A: cursor(13n) });
      expect(h.value()).toBe("red");
    },
  );

  it("waits for every captured id, never for another row's higher revision", async () => {
    const h = setup(null);
    await h.pick("red");
    await h.pick(null);
    h.poll("red", { A: cursor(10n), B: cursor(12n), C: cursor(99n) });
    await h.settle(0, receipts(10n));
    await h.settle(1, [
      { id: "A", cursor: cursor(11n) },
      { id: "B", cursor: cursor(12n) },
    ]);
    expect(h.value()).toBeNull();
    h.poll("red", { A: cursor(11n), B: cursor(12n), C: cursor(99n) });
    expect(h.value()).toBe("red");
  });

  it("exposes a newer canonical same-id value rather than imposing the requested value", async () => {
    const h = setup(null);
    await h.pick("red");
    await h.pick(null);
    h.poll("red", { A: cursor(13n) });
    await h.settle(0, receipts(10n));
    expect(h.value()).toBeNull();
    await h.settle(1, receipts(11n));
    expect(h.value()).toBe("red");
  });

  it("ignores disposed/old-epoch completions and abandons queued saves on identity change", async () => {
    const h = setup(null);
    await h.pick("red");
    await h.pick(null);
    h.poll("green", { A: cursor(1n, "new") }, "A:new");
    expect(h.value()).toBe("green");
    await h.settle(0, receipts(99n));
    expect(h.save).toHaveBeenCalledTimes(1);
    expect(h.value()).toBe("green");
    await h.pick("amber");
    h.unmount();
    await h.settle(1, null);
    expect(h.onError).not.toHaveBeenCalled();
  });
});
