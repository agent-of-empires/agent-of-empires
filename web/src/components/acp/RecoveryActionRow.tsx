import { useState, type ComponentProps, type ReactNode } from "react";

import { writeClipboard } from "../../lib/clipboard";
import { BUTTON_DANGER, BUTTON_SECONDARY, CARD } from "../../lib/surfaces";
import { BUTTON_SECONDARY_SM } from "../../lib/surfaces";
import { Spinner } from "../Dialog";

export interface RecoveryAction {
  label: string;
  onClick: () => void;
  testId: string;
  disabled?: boolean;
  /** In flight: shows a spinner beside the label. */
  busy?: boolean;
  title?: string;
  /** Id of the text that says why the action is disabled. */
  describedBy?: string;
}

/** Label and fill are a theme-projected readable pair; hover adds a ring instead of
 *  changing either, so the pair holds in every state. */
const PRIMARY = BUTTON_DANGER.replace(
  "text-white hover:opacity-90",
  "text-text-on-status-error hover:ring-2 hover:ring-status-error/50",
);

const SCREEN_CARD = `${CARD.replace("border-surface-700/50", "border-status-error/60").replace("px-4", "p-4 sm:p-6")} w-full max-w-xl text-text-primary`;

/** Fills the agent pane with one red-edged card, for a Session that can't start. */
export function RecoveryScreen({ children, ...props }: ComponentProps<"div">) {
  return (
    <div {...props} className="flex h-full flex-1 items-center justify-center bg-surface-900 px-4 py-12">
      <div className={SCREEN_CARD}>{children}</div>
    </div>
  );
}

export function InlineCode({ children }: { children: ReactNode }) {
  return <code className="rounded bg-surface-950 px-1 font-mono text-[12px]">{children}</code>;
}

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
            aria-busy={action.busy || undefined}
            className={action === primary ? `${PRIMARY} w-full sm:w-auto` : BUTTON_SECONDARY}
          >
            {action.busy && <Spinner />}
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

/** A command to run by hand, wrapped so all of it reads before it runs, with a copy button. */
export function CommandBlock({
  command,
  note = null,
  testId,
}: {
  command: string;
  note?: string | null;
  testId: string;
}) {
  const [copied, setCopied] = useState(false);
  return (
    <>
      <div className="mt-1 flex items-start gap-2 rounded-md border border-surface-700 bg-surface-950 p-2">
        <code
          data-testid={testId}
          className="min-w-0 flex-1 whitespace-pre-wrap break-all font-mono text-[12px] text-text-primary"
        >
          {command}
        </code>
        <button
          type="button"
          onClick={() => void writeClipboard(command).then(setCopied)}
          className={`${BUTTON_SECONDARY_SM} shrink-0`}
        >
          {copied ? "Copied" : "Copy"}
        </button>
      </div>
      {note && <p className="mt-1 text-text-subtle">{note}</p>}
    </>
  );
}
