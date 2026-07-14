import { lazy, Suspense } from "react";

import { TerminalSessionStack } from "./TerminalSessionStack";
import { PairedShellPane } from "./PairedTerminal";
import { DiffFileList } from "./diff/DiffFileList";
import { DiffFileViewer } from "./diff/DiffFileViewer";
import { CommentsBanner } from "./diff/comments/CommentsBanner";
import { SendCommentsDialog } from "./diff/comments/SendCommentsDialog";
import { PluginPaneBody } from "./plugin/PluginSlots";
import type { RightPanelView } from "../lib/rightPanelView";
import { isPluginPaneId, type PluginPane } from "../lib/pluginPanes";
import type { RepoBase, RichDiffFile, SessionResponse } from "../lib/types";
import type { useDiffComments } from "../hooks/useDiffComments";
import type { FileRef } from "../lib/fileRef";

const StructuredView = lazy(() => import("./acp/StructuredView").then((m) => ({ default: m.StructuredView })));

interface Props {
  view: RightPanelView;
  pluginPanes: PluginPane[];
  onBackToAgent: () => void;
  pairedMounted: boolean;
  activeSession: SessionResponse | null;
  activeSessionId: string | null;
  sessions: SessionResponse[];
  webSettings: { persistentTerminals: boolean; maxPersistentTerminals: number };
  selectedFilePath: string | null;
  selectedRepoName: string | undefined;
  selectedFileLine: number | undefined;
  revision: number;
  diffFiles: RichDiffFile[];
  perRepoBases: RepoBase[];
  warning: string | null;
  diffFilesLoading: boolean;
  onSelectFile: (path: string, repoName?: string) => void;
  onOpenFileRef: (ref: FileRef) => void;
  onCloseFile: () => void;
  onDiffRefresh: () => void;
  commentsEnabled: boolean;
  commentSendEnabled: boolean;
  commentSendDisabledReason?: string;
  diffComments: ReturnType<typeof useDiffComments>;
  commentsIsMultiRepo: boolean;
  sendDialogOpen: boolean;
  onOpenSendDialog: () => void;
  onCloseSendDialog: () => void;
  onClearSelectedFile: () => void;
}

function layerClass(active: boolean): string {
  const base = "absolute inset-0 flex flex-col min-h-0 overflow-hidden";
  return active ? base : `${base} invisible pointer-events-none`;
}

/** The single full-viewport pane shown below the `md` breakpoint (#1452).
 *  The picker promotes one of agent / diff / paired into it. The agent
 *  terminal and the paired shell (once first opened) stay mounted but
 *  hidden via `visibility` so their PTY, scrollback, and focus survive
 *  view switches; `display:none` would collapse xterm geometry to zero. */
export function MobileMainPane({
  view,
  pluginPanes,
  onBackToAgent,
  pairedMounted,
  activeSession,
  activeSessionId,
  sessions,
  webSettings,
  selectedFilePath,
  selectedRepoName,
  selectedFileLine,
  revision,
  diffFiles,
  perRepoBases,
  warning,
  diffFilesLoading,
  onSelectFile,
  onOpenFileRef,
  onCloseFile,
  onDiffRefresh,
  commentsEnabled,
  commentSendEnabled,
  commentSendDisabledReason,
  diffComments,
  commentsIsMultiRepo,
  sendDialogOpen,
  onOpenSendDialog,
  onCloseSendDialog,
  onClearSelectedFile,
}: Props) {
  const activePluginPane = isPluginPaneId(view) ? (pluginPanes.find((p) => p.id === view) ?? null) : null;
  const viewLabel =
    view === "diff" ? "Diff" : view === "paired" ? "Paired terminal" : (activePluginPane?.title ?? "Plugin");

  return (
    <div className="flex-1 flex flex-col min-h-0">
      {view !== "agent" && (
        <div className="flex items-center gap-2 h-9 px-2 border-b border-surface-700/60 bg-surface-900 shrink-0">
          <button
            onClick={onBackToAgent}
            data-testid="mobile-back-to-agent"
            className="flex items-center gap-1 px-2 py-1 rounded-md text-xs text-text-secondary hover:text-text-primary hover:bg-surface-800 cursor-pointer transition-colors"
          >
            <span aria-hidden>&larr;</span> Agent
          </button>
          <span className="text-xs text-text-dim">{viewLabel}</span>
        </div>
      )}
      <div className="relative flex-1 flex flex-col min-h-0 overflow-hidden">
        <div className={layerClass(view === "agent")} inert={view !== "agent"}>
          {activeSession?.view === "structured" ? (
            <Suspense fallback={null}>
              <StructuredView
                key={activeSessionId}
                sessionId={activeSessionId!}
                acpWorkerState={activeSession.acp_worker_state ?? "absent"}
                tool={activeSession.tool}
                acpAgent={activeSession.acp_agent ?? null}
                archivedAt={activeSession.archived_at ?? null}
                snoozedUntil={activeSession.snoozed_until ?? null}
                trashedAt={activeSession.trashed_at ?? null}
                onOpenFileRef={onOpenFileRef}
                fileRefSession={activeSession}
              />
            </Suspense>
          ) : (
            <TerminalSessionStack
              activeSessionId={activeSessionId!}
              sessions={sessions.filter((session) => session.view !== "structured")}
              persistent={webSettings.persistentTerminals}
              maxPersistentTerminals={webSettings.maxPersistentTerminals}
            />
          )}
        </div>

        {pairedMounted && (
          <div className={layerClass(view === "paired")} inert={view !== "paired"}>
            <PairedShellPane session={activeSession} sessionId={activeSessionId} />
          </div>
        )}

        {view === "diff" && (
          <div className="absolute inset-0 z-10 flex flex-col min-h-0 overflow-hidden bg-surface-900">
            {selectedFilePath && activeSessionId ? (
              <DiffFileViewer
                sessionId={activeSessionId}
                filePath={selectedFilePath}
                repoName={selectedRepoName}
                targetLine={selectedFileLine}
                revision={revision}
                onClose={onCloseFile}
                commentsEnabled={commentsEnabled}
                commentsStore={diffComments}
              />
            ) : (
              <div className="flex-1 flex flex-col min-h-0 overflow-hidden">
                {commentsEnabled && diffComments.count > 0 && (
                  <CommentsBanner
                    count={diffComments.count}
                    sendEnabled={commentSendEnabled}
                    sendDisabledReason={commentSendDisabledReason}
                    onSend={onOpenSendDialog}
                    onDiscardAll={diffComments.clearComments}
                  />
                )}
                <DiffFileList
                  files={diffFiles}
                  perRepoBases={perRepoBases}
                  warning={warning}
                  selectedPath={selectedFilePath}
                  selectedRepoName={selectedRepoName}
                  loading={diffFilesLoading}
                  onSelectFile={onSelectFile}
                  sessionId={activeSessionId}
                  repoPath={activeSession?.main_repo_path ?? activeSession?.project_path ?? null}
                  baseBranchOverride={activeSession?.base_branch_override ?? null}
                  onBaseBranchChanged={onDiffRefresh}
                />
              </div>
            )}
          </div>
        )}

        {activePluginPane && (
          <div className="absolute inset-0 z-10 flex flex-col min-h-0 overflow-hidden bg-surface-900">
            <PluginPaneBody entry={activePluginPane.entry} />
          </div>
        )}
      </div>
      {sendDialogOpen && commentsEnabled && activeSessionId && (
        <SendCommentsDialog
          sessionId={activeSessionId}
          comments={diffComments.comments}
          isMultiRepo={commentsIsMultiRepo}
          sendEnabled={commentSendEnabled}
          sendDisabledReason={commentSendDisabledReason}
          introDraft={diffComments.introDraft}
          outroDraft={diffComments.outroDraft}
          clearAfterSend={diffComments.clearAfterSend}
          onChangeIntro={diffComments.setIntroDraft}
          onChangeOutro={diffComments.setOutroDraft}
          onChangeClearAfterSend={diffComments.setClearAfterSend}
          onClose={onCloseSendDialog}
          onSent={() => {
            if (diffComments.clearAfterSend) {
              diffComments.clearComments();
              diffComments.setIntroDraft("");
              diffComments.setOutroDraft("");
            }
            onCloseSendDialog();
            onClearSelectedFile();
          }}
        />
      )}
    </div>
  );
}
