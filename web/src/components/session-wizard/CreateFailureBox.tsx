import { useEffect, useRef, useState } from "react";
import { writeClipboard } from "../../lib/clipboard";
import type { CreateFailure } from "../../lib/types";
import { FAILURE_ACTION_LABEL, type FailureAction, type FailureView } from "./createFailureView";

const OUTPUT_LINES = 8;

/** A failed create: what failed, where, one way forward, and the reference to look it up by.
 *  Keyed by its reference, so "Copied" never outlives the failure it copied. */
export function CreateFailureBox({
  failure,
  view,
  output,
  onAction,
  busy = false,
  blocked,
}: {
  failure: CreateFailure;
  view: FailureView;
  /** The last create-progress output, shown only where the view asks for it. */
  output: string[];
  onAction: (action: FailureAction) => void;
  busy?: boolean;
  /** Actions the form can't run yet, such as a resend before Launch could send. */
  blocked?: (action: FailureAction) => boolean;
}) {
  const boxRef = useRef<HTMLDivElement | null>(null);
  const [copied, setCopied] = useState(false);
  useEffect(() => {
    boxRef.current?.focus();
  }, [failure]);

  const tail = view.showOutput ? output.slice(-OUTPUT_LINES) : [];
  const button = (action: FailureAction, primary: boolean) => (
    <button
      type="button"
      disabled={busy || !!blocked?.(action)}
      onClick={() => onAction(action)}
      data-testid={`create-failure-${action}`}
      className={`min-h-8 rounded-md px-3 py-1.5 text-xs font-medium cursor-pointer disabled:cursor-not-allowed disabled:opacity-60 ${
        primary
          ? "bg-brand-600 text-surface-900 hover:bg-brand-700"
          : "border border-surface-700 bg-surface-800 text-text-primary hover:bg-surface-700"
      }`}
    >
      {FAILURE_ACTION_LABEL[action]}
    </button>
  );

  return (
    <div
      ref={boxRef}
      role="alert"
      tabIndex={-1}
      data-testid="create-failure"
      className="mb-4 rounded-lg border border-status-error/40 bg-status-error/10 p-3 text-sm outline-none focus-visible:ring-2 focus-visible:ring-status-error/60"
    >
      <p className="font-semibold text-status-error">{view.title}</p>
      {view.step && <p className="mt-1 text-text-secondary">{view.step}</p>}
      {view.detail && <p className="mt-1 break-words text-text-primary">{view.detail}</p>}
      {tail.length > 0 && (
        <pre
          data-testid="create-failure-output"
          className="mt-2 max-h-24 overflow-auto whitespace-pre-wrap break-all rounded bg-surface-900 p-2 font-mono text-xs text-text-secondary"
        >
          {tail.join("\n")}
        </pre>
      )}
      <div className="mt-3 flex flex-wrap gap-2">
        {button(view.primary, true)}
        {view.secondary && button(view.secondary, false)}
      </div>
      {failure.reference && (
        <p className="mt-2 flex flex-wrap items-center gap-x-2 text-xs text-text-subtle">
          <span data-testid="create-failure-reference">
            Reference <span className="font-mono">{failure.reference.slice(0, 8)}…</span> · in debug.log
          </span>
          <button
            type="button"
            onClick={() => void writeClipboard(failure.reference!).then(setCopied)}
            className="font-medium text-brand-400 hover:underline cursor-pointer"
          >
            {copied ? "Copied" : "Copy"}
          </button>
        </p>
      )}
    </div>
  );
}
