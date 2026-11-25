import { getSharedHighlighter, type DiffsHighlighter, type ThemedToken } from "@pierre/diffs";
import { bundledLanguages } from "shiki";
// Shiki's bundled theme registry, used to validate names before `getSharedHighlighter` throws on them.
import { bundledThemes, type BundledTheme } from "shiki/themes";
import { DEFAULT_SHIKI_THEME, DEFAULT_SHIKI_THEME_LIGHT } from "./shikiDefaults";

export { DEFAULT_SHIKI_THEME, DEFAULT_SHIKI_THEME_LIGHT };

export function fallbackShikiTheme(appearance: "dark" | "light" | undefined): string {
  return appearance === "light" ? DEFAULT_SHIKI_THEME_LIGHT : DEFAULT_SHIKI_THEME;
}

/** `Object.hasOwn`, so an arbitrary name cannot reach an inherited property. */
function isBundledTheme(name: string): name is BundledTheme {
  return Object.hasOwn(bundledThemes, name);
}

const warnedUnknownThemes = new Set<string>();

/** Warn once per unknown id, not per render. */
function warnUnknownTheme(name: string): void {
  if (warnedUnknownThemes.has(name)) return;
  warnedUnknownThemes.add(name);
  console.warn(`shiki_theme "${name}" is not a theme Shiki bundles; falling back.`);
}

export function resolveSnippetTheme(name: string, appearance?: "dark" | "light"): string {
  if (!isBundledTheme(name)) {
    warnUnknownTheme(name);
    return fallbackShikiTheme(appearance);
  }
  return name;
}

/** Hints whose extension is not already a Shiki id or alias. */
const EXT_ALIASES: Record<string, string> = {
  h: "c",
  hpp: "cpp",
  cc: "cpp",
  htm: "html",
  svg: "xml",
  ex: "elixir",
  exs: "elixir",
  hrl: "erlang",
  ml: "ocaml",
  mli: "ocaml",
};

/** Fence aliases Shiki's own alias table doesn't cover. */
const FENCE_ALIASES: Record<string, string> = {
  golang: "go",
  cplusplus: "cpp",
  "bash-session": "bash",
  terminal: "bash",
};

const FILENAME_TO_LANG: Record<string, string> = {
  Dockerfile: "dockerfile",
  Makefile: "make",
  makefile: "make",
  CMakeLists: "cmake",
};

function isBundledLanguage(id: string): id is keyof typeof bundledLanguages {
  return Object.hasOwn(bundledLanguages, id);
}

/** Own-property lookup, so a hint like `constructor` misses. */
function lookup(table: Record<string, string>, key: string): string | undefined {
  return Object.hasOwn(table, key) ? table[key] : undefined;
}

/** Null for a hint Shiki doesn't recognise; never returns an id that would make `getSharedHighlighter` throw. */
export function langIdForHint(hint: string): string | null {
  const byFilename = lookup(FILENAME_TO_LANG, hint);
  if (byFilename) return byFilename;
  const lower = hint.toLowerCase();
  const canonical = lookup(FENCE_ALIASES, lower) ?? lookup(EXT_ALIASES, lower) ?? lower;
  return isBundledLanguage(canonical) ? canonical : null;
}

/** Filename overrides (Dockerfile, Makefile) beat the extension. */
export function langHintForPath(filePath: string): string {
  const basename = filePath.split("/").pop() ?? filePath;
  const nameNoExt = basename.split(".")[0] ?? "";
  if (lookup(FILENAME_TO_LANG, nameNoExt)) return nameNoExt;
  if (lookup(FILENAME_TO_LANG, basename)) return basename;
  return basename.includes(".") ? (basename.split(".").pop() ?? "") : "";
}

interface SnippetHighlightOpts {
  langHint: string;
  theme: string;
  appearance?: "dark" | "light";
}

interface ResolvedSnippetHighlighter {
  highlighter: DiffsHighlighter;
  langId: string;
  theme: string;
}

/** The shared highlighter (also used by `@pierre/diffs`), or null for an unrecognised language. */
export async function getSnippetHighlighter(opts: SnippetHighlightOpts): Promise<ResolvedSnippetHighlighter | null> {
  const langId = langIdForHint(opts.langHint);
  if (!langId) return null;
  const theme = resolveSnippetTheme(opts.theme, opts.appearance);
  const highlighter = await getSharedHighlighter({ themes: [theme], langs: [langId] });
  return { highlighter, langId, theme };
}

const HIGHLIGHT_CACHE_CAP = 64;
const highlightCache = new Map<string, string>();

function highlightCacheKey(
  code: string,
  opts: SnippetHighlightOpts,
): { key: string; langId: string; theme: string } | null {
  const langId = langIdForHint(opts.langHint);
  if (!langId) return null;
  const theme = resolveSnippetTheme(opts.theme, opts.appearance);
  return { key: `${theme}\u0000${langId}\u0000${code}`, langId, theme };
}

function cacheRead(key: string): string | undefined {
  const html = highlightCache.get(key);
  if (html === undefined) return undefined;
  highlightCache.delete(key);
  highlightCache.set(key, html);
  return html;
}

/** Cached html for these inputs without awaiting, or undefined. */
export function peekHighlight(code: string, opts: SnippetHighlightOpts): string | undefined {
  const resolved = highlightCacheKey(code, opts);
  return resolved ? cacheRead(resolved.key) : undefined;
}

/** Null for an unrecognised language, so the caller renders plain text. */
export async function highlightSnippet(code: string, opts: SnippetHighlightOpts): Promise<string | null> {
  const resolved = highlightCacheKey(code, opts);
  if (!resolved) return null;
  const hit = cacheRead(resolved.key);
  if (hit !== undefined) return hit;
  const highlighter = await getSharedHighlighter({ themes: [resolved.theme], langs: [resolved.langId] });
  const html = highlighter.codeToHtml(code, { lang: resolved.langId, theme: resolved.theme });
  highlightCache.set(resolved.key, html);
  if (highlightCache.size > HIGHLIGHT_CACHE_CAP) highlightCache.delete(highlightCache.keys().next().value!);
  return html;
}

export type { ThemedToken };
