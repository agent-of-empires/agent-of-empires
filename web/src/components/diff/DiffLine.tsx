import { memo } from "react";
import type { SyntaxToken } from "../../hooks/useHighlightedLines";
import type { RichDiffLine } from "../../lib/types";

interface Props {
  line: RichDiffLine;
  tokens?: SyntaxToken[];
  /** Hide the gutters in compact embedded diffs. */
  hideLineNumbers?: boolean;
}

const STYLE: Record<string, [bg: string, text: string, prefix: string]> = {
  add: ["bg-status-running/5", "text-status-running", "+"],
  delete: ["bg-status-error/5", "text-status-error", "-"],
};

const GUTTER =
  "shrink-0 w-[50px] text-right pr-2 font-mono text-[11px] text-text-dim select-none border-r border-surface-700/30";

function DiffLineImpl({ line, tokens, hideLineNumbers }: Props) {
  const [bgClass, textClass, prefix] = STYLE[line.type] ?? ["", "text-text-secondary", " "];
  const opacity = line.type === "equal" ? 1 : 0.7;
  const content =
    tokens && tokens.length > 0
      ? tokens.map((tok, i) => (
          <span key={i} style={tok.color ? { color: tok.color, opacity } : { opacity }}>
            {tok.content}
          </span>
        ))
      : line.content.replace(/\r?\n$/, "") || " ";

  return (
    <div className={`group flex ${bgClass} hover:brightness-110 transition-[filter] duration-75`}>
      {!hideLineNumbers && (
        <>
          <span className={`${GUTTER} relative`}>{line.old_line_num ?? ""}</span>
          <span className={GUTTER}>{line.new_line_num ?? ""}</span>
        </>
      )}
      <span className={`shrink-0 w-4 text-center font-mono text-[12px] ${textClass} select-none`}>{prefix}</span>
      <span className={`flex-1 font-mono text-[12px] whitespace-pre${tokens ? "" : ` ${textClass}`}`}>{content}</span>
    </div>
  );
}

export const DiffLine = memo(DiffLineImpl);
