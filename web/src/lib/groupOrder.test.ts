import { describe, expect, it } from "vitest";
import { arrangeGroups, groupMove, groupPathComparator, mergeGroupOrder } from "./groupOrder";
import { compareNames } from "./sidebarSort";

describe("groupPathComparator", () => {
  it("orders a tree as listed, parents first, unlisted siblings after by name", () => {
    const order = ["b", "b/z", "b/a", "a"];
    const paths = ["a", "b/a", "Loose", "b", "b/new", "b/z", "c"];
    expect([...paths].sort(groupPathComparator(order))).toEqual(["b", "b/z", "b/a", "b/new", "a", "c", "Loose"]);
  });

  it("gives siblings equal but for case one order whatever the input order", () => {
    const paths = ["A/z", "a/a", "A/b", "a"];
    const sorted = [...paths].sort(groupPathComparator([]));
    expect([...paths].reverse().sort(groupPathComparator([]))).toEqual(sorted);
    expect(sorted).toEqual(["A/b", "A/z", "a", "a/a"]);
  });

  it("merges profiles by name, each path where first seen", () => {
    expect(mergeGroupOrder({ work: ["x", "y"], home: ["y", "z"] })).toEqual(["y", "z", "x"]);
  });
});

describe("groupMove", () => {
  const order = ["b", "b/z", "a", "b/a", "c"];
  it.each([
    // [active, over, after, direction, siblings sent, order]
    ["c", "b", false, "up", ["b", "c", "a"], ["c", "b/z", "b", "b/a", "a"]],
    ["b", "c", true, "down", ["a", "b", "c"], ["a", "b/z", "c", "b/a", "b"]],
    ["b/a", "b/z", false, "up", ["b/z", "b/a"], ["b", "b/a", "a", "b/z", "c"]],
  ] as const)("moves %s next to %s in one request", (active, over, after, direction, siblings, next) => {
    expect(groupMove(order, [], active, over, after)).toEqual({ direction, siblings, order: next });
  });

  it("places an unlisted sibling after the listed ones, and refuses other parents and no-ops", () => {
    expect(groupMove(["a", "b"], ["new"], "new", "a", false)).toEqual({
      direction: "up",
      siblings: ["a", "new", "b"],
      order: ["new", "a", "b"],
    });
    expect(groupMove(order, [], "b/a", "a", false)).toBeNull();
    expect(groupMove(order, [], "a", "b", true)).toBeNull();
  });
});

describe("arrangeGroups", () => {
  it("puts each sibling set in order with one move, skipping sets already in order", () => {
    const moves = arrangeGroups(["zeta", "zeta/b", "zeta/a", "alpha", "mid/x"], ["new"], compareNames);
    expect(moves).toEqual([
      {
        path: "alpha",
        move: {
          direction: "up",
          siblings: ["new", "alpha", "zeta"],
          order: ["alpha", "zeta/b", "zeta/a", "new", "mid/x", "zeta"],
        },
      },
      {
        path: "zeta/a",
        move: {
          direction: "up",
          siblings: ["zeta/b", "zeta/a"],
          order: ["alpha", "zeta/a", "zeta/b", "new", "mid/x", "zeta"],
        },
      },
    ]);
    expect(arrangeGroups(["a", "b"], [], compareNames)).toEqual([]);
  });
});
