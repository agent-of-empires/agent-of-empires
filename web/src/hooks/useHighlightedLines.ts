import { useEffect, useRef, useState } from "react";
import { getSnippetHighlighter, langHintForPath, type ThemedToken } from "../lib/snippetHighlighter";
import type { RichDiffHunk } from "../lib/types";
import { useShikiTheme } from "./useShikiTheme";

// Highlights on the main thread via the shared `@pierre/diffs` highlighter,
// not the diff pane's worker pool: short per-line highlighting doesn't
// benefit from a worker round-trip.

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

    const langHint = langHintForPath(filePath);
    if (!langHint) return;

    (async () => {
      try {
        const resolved = await getSnippetHighlighter({ langHint, theme: shiki.theme, appearance: shiki.appearance });

        if (!isMountedRef.current || reqId !== requestRef.current) return;

        if (!resolved) {
          setState({ grid: [], path: filePath });
          return;
        }
        const { highlighter: hl, langId, theme: resolvedTheme } = resolved;

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
