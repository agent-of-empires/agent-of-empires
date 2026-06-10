import { describe, expect, it } from "vitest";
import { WheelAccumulator } from "./wheelScroll";

const CELL_H = 16.8; // 14px font * 1.2
const ROWS = 40;

const feed = (acc: WheelAccumulator, deltaY: number, deltaMode: number, ts: number) =>
  acc.feed({ deltaY, deltaMode, timeStamp: ts }, CELL_H, ROWS);

describe("WheelAccumulator", () => {
  it("fires the first event after one line of travel from rest", () => {
    const acc = new WheelAccumulator();
    // A trackpad swipe: small pixel deltas. 4px + 4px + 4px < one line.
    expect(feed(acc, 4, 0, 0)).toBe(0);
    expect(feed(acc, 4, 0, 10)).toBe(0);
    expect(feed(acc, 4, 0, 20)).toBe(0);
    // Crossing one line height (16.8px) emits immediately.
    expect(feed(acc, 6, 0, 30)).toBe(1);
  });

  it("sustains pixel scrolling at five lines of travel per event (1:1 content speed)", () => {
    const acc = new WheelAccumulator();
    expect(feed(acc, 20, 0, 0)).toBe(1); // first event, travel absorbed
    // Sustained cadence: 5 * 16.8 = 84px per event.
    expect(feed(acc, 80, 0, 10)).toBe(0);
    expect(feed(acc, 10, 0, 20)).toBe(1);
    expect(feed(acc, 168, 0, 30)).toBe(2);
  });

  it("maps one line-mode notch to one event regardless of the OS lines-per-notch", () => {
    const acc = new WheelAccumulator();
    // Windows wheel settings can make line-mode notches report deltaY=1
    // (or any other value); the cadence must stay one event per notch.
    expect(feed(acc, 1, 1, 0)).toBe(1);
    expect(feed(acc, 1, 1, 50)).toBe(1);
    expect(feed(acc, 1, 1, 100)).toBe(1);
  });

  it("maps one Firefox line-mode notch to one event, every notch", () => {
    const acc = new WheelAccumulator();
    // Firefox wheel mice: deltaMode=1, deltaY=3 per notch.
    expect(feed(acc, 3, 1, 0)).toBe(1);
    expect(feed(acc, 3, 1, 50)).toBe(1);
    expect(feed(acc, 3, 1, 100)).toBe(1);
    expect(feed(acc, -3, 1, 150)).toBe(-1);
  });

  it("converts page-mode deltas through the viewport height", () => {
    const acc = new WheelAccumulator();
    // One page = rows * cellH px; well past every threshold.
    expect(feed(acc, 1, 2, 0)).toBe(1);
    expect(feed(acc, 1, 2, 10)).toBeGreaterThanOrEqual(1);
  });

  it("scrolls up with negative deltas", () => {
    const acc = new WheelAccumulator();
    expect(feed(acc, -20, 0, 0)).toBe(-1);
    expect(feed(acc, -84, 0, 10)).toBe(-1);
  });

  it("resets the dead zone after the wheel goes idle", () => {
    const acc = new WheelAccumulator();
    expect(feed(acc, 20, 0, 0)).toBe(1);
    expect(feed(acc, 40, 0, 10)).toBe(0); // partial travel banked
    // After an idle pause the bank is dropped and the next gesture
    // starts fresh: first event again needs only one line.
    expect(feed(acc, 4, 0, 1000)).toBe(0);
    expect(feed(acc, 14, 0, 1010)).toBe(1);
  });

  it("drops banked travel when the direction flips", () => {
    const acc = new WheelAccumulator();
    expect(feed(acc, 20, 0, 0)).toBe(1);
    expect(feed(acc, 60, 0, 10)).toBe(0); // 60px banked downward
    // Reversing should not need to pay back the banked 60px first.
    expect(feed(acc, -20, 0, 20)).toBe(-1);
  });

  it("treats a zero-height cell defensively", () => {
    const acc = new WheelAccumulator();
    expect(() => acc.feed({ deltaY: 10, deltaMode: 0, timeStamp: 0 }, 0, ROWS)).not.toThrow();
  });
});
