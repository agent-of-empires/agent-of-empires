// @vitest-environment jsdom

import { beforeEach, describe, expect, it } from "vitest";

import {
  applyRepoAppearanceUpdate,
  loadRepoAppearances,
  persistRepoAppearances,
  type RepoAppearance,
} from "./repoAppearance";

const STORAGE_KEY = "aoe-repo-appearance-v1";

it("applyRepoAppearanceUpdate trims, validates, clears, and prunes without mutating its input", () => {
  const cases: [string, Record<string, RepoAppearance>, Partial<Record<"alias" | "color", string | null>>, unknown][] =
    [
      ["trimmed alias", {}, { alias: "  Alpha  " }, { "/repo/a": { alias: "Alpha" } }],
      ["null alias prunes", { "/repo/a": { alias: "Alpha" } }, { alias: null }, {}],
      [
        "blank alias clears",
        { "/repo/a": { alias: "Alpha", color: "amber" } },
        { alias: "   " },
        { "/repo/a": { color: "amber" } },
      ],
      [
        "color keeps alias",
        { "/repo/a": { alias: "Alpha" } },
        { color: "teal" },
        { "/repo/a": { alias: "Alpha", color: "teal" } },
      ],
      ["null color prunes", { "/repo/a": { color: "rose" } }, { color: null }, {}],
      [
        "unknown color ignored",
        { "/repo/a": { alias: "Alpha" } },
        { color: "bogus" },
        { "/repo/a": { alias: "Alpha" } },
      ],
    ];
  for (const [name, current, update, expected] of cases) {
    const snapshot = JSON.stringify(current);
    expect(applyRepoAppearanceUpdate(current, "/repo/a", update as never), name).toEqual(expected);
    expect(JSON.stringify(current), name).toBe(snapshot);
  }
});

describe("persistRepoAppearances / loadRepoAppearances", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });

  it("round-trips a populated map and removes the entry when empty", () => {
    const map: Record<string, RepoAppearance> = {
      "/repo/a": { alias: "Alpha", color: "amber" },
      "/repo/b": { color: "violet" },
    };
    persistRepoAppearances(map);
    expect(loadRepoAppearances()).toEqual(map);
    persistRepoAppearances({});
    expect(window.localStorage.getItem(STORAGE_KEY)).toBeNull();
  });

  it("loads an empty map from invalid JSON and drops entries with neither alias nor known color", () => {
    window.localStorage.setItem(STORAGE_KEY, "{not json");
    expect(loadRepoAppearances()).toEqual({});
    window.localStorage.setItem(
      STORAGE_KEY,
      JSON.stringify({ "/repo/a": { alias: "Alpha" }, "/repo/b": { color: "rainbow" }, "/repo/c": {} }),
    );
    expect(loadRepoAppearances()).toEqual({ "/repo/a": { alias: "Alpha" } });
  });
});
