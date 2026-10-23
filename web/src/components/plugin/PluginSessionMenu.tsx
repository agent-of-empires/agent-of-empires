import { useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useSidebarTriage } from "../../hooks/useSidebarTriage";
import { renameSession, type RuntimeCursor } from "../../lib/api";
import type { PageMenuItem } from "../../lib/pluginBridge";
import { reportError, reportInfo } from "../../lib/toastBus";
import type { Workspace } from "../../lib/types";
import { MenuHeading, MenuItem, MenuSeparator } from "../ContextMenu";
import { useContextMenu } from "../useContextMenu";
import { SessionActionsMenu, type SessionActionHandlers, type SessionMenuState } from "../sidebar/SessionActions";
import { useSessionRowModel, type RowObservation } from "../sidebar/useSessionRowModel";
import { SidebarModal } from "../sidebar/SidebarModal";
import { MODAL_CANCEL, MODAL_INPUT, MODAL_PRIMARY } from "../sidebar/styles";

// The page picks when and where the menu opens, so it could put an item under a click it asked for.
const ARM_MS = 300;
const RENAME_UNKNOWN = "Rename outcome is unconfirmed because the runtime changed. Check the session before retrying.";

export interface SessionMenuRequest {
  /** Unique per call, so each call mounts a fresh menu. */
  key: number;
  sessionId: string;
  /** Dashboard viewport coordinates. */
  x: number;
  y: number;
  items: PageMenuItem[];
  /** The page's title, heading its items so they never pass for the dashboard's own. */
  source: string;
  /** Called when the menu closes, with the picked page item's id or null. */
  onClose: (picked: string | null) => void;
}

type TriageHandlers = "onPinToggle" | "onArchiveToggle" | "onSnooze" | "onUnreadToggle";

interface Props extends Omit<RowObservation, "onSessionMutation"> {
  request: SessionMenuRequest;
  workspaces: readonly Workspace[];
  readOnly: boolean;
  handlers: Omit<SessionActionHandlers, TriageHandlers>;
}

/** A workspace groups sessions on one branch and its row acts on the first or all of them; a page names one session. */
function scopeTo(workspaces: readonly Workspace[], sessionId: string): Workspace[] {
  for (const ws of workspaces) {
    const session = ws.sessions.find((s) => s.id === sessionId);
    if (session) return [{ ...ws, sessions: [session] }];
  }
  return [];
}

/** The sidebar's session menu, opened for a plugin page. Stays mounted after closing so its pickers can finish. */
export function PluginSessionMenu(props: Props) {
  // Scoped workspaces keep their shared id, so a new target remounts rather than inheriting another session's optimistic triage.
  return <ScopedMenu key={props.request.sessionId} {...props} />;
}

function ScopedMenu({ request, workspaces, readOnly, handlers, observedById, runtimeEpoch }: Props) {
  const scoped = useMemo(() => scopeTo(workspaces, request.sessionId), [workspaces, request.sessionId]);
  const triage = useSidebarTriage(scoped);
  const [workspace] = scoped;
  if (!workspace) return <Unknown key={request.key} onClose={request.onClose} />;
  return (
    <HostMenu
      key={request.key}
      request={request}
      workspace={workspace}
      optimistic={triage.optimisticFor(workspace.id)}
      readOnly={readOnly}
      observedById={observedById}
      runtimeEpoch={runtimeEpoch}
      handlers={{
        ...handlers,
        onPinToggle: triage.pinToggle,
        onArchiveToggle: triage.archiveToggle,
        onSnooze: triage.snooze,
        onUnreadToggle: triage.unreadToggle,
      }}
    />
  );
}

/** The dashboard does not know the session yet, so the call resolves with nothing picked. */
function Unknown({ onClose }: { onClose: (picked: null) => void }) {
  useEffect(() => onClose(null), [onClose]);
  return null;
}

function HostMenu({
  request,
  workspace,
  optimistic,
  readOnly,
  handlers,
  observedById,
  runtimeEpoch,
}: {
  request: SessionMenuRequest;
  workspace: Workspace;
  optimistic: ReturnType<ReturnType<typeof useSidebarTriage>["optimisticFor"]>;
  readOnly: boolean;
  handlers: SessionActionHandlers;
  observedById: RowObservation["observedById"];
  runtimeEpoch: RowObservation["runtimeEpoch"];
}) {
  const { model, setNotify, setColor } = useSessionRowModel(workspace, optimistic, false, {
    observedById,
    runtimeEpoch,
    onSessionMutation: handlers.onSessionMutation,
  });
  const { menu, menuRef, openMenu, closeMenu } = useContextMenu<SessionMenuState>();
  const [renaming, setRenaming] = useState<{ receipt: RuntimeCursor | null } | null>(null);
  const receipt = renaming?.receipt;
  const observedRename = model.sessionId ? observedById[model.sessionId] : undefined;
  if (receipt && observedRename?.epoch === receipt.epoch && observedRename.revision >= receipt.revision) {
    setRenaming(null);
  }
  const picked = useRef<string | null>(null);
  const opened = useRef(false);
  const [armed, setArmed] = useState(false);
  const { onClose } = request;

  useEffect(() => {
    openMenu({ x: request.x, y: request.y, scope: { kind: "single" } });
    const timer = setTimeout(() => setArmed(true), ARM_MS);
    return () => clearTimeout(timer);
  }, [openMenu, request]);

  useEffect(() => {
    if (menu) opened.current = true;
    else if (opened.current) {
      opened.current = false;
      onClose(picked.current);
    }
  }, [menu, onClose]);

  return (
    <>
      {/* Takes the dismissing click, which inside the page frame would never reach this document. Until armed it
          covers the menu too and swallows clicks. */}
      {menu &&
        createPortal(
          <div
            className={`fixed inset-0 ${armed ? "z-40" : "z-[60]"}`}
            aria-hidden="true"
            data-testid="plugin-session-menu-backdrop"
            data-armed={armed || undefined}
            onMouseDown={() => armed && closeMenu()}
            onContextMenu={(e) => {
              e.preventDefault();
              if (armed) closeMenu();
            }}
          />,
          document.body,
        )}
      <SessionActionsMenu
        menu={menu}
        menuRef={menuRef}
        closeMenu={closeMenu}
        workspace={workspace}
        model={model}
        setNotify={setNotify}
        setColor={setColor}
        readOnly={readOnly}
        handlers={handlers}
        onRename={() => {
          closeMenu();
          setRenaming({ receipt: null });
        }}
        extraItems={
          request.items.length > 0 && (
            <>
              <MenuSeparator />
              <MenuHeading>{request.source}</MenuHeading>
              {request.items.map((item) => (
                <MenuItem
                  key={item.id}
                  onClick={() => {
                    picked.current = item.id;
                    closeMenu();
                  }}
                  testId="plugin-session-menu-item"
                >
                  {item.label}
                </MenuItem>
              ))}
            </>
          )
        }
      />
      {renaming &&
        model.sessionId &&
        createPortal(
          <RenameModal
            sessionId={model.sessionId}
            title={model.label}
            current={model.sessionTitle || model.label}
            onClose={() => setRenaming(null)}
            onSessionMutation={(mutation) => {
              handlers.onSessionMutation(mutation);
              setRenaming({ receipt: mutation.cursor });
            }}
            pending={renaming.receipt}
            runtimeEpoch={runtimeEpoch}
          />,
          document.body,
        )}
    </>
  );
}

function RenameModal({
  sessionId,
  title,
  current,
  onClose,
  onSessionMutation,
  pending,
  runtimeEpoch,
}: {
  sessionId: string;
  title: string;
  current: string;
  onClose: () => void;
  onSessionMutation: RowObservation["onSessionMutation"];
  pending: RuntimeCursor | null;
  runtimeEpoch: RowObservation["runtimeEpoch"];
}) {
  const [value, setValue] = useState(current);
  const [busy, setBusy] = useState(false);
  const unknown = pending !== null && runtimeEpoch !== pending.epoch;
  const locked = busy || (pending !== null && !unknown);

  const submit = async () => {
    const trimmed = value.trim();
    if (locked) return;
    if (!trimmed || trimmed === current) {
      onClose();
      return;
    }
    setBusy(true);
    const result = await renameSession(sessionId, trimmed);
    setBusy(false);
    if (!result.ok) {
      reportError(result.message ?? "Could not rename this session. Please try again.");
      return;
    }
    onSessionMutation(result);
    result.warnings?.forEach((warning) => reportInfo(warning));
  };

  return (
    <SidebarModal
      testId="rename-modal"
      label="Rename session"
      heading="Rename"
      title={title}
      locked={locked}
      onCancel={onClose}
    >
      <form
        className="px-4 py-3 flex flex-col gap-3"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        <input
          type="text"
          autoFocus
          onFocus={(e) => e.currentTarget.select()}
          aria-label="Session name"
          disabled={locked}
          value={value}
          onChange={(e) => setValue(e.target.value)}
          data-testid="rename-modal-input"
          className={MODAL_INPUT}
        />
        {unknown && (
          <p role="alert" className="text-sm text-text-dim">
            {RENAME_UNKNOWN}
          </p>
        )}
        <div className="flex justify-end gap-2">
          <button type="button" disabled={locked} onClick={onClose} className={MODAL_CANCEL}>
            Cancel
          </button>
          <button type="submit" disabled={locked} className={MODAL_PRIMARY}>
            Rename
          </button>
        </div>
      </form>
    </SidebarModal>
  );
}
