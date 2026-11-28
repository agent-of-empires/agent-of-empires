import { useLivePanes } from "../lib/liveControl";
import { BUTTON_INLINE } from "../lib/surfaces";

// The top bar's controls stay 32px on a phone too.
const TOP_BAR_BUTTON = `${BUTTON_INLINE.replace("max-md:h-11", "max-md:h-8")} shrink-0`;

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
      <button type="button" onClick={slow.reconnect} className={TOP_BAR_BUTTON}>
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
