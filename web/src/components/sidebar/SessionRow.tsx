import { memo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Pin } from "lucide-react";
import type { Workspace } from "../../lib/types";
import {
  attachSessionProject,
  createSession,
  fetchProjects,
  renameSession,
  setSessionColor,
  setSessionNotifications,
  setWorktreeName,
  smartRenameSession,
  summarizeSession,
  updateSessionGroup,
} from "../../lib/api";
import { isSessionActive } from "../../lib/session";
import { useIdleDecayWindowMs } from "../../lib/idleDecay";
import { useUnreadIndicatorEnabled } from "../../lib/unreadIndicator";
import { sessionRowChromeClass } from "../../lib/sessionRowChrome";
import { useSessionColorsEnabled } from "../../lib/sessionColors";
import { useSidebarCompact } from "../../lib/sidebarCompact";
import { requestOpenSession } from "../../lib/sessionRoute";
import { requestSwitchAgent } from "../../lib/switchAgentTrigger";
import type { OptimisticTriage } from "../../lib/sidebarOptimistic";
import { reportError, reportInfo } from "../../lib/toastBus";
import { ContextMenu } from "../ContextMenu";
import { SessionGroupModal } from "../SessionGroupModal";
import { StatusGlyph } from "../StatusGlyph";
import { useContextMenu } from "../useContextMenu";
import { AddProjectModal } from "./AddProjectModal";
import { RowSubRows, RowTrailingBadges } from "./RowBadges";
import { deriveRowModel, type RowModel } from "./rowModel";
import { BulkTriageMenuItems, SingleRowMenuItems, type SingleRowActions } from "./SessionRowMenu";
import { SnoozeModal } from "./SnoozeModal";
import type { RowActivate, RowBulkApi, RowContextScope } from "./types";
import { useLongPress } from "./useLongPress";
import { WorkdirNameModal } from "./WorkdirNameModal";

type Modal = "snooze" | "workdir" | "addProject" | "group" | null;

export interface SessionRowProps {
  workspace: Workspace;
  isActive: boolean;
  isSelected: boolean;
  onActivate: RowActivate;
  /** Receives the row's sessions, which a group slice narrows to a subset of the workspace. */
  onDelete?: (sessionIds: string[]) => void;
  onStop?: (sessionId: string) => void;
  onStart?: (sessionId: string) => void;
  onSwitchView?: (sessionId: string, toStructured: boolean) => void;
  /** Opens the wizard prefilled from this row's project, like the group header's "+". */
  onCreateSession?: (repoPath: string) => void;
  readOnly?: boolean;
  indented?: boolean;
  optimistic: OptimisticTriage;
  onPinToggle: (ws: Workspace, pinned: boolean) => void;
  onArchiveToggle: (ws: Workspace, archived: boolean) => void;
  onSnooze: (ws: Workspace, minutes: number | null) => void;
  onUnreadToggle: (ws: Workspace, markUnread: boolean) => void;
  bulkApi: RowBulkApi;
}

export const SessionRow = memo(function SessionRow(props: SessionRowProps) {
  const { workspace, isActive, isSelected, onActivate, readOnly, indented, bulkApi } = props;
  const idleDecayWindowMs = useIdleDecayWindowMs();
  const unreadIndicatorEnabled = useUnreadIndicatorEnabled();
  const sessionColorsEnabled = useSessionColorsEnabled();
  const compact = useSidebarCompact();
  const model = deriveRowModel(workspace, props.optimistic, { idleDecayWindowMs, isActive, unreadIndicatorEnabled });
  const { label, sessionId, isDeleting, navigationSession } = model;

  const [modal, setModal] = useState<Modal>(null);
  const [addProjectOptions, setAddProjectOptions] = useState<{ name: string; path: string }[]>([]);
  const [renaming, setRenaming] = useState(false);
  const [renameValue, setRenameValue] = useState(label);
  const renameRef = useRef<HTMLInputElement>(null);

  const openedAtRef = useRef(0);
  const { menu, menuRef, openMenu, closeMenu } = useContextMenu<{ x: number; y: number; scope: RowContextScope }>(
    openedAtRef,
  );
  const openMenuAt = (x: number, y: number) => openMenu({ x, y, scope: bulkApi.prepareScope(workspace) });
  const longPress = useLongPress(!!sessionId && !isDeleting, (x, y) => {
    openedAtRef.current = Date.now();
    openMenuAt(x, y);
  });

  const commitRename = async () => {
    setRenaming(false);
    const trimmed = renameValue.trim();
    // Compared with the title, not the label, so accepting a branch-prefilled value still sets a title.
    if (!trimmed || trimmed === model.sessionTitle || !sessionId) return;
    const result = await renameSession(sessionId, trimmed);
    if (result.ok) {
      // The title persisted; a tmux rekey warning is informational.
      result.warnings?.forEach((warning) => reportInfo(warning));
    } else {
      reportError(result.message ?? "Could not rename this session. Please try again.");
    }
  };

  const startRename = () => {
    closeMenu();
    if (renaming) return;
    setRenameValue(model.sessionTitle || label);
    setRenaming(true);
    requestAnimationFrame(() => renameRef.current?.select());
  };
  const actions = { ...buildRowActions(props, model, closeMenu, setModal), rename: startRename };
  const openAddProject = () => {
    actions.addProject();
    void fetchProjects().then((projects) =>
      setAddProjectOptions(projects.map((p) => ({ name: p.name, path: p.path }))),
    );
  };

  if (renaming) {
    return (
      <div className={`py-1 ${indented ? "pl-6 pr-3" : "px-3"}`}>
        <input
          ref={renameRef}
          type="text"
          value={renameValue}
          onChange={(e) => setRenameValue(e.target.value)}
          onBlur={commitRename}
          onKeyDown={(e) => {
            if (e.key === "Enter") commitRename();
            if (e.key === "Escape") setRenaming(false);
          }}
          data-testid="sidebar-rename-input"
          className="w-full bg-surface-900 border border-brand-600 rounded px-2 py-1 text-[13px] md:text-[14px] font-mono text-text-primary focus:outline-none"
        />
      </div>
    );
  }

  const attention = model.needsAttention && !model.showUnreadGlyph;
  const labelTone = model.showUnreadGlyph
    ? "text-status-unread font-semibold"
    : isSessionActive({ status: model.status, idle_entered_at: model.idleEnteredAt }, idleDecayWindowMs)
      ? model.textClass
      : isActive
        ? "text-text-primary"
        : "text-text-secondary";
  const navigationSessionId = navigationSession?.id ?? null;

  return (
    <>
      <a
        href={navigationSessionId ? `/session/${encodeURIComponent(navigationSessionId)}` : "/"}
        tabIndex={isDeleting ? -1 : undefined}
        aria-disabled={isDeleting || undefined}
        data-testid="sidebar-session-row"
        title={model.needsAttention ? `${label} · ${model.attentionHint}` : label}
        draggable={false}
        onClick={(e) => {
          // Middle-click and Alt+click keep the browser's href behavior.
          if (e.button !== 0 || e.altKey) return;
          e.preventDefault();
          if (isDeleting || longPress.fired.current) return;
          onActivate(workspace.id, e, navigationSessionId);
        }}
        onContextMenu={(e) => {
          if (isDeleting) return;
          e.preventDefault();
          openMenuAt(e.clientX, e.clientY);
        }}
        {...longPress.handlers}
        data-selected={isSelected || undefined}
        className={`block w-full text-left py-2 cursor-pointer select-none [-webkit-touch-callout:none] transition-colors duration-75 ${
          compact ? (indented ? "pl-3 pr-1" : "px-2") : indented ? "pl-6 pr-3" : "px-3"
        } ${sessionRowChromeClass(isActive, isSelected)} ${isDeleting ? "opacity-50 pointer-events-none" : ""}`}
      >
        {isSelected && <span className="sr-only">Selected</span>}
        <div className="flex items-center gap-2">
          <span
            className={`text-sm shrink-0 leading-none font-mono ${model.showUnreadGlyph ? "text-status-unread font-semibold" : model.textClass} ${
              attention ? "motion-safe:animate-pulse font-semibold" : ""
            }`}
            data-attention={attention ? "true" : undefined}
            aria-label={attention ? `${model.status} · ${model.attentionHint}` : undefined}
          >
            {model.showUnreadGlyph ? (
              <span title="Unread" aria-label="Unread" data-testid="sidebar-unread-dot">
                ●
              </span>
            ) : (
              <StatusGlyph
                status={model.status}
                createdAt={model.createdAt}
                idleEnteredAt={model.idleEnteredAt}
                dormant={model.dormant}
              />
            )}
          </span>
          <div className="min-w-0 flex-1">
            <span
              className={`flex items-center gap-1.5 text-[13px] md:text-[14px] ${labelTone} ${model.isFavorited || model.effectivePinned ? "font-semibold" : ""} ${model.effectiveArchived || model.effectiveSnoozed ? "italic opacity-70" : ""}`}
            >
              {sessionColorsEnabled && model.sessionColorDot && (
                <span
                  title={`Color: ${model.sessionColor}`}
                  aria-label={`Color: ${model.sessionColor}`}
                  data-testid="sidebar-session-color-dot"
                  data-color={model.sessionColor ?? undefined}
                  className={`shrink-0 inline-block h-2 w-2 rounded-full ${model.sessionColorDot}`}
                />
              )}
              {!compact && model.effectivePinned && (
                <span title="Pinned" aria-label="Pinned" className="shrink-0 inline-flex text-brand-400">
                  <Pin className="h-3 w-3 -rotate-45" />
                </span>
              )}
              {!compact && model.isFavorited && (
                <span title="Favorited" aria-label="Favorited" className="shrink-0 text-amber-300">
                  *
                </span>
              )}
              <span className="truncate" title={label}>
                {label}
              </span>
              {!compact && <RowTrailingBadges workspace={workspace} model={model} />}
            </span>
            {!compact && <RowSubRows first={model.firstSession} />}
          </div>
        </div>
      </a>
      {menu && (
        <ContextMenu menu={menu} menuRef={menuRef} testId="sidebar-context-menu" minWidth="min-w-[180px]">
          {menu.scope.kind === "bulk" ? (
            <BulkTriageMenuItems
              count={menu.scope.count}
              buckets={menu.scope.buckets}
              api={bulkApi}
              onDone={closeMenu}
            />
          ) : (
            <SingleRowMenuItems
              model={model}
              readOnly={readOnly}
              colorsEnabled={sessionColorsEnabled}
              unreadEnabled={unreadIndicatorEnabled}
              actions={{ ...actions, addProject: openAddProject }}
            />
          )}
        </ContextMenu>
      )}
      <RowModals
        modal={modal}
        setModal={setModal}
        model={model}
        addProjectOptions={addProjectOptions}
        onSnooze={(minutes) => {
          closeMenu();
          setModal(null);
          props.onSnooze(workspace, minutes);
        }}
      />
    </>
  );
});

/** Menu actions; each closes the menu first so its dismiss listener cannot race a modal mount. */
function buildRowActions(
  props: SessionRowProps,
  model: RowModel,
  closeMenu: () => void,
  setModal: (m: Modal) => void,
): Omit<SingleRowActions, "rename"> {
  const { workspace } = props;
  const { firstSession: first, acpSession: acp, sessionId } = model;
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
    switchView: after(() => first && props.onSwitchView?.(first.id, first.view !== "structured")),
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
    notify: (preset) =>
      after(async () => {
        if (!sessionId || preset === model.notifyPreset) return;
        await setSessionNotifications(sessionId, preset);
      })(),
    // No optimistic overlay for color; the dot updates on the next poll.
    color: (color) =>
      after(async () => {
        if (!sessionId || color === model.sessionColor) return;
        if (!(await setSessionColor(sessionId, color))) {
          reportError(color ? "Failed to set session color" : "Failed to clear session color");
        }
      })(),
    pin: after(() => props.onPinToggle(workspace, !model.effectivePinned)),
    archive: after(() => props.onArchiveToggle(workspace, !model.effectiveArchived)),
    openSnooze: after(() => setModal("snooze")),
    unsnooze: after(() => {
      setModal(null);
      props.onSnooze(workspace, null);
    }),
    unread: after(() => props.onUnreadToggle(workspace, !model.effectiveUnread)),
    remove: after(() => props.onDelete?.(workspace.sessions.map((s) => s.id))),
  };
}

function RowModals({
  modal,
  setModal,
  model,
  addProjectOptions,
  onSnooze,
}: {
  modal: Modal;
  setModal: (m: Modal) => void;
  model: RowModel;
  addProjectOptions: { name: string; path: string }[];
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
        projects={addProjectOptions}
        onCancel={close}
        onDone={close}
        onSubmit={(project, attachExistingBranch) => attachSessionProject(sessionId, project, { attachExistingBranch })}
      />,
      document.body,
    );
  }
  return null;
}
