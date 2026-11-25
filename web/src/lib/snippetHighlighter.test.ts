import { readFileSync, readdirSync } from "node:fs";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { bundledThemes } from "shiki/themes";

interface FakeHighlighter {
  codeToHtml: (code: string, opts: { lang: string; theme: string }) => string;
  codeToTokens: (code: string, opts: { lang: string; theme: string }) => unknown;
}

const getSharedHighlighterMock = vi.fn(async (): Promise<FakeHighlighter> => ({
  codeToHtml: (code, opts) => `<pre data-lang="${opts.lang}" data-theme="${opts.theme}">${code}</pre>`,
  codeToTokens: vi.fn(),
}));

vi.mock("@pierre/diffs", () => ({
  getSharedHighlighter: (...args: unknown[]) => getSharedHighlighterMock(...args),
}));

import {
  DEFAULT_SHIKI_THEME,
  DEFAULT_SHIKI_THEME_LIGHT,
  fallbackShikiTheme,
  getSnippetHighlighter,
  highlightSnippet,
  langHintForPath,
  langIdForHint,
  peekHighlight,
  resolveSnippetTheme,
} from "./snippetHighlighter";

beforeEach(() => {
  getSharedHighlighterMock.mockClear();
});

afterEach(() => {
  vi.restoreAllMocks();
});

it.each([
  ["dark", DEFAULT_SHIKI_THEME, "github-dark"],
  ["light", DEFAULT_SHIKI_THEME_LIGHT, "github-light"],
  [undefined, DEFAULT_SHIKI_THEME, "github-dark"],
] as const)("fallbackShikiTheme(%s) is %s", (appearance, constant, name) => {
  expect(fallbackShikiTheme(appearance)).toBe(constant);
  expect(constant).toBe(name);
});

describe("resolveSnippetTheme", () => {
  it.each(["dracula", "github-light-high-contrast"])("keeps the bundled theme %s", (theme) => {
    expect(resolveSnippetTheme(theme, "dark")).toBe(theme);
  });

  it("returns the appearance-appropriate fallback for an unknown theme, warning once", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    expect(resolveSnippetTheme("not-a-real-theme", "light")).toBe(DEFAULT_SHIKI_THEME_LIGHT);
    expect(resolveSnippetTheme("not-a-real-theme", "dark")).toBe(DEFAULT_SHIKI_THEME);
    expect(resolveSnippetTheme("not-a-real-theme")).toBe(DEFAULT_SHIKI_THEME);
    expect(warn).toHaveBeenCalledTimes(1);
  });
});

describe("builtin theme syntax palettes", () => {
  it("each name a palette shiki bundles", () => {
    const dir = new URL("../../../themes/builtin/", import.meta.url);
    const files = readdirSync(dir).filter((f) => f.endsWith(".toml"));
    expect(files.length).toBeGreaterThan(0);
    for (const file of files) {
      const id = /^\s*shiki_theme\s*=\s*"([^"]+)"/m.exec(readFileSync(new URL(file, dir), "utf8"))?.[1];
      expect(id, `${file} declares no shiki_theme`).toBeTruthy();
      expect(Object.hasOwn(bundledThemes, id!), `${file} names "${id}"`).toBe(true);
    }
  });
});

it.each<[string, string | null]>([
  ...["typescript", "rs", "c#"].map((id): [string, string] => [id, id]),
  ["h", "c"],
  ["mli", "ocaml"],
  ["golang", "go"],
  ["RUST", "rust"],
  ["Dockerfile", "dockerfile"],
  ["makefile", "make"],
  ["notalang", null],
  ["", null],
  ["constructor", null],
])("langIdForHint(%j) is %j", (hint, expected) => {
  expect(langIdForHint(hint)).toBe(expected);
});

describe("langHintForPath", () => {
  it.each([
    ["src/lib/highlighter.ts", "ts"],
    ["src/constructor.ts", "ts"],
    ["Dockerfile", "Dockerfile"],
    ["/repo/build/Dockerfile", "Dockerfile"],
    ["a/b/c/CMakeLists.txt", "CMakeLists"],
    ["README", ""],
  ])("%j gives %j", (path, expected) => {
    expect(langHintForPath(path)).toBe(expected);
  });

  it("a dotfile has no recognised language", () => {
    expect(langIdForHint(langHintForPath(".gitignore"))).toBeNull();
  });
});

describe("getSnippetHighlighter", () => {
  it("resolves the shared highlighter for a known language", async () => {
    const resolved = await getSnippetHighlighter({ langHint: "ts", theme: "github-dark" });
    expect(resolved).not.toBeNull();
    expect(resolved?.langId).toBe("ts");
    expect(resolved?.theme).toBe("github-dark");
    expect(getSharedHighlighterMock).toHaveBeenCalledWith({ themes: ["github-dark"], langs: ["ts"] });
  });

  it("falls back to a safe theme for an unknown shiki_theme value", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const resolved = await getSnippetHighlighter({ langHint: "ts", theme: "not-a-real-theme", appearance: "light" });
    expect(resolved?.theme).toBe(DEFAULT_SHIKI_THEME_LIGHT);
    warn.mockRestore();
  });

  it("returns null without calling getSharedHighlighter for an unresolvable hint", async () => {
    const resolved = await getSnippetHighlighter({ langHint: "notalang", theme: "github-dark" });
    expect(resolved).toBeNull();
    expect(getSharedHighlighterMock).not.toHaveBeenCalled();
  });
});

describe("highlightSnippet", () => {
  it("renders HTML for a known language", async () => {
    const html = await highlightSnippet("const x = 1;", { langHint: "ts", theme: "github-dark" });
    expect(html).toBe('<pre data-lang="ts" data-theme="github-dark">const x = 1;</pre>');
  });

  it("returns null for an unresolvable hint", async () => {
    const html = await highlightSnippet("const x = 1;", { langHint: "notalang", theme: "github-dark" });
    expect(html).toBeNull();
  });
});

describe("highlight cache", () => {
  const opts = { langHint: "ts", theme: "github-dark" };

  it("peeks undefined before a highlight and the html after, without calling the highlighter", async () => {
    expect(peekHighlight("peek();", opts)).toBeUndefined();
    const html = await highlightSnippet("peek();", opts);
    getSharedHighlighterMock.mockClear();
    expect(peekHighlight("peek();", opts)).toBe(html);
    expect(peekHighlight("peek();", { langHint: "notalang", theme: "github-dark" })).toBeUndefined();
    expect(await highlightSnippet("peek();", opts)).toBe(html);
    expect(getSharedHighlighterMock).not.toHaveBeenCalled();
  });

  it("keys on the resolved theme, so fallbacks share an entry", async () => {
    vi.spyOn(console, "warn").mockImplementation(() => {});
    await highlightSnippet("fb();", { langHint: "ts", theme: "not-a-real-theme" });
    expect(peekHighlight("fb();", { langHint: "ts", theme: "github-dark" })).toBeDefined();
  });

  it("evicts the oldest entry at the cap, and a hit refreshes recency", async () => {
    await highlightSnippet("first", opts);
    for (let i = 0; i < 63; i++) await highlightSnippet(`fill${i}`, opts);
    expect(peekHighlight("first", opts)).toBeDefined();
    await highlightSnippet("overflow", opts);
    expect(peekHighlight("first", opts)).toBeDefined();
    expect(peekHighlight("fill0", opts)).toBeUndefined();
    expect(peekHighlight("overflow", opts)).toBeDefined();
  });
});
