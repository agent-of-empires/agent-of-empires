import { describe, expect, it } from "vitest";

import { applyFilter, foldActivity, foldLabel, followTail, type ActivityNode } from "./activityFold";
import type { ActivityTool } from "./model";

const tool = (i: number, kind = "read", status: ActivityTool["status"] = "ok"): ActivityTool => ({
  id: `t${i}`,
  kind,
  name: kind,
  target: null,
  status,
});

const foldAt = (nodes: ActivityNode[], i: number) => {
  const node = nodes[i]!;
  if (node.type !== "fold") throw new Error(`expected a fold at ${i}`);
  return node;
};

const shape = (nodes: ActivityNode[]) =>
  nodes.map((n) => (n.type === "row" ? n.tool.id : `fold(${n.tools.map((t) => t.id).join(",")})`));

describe("foldActivity", () => {
  it("folds adjacent same-kind calls but not short runs", () => {
    const tools = [tool(0, "read"), tool(1, "read"), tool(2, "read"), tool(3, "execute"), tool(4, "execute")];
    expect(shape(foldActivity(tools))).toEqual(["fold(t0,t1,t2)", "t3", "t4"]);
  });

  it("never folds across an error", () => {
    const tools = [tool(0), tool(1), tool(2, "read", "err"), tool(3), tool(4), tool(5)];
    expect(shape(foldActivity(tools))).toEqual(["t0", "t1", "t2", "fold(t3,t4,t5)"]);
  });

  it("shows head and tail of a long run and keeps every error as a row", () => {
    const tools = Array.from({ length: 54 }, (_, i) =>
      tool(i, i % 2 ? "read" : "execute", i === 20 || i === 40 ? "err" : "ok"),
    );
    const nodes = foldActivity(tools);
    expect(shape(nodes).slice(0, 3)).toEqual(["t0", "t1", "t2"]);
    expect(shape(nodes).slice(-4)).toEqual(["t50", "t51", "t52", "t53"]);
    const rows = nodes.filter((n) => n.type === "row").map((n) => (n as { tool: ActivityTool }).tool.id);
    expect(rows).toContain("t20");
    expect(rows).toContain("t40");
    const folds = nodes.filter((n) => n.type === "fold") as Extract<ActivityNode, { type: "fold" }>[];
    expect(folds).toHaveLength(3);
    expect(folds.flatMap((f) => f.tools)).toHaveLength(54 - 3 - 4 - 2);
    expect(folds.every((f) => f.tools.every((t) => t.status !== "err"))).toBe(true);
  });

  it("labels runs and spans by count and kind", () => {
    const run = foldAt(foldActivity(Array.from({ length: 6 }, (_, i) => tool(i))), 0);
    expect(foldLabel(run)).toBe("6 reads");
    const span = foldAt(foldActivity(Array.from({ length: 30 }, (_, i) => tool(i, i < 15 ? "read" : "execute"))), 3);
    expect(foldLabel(span)).toBe("23 calls: 12 reads, 11 commands");
  });
});

describe("applyFilter and followTail", () => {
  const tools = [tool(0, "read"), tool(1, "execute", "err"), tool(2, "read")];
  it("filters by errors or kind", () => {
    expect(applyFilter(tools, "errors").map((t) => t.id)).toEqual(["t1"]);
    expect(applyFilter(tools, { kind: "read" }).map((t) => t.id)).toEqual(["t0", "t2"]);
    expect(applyFilter(tools, "all")).toBe(tools);
  });

  it("keeps the newest rows and folds the earlier ones", () => {
    const many = Array.from({ length: 12 }, (_, i) => tool(i));
    const nodes = followTail(many, 5);
    expect(nodes.filter((n) => n.type === "row")).toHaveLength(5);
    expect(nodes[0]!.type).toBe("fold");
    expect(followTail(many.slice(0, 3), 5).every((n) => n.type === "row")).toBe(true);
  });
});
