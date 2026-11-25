import { describe, expect, it } from "vitest";
import { groupMove, groupPathComparator, mergeGroupOrder } from "./groupOrder";

describe("groupPathComparator", () => {
  it("orders a tree as listed, parents first, unlisted siblings after by name", () => {
    const order = ["b", "b/z", "b/a", "a"];
    const paths = ["a", "b/a", "Loose", "b", "b/new", "b/z", "c"];
    expect([...paths].sort(groupPathComparator(order))).toEqual(["b", "b/z", "b/a", "b/new", "a", "c", "Loose"]);
  });

  it("merges profiles by name, each path where first seen", () => {
    expect(mergeGroupOrder({ work: ["x", "y"], home: ["y", "z"] })).toEqual(["y", "z", "x"]);
  });
});

describe("groupMove", () => {
  const order = ["b", "b/z", "a", "b/a", "c"];
  it.each([
    // [active, over, after, direction, steps, order]
    ["c", "b", false, "up", 2, ["c", "b/z", "b", "b/a", "a"]],
    ["b", "c", true, "down", 2, ["a", "b/z", "c", "b/a", "b"]],
    ["b/a", "b/z", false, "up", 1, ["b", "b/a", "a", "b/z", "c"]],
  ] as const)("moves %s next to %s", (active, over, after, direction, steps, next) => {
    expect(groupMove(order, [], active, over, after)).toEqual({ direction, steps, order: next });
  });

  it("places an unlisted sibling after the listed ones, and refuses other parents and no-ops", () => {
    expect(groupMove(["a", "b"], ["new"], "new", "a", false)).toEqual({
      direction: "up",
      steps: 2,
      order: ["new", "a", "b"],
    });
    expect(groupMove(order, [], "b/a", "a", false)).toBeNull();
    expect(groupMove(order, [], "a", "b", true)).toBeNull();
  });
});
