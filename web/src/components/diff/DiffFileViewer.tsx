import { Fragment, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useFileDiff } from "../../hooks/useFileDiff";
import {
  useHighlightedLines,
  type SyntaxToken,
} from "../../hooks/useHighlightedLines";
import { useWebSettings } from "../../hooks/useWebSettings";
import { SplitHunkView } from "./SplitHunkView";
import type { RichDiffHunk, RichDiffLine } from "../../lib/types";
import { DiffLine } from "./DiffLine";
import type { UseDiffCommentsResult } from "../../hooks/useDiffComments";
import { anchorComments } from "./comments/anchor";
import { extractSnippetFromHunks } from "./comments/extractSnippet";
import { extensionToLanguage } from "./comments/language";
import { CommentCard } from "./comments/CommentCard";
import { CommentForm } from "./comments/CommentForm";
import type { AnchoredComment, DiffSide } from "./comments/types";

interface Props {
  sessionId: string;
  filePath: string;
  /** Workspace repo name; passed through to the diff endpoint so the
   *  file is resolved against the correct repo when the session is a
   *  multi-repo workspace. Omit for single-repo sessions. See #1047. */
  repoName?: string;
  /** Triggers a re-fetch when the file list changes. */
  revision?: number;
  /** Called when the user wants to return to the terminal view. */
  onClose?: () => void;
  /** When true, the in-diff comment UI ("+" gutter buttons, inline
   *  cards/forms, stale block) is enabled. False for non-cockpit
   *  sessions where prompts can't be sent. */
  commentsEnabled?: boolean;
  /** Session-scoped comments store. Required when `commentsEnabled`. */
  commentsStore?: UseDiffCommentsResult;
}

const STATUS_LABELS: Record<string, string> = {
  added: "Added",
  modified: "Modified",
  deleted: "Deleted",
  renamed: "Renamed",
  copied: "Copied",
  untracked: "Untracked",
  conflicted: "Conflicted",
};

const STATUS_COLORS: Record<string, string> = {
  added: "text-status-running",
  modified: "text-status-waiting",
  deleted: "text-status-error",
  renamed: "text-accent-600",
  copied: "text-accent-600",
  untracked: "text-text-muted",
  conflicted: "text-status-waiting",
};

/** Transient range-selection state. Local to this viewer because it
 *  changes on every gutter click and shouldn't broadcast through the
 *  comments store. */
interface RangeStart {
  hunkIndex: number;
  side: DiffSide;
  line: number;
}

interface DraftRange {
  hunkIndex: number;
  side: DiffSide;
  startLine: number;
  endLine: number;
  endRowIndex: number;
  snippet: string;
}

function HunkView({
  hunk,
  hunkIndex,
  lineTokens,
  rangeStart,
  draft,
  cardsByEndRow,
  formRowIndex,
  commentsEnabled,
  onPlusClick,
  onCommentSave,
  onCommentDelete,
  onDraftSave,
  onDraftCancel,
}: {
  hunk: RichDiffHunk;
  hunkIndex: number;
  lineTokens?: SyntaxToken[][];
  rangeStart: RangeStart | null;
  draft: DraftRange | null;
  cardsByEndRow: Map<number, AnchoredComment[]>;
  formRowIndex: number | null;
  commentsEnabled: boolean;
  onPlusClick: (hunkIndex: number, side: DiffSide, lineNum: number) => void;
  onCommentSave: (id: string, body: string) => void;
  onCommentDelete: (id: string) => void;
  onDraftSave: (body: string) => void;
  onDraftCancel: () => void;
}) {
  return (
    <div>
      <div className="flex bg-surface-850 border-y border-surface-700/20 sticky top-0 z-[1]">
        <span className="shrink-0 w-[50px] border-r border-surface-700/30" />
        <span className="shrink-0 w-[50px] border-r border-surface-700/30" />
        <span className="shrink-0 w-4" />
        <span className="flex-1 font-mono text-[11px] text-accent-600 py-0.5 px-1">
          @@ -{hunk.old_start},{hunk.old_lines} +{hunk.new_start},{hunk.new_lines} @@
        </span>
      </div>
      {hunk.lines.map((line, i) => {
        const rowKey = `h${hunkIndex}-r${i}`;
        const { isHighlighted, isRangeEndpoint, side, lineNum } =
          highlightForRow(line, hunkIndex, rangeStart, draft);
        const plusEnabled =
          commentsEnabled && lineNum != null && side != null;
        const plusActive =
          plusEnabled &&
          rangeStart != null &&
          rangeStart.hunkIndex === hunkIndex &&
          rangeStart.side === side &&
          rangeStart.line === lineNum;
        const cards = cardsByEndRow.get(i);
        const showForm = formRowIndex === i && draft != null;
        return (
          <Fragment key={rowKey}>
            <DiffLine
              line={line}
              tokens={lineTokens?.[i]}
              plusEnabled={plusEnabled}
              plusHunkIndex={hunkIndex}
              plusSide={side ?? undefined}
              plusLineNum={lineNum ?? undefined}
              plusActive={plusActive}
              onPlusClick={onPlusClick}
              isHighlighted={isHighlighted}
              isRangeEndpoint={isRangeEndpoint}
            />
            {cards?.map((anchored) => (
              <CommentCard
                key={`card-${anchored.comment.id}`}
                anchored={anchored}
                onSave={onCommentSave}
                onDelete={onCommentDelete}
              />
            ))}
            {showForm && draft && (
              <CommentForm
                key={`form-h${hunkIndex}-r${i}`}
                startLine={draft.startLine}
                endLine={draft.endLine}
                side={draft.side}
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

function highlightForRow(
  line: RichDiffLine,
  hunkIndex: number,
  rangeStart: RangeStart | null,
  draft: DraftRange | null,
): {
  isHighlighted: boolean;
  isRangeEndpoint: boolean;
  side: DiffSide | null;
  lineNum: number | null;
} {
  // Side preference: added → new, deleted → old, equal → new.
  const sideForRow: DiffSide =
    line.type === "delete" ? "old" : "new";
  const lineNum =
    sideForRow === "new" ? line.new_line_num : line.old_line_num;
  if (lineNum == null) {
    return {
      isHighlighted: false,
      isRangeEndpoint: false,
      side: null,
      lineNum: null,
    };
  }

  const draftMatch =
    draft != null &&
    draft.hunkIndex === hunkIndex &&
    draft.side === sideForRow &&
    lineNum >= draft.startLine &&
    lineNum <= draft.endLine;
  const startMatch =
    rangeStart != null &&
    rangeStart.hunkIndex === hunkIndex &&
    rangeStart.side === sideForRow &&
    rangeStart.line === lineNum;

  return {
    isHighlighted: draftMatch || startMatch,
    isRangeEndpoint:
      startMatch ||
      (draft != null &&
        draft.hunkIndex === hunkIndex &&
        draft.side === sideForRow &&
        (lineNum === draft.startLine || lineNum === draft.endLine)),
    side: sideForRow,
    lineNum,
  };
}

export function DiffFileViewer({
  sessionId,
  filePath,
  repoName,
  revision,
  onClose,
  commentsEnabled = false,
  commentsStore,
}: Props) {
  const { diff, loading, error } = useFileDiff(
    sessionId,
    filePath,
    repoName,
    revision,
  );
  const { tokens: tokenGrid } = useHighlightedLines(
    diff?.hunks ?? [],
    diff?.file.path ?? filePath,
  );

  const { settings, update } = useWebSettings();
  const [isWide, setIsWide] = useState(true);
  const widthObserverRef = useRef<ResizeObserver | null>(null);

  // Callback ref: the scroll container is rendered only after the loading /
  // error / empty guards below, so it may mount well after first paint. A
  // callback ref attaches the observer the moment the node mounts (and tears it
  // down on unmount), unlike a one-shot mount effect that would miss the late
  // mount and never measure width.
  const measureRef = useCallback((el: HTMLDivElement | null) => {
    widthObserverRef.current?.disconnect();
    widthObserverRef.current = null;
    if (!el || typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver((entries) => {
      setIsWide((entries[0]?.contentRect.width ?? 0) >= 640);
    });
    ro.observe(el);
    widthObserverRef.current = ro;
  }, []);

  const splitActive = settings.diffViewLayout === "split" && isWide;

  const [rangeStart, setRangeStart] = useState<RangeStart | null>(null);
  const [draft, setDraft] = useState<DraftRange | null>(null);

  // Reset transient selection when the viewer switches to a different
  // file / repo / session, or the diff refreshes. Without this an
  // unfinished selection or open draft from file A would render in
  // file B (and save the wrong filePath against the wrong snippet).
  useEffect(() => {
    setRangeStart(null);
    setDraft(null);
  }, [sessionId, repoName, filePath, revision]);

  const hunks = diff?.hunks ?? [];
  const comments = commentsStore?.comments ?? [];

  const anchored: AnchoredComment[] = useMemo(
    () => anchorComments(comments, filePath, repoName, hunks),
    [comments, filePath, repoName, hunks],
  );

  const staleComments = useMemo(
    () => anchored.filter((a) => a.status === "stale"),
    [anchored],
  );

  // Group active anchors per (hunkIndex, endRowIndex) so HunkView can
  // emit cards inline without re-scanning the comments list per row.
  const cardsByHunkRow = useMemo(() => {
    const map = new Map<number, Map<number, AnchoredComment[]>>();
    for (const a of anchored) {
      if (a.status !== "active" || a.hunkIndex == null || a.endRowIndex == null)
        continue;
      let inner = map.get(a.hunkIndex);
      if (!inner) {
        inner = new Map();
        map.set(a.hunkIndex, inner);
      }
      const list = inner.get(a.endRowIndex) ?? [];
      list.push(a);
      inner.set(a.endRowIndex, list);
    }
    return map;
  }, [anchored]);

  const handlePlusClick = useCallback(
    (hunkIndex: number, side: DiffSide, lineNum: number) => {
      if (draft) return; // form is open; ignore further clicks until resolved.
      if (!rangeStart) {
        setRangeStart({ hunkIndex, side, line: lineNum });
        return;
      }
      if (rangeStart.hunkIndex !== hunkIndex || rangeStart.side !== side) {
        // Restart selection on a cross-hunk or cross-side click instead
        // of silently ignoring; the new line becomes the new start.
        setRangeStart({ hunkIndex, side, line: lineNum });
        return;
      }
      const startLine = Math.min(rangeStart.line, lineNum);
      const endLine = Math.max(rangeStart.line, lineNum);
      const extracted = extractSnippetFromHunks(
        hunks,
        side,
        startLine,
        endLine,
      );
      if (!extracted) {
        // Range failed to resolve (probably straddled a non-side row at
        // the boundary). Fall back to single-line on the clicked row.
        setRangeStart(null);
        const fallback = extractSnippetFromHunks(
          hunks,
          side,
          lineNum,
          lineNum,
        );
        if (!fallback) return;
        setDraft({
          hunkIndex,
          side,
          startLine: lineNum,
          endLine: lineNum,
          endRowIndex: fallback.endRowIndex,
          snippet: fallback.snippet,
        });
        return;
      }
      setRangeStart(null);
      setDraft({
        hunkIndex,
        side,
        startLine,
        endLine,
        endRowIndex: extracted.endRowIndex,
        snippet: extracted.snippet,
      });
    },
    [draft, rangeStart, hunks],
  );

  const handleDraftSave = useCallback(
    (body: string) => {
      if (!draft || !commentsStore) return;
      commentsStore.addComment({
        repoName,
        filePath,
        side: draft.side,
        startLine: draft.startLine,
        endLine: draft.endLine,
        body,
        capturedSnippet: draft.snippet,
        language: extensionToLanguage(filePath),
      });
      setDraft(null);
    },
    [draft, commentsStore, repoName, filePath],
  );

  const handleDraftCancel = useCallback(() => {
    setDraft(null);
    setRangeStart(null);
  }, []);

  const handleCommentSave = useCallback(
    (id: string, body: string) => {
      commentsStore?.updateComment(id, body);
    },
    [commentsStore],
  );

  const handleCommentDelete = useCallback(
    (id: string) => {
      commentsStore?.deleteComment(id);
    },
    [commentsStore],
  );

  if (loading && !diff) {
    return (
      <div className="flex-1 flex items-center justify-center bg-surface-900 text-text-dim">
        <span className="text-sm">Loading diff...</span>
      </div>
    );
  }

  if (error) {
    return (
      <div className="flex-1 flex items-center justify-center bg-surface-900 text-status-error">
        <span className="text-sm">{error}</span>
      </div>
    );
  }

  if (!diff) {
    return (
      <div className="flex-1 flex items-center justify-center bg-surface-900 text-text-dim">
        <span className="text-sm">Select a file to view changes</span>
      </div>
    );
  }

  const statusColor = STATUS_COLORS[diff.file.status] ?? "text-text-muted";
  const statusLabel = STATUS_LABELS[diff.file.status] ?? diff.file.status;

  return (
    <div className="flex-1 flex flex-col bg-surface-900 overflow-hidden">
      {/* File header */}
      <div className="px-3 py-2 border-b border-surface-700/20 flex items-center gap-2 shrink-0 flex-wrap">
        {onClose && (
          <button
            onClick={onClose}
            className="text-text-dim hover:text-text-secondary cursor-pointer transition-colors flex items-center gap-1 text-[11px]"
            title="Back to terminal"
            aria-label="Back to terminal"
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.75" strokeLinecap="round" strokeLinejoin="round">
              <path d="M15 18l-6-6 6-6" />
            </svg>
            <span className="hidden sm:inline">Terminal</span>
          </button>
        )}
        <span className={`font-mono text-[11px] font-semibold ${statusColor}`}>
          {statusLabel}
        </span>
        <span className="font-mono text-[12px] text-text-primary truncate">
          {diff.file.old_path
            ? `${diff.file.old_path} → ${diff.file.path}`
            : diff.file.path}
        </span>
        <span className="font-mono text-[11px] flex items-center gap-1">
          {diff.file.additions > 0 && (
            <span className="text-status-running">+{diff.file.additions}</span>
          )}
          {diff.file.deletions > 0 && (
            <span className="text-status-error">-{diff.file.deletions}</span>
          )}
        </span>
        <div className="ml-auto flex items-center rounded border border-surface-700/40 overflow-hidden">
          <button
            type="button"
            onClick={() => update({ diffViewLayout: "unified" })}
            aria-pressed={settings.diffViewLayout === "unified"}
            title="Unified diff"
            className={`px-2 py-0.5 text-[11px] font-mono cursor-pointer transition-colors ${
              settings.diffViewLayout === "unified"
                ? "bg-brand-600 text-white"
                : "text-text-dim hover:text-text-secondary"
            }`}
          >
            Unified
          </button>
          <button
            type="button"
            onClick={() => update({ diffViewLayout: "split" })}
            aria-pressed={settings.diffViewLayout === "split"}
            title={
              settings.diffViewLayout === "split" && !isWide
                ? "Split selected, but this pane is too narrow; showing unified"
                : "Side-by-side diff"
            }
            className={`px-2 py-0.5 text-[11px] font-mono cursor-pointer transition-colors ${
              settings.diffViewLayout === "split"
                ? splitActive
                  ? "bg-brand-600 text-white"
                  : "bg-brand-600/40 text-white/80"
                : "text-text-dim hover:text-text-secondary"
            }`}
          >
            Split
          </button>
        </div>
      </div>

      {/* Diff content */}
      <div ref={measureRef} className="flex-1 overflow-auto">
        {diff.is_binary ? (
          <div className="flex items-center justify-center h-full text-text-dim">
            <span className="text-sm">Binary file changed</span>
          </div>
        ) : diff.truncated ? (
          <div className="flex items-center justify-center h-full text-text-dim">
            <div className="text-center px-4">
              <p className="text-sm mb-1">File too large to diff inline</p>
              <p className="text-xs">
                Open it in your editor to review the changes.
              </p>
            </div>
          </div>
        ) : diff.hunks.length === 0 && staleComments.length === 0 ? (
          <div className="flex items-center justify-center h-full text-text-dim">
            <span className="text-sm">No changes in this file</span>
          </div>
        ) : (
          <div className="leading-[1.6]">
            {staleComments.length > 0 && (
              <div className="px-3 py-2 bg-status-error/5 border-b border-status-error/30">
                <div className="text-[11px] font-mono text-status-error mb-2">
                  {staleComments.length} stale comment
                  {staleComments.length === 1 ? "" : "s"} (line range no longer
                  in current diff)
                </div>
                {staleComments.map((anchored) => (
                  <CommentCard
                    key={`stale-${anchored.comment.id}`}
                    anchored={anchored}
                    onSave={handleCommentSave}
                    onDelete={handleCommentDelete}
                  />
                ))}
              </div>
            )}
            {diff.hunks.length === 0 && (
              <div className="px-3 py-4 text-center text-text-dim text-sm">
                No changes in this file
              </div>
            )}
            {diff.hunks.map((hunk, hi) =>
              splitActive ? (
                <SplitHunkView
                  key={`${hunk.old_start}-${hunk.new_start}`}
                  hunk={hunk}
                  hunkIndex={hi}
                  lineTokens={tokenGrid?.[hi]}
                  commentsEnabled={commentsEnabled && !!commentsStore}
                  cardsByEndRow={cardsByHunkRow.get(hi) ?? EMPTY_MAP}
                  formRowIndex={draft?.hunkIndex === hi ? draft.endRowIndex : null}
                  draftSide={draft?.hunkIndex === hi ? draft.side : null}
                  draftStartLine={draft?.hunkIndex === hi ? draft.startLine : null}
                  draftEndLine={draft?.hunkIndex === hi ? draft.endLine : null}
                  rangeStart={
                    rangeStart?.hunkIndex === hi
                      ? { side: rangeStart.side, line: rangeStart.line }
                      : null
                  }
                  onPlusClick={handlePlusClick}
                  onCommentSave={handleCommentSave}
                  onCommentDelete={handleCommentDelete}
                  onDraftSave={handleDraftSave}
                  onDraftCancel={handleDraftCancel}
                />
              ) : (
                <HunkView
                  key={`${hunk.old_start}-${hunk.new_start}`}
                  hunk={hunk}
                  hunkIndex={hi}
                  lineTokens={tokenGrid?.[hi]}
                  rangeStart={rangeStart}
                  draft={draft?.hunkIndex === hi ? draft : null}
                  cardsByEndRow={cardsByHunkRow.get(hi) ?? EMPTY_MAP}
                  formRowIndex={
                    draft?.hunkIndex === hi ? draft.endRowIndex : null
                  }
                  commentsEnabled={commentsEnabled && !!commentsStore}
                  onPlusClick={handlePlusClick}
                  onCommentSave={handleCommentSave}
                  onCommentDelete={handleCommentDelete}
                  onDraftSave={handleDraftSave}
                  onDraftCancel={handleDraftCancel}
                />
              ),
            )}
          </div>
        )}
      </div>
    </div>
  );
}

const EMPTY_MAP: Map<number, AnchoredComment[]> = new Map();
