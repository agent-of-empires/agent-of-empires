// @vitest-environment jsdom
import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { beginRead, markReadApplied } from "../../lib/sessionReads";
import { usePendingSetting } from "./usePendingSetting";

function setup() {
  const saves: ((ok: boolean) => void)[] = [];
  const save = () => new Promise<boolean>((settle) => saves.push(settle));
  const onError = vi.fn();
  const hook = renderHook(({ server }) => usePendingSetting(server, save, onError), {
    initialProps: { server: "x" },
  });
  const value = () => hook.result.current[0];
  const pick = (next: string) => act(() => hook.result.current[1](next));
  const deliver = (read: number, server: string) => {
    act(() => markReadApplied(read));
    hook.rerender({ server });
  };
  const poll = (server: string) => deliver(beginRead(), server);
  const settle = async (i: number, ok: boolean) => act(async () => saves[i]!(ok));
  return { value, pick, poll, deliver, settle, onError };
}

describe("usePendingSetting", () => {
  it("holds a pick until the server moves, and lets another writer's change through", async () => {
    const h = setup();
    h.pick("a");
    h.poll("x");
    expect(h.value()).toBe("a");
    await h.settle(0, true);
    h.poll("b");
    expect(h.value()).toBe("b");
  });

  it("does not flash an earlier pick landing while a later one is in flight", () => {
    const h = setup();
    h.pick("a");
    h.pick("b");
    h.poll("a");
    expect(h.value()).toBe("b");
    h.poll("b");
    h.poll("c");
    expect(h.value()).toBe("c");
  });

  it("reverts only when the latest save fails", async () => {
    const h = setup();
    h.pick("a");
    h.pick("b");
    await h.settle(0, false);
    expect(h.value()).toBe("b");
    expect(h.onError).not.toHaveBeenCalled();
    await h.settle(1, false);
    expect(h.value()).toBe("x");
    expect(h.onError).toHaveBeenCalledOnce();
  });

  it("yields to a later read of an earlier pick once every save is acknowledged", async () => {
    const h = setup();
    h.pick("red");
    h.pick("green");
    await h.settle(0, true);
    await h.settle(1, true);
    h.poll("red");
    expect(h.value()).toBe("red");
  });

  it("ignores a read that began before the saves settled", async () => {
    const h = setup();
    const stale = beginRead();
    h.pick("a");
    await h.settle(0, true);
    h.deliver(stale, "x");
    expect(h.value()).toBe("a");
    h.poll("x");
    expect(h.value()).toBe("x");
  });

  it("follows another writer returning to the original value", async () => {
    const h = setup();
    h.pick("a");
    await h.settle(0, true);
    h.poll("a");
    h.poll("x");
    expect(h.value()).toBe("x");
  });

  it("keeps the latest pick through A, B, A while saves are in flight", async () => {
    const h = setup();
    h.pick("b");
    h.pick("x");
    h.poll("b");
    expect(h.value()).toBe("x");
    await h.settle(0, true);
    await h.settle(1, true);
    h.poll("x");
    expect(h.value()).toBe("x");
  });

  it("ignores an older failure after a newer pick", async () => {
    const h = setup();
    h.pick("a");
    h.pick("b");
    await h.settle(1, true);
    await h.settle(0, false);
    expect(h.value()).toBe("b");
    expect(h.onError).not.toHaveBeenCalled();
  });

  it("does not flicker when a stale read echoes the last settled pick", async () => {
    const h = setup();
    h.pick("a");
    await h.settle(0, true);
    h.pick("b");
    h.poll("a");
    expect(h.value()).toBe("b");
    await h.settle(1, true);
    h.pick("x");
    h.poll("b");
    expect(h.value()).toBe("x");
  });
});
