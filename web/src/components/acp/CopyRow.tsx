import type { ReactNode } from "react";

import { CopyMarkdownButton } from "./CopyMarkdownButton";

/** A block with its copy button beside it, so a long line can scroll without running under the button. */
export function CopyRow({ source, label, children }: { source: string; label: string; children: ReactNode }) {
  return (
    <div className="flex items-start gap-1">
      <div className="min-w-0 flex-1">{children}</div>
      <CopyMarkdownButton compact source={source} label={label} />
    </div>
  );
}
