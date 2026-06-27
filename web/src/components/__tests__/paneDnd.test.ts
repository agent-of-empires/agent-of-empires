import { describe, expect, it } from "vitest";

import { resolvePlacement, visibleToFullIndex, type PlacementOver } from "../paneDnd";

const tabsByDock = { right: ["diff", "terminal:0", "terminal:1"], bottom: ["plugin:p:a"] };

function over(partial: Partial<PlacementOver>): PlacementOver {
  return { type: "pane-tab", dock: "right", tabId: "diff", after: false, ...partial };
}

describe("resolvePlacement", () => {
  it("inserts before the hovered tab when the pointer is on its leading half", () => {
    // Dragging terminal:1 onto diff's leading half; base (without the dragged
    // tab) is [diff, terminal:0], so before diff is index 0.
    expect(resolvePlacement(over({ tabId: "diff", after: false }), "terminal:1", tabsByDock)).toEqual({
      dock: "right",
      index: 0,
    });
  });

  it("inserts after the hovered tab when the pointer is on its trailing half", () => {
    expect(resolvePlacement(over({ tabId: "diff", after: true }), "terminal:1", tabsByDock)).toEqual({
      dock: "right",
      index: 1,
    });
  });

  it("appends when dropping on a dock body rather than a tab", () => {
    // base without the dragged diff is [terminal:0, terminal:1], so append is 2.
    expect(resolvePlacement(over({ type: "pane-dock", tabId: "" }), "diff", tabsByDock)).toEqual({
      dock: "right",
      index: 2,
    });
  });

  it("appends to an empty-dock zone", () => {
    expect(resolvePlacement(over({ type: "pane-empty-dock", dock: "bottom", tabId: "" }), "diff", tabsByDock)).toEqual({
      dock: "bottom",
      index: 1,
    });
  });

  it("appends when the hovered tab is not in the destination (cross-dock to a stale id)", () => {
    expect(resolvePlacement(over({ dock: "bottom", tabId: "ghost" }), "diff", tabsByDock)).toEqual({
      dock: "bottom",
      index: 1,
    });
  });
});

describe("visibleToFullIndex", () => {
  const visible = (id: string) => !id.startsWith("plugin:");

  it("is the identity when every tab is visible", () => {
    expect(visibleToFullIndex(["diff", "terminal:0"], 1, visible)).toBe(1);
  });

  it("skips a hidden tab that still holds a persisted slot", () => {
    // Full base [diff, plugin:p:x(hidden), terminal:0]: visible slot 1 is the
    // terminal at full index 2.
    expect(visibleToFullIndex(["diff", "plugin:p:x", "terminal:0"], 1, visible)).toBe(2);
  });

  it("appends to the full length when the visible index is at or past the end", () => {
    expect(visibleToFullIndex(["diff", "plugin:p:x"], 1, visible)).toBe(2);
  });
});
