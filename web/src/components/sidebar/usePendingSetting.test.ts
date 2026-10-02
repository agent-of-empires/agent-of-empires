// @vitest-environment jsdom
import { act, renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { usePendingSetting } from "./usePendingSetting";

function setup(initial: string | null = "x") {
  const saves: ((ok: boolean) => void)[] = [];
  const save = () => new Promise<boolean>((settle) => saves.push(settle));
  const onError = vi.fn();
  const hook = renderHook(({ server }) => usePendingSetting(server, save, onError), {
    initialProps: { server: initial as string | null },
  });
  const value = () => hook.result.current[0];
  const pick = (next: string | null) => act(() => hook.result.current[1](next));
  const poll = (server: string | null) => hook.rerender({ server });
  const settle = async (i: number, ok: boolean) => act(async () => saves[i]!(ok));
  return { value, pick, poll, settle, onError };
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

  // Returning to the server value cancels the earlier pick, so the row must keep reading "no color"
  // even while that first pick's PATCH is still in flight and the next poll reports it.
  it("holds a pick that returns to the server value while an earlier pick is still in flight", () => {
    const h = setup(null);
    h.pick("red");
    h.pick(null);
    expect(h.value()).toBeNull();
    h.poll("red");
    expect(h.value()).toBeNull();
    // Holding through the echo must not mask a genuine change from another writer.
    h.poll("blue");
    expect(h.value()).toBe("blue");
  });
});
