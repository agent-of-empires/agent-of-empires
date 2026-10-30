import type { ReactNode } from "react";
import { Undo2, X } from "lucide-react";
import { OFFLINE_TITLE } from "../lib/connectionState";

export const SUGGESTION_NOTE_CLASS = "border-t border-surface-700/40 px-3 py-2 text-xs text-text-dim first:border-t-0";

interface RowProps {
  title: string;
  source: string;
  reason: string;
  project?: string | null;
  actions: ReactNode;
}

export function SuggestionRow({ title, source, reason, project, actions }: RowProps) {
  return (
    <li className="flex items-center gap-3 border-t border-surface-700/40 px-3 py-2.5 first:border-t-0">
      <div className="min-w-0 flex-1">
        <p className="truncate text-sm text-text-primary">{title}</p>
        <p className="mt-0.5 truncate text-xs text-text-muted">
          <span className="font-mono font-medium text-text-secondary">{source}</span>
          {" · "}
          {reason}
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
}

/** Start plus the dismiss (or, for a dismissed item, restore) button. */
export function SuggestionActions({
  dismissed = false,
  readOnly,
  offline,
  busy,
  onStart,
  onToggleDismiss,
}: ActionsProps) {
  const disabled = busy || readOnly || offline;
  const label = dismissed ? "Restore" : "Dismiss";
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
