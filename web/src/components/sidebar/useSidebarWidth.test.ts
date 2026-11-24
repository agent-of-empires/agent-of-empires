import { describe, expect, it } from "vitest";
import { layoutForKey, snapForPointer, type SidebarLayout, type SidebarSnap } from "./useSidebarWidth";

describe("snapForPointer", () => {
  it.each<[number, SidebarSnap]>([
    [600, { mode: "full", width: 480 }],
    [321.4, { mode: "full", width: 321 }],
    // Between the snap point and the minimum the sidebar holds at the minimum.
    [200, { mode: "full", width: 200 }],
    [144, { mode: "full", width: 200 }],
    [143, { mode: "compact" }],
    [44, { mode: "compact" }],
    [43, { mode: "closed" }],
    [-20, { mode: "closed" }],
  ])("x=%d", (x, expected) => {
    expect(snapForPointer(x)).toEqual(expected);
  });
});

describe("layoutForKey", () => {
  const full = (width: number): SidebarLayout => ({ compact: false, width });
  const rail = (width: number): SidebarLayout => ({ compact: true, width });

  it.each<[string, SidebarLayout, string, SidebarLayout | null]>([
    ["narrows by a step", full(280), "ArrowLeft", full(264)],
    ["stops at the minimum", full(210), "ArrowLeft", full(200)],
    ["snaps to the rail from the minimum", full(200), "ArrowLeft", rail(200)],
    ["does not close from the rail", rail(300), "ArrowLeft", null],
    ["widens by a step", full(280), "ArrowRight", full(296)],
    ["stops at the maximum", full(475), "ArrowRight", full(480)],
    ["leaves the rail at the width it kept", rail(300), "ArrowRight", full(300)],
    ["Home goes to the rail", full(300), "Home", rail(300)],
    ["End goes to the maximum", rail(300), "End", full(480)],
    ["Enter collapses", full(300), "Enter", rail(300)],
    ["Enter restores", rail(300), "Enter", full(300)],
    ["ignores other keys", full(300), "a", null],
  ])("%s", (_name, layout, key, expected) => {
    expect(layoutForKey(layout, key)).toEqual(expected);
  });
});
