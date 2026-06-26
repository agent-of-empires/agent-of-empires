import { DiffFileList } from "./diff/DiffFileList";
import { CommentsBanner } from "./diff/comments/CommentsBanner";
import { PluginDetailBadges } from "./plugin/PluginSlots";
import type { RepoBase, RichDiffFile, SessionResponse } from "../lib/types";

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
  onDiffRefresh: () => void;
  commentsEnabled: boolean;
  commentsCount: number;
  commentsSendEnabled: boolean;
  commentsSendDisabledReason?: string;
  onOpenSendDialog: () => void;
  onDiscardAllComments: () => void;
}

/** Body of the "diff" pane: comments banner, per-session plugin detail
 *  badges/panels, and the changed-file list. Pure content; the dock supplies
 *  the frame, resize, and dock-location chrome. */
export function DiffPane({
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
  return (
    <div className="flex flex-col min-h-0 flex-1 overflow-hidden">
      {commentsEnabled && commentsCount > 0 && (
        <CommentsBanner
          count={commentsCount}
          sendEnabled={commentsSendEnabled}
          sendDisabledReason={commentsSendDisabledReason}
          onSend={onOpenSendDialog}
          onDiscardAll={onDiscardAllComments}
        />
      )}
      {sessionId && (
        <div className="shrink-0 flex flex-col gap-2 p-2 empty:hidden">
          <PluginDetailBadges sessionId={sessionId} />
        </div>
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
  );
}
