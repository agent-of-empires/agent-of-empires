import { describe, expect, it } from "vitest";
import type { PluginUiEntry, PluginUiSlot } from "../api";
import {
  accentStyle,
  buildSortValueMap,
  compareSortValues,
  entryFilterValues,
  entrySortValue,
  entryText,
  entryTone,
  globalEntries,
  payloadStr,
  pluginFacetSpecs,
  pluginSortSpecs,
  sessionEntries,
  sessionMatchesFacets,
  toneClasses,
  validColor,
} from "../pluginUi";

function entry(slot: PluginUiSlot, over: Partial<PluginUiEntry> = {}): PluginUiEntry {
  return {
    plugin_id: "acme.kit",
    slot,
    id: "x",
    payload: {},
    ...over,
  };
}

describe("pluginUi selectors", () => {
  it("globalEntries keeps only matching-slot entries without a session_id", () => {
    const entries = [
      entry("status-bar", { id: "a" }),
      entry("status-bar", { id: "b", session_id: "s1" }), // has session => not global
      entry("card", { id: "c" }),
    ];
    const got = globalEntries(entries, "status-bar");
    expect(got.map((e) => e.id)).toEqual(["a"]);
  });

  it("sessionEntries scopes to one session and is empty for a missing id", () => {
    const entries = [
      entry("row-badge", { id: "a", session_id: "s1" }),
      entry("row-badge", { id: "b", session_id: "s2" }),
      entry("row-badge", { id: "c" }), // global, excluded
    ];
    expect(sessionEntries(entries, "row-badge", "s1").map((e) => e.id)).toEqual(["a"]);
    expect(sessionEntries(entries, "row-badge", undefined)).toEqual([]);
    // Tearing guard: an id for a session that no longer exists matches nothing.
    expect(sessionEntries(entries, "row-badge", "gone")).toEqual([]);
  });

  it("payloadStr and entryText read string fields, falling back to empty", () => {
    const e = entry("card", { payload: { title: "Hi", text: "ok", n: 3 } });
    expect(payloadStr(e, "title")).toBe("Hi");
    expect(payloadStr(e, "n")).toBe(""); // non-string
    expect(payloadStr(e, "missing")).toBe("");
    expect(entryText(e)).toBe("ok");
  });

  it("entryTone validates against the closed set", () => {
    expect(entryTone(entry("status-bar", { payload: { tone: "success" } }))).toBe("success");
    expect(entryTone(entry("status-bar", { payload: { tone: "rainbow" } }))).toBeUndefined();
    expect(entryTone(entry("status-bar"))).toBeUndefined();
  });

  it("toneClasses maps every tone to theme tokens and falls back to neutral", () => {
    expect(toneClasses("success")).toContain("status-running");
    expect(toneClasses("danger")).toContain("status-error");
    expect(toneClasses(undefined)).toContain("status-idle");
  });

  it("validColor accepts only hex literals and normalizes them", () => {
    expect(validColor("#8957E5")).toBe("#8957e5");
    expect(validColor("#abc")).toBe("#aabbcc"); // shorthand expanded
    expect(validColor("red")).toBeUndefined();
    expect(validColor("rgb(1,2,3)")).toBeUndefined();
    expect(validColor("var(--x)")).toBeUndefined();
    expect(validColor("# <script>")).toBeUndefined();
    expect(validColor(123)).toBeUndefined();
  });

  it("accentStyle tints from a valid color and ignores junk", () => {
    expect(accentStyle("#8957e5")).toEqual({ color: "#8957e5" });
    const filled = accentStyle("#8957e5", true);
    expect(filled?.color).toBe("#8957e5");
    expect(String(filled?.backgroundColor)).toContain("#8957e5");
    expect(accentStyle("red")).toBeUndefined();
  });
});

describe("pluginUi sort-key / filter-facet (#2401)", () => {
  it("entrySortValue accepts finite numbers and strings, rejects the rest", () => {
    expect(entrySortValue(entry("row-column", { payload: { sort_value: 42 } }))).toBe(42);
    expect(entrySortValue(entry("row-column", { payload: { sort_value: "b" } }))).toBe("b");
    expect(entrySortValue(entry("row-column", { payload: { sort_value: 0 } }))).toBe(0);
    expect(entrySortValue(entry("row-column", { payload: { sort_value: Infinity } }))).toBeUndefined();
    expect(entrySortValue(entry("row-column", { payload: { sort_value: { x: 1 } } }))).toBeUndefined();
    expect(entrySortValue(entry("row-column"))).toBeUndefined();
  });

  it("entryFilterValues keeps only string tokens", () => {
    expect(entryFilterValues(entry("row-column", { payload: { filter_values: ["a", 1, "b", null] } }))).toEqual([
      "a",
      "b",
    ]);
    expect(entryFilterValues(entry("row-column", { payload: { filter_values: "nope" } }))).toEqual([]);
    expect(entryFilterValues(entry("row-column"))).toEqual([]);
  });

  it("compareSortValues sinks missing values regardless of direction", () => {
    expect(compareSortValues(undefined, 5, "asc")).toBeGreaterThan(0);
    expect(compareSortValues(undefined, 5, "desc")).toBeGreaterThan(0);
    expect(compareSortValues(5, undefined, "asc")).toBeLessThan(0);
    expect(compareSortValues(5, undefined, "desc")).toBeLessThan(0);
    expect(compareSortValues(undefined, undefined, "asc")).toBe(0);
  });

  it("compareSortValues orders numbers and strings by direction", () => {
    expect(compareSortValues(1, 2, "asc")).toBeLessThan(0);
    expect(compareSortValues(1, 2, "desc")).toBeGreaterThan(0);
    expect(compareSortValues("a", "b", "asc")).toBeLessThan(0);
    expect(compareSortValues("a", "b", "desc")).toBeGreaterThan(0);
    // Mixed types are deterministic: a number sorts before a string (asc).
    expect(compareSortValues(1, "a", "asc")).toBeLessThan(0);
  });

  it("pluginSortSpecs reads global sort-key entries and defaults direction to asc", () => {
    const entries = [
      entry("sort-key", { id: "cpu", payload: { label: "CPU", column: "col-cpu", direction: "desc" } }),
      entry("sort-key", { id: "mem", payload: { label: "Mem", column: "col-mem" } }),
      entry("sort-key", { id: "bad", payload: { label: "", column: "x" } }), // no label => skipped
      entry("sort-key", { id: "perses", session_id: "s1", payload: { label: "L", column: "c" } }), // per-session => skipped
    ];
    const specs = pluginSortSpecs(entries);
    expect(specs.map((s) => [s.entryId, s.column, s.direction])).toEqual([
      ["cpu", "col-cpu", "desc"],
      ["mem", "col-mem", "asc"],
    ]);
  });

  it("pluginFacetSpecs reads global filter-facet entries and drops valueless options", () => {
    const entries = [
      entry("filter-facet", {
        id: "status",
        payload: {
          label: "Status",
          column: "col-st",
          options: [
            { value: "run", label: "Running", tone: "success" },
            { value: "idle" }, // label defaults to value
            { label: "no value" }, // dropped
          ],
        },
      }),
    ];
    const specs = pluginFacetSpecs(entries);
    expect(specs).toHaveLength(1);
    expect(specs[0]!.options).toEqual([
      { value: "run", label: "Running", tone: "success" },
      { value: "idle", label: "idle", tone: undefined },
    ]);
  });

  it("buildSortValueMap scopes to the same plugin_id and column", () => {
    const entries = [
      entry("row-column", { id: "col", session_id: "s1", payload: { sort_value: 1 } }),
      entry("row-column", { id: "col", session_id: "s2", payload: { sort_value: 2 } }),
      // Same column id but a different plugin must not bleed in.
      entry("row-column", { id: "col", session_id: "s3", plugin_id: "other.kit", payload: { sort_value: 9 } }),
      // Different column id, ignored.
      entry("row-column", { id: "elsewhere", session_id: "s4", payload: { sort_value: 5 } }),
    ];
    const map = buildSortValueMap(entries, "acme.kit", "col");
    expect([...map.entries()].sort()).toEqual([
      ["s1", 1],
      ["s2", 2],
    ]);
  });

  it("sessionMatchesFacets ANDs across facets and ORs within one (same-plugin column)", () => {
    const entries = [
      entry("row-column", { id: "st", session_id: "s1", payload: { filter_values: ["run"] } }),
      entry("row-column", { id: "lang", session_id: "s1", payload: { filter_values: ["rust"] } }),
      entry("row-column", { id: "st", session_id: "s2", payload: { filter_values: ["idle"] } }),
    ];
    const facetStatus = { pluginId: "acme.kit", column: "st", values: new Set(["run", "idle"]) };
    const facetLang = { pluginId: "acme.kit", column: "lang", values: new Set(["rust"]) };
    // s1 matches both facets.
    expect(sessionMatchesFacets(entries, "s1", [facetStatus, facetLang])).toBe(true);
    // s2 matches status (idle) but has no lang row-column => fails the AND.
    expect(sessionMatchesFacets(entries, "s2", [facetStatus, facetLang])).toBe(false);
    // s2 matches status alone.
    expect(sessionMatchesFacets(entries, "s2", [facetStatus])).toBe(true);
    // No active facets => trivially true.
    expect(sessionMatchesFacets(entries, "s2", [])).toBe(true);
  });
});
