import { useCallback, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { RotateCcw, Trash2, X } from "lucide-react";
import type { Workspace } from "../../lib/types";
import { isSessionActive } from "../../lib/session";
import { useIdleDecayWindowMs } from "../../lib/idleDecay";
import { workspaceTrashedAtMs } from "../../lib/sidebarSort";
import { EmptyTrashConfirm } from "../EmptyTrashConfirm";
import { useOutsideDismiss } from "../useOutsideDismiss";
import type { RowActivate } from "./types";

const ACTION =
  "inline-flex h-7 items-center rounded-md border px-2.5 text-[12px] font-medium cursor-pointer transition-colors";
const DANGER = `${ACTION} gap-1.5 border-status-error/30 bg-status-error/10 text-status-error/85 hover:border-status-error/50 hover:bg-status-error/15 hover:text-status-error`;

/** Footer Trash control; kept outside the filtered list so recovery stays reachable while a filter hides every row. */
export function TrashMenu({
  trashedWorkspaces,
  readOnly,
  onOpen,
  onRestore,
  onDelete,
  onEmptyTrash,
}: {
  trashedWorkspaces: Workspace[];
  readOnly?: boolean;
  onOpen: RowActivate;
  onRestore: (sessionIds: string[]) => void;
  onDelete: (sessionIds: string[]) => void;
  onEmptyTrash: () => void;
}) {
  const [open, setOpen] = useState(false);
  const [confirmEmpty, setConfirmEmpty] = useState(false);
  const [panelPosition, setPanelPosition] = useState<{ left: number; bottom: number; width: number } | null>(null);
  const ref = useRef<HTMLDivElement>(null);
  const panelRef = useRef<HTMLDivElement>(null);

  const positionPanel = useCallback(() => {
    const rect = ref.current?.getBoundingClientRect();
    if (!rect) return;
    const gutter = 8;
    const width = Math.min(420, window.innerWidth - gutter * 2);
    const left = Math.min(Math.max(gutter, rect.left), Math.max(gutter, window.innerWidth - width - gutter));
    setPanelPosition({ left, bottom: Math.max(gutter, window.innerHeight - rect.top + gutter), width });
  }, []);

  // The Empty Trash confirm portals outside both refs and owns dismissal while it is up.
  useOutsideDismiss(open && !confirmEmpty, [ref, panelRef], () => setOpen(false));

  useLayoutEffect(() => {
    if (!open) return;
    positionPanel();
    window.addEventListener("resize", positionPanel);
    window.addEventListener("scroll", positionPanel, true);
    return () => {
      window.removeEventListener("resize", positionPanel);
      window.removeEventListener("scroll", positionPanel, true);
    };
  }, [open, positionPanel]);

  const count = trashedWorkspaces.length;

  return (
    <div ref={ref} className="relative min-w-0 flex-1">
      <button
        onClick={() => {
          if (!open) positionPanel();
          setOpen((o) => !o);
        }}
        aria-expanded={open}
        aria-controls={open ? "sidebar-trash-panel" : undefined}
        data-testid="sidebar-trash-toggle"
        className="h-8 w-full min-w-0 flex items-center gap-2 rounded-md px-2.5 text-text-secondary hover:text-text-primary hover:bg-surface-800/50 cursor-pointer transition-colors"
        title={`Trash (${count})`}
        aria-label={`Trash (${count})`}
      >
        <Trash2 className="h-4 w-4 shrink-0" />
        <span
          data-testid="sidebar-trash-count"
          className="shrink-0 rounded-full bg-surface-900 px-1.5 py-0.5 text-[10px] font-mono tabular-nums text-text-dim leading-none"
        >
          {count}
        </span>
        <span className="min-w-0 flex-1 truncate text-left text-[13px] font-medium">Trash</span>
      </button>
      {open &&
        panelPosition &&
        createPortal(
          <div
            ref={panelRef}
            id="sidebar-trash-panel"
            role="region"
            aria-label="Trash"
            data-testid="sidebar-trash-menu"
            className="fixed z-40 flex max-h-[min(520px,calc(100dvh-5rem))] flex-col overflow-hidden rounded-lg border border-surface-700/60 bg-surface-800 shadow-2xl animate-fade-in"
            style={panelPosition}
          >
            <div className="flex items-start justify-between gap-3 border-b border-surface-700/60 px-4 py-3">
              <div className="min-w-0">
                <div className="flex items-center gap-2">
                  <Trash2 className="h-4 w-4 shrink-0 text-text-muted" />
                  <h2 className="text-sm font-semibold text-text-primary">Trash</h2>
                  <span className="rounded-full bg-surface-900 px-2 py-0.5 text-[11px] font-mono tabular-nums text-text-dim leading-none">
                    {count}
                  </span>
                </div>
                <p className="mt-1 text-[12px] text-text-dim">Restore sessions, or delete them permanently.</p>
              </div>
              <div className="flex shrink-0 items-center gap-1">
                {!readOnly && (
                  <button
                    type="button"
                    onClick={() => setConfirmEmpty(true)}
                    data-testid="sidebar-trash-empty"
                    title="Empty Trash"
                    aria-label="Empty Trash"
                    className={DANGER}
                  >
                    <Trash2 className="h-3.5 w-3.5 shrink-0" />
                    Empty Trash
                  </button>
                )}
                <button
                  type="button"
                  onClick={() => setOpen(false)}
                  aria-label="Close Trash"
                  className="-mr-1 rounded-md p-1 text-text-muted hover:bg-surface-700/50 hover:text-text-primary cursor-pointer transition-colors"
                >
                  <X className="h-4 w-4" />
                </button>
              </div>
            </div>

            <div className="min-h-0 flex-1 overflow-y-auto p-2">
              <div className="space-y-1">
                {[...trashedWorkspaces]
                  .sort((a, b) => workspaceTrashedAtMs(b) - workspaceTrashedAtMs(a))
                  .map((ws) => (
                    <TrashRow
                      key={ws.id}
                      ws={ws}
                      readOnly={readOnly}
                      onOpen={(sessionId) => {
                        setOpen(false);
                        onOpen(ws.id, { metaKey: false, ctrlKey: false, shiftKey: false }, sessionId);
                      }}
                      onRestore={onRestore}
                      onDelete={() => {
                        setOpen(false);
                        onDelete(ws.sessions.map((s) => s.id));
                      }}
                    />
                  ))}
              </div>
            </div>
          </div>,
          document.body,
        )}
      {confirmEmpty &&
        createPortal(
          <EmptyTrashConfirm
            // Counts sessions, not workspaces, matching the TUI.
            sessionCount={trashedWorkspaces.reduce((n, ws) => n + ws.sessions.length, 0)}
            onConfirm={() => {
              setConfirmEmpty(false);
              setOpen(false);
              onEmptyTrash();
            }}
            onCancel={() => setConfirmEmpty(false)}
          />,
          document.body,
        )}
    </div>
  );
}

function TrashRow({
  ws,
  readOnly,
  onOpen,
  onRestore,
  onDelete,
}: {
  ws: Workspace;
  readOnly?: boolean;
  onOpen: (sessionId: string | null) => void;
  onRestore: (sessionIds: string[]) => void;
  onDelete: () => void;
}) {
  const idleDecayWindowMs = useIdleDecayWindowMs();
  const sessionCount = ws.sessions.length;
  return (
    <div
      data-testid="sidebar-trash-row"
      className="rounded-md border border-surface-700/30 bg-surface-900/20 px-3 py-2.5 text-[13px] text-text-secondary"
    >
      <div className="min-w-0">
        <div className="truncate font-medium text-text-primary" title={ws.displayName}>
          {ws.displayName}
        </div>
        <div className="mt-0.5 flex min-w-0 flex-wrap items-center gap-x-2 gap-y-0.5 text-[11px] text-text-dim">
          <span className="max-w-full truncate font-mono" title={ws.projectPath}>
            {ws.projectPath}
          </span>
          {ws.branch && (
            <span className="max-w-full truncate font-mono text-accent-500" title={ws.branch}>
              {ws.branch}
            </span>
          )}
          <span>{sessionCount === 1 ? "1 session" : `${sessionCount} sessions`}</span>
        </div>
      </div>
      <div className="mt-2 flex flex-wrap items-center gap-2">
        <button
          type="button"
          onClick={() => {
            const target = ws.sessions.find((s) => isSessionActive(s, idleDecayWindowMs)) ?? ws.sessions[0];
            onOpen(target?.id ?? null);
          }}
          data-testid="sidebar-trash-open"
          className={`${ACTION} border-surface-700/50 text-text-secondary hover:border-surface-600 hover:bg-surface-700/40 hover:text-text-primary`}
        >
          Open
        </button>
        {!readOnly && (
          <>
            <button
              type="button"
              onClick={() => {
                const ids = ws.sessions.map((s) => s.id);
                if (ids.length > 0) onRestore(ids);
              }}
              data-testid="sidebar-trash-restore"
              title="Restore"
              aria-label="Restore"
              className={`${ACTION} gap-1.5 border-accent-500/30 bg-accent-500/10 text-accent-500 hover:border-accent-500/50 hover:bg-accent-500/15 hover:text-accent-600`}
            >
              <RotateCcw className="h-3.5 w-3.5 shrink-0" />
              Restore
            </button>
            <button
              type="button"
              onClick={onDelete}
              data-testid="sidebar-trash-purge"
              title="Delete permanently"
              aria-label="Delete permanently"
              className={DANGER}
            >
              <X className="h-3.5 w-3.5 shrink-0" />
              Delete
            </button>
          </>
        )}
      </div>
    </div>
  );
}
