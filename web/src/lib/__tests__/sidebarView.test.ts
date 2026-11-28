import { describe, expect, it } from "vitest";
import type { SidebarView, SidebarViewPatch } from "../api";
import { applyViewPatch } from "../sidebarView";

describe("applyViewPatch", () => {
  it("lets an explicit plugin sort marker win over the clear a sort pick makes, as the Daemon does", () => {
    const marked: SidebarView = { group_by: "project", sort: "custom", web_plugin_sort: "Cost" };
    const cases: [SidebarViewPatch, string | undefined][] = [
      [{ sort: "name" }, undefined],
      [{ sort: "name", web_plugin_sort: "Tokens" }, "Tokens"],
      [{ web_plugin_sort: null }, undefined],
      [{ group_by: "group" }, "Cost"],
    ];
    for (const [patch, marker] of cases) expect(applyViewPatch(marked, patch).web_plugin_sort).toBe(marker);
  });
});
