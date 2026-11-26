import { describe, expect, it } from "vitest";
import { filterSuggestions, keyChipGroups } from "./suggestionFilter";
import type { SuggestionItem } from "./types";

const item = (id: string, mapping_key: string | null, over: Partial<SuggestionItem> = {}): SuggestionItem => ({
  id,
  provider_id: "gh",
  title: `Title ${id}`,
  reason: "Assigned to you",
  source_url: "",
  source_ref: `${mapping_key ?? "x"}#${id}`,
  preview: "",
  criterion: "assigned",
  mapping_key,
  draft: { title: "", prompt: "" },
  dismissed: false,
  ...over,
});

const items = [
  item("1", "acme/web"),
  item("2", "acme/web"),
  item("3", "acme/api", { title: "Fix the LOGIN page" }),
  item("4", "other/tool", { reason: "Review requested" }),
  item("5", "standalone"),
  item("6", null),
];

const ids = (list: SuggestionItem[]) => list.map((i) => i.id);

describe("keyChipGroups", () => {
  it("groups keys by org with counts and keeps slashless keys on their own", () => {
    expect(keyChipGroups(items)).toEqual([
      {
        org: "acme",
        count: 3,
        keys: [
          { key: "acme/api", label: "api", count: 1 },
          { key: "acme/web", label: "web", count: 2 },
        ],
      },
      { org: "other", count: 1, keys: [{ key: "other/tool", label: "tool", count: 1 }] },
      { org: null, count: 1, keys: [{ key: "standalone", label: "standalone", count: 1 }] },
    ]);
  });
});

describe("filterSuggestions", () => {
  it("returns everything with no query and no chip", () => {
    expect(ids(filterSuggestions(items, "  ", []))).toEqual(["1", "2", "3", "4", "5", "6"]);
  });

  it("searches title, source ref, reason and key case-insensitively", () => {
    expect(ids(filterSuggestions(items, "login", []))).toEqual(["3"]);
    expect(ids(filterSuggestions(items, "#4", []))).toEqual(["4"]);
    expect(ids(filterSuggestions(items, "REVIEW", []))).toEqual(["4"]);
    expect(ids(filterSuggestions(items, "standal", []))).toEqual(["5"]);
  });

  it("an org chip keeps every key under it, a key chip one key, and chips combine", () => {
    expect(ids(filterSuggestions(items, "", [{ org: "acme" }]))).toEqual(["1", "2", "3"]);
    expect(ids(filterSuggestions(items, "", [{ key: "acme/web" }]))).toEqual(["1", "2"]);
    expect(ids(filterSuggestions(items, "", [{ key: "acme/web" }, { org: "other" }]))).toEqual(["1", "2", "4"]);
    // An org chip does not match a key that merely shares its prefix.
    expect(ids(filterSuggestions([item("7", "acme-labs/x")], "", [{ org: "acme" }]))).toEqual([]);
  });

  it("applies the search within the selected chips", () => {
    expect(ids(filterSuggestions(items, "login", [{ key: "acme/web" }]))).toEqual([]);
    expect(ids(filterSuggestions(items, "title 2", [{ org: "acme" }]))).toEqual(["2"]);
  });
});
