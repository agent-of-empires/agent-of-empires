import { StructuredSessionStack } from "./StructuredSessionStack";
import { TerminalSessionStack } from "./TerminalSessionStack";
import { PairedShellPane, ShellModeToggle, type ShellMode } from "./PairedTerminal";
import { shellLabel } from "../lib/pairedShells";
import type { AgentFocus } from "./acp/BackgroundAgentsPanel";
import { hasStartedBefore } from "./acp/startupWait";
import { recreateContainerBlock, recreateContainerTitle } from "./sidebar/sessionActionTargets";
import type { NewSessionLike } from "./acp/ContainerGoneBanner";
import { FilesPane } from "./FilesPane";
import { DiffFileList } from "./diff/DiffFileList";
import { FileContentViewer } from "./diff/FileContentViewer";
import { CommentsBanner } from "./diff/comments/CommentsBanner";
import { PluginPaneBody } from "./plugin/PluginPane";
import type { RightPanelView } from "../lib/rightPanelView";
import { isPluginPaneId, type PluginPane } from "../lib/pluginPanes";
import type { RepoBase, RichDiffFile, SessionResponse } from "../lib/types";
import type { useDiffComments } from "../hooks/useDiffComments";
import type { WebSettings } from "../hooks/useWebSettings";
import type { FileRef } from "../lib/fileRef";
import type { ServerAbout } from "../lib/api";
import { autoResumeSettingsHref } from "../lib/autoResumeSettingsHref";
import { agentDefaultsHref } from "../lib/agentModelDefault";
import { lazyView } from "../lib/lazyView";
import { CloseOnEscape } from "./diff/comments/CloseOnEscape";

const BackgroundAgentsPanel = lazyView(
  () => import("./acp/BackgroundAgentsPanel").then((m) => m.BackgroundAgentsPanel),
  null,
);
const DiffFileViewer = lazyView(() => import("./diff/DiffFileViewer").then((m) => m.DiffFileViewer), null);
const SendCommentsDialog = lazyView(
  () => import("./diff/comments/SendCommentsDialog").then((m) => m.SendCommentsDialog),
  (p) => <CloseOnEscape onClose={p.onClose} />,
);
const StructuredView = lazyView(() => import("./acp/StructuredView").then((m) => m.StructuredView), null);

interface Props {
  /** Start a Session, for a queued row's "Start and send". */
  onStartSession?: (sessionId: string) => void;
  readOnly: boolean;
  recoveryCurrentIds?: readonly string[];
  view: RightPanelView;
  pluginPanes: PluginPane[];
  onBackToAgent: () => void;
  onOpenAgentsPane: (agentId?: string) => void;
  agentFocus: AgentFocus | null;
  /** Opens the switch-view confirmation for that Session; omitted where it can't switch. */
  onSwitchToTerminal?: (sessionId: string) => void;
  /** Shows that Session's paired shell in `mode`; omitted where the terminal can't be used. */
  onOpenTerminal?: (sessionId: string, mode: ShellMode) => void;
  /** Whether this server lets the dashboard restart an Agent. */
  canRestartAgent: boolean;
  /** Opens the wizard for a Session like that one; undefined on a read-only server. */
  newSessionLike?: (session: SessionResponse) => NewSessionLike | undefined;
  /** The shell the paired view shows, which may be another Session's, and its mode. */
  pairedShell: { session: SessionResponse | null; index: number };
  shellMode: ShellMode;
  onShellModeChange: (mode: ShellMode) => void;
  pairedMounted: boolean;
  activeSession: SessionResponse | null;
  activeSessionId: string | null;
  sessions: SessionResponse[];
  webSettings: Pick<
    WebSettings,
    "persistentTerminals" | "maxPersistentTerminals" | "persistentStructuredViews" | "maxPersistentStructuredViews"
  >;
  selectedFilePath: string | null;
  selectedFileExternal?: boolean;
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
  commentSendDisabledReason: string;
  diffComments: ReturnType<typeof useDiffComments>;
  commentsIsMultiRepo: boolean;
  sendDialogOpen: boolean;
  onOpenSendDialog: () => void;
  onCloseSendDialog: () => void;
  onClearSelectedFile: () => void;
  /** Decides whether the usage-limit banner links to Settings. */
  serverAbout: ServerAbout | null;
  /** Opens a Settings link in the app. */
  onOpenSettings: (href: string) => void;
}

function layerClass(active: boolean): string {
  const base = "absolute inset-0 flex flex-col min-h-0 overflow-hidden";
  return active ? base : `${base} invisible pointer-events-none`;
}

/** Keep terminal geometry and scrollback across switches; only the visible
 *  surface owns keyboard input. */
export function MobileMainPane({
  onStartSession,
  readOnly,
  recoveryCurrentIds,
  view,
  pluginPanes,
  onBackToAgent,
  onOpenAgentsPane,
  agentFocus,
  onSwitchToTerminal,
  onOpenTerminal,
  canRestartAgent,
  newSessionLike,
  pairedShell,
  shellMode,
  onShellModeChange,
  pairedMounted,
  activeSession,
  activeSessionId,
  sessions,
  webSettings,
  selectedFilePath,
  selectedFileExternal,
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
  serverAbout,
  onOpenSettings,
}: Props) {
  const activePluginPane = isPluginPaneId(view) ? (pluginPanes.find((p) => p.id === view) ?? null) : null;
  return (
    // Only the agent scrolls under the floating header; other views start below it.
    <div className={`flex-1 flex flex-col min-h-0 ${view === "agent" ? "" : "pt-[var(--phone-overlay-top,0px)]"}`}>
      {view === "paired" && (
        // Which shell shows, since it can be another Session's, and its Host/Container control.
        <div
          data-testid="mobile-paired-bar"
          className="flex items-center gap-2 h-8 px-3 border-b border-surface-700/40 bg-surface-900 shrink-0"
        >
          <span className="text-xs text-text-dim truncate">
            {pairedShell.session ? `${pairedShell.session.title} · ${pairedShell.index + 1}` : "Paired terminal"}
          </span>
          {pairedShell.session?.is_sandboxed && (
            <div className="ml-auto shrink-0">
              <ShellModeToggle
                mode={shellMode}
                onModeChange={onShellModeChange}
                label={`${pairedShell.session.title} ${shellLabel(pairedShell.index).toLowerCase()}`}
              />
            </div>
          )}
        </div>
      )}
      <div className="relative flex-1 flex flex-col min-h-0 overflow-hidden">
        <div className={layerClass(view === "agent")} inert={view !== "agent"}>
          {activeSession?.view === "structured" ? (
            <StructuredSessionStack
              active={view === "agent"}
              activeSessionId={activeSessionId!}
              sessions={sessions.filter((session) => session.view === "structured")}
              persistent={webSettings.persistentStructuredViews}
              maxPersistentViews={webSettings.maxPersistentStructuredViews}
              renderSession={(session, active, selected) => (
                <StructuredView
                  active={active}
                  selected={selected}
                  sessionId={session.id}
                  readOnly={readOnly}
                  acpWorkerState={session.acp_worker_state ?? "absent"}
                  rateLimitAutoResume={session.rate_limit_auto_resume}
                  rateLimitFailoverTo={session.rate_limit_failover_to}
                  settingsHref={autoResumeSettingsHref(session.profile, serverAbout)}
                  agentDefaultsHref={agentDefaultsHref(session.profile, serverAbout)}
                  onOpenSettings={onOpenSettings}
                  tool={session.tool}
                  acpAgent={session.acp_agent ?? null}
                  startedBefore={hasStartedBefore(session)}
                  acpProvider={session.acp_provider ?? null}
                  clearAliases={session.clear_aliases}
                  compactAliases={session.compact_aliases}
                  onStartSession={onStartSession && (() => onStartSession(session.id))}
                  archivedAt={session.archived_at ?? null}
                  snoozedUntil={session.snoozed_until ?? null}
                  queuedCount={session.queued_count}
                  trashedAt={session.trashed_at ?? null}
                  onOpenFileRef={onOpenFileRef}
                  fileRefSession={session}
                  onOpenAgentsPane={onOpenAgentsPane}
                  serverReadOnly={readOnly}
                  isSandboxed={session.is_sandboxed}
                  recreateContainerTitle={recreateContainerTitle(session)}
                  recreateContainerBlock={recreateContainerBlock(session, !!session.archived_at)}
                  onSwitchToTerminal={onSwitchToTerminal ? () => onSwitchToTerminal(session.id) : undefined}
                  onOpenTerminal={onOpenTerminal ? (mode) => onOpenTerminal(session.id, mode) : undefined}
                  canRestartAgent={canRestartAgent}
                  newSessionLike={newSessionLike?.(session)}
                />
              )}
            />
          ) : (
            // Clear the home indicator without changing the keyboard-open lift.
            <div className="home-indicator-clearance flex-1 flex flex-col min-h-0 overflow-hidden">
              <TerminalSessionStack
                readOnly={readOnly}
                recoveryCurrentIds={recoveryCurrentIds}
                active={view === "agent"}
                activeSessionId={activeSessionId!}
                sessions={sessions.filter((session) => session.view !== "structured")}
                persistent={webSettings.persistentTerminals}
                maxPersistentTerminals={webSettings.maxPersistentTerminals}
              />
            </div>
          )}
        </div>

        {pairedMounted && (
          // Match the agent terminal’s home-indicator clearance.
          <div
            className={`home-indicator-clearance ${layerClass(view === "paired")}`}
            inert={view !== "paired"}
            data-testid="mobile-paired-layer"
          >
            <PairedShellPane
              session={pairedShell.session}
              terminalIndex={pairedShell.index}
              readOnly={readOnly}
              active={view === "paired"}
              mode={shellMode}
            />
          </div>
        )}

        {view === "diff" && (
          // Reserve the bottom home-indicator inset here (the App root no longer
          // does; see index.css .safe-area-inset) so the last diff row clears it.
          <div
            className="absolute inset-0 z-10 flex flex-col min-h-0 overflow-hidden bg-surface-900"
            style={{ paddingBottom: "env(safe-area-inset-bottom)" }}
          >
            {selectedFilePath && activeSessionId ? (
              selectedFileExternal ? (
                <FileContentViewer
                  sessionId={activeSessionId}
                  filePath={selectedFilePath}
                  structured={activeSession?.view === "structured"}
                  onBack={onCloseFile}
                />
              ) : (
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
              )
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

        {view === "agents" && (
          <div
            className="absolute inset-0 z-10 flex flex-col min-h-0 overflow-hidden bg-surface-900"
            style={{ paddingBottom: "env(safe-area-inset-bottom)" }}
          >
            <BackgroundAgentsPanel
              sessionId={activeSessionId}
              readOnly={readOnly || !!activeSession?.trashed_at || !!activeSession?.archived_at}
              focus={agentFocus}
              onShowThread={onBackToAgent}
            />
          </div>
        )}

        {view === "files" && (
          <div
            className="absolute inset-0 z-10 flex flex-col min-h-0 overflow-hidden bg-surface-900"
            style={{ paddingBottom: "env(safe-area-inset-bottom)" }}
          >
            <FilesPane
              key={activeSessionId ?? "none"}
              sessionId={activeSessionId}
              structured={activeSession?.view === "structured"}
            />
          </div>
        )}

        {activePluginPane && (
          // Reserve the bottom home-indicator inset here too (see the diff and paired wrappers); the App root no
          // longer does.
          <div
            className="absolute inset-0 z-10 flex flex-col min-h-0 overflow-hidden bg-surface-900"
            data-testid="mobile-plugin-layer"
            style={{ paddingBottom: "env(safe-area-inset-bottom)" }}
          >
            <PluginPaneBody entry={activePluginPane.entry} />
          </div>
        )}
      </div>
      {sendDialogOpen && commentsEnabled && activeSessionId && (
        <SendCommentsDialog
          sessionId={activeSessionId}
          delivery={activeSession?.view === "structured" ? "structured" : "terminal"}
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
            if (activeSession?.view !== "structured") onBackToAgent();
          }}
        />
      )}
    </div>
  );
}
