import { useState, type ReactNode, type RefObject } from "react";
import { createPortal } from "react-dom";
import type { SessionResponse, Workspace } from "../../lib/types";
import {
  createSession,
  setWorktreeName,
  smartRenameSession,
  summarizeSession,
  updateSessionGroup,
  type SessionMutation,
} from "../../lib/api";
import { useUnreadIndicatorEnabled } from "../../lib/unreadIndicator";
import { useSessionColorsEnabled } from "../../lib/sessionColors";
import { requestOpenSession } from "../../lib/sessionRoute";
import { requestSwitchAgent } from "../../lib/switchAgentTrigger";
import { reportError, reportInfo } from "../../lib/toastBus";
import { ContextMenu } from "../ContextMenu";
import { SessionGroupModal } from "../SessionGroupModal";
import { StatusGlyph } from "../StatusGlyph";
import type { RowModel } from "./rowModel";
import { BulkTriageMenuItems, SingleRowMenuItems, type SingleRowActions } from "./SessionRowMenu";
import type { SessionAction } from "./sessionActionTargets";
import { RecreateContainerDialog } from "./RecreateContainerDialog";
import { SnoozeModal } from "./SnoozeModal";
import type { RowBulkApi, RowContextScope } from "./types";
import { WorkdirNameModal } from "./WorkdirNameModal";
import { useSessionActions } from "./usePendingSetting";

type Modal = "snooze" | "workdir" | "group" | null;

/** What a session's actions call back into; the sidebar and a plugin page's host menu both supply them. */
export interface SessionActionHandlers {
  onSessionMutation: (mutation: SessionMutation) => void;
  /** Receives the row's sessions, which a group slice narrows to a subset of the workspace. */
  onDelete?: (sessionIds: string[]) => void;
  onStop?: (sessionId: string) => void;
  onStart?: (sessionId: string) => void;
  onRestartForUpdate?: (sessionId: string) => void;
  onSwitchView?: (sessionId: string, toStructured: boolean) => void;
  /** Opens the wizard prefilled from this row's project, like the group header's "+". */
  onCreateSession?: (repoPath: string) => void;
  onPinToggle: (ws: Workspace, pinned: boolean) => void;
  onArchiveToggle: (ws: Workspace, archived: boolean) => void;
  onSnooze: (ws: Workspace, minutes: number | null) => void;
  onUnreadToggle: (ws: Workspace, markUnread: boolean) => void;
}

export type SessionMenuState = { x: number; y: number; scope: RowContextScope };

/** A session's context menu and the pickers it opens. Stays mounted while closed so a picker outlives the menu. */
export function SessionActionsMenu({
  menu,
  menuRef,
  closeMenu,
  returnFocusTo,
  workspace,
  model,
  setNotify,
  setColor,
  readOnly,
  handlers,
  onRename,
  bulkApi,
  extraItems,
}: {
  menu: SessionMenuState | null;
  menuRef: RefObject<HTMLDivElement | null>;
  closeMenu: () => void;
  returnFocusTo?: RefObject<HTMLElement | null>;
  workspace: Workspace;
  model: RowModel;
  setNotify: SingleRowActions["notify"];
  setColor: SingleRowActions["color"];
  readOnly?: boolean;
  handlers: SessionActionHandlers;
  onRename: () => void;
  /** Required for a menu that can open on a bulk scope. */
  bulkApi?: RowBulkApi;
  extraItems?: ReactNode;
}) {
  const [modal, setModal] = useState<Modal>(null);
  const [recreating, setRecreating] = useState<SessionResponse | null>(null);
  const owner = useSessionActions();
  const colorsEnabled = useSessionColorsEnabled();
  const unreadEnabled = useUnreadIndicatorEnabled();
  const { label } = model;
  const actions = {
    ...buildRowActions(workspace, handlers, model, closeMenu, setModal, setRecreating, () => {
      if (model.firstSession) owner.openAttach(model.firstSession);
    }),
    rename: onRename,
    notify: setNotify,
    color: setColor,
  };

  return (
    <>
      {menu && (
        <ContextMenu
          menu={menu}
          menuRef={menuRef}
          testId="sidebar-context-menu"
          minWidth="min-w-[240px]"
          sheetOnMobile
          label={menu.scope.kind === "bulk" ? `${menu.scope.count} selected sessions` : `${label} actions`}
          heading={
            menu.scope.kind === "bulk" ? (
              <span className="text-sm text-text-primary">{menu.scope.count} selected</span>
            ) : (
              <span className="flex items-center gap-2 text-sm font-mono text-text-primary">
                <span className={`shrink-0 leading-none ${model.textClass}`}>
                  <StatusGlyph
                    status={model.status}
                    createdAt={model.createdAt}
                    idleEnteredAt={model.idleEnteredAt}
                    dormant={model.dormant}
                  />
                </span>
                {model.sessionColorDot && colorsEnabled && (
                  <span className={`h-2 w-2 shrink-0 rounded-full ${model.sessionColorDot}`} aria-hidden="true" />
                )}
                <span className="min-w-0">
                  <span className="block truncate">{label}</span>
                  {model.sessionCount > 0 && (
                    <span
                      data-testid="sidebar-context-menu-summary"
                      className="block font-sans text-xs text-text-subtle"
                    >
                      {model.sessionCount === 1 && model.firstSession
                        ? `${model.firstSession.status} · ${model.firstSession.view === "structured" ? "Structured view" : "Terminal view"}`
                        : `${model.sessionCount} Sessions`}
                    </span>
                  )}
                </span>
              </span>
            )
          }
          onClose={closeMenu}
          returnFocusTo={returnFocusTo}
        >
          {menu.scope.kind === "bulk" && bulkApi ? (
            <BulkTriageMenuItems buckets={menu.scope.buckets} api={bulkApi} onDone={closeMenu} />
          ) : (
            <SingleRowMenuItems
              model={model}
              readOnly={readOnly}
              colorsEnabled={colorsEnabled}
              unreadEnabled={unreadEnabled}
              actions={actions}
              extraItems={extraItems}
            />
          )}
        </ContextMenu>
      )}
      <RowModals
        modal={modal}
        setModal={setModal}
        model={model}
        onSessionMutation={handlers.onSessionMutation}
        onSnooze={(minutes) => {
          closeMenu();
          setModal(null);
          handlers.onSnooze(workspace, minutes);
        }}
      />
      {recreating &&
        createPortal(
          <RecreateContainerDialog
            sessionId={recreating.id}
            sessionTitle={recreating.title}
            onClose={() => setRecreating(null)}
          />,
          document.body,
        )}
    </>
  );
}

/** Menu actions; each closes the menu first so its dismiss listener cannot race a modal mount.
 *  Pin, unread, notify and color are in-place toggles that keep it open; the row supplies the last two. */
function buildRowActions(
  workspace: Workspace,
  props: SessionActionHandlers,
  model: RowModel,
  closeMenu: () => void,
  setModal: (m: Modal) => void,
  setRecreating: (session: SessionResponse) => void,
  openAttach: () => void,
): Omit<SingleRowActions, "rename" | "notify" | "color"> {
  const after = (fn: () => unknown) => () => {
    closeMenu();
    void fn();
  };
  // Re-checks the chosen session against the live row, since it can change while the menu is open.
  const afterTarget = (action: SessionAction, fn: (s: SessionResponse) => unknown) => (sessionId: string) => {
    closeMenu();
    const session = workspace.sessions.find((s) => s.id === sessionId);
    if (!session || !model.actionTargets[action].some((t) => t.id === sessionId)) {
      reportError("That session changed. Please try again.");
      return;
    }
    void fn(session);
  };
  return {
    newSession:
      props.onCreateSession && model.newSessionRepoPath
        ? after(() => props.onCreateSession!(model.newSessionRepoPath!))
        : undefined,
    editWorkdir: after(() => setModal("workdir")),
    addProject: after(openAttach),
    editGroup: after(() => setModal("group")),
    switchView: afterTarget("switchView", (s) => props.onSwitchView?.(s.id, s.view !== "structured")),
    // The dialog lives in the session's Composer, so navigate first; a pending latch opens it on mount.
    switchAgent: afterTarget("switchAgent", (s) => {
      requestOpenSession(s.id);
      requestSwitchAgent(s.id);
    }),
    fork: afterTarget("fork", async (s) => {
      const result = await createSession({
        path: s.project_path,
        tool: s.tool,
        view: "structured",
        profile: s.profile || undefined,
        fork_from: s.acp_session_id!,
      });
      if (result.ok && result.session) requestOpenSession(result.session.id);
      else reportError(result.error ?? "Could not fork this session. Please try again.");
    }),
    autoName: afterTarget("autoName", async (s) => {
      const result = await smartRenameSession(s.id);
      if (!result.ok) reportError(result.message ?? "Could not start auto-name. Please try again.");
    }),
    summarize: afterTarget("summarize", async (s) => {
      const result = await summarizeSession(s.id);
      if (result.ok) reportInfo("Summarizing the conversation so far…");
      else reportError(result.message ?? "Could not start the summary. Please try again.");
    }),
    stop: afterTarget("stop", (s) => props.onStop?.(s.id)),
    start: afterTarget("start", (s) => props.onStart?.(s.id)),
    restartUpdate: afterTarget("restartUpdate", (s) => props.onRestartForUpdate?.(s.id)),
    recreateContainer: afterTarget("recreateContainer", setRecreating),
    pin: () => props.onPinToggle(workspace, !model.effectivePinned),
    archive: after(() => props.onArchiveToggle(workspace, !model.effectiveArchived)),
    openSnooze: after(() => setModal("snooze")),
    unsnooze: after(() => {
      setModal(null);
      props.onSnooze(workspace, null);
    }),
    unread: () => props.onUnreadToggle(workspace, !model.effectiveUnread),
    remove: after(() => props.onDelete?.(workspace.sessions.map((s) => s.id))),
  };
}

function RowModals({
  modal,
  setModal,
  model,
  onSnooze,
  onSessionMutation,
}: {
  modal: Modal;
  setModal: (m: Modal) => void;
  model: RowModel;
  onSnooze: (minutes: number) => void;
  onSessionMutation: SessionActionHandlers["onSessionMutation"];
}) {
  const { label, sessionId } = model;
  const close = () => setModal(null);
  if (modal === "snooze") {
    return createPortal(<SnoozeModal title={label} onCancel={close} onPick={onSnooze} />, document.body);
  }
  if (modal === "group") {
    return createPortal(
      <SessionGroupModal
        sessionTitle={model.sessionTitle || label}
        currentGroup={model.firstSession?.group_path ?? ""}
        onSave={async (group) => {
          if (!sessionId) return false;
          const mutation = await updateSessionGroup(sessionId, group);
          if (!mutation) return false;
          onSessionMutation(mutation);
          return true;
        }}
        onClose={close}
      />,
      document.body,
    );
  }
  if (!sessionId) return null;
  if (modal === "workdir") {
    return createPortal(
      <WorkdirNameModal
        title={label}
        currentBranch={model.branchLabel}
        onCancel={close}
        onSubmit={async (name, renameBranch) => {
          const res = await setWorktreeName(sessionId, name, renameBranch);
          if (res.ok) {
            onSessionMutation(res);
            res.warnings?.forEach((warning) => reportInfo(warning));
            close();
          }
          return res;
        }}
      />,
      document.body,
    );
  }
  return null;
}
