import { Fragment } from "react";
import type { SyntaxToken } from "../../hooks/useHighlightedLines";
import type { RichDiffHunk, RichDiffLine } from "../../lib/types";
import { buildSplitRows } from "../../lib/splitDiff";
import { CommentCard } from "./comments/CommentCard";
import { CommentForm } from "./comments/CommentForm";
import type { AnchoredComment, DiffSide } from "./comments/types";

interface Props {
  hunk: RichDiffHunk;
  hunkIndex: number;
  lineTokens?: SyntaxToken[][];
  commentsEnabled: boolean;
  /** Active comment cards for this hunk, keyed by the row index (within
   *  `hunk.lines`) of the anchor's end line. */
  cardsByEndRow: Map<number, AnchoredComment[]>;
  /** Row index (within `hunk.lines`) at which to render the draft form. */
  formRowIndex: number | null;
  draftSide: DiffSide | null;
  draftStartLine: number | null;
  draftEndLine: number | null;
  rangeStart: { side: DiffSide; line: number } | null;
  onPlusClick: (hunkIndex: number, side: DiffSide, lineNum: number) => void;
  onCommentSave: (id: string, body: string) => void;
  onCommentDelete: (id: string) => void;
  onDraftSave: (body: string) => void;
  onDraftCancel: () => void;
}

function renderTokens(line: RichDiffLine, tokens?: SyntaxToken[]) {
  const content = line.content.replace(/\r?\n$/, "");
  if (tokens && tokens.length > 0) {
    const opacity = line.type === "equal" ? 1 : 0.7;
    return tokens.map((tok, i) => (
      <span key={i} style={tok.color ? { color: tok.color, opacity } : { opacity }}>
        {tok.content}
      </span>
    ));
  }
  return content || " ";
}

/** One side (column) of a split row. A null line renders an empty,
 *  subtly-tinted placeholder cell with no gutter number. */
function SplitCell({
  line,
  lineNum,
  tokens,
  side,
  hunkIndex,
  commentsEnabled,
  plusActive,
  onPlusClick,
}: {
  line: RichDiffLine | null;
  lineNum: number | null;
  tokens?: SyntaxToken[];
  side: DiffSide;
  hunkIndex: number;
  commentsEnabled: boolean;
  plusActive: boolean;
  onPlusClick: (hunkIndex: number, side: DiffSide, lineNum: number) => void;
}) {
  if (!line) {
    return <div className="flex flex-1 min-w-0 bg-surface-850/40" />;
  }
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
  const showPlus = commentsEnabled && lineNum != null;
  return (
    <div className={`group/cell flex flex-1 min-w-0 ${bgClass}`}>
      <span className="shrink-0 w-[50px] text-right pr-2 font-mono text-[11px] text-text-dim select-none border-r border-surface-700/30 relative">
        {showPlus && (
          <button
            type="button"
            onClick={() => onPlusClick(hunkIndex, side, lineNum)}
            aria-label={`Add comment on ${side} line ${lineNum}`}
            tabIndex={-1}
            className={`absolute left-0 top-0 h-full w-4 flex items-center justify-center text-white text-[11px] leading-none cursor-pointer transition-opacity ${
              plusActive
                ? "bg-brand-600 opacity-100"
                : "bg-brand-600 opacity-0 group-hover/cell:opacity-100 hover:bg-brand-500"
            }`}
          >
            +
          </button>
        )}
        {lineNum ?? ""}
      </span>
      <span className={`shrink-0 w-4 text-center font-mono text-[12px] ${textClass} select-none`}>
        {prefix}
      </span>
      <span className="flex-1 min-w-0 font-mono text-[12px] whitespace-pre-wrap break-words">
        {renderTokens(line, tokens)}
      </span>
    </div>
  );
}

export function SplitHunkView({
  hunk,
  hunkIndex,
  lineTokens,
  commentsEnabled,
  cardsByEndRow,
  formRowIndex,
  draftSide,
  draftStartLine,
  draftEndLine,
  rangeStart,
  onPlusClick,
  onCommentSave,
  onCommentDelete,
  onDraftSave,
  onDraftCancel,
}: Props) {
  const rows = buildSplitRows(hunk);
  return (
    <div>
      <div className="flex bg-surface-850 border-y border-surface-700/20 sticky top-0 z-[1]">
        <span className="flex-1 font-mono text-[11px] text-accent-600 py-0.5 px-1">
          @@ -{hunk.old_start},{hunk.old_lines} +{hunk.new_start},{hunk.new_lines} @@
        </span>
      </div>
      {rows.map((row, i) => {
        // Cards and forms anchor to a line's index within the hunk, but a
        // modify row carries two lines (old on the left, new on the right).
        // Look up both sides so an old-side comment on a modify row isn't
        // dropped; context rows share one index, so guard against
        // double-counting.
        const leftCards =
          row.leftIndex != null ? cardsByEndRow.get(row.leftIndex) : undefined;
        const rightCards =
          row.rightIndex != null && row.rightIndex !== row.leftIndex
            ? cardsByEndRow.get(row.rightIndex)
            : undefined;
        const cards = [...(leftCards ?? []), ...(rightCards ?? [])];
        const showForm =
          formRowIndex != null &&
          draftSide != null &&
          (formRowIndex === row.leftIndex || formRowIndex === row.rightIndex);
        const leftActive =
          rangeStart?.side === "old" && rangeStart.line === row.left?.old_line_num;
        const rightActive =
          rangeStart?.side === "new" && rangeStart.line === row.right?.new_line_num;
        return (
          <Fragment key={i}>
            <div className="flex">
              <SplitCell
                line={row.left}
                lineNum={row.left?.old_line_num ?? null}
                tokens={row.leftIndex != null ? lineTokens?.[row.leftIndex] : undefined}
                side="old"
                hunkIndex={hunkIndex}
                commentsEnabled={commentsEnabled}
                plusActive={!!leftActive}
                onPlusClick={onPlusClick}
              />
              <div className="w-px bg-surface-700/40 shrink-0" />
              <SplitCell
                line={row.right}
                lineNum={row.right?.new_line_num ?? null}
                tokens={row.rightIndex != null ? lineTokens?.[row.rightIndex] : undefined}
                side="new"
                hunkIndex={hunkIndex}
                commentsEnabled={commentsEnabled}
                plusActive={!!rightActive}
                onPlusClick={onPlusClick}
              />
            </div>
            {cards.map((anchored) => (
              <CommentCard
                key={`card-${anchored.comment.id}`}
                anchored={anchored}
                onSave={onCommentSave}
                onDelete={onCommentDelete}
              />
            ))}
            {showForm && draftSide && draftStartLine != null && draftEndLine != null && (
              <CommentForm
                key={`form-${hunkIndex}-${i}`}
                startLine={draftStartLine}
                endLine={draftEndLine}
                side={draftSide}
                onSave={onDraftSave}
                onCancel={onDraftCancel}
              />
            )}
          </Fragment>
        );
      })}
    </div>
  );
}
