import { describe, expect, it } from "vitest";

import {
  centerX,
  pointerInsertsAfter,
  resolvePlacement,
  shouldApplyPlacement,
  visibleToFullIndex,
  type PlacementOver,
  type RenderGroup,
} from "../paneDnd";

const groupsByDock: Record<"right" | "bottom", RenderGroup[]> = {
  right: [{ group: 0, tabs: ["diff", "terminal:0", "terminal:1"] }],
  bottom: [{ group: 0, tabs: ["plugin:p:a"] }],
};

function over(partial: Partial<PlacementOver>): PlacementOver {
  return { type: "pane-tab", dock: "right", group: 0, tabId: "diff", after: false, ...partial };
}

describe("resolvePlacement", () => {
  it("inserts before the hovered tab when the pointer is on its leading half", () => {
    // Dragging terminal:1 onto diff's leading half; base (without the dragged
    // tab) is [diff, terminal:0], so before diff is index 0.
    expect(resolvePlacement(over({ tabId: "diff", after: false }), "terminal:1", groupsByDock)).toEqual({
      dock: "right",
      group: 0,
      index: 0,
    });
  });

  it("inserts after the hovered tab when the pointer is on its trailing half", () => {
    expect(resolvePlacement(over({ tabId: "diff", after: true }), "terminal:1", groupsByDock)).toEqual({
      dock: "right",
      group: 0,
      index: 1,
    });
  });

  it("appends when dropping on a group strip rather than a tab", () => {
    // base without the dragged diff is [terminal:0, terminal:1], so append is 2.
    expect(resolvePlacement(over({ type: "pane-group", tabId: "" }), "diff", groupsByDock)).toEqual({
      dock: "right",
      group: 0,
      index: 2,
    });
  });

  it("splits into a new group before the hovered group", () => {
    expect(
      resolvePlacement(over({ type: "pane-split", side: "before", group: 0, tabId: "" }), "diff", groupsByDock),
    ).toEqual({
      dock: "right",
      group: 0,
      newGroup: true,
    });
  });

  it("splits into a new group after the hovered group", () => {
    expect(
      resolvePlacement(over({ type: "pane-split", side: "after", group: 0, tabId: "" }), "diff", groupsByDock),
    ).toEqual({
      dock: "right",
      group: 1,
      newGroup: true,
    });
  });

  it("seeds the first group on an empty-dock zone", () => {
    expect(
      resolvePlacement(over({ type: "pane-empty-dock", dock: "bottom", group: 0, tabId: "" }), "diff", groupsByDock),
    ).toEqual({
      dock: "bottom",
      group: 0,
      newGroup: true,
    });
  });

  it("treats a drop onto the dragged tab's own tab as a no-op, keeping its slot", () => {
    // terminal:0 sits at index 1; dropping it on itself must not append it.
    expect(resolvePlacement(over({ tabId: "terminal:0", after: true }), "terminal:0", groupsByDock)).toEqual({
      dock: "right",
      group: 0,
      index: 1,
    });
  });

  it("appends when the hovered tab is not in the destination group (stale id)", () => {
    expect(resolvePlacement(over({ dock: "bottom", group: 0, tabId: "ghost" }), "diff", groupsByDock)).toEqual({
      dock: "bottom",
      group: 0,
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
  const src = (group: number) => ({ dock: "right" as const, group });

  it("applies a cross-dock move", () => {
    expect(shouldApplyPlacement(groupsByDock, "diff", { dock: "bottom", group: 0, index: 0 }, src(0))).toBe(true);
  });
  it("applies a within-group move to a different slot", () => {
    expect(shouldApplyPlacement(groupsByDock, "diff", { dock: "right", group: 0, index: 2 }, src(0))).toBe(true);
  });
  it("applies a cross-group move within the same dock", () => {
    expect(shouldApplyPlacement(groupsByDock, "diff", { dock: "right", group: 1, index: 0 }, src(0))).toBe(true);
  });
  it("always applies a split into a new group", () => {
    expect(shouldApplyPlacement(groupsByDock, "diff", { dock: "right", group: 0, newGroup: true }, src(0))).toBe(true);
  });
  it("skips a within-group drop onto the tab's own slot", () => {
    // diff is at index 0; a post-removal target index of 0 is a no-op.
    expect(shouldApplyPlacement(groupsByDock, "diff", { dock: "right", group: 0, index: 0 }, src(0))).toBe(false);
  });
  it("skips when the tab is not in the target group", () => {
    expect(shouldApplyPlacement(groupsByDock, "ghost", { dock: "right", group: 0, index: 0 }, src(0))).toBe(false);
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
