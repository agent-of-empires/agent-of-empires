import { useState } from "react";

import { writeClipboard } from "../../lib/clipboard";

export interface RecoveryAction {
  label: string;
  onClick: () => void;
  testId: string;
  disabled?: boolean;
  title?: string;
  /** Id of the text that says why the action is disabled. */
  describedBy?: string;
}

const BUTTON =
  "min-h-8 rounded-md px-3 py-1.5 text-xs font-medium transition-colors disabled:cursor-not-allowed disabled:opacity-60";

/** Label and fill are a theme-projected readable pair; hover adds a ring instead of
 *  changing either, so the pair holds in every state. */
const PRIMARY = `${BUTTON} bg-status-error text-text-on-status-error hover:ring-2 hover:ring-status-error/50`;

/** The recovery buttons both startup error surfaces show, primary first, and the
 *  install's failure and npm output under them. */
export function RecoveryActionRow({
  primary,
  secondary,
  install,
}: {
  primary: RecoveryAction;
  secondary: RecoveryAction[];
  install: { error: string | null; output: string | null };
}) {
  return (
    <>
      <div className="mt-3 flex flex-wrap items-center gap-2">
        {[primary, ...secondary].map((action) => (
          <button
            key={action.testId}
            type="button"
            data-testid={action.testId}
            onClick={action.onClick}
            disabled={action.disabled}
            title={action.title}
            aria-describedby={action.describedBy}
            className={
              action === primary
                ? `${PRIMARY} w-full sm:w-auto`
                : `${BUTTON} border border-surface-700 bg-surface-800 text-text-primary hover:bg-surface-700`
            }
          >
            {action.label}
          </button>
        ))}
      </div>
      {install.error && (
        <div className="mt-2 text-xs text-status-error-text" data-testid="adapter-install-error">
          {install.error}
        </div>
      )}
      {install.output && (
        <pre className="mt-2 max-h-40 overflow-auto rounded-md border border-surface-700 bg-surface-950 p-2 font-mono text-[11px] text-text-subtle">
          {install.output}
        </pre>
      )}
    </>
  );
}

/** A command to run by hand, with a copy button. `scroll` keeps it on one line
 *  and scrolls it sideways instead of wrapping. */
export function CommandBlock({
  command,
  note = null,
  testId,
  scroll = false,
}: {
  command: string;
  note?: string | null;
  testId: string;
  scroll?: boolean;
}) {
  const [copied, setCopied] = useState(false);
  return (
    <>
      <div className="mt-1 flex items-start gap-2 rounded-md border border-surface-700 bg-surface-950 p-2">
        <code
          data-testid={testId}
          className={`min-w-0 flex-1 font-mono text-[12px] text-text-primary ${
            scroll ? "overflow-x-auto whitespace-pre" : "whitespace-pre-wrap break-all"
          }`}
        >
          {command}
        </code>
        <button
          type="button"
          onClick={() => void writeClipboard(command).then(setCopied)}
          className="shrink-0 rounded-md px-2 text-[11px] text-text-subtle hover:text-text-primary"
        >
          {copied ? "Copied" : "Copy"}
        </button>
      </div>
      {note && <p className="mt-1 text-text-subtle">{note}</p>}
    </>
  );
}
