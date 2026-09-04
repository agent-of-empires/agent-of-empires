import { afterEach, describe, expect, it, vi } from "vitest";

import { repinOnResize } from "./repinOnResize";

/** Records every observer built during a test so the resize callbacks can be
 *  fired on demand; jsdom has no layout engine, so nothing fires on its own. */
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
  /** Stand-in for the browser delivering a resize entry. */
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
    // (height after the resize, pinned before it) -> re-pinned?
    // Starting height is 300 and every case runs against a fresh subscription.
    const cases: Array<[number, boolean, boolean]> = [
      // The case this exists for: chrome expanded, the viewport shrank, and
      // the browser does not clamp scrollTop back down for us.
      [150, true, true],
      // Growing is self-correcting, but re-pinning is still correct and keeps
      // the transcript bottom under the composer.
      [500, true, true],
      // The user deliberately scrolled up: a resize must not yank them down.
      [150, false, false],
      // ResizeObserver also fires on width-only changes and on resizes that
      // net to zero; neither can move the scroll bottom.
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
    // Same height again: the observer must not treat it as a second change.
    fake.fire();
    expect(repin).toHaveBeenCalledTimes(1);
    state.height = 300;
    fake.fire();
    expect(repin).toHaveBeenCalledTimes(2);
  });

  it("observes the requested target and hands back the observer the caller must disconnect", () => {
    const { target, observer, fake } = setup(300);
    expect(fake.observed).toEqual([target]);
    // The returned handle has to be the live observer, or the caller's effect
    // cleanup would leak a subscription on every remount.
    expect(observer).toBe(fake as unknown as ResizeObserver);
    observer.disconnect();
    expect(fake.disconnected).toBe(true);
  });
});
