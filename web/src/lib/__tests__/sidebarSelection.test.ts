import { describe, expect, it } from "vitest";

import {
  EMPTY_SELECTION,
  classifyClick,
  rangeBetween,
  selectionReducer,
  type SidebarSelectionState,
} from "../sidebarSelection";

type Action = Parameters<typeof selectionReducer>[1];
const ORDER = ["a", "b", "c", "d", "e"];
const state = (ids: string[], anchorId: string | null): SidebarSelectionState => ({
  selectedIds: new Set(ids),
  anchorId,
});
const range = (targetId: string, additive = false): Action => ({
  type: "range",
  targetId,
  orderedIds: ORDER,
  additive,
});
const toggle = (id: string): Action => ({ type: "toggle", id });

it.each([
  [false, false, false, "navigate"],
  [true, false, false, "toggle"],
  [false, true, false, "toggle"],
  [false, false, true, "range"],
  [true, false, true, "additive-range"],
  [false, true, true, "additive-range"],
])("classifyClick(meta=%s, ctrl=%s, shift=%s) is %s", (metaKey, ctrlKey, shiftKey, expected) => {
  expect(classifyClick({ metaKey, ctrlKey, shiftKey })).toBe(expected);
});

it.each([
  ["b", "d", ["b", "c", "d"]],
  ["d", "b", ["b", "c", "d"]],
  ["c", "c", ["c"]],
  ["missing", "c", ["c"]],
  ["c", "missing", ["missing"]],
])("rangeBetween(%s, %s) is %o", (anchor, target, expected) => {
  expect(rangeBetween(ORDER, anchor, target)).toEqual(expected);
});

describe("selectionReducer", () => {
  it.each<[string, SidebarSelectionState, Action[], string[], string | null]>([
    ["toggle adds and anchors", EMPTY_SELECTION, [toggle("b")], ["b"], "b"],
    ["toggle removes but keeps the anchor", EMPTY_SELECTION, [toggle("b"), toggle("b")], [], "b"],
    ["range spans from the anchor", EMPTY_SELECTION, [toggle("b"), range("d")], ["b", "c", "d"], "b"],
    ["range re-pivots from the same anchor", EMPTY_SELECTION, [toggle("b"), range("d"), range("a")], ["a", "b"], "b"],
    ["range then extends from the new anchor", state(["x"], "x"), [range("c"), range("e")], ["c", "d", "e"], "c"],
    ["range without an anchor selects the target", EMPTY_SELECTION, [range("c")], ["c"], "c"],
    ["additive range unions", state(["a"], "a"), [toggle("d"), range("e", true)], ["a", "d", "e"], "d"],
    ["navigate clears but anchors", state(["a", "b"], "b"), [{ type: "navigate", id: "c" }], [], "c"],
    [
      "navigate then Shift+click ranges from it (#2312)",
      EMPTY_SELECTION,
      [{ type: "navigate", id: "a" }, range("c")],
      ["a", "b", "c"],
      "a",
    ],
    ["select-only replaces and anchors", state(["a", "b"], "a"), [{ type: "select-only", id: "d" }], ["d"], "d"],
    ["clear empties", state(["a", "b"], "b"), [{ type: "clear" }], [], null],
    [
      "prune drops missing ids and anchor",
      state(["a", "b", "gone"], "gone"),
      [{ type: "prune", validIds: new Set(["a", "b"]) }],
      ["a", "b"],
      null,
    ],
  ])("%s", (_name, initial, actions, selected, anchor) => {
    const next = actions.reduce(selectionReducer, initial);
    expect([[...next.selectedIds].sort(), next.anchorId]).toEqual([selected, anchor]);
  });

  it("prune returns the same reference when nothing changed", () => {
    const s = state(["a", "b"], "a");
    expect(selectionReducer(s, { type: "prune", validIds: new Set(["a", "b", "c"]) })).toBe(s);
  });
});
