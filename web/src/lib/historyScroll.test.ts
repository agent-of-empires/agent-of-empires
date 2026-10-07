import { expect, it } from "vitest";

import {
  anchorIsStale,
  autoLoadDecision,
  canOfferEarlier,
  earlierAction,
  HISTORY_AUTOLOAD_COOLDOWN_MS,
  HISTORY_PRELOAD_PX,
  isPinnedToBottom,
  PINNED_BOTTOM_SLOP_PX,
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

it("autoLoadDecision fires once per arming at the top of an overflowing transcript", () => {
  const cases: [string, Partial<typeof base>, { armed: boolean; fire: boolean }][] = [
    ["armed at the top past the cooldown", {}, { armed: false, fire: true }],
    ["away from the top re-arms", { scrollTop: HISTORY_PRELOAD_PX + 1, armed: false }, { armed: true, fire: false }],
    ["no overflow", { scrollHeight: base.clientHeight + HISTORY_PRELOAD_PX }, { armed: true, fire: false }],
    ["disarmed", { armed: false }, { armed: false, fire: false }],
    [
      "within the cooldown",
      { lastLoadAt: base.now - (HISTORY_AUTOLOAD_COOLDOWN_MS - 1) },
      { armed: true, fire: false },
    ],
    ["no older history", { canLoadEarlier: false }, { armed: true, fire: false }],
  ];
  for (const [name, over, expected] of cases) expect(autoLoadDecision({ ...base, ...over }), name).toEqual(expected);
});

it("isPinnedToBottom allows the slop and treats non-overflowing content as pinned", () => {
  const cases: [number, number, number, boolean][] = [
    [4500, 500, 5000, true],
    [4500 - PINNED_BOTTOM_SLOP_PX, 500, 5000, true],
    [4500 - PINNED_BOTTOM_SLOP_PX - 1, 500, 5000, false],
    [0, 500, 5000, false],
    [0, 800, 800, true],
    [0, 800, 400, true],
  ];
  for (const [top, client, height, expected] of cases) {
    expect(isPinnedToBottom(top, client, height), `${top}/${client}/${height}`).toBe(expected);
  }
});

it("scrollRestoreDelta compensates growth only when scrolled up", () => {
  expect(scrollRestoreDelta(1000, 1300, false)).toBe(300);
  expect(scrollRestoreDelta(1000, 1300, true)).toBe(0);
  expect(scrollRestoreDelta(1300, 1300, false)).toBe(0);
  expect(scrollRestoreDelta(1300, 1000, false)).toBe(0);
});

it("earlierAction reveals loaded rows before fetching; canOfferEarlier needs either source", () => {
  expect(earlierAction(true, true)).toBe("reveal");
  expect(earlierAction(true, false)).toBe("reveal");
  expect(earlierAction(false, true)).toBe("fetch");
  expect(earlierAction(false, false)).toBe("none");
  expect(canOfferEarlier(true, false)).toBe(true);
  expect(canOfferEarlier(false, true)).toBe(true);
  expect(canOfferEarlier(false, false)).toBe(false);
});

it("anchorIsStale only once settled with an anchor and no growth", () => {
  expect(anchorIsStale(false, 1000, 1000)).toBe(true);
  expect(anchorIsStale(true, 1000, 1000)).toBe(false);
  expect(anchorIsStale(false, 1000, 1300)).toBe(false);
  expect(anchorIsStale(false, null, 1000)).toBe(false);
});
