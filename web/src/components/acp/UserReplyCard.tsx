import type { ReactNode } from "react";

import { CopyMarkdownButton } from "./CopyMarkdownButton";

/** A structured user message in the transcript: a tagged bubble with a count and a copy button. */
export function UserReplyCard({
  tag,
  summary,
  copySource,
  copyLabel,
  testId,
  children,
}: {
  tag: string;
  summary: string;
  copySource: string;
  copyLabel: string;
  testId?: string;
  children: ReactNode;
}) {
  return (
    <div
      data-testid={testId}
      className="w-full max-w-3xl rounded-2xl rounded-br-sm border border-surface-700/50 bg-surface-800/70 px-4 py-3 text-sm"
    >
      <div className="mb-2 flex items-center gap-2 text-[12px] text-text-dim">
        <span className="rounded bg-surface-700/60 px-1.5 py-0.5 font-mono text-[11px] text-text-secondary">{tag}</span>
        <span>{summary}</span>
        <span className="ml-auto">
          <CopyMarkdownButton compact source={copySource} label={copyLabel} />
        </span>
      </div>
      {children}
    </div>
  );
}

/** One item inside a UserReplyCard. */
export const USER_REPLY_ITEM = "rounded-lg border border-surface-700/50 bg-surface-900/60";
