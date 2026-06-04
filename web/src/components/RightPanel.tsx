import { useCallback, useEffect, useRef, useState } from "react";
import { DiffFileList } from "./diff/DiffFileList";
import { CommentsBanner } from "./diff/comments/CommentsBanner";
import { PairedShellPane } from "./PairedTerminal";
import { safeGetItem, safeSetItem } from "../lib/safeStorage";
import type { RepoBase, RichDiffFile, SessionResponse } from "../lib/types";
import { TOUR_ANCHORS, tourAnchor } from "../lib/tourSteps";

const VSPLIT_STORAGE_KEY = "aoe-right-vsplit";
const DEFAULT_TOP_RATIO = 0.5;
const MIN_TOP_PX = 80;
const MIN_BOTTOM_PX = 120;

function loadSavedRatio(): number {
  const saved = safeGetItem(VSPLIT_STORAGE_KEY);
  if (saved) {
    const r = parseFloat(saved);
    if (r > 0 && r < 1) return r;
  }
  return DEFAULT_TOP_RATIO;
}

interface Props {
  session: SessionResponse | null;
  sessionId: string | null;
  files: RichDiffFile[];
  perRepoBases: RepoBase[];
  warning: string | null;
  filesLoading: boolean;
  selectedFilePath: string | null;
  selectedRepoName: string | undefined;
  onSelectFile: (path: string, repoName?: string) => void;
  /** Re-fetch the diff. Called after the user changes the per-session
   *  base-branch override so the file list reflects the new comparison. */
  onDiffRefresh: () => void;
  /** Diff-comments banner state (#928). Hidden on tmux sessions. */
  commentsEnabled: boolean;
  commentsCount: number;
  commentsSendEnabled: boolean;
  commentsSendDisabledReason?: string;
  onOpenSendDialog: () => void;
  onDiscardAllComments: () => void;
}

export function RightPanel({
  session,
  sessionId,
  files,
  perRepoBases,
  warning,
  filesLoading,
  selectedFilePath,
  selectedRepoName,
  onSelectFile,
  onDiffRefresh,
  commentsEnabled,
  commentsCount,
  commentsSendEnabled,
  commentsSendDisabledReason,
  onOpenSendDialog,
  onDiscardAllComments,
}: Props) {
  const [topRatio, setTopRatio] = useState(loadSavedRatio);
  const containerRef = useRef<HTMLDivElement>(null);
  const dragging = useRef(false);

  const handleMouseDown = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    dragging.current = true;
    document.body.style.cursor = "row-resize";
    document.body.style.userSelect = "none";
  }, []);

  const handleTouchStart = useCallback((e: React.TouchEvent) => {
    e.preventDefault();
    dragging.current = true;
    document.body.style.userSelect = "none";
  }, []);

  useEffect(() => {
    const applyY = (clientY: number) => {
      if (!containerRef.current) return;
      const rect = containerRef.current.getBoundingClientRect();
      const y = clientY - rect.top;
      if (y < MIN_TOP_PX || rect.height - y < MIN_BOTTOM_PX) return;
      setTopRatio(y / rect.height);
    };
    const persistAndSettle = () => {
      if (!dragging.current) return;
      dragging.current = false;
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
      setTopRatio((r) => {
        safeSetItem(VSPLIT_STORAGE_KEY, String(r));
        return r;
      });
      window.dispatchEvent(new Event("resize"));
    };

    const handleMouseMove = (e: MouseEvent) => {
      if (!dragging.current) return;
      applyY(e.clientY);
    };
    const handleMouseUp = () => persistAndSettle();

    const handleTouchMove = (e: TouchEvent) => {
      if (!dragging.current) return;
      const t = e.touches[0];
      if (!t) return;
      e.preventDefault();
      applyY(t.clientY);
    };
    const handleTouchEnd = () => persistAndSettle();

    document.addEventListener("mousemove", handleMouseMove);
    document.addEventListener("mouseup", handleMouseUp);
    document.addEventListener("touchmove", handleTouchMove, { passive: false });
    document.addEventListener("touchend", handleTouchEnd);
    document.addEventListener("touchcancel", handleTouchEnd);
    return () => {
      document.removeEventListener("mousemove", handleMouseMove);
      document.removeEventListener("mouseup", handleMouseUp);
      document.removeEventListener("touchmove", handleTouchMove);
      document.removeEventListener("touchend", handleTouchEnd);
      document.removeEventListener("touchcancel", handleTouchEnd);
      // If component unmounts mid-drag, reset body styles so the cursor
      // doesn't stay in row-resize / text-select-disabled state.
      if (dragging.current) {
        dragging.current = false;
        document.body.style.cursor = "";
        document.body.style.userSelect = "";
      }
    };
  }, []);

  return (
    <div ref={containerRef} {...tourAnchor(TOUR_ANCHORS.rightPanel)} className="flex-1 flex flex-col min-h-0 overflow-hidden md:bg-surface-800 md:pb-1.5">
      {/* Upper: file list */}
      <div
        style={{ flexBasis: `${topRatio * 100}%` }}
        className="flex flex-col min-h-0 overflow-hidden"
      >
        {commentsEnabled && commentsCount > 0 && (
          <CommentsBanner
            count={commentsCount}
            sendEnabled={commentsSendEnabled}
            sendDisabledReason={commentsSendDisabledReason}
            onSend={onOpenSendDialog}
            onDiscardAll={onDiscardAllComments}
          />
        )}
        <DiffFileList
          files={files}
          perRepoBases={perRepoBases}
          warning={warning}
          selectedPath={selectedFilePath}
          selectedRepoName={selectedRepoName}
          loading={filesLoading}
          onSelectFile={onSelectFile}
          sessionId={sessionId}
          repoPath={session?.main_repo_path ?? session?.project_path ?? null}
          baseBranchOverride={session?.base_branch_override ?? null}
          onBaseBranchChanged={onDiffRefresh}
        />
      </div>

      {/* Drag handle: taller on mobile for easier touch targeting */}
      <div
        onMouseDown={handleMouseDown}
        onTouchStart={handleTouchStart}
        className="h-3 md:h-1 cursor-row-resize shrink-0 bg-surface-700/20 hover:bg-brand-600/50 transition-colors duration-75 touch-none flex items-center justify-center"
      >
        <div className="w-8 h-0.5 rounded-full bg-surface-500/40 md:hidden" />
      </div>

      {/* Lower: paired terminal */}
      <div
        style={{ flexBasis: `${(1 - topRatio) * 100}%` }}
        className="flex flex-col min-h-0">
        <PairedShellPane session={session} sessionId={sessionId} />
      </div>
    </div>
  );
}
