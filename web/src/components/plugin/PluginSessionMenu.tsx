import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useSidebarTriage } from "../../hooks/useSidebarTriage";
import { renameSession } from "../../lib/api";
import type { PageMenuItem } from "../../lib/pluginBridge";
import { reportError, reportInfo } from "../../lib/toastBus";
import type { Workspace } from "../../lib/types";
import { MenuItem, MenuSeparator } from "../ContextMenu";
import { useContextMenu } from "../useContextMenu";
import { SessionActionsMenu, type SessionActionHandlers, type SessionMenuState } from "../sidebar/SessionActions";
import { useSessionRowModel } from "../sidebar/useSessionRowModel";
import { SidebarModal } from "../sidebar/SidebarModal";
import { MODAL_CANCEL, MODAL_INPUT, MODAL_PRIMARY } from "../sidebar/styles";

export interface SessionMenuRequest {
  /** Unique per call, so each call mounts a fresh menu. */
  key: number;
  sessionId: string;
  /** Dashboard viewport coordinates. */
  x: number;
  y: number;
  items: PageMenuItem[];
  /** Called when the menu closes, with the picked page item's id or null. */
  onClose: (picked: string | null) => void;
}

type TriageHandlers = "onPinToggle" | "onArchiveToggle" | "onSnooze" | "onUnreadToggle";

interface Props {
  request: SessionMenuRequest;
  workspaces: readonly Workspace[];
  readOnly: boolean;
  handlers: Omit<SessionActionHandlers, TriageHandlers>;
}

/** The sidebar's session menu, opened for a plugin page. Stays mounted after closing so its pickers can finish. */
export function PluginSessionMenu({ request, workspaces, readOnly, handlers }: Props) {
  const triage = useSidebarTriage(workspaces);
  const workspace = workspaces.find((ws) => ws.sessions.some((s) => s.id === request.sessionId));
  if (!workspace) return <Unknown key={request.key} onClose={request.onClose} />;
  return (
    <HostMenu
      key={request.key}
      request={request}
      workspace={workspace}
      optimistic={triage.optimisticFor(workspace.id)}
      readOnly={readOnly}
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
}: {
  request: SessionMenuRequest;
  workspace: Workspace;
  optimistic: ReturnType<ReturnType<typeof useSidebarTriage>["optimisticFor"]>;
  readOnly: boolean;
  handlers: SessionActionHandlers;
}) {
  const { model, setNotify, setColor } = useSessionRowModel(workspace, optimistic, false);
  const { menu, menuRef, openMenu, closeMenu } = useContextMenu<SessionMenuState>();
  const [renaming, setRenaming] = useState(false);
  const picked = useRef<string | null>(null);
  const opened = useRef(false);
  const { onClose } = request;

  useEffect(() => {
    openMenu({ x: request.x, y: request.y, scope: { kind: "single" } });
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
      {/* The menu opens from a click inside the page frame, whose events never reach this document: the backdrop
          takes the dismissing click instead. */}
      {menu &&
        createPortal(
          <div
            className="fixed inset-0 z-40"
            aria-hidden="true"
            data-testid="plugin-session-menu-backdrop"
            onMouseDown={closeMenu}
            onContextMenu={(e) => {
              e.preventDefault();
              closeMenu();
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
          setRenaming(true);
        }}
        extraItems={
          request.items.length > 0 && (
            <>
              <MenuSeparator />
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
            onClose={() => setRenaming(false)}
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
}: {
  sessionId: string;
  title: string;
  current: string;
  onClose: () => void;
}) {
  const [value, setValue] = useState(current);
  const [busy, setBusy] = useState(false);

  const submit = async () => {
    const trimmed = value.trim();
    if (busy) return;
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
    result.warnings?.forEach((warning) => reportInfo(warning));
    onClose();
  };

  return (
    <SidebarModal testId="rename-modal" label="Rename session" heading="Rename" title={title} onCancel={onClose}>
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
          disabled={busy}
          value={value}
          onChange={(e) => setValue(e.target.value)}
          data-testid="rename-modal-input"
          className={MODAL_INPUT}
        />
        <div className="flex justify-end gap-2">
          <button type="button" onClick={onClose} className={MODAL_CANCEL}>
            Cancel
          </button>
          <button type="submit" disabled={busy} className={MODAL_PRIMARY}>
            Rename
          </button>
        </div>
      </form>
    </SidebarModal>
  );
}
