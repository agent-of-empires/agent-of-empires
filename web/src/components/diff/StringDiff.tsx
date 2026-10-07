// Inline diff of an (old_string, new_string) pair for the Edit/Write card.

import { useMemo } from "react";
import { useHighlightedLines } from "../../hooks/useHighlightedLines";
import { diffPair } from "../../lib/diffPair";
import type { RichDiffHunk } from "../../lib/types";
import { DiffLine } from "./DiffLine";

interface Props {
  oldText: string;
  newText: string;
  /** Used for language detection; unknown paths render unhighlighted. */
  filePath: string;
}

export function StringDiff({ oldText, newText, filePath }: Props) {
  const hunk: RichDiffHunk = useMemo(() => diffPair(oldText, newText).hunk, [oldText, newText]);
  const hunks = useMemo(() => [hunk], [hunk]);
  const { tokens } = useHighlightedLines(hunks, filePath);

  if (hunk.lines.length === 0) return null;

  const lineTokens = tokens?.[0];

  return (
    // The card clips overflow and lines are whitespace-pre, so this owns horizontal scroll.
    <div data-testid="string-diff" className="leading-[1.6] overflow-x-auto">
      {hunk.lines.map((line, i) => (
        <DiffLine
          key={`${line.old_line_num ?? "_"}-${line.new_line_num ?? "_"}-${i}`}
          line={line}
          tokens={lineTokens?.[i]}
          hideLineNumbers
        />
      ))}
    </div>
  );
}
