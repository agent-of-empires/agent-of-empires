import { describe, expect, it } from "vitest";

import {
  EMPTY_SELECTION,
  classifyClick,
  rangeBetween,
  selectionReducer,
  type SidebarSelectionState,
} from "../sidebarSelection";

const ORDER = ["a", "b", "c", "d", "e"];

function state(ids: string[], anchorId: string | null): SidebarSelectionState {
  return { selectedIds: new Set(ids), anchorId };
}

describe("classifyClick", () => {
  it("maps modifier combinations to intents", () => {
    expect(
      classifyClick({ metaKey: false, ctrlKey: false, shiftKey: false }),
    ).toBe("navigate");
    expect(
      classifyClick({ metaKey: true, ctrlKey: false, shiftKey: false }),
    ).toBe("toggle");
    expect(
      classifyClick({ metaKey: false, ctrlKey: true, shiftKey: false }),
    ).toBe("toggle");
    expect(
      classifyClick({ metaKey: false, ctrlKey: false, shiftKey: true }),
    ).toBe("range");
    expect(
      classifyClick({ metaKey: true, ctrlKey: false, shiftKey: true }),
    ).toBe("additive-range");
    // Ctrl+Shift on Windows/Linux is the same additive-range gesture as
    // Cmd+Shift on macOS.
    expect(
      classifyClick({ metaKey: false, ctrlKey: true, shiftKey: true }),
    ).toBe("additive-range");
  });
});

describe("rangeBetween", () => {
  it("returns an inclusive range regardless of direction", () => {
    expect(rangeBetween(ORDER, "b", "d")).toEqual(["b", "c", "d"]);
    expect(rangeBetween(ORDER, "d", "b")).toEqual(["b", "c", "d"]);
  });

  it("returns the single id when anchor equals target", () => {
    expect(rangeBetween(ORDER, "c", "c")).toEqual(["c"]);
  });

  it("collapses to the target when an endpoint is missing", () => {
    expect(rangeBetween(ORDER, "missing", "c")).toEqual(["c"]);
    expect(rangeBetween(ORDER, "c", "missing")).toEqual(["missing"]);
  });
});

describe("selectionReducer", () => {
  it("toggle adds, then removes, and tracks the anchor", () => {
    const s1 = selectionReducer(EMPTY_SELECTION, { type: "toggle", id: "b" });
    expect([...s1.selectedIds]).toEqual(["b"]);
    expect(s1.anchorId).toBe("b");
    const s2 = selectionReducer(s1, { type: "toggle", id: "b" });
    expect([...s2.selectedIds]).toEqual([]);
    expect(s2.anchorId).toBe("b");
  });

  it("range replaces the selection from the anchor to the target", () => {
    const anchored = selectionReducer(EMPTY_SELECTION, {
      type: "toggle",
      id: "b",
    });
    const ranged = selectionReducer(anchored, {
      type: "range",
      targetId: "d",
      orderedIds: ORDER,
      additive: false,
    });
    expect([...ranged.selectedIds].sort()).toEqual(["b", "c", "d"]);
    // Anchor stays put so a second Shift+click re-pivots from "b".
    expect(ranged.anchorId).toBe("b");
    const reranged = selectionReducer(ranged, {
      type: "range",
      targetId: "a",
      orderedIds: ORDER,
      additive: false,
    });
    expect([...reranged.selectedIds].sort()).toEqual(["a", "b"]);
  });

  it("range re-anchors to the target when the old anchor is no longer rendered", () => {
    // Anchor "x" scrolled out of the rendered order (collapsed group, filter).
    // The range falls back to the clicked row AND re-anchors there, so the
    // next Shift+click forms a real range instead of collapsing again.
    const stale = state(["x"], "x");
    const ranged = selectionReducer(stale, {
      type: "range",
      targetId: "c",
      orderedIds: ORDER,
      additive: false,
    });
    expect([...ranged.selectedIds]).toEqual(["c"]);
    expect(ranged.anchorId).toBe("c");
    const next = selectionReducer(ranged, {
      type: "range",
      targetId: "e",
      orderedIds: ORDER,
      additive: false,
    });
    expect([...next.selectedIds].sort()).toEqual(["c", "d", "e"]);
  });

  it("range with no anchor selects only the clicked row and sets the anchor", () => {
    const ranged = selectionReducer(EMPTY_SELECTION, {
      type: "range",
      targetId: "c",
      orderedIds: ORDER,
      additive: false,
    });
    expect([...ranged.selectedIds]).toEqual(["c"]);
    expect(ranged.anchorId).toBe("c");
  });

  it("additive range unions the new range with the existing selection", () => {
    const start = state(["a"], "a");
    const moved = selectionReducer(start, { type: "toggle", id: "d" });
    const added = selectionReducer(moved, {
      type: "range",
      targetId: "e",
      orderedIds: ORDER,
      additive: true,
    });
    expect([...added.selectedIds].sort()).toEqual(["a", "d", "e"]);
  });

  it("clear empties the selection and anchor", () => {
    expect(selectionReducer(state(["a", "b"], "b"), { type: "clear" })).toEqual(
      EMPTY_SELECTION,
    );
  });

  it("prune drops ids and the anchor that no longer exist", () => {
    const pruned = selectionReducer(state(["a", "b", "gone"], "gone"), {
      type: "prune",
      validIds: new Set(["a", "b"]),
    });
    expect([...pruned.selectedIds].sort()).toEqual(["a", "b"]);
    expect(pruned.anchorId).toBeNull();
  });

  it("prune returns the same reference when nothing changed", () => {
    const s = state(["a", "b"], "a");
    expect(
      selectionReducer(s, {
        type: "prune",
        validIds: new Set(["a", "b", "c"]),
      }),
    ).toBe(s);
  });
});
