// Shared body wrapper for tool-call cards. When the tool failed
// (`status === "err"`), render the adapter's failure reason in a
// dedicated error block and tuck the per-kind card's normal body
// (e.g. the attempted Edit diff, parsed search match list, MCP input
// payload) below it inside a collapsed `<details>`. When the tool
// succeeded or is still running, render the per-kind body verbatim.
//
// Without this wrapper, cards with rich custom bodies (EditToolCard
// most notably) drop the error text on the floor; the only signal
// that anything went wrong is a tiny red status dot in the header.
// See issue #1090.

import type { ReactNode } from "react";

import { describeToolErrorTag, parseToolError } from "../../lib/toolErrorParse";

interface Props {
  status: "running" | "ok" | "err" | "stopped";
  /** Raw `result.text` from the completion row. claude-agent-acp wraps
   *  Claude's tool errors in `<tool_use_error>...</tool_use_error>`;
   *  the parser peels the wrapper and surfaces it as a label outside
   *  the error body so the source is clear. */
  errorText?: string;
  /** The per-kind card's normal body. Rendered as-is on success;
   *  shown below the error block in a collapsed `<details>` on error
   *  (so power users can still inspect what was attempted). */
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
