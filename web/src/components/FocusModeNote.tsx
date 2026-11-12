import { useState } from "react";
import { safeGetItem, safeSetItem } from "../lib/safeStorage";

const DISMISSED_KEY = "aoe.focusMode.noteDismissed";

/** Explains a focus-mode window on first use: it shows `title` alone, and other sessions still notify through the
 *  system. Dismissing it hides it in every later focus window of this browser. */
export function FocusModeNote({ title }: { title: string }) {
  const [dismissed, setDismissed] = useState(() => safeGetItem(DISMISSED_KEY) !== null);
  if (dismissed) return null;
  return (
    <div
      role="status"
      data-testid="focus-mode-note"
      className="bg-surface-800 border-b border-surface-700/60 px-4 py-2 flex items-center justify-center gap-3 text-xs text-text-secondary animate-fade-in"
    >
      <span>
        Only <span className="font-mono text-text-primary">{title}</span> shows here. Other Sessions still notify
        through your system.
      </span>
      <button
        type="button"
        onClick={() => {
          safeSetItem(DISMISSED_KEY, "1");
          setDismissed(true);
        }}
        aria-label="Dismiss"
        className="ml-2 text-text-subtle hover:text-text-primary cursor-pointer text-base leading-none px-1"
      >
        &times;
      </button>
    </div>
  );
}
