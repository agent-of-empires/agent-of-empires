import { useRef, useState, type ReactNode } from "react";
import { MoreHorizontal, Undo2, X } from "lucide-react";
import { OFFLINE_TITLE } from "../lib/connectionState";
import type { StartedSession } from "../lib/types";
import { agoLabel } from "../lib/suggestionTime";
import { StartInMenu, StartMenu, type StartMenuProps } from "./SuggestionStartMenu";
import { useOutsideDismiss } from "./useOutsideDismiss";

/** Buttons look 32px; on a phone an invisible 44px area around them takes the tap. */
const TAP =
  "relative after:absolute after:left-1/2 after:top-1/2 after:h-11 after:w-11 after:-translate-x-1/2 after:-translate-y-1/2 after:content-[''] md:after:hidden";

export const SUGGESTION_NOTE_CLASS = "border-t border-surface-700/40 px-3 py-2 text-xs text-text-dim first:border-t-0";

/** The reason line of a started item: Session state first, then how long ago it began. */
function StartedReason({ session }: { session: StartedSession }) {
  const status = session.archived ? null : session.status;
  return (
    <>
      {status === "Running" ? (
        <span className="text-status-running">
          <span aria-hidden="true" className="font-mono">
            ⠋
          </span>{" "}
          Running
        </span>
      ) : status === "Idle" ? (
        <span>
          <span aria-hidden="true" className="font-mono">
            ⠒
          </span>{" "}
          Idle
        </span>
      ) : (
        <span>{status ?? "Archived"}</span>
      )}
      {" · "}started {agoLabel(Date.parse(session.created_at))}
    </>
  );
}

interface RowProps {
  title: string;
  /** Shown above the title when non-empty. */
  sourceRef?: string;
  /** One line on wide screens, with this short reference (`#12`) ahead of the reason. */
  inlineRef?: string;
  reason: string;
  project?: string | null;
  /** Replaces the reason with the started Session's state. */
  session?: StartedSession;
  /** Dims the row: the source closed or changed. */
  stale?: boolean;
  /** A start that failed: the reason, then what to do. The row stays startable. */
  failure?: { reason: string; hint?: string };
  actions: ReactNode;
}

/**
 * A `scope/name#n` reference that, when cut, loses the scope first and never the
 * trailing `#n`.
 */
function SourceRef({ value }: { value: string }) {
  const hash = value.lastIndexOf("#");
  const head = hash > 0 ? value.slice(0, hash) : value;
  const tail = hash > 0 ? value.slice(hash) : "";
  const slash = head.indexOf("/");
  const scope = slash > 0 ? head.slice(0, slash + 1) : "";
  const name = slash > 0 ? head.slice(slash + 1) : head;
  return (
    <p className="flex font-mono text-[11px] text-text-dim" title={value} data-testid="suggestion-source-ref">
      {scope && <span className="min-w-0 shrink-[100] truncate">{scope}</span>}
      <span className="min-w-0 truncate">{name}</span>
      {tail && <span className="shrink-0">{tail}</span>}
    </p>
  );
}

export function SuggestionRow({
  title,
  sourceRef,
  inlineRef,
  reason,
  project,
  session,
  stale,
  failure,
  actions,
}: RowProps) {
  return (
    <li
      className={`flex items-center gap-3 border-t border-surface-700/40 px-3 py-2.5 first:rounded-t-lg first:border-t-0 last:rounded-b-lg ${
        failure ? "bg-status-error/[0.07]" : ""
      }`}
    >
      <div className="min-w-0 flex-1">
        {sourceRef && <SourceRef value={sourceRef} />}
        <div className={inlineRef !== undefined ? "md:flex md:items-baseline md:gap-3" : ""}>
          <p className={`truncate text-sm text-text-primary ${stale ? "opacity-[0.55]" : ""} md:min-w-0 md:flex-1`}>
            {title}
          </p>
          <p
            className={`mt-0.5 truncate text-xs md:mt-0 md:flex-none md:max-w-[45%] ${stale ? "text-text-dim" : "text-text-muted"}`}
          >
            {inlineRef && (
              <>
                <span className="font-mono text-text-secondary">{inlineRef}</span>
                {" · "}
              </>
            )}
            {session ? <StartedReason session={session} /> : reason}
            {project && (
              <>
                {" · "}
                <span className="font-mono font-medium text-text-secondary">{project}</span>
              </>
            )}
          </p>
        </div>
        {failure && (
          <p className="mt-0.5 break-words text-xs text-status-error">
            Couldn&apos;t start: {/[.!?]$/.test(failure.reason) ? failure.reason : `${failure.reason}.`}
            {failure.hint ? ` ${failure.hint}` : ""}
          </p>
        )}
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
  /** The source closed or changed: Start is removed. */
  stale?: boolean;
  /** The last start failed: Start becomes Try again, beside Start with changes. */
  failed?: boolean;
  /** False when trying again would fail the same way: only Start with changes is offered. */
  retryable?: boolean;
  /** This row's start is running. */
  starting?: boolean;
  /** The ▾ menu; absent on a dismissed row. */
  menu?: Omit<StartMenuProps, "disabled">;
  /** The repo has no Project yet: "Start in…" replaces Start, and picking a Project links the repo. */
  startIn?: { onPick: (path: string) => void; disabled?: boolean; title?: string };
}

const MENU_ITEM_CLASS =
  "block w-full px-3 py-1.5 text-left text-xs text-text-secondary hover:bg-surface-700 disabled:opacity-50";

function MoreMenu({
  started,
  stale,
  disabled,
  readOnly,
  offline,
  onDismiss,
}: {
  started: NonNullable<ActionsProps["started"]>;
  stale: boolean;
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
        className={`${TAP} inline-flex h-8 w-8 items-center justify-center rounded-md text-text-secondary hover:bg-surface-700/50`}
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
            disabled={disabled || stale}
            title={stale ? "No longer open at the source" : unavailable}
            className={MENU_ITEM_CLASS}
            onClick={() => {
              setOpen(false);
              started.onStartAnother();
            }}
          >
            Start another Session…
          </button>
          {/^https?:\/\//i.test(started.sourceUrl) && (
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
          )}
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
  stale = false,
  failed = false,
  retryable = true,
  starting = false,
  menu,
  startIn,
}: ActionsProps) {
  const disabled = busy || readOnly || offline;
  const label = dismissed ? "Restore" : "Dismiss";
  if (started && !dismissed) {
    return (
      <>
        <button
          type="button"
          onClick={started.onOpen}
          className={`${TAP} inline-flex h-8 items-center rounded-md bg-brand-600 px-3 text-xs font-medium text-text-on-brand hover:bg-brand-500`}
        >
          Open
        </button>
        <MoreMenu
          started={started}
          stale={stale}
          disabled={disabled}
          readOnly={readOnly}
          offline={offline}
          onDismiss={onToggleDismiss}
        />
      </>
    );
  }
  const unavailable = readOnly ? "Read-only: can't create Sessions" : offline ? OFFLINE_TITLE : undefined;
  const showStart = !dismissed && !stale;
  return (
    <>
      {showStart && startIn && !failed && !starting && (
        <StartInMenu
          disabled={disabled || startIn.disabled === true}
          title={startIn.title ?? unavailable}
          onPick={startIn.onPick}
        />
      )}
      {showStart && !(startIn && !failed && !starting) && !(failed && !retryable) && (
        <button
          type="button"
          onClick={onStart}
          disabled={disabled || starting}
          title={unavailable}
          className={`${TAP} inline-flex h-8 items-center rounded-md border border-surface-700 bg-surface-800 px-3 text-xs font-medium text-text-primary hover:bg-surface-700/50 disabled:opacity-50`}
        >
          {starting ? "Starting…" : failed ? "Try again" : "Start"}
        </button>
      )}
      {showStart && menu && failed && (
        <button
          type="button"
          onClick={menu.onStartWithChanges}
          disabled={disabled}
          title={unavailable}
          className={`${TAP} inline-flex h-8 items-center rounded-md border border-surface-700 bg-surface-800 px-3 text-xs font-medium text-text-primary hover:bg-surface-700/50 disabled:opacity-50`}
        >
          Start with changes
        </button>
      )}
      {showStart && menu && !failed && !starting && !startIn && <StartMenu {...menu} disabled={disabled} />}
      {!readOnly && (
        <button
          type="button"
          aria-label={label}
          title={label}
          onClick={onToggleDismiss}
          disabled={disabled}
          className={`${TAP} inline-flex h-8 w-8 items-center justify-center rounded-md text-text-secondary hover:bg-surface-700/50 disabled:opacity-50`}
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
          <span className="flex-none text-text-dim">(back on in Settings › Suggestions)</span>
        </>
      )}
    </div>
  );
}
