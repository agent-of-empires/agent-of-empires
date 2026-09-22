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
  it.each([
    "dracula",
    "github-dark",
    "github-light",
    "github-dark-dimmed",
    "catppuccin-latte",
    "material-theme-ocean",
    "dark-plus",
    "light-plus",
    "monokai",
    "solarized-dark",
    "solarized-light",
    "red",
    "min-light",
    "gruvbox-dark-medium",
    "github-dark-high-contrast",
    "github-light-high-contrast",
  ])("keeps the bundled theme %s", (theme) => {
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
  ...["typescript", "json", "ts", "rs", "yaml", "console", "c++", "c#"].map((id): [string, string] => [id, id]),
  ["h", "c"],
  ["hpp", "cpp"],
  ["cc", "cpp"],
  ["htm", "html"],
  ["svg", "xml"],
  ["ex", "elixir"],
  ["exs", "elixir"],
  ["hrl", "erlang"],
  ["ml", "ocaml"],
  ["mli", "ocaml"],
  ["golang", "go"],
  ["cplusplus", "cpp"],
  ["bash-session", "bash"],
  ["terminal", "bash"],
  ["RUST", "rust"],
  ["Python", "python"],
  ["TS", "ts"],
  ["Dockerfile", "dockerfile"],
  ["Makefile", "make"],
  ["makefile", "make"],
  ["CMakeLists", "cmake"],
  ["notalang", null],
  ["", null],
  ["unknownext", null],
  ["constructor", null],
  ["toString", null],
])("langIdForHint(%j) is %j", (hint, expected) => {
  expect(langIdForHint(hint)).toBe(expected);
});

describe("langHintForPath", () => {
  it.each([
    ["src/lib/highlighter.ts", "ts"],
    ["/abs/path/to/main.rs", "rs"],
    ["src/constructor.ts", "ts"],
    ["Dockerfile", "Dockerfile"],
    ["Makefile", "Makefile"],
    ["makefile", "makefile"],
    ["/repo/build/Dockerfile", "Dockerfile"],
    ["a/b/c/CMakeLists.txt", "CMakeLists"],
    ["README", ""],
    ["/some/dir/LICENSE", ""],
  ])("%j gives %j", (path, expected) => {
    expect(langHintForPath(path)).toBe(expected);
  });

  it.each([".gitignore", ".env"])("dotfile %j has no recognised language", (path) => {
    expect(langIdForHint(langHintForPath(path))).toBeNull();
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
