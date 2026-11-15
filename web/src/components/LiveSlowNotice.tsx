import { useLivePanes } from "../lib/liveControl";

/** In the Session's top bar, outside the terminal, so showing or dismissing it never resizes a pane this tab sizes. */
export function LiveSlowNotice({ sessionId }: { sessionId: string | null }) {
  const slow = useLivePanes(sessionId).find((p) => p.slow)?.slow;
  if (!slow) return null;
  return (
    <div data-live-slow-notice className="flex shrink-0 items-center sm:gap-1 text-xs text-text-primary">
      {/* Phones keep the top bar's own controls in view; the sentence stays for screen readers. */}
      <span role="status" className="whitespace-nowrap max-sm:sr-only">
        Updates are slower.
      </span>
      <button
        type="button"
        onClick={slow.reconnect}
        className="h-8 shrink-0 px-1 sm:px-2 rounded-md text-brand-400 hover:text-brand-300 cursor-pointer underline"
      >
        Reconnect
      </button>
      <button
        type="button"
        onClick={slow.dismiss}
        aria-label="Dismiss slower updates notice"
        className="h-8 w-8 shrink-0 rounded-md text-text-primary hover:bg-surface-700/50 cursor-pointer text-base leading-none"
      >
        &times;
      </button>
    </div>
  );
}
