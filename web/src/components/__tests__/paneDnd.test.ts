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
  it("maps each drop zone to a placement", () => {
    const cases: [string, Partial<PlacementOver>, string, ReturnType<typeof resolvePlacement>][] = [
      // Base without the dragged terminal:1 is [diff, terminal:0].
      [
        "before the hovered tab on its leading half",
        { tabId: "diff", after: false },
        "terminal:1",
        { dock: "right", group: 0, index: 0 },
      ],
      [
        "after the hovered tab on its trailing half",
        { tabId: "diff", after: true },
        "terminal:1",
        { dock: "right", group: 0, index: 1 },
      ],
      // Base without the dragged diff is [terminal:0, terminal:1].
      ["append on a group strip", { type: "pane-group", tabId: "" }, "diff", { dock: "right", group: 0, index: 2 }],
      [
        "split before the hovered group",
        { type: "pane-split", side: "before", group: 0, tabId: "" },
        "diff",
        { dock: "right", group: 0, newGroup: true },
      ],
      [
        "split after the hovered group",
        { type: "pane-split", side: "after", group: 0, tabId: "" },
        "diff",
        { dock: "right", group: 1, newGroup: true },
      ],
      [
        "seed the first group on an empty dock",
        { type: "pane-empty-dock", dock: "bottom", group: 0, tabId: "" },
        "diff",
        { dock: "bottom", group: 0, newGroup: true },
      ],
      // Dropping terminal:0 (index 1) on itself must not append it.
      [
        "keep the slot when dropped on itself",
        { tabId: "terminal:0", after: true },
        "terminal:0",
        { dock: "right", group: 0, index: 1 },
      ],
      [
        "append for a stale hovered id",
        { dock: "bottom", group: 0, tabId: "ghost" },
        "diff",
        { dock: "bottom", group: 0, index: 1 },
      ],
    ];
    for (const [name, partial, dragged, expected] of cases) {
      expect(resolvePlacement(over(partial), dragged, groupsByDock), name).toEqual(expected);
    }
  });
});

describe("rect helpers", () => {
  it("centerX and pointerInsertsAfter compare horizontal centers", () => {
    expect(centerX({ left: 10, width: 40 })).toBe(30);
    expect(centerX(null)).toBeNull();
    expect(centerX(undefined)).toBeNull();
    expect(pointerInsertsAfter({ left: 50, width: 20 }, { left: 0, width: 20 })).toBe(true);
    expect(pointerInsertsAfter({ left: 0, width: 20 }, { left: 50, width: 20 })).toBe(false);
    expect(pointerInsertsAfter(null, { left: 0, width: 20 })).toBe(false);
  });
});

describe("shouldApplyPlacement", () => {
  it("applies real moves and skips no-ops", () => {
    const src = { dock: "right" as const, group: 0 };
    const cases: [string, string, Parameters<typeof shouldApplyPlacement>[2], boolean][] = [
      ["cross-dock", "diff", { dock: "bottom", group: 0, index: 0 }, true],
      ["within-group new slot", "diff", { dock: "right", group: 0, index: 2 }, true],
      ["cross-group same dock", "diff", { dock: "right", group: 1, index: 0 }, true],
      ["split", "diff", { dock: "right", group: 0, newGroup: true }, true],
      // diff is at index 0; a post-removal target index of 0 is a no-op.
      ["own slot", "diff", { dock: "right", group: 0, index: 0 }, false],
      ["tab not in group", "ghost", { dock: "right", group: 0, index: 0 }, false],
    ];
    for (const [name, tab, target, want] of cases) {
      expect(shouldApplyPlacement(groupsByDock, tab, target, src), name).toBe(want);
    }
  });
});

describe("visibleToFullIndex", () => {
  it("maps visible slots past hidden persisted tabs", () => {
    const visible = (id: string) => !id.startsWith("plugin:");
    expect(visibleToFullIndex(["diff", "terminal:0"], 1, visible)).toBe(1);
    // Visible slot 1 is terminal:0 at full index 2, past the hidden plugin tab.
    expect(visibleToFullIndex(["diff", "plugin:p:x", "terminal:0"], 1, visible)).toBe(2);
    expect(visibleToFullIndex(["diff", "plugin:p:x"], 1, visible)).toBe(2);
  });
});
