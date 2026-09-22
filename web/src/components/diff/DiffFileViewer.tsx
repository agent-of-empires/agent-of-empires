import { useCallback, useMemo, useRef, useState, type ReactNode } from "react";
import { FileDiff, Virtualizer } from "@pierre/diffs/react";
import { processFile } from "@pierre/diffs";
import type { DiffLineAnnotation, FileContents, FileDiffOptions, SelectedLineRange } from "@pierre/diffs";
import { useFileContents } from "../../hooks/useFileContents";
import { useWebSettings } from "../../hooks/useWebSettings";
import { useShikiTheme } from "../../hooks/useShikiTheme";
import type { UseDiffCommentsResult } from "../../hooks/useDiffComments";
import { anchorCommentsToContents } from "./comments/anchorToContents";
import { extractSnippetFromContents } from "./comments/extractSnippetFromContents";
import { extensionToLanguage } from "./comments/language";
import { CommentCard } from "./comments/CommentCard";
import { CommentForm } from "./comments/CommentForm";
import type { AnchoredComment, DiffSide } from "./comments/types";
import { DiffWorkerPoolProvider } from "./pierre/DiffWorkerPoolProvider";
import { DiffViewerHeader } from "./DiffViewerHeader";
import { FullFileViewer } from "./FullFileViewer";
import { MarkdownFileView } from "./MarkdownFileView";
import { FindBar } from "./find/FindBar";
import { changedLines } from "./find/changedLines";
import type { FindMatch } from "./find/findMatches";
import { targetScrollFraction } from "./scrollFraction";
import { useDiffScrollHold } from "./useDiffScrollHold";
import { Centered, TooLarge } from "./viewerChrome";

interface Props {
  sessionId: string;
  filePath: string;
  /** Workspace repo the file belongs to. */
  repoName?: string;
  /** 1-based new-side line to scroll to and highlight, from a transcript `path:line` link. */
  targetLine?: number;
  /** Triggers a re-fetch when the file list changes. */
  revision?: number;
  onClose?: () => void;
  /** Enables line selection comments; requires `commentsStore`. */
  commentsEnabled?: boolean;
  commentsStore?: UseDiffCommentsResult;
}

interface DraftRange {
  side: DiffSide;
  startLine: number;
  endLine: number;
  snippet: string;
}

type AnnotationMeta = { kind: "card"; anchored: AnchoredComment } | { kind: "form"; draft: DraftRange };

const sideToAnnotation = (side: DiffSide) => (side === "old" ? ("deletions" as const) : ("additions" as const));
const lineRange = (line: number, side: "deletions" | "additions"): SelectedLineRange => ({
  start: line,
  end: line,
  side,
  endSide: side,
});

export function DiffFileViewer({
  sessionId,
  filePath,
  repoName,
  targetLine,
  revision,
  onClose,
  commentsEnabled = false,
  commentsStore,
}: Props) {
  const { contents, loading, error } = useFileContents(sessionId, filePath, repoName, revision);
  const { theme } = useShikiTheme();
  const { settings } = useWebSettings();

  const [isWide, setIsWide] = useState(true);
  const widthObserverRef = useRef<ResizeObserver | null>(null);
  const measureRef = useCallback((el: HTMLDivElement | null) => {
    widthObserverRef.current?.disconnect();
    widthObserverRef.current = null;
    if (!el || typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver((entries) => setIsWide((entries[0]?.contentRect.width ?? 0) >= 640));
    ro.observe(el);
    widthObserverRef.current = ro;
  }, []);
  const splitActive = settings.diffViewLayout === "split" && isWide;

  const [draft, setDraft] = useState<DraftRange | null>(null);
  const [selected, setSelected] = useState<SelectedLineRange | null>(null);
  const [findOpen, setFindOpen] = useState(false);

  // Reset transient state on a file or target change, during render; a cited line starts selected.
  const syncKey = JSON.stringify([sessionId, repoName ?? null, filePath, revision, targetLine ?? null]);
  const [handledSyncKey, setHandledSyncKey] = useState(syncKey);
  if (syncKey !== handledSyncKey) {
    setHandledSyncKey(syncKey);
    setDraft(null);
    setSelected(targetLine != null ? lineRange(targetLine, "additions") : null);
    setFindOpen(false);
  }

  const oldContent = contents?.old_content ?? "";
  const newContent = contents?.new_content ?? "";
  const patch = contents?.patch ?? "";
  const resolvedPath = contents?.file.path ?? filePath;
  const oldPath = contents?.file.old_path ?? resolvedPath;

  const markdownAvailable =
    extensionToLanguage(resolvedPath) === "markdown" && !contents?.is_binary && !contents?.truncated;
  const showRendered = markdownAvailable && settings.markdownPreview === "rendered";

  const commentsActive = commentsEnabled && !!commentsStore;
  const comments = useMemo(() => commentsStore?.comments ?? [], [commentsStore]);
  const anchored = useMemo(
    () => anchorCommentsToContents(comments, filePath, repoName, oldContent, newContent),
    [comments, filePath, repoName, oldContent, newContent],
  );
  const staleComments = useMemo(() => anchored.filter((a) => a.status === "stale"), [anchored]);

  const oldFile = useMemo<FileContents>(() => ({ name: oldPath, contents: oldContent }), [oldPath, oldContent]);
  const newFile = useMemo<FileContents>(
    () => ({ name: resolvedPath, contents: newContent }),
    [resolvedPath, newContent],
  );

  // Also keys the Virtualizer: it caches row measurements and would keep painting the old file.
  const viewKey = `${repoName ?? ""}:${resolvedPath}:${revision ?? 0}`;

  // Parses the server patch only; highlighting runs in the worker pool.
  const fileDiff = useMemo(
    () => (patch ? processFile(patch, { oldFile, newFile, cacheKey: viewKey }) : undefined),
    [patch, oldFile, newFile, viewKey],
  );

  const lineAnnotations = useMemo<DiffLineAnnotation<AnnotationMeta>[]>(() => {
    const out: DiffLineAnnotation<AnnotationMeta>[] = anchored
      .filter((a) => a.status === "active")
      .map((a) => ({
        side: sideToAnnotation(a.comment.side),
        lineNumber: a.comment.endLine,
        metadata: { kind: "card", anchored: a },
      }));
    if (draft) {
      out.push({ side: sideToAnnotation(draft.side), lineNumber: draft.endLine, metadata: { kind: "form", draft } });
    }
    return out;
  }, [anchored, draft]);

  const handleSave = useCallback((id: string, body: string) => commentsStore?.updateComment(id, body), [commentsStore]);
  const handleDelete = useCallback((id: string) => commentsStore?.deleteComment(id), [commentsStore]);
  const clearDraft = useCallback(() => {
    setDraft(null);
    setSelected(null);
  }, []);
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
      clearDraft();
    },
    [draft, commentsStore, repoName, filePath, clearDraft],
  );

  const renderAnnotation = useCallback(
    ({ metadata: meta }: DiffLineAnnotation<AnnotationMeta>) =>
      meta.kind === "form" ? (
        <CommentForm
          startLine={meta.draft.startLine}
          endLine={meta.draft.endLine}
          side={meta.draft.side}
          onSave={handleDraftSave}
          onCancel={clearDraft}
        />
      ) : (
        <CommentCard anchored={meta.anchored} onSave={handleSave} onDelete={handleDelete} />
      ),
    [handleDraftSave, clearDraft, handleSave, handleDelete],
  );

  const handleLineSelected = useCallback(
    (range: SelectedLineRange | null) => {
      setSelected(range);
      if (!commentsActive || !range) return;
      const side: DiffSide = range.side === "deletions" ? "old" : "new";
      const startLine = Math.min(range.start, range.end);
      const endLine = Math.max(range.start, range.end);
      const snippet = extractSnippetFromContents(oldContent, newContent, side, startLine, endLine);
      if (snippet != null) setDraft({ side, startLine, endLine, snippet });
    },
    [commentsActive, oldContent, newContent],
  );

  const options = useMemo<FileDiffOptions<AnnotationMeta, undefined>>(
    () => ({
      diffStyle: splitActive ? "split" : "unified",
      theme,
      disableFileHeader: true,
      // Selection also renders the find match and a cited line's highlight.
      enableLineSelection: commentsActive || findOpen || targetLine != null,
      controlledSelection: true,
      onLineSelectionChange: setSelected,
      onLineSelected: handleLineSelected,
    }),
    [splitActive, theme, commentsActive, findOpen, targetLine, handleLineSelected],
  );

  const findLines = useMemo(() => (findOpen && fileDiff ? changedLines(fileDiff) : []), [findOpen, fileDiff]);

  const { wrapRef, scrollerRef, userScrolledRef } = useDiffScrollHold(
    () =>
      targetLine != null && fileDiff ? targetScrollFraction(fileDiff, targetLine, newContent.split("\n").length) : null,
    [resolvedPath, repoName, splitActive, oldContent, newContent, fileDiff, targetLine],
  );

  const handleFindJump = useCallback(
    (match: FindMatch | null) => {
      if (!match) return;
      setSelected(lineRange(match.lineNumber, match.side === "old" ? "deletions" : "additions"));
      // No scroll-to-line API and the row is likely unmounted: scroll to its line fraction.
      const scroller = scrollerRef.current;
      if (scroller) {
        const lineCount = (match.side === "old" ? oldContent : newContent).split("\n").length;
        const frac = Math.min(1, Math.max(0, (match.lineNumber - 1) / lineCount));
        userScrolledRef.current = true;
        scroller.scrollTop = frac * (scroller.scrollHeight - scroller.clientHeight);
      }
    },
    [oldContent, newContent, scrollerRef, userScrolledRef],
  );

  const onKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      // Rendered Markdown has no FindBar, so keep the browser's native find.
      if (!showRendered && (e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "f") {
        e.preventDefault();
        setFindOpen(true);
      }
    },
    [showRendered],
  );

  if (loading && !contents) {
    return (
      <Centered className="bg-surface-900 text-text-dim">
        <span className="text-sm">Loading diff...</span>
      </Centered>
    );
  }
  if (error) {
    return (
      <Centered className="bg-surface-900 text-status-error">
        <span className="text-sm">{error}</span>
      </Centered>
    );
  }
  if (!contents) {
    return (
      <Centered className="bg-surface-900 text-text-dim">
        <span className="text-sm">Select a file to view changes</span>
      </Centered>
    );
  }

  // An agent-cited file with no diff arrives whole in new_content with an empty patch.
  const isFullFile = contents.file.status === "unchanged";

  let body: ReactNode;
  if (showRendered) {
    body = <MarkdownFileView content={contents.file.status === "deleted" ? oldContent : newContent} />;
  } else if (contents.is_binary) {
    body = (
      <Centered>
        <span className="text-sm">{isFullFile ? "Binary file" : "Binary file changed"}</span>
      </Centered>
    );
  } else if (contents.truncated) {
    body = <TooLarge what="File too large to diff inline" hint="Open it in your editor to review the changes." />;
  } else if (isFullFile) {
    body = <FullFileViewer content={newContent} filePath={resolvedPath} />;
  } else if (oldContent === newContent && staleComments.length === 0) {
    body = (
      <Centered>
        <span className="text-sm">No changes in this file</span>
      </Centered>
    );
  } else {
    body = (
      <>
        {staleComments.length > 0 && (
          <div className="px-3 py-2 bg-status-error/5 border-b border-status-error/30 shrink-0 overflow-auto max-h-48">
            <div className="text-[11px] font-mono text-status-error mb-2">
              {staleComments.length} stale comment
              {staleComments.length === 1 ? "" : "s"} (line range no longer in current diff)
            </div>
            {staleComments.map((a) => (
              <CommentCard key={`stale-${a.comment.id}`} anchored={a} onSave={handleSave} onDelete={handleDelete} />
            ))}
          </div>
        )}
        <div ref={wrapRef} className="flex-1 min-h-0 flex flex-col">
          <DiffWorkerPoolProvider>
            <Virtualizer key={viewKey} className="flex-1 overflow-auto">
              {fileDiff && (
                <FileDiff<AnnotationMeta>
                  fileDiff={fileDiff}
                  options={options}
                  lineAnnotations={lineAnnotations}
                  selectedLines={selected}
                  renderAnnotation={renderAnnotation}
                />
              )}
            </Virtualizer>
          </DiffWorkerPoolProvider>
        </div>
      </>
    );
  }

  return (
    <div className="flex-1 flex flex-col bg-surface-900 overflow-hidden" onKeyDown={onKeyDown}>
      <DiffViewerHeader
        file={contents.file}
        onClose={onClose}
        markdownAvailable={markdownAvailable}
        showRendered={showRendered}
        findOpen={findOpen}
        onToggleFind={() => setFindOpen((v) => !v)}
        isWide={isWide}
        splitActive={splitActive}
      />

      {findOpen && !showRendered && !contents.is_binary && !contents.truncated && (
        <FindBar lines={findLines} onJump={handleFindJump} onClose={() => setFindOpen(false)} />
      )}

      <div ref={measureRef} className="relative flex-1 overflow-hidden flex flex-col">
        {/* A switch keeps the previous diff painted under a light scrim. */}
        {loading && (
          <div className="animate-fade-in absolute inset-0 z-10 flex items-center justify-center bg-surface-900/25 pointer-events-none">
            <span className="text-xs text-text-dim bg-surface-900/80 rounded px-2 py-1">Loading diff...</span>
          </div>
        )}
        {body}
      </div>
    </div>
  );
}
