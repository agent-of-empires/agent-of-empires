// @vitest-environment jsdom
//
// The theme store paints both palettes on <html>, caches the payload, and notifies subscribers.

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import {
  getResolvedTheme,
  readCachedResolvedTheme,
  setResolvedTheme,
  subscribeTheme,
  type ResolvedTheme,
} from "./theme";

const STORAGE_KEY = "aoe-resolved-theme";

function theme(over: Partial<ResolvedTheme> = {}): ResolvedTheme {
  return {
    name: "dracula",
    source: "builtin",
    appearance: "dark",
    web: { cssVars: { "--bg": "#000", "--fg": "#fff" } },
    terminal: { cssVars: { "--term-bg": "#111" } },
    syntax: { shikiTheme: "dracula" },
    ...over,
  };
}

beforeEach(() => {
  localStorage.clear();
  document.documentElement.removeAttribute("style");
  document.documentElement.removeAttribute("data-theme");
});
afterEach(() => localStorage.clear());

describe("setResolvedTheme", () => {
  it("sets web + terminal css vars, dataset, colorScheme, and caches", () => {
    setResolvedTheme(theme());
    const root = document.documentElement;
    expect(root.style.getPropertyValue("--bg")).toBe("#000");
    expect(root.style.getPropertyValue("--fg")).toBe("#fff");
    expect(root.style.getPropertyValue("--term-bg")).toBe("#111");
    expect(root.dataset.theme).toBe("dracula");
    expect(root.dataset.themeAppearance).toBe("dark");
    expect(root.style.colorScheme).toBe("dark");
    expect(localStorage.getItem(STORAGE_KEY)).toContain("dracula");
  });
});

describe("readCachedResolvedTheme", () => {
  it("returns null when nothing cached", () => {
    expect(readCachedResolvedTheme()).toBeNull();
  });

  it("round-trips an applied theme", () => {
    setResolvedTheme(theme({ name: "light", appearance: "light" }));
    const cached = readCachedResolvedTheme();
    expect(cached?.name).toBe("light");
    expect(cached?.appearance).toBe("light");
  });

  it("returns null on corrupt cached JSON", () => {
    localStorage.setItem(STORAGE_KEY, "{not json");
    expect(readCachedResolvedTheme()).toBeNull();
  });
});

describe("theme store", () => {
  it("notifies subscribers and serves the new theme", () => {
    const seen: string[] = [];
    const stop = subscribeTheme(() => seen.push(getResolvedTheme()!.name));
    setResolvedTheme(theme({ name: "nord" }));
    stop();
    setResolvedTheme(theme({ name: "zinc" }));
    expect(seen).toEqual(["nord"]);
    expect(getResolvedTheme()?.name).toBe("zinc");
  });

  it("does not rewrite the cache when told another window already did", () => {
    setResolvedTheme(theme({ name: "nord" }), false);
    expect(localStorage.getItem(STORAGE_KEY)).toBeNull();
    expect(document.documentElement.dataset.theme).toBe("nord");
  });
});
