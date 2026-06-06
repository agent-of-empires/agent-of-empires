// @vitest-environment jsdom

import { beforeEach, describe, expect, it } from "vitest";

import {
  REPO_COLOR_OPTIONS,
  applyRepoAppearanceUpdate,
  loadRepoAppearances,
  persistRepoAppearances,
  type RepoAppearance,
} from "./repoAppearance";

const STORAGE_KEY = "aoe-repo-appearance-v1";

describe("applyRepoAppearanceUpdate", () => {
  it("sets a trimmed alias", () => {
    const next = applyRepoAppearanceUpdate({}, "/repo/a", {
      alias: "  Alpha  ",
    });
    expect(next).toEqual({ "/repo/a": { alias: "Alpha" } });
  });

  it("clears the alias when null is passed and prunes the entry if color is also absent", () => {
    const next = applyRepoAppearanceUpdate(
      { "/repo/a": { alias: "Alpha" } },
      "/repo/a",
      { alias: null },
    );
    expect(next).toEqual({});
  });

  it("treats whitespace-only alias as a clear", () => {
    const next = applyRepoAppearanceUpdate(
      { "/repo/a": { alias: "Alpha", color: "amber" } },
      "/repo/a",
      { alias: "   " },
    );
    expect(next).toEqual({ "/repo/a": { color: "amber" } });
  });

  it("sets a valid color and keeps an existing alias", () => {
    const next = applyRepoAppearanceUpdate(
      { "/repo/a": { alias: "Alpha" } },
      "/repo/a",
      { color: "teal" },
    );
    expect(next).toEqual({ "/repo/a": { alias: "Alpha", color: "teal" } });
  });

  it("clears the color when null is passed and prunes the entry if alias is also absent", () => {
    const next = applyRepoAppearanceUpdate(
      { "/repo/a": { color: "rose" } },
      "/repo/a",
      { color: null },
    );
    expect(next).toEqual({});
  });

  it("rejects an unknown color string", () => {
    const next = applyRepoAppearanceUpdate(
      { "/repo/a": { alias: "Alpha" } },
      "/repo/a",
      // @ts-expect-error intentionally bad color
      { color: "bogus" },
    );
    expect(next).toEqual({ "/repo/a": { alias: "Alpha" } });
  });

  it("does not mutate the input map", () => {
    const current: Record<string, RepoAppearance> = {
      "/repo/a": { alias: "Alpha" },
    };
    const snapshot = JSON.parse(JSON.stringify(current));
    applyRepoAppearanceUpdate(current, "/repo/a", { alias: "Beta" });
    expect(current).toEqual(snapshot);
  });
});

describe("persistRepoAppearances / loadRepoAppearances", () => {
  beforeEach(() => {
    window.localStorage.clear();
  });

  it("round-trips a populated map", () => {
    const map: Record<string, RepoAppearance> = {
      "/repo/a": { alias: "Alpha", color: "amber" },
      "/repo/b": { color: "violet" },
    };
    persistRepoAppearances(map);
    expect(loadRepoAppearances()).toEqual(map);
  });

  it("removes the storage entry when the map is empty", () => {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify({ stale: true }));
    persistRepoAppearances({});
    expect(window.localStorage.getItem(STORAGE_KEY)).toBeNull();
  });

  it("returns an empty map for invalid JSON", () => {
    window.localStorage.setItem(STORAGE_KEY, "{not json");
    expect(loadRepoAppearances()).toEqual({});
  });

  it("drops entries that have neither alias nor a known color", () => {
    window.localStorage.setItem(
      STORAGE_KEY,
      JSON.stringify({
        "/repo/a": { alias: "Alpha" },
        "/repo/b": { color: "rainbow" },
        "/repo/c": {},
      }),
    );
    expect(loadRepoAppearances()).toEqual({
      "/repo/a": { alias: "Alpha" },
    });
  });
});

describe("REPO_COLOR_OPTIONS", () => {
  it("has unique ids", () => {
    const ids = REPO_COLOR_OPTIONS.map((o) => o.id);
    expect(new Set(ids).size).toBe(ids.length);
  });
});
