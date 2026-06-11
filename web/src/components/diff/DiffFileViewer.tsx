import { useCallback, useEffect, useMemo, useRef, useState } from "react";
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
import { FindBar } from "./find/FindBar";
import { changedLines } from "./find/changedLines";
import type { FindMatch } from "./find/findMatches";

interface Props {
  sessionId: string;
  filePath: string;
  /** Workspace repo name; passed through to the diff endpoint so the file is
   *  resolved against the correct repo for multi-repo workspaces. See #1047. */
  repoName?: string;
  /** Triggers a re-fetch when the file list changes. */
  revision?: number;
  /** Called when the user wants to return to the terminal view. */
  onClose?: () => void;
  /** When true, the in-diff comment UI (line selection, inline cards/forms,
   *  stale block) is enabled. False for non-structured-view sessions. */
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

/** Transient draft for an in-progress comment range. */
interface DraftRange {
  side: DiffSide;
  startLine: number;
  endLine: number;
  snippet: string;
}

/** Metadata carried on each Pierre line annotation so `renderAnnotation`
 *  knows whether to draw a saved card or the active draft form. */
type AnnotationMeta = { kind: "card"; anchored: AnchoredComment } | { kind: "form"; draft: DraftRange };

const sideToAnnotation = (side: DiffSide) => (side === "old" ? ("deletions" as const) : ("additions" as const));
const annotationToSide = (side: "deletions" | "additions"): DiffSide => (side === "deletions" ? "old" : "new");

export function DiffFileViewer({
  sessionId,
  filePath,
  repoName,
  revision,
  onClose,
  commentsEnabled = false,
  commentsStore,
}: Props) {
  const { contents, loading, error } = useFileContents(sessionId, filePath, repoName, revision);
  const { theme } = useShikiTheme();
  const { settings, update } = useWebSettings();

  const [isWide, setIsWide] = useState(true);
  const widthObserverRef = useRef<ResizeObserver | null>(null);
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

  const [draft, setDraft] = useState<DraftRange | null>(null);
  const [selected, setSelected] = useState<SelectedLineRange | null>(null);
  const [findOpen, setFindOpen] = useState(false);
  const scrollResetRef = useRef<HTMLDivElement | null>(null);
  const scrollerRef = useRef<HTMLElement | null>(null);
  const userScrolledRef = useRef(false);

  // Reset transient state when the viewer switches files / repos / sessions.
  // Synced at render time (not in an effect) to avoid set-state-in-effect.
  const syncKey = JSON.stringify([sessionId, repoName ?? null, filePath, revision]);
  const [handledSyncKey, setHandledSyncKey] = useState(syncKey);
  if (syncKey !== handledSyncKey) {
    setHandledSyncKey(syncKey);
    setDraft(null);
    setSelected(null);
    setFindOpen(false);
  }

  const oldContent = contents?.old_content ?? "";
  const newContent = contents?.new_content ?? "";
  const patch = contents?.patch ?? "";
  const resolvedPath = contents?.file.path ?? filePath;
  const oldPath = contents?.file.old_path ?? resolvedPath;

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

  // Identity of the currently rendered file + revision. Drives the Pierre
  // parse cache and (below) remounts the Virtualizer on a file switch: the
  // virtualizer caches row measurements internally and does not reset them
  // when its children change, so without a fresh mount it can keep painting
  // the previously opened file's rows even after the contents update.
  const viewKey = `${repoName ?? ""}:${resolvedPath}:${revision ?? 0}`;

  // Parse the server-computed patch into Pierre's diff metadata. Plain text
  // parsing; no diff algorithm runs in the browser, so even huge generated
  // files don't block the main thread. The raw old/new contents are grafted
  // on so hunk expansion still works; highlighting happens in the worker pool.
  const fileDiff = useMemo(() => {
    if (!patch) return undefined;
    return processFile(patch, {
      oldFile,
      newFile,
      cacheKey: viewKey,
    });
  }, [patch, oldFile, newFile, viewKey]);

  const lineAnnotations = useMemo<DiffLineAnnotation<AnnotationMeta>[]>(() => {
    const out: DiffLineAnnotation<AnnotationMeta>[] = [];
    for (const a of anchored) {
      if (a.status !== "active") continue;
      out.push({
        side: sideToAnnotation(a.comment.side),
        lineNumber: a.comment.endLine,
        metadata: { kind: "card", anchored: a },
      });
    }
    if (draft) {
      out.push({
        side: sideToAnnotation(draft.side),
        lineNumber: draft.endLine,
        metadata: { kind: "form", draft },
      });
    }
    return out;
  }, [anchored, draft]);

  const handleSave = useCallback((id: string, body: string) => commentsStore?.updateComment(id, body), [commentsStore]);
  const handleDelete = useCallback((id: string) => commentsStore?.deleteComment(id), [commentsStore]);
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
      setSelected(null);
    },
    [draft, commentsStore, repoName, filePath],
  );
  const handleDraftCancel = useCallback(() => {
    setDraft(null);
    setSelected(null);
  }, []);

  const renderAnnotation = useCallback(
    (annotation: DiffLineAnnotation<AnnotationMeta>) => {
      const meta = annotation.metadata;
      if (meta.kind === "form") {
        return (
          <CommentForm
            startLine={meta.draft.startLine}
            endLine={meta.draft.endLine}
            side={meta.draft.side}
            onSave={handleDraftSave}
            onCancel={handleDraftCancel}
          />
        );
      }
      return <CommentCard anchored={meta.anchored} onSave={handleSave} onDelete={handleDelete} />;
    },
    [handleDraftSave, handleDraftCancel, handleSave, handleDelete],
  );

  const handleLineSelected = useCallback(
    (range: SelectedLineRange | null) => {
      setSelected(range);
      if (!commentsActive || !range) return;
      const side = annotationToSide(range.side ?? "additions");
      const startLine = Math.min(range.start, range.end);
      const endLine = Math.max(range.start, range.end);
      const snippet = extractSnippetFromContents(oldContent, newContent, side, startLine, endLine);
      if (snippet == null) return;
      setDraft({ side, startLine, endLine, snippet });
    },
    [commentsActive, oldContent, newContent],
  );

  const options = useMemo<FileDiffOptions<AnnotationMeta>>(
    () => ({
      diffStyle: splitActive ? "split" : "unified",
      theme,
      disableFileHeader: true,
      // Enable selection for commenting, and also while find is open so the
      // jumped-to match line renders its selection highlight.
      enableLineSelection: commentsActive || findOpen,
      controlledSelection: true,
      onLineSelectionChange: setSelected,
      onLineSelected: handleLineSelected,
    }),
    [splitActive, theme, commentsActive, findOpen, handleLineSelected],
  );

  // Searchable line set for find: the diff's changed lines, read straight off
  // the already-parsed patch metadata (no extra work beyond walking hunks).
  const findLines = useMemo(() => (findOpen && fileDiff ? changedLines(fileDiff) : []), [findOpen, fileDiff]);

  const handleFindJump = useCallback(
    (match: FindMatch | null) => {
      if (!match) return;
      const side = match.side === "old" ? "deletions" : "additions";
      setSelected({
        start: match.lineNumber,
        end: match.lineNumber,
        side,
        endSide: side,
      });
      // The Virtualizer has no scroll-to-line API and the target row is likely
      // unmounted, so approximate its position (line fraction of the file) and
      // scroll there; the renderer then mounts the row and `selectedLines`
      // highlights it. Mark this as a user scroll so the keep-at-top reset
      // observer backs off.
      const scroller = scrollerRef.current;
      if (scroller) {
        const text = match.side === "old" ? oldContent : newContent;
        const lineCount = text.split("\n").length;
        const frac = Math.min(1, Math.max(0, (match.lineNumber - 1) / lineCount));
        userScrolledRef.current = true;
        scroller.scrollTop = frac * (scroller.scrollHeight - scroller.clientHeight);
      }
    },
    [oldContent, newContent],
  );

  const onKeyDown = useCallback((e: React.KeyboardEvent) => {
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "f") {
      e.preventDefault();
      setFindOpen(true);
    }
  }, []);

  // Keep the diff scrolled to the top when a file first opens. The
  // virtualized renderer reconciles row heights asynchronously (and again
  // when off-thread highlighting lands), which otherwise settles the scroll
  // position at the bottom of large diffs. We force the top across those
  // reflows until the user scrolls, then get out of the way.
  useEffect(() => {
    const wrap = scrollResetRef.current;
    if (!wrap) return;
    const scroller = wrap.querySelector<HTMLElement>(".overflow-auto");
    const content = scroller?.firstElementChild;
    if (!scroller || !content) return;
    scrollerRef.current = scroller;
    userScrolledRef.current = false;
    const markUser = () => {
      userScrolledRef.current = true;
    };
    scroller.addEventListener("wheel", markUser, { passive: true });
    scroller.addEventListener("pointerdown", markUser, { passive: true });
    scroller.addEventListener("keydown", markUser);
    scroller.scrollTop = 0;
    const ro = new ResizeObserver(() => {
      if (!userScrolledRef.current) scroller.scrollTop = 0;
    });
    ro.observe(content);
    return () => {
      ro.disconnect();
      scroller.removeEventListener("wheel", markUser);
      scroller.removeEventListener("pointerdown", markUser);
      scroller.removeEventListener("keydown", markUser);
      if (scrollerRef.current === scroller) scrollerRef.current = null;
    };
  }, [resolvedPath, repoName, splitActive, oldContent, newContent]);

  if (loading && !contents) {
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
  if (!contents) {
    return (
      <div className="flex-1 flex items-center justify-center bg-surface-900 text-text-dim">
        <span className="text-sm">Select a file to view changes</span>
      </div>
    );
  }

  const statusColor = STATUS_COLORS[contents.file.status] ?? "text-text-muted";
  const statusLabel = STATUS_LABELS[contents.file.status] ?? contents.file.status;
  const noChanges = oldContent === newContent;

  return (
    <div className="flex-1 flex flex-col bg-surface-900 overflow-hidden" onKeyDown={onKeyDown}>
      {/* File header */}
      <div className="px-3 py-2 border-b border-surface-700/20 flex items-center gap-2 shrink-0 flex-wrap">
        {onClose && (
          <button
            onClick={onClose}
            className="text-text-dim hover:text-text-secondary cursor-pointer transition-colors flex items-center gap-1 text-[11px]"
            title="Back to terminal"
            aria-label="Back to terminal"
          >
            <svg
              width="14"
              height="14"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.75"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <path d="M15 18l-6-6 6-6" />
            </svg>
            <span className="hidden sm:inline">Terminal</span>
          </button>
        )}
        <span className={`font-mono text-[11px] font-semibold ${statusColor}`}>{statusLabel}</span>
        <span className="font-mono text-[12px] text-text-primary truncate">
          {contents.file.old_path ? `${contents.file.old_path} → ${contents.file.path}` : contents.file.path}
        </span>
        <span className="font-mono text-[11px] flex items-center gap-1">
          {contents.file.additions > 0 && <span className="text-status-running">+{contents.file.additions}</span>}
          {contents.file.deletions > 0 && <span className="text-status-error">-{contents.file.deletions}</span>}
        </span>
        <div className="ml-auto flex items-center gap-2">
          <button
            type="button"
            onClick={() => setFindOpen((v) => !v)}
            aria-pressed={findOpen}
            title="Find in diff (Cmd/Ctrl+F)"
            aria-label="Find in diff"
            className={`px-2 py-0.5 text-[11px] font-mono rounded cursor-pointer transition-colors ${
              findOpen ? "bg-brand-600 text-white" : "text-text-dim hover:text-text-secondary"
            }`}
          >
            Find
          </button>
          <div className="flex items-center rounded border border-surface-700/40 overflow-hidden">
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
      </div>

      {findOpen && !contents.is_binary && !contents.truncated && (
        <FindBar lines={findLines} onJump={handleFindJump} onClose={() => setFindOpen(false)} />
      )}

      {/* Diff content */}
      <div ref={measureRef} className="relative flex-1 overflow-hidden flex flex-col">
        {/* While switching to an uncached file we keep the previous diff
            painted and lay a light scrim over it (the full loading screen is
            only used on the very first load, handled above). */}
        {loading && (
          <div className="animate-fade-in absolute inset-0 z-10 flex items-center justify-center bg-surface-900/25 pointer-events-none">
            <span className="text-xs text-text-dim bg-surface-900/80 rounded px-2 py-1">Loading diff...</span>
          </div>
        )}
        {contents.is_binary ? (
          <div className="flex-1 flex items-center justify-center text-text-dim">
            <span className="text-sm">Binary file changed</span>
          </div>
        ) : contents.truncated ? (
          <div className="flex-1 flex items-center justify-center text-text-dim">
            <div className="text-center px-4">
              <p className="text-sm mb-1">File too large to diff inline</p>
              <p className="text-xs">Open it in your editor to review the changes.</p>
            </div>
          </div>
        ) : noChanges && staleComments.length === 0 ? (
          <div className="flex-1 flex items-center justify-center text-text-dim">
            <span className="text-sm">No changes in this file</span>
          </div>
        ) : (
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
            <div ref={scrollResetRef} className="flex-1 min-h-0 flex flex-col">
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
        )}
      </div>
    </div>
  );
}
