// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";

import { loadDiffScope, saveDiffScope, scopeParam, sweepOrphanDiffScopes } from "../diffScope";

afterEach(() => localStorage.clear());

describe("diff scope", () => {
  it("remembers a picked scope per Session, Branch by default", () => {
    saveDiffScope("a", "uncommitted");
    saveDiffScope("b", "uncommitted");
    expect([loadDiffScope("a"), loadDiffScope("b"), loadDiffScope("c"), loadDiffScope(null)]).toEqual([
      "uncommitted",
      "uncommitted",
      "branch",
      "branch",
    ]);
    saveDiffScope("a", "branch");
    expect(JSON.parse(localStorage.getItem("aoe-diff-scope")!)).toEqual({ b: "uncommitted" });
  });

  it("drops gone Sessions and unknown values", () => {
    localStorage.setItem("aoe-diff-scope", JSON.stringify({ a: "uncommitted", gone: "uncommitted", b: "last_turn" }));
    expect(loadDiffScope("b")).toBe("branch");
    sweepOrphanDiffScopes(new Set(["a"]));
    expect(JSON.parse(localStorage.getItem("aoe-diff-scope")!)).toEqual({ a: "uncommitted" });
  });

  it("sends no parameter for the server's default", () => {
    expect([scopeParam("branch"), scopeParam("uncommitted")]).toEqual([null, "uncommitted"]);
  });
});
