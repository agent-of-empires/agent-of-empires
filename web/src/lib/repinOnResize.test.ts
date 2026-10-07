import { afterEach, describe, expect, it, vi } from "vitest";

import { repinOnResize } from "./repinOnResize";

class FakeResizeObserver {
  static instances: FakeResizeObserver[] = [];
  observed: unknown[] = [];
  disconnected = false;
  constructor(private readonly cb: () => void) {
    FakeResizeObserver.instances.push(this);
  }
  observe(target: unknown): void {
    this.observed.push(target);
  }
  unobserve(): void {}
  disconnect(): void {
    this.disconnected = true;
  }
  fire(): void {
    this.cb();
  }
}

function setup(initialHeight: number) {
  FakeResizeObserver.instances = [];
  vi.stubGlobal("ResizeObserver", FakeResizeObserver);
  const target = { tag: "viewport" } as unknown as Element;
  const state = { height: initialHeight, atBottom: true };
  const repin = vi.fn();
  const observer = repinOnResize({
    target,
    readHeight: () => state.height,
    wasAtBottom: () => state.atBottom,
    repin,
  });
  return { target, state, repin, observer, fake: FakeResizeObserver.instances[0] };
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("repinOnResize", () => {
  it("re-pins only when the observed height actually changed and the user was at the bottom", () => {
    const cases: Array<[number, boolean, boolean]> = [
      [150, true, true],
      [500, true, true],
      [150, false, false],
      [300, true, false],
    ];
    for (const [nextHeight, atBottom, expected] of cases) {
      const { state, repin, fake } = setup(300);
      state.height = nextHeight;
      state.atBottom = atBottom;
      fake.fire();
      expect(repin).toHaveBeenCalledTimes(expected ? 1 : 0);
    }
  });

  it("tracks the height it last saw, so a shrink then a restore both re-pin once", () => {
    const { state, repin, fake } = setup(300);
    state.height = 150;
    fake.fire();
    fake.fire();
    expect(repin).toHaveBeenCalledTimes(1);
    state.height = 300;
    fake.fire();
    expect(repin).toHaveBeenCalledTimes(2);
  });

  it("observes the requested target and hands back the observer the caller must disconnect", () => {
    const { target, observer, fake } = setup(300);
    expect(fake.observed).toEqual([target]);
    expect(observer).toBe(fake as unknown as ResizeObserver);
    observer.disconnect();
    expect(fake.disconnected).toBe(true);
  });
});
