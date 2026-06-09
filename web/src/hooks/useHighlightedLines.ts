import { useEffect, useRef, useState } from "react";
import { ensureThemeLoaded, getHighlighter, langImportForPath, type ThemedToken } from "../lib/highlighter";
import type { RichDiffHunk } from "../lib/types";
import { useShikiTheme } from "./useShikiTheme";

/** A single token with content and an optional foreground color. */
export interface SyntaxToken {
  content: string;
  color?: string;
}

/**
 * Tokenized lines indexed by `[hunkIndex][lineIndex]`.
 * Each entry is an array of colored tokens for that line.
 */
export type TokenGrid = SyntaxToken[][][];

interface GridState {
  grid: TokenGrid;
  /** The file path this grid was tokenized for. */
  path: string;
}

export interface HighlightResult {
  /** Tokenized lines, or null if the language is unrecognised or
   *  highlighting has not arrived yet. Callers must render plain text
   *  when null; DiffLine handles this automatically by falling back to
   *  its textClass when no tokens are passed for a row. */
  tokens: TokenGrid | null;
}

/**
 * Asynchronously syntax-highlights all lines in the given diff hunks.
 *
 * Returns `{ tokens }` (null until the grammar loads, null forever for
 * unrecognised languages). Callers must always render the raw text
 * regardless of token state; do not gate visibility on highlighting,
 * since any failure in the async load would otherwise hide content
 * permanently.
 */
export function useHighlightedLines(hunks: RichDiffHunk[], filePath: string): HighlightResult {
  const [state, setState] = useState<GridState | null>(null);
  const requestRef = useRef(0);
  // Tracks whether the host component is still mounted. The async IIFE
  // below awaits Shiki imports / WASM init that can outlive a fast
  // unmount (e.g. test teardown, route switch). Without this guard the
  // final `setState` fires after unmount and React's scheduler then
  // touches a torn-down environment, surfacing as an unhandled
  // "ReferenceError: window is not defined" in Vitest CI.
  const isMountedRef = useRef(true);
  const shiki = useShikiTheme();

  useEffect(() => {
    isMountedRef.current = true;
    return () => {
      isMountedRef.current = false;
    };
  }, []);

  useEffect(() => {
    const reqId = ++requestRef.current;

    const langImport = langImportForPath(filePath);
    if (!langImport) return;

    (async () => {
      try {
        const resolvedTheme = await ensureThemeLoaded(shiki.theme, shiki.appearance);
        const hl = await getHighlighter();

        // Load the grammar if not already registered.
        const mod = await langImport();
        const registration = (mod as Record<string, unknown>).default ?? mod;
        const langs = Array.isArray(registration) ? registration : [registration];
        for (const lang of langs) {
          const id = (lang as { name?: string }).name;
          if (id && !hl.getLoadedLanguages().includes(id)) {
            await hl.loadLanguage(lang as Parameters<typeof hl.loadLanguage>[0]);
          }
        }

        if (!isMountedRef.current || reqId !== requestRef.current) return;

        // Determine the language id from the first registration.
        const langId = (langs[0] as { name?: string }).name;
        if (!langId) {
          setState({ grid: [], path: filePath });
          return;
        }

        const result: TokenGrid = [];

        for (const hunk of hunks) {
          const hunkTokens: SyntaxToken[][] = [];
          for (const line of hunk.lines) {
            const raw = line.content.replace(/\r?\n$/, "");
            if (!raw) {
              hunkTokens.push([]);
              continue;
            }
            try {
              const { tokens } = hl.codeToTokens(raw, {
                lang: langId,
                theme: resolvedTheme,
              });
              const mapped: SyntaxToken[] = (tokens[0] as ThemedToken[] | undefined)?.map((t) => ({
                content: t.content,
                color: t.color,
              })) ?? [{ content: raw }];
              hunkTokens.push(mapped);
            } catch {
              hunkTokens.push([{ content: raw }]);
            }
          }
          result.push(hunkTokens);
        }

        if (isMountedRef.current && reqId === requestRef.current) {
          setState({ grid: result, path: filePath });
        }
      } catch (err) {
        // Theme load, grammar import, or highlighter init failed.
        // Settle state with an empty grid so callers stop waiting and
        // the diff renders unstyled (DiffLine falls back to textClass
        // when tokens is undefined for a row). Without this, an
        // unhandled rejection would leave loading=true forever.
        if (isMountedRef.current && reqId === requestRef.current) {
          console.error("useHighlightedLines: highlighter failed", err);
          setState({ grid: [], path: filePath });
        }
      }
    })();
  }, [hunks, filePath, shiki.theme, shiki.appearance]);

  // Only return the grid if it matches the current file path.
  const tokens = state && state.path === filePath ? state.grid : null;
  return { tokens };
}
