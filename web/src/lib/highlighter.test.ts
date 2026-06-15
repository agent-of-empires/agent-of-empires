import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// Stub the shiki core/engine so no real wasm or grammar loads happen.
// `createHighlighterCore` returns a fake HighlighterCore whose loaded-theme
// and loaded-language state is driven by the `loadTheme` / `loadLanguage`
// spies, letting us assert idempotency and fallback branches deterministically.
const loadedThemes: string[] = [];
const loadedLanguages: string[] = [];

const loadThemeMock = vi.fn(async (theme: { name?: string }) => {
  if (theme?.name) loadedThemes.push(theme.name);
});
const loadLanguageMock = vi.fn(async (lang: { name?: string }) => {
  if (lang?.name) loadedLanguages.push(lang.name);
});

const createHighlighterCoreMock = vi.fn(async () => ({
  getLoadedThemes: () => loadedThemes,
  getLoadedLanguages: () => loadedLanguages,
  loadTheme: loadThemeMock,
  loadLanguage: loadLanguageMock,
}));

vi.mock("shiki/core", () => ({
  createHighlighterCore: (...args: unknown[]) => createHighlighterCoreMock(...args),
}));

vi.mock("shiki/engine/oniguruma", () => ({
  createOnigurumaEngine: vi.fn(() => ({})),
}));

// The dynamic `import("shiki/wasm")` inside getHighlighter is passed straight
// into the stubbed engine, so we don't need to resolve it to a real module.
vi.mock("shiki/wasm", () => ({ default: {} }));

// Every theme module the source can import. Each resolves to a `default`
// carrying a `name`, except the two below that exercise fallback branches.
vi.mock("shiki/themes/github-dark.mjs", () => ({ default: { name: "github-dark" } }));
vi.mock("shiki/themes/github-light.mjs", () => ({ default: { name: "github-light" } }));
vi.mock("shiki/themes/github-dark-dimmed.mjs", () => ({ default: { name: "github-dark-dimmed" } }));
vi.mock("shiki/themes/catppuccin-latte.mjs", () => ({ default: { name: "catppuccin-latte" } }));
vi.mock("shiki/themes/dracula.mjs", () => ({ default: { name: "dracula" } }));
vi.mock("shiki/themes/material-theme-ocean.mjs", () => ({ default: { name: "material-theme-ocean" } }));
// Resolves with no usable default -> exercises the `if (theme)` falsy branch.
vi.mock("shiki/themes/tokyo-night.mjs", () => ({ default: undefined }));
// Import rejects -> exercises the catch branch.
vi.mock("shiki/themes/rose-pine.mjs", () => {
  throw new Error("boom");
});

// Every language module the source can import. The exact `name` value is
// irrelevant; loadLanguage only needs a truthy `default`.
const langModule = (name: string) => ({ default: { name } });
vi.mock("shiki/langs/typescript.mjs", () => langModule("ts"));
vi.mock("shiki/langs/tsx.mjs", () => langModule("tsx"));
vi.mock("shiki/langs/javascript.mjs", () => langModule("js"));
vi.mock("shiki/langs/jsx.mjs", () => langModule("jsx"));
vi.mock("shiki/langs/rust.mjs", () => langModule("rs"));
vi.mock("shiki/langs/python.mjs", () => langModule("py"));
vi.mock("shiki/langs/ruby.mjs", () => langModule("rb"));
vi.mock("shiki/langs/go.mjs", () => langModule("go"));
vi.mock("shiki/langs/java.mjs", () => langModule("java"));
vi.mock("shiki/langs/kotlin.mjs", () => langModule("kt"));
vi.mock("shiki/langs/swift.mjs", () => langModule("swift"));
vi.mock("shiki/langs/c.mjs", () => langModule("c"));
vi.mock("shiki/langs/cpp.mjs", () => langModule("cpp"));
vi.mock("shiki/langs/csharp.mjs", () => langModule("cs"));
vi.mock("shiki/langs/css.mjs", () => langModule("css"));
vi.mock("shiki/langs/scss.mjs", () => langModule("scss"));
vi.mock("shiki/langs/less.mjs", () => langModule("less"));
vi.mock("shiki/langs/html.mjs", () => langModule("html"));
vi.mock("shiki/langs/vue.mjs", () => langModule("vue"));
vi.mock("shiki/langs/svelte.mjs", () => langModule("svelte"));
vi.mock("shiki/langs/json.mjs", () => langModule("json"));
vi.mock("shiki/langs/jsonc.mjs", () => langModule("jsonc"));
vi.mock("shiki/langs/yaml.mjs", () => langModule("yaml"));
vi.mock("shiki/langs/toml.mjs", () => langModule("toml"));
vi.mock("shiki/langs/markdown.mjs", () => langModule("md"));
vi.mock("shiki/langs/mdx.mjs", () => langModule("mdx"));
vi.mock("shiki/langs/shellscript.mjs", () => langModule("bash"));
vi.mock("shiki/langs/sql.mjs", () => langModule("sql"));
vi.mock("shiki/langs/graphql.mjs", () => langModule("graphql"));
vi.mock("shiki/langs/dockerfile.mjs", () => langModule("dockerfile"));
vi.mock("shiki/langs/xml.mjs", () => langModule("xml"));
vi.mock("shiki/langs/lua.mjs", () => langModule("lua"));
vi.mock("shiki/langs/php.mjs", () => langModule("php"));
vi.mock("shiki/langs/r.mjs", () => langModule("r"));
vi.mock("shiki/langs/scala.mjs", () => langModule("scala"));
vi.mock("shiki/langs/zig.mjs", () => langModule("zig"));
vi.mock("shiki/langs/elixir.mjs", () => langModule("elixir"));
vi.mock("shiki/langs/erlang.mjs", () => langModule("erlang"));
vi.mock("shiki/langs/haskell.mjs", () => langModule("haskell"));
vi.mock("shiki/langs/ocaml.mjs", () => langModule("ocaml"));
vi.mock("shiki/langs/clojure.mjs", () => langModule("clojure"));
vi.mock("shiki/langs/dart.mjs", () => langModule("dart"));
vi.mock("shiki/langs/hcl.mjs", () => langModule("hcl"));
vi.mock("shiki/langs/astro.mjs", () => langModule("astro"));
vi.mock("shiki/langs/nix.mjs", () => langModule("nix"));
vi.mock("shiki/langs/make.mjs", () => langModule("make"));
vi.mock("shiki/langs/cmake.mjs", () => langModule("cmake"));

import {
  DEFAULT_SHIKI_THEME,
  DEFAULT_SHIKI_THEME_LIGHT,
  ensureThemeLoaded,
  fallbackShikiTheme,
  getHighlighter,
  langImportForPath,
  langKeyForExt,
  loadLanguage,
} from "./highlighter";

beforeEach(() => {
  loadedThemes.length = 0;
  loadedLanguages.length = 0;
  loadThemeMock.mockClear();
  loadLanguageMock.mockClear();
  createHighlighterCoreMock.mockClear();
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

describe("langImportForPath", () => {
  it("resolves common extensions to importer functions", () => {
    const exts = [
      "a.ts",
      "a.tsx",
      "a.js",
      "a.jsx",
      "a.mjs",
      "a.cjs",
      "a.rs",
      "a.py",
      "a.rb",
      "a.go",
      "a.java",
      "a.kt",
      "a.swift",
      "a.c",
      "a.h",
      "a.cpp",
      "a.cs",
      "a.css",
      "a.scss",
      "a.html",
      "a.vue",
      "a.json",
      "a.jsonc",
      "a.yaml",
      "a.yml",
      "a.toml",
      "a.md",
      "a.sh",
      "a.bash",
      "a.sql",
      "a.graphql",
      "a.xml",
      "a.lua",
      "a.php",
      "a.zig",
      "a.nix",
      "a.astro",
    ];
    for (const p of exts) {
      expect(typeof langImportForPath(p)).toBe("function");
    }
  });

  it("uppercase extensions still resolve via toLowerCase", () => {
    expect(typeof langImportForPath("Component.TS")).toBe("function");
    expect(typeof langImportForPath("Main.RS")).toBe("function");
  });

  it("resolves filename overrides without an extension", () => {
    expect(typeof langImportForPath("Dockerfile")).toBe("function");
    expect(typeof langImportForPath("Makefile")).toBe("function");
    expect(typeof langImportForPath("makefile")).toBe("function");
    expect(typeof langImportForPath("CMakeLists")).toBe("function");
  });

  it("resolves filename overrides through directory paths", () => {
    expect(typeof langImportForPath("/repo/build/Dockerfile")).toBe("function");
    expect(typeof langImportForPath("a/b/c/CMakeLists.txt")).toBe("function");
  });

  it("resolves extensions through directory paths", () => {
    expect(typeof langImportForPath("src/lib/highlighter.ts")).toBe("function");
    expect(typeof langImportForPath("/abs/path/to/main.rs")).toBe("function");
  });

  it("returns null for files with no extension", () => {
    expect(langImportForPath("README")).toBeNull();
    expect(langImportForPath("/some/dir/LICENSE")).toBeNull();
  });

  it("returns null for unknown extensions", () => {
    expect(langImportForPath("a.unknownext")).toBeNull();
    expect(langImportForPath("file.xyz")).toBeNull();
  });

  it("treats dotfiles as having no recognised extension", () => {
    // basename ".gitignore" -> nameNoExt "" -> not a filename override,
    // ext is "gitignore" (unknown) -> null.
    expect(langImportForPath(".gitignore")).toBeNull();
    expect(langImportForPath(".env")).toBeNull();
  });
});

describe("langKeyForExt", () => {
  it("maps canonical extensions to themselves", () => {
    expect(langKeyForExt("ts")).toBe("ts");
    expect(langKeyForExt("rs")).toBe("rs");
    expect(langKeyForExt("json")).toBe("json");
    expect(langKeyForExt("yaml")).toBe("yaml");
  });

  it("resolves fence aliases", () => {
    expect(langKeyForExt("console")).toBe("bash");
    expect(langKeyForExt("shellsession")).toBe("bash");
    expect(langKeyForExt("terminal")).toBe("bash");
    expect(langKeyForExt("rust")).toBe("rs");
    expect(langKeyForExt("python")).toBe("py");
    expect(langKeyForExt("ruby")).toBe("rb");
    expect(langKeyForExt("c++")).toBe("cpp");
    expect(langKeyForExt("c#")).toBe("cs");
    expect(langKeyForExt("golang")).toBe("go");
    expect(langKeyForExt("yml")).toBe("yaml");
    expect(langKeyForExt("markdown")).toBe("md");
  });

  it("is case insensitive", () => {
    expect(langKeyForExt("RUST")).toBe("rs");
    expect(langKeyForExt("Python")).toBe("py");
    expect(langKeyForExt("TS")).toBe("ts");
  });

  it("resolves filename-based keys", () => {
    // FILENAME_TO_LANG keys are matched against the original (non-lowered)
    // ext via the `FILENAME_TO_LANG[ext]` branch.
    expect(langKeyForExt("Dockerfile")).toBe("dockerfile");
  });

  it("returns null for unknown hints", () => {
    expect(langKeyForExt("notalang")).toBeNull();
    expect(langKeyForExt("")).toBeNull();
  });
});

describe("getHighlighter", () => {
  it("constructs a single highlighter instance and caches it", async () => {
    const a = await getHighlighter();
    const b = await getHighlighter();
    expect(a).toBe(b);
    expect(createHighlighterCoreMock).toHaveBeenCalledTimes(1);
  });
});

describe("ensureThemeLoaded", () => {
  it("loads a known theme and returns its name", async () => {
    const name = await ensureThemeLoaded("dracula", "dark");
    expect(name).toBe("dracula");
    expect(loadThemeMock).toHaveBeenCalledTimes(1);
    expect(loadedThemes).toContain("dracula");
  });

  it("is idempotent for an already-loaded theme", async () => {
    await ensureThemeLoaded("dracula", "dark");
    loadThemeMock.mockClear();
    const name = await ensureThemeLoaded("dracula", "dark");
    expect(name).toBe("dracula");
    expect(loadThemeMock).not.toHaveBeenCalled();
  });

  it("returns the appearance-appropriate fallback for an unknown theme", async () => {
    expect(await ensureThemeLoaded("not-a-real-theme", "light")).toBe(DEFAULT_SHIKI_THEME_LIGHT);
    expect(await ensureThemeLoaded("not-a-real-theme", "dark")).toBe(DEFAULT_SHIKI_THEME);
    expect(await ensureThemeLoaded("not-a-real-theme")).toBe(DEFAULT_SHIKI_THEME);
  });

  it("falls back when the module resolves without a usable default", async () => {
    const name = await ensureThemeLoaded("tokyo-night", "light");
    expect(name).toBe(DEFAULT_SHIKI_THEME_LIGHT);
    expect(loadThemeMock).not.toHaveBeenCalled();
  });

  it("falls back when the importer rejects", async () => {
    const name = await ensureThemeLoaded("rose-pine", "dark");
    expect(name).toBe(DEFAULT_SHIKI_THEME);
  });

  it("loads every other registered theme cleanly", async () => {
    for (const theme of [
      "github-dark",
      "github-light",
      "github-dark-dimmed",
      "catppuccin-latte",
      "material-theme-ocean",
    ]) {
      expect(await ensureThemeLoaded(theme, "dark")).toBe(theme);
      expect(loadedThemes).toContain(theme);
    }
  });
});

describe("loadLanguage", () => {
  it("loads a known language key", async () => {
    await loadLanguage("ts");
    expect(loadLanguageMock).toHaveBeenCalledTimes(1);
    expect(loadedLanguages).toContain("ts");
  });

  it("is a no-op for an already-loaded key", async () => {
    await loadLanguage("rs");
    loadLanguageMock.mockClear();
    await loadLanguage("rs");
    expect(loadLanguageMock).not.toHaveBeenCalled();
  });

  it("is a no-op for an unknown key", async () => {
    await loadLanguage("definitely-not-a-lang");
    expect(loadLanguageMock).not.toHaveBeenCalled();
  });

  it("loads a filename-override language key (Makefile)", async () => {
    await loadLanguage("Makefile");
    expect(loadedLanguages).toContain("make");
  });

  it("loads every EXT_TO_LANG key, invoking each importer arrow", async () => {
    // Drives every importer thunk in the EXT_TO_LANG and FILENAME_TO_LANG
    // maps at least once so the per-extension arrow functions are covered.
    const keys = [
      "ts",
      "tsx",
      "js",
      "jsx",
      "mjs",
      "cjs",
      "rs",
      "py",
      "rb",
      "go",
      "java",
      "kt",
      "kts",
      "swift",
      "c",
      "h",
      "cpp",
      "hpp",
      "cc",
      "cs",
      "css",
      "scss",
      "less",
      "html",
      "htm",
      "vue",
      "svelte",
      "json",
      "jsonc",
      "yaml",
      "yml",
      "toml",
      "md",
      "mdx",
      "sh",
      "bash",
      "zsh",
      "fish",
      "sql",
      "graphql",
      "gql",
      "dockerfile",
      "docker",
      "xml",
      "svg",
      "lua",
      "php",
      "r",
      "scala",
      "zig",
      "elixir",
      "ex",
      "exs",
      "erl",
      "hrl",
      "hs",
      "ml",
      "mli",
      "clj",
      "dart",
      "tf",
      "hcl",
      "astro",
      "nix",
      "Dockerfile",
      "CMakeLists",
    ];
    for (const k of keys) {
      await loadLanguage(k);
    }
    expect(loadLanguageMock.mock.calls.length).toBeGreaterThan(0);
  });
});

afterEach(() => {
  vi.restoreAllMocks();
});
