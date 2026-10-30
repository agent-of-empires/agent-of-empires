import { useRef, useState, type ReactNode } from "react";
import { MoreHorizontal, Undo2, X } from "lucide-react";
import { OFFLINE_TITLE } from "../lib/connectionState";
import type { StartedSession } from "../lib/types";
import { agoLabel } from "../lib/suggestionTime";
import { useOutsideDismiss } from "./useOutsideDismiss";

export const SUGGESTION_NOTE_CLASS = "border-t border-surface-700/40 px-3 py-2 text-xs text-text-dim first:border-t-0";

/** The reason line of a started item: Session state, its title, how long ago it began. */
function StartedReason({ session }: { session: StartedSession }) {
  const status = session.archived ? null : session.status;
  return (
    <>
      {status === "Running" ? (
        <span className="text-status-running">
          <span aria-hidden="true" className="font-mono">
            ⠋
          </span>{" "}
          Session running
        </span>
      ) : status === "Idle" ? (
        <span>
          <span aria-hidden="true" className="font-mono">
            ⠒
          </span>{" "}
          Session idle
        </span>
      ) : (
        <span>{status ? `Session ${status.toLowerCase()}` : "Archived"}</span>
      )}
      {" · "}
      {session.title}
      {" · "}started {agoLabel(Date.parse(session.created_at))}
    </>
  );
}

interface RowProps {
  title: string;
  source: string;
  reason: string;
  project?: string | null;
  /** Replaces the reason with the started Session's state. */
  session?: StartedSession;
  actions: ReactNode;
}

export function SuggestionRow({ title, source, reason, project, session, actions }: RowProps) {
  return (
    <li className="flex items-center gap-3 border-t border-surface-700/40 px-3 py-2.5 first:border-t-0">
      <div className="min-w-0 flex-1">
        <p className="truncate text-sm text-text-primary">{title}</p>
        <p className="mt-0.5 truncate text-xs text-text-muted">
          <span className="font-mono font-medium text-text-secondary">{source}</span>
          {" · "}
          {session ? <StartedReason session={session} /> : reason}
          {project && (
            <>
              {" · "}
              <span className="font-mono font-medium text-text-secondary">{project}</span>
            </>
          )}
        </p>
      </div>
      <div className="flex flex-none gap-1">{actions}</div>
    </li>
  );
}

interface ActionsProps {
  dismissed?: boolean;
  readOnly: boolean;
  offline: boolean;
  busy: boolean;
  onStart: () => void;
  onToggleDismiss: () => void;
  /** A started item: Open replaces Start, and the others move into a menu. */
  started?: { onOpen: () => void; onStartAnother: () => void; providerName: string; sourceUrl: string };
}

const MENU_ITEM_CLASS =
  "block w-full px-3 py-1.5 text-left text-xs text-text-secondary hover:bg-surface-700 disabled:opacity-50";

function MoreMenu({
  started,
  disabled,
  readOnly,
  offline,
  onDismiss,
}: {
  started: NonNullable<ActionsProps["started"]>;
  disabled: boolean;
  readOnly: boolean;
  offline: boolean;
  onDismiss: () => void;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useOutsideDismiss(open, [ref], () => setOpen(false));
  const unavailable = readOnly ? "Read-only: can't create Sessions" : offline ? OFFLINE_TITLE : undefined;
  return (
    <div ref={ref} className="relative">
      <button
        type="button"
        aria-label="More"
        title="More"
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
        className="inline-flex h-8 w-8 items-center justify-center rounded-md text-text-secondary hover:bg-surface-700/50"
      >
        <MoreHorizontal className="h-4 w-4" />
      </button>
      {open && (
        <div
          role="menu"
          className="absolute right-0 z-20 mt-1 w-[220px] rounded-md border border-surface-700 bg-surface-800 py-1 shadow-lg"
        >
          <button
            type="button"
            role="menuitem"
            disabled={disabled}
            title={unavailable}
            className={MENU_ITEM_CLASS}
            onClick={() => {
              setOpen(false);
              started.onStartAnother();
            }}
          >
            Start another Session…
          </button>
          <a
            role="menuitem"
            href={started.sourceUrl}
            target="_blank"
            rel="noopener noreferrer"
            className={MENU_ITEM_CLASS}
            onClick={() => setOpen(false)}
          >
            Open in {started.providerName} ↗
          </a>
          {!readOnly && (
            <>
              <hr className="my-1 border-surface-700/60" />
              <button
                type="button"
                role="menuitem"
                disabled={disabled}
                className={MENU_ITEM_CLASS}
                onClick={() => {
                  setOpen(false);
                  onDismiss();
                }}
              >
                Dismiss
              </button>
            </>
          )}
        </div>
      )}
    </div>
  );
}

/** Start plus the dismiss (or, for a dismissed item, restore) button. */
export function SuggestionActions({
  dismissed = false,
  readOnly,
  offline,
  busy,
  onStart,
  onToggleDismiss,
  started,
}: ActionsProps) {
  const disabled = busy || readOnly || offline;
  const label = dismissed ? "Restore" : "Dismiss";
  if (started && !dismissed) {
    return (
      <>
        <button
          type="button"
          onClick={started.onOpen}
          className="inline-flex h-8 items-center rounded-md bg-brand-600 px-3 text-xs font-medium text-text-on-brand hover:bg-brand-500"
        >
          Open
        </button>
        <MoreMenu
          started={started}
          disabled={disabled}
          readOnly={readOnly}
          offline={offline}
          onDismiss={onToggleDismiss}
        />
      </>
    );
  }
  return (
    <>
      {!dismissed && (
        <button
          type="button"
          onClick={onStart}
          disabled={disabled}
          title={readOnly ? "Read-only: can't create Sessions" : offline ? OFFLINE_TITLE : undefined}
          className="inline-flex h-8 items-center rounded-md border border-surface-700 bg-surface-800 px-3 text-xs font-medium text-text-primary hover:bg-surface-700/50 disabled:opacity-50"
        >
          Start
        </button>
      )}
      {!readOnly && (
        <button
          type="button"
          aria-label={label}
          title={label}
          onClick={onToggleDismiss}
          disabled={disabled}
          className="inline-flex h-8 w-8 items-center justify-center rounded-md text-text-secondary hover:bg-surface-700/50 disabled:opacity-50"
        >
          {dismissed ? <Undo2 className="h-4 w-4" /> : <X className="h-4 w-4" />}
        </button>
      )}
    </>
  );
}

export function SuggestionStatusNotes({ readOnly, offline }: { readOnly: boolean; offline: boolean }) {
  return (
    <>
      {readOnly && (
        <li className={SUGGESTION_NOTE_CLASS}>This dashboard is read-only. Start, Dismiss and Refresh are off.</li>
      )}
      {offline && (
        <li className={SUGGESTION_NOTE_CLASS}>
          Disconnected. Suggestions show from the last fetch; reconnect to start one.
        </li>
      )}
    </>
  );
}

/** Shown for 10 s after a dismiss; `onStop` turns the item's kind off (homepage only). */
export function UndoBar({
  title,
  disabled,
  onUndo,
  onStop,
  stopLabel,
}: {
  title: string;
  disabled: boolean;
  onUndo: () => void;
  onStop?: () => void;
  stopLabel?: string;
}) {
  return (
    <div className="mt-2 flex w-max max-w-full items-baseline gap-1 rounded-md bg-surface-800 px-3 py-1.5 text-[13px] text-text-secondary">
      <span className="min-w-0 truncate">Dismissed &ldquo;{title}&rdquo;.</span>
      <button
        type="button"
        onClick={onUndo}
        disabled={disabled}
        className="flex-none cursor-pointer text-brand-500 hover:underline disabled:opacity-50"
      >
        Undo
      </button>
      {onStop && stopLabel && (
        <>
          <span aria-hidden="true">·</span>
          <button
            type="button"
            onClick={onStop}
            disabled={disabled}
            className="flex-none cursor-pointer text-brand-500 hover:underline disabled:opacity-50"
          >
            Stop suggesting {stopLabel}
          </button>
        </>
      )}
    </div>
  );
}
