import { describe, expect, it } from "vitest";

import {
  anchorIsStale,
  autoLoadDecision,
  canOfferEarlier,
  earlierAction,
  HISTORY_AUTOLOAD_COOLDOWN_MS,
  HISTORY_PRELOAD_PX,
  scrollRestoreDelta,
} from "./historyScroll";

const base = {
  scrollTop: 0,
  clientHeight: 500,
  scrollHeight: 5000,
  armed: true,
  canLoadEarlier: true,
  now: 10_000,
  lastLoadAt: 0,
};

describe("autoLoadDecision", () => {
  it("fires at the top when armed, overflowing, and past the cooldown", () => {
    expect(autoLoadDecision(base)).toEqual({ armed: false, fire: true });
  });

  it("re-arms and does not fire away from the top", () => {
    expect(autoLoadDecision({ ...base, scrollTop: HISTORY_PRELOAD_PX + 1, armed: false })).toEqual({
      armed: true,
      fire: false,
    });
  });

  it("never fires (and arms) when the transcript does not overflow", () => {
    // scrollHeight within clientHeight + preload: nothing to scroll up.
    expect(autoLoadDecision({ ...base, scrollHeight: base.clientHeight + HISTORY_PRELOAD_PX })).toEqual({
      armed: true,
      fire: false,
    });
  });

  it("does not fire while disarmed (one load per arming)", () => {
    expect(autoLoadDecision({ ...base, armed: false })).toEqual({ armed: false, fire: false });
  });

  it("holds fire within the cooldown window", () => {
    expect(autoLoadDecision({ ...base, lastLoadAt: base.now - (HISTORY_AUTOLOAD_COOLDOWN_MS - 1) })).toEqual({
      armed: true,
      fire: false,
    });
  });

  it("does not fire when there is no older history left", () => {
    expect(autoLoadDecision({ ...base, canLoadEarlier: false })).toEqual({ armed: true, fire: false });
  });
});

describe("scrollRestoreDelta", () => {
  it("returns the growth delta when scrolled up", () => {
    expect(scrollRestoreDelta(1000, 1300, false)).toBe(300);
  });
  it("returns 0 when pinned to the bottom", () => {
    expect(scrollRestoreDelta(1000, 1300, true)).toBe(0);
  });
  it("returns 0 when nothing grew", () => {
    expect(scrollRestoreDelta(1300, 1300, false)).toBe(0);
    expect(scrollRestoreDelta(1300, 1000, false)).toBe(0);
  });
});

describe("earlierAction / canOfferEarlier", () => {
  it("reveals loaded rows before fetching", () => {
    expect(earlierAction(true, true)).toBe("reveal");
    expect(earlierAction(true, false)).toBe("reveal");
  });
  it("fetches when nothing loaded remains but the server has more", () => {
    expect(earlierAction(false, true)).toBe("fetch");
  });
  it("is a no-op when neither has more", () => {
    expect(earlierAction(false, false)).toBe("none");
  });
  it("offers the control when either source has more", () => {
    expect(canOfferEarlier(true, false)).toBe(true);
    expect(canOfferEarlier(false, true)).toBe(true);
    expect(canOfferEarlier(false, false)).toBe(false);
  });
});

describe("anchorIsStale", () => {
  it("is stale when settled with no growth", () => {
    // load done, anchor still equals current scrollHeight => nothing grew.
    expect(anchorIsStale(false, 1000, 1000)).toBe(true);
  });
  it("is not stale while a fetch is in flight", () => {
    expect(anchorIsStale(true, 1000, 1000)).toBe(false);
  });
  it("is not stale once the transcript grew", () => {
    expect(anchorIsStale(false, 1000, 1300)).toBe(false);
  });
  it("is not stale with no anchor set", () => {
    expect(anchorIsStale(false, null, 1000)).toBe(false);
  });
});
