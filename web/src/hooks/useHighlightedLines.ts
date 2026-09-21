import { useEffect, useRef, useState } from "react";
import { getSnippetHighlighter, langHintForPath, type ThemedToken } from "../lib/snippetHighlighter";
import type { RichDiffHunk } from "../lib/types";
import { useShikiTheme } from "./useShikiTheme";

export interface SyntaxToken {
  content: string;
  color?: string;
}

export type TokenGrid = SyntaxToken[][][];

interface GridState {
  grid: TokenGrid;
  path: string;
}

export interface HighlightResult {
  tokens: TokenGrid | null;
}

function tokenizeHunks(
  hunks: RichDiffHunk[],
  hl: { codeToTokens: (code: string, opts: { lang: string; theme: string }) => { tokens: unknown[] } },
  lang: string,
  theme: string,
): TokenGrid {
  return hunks.map((hunk) =>
    hunk.lines.map((line) => {
      const raw = line.content.replace(/\r?\n$/, "");
      if (!raw) return [];
      try {
        const { tokens } = hl.codeToTokens(raw, { lang, theme });
        return (
          (tokens[0] as ThemedToken[] | undefined)?.map((t) => ({ content: t.content, color: t.color })) ?? [
            { content: raw },
          ]
        );
      } catch {
        return [{ content: raw }];
      }
    }),
  );
}

export function useHighlightedLines(hunks: RichDiffHunk[], filePath: string): HighlightResult {
  const [state, setState] = useState<GridState | null>(null);
  const requestRef = useRef(0);
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

        const result = tokenizeHunks(hunks, hl, langId, resolvedTheme);

        if (isMountedRef.current && reqId === requestRef.current) {
          setState({ grid: result, path: filePath });
        }
      } catch (err) {
        if (isMountedRef.current && reqId === requestRef.current) {
          console.error("useHighlightedLines: highlighter failed", err);
          setState({ grid: [], path: filePath });
        }
      }
    })();
  }, [hunks, filePath, shiki.theme, shiki.appearance]);

  const tokens = state && state.path === filePath ? state.grid : null;
  return { tokens };
}
