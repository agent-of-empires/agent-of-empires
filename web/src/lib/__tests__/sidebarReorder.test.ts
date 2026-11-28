import { expect, it } from "vitest";
import { keepUnshown } from "../sidebarReorder";

it("keeps each saved id the screen lacks after its saved predecessor", () => {
  for (const [shown, saved, expected] of [
    [
      ["b", "a"],
      ["x", "a", "b"],
      ["x", "b", "a"],
    ],
    [
      ["b", "a"],
      ["a", "x", "b"],
      ["b", "a", "x"],
    ],
    [
      ["new", "b", "a"],
      ["a", "b", "x"],
      ["new", "b", "x", "a"],
    ],
    [
      ["a", "new"],
      ["x", "a"],
      ["x", "a", "new"],
    ],
    [["a"], [], ["a"]],
  ]) {
    expect(keepUnshown(shown!, saved!), `${shown} over ${saved}`).toEqual(expected);
  }
});
