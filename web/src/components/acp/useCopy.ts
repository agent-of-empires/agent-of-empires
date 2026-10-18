import { useEffect, useRef, useState } from "react";

import { writeClipboard } from "../../lib/clipboard";

export const COPY_BUTTON_CLASS =
  "inline-flex min-h-8 min-w-8 items-center justify-center rounded bg-surface-900 p-1.5 text-text-dim hover:bg-surface-800 hover:text-text-secondary";

/** Footer rows: small with a pointer, full touch size otherwise. */
export const COMPACT_COPY_BUTTON_CLASS =
  "inline-flex min-h-6 min-w-6 items-center justify-center rounded bg-surface-900 p-1 text-text-dim hover:bg-surface-800 hover:text-text-secondary pointer-coarse:min-h-8 pointer-coarse:min-w-8 pointer-coarse:p-1.5";

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
