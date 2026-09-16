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

describe("fallbackShikiTheme", () => {
  it("returns the dark default for dark appearance", () => {
    expect(fallbackShikiTheme("dark")).toBe(DEFAULT_SHIKI_THEME);
    expect(DEFAULT_SHIKI_THEME).toBe("github-dark");
  });

  it("returns the light default for light appearance", () => {
    expect(fallbackShikiTheme("light")).toBe(DEFAULT_SHIKI_THEME_LIGHT);
    expect(DEFAULT_SHIKI_THEME_LIGHT).toBe("github-light");
  });

  it("returns the dark default when appearance is undefined", () => {
    expect(fallbackShikiTheme(undefined)).toBe(DEFAULT_SHIKI_THEME);
  });
});

describe("resolveSnippetTheme", () => {
  it("returns a known theme unchanged", () => {
    expect(resolveSnippetTheme("dracula", "dark")).toBe("dracula");
  });

  it("returns the appearance-appropriate fallback for an unknown theme, warning once", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    expect(resolveSnippetTheme("not-a-real-theme", "light")).toBe(DEFAULT_SHIKI_THEME_LIGHT);
    expect(resolveSnippetTheme("not-a-real-theme", "dark")).toBe(DEFAULT_SHIKI_THEME);
    expect(resolveSnippetTheme("not-a-real-theme")).toBe(DEFAULT_SHIKI_THEME);
    expect(warn).toHaveBeenCalledTimes(1);
    warn.mockRestore();
  });

  it("accepts every other registered theme cleanly", () => {
    for (const theme of [
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
    ]) {
      expect(resolveSnippetTheme(theme, "dark")).toBe(theme);
    }
  });
});

// The Rust side asserts every builtin theme *has* a `shiki_theme`; this asserts
// the id it names is one shiki can actually load, which the wholesale registry
// read no longer checks for us.
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

describe("langIdForHint", () => {
  it("passes through ids Shiki already bundles under that name", () => {
    expect(langIdForHint("typescript")).toBe("typescript");
    expect(langIdForHint("json")).toBe("json");
    expect(langIdForHint("ts")).toBe("ts");
    expect(langIdForHint("rs")).toBe("rs");
    expect(langIdForHint("yaml")).toBe("yaml");
    expect(langIdForHint("console")).toBe("console");
    expect(langIdForHint("c++")).toBe("c++");
    expect(langIdForHint("c#")).toBe("c#");
  });

  it("maps the residual extension gaps Shiki doesn't alias itself", () => {
    expect(langIdForHint("h")).toBe("c");
    expect(langIdForHint("hpp")).toBe("cpp");
    expect(langIdForHint("cc")).toBe("cpp");
    expect(langIdForHint("htm")).toBe("html");
    expect(langIdForHint("svg")).toBe("xml");
    expect(langIdForHint("ex")).toBe("elixir");
    expect(langIdForHint("exs")).toBe("elixir");
    expect(langIdForHint("hrl")).toBe("erlang");
    expect(langIdForHint("ml")).toBe("ocaml");
    expect(langIdForHint("mli")).toBe("ocaml");
  });

  it("resolves fence aliases Shiki doesn't ship", () => {
    expect(langIdForHint("golang")).toBe("go");
    expect(langIdForHint("cplusplus")).toBe("cpp");
    expect(langIdForHint("bash-session")).toBe("bash");
    expect(langIdForHint("terminal")).toBe("bash");
  });

  it("is case insensitive", () => {
    expect(langIdForHint("RUST")).toBe("rust");
    expect(langIdForHint("Python")).toBe("python");
    expect(langIdForHint("TS")).toBe("ts");
  });

  it("resolves filename-based keys", () => {
    expect(langIdForHint("Dockerfile")).toBe("dockerfile");
    expect(langIdForHint("Makefile")).toBe("make");
    expect(langIdForHint("makefile")).toBe("make");
    expect(langIdForHint("CMakeLists")).toBe("cmake");
  });

  it("returns null for hints Shiki doesn't recognise", () => {
    expect(langIdForHint("notalang")).toBeNull();
    expect(langIdForHint("")).toBeNull();
    expect(langIdForHint("unknownext")).toBeNull();
    expect(langIdForHint("constructor")).toBeNull();
    expect(langIdForHint("toString")).toBeNull();
  });
});

describe("langHintForPath", () => {
  it("resolves extensions through directory paths", () => {
    expect(langHintForPath("src/lib/highlighter.ts")).toBe("ts");
    expect(langHintForPath("/abs/path/to/main.rs")).toBe("rs");
    expect(langHintForPath("src/constructor.ts")).toBe("ts");
  });

  it("resolves filename overrides without an extension", () => {
    expect(langHintForPath("Dockerfile")).toBe("Dockerfile");
    expect(langHintForPath("Makefile")).toBe("Makefile");
    expect(langHintForPath("makefile")).toBe("makefile");
  });

  it("resolves filename overrides through directory paths", () => {
    expect(langHintForPath("/repo/build/Dockerfile")).toBe("Dockerfile");
    expect(langHintForPath("a/b/c/CMakeLists.txt")).toBe("CMakeLists");
  });

  it("returns an empty hint for files with no extension", () => {
    expect(langHintForPath("README")).toBe("");
    expect(langHintForPath("/some/dir/LICENSE")).toBe("");
  });

  it("treats dotfiles as having no recognised extension", () => {
    expect(langIdForHint(langHintForPath(".gitignore"))).toBeNull();
    expect(langIdForHint(langHintForPath(".env"))).toBeNull();
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
