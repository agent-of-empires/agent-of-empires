import type { HighlighterCore, ThemedToken } from "shiki";
import { createHighlighterCore } from "shiki/core";
import { createOnigurumaEngine } from "shiki/engine/oniguruma";

let instance: HighlighterCore | null = null;
let loading: Promise<HighlighterCore> | null = null;

/** Shiki theme module imports for every theme an AoE-resolved theme
 *  can name. Unknown values fall back to `DEFAULT_SHIKI_THEME`. Lazy
 *  imports so users on Empire don't pay for the Dracula/Tokyo Night
 *  modules they never see. */
const SHIKI_THEME_IMPORTS: Record<string, () => Promise<unknown>> = {
  "github-dark": () => import("shiki/themes/github-dark.mjs"),
  "github-light": () => import("shiki/themes/github-light.mjs"),
  "github-dark-dimmed": () => import("shiki/themes/github-dark-dimmed.mjs"),
  "tokyo-night": () => import("shiki/themes/tokyo-night.mjs"),
  "catppuccin-latte": () => import("shiki/themes/catppuccin-latte.mjs"),
  dracula: () => import("shiki/themes/dracula.mjs"),
  "rose-pine": () => import("shiki/themes/rose-pine.mjs"),
  "material-theme-ocean": () => import("shiki/themes/material-theme-ocean.mjs"),
};

/** Fallback Shiki themes when the resolver names a theme this bundle
 *  doesn't carry (user-defined themes with arbitrary shiki_theme
 *  entries). Picked by appearance so a light AoE theme falling back
 *  doesn't end up rendering code on a light surface with a dark
 *  syntax theme. */
export const DEFAULT_SHIKI_THEME = "github-dark";
export const DEFAULT_SHIKI_THEME_LIGHT = "github-light";

export function fallbackShikiTheme(
  appearance: "dark" | "light" | undefined,
): string {
  return appearance === "light"
    ? DEFAULT_SHIKI_THEME_LIGHT
    : DEFAULT_SHIKI_THEME;
}

/**
 * Returns a singleton Shiki highlighter. Languages are loaded on demand
 * via `loadLanguage()` so the initial bundle stays small. The default
 * theme is bundled at construction time; switch to another bundled
 * theme via `ensureThemeLoaded`.
 */
export async function getHighlighter(): Promise<HighlighterCore> {
  if (instance) return instance;
  if (loading) return loading;
  loading = createHighlighterCore({
    themes: [import("shiki/themes/github-dark.mjs")],
    langs: [],
    engine: createOnigurumaEngine(import("shiki/wasm")),
  }).then((hl) => {
    instance = hl;
    return hl;
  });
  return loading;
}

/** Lazy-load a Shiki theme module and register it on the singleton
 *  highlighter. Returns the name the caller should pass to
 *  `codeToHtml` / `codeToTokens`: the requested name if it loaded
 *  cleanly, otherwise an appearance-appropriate fallback
 *  (`github-dark` / `github-light`) so a light AoE theme isn't
 *  rendered with a dark syntax palette. Idempotent. */
export async function ensureThemeLoaded(
  name: string,
  appearance?: "dark" | "light",
): Promise<string> {
  const importer = SHIKI_THEME_IMPORTS[name];
  if (!importer) return fallbackShikiTheme(appearance);
  const hl = await getHighlighter();
  if (hl.getLoadedThemes().includes(name)) return name;
  try {
    const mod = await importer();
    const theme = (mod as { default: unknown }).default;
    if (theme) {
      await hl.loadTheme(theme as Parameters<typeof hl.loadTheme>[0]);
      return name;
    }
  } catch {
    // Module load failed; fall through.
  }
  return fallbackShikiTheme(appearance);
}

const EXT_TO_LANG: Record<string, () => Promise<unknown>> = {
  ts: () => import("shiki/langs/typescript.mjs"),
  tsx: () => import("shiki/langs/tsx.mjs"),
  js: () => import("shiki/langs/javascript.mjs"),
  jsx: () => import("shiki/langs/jsx.mjs"),
  mjs: () => import("shiki/langs/javascript.mjs"),
  cjs: () => import("shiki/langs/javascript.mjs"),
  rs: () => import("shiki/langs/rust.mjs"),
  py: () => import("shiki/langs/python.mjs"),
  rb: () => import("shiki/langs/ruby.mjs"),
  go: () => import("shiki/langs/go.mjs"),
  java: () => import("shiki/langs/java.mjs"),
  kt: () => import("shiki/langs/kotlin.mjs"),
  kts: () => import("shiki/langs/kotlin.mjs"),
  swift: () => import("shiki/langs/swift.mjs"),
  c: () => import("shiki/langs/c.mjs"),
  h: () => import("shiki/langs/c.mjs"),
  cpp: () => import("shiki/langs/cpp.mjs"),
  hpp: () => import("shiki/langs/cpp.mjs"),
  cc: () => import("shiki/langs/cpp.mjs"),
  cs: () => import("shiki/langs/csharp.mjs"),
  css: () => import("shiki/langs/css.mjs"),
  scss: () => import("shiki/langs/scss.mjs"),
  less: () => import("shiki/langs/less.mjs"),
  html: () => import("shiki/langs/html.mjs"),
  htm: () => import("shiki/langs/html.mjs"),
  vue: () => import("shiki/langs/vue.mjs"),
  svelte: () => import("shiki/langs/svelte.mjs"),
  json: () => import("shiki/langs/json.mjs"),
  jsonc: () => import("shiki/langs/jsonc.mjs"),
  yaml: () => import("shiki/langs/yaml.mjs"),
  yml: () => import("shiki/langs/yaml.mjs"),
  toml: () => import("shiki/langs/toml.mjs"),
  md: () => import("shiki/langs/markdown.mjs"),
  mdx: () => import("shiki/langs/mdx.mjs"),
  sh: () => import("shiki/langs/shellscript.mjs"),
  bash: () => import("shiki/langs/shellscript.mjs"),
  zsh: () => import("shiki/langs/shellscript.mjs"),
  fish: () => import("shiki/langs/shellscript.mjs"),
  sql: () => import("shiki/langs/sql.mjs"),
  graphql: () => import("shiki/langs/graphql.mjs"),
  gql: () => import("shiki/langs/graphql.mjs"),
  dockerfile: () => import("shiki/langs/dockerfile.mjs"),
  docker: () => import("shiki/langs/dockerfile.mjs"),
  xml: () => import("shiki/langs/xml.mjs"),
  svg: () => import("shiki/langs/xml.mjs"),
  lua: () => import("shiki/langs/lua.mjs"),
  php: () => import("shiki/langs/php.mjs"),
  r: () => import("shiki/langs/r.mjs"),
  scala: () => import("shiki/langs/scala.mjs"),
  zig: () => import("shiki/langs/zig.mjs"),
  elixir: () => import("shiki/langs/elixir.mjs"),
  ex: () => import("shiki/langs/elixir.mjs"),
  exs: () => import("shiki/langs/elixir.mjs"),
  erl: () => import("shiki/langs/erlang.mjs"),
  hrl: () => import("shiki/langs/erlang.mjs"),
  hs: () => import("shiki/langs/haskell.mjs"),
  ml: () => import("shiki/langs/ocaml.mjs"),
  mli: () => import("shiki/langs/ocaml.mjs"),
  clj: () => import("shiki/langs/clojure.mjs"),
  dart: () => import("shiki/langs/dart.mjs"),
  tf: () => import("shiki/langs/hcl.mjs"),
  hcl: () => import("shiki/langs/hcl.mjs"),
  astro: () => import("shiki/langs/astro.mjs"),
  nix: () => import("shiki/langs/nix.mjs"),
};

/** Filename-based overrides for files without a meaningful extension. */
const FILENAME_TO_LANG: Record<string, () => Promise<unknown>> = {
  Dockerfile: () => import("shiki/langs/dockerfile.mjs"),
  Makefile: () => import("shiki/langs/make.mjs"),
  makefile: () => import("shiki/langs/make.mjs"),
  CMakeLists: () => import("shiki/langs/cmake.mjs"),
};

/**
 * Resolve a file path to a Shiki language import. Returns null for
 * unrecognised extensions so the caller can fall back to plain text.
 */
export function langImportForPath(
  filePath: string,
): (() => Promise<unknown>) | null {
  const basename = filePath.split("/").pop() ?? filePath;
  const nameNoExt = basename.split(".")[0] ?? "";
  if (FILENAME_TO_LANG[nameNoExt]) return FILENAME_TO_LANG[nameNoExt];
  if (FILENAME_TO_LANG[basename]) return FILENAME_TO_LANG[basename];
  const ext = basename.includes(".")
    ? basename.split(".").pop()!.toLowerCase()
    : "";
  return EXT_TO_LANG[ext] ?? null;
}

/**
 * Aliases that markdown fences and ACP tool output use which Shiki's
 * grammar registry doesn't recognise on its own. Map them to a key
 * Shiki *does* know after loading the underlying grammar (e.g.
 * `console` from claude-agent-acp's Bash output → `bash`, which
 * shellscript.mjs registers as an alias).
 */
const FENCE_ALIASES: Record<string, string> = {
  console: "bash",
  shellsession: "bash",
  "bash-session": "bash",
  terminal: "bash",
  shell: "bash",
  // fish has no separate grammar in the bundled langs; reuse bash so
  // we at least get colored prompts/keywords instead of plain text.
  fish: "bash",
  // Long-form names that markdown fences commonly use, mapped to the
  // short EXT_TO_LANG keys above. Without these, ```rust``` resolves
  // to lang="rust" which is neither registered in our EXT_TO_LANG nor
  // recognised by Shiki at codeToHtml time, so the highlighter throws
  // and we fall through to the plain-pre branch.
  rust: "rs",
  python: "py",
  ruby: "rb",
  typescript: "ts",
  javascript: "js",
  golang: "go",
  csharp: "cs",
  "c#": "cs",
  cplusplus: "cpp",
  "c++": "cpp",
  kotlin: "kt",
  haskell: "hs",
  shellscript: "bash",
  markdown: "md",
  // Common typos / casing that the markdown side surfaces.
  yml: "yaml",
};

/**
 * Map a markdown fence language hint (e.g. `bash`, `tsx`, `js`) to a
 * Shiki language key. Falls back to the hint itself.
 */
export function langKeyForExt(ext: string): string | null {
  const lower = ext.toLowerCase();
  const canonical = FENCE_ALIASES[lower] ?? lower;
  if (EXT_TO_LANG[canonical] || FILENAME_TO_LANG[ext]) return canonical;
  return null;
}

/**
 * Lazy-load and register a Shiki language by its key. Idempotent.
 */
export async function loadLanguage(langKey: string): Promise<void> {
  const importer = EXT_TO_LANG[langKey] ?? FILENAME_TO_LANG[langKey];
  if (!importer) return;
  const hl = await getHighlighter();
  if (hl.getLoadedLanguages().includes(langKey)) return;
  const mod = await importer();
  // Shiki language modules export the lang object as default.
  const lang = (mod as { default: unknown }).default;
  if (lang) {
    await hl.loadLanguage(lang as Parameters<typeof hl.loadLanguage>[0]);
  }
}

export type { ThemedToken };
