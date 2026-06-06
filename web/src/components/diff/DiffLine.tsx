import { memo } from "react";
import type { SyntaxToken } from "../../hooks/useHighlightedLines";
import type { RichDiffLine } from "../../lib/types";

type PlusSide = "old" | "new";

interface Props {
  line: RichDiffLine;
  tokens?: SyntaxToken[];
  /** Hide the per-side line-number gutters. Used inside compact embedded
   *  diffs (e.g. the structured view Edit card) where snippet line numbers add
   *  more clutter than signal. */
  hideLineNumbers?: boolean;
  /** Render the hover-revealed "+" gutter button for the diff-comments
   *  feature (#928). Props are primitives instead of a ReactNode slot
   *  so `React.memo`'s shallow comparison succeeds across parent
   *  re-renders — passing a fresh `<PlusButton/>` JSX node per row
   *  would bust memo on every range-selection state change. */
  plusEnabled?: boolean;
  plusHunkIndex?: number;
  plusSide?: PlusSide;
  plusLineNum?: number;
  plusActive?: boolean;
  onPlusClick?: (hunkIndex: number, side: PlusSide, lineNum: number) => void;
  /** Tint the row to indicate it's part of the active comment range. */
  isHighlighted?: boolean;
  /** Mark the row as the start/end endpoint of a range with a stronger
   *  left border. */
  isRangeEndpoint?: boolean;
}

function DiffLineImpl({
  line,
  tokens,
  hideLineNumbers,
  plusEnabled,
  plusHunkIndex,
  plusSide,
  plusLineNum,
  plusActive,
  onPlusClick,
  isHighlighted,
  isRangeEndpoint,
}: Props) {
  let bgClass = "";
  let textClass = "text-text-secondary";
  let prefix = " ";

  if (line.type === "add") {
    bgClass = "bg-status-running/5";
    textClass = "text-status-running";
    prefix = "+";
  } else if (line.type === "delete") {
    bgClass = "bg-status-error/5";
    textClass = "text-status-error";
    prefix = "-";
  }

  const highlightOverlay = isHighlighted ? " bg-brand-600/10" : "";
  const endpointBorder = isRangeEndpoint ? " border-l-2 border-brand-600" : "";

  const content = line.content.replace(/\r?\n$/, "");

  const renderContent = () => {
    if (tokens && tokens.length > 0) {
      const opacity = line.type === "equal" ? 1 : 0.7;
      return tokens.map((tok, i) => (
        <span
          key={i}
          style={tok.color ? { color: tok.color, opacity } : { opacity }}
        >
          {tok.content}
        </span>
      ));
    }
    return content || " ";
  };

  const showPlus =
    plusEnabled === true &&
    plusSide != null &&
    plusLineNum != null &&
    plusHunkIndex != null &&
    onPlusClick != null;

  return (
    <div
      className={`group flex ${bgClass}${highlightOverlay}${endpointBorder} hover:brightness-110 transition-[filter] duration-75`}
    >
      {!hideLineNumbers && (
        <>
          <span className="shrink-0 w-[50px] text-right pr-2 font-mono text-[11px] text-text-dim select-none border-r border-surface-700/30 relative">
            {showPlus && (
              <button
                type="button"
                onClick={() =>
                  onPlusClick(plusHunkIndex, plusSide, plusLineNum)
                }
                aria-label={`Add comment on ${plusSide} line ${plusLineNum}`}
                // `tabIndex={-1}` keeps the hover-revealed button out of
                // the tab order; otherwise keyboard users would land on
                // dozens of invisible buttons walking through the diff.
                // v1 is mouse-only; a focus-driven flow can be added
                // with a real row-focus model.
                tabIndex={-1}
                className={`absolute left-0 top-0 h-full w-4 flex items-center justify-center text-white text-[11px] leading-none cursor-pointer transition-opacity ${
                  plusActive
                    ? "bg-brand-600 opacity-100"
                    : "bg-brand-600 opacity-0 group-hover:opacity-100 hover:bg-brand-500 focus-visible:opacity-100"
                }`}
              >
                +
              </button>
            )}
            {line.old_line_num ?? ""}
          </span>
          <span className="shrink-0 w-[50px] text-right pr-2 font-mono text-[11px] text-text-dim select-none border-r border-surface-700/30">
            {line.new_line_num ?? ""}
          </span>
        </>
      )}
      <span
        className={`shrink-0 w-4 text-center font-mono text-[12px] ${textClass} select-none`}
      >
        {prefix}
      </span>
      <span
        className={`flex-1 font-mono text-[12px] whitespace-pre${tokens ? "" : ` ${textClass}`}`}
      >
        {renderContent()}
      </span>
    </div>
  );
}

/** Memoized so range-selection state changes in the parent don't
 *  re-render every line. All props are primitives (or stable callback
 *  references via `useCallback`) so the default shallow comparison is
 *  effective; passing a fresh JSX node per row would defeat memo. */
export const DiffLine = memo(DiffLineImpl);
