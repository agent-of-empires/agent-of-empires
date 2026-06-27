import { describe, expect, it } from "vitest";

import {
  centerX,
  pointerInsertsAfter,
  resolvePlacement,
  shouldApplyPlacement,
  visibleToFullIndex,
  type PlacementOver,
} from "../paneDnd";

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

describe("centerX", () => {
  it("returns the horizontal center", () => {
    expect(centerX({ left: 10, width: 40 })).toBe(30);
  });
  it("returns null for a missing rect", () => {
    expect(centerX(null)).toBeNull();
    expect(centerX(undefined)).toBeNull();
  });
});

describe("pointerInsertsAfter", () => {
  it("is true when the dragged center is past the hovered center", () => {
    expect(pointerInsertsAfter({ left: 50, width: 20 }, { left: 0, width: 20 })).toBe(true);
  });
  it("is false on the leading half", () => {
    expect(pointerInsertsAfter({ left: 0, width: 20 }, { left: 50, width: 20 })).toBe(false);
  });
  it("is false when a rect is unknown", () => {
    expect(pointerInsertsAfter(null, { left: 0, width: 20 })).toBe(false);
  });
});

describe("shouldApplyPlacement", () => {
  it("applies a cross-dock move", () => {
    expect(shouldApplyPlacement(tabsByDock, "diff", { dock: "bottom", index: 0 }, "right")).toBe(true);
  });
  it("applies a within-dock move to a different slot", () => {
    expect(shouldApplyPlacement(tabsByDock, "diff", { dock: "right", index: 2 }, "right")).toBe(true);
  });
  it("skips a within-dock drop onto the tab's own slot", () => {
    // diff is at index 0; a post-removal target index of 0 is a no-op.
    expect(shouldApplyPlacement(tabsByDock, "diff", { dock: "right", index: 0 }, "right")).toBe(false);
  });
  it("skips when the tab is not in the target dock", () => {
    expect(shouldApplyPlacement(tabsByDock, "ghost", { dock: "right", index: 0 }, "right")).toBe(false);
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
