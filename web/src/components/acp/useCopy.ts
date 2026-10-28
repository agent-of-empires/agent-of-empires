import { useEffect, useRef, useState } from "react";

import { writeClipboard } from "../../lib/clipboard";

export const COPY_BUTTON_CLASS =
  "inline-flex min-h-8 min-w-8 items-center justify-center rounded bg-surface-900 p-1.5 text-text-dim hover:bg-surface-800 hover:text-text-secondary";

const COMPACT_BASE =
  "inline-flex min-h-6 min-w-6 items-center justify-center rounded bg-surface-900 p-1 hover:bg-surface-800 pointer-coarse:min-h-8 pointer-coarse:min-w-8 pointer-coarse:p-1.5";

/** Footer rows: small with a pointer, full touch size otherwise. */
export const COMPACT_COPY_BUTTON_CLASS = `${COMPACT_BASE} text-text-dim hover:text-text-secondary`;

/** For surfaces that allow only `text-text-subtle` (the startup error screen, for contrast). */
export const COMPACT_SUBTLE_COPY_BUTTON_CLASS = `${COMPACT_BASE} text-text-subtle hover:text-text-primary`;

/** `copied` stays true for 1.5s after a successful write. */
export function useCopy(): [copied: boolean, copy: (text: string) => Promise<void>] {
  const [copied, setCopied] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
  useEffect(() => () => clearTimeout(timer.current), []);

  async function copy(text: string) {
    if (!(await writeClipboard(text))) return;
    setCopied(true);
    clearTimeout(timer.current);
    timer.current = setTimeout(() => setCopied(false), 1500);
  }
  return [copied, copy];
}
