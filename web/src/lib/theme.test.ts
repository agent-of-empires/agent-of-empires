// @vitest-environment jsdom
//
// Coverage for the theme applicator. applyResolvedTheme writes both palettes'
// CSS vars onto the document root via setProperty, stamps the dataset/
// colorScheme, and caches the payload; readCachedResolvedTheme round-trips it
// and tolerates corrupt JSON; dispatchThemeChanged broadcasts the typed event.

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import {
  applyResolvedTheme,
  readCachedResolvedTheme,
  dispatchThemeChanged,
  THEME_CHANGED_EVENT,
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

describe("applyResolvedTheme", () => {
  it("sets web + terminal css vars, dataset, colorScheme, and caches", () => {
    applyResolvedTheme(theme());
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
    applyResolvedTheme(theme({ name: "light", appearance: "light" }));
    const cached = readCachedResolvedTheme();
    expect(cached?.name).toBe("light");
    expect(cached?.appearance).toBe("light");
  });

  it("returns null on corrupt cached JSON", () => {
    localStorage.setItem(STORAGE_KEY, "{not json");
    expect(readCachedResolvedTheme()).toBeNull();
  });
});

describe("dispatchThemeChanged", () => {
  it("emits the typed theme-changed event with the payload as detail", () => {
    let detail: ResolvedTheme | null = null;
    const handler = (e: Event) => {
      detail = (e as CustomEvent<ResolvedTheme>).detail;
    };
    window.addEventListener(THEME_CHANGED_EVENT, handler);
    dispatchThemeChanged(theme({ name: "nord" }));
    window.removeEventListener(THEME_CHANGED_EVENT, handler);
    expect(detail).not.toBeNull();
    expect(detail!.name).toBe("nord");
  });
});
