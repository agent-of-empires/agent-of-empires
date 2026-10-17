import { useState, type ReactNode, type RefObject } from "react";
import { createPortal } from "react-dom";
import type { Workspace } from "../../lib/types";
import {
  attachSessionProject,
  createSession,
  setWorktreeName,
  smartRenameSession,
  summarizeSession,
  updateSessionGroup,
} from "../../lib/api";
import { useUnreadIndicatorEnabled } from "../../lib/unreadIndicator";
import { useSessionColorsEnabled } from "../../lib/sessionColors";
import { requestOpenSession } from "../../lib/sessionRoute";
import { requestSwitchAgent } from "../../lib/switchAgentTrigger";
import { reportError, reportInfo } from "../../lib/toastBus";
import { ContextMenu } from "../ContextMenu";
import { SessionGroupModal } from "../SessionGroupModal";
import { StatusGlyph } from "../StatusGlyph";
import { AddProjectModal } from "./AddProjectModal";
import type { RowModel } from "./rowModel";
import { BulkTriageMenuItems, SingleRowMenuItems, type SingleRowActions } from "./SessionRowMenu";
import { SnoozeModal } from "./SnoozeModal";
import type { RowBulkApi, RowContextScope } from "./types";
import { WorkdirNameModal } from "./WorkdirNameModal";

type Modal = "snooze" | "workdir" | "addProject" | "group" | null;

/** What a session's actions call back into; the sidebar and a plugin page's host menu both supply them. */
export interface SessionActionHandlers {
  /** Receives the row's sessions, which a group slice narrows to a subset of the workspace. */
  onDelete?: (sessionIds: string[]) => void;
  onStop?: (sessionId: string) => void;
  onStart?: (sessionId: string) => void;
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
  const colorsEnabled = useSessionColorsEnabled();
  const unreadEnabled = useUnreadIndicatorEnabled();
  const { label } = model;
  const actions = {
    ...buildRowActions(workspace, handlers, model, closeMenu, setModal),
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
                <span className="truncate">{label}</span>
              </span>
            )
          }
          onClose={closeMenu}
          returnFocusTo={returnFocusTo}
        >
          {menu.scope.kind === "bulk" && bulkApi ? (
            <BulkTriageMenuItems buckets={menu.scope.buckets} api={bulkApi} onDone={closeMenu} />
          ) : (
            <>
              <SingleRowMenuItems
                model={model}
                readOnly={readOnly}
                colorsEnabled={colorsEnabled}
                unreadEnabled={unreadEnabled}
                actions={actions}
              />
              {extraItems}
            </>
          )}
        </ContextMenu>
      )}
      <RowModals
        modal={modal}
        setModal={setModal}
        model={model}
        onSnooze={(minutes) => {
          closeMenu();
          setModal(null);
          handlers.onSnooze(workspace, minutes);
        }}
      />
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
): Omit<SingleRowActions, "rename" | "notify" | "color"> {
  const { firstSession: first, acpSession: acp } = model;
  const after = (fn: () => unknown) => () => {
    closeMenu();
    void fn();
  };
  return {
    newSession:
      props.onCreateSession && model.newSessionRepoPath
        ? after(() => props.onCreateSession!(model.newSessionRepoPath!))
        : undefined,
    editWorkdir: after(() => setModal("workdir")),
    addProject: after(() => setModal("addProject")),
    editGroup: after(() => setModal("group")),
    switchView: after(
      () => first && props.onSwitchView?.(first.id, first.view !== "structured" && !first.terminal_handoff_pending),
    ),
    // The dialog lives in the session's Composer, so navigate first; a pending latch opens it on mount.
    switchAgent: after(() => {
      if (!acp) return;
      requestOpenSession(acp.id);
      requestSwitchAgent(acp.id);
    }),
    fork: after(async () => {
      if (!acp?.acp_session_id || !acp.acp_can_fork) return;
      const result = await createSession({
        path: acp.project_path,
        tool: acp.tool,
        view: "structured",
        profile: acp.profile || undefined,
        fork_from: acp.acp_session_id,
      });
      if (result.ok && result.session) requestOpenSession(result.session.id);
      else reportError(result.error ?? "Could not fork this session. Please try again.");
    }),
    autoName: after(async () => {
      if (!acp) return;
      const result = await smartRenameSession(acp.id);
      if (!result.ok) reportError(result.message ?? "Could not start auto-name. Please try again.");
    }),
    summarize: after(async () => {
      if (!acp) return;
      const result = await summarizeSession(acp.id);
      if (result.ok) reportInfo("Summarizing the conversation so far…");
      else reportError(result.message ?? "Could not start the summary. Please try again.");
    }),
    stop: after(() => first && props.onStop?.(first.id)),
    start: after(() => first && props.onStart?.(first.id)),
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
}: {
  modal: Modal;
  setModal: (m: Modal) => void;
  model: RowModel;
  onSnooze: (minutes: number) => void;
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
        onSave={async (group) => (sessionId ? updateSessionGroup(sessionId, group) : false)}
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
          if (res.ok) close();
          return res;
        }}
      />,
      document.body,
    );
  }
  if (modal === "addProject") {
    return createPortal(
      <AddProjectModal
        title={label}
        onCancel={close}
        onDone={close}
        onSubmit={(project, attachExistingBranch) => attachSessionProject(sessionId, project, { attachExistingBranch })}
      />,
      document.body,
    );
  }
  return null;
}
