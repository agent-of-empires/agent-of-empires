import { getSharedHighlighter, type DiffsHighlighter, type ThemedToken } from "@pierre/diffs";
import { bundledLanguages } from "shiki";
// Shiki's own registry of the themes it bundles, keyed by theme id. Reading
// it means any AoE theme, builtin or user-authored, can name any theme Shiki
// ships; `getSharedHighlighter` throws on a name it doesn't recognise, so
// this validates the name before it ever reaches that call.
import { bundledThemes, type BundledTheme } from "shiki/themes";

/** Fallback Shiki themes for a `shiki_theme` value shiki doesn't ship
 *  (a user-defined theme naming something arbitrary, or a typo). Picked
 *  by appearance so a light AoE theme falling back doesn't end up
 *  rendering code on a light surface with a dark syntax theme. */
export const DEFAULT_SHIKI_THEME = "github-dark";
export const DEFAULT_SHIKI_THEME_LIGHT = "github-light";

export function fallbackShikiTheme(appearance: "dark" | "light" | undefined): string {
  return appearance === "light" ? DEFAULT_SHIKI_THEME_LIGHT : DEFAULT_SHIKI_THEME;
}

/** `Object.hasOwn` rather than a truthy lookup so an arbitrary
 *  `shiki_theme` string cannot reach an inherited property. */
function isBundledTheme(name: string): name is BundledTheme {
  return Object.hasOwn(bundledThemes, name);
}

const warnedUnknownThemes = new Set<string>();

/** A theme naming a palette shiki doesn't ship is almost always a typo, and
 *  the fallback is otherwise silent. Warn once per id, not per render. */
function warnUnknownTheme(name: string): void {
  if (warnedUnknownThemes.has(name)) return;
  warnedUnknownThemes.add(name);
  console.warn(`shiki_theme "${name}" is not a theme Shiki bundles; falling back.`);
}

/** Validate a `shiki_theme` value against Shiki's real theme registry,
 *  returning the appearance-appropriate fallback for a name it doesn't
 *  recognise. */
export function resolveSnippetTheme(name: string, appearance?: "dark" | "light"): string {
  if (!isBundledTheme(name)) {
    warnUnknownTheme(name);
    return fallbackShikiTheme(appearance);
  }
  return name;
}

/** Extension/hint → canonical Shiki language id, for the handful of cases
 *  where the extension itself isn't already a Shiki id or alias. */
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

/** Fence/hint aliases that aren't already covered by Shiki's own alias
 *  table (e.g. `rust`, `python`, `console`, `c++`, `c#`, `yml` all resolve
 *  as themselves; these don't). */
const FENCE_ALIASES: Record<string, string> = {
  golang: "go",
  cplusplus: "cpp",
  "bash-session": "bash",
  terminal: "bash",
};

/** Filename-based overrides for files without a meaningful extension. */
const FILENAME_TO_LANG: Record<string, string> = {
  Dockerfile: "dockerfile",
  Makefile: "make",
  makefile: "make",
  CMakeLists: "cmake",
};

function isBundledLanguage(id: string): id is keyof typeof bundledLanguages {
  return Object.hasOwn(bundledLanguages, id);
}

/** Own-property lookup, so a hint like `constructor` misses instead of
 *  returning an inherited function. */
function lookup(table: Record<string, string>, key: string): string | undefined {
  return Object.hasOwn(table, key) ? table[key] : undefined;
}

/**
 * Resolve an extension, filename, or markdown-fence hint to a Shiki
 * language id. Returns null for a hint Shiki doesn't recognise so the
 * caller can fall back to plain text; never returns a string that would
 * make `getSharedHighlighter` throw.
 */
export function langIdForHint(hint: string): string | null {
  const byFilename = lookup(FILENAME_TO_LANG, hint);
  if (byFilename) return byFilename;
  const lower = hint.toLowerCase();
  const canonical = lookup(FENCE_ALIASES, lower) ?? lookup(EXT_ALIASES, lower) ?? lower;
  return isBundledLanguage(canonical) ? canonical : null;
}

/**
 * Resolve a file path to a language hint: filename overrides (Dockerfile,
 * Makefile, CMakeLists) take priority over the extension.
 */
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

/**
 * Resolve the language + theme for a snippet and return the shared
 * main-thread highlighter (the same singleton `@pierre/diffs`'s worker
 * pool pre-resolves languages/themes against for the diff pane), ready
 * for `codeToHtml`/`codeToTokens`. Returns null when the language hint
 * is unrecognised.
 */
export async function getSnippetHighlighter(opts: SnippetHighlightOpts): Promise<ResolvedSnippetHighlighter | null> {
  const langId = langIdForHint(opts.langHint);
  if (!langId) return null;
  const theme = resolveSnippetTheme(opts.theme, opts.appearance);
  const highlighter = await getSharedHighlighter({ themes: [theme], langs: [langId] });
  return { highlighter, langId, theme };
}

/**
 * One-shot HTML highlighting for a code snippet. Returns null when the
 * language hint is unrecognised so the caller renders plain text.
 */
export async function highlightSnippet(code: string, opts: SnippetHighlightOpts): Promise<string | null> {
  const resolved = await getSnippetHighlighter(opts);
  if (!resolved) return null;
  return resolved.highlighter.codeToHtml(code, { lang: resolved.langId, theme: resolved.theme });
}

export type { ThemedToken };
