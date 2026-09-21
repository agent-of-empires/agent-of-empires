// On failure, shows the error and tucks the card's normal body into a collapsed
// "attempted action"; otherwise renders the body as-is.

import type { ReactNode } from "react";

import { describeToolErrorTag, parseToolError } from "../../lib/toolErrorParse";

interface Props {
  status: "running" | "ok" | "err" | "stopped";
  /** Raw result text; a `<tag>...</tag>` wrapper becomes a label chip. */
  errorText?: string;
  children: ReactNode;
}

export function ToolErrorBody({ status, errorText, children }: Props) {
  if (status !== "err") {
    return <>{children}</>;
  }
  const { body, tag } = parseToolError(errorText);
  const label = describeToolErrorTag(tag);
  return (
    <>
      <div className="border-t border-rose-900/40 bg-rose-950/30 px-3 py-2 text-xs text-rose-200">
        <div className="mb-1 flex items-center gap-2 text-[10px] uppercase tracking-wider text-rose-300/80">
          <span>tool failed</span>
          {label && (
            <span className="rounded border border-rose-800/60 bg-rose-900/40 px-1 py-px font-mono normal-case tracking-normal text-[10px] text-rose-200/90">
              {label}
            </span>
          )}
        </div>
        <pre className="whitespace-pre-wrap break-words font-mono text-[11px] text-rose-100/90">
          {body || "(no error detail provided)"}
        </pre>
      </div>
      {children && (
        <details className="border-t border-surface-800 bg-surface-900/40">
          <summary className="cursor-pointer select-none px-3 py-1 text-[11px] text-text-dim hover:text-text-secondary">
            Show attempted action
          </summary>
          {children}
        </details>
      )}
    </>
  );
}
