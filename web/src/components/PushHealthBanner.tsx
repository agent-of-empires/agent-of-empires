import { useEffect, useState } from "react";
import { usePushSubscription } from "../hooks/usePushSubscription";
import { deniedGuidance, needsAttention, type PushHealth } from "../lib/pushHealth";
import { safeGetItem, safeRemoveItem, safeSetItem } from "../lib/safeStorage";

const DISMISSED_KEY = "aoe.push.bannerDismissed";

function writeDismissed(health: PushHealth | null): void {
  if (health) safeSetItem(DISMISSED_KEY, health);
  else safeRemoveItem(DISMISSED_KEY);
}

/** Alerts a device that asked for push notifications and has stopped receiving them. */
export function PushHealthBanner() {
  const { state, health, enable } = usePushSubscription();
  // Dismissal is per health state, so a different failure shows again.
  const [dismissed, setDismissed] = useState(() => safeGetItem(DISMISSED_KEY));

  // A recovery re-arms the banner for the next failure.
  useEffect(() => {
    if (health === "healthy") writeDismissed(null);
  }, [health]);

  if (!needsAttention(health) || dismissed === health) return null;
  const busy = state.kind === "asking" || state.kind === "subscribing";

  return (
    <div
      role="alert"
      aria-label="Notifications stopped"
      className="bg-status-waiting/10 border-b border-status-waiting/30 px-4 py-2 flex items-center justify-center gap-3 text-xs font-mono text-status-waiting animate-fade-in"
    >
      <span className="w-1.5 h-1.5 rounded-full bg-status-waiting shrink-0" />
      {health === "permission-denied" ? (
        <span>{deniedGuidance()}</span>
      ) : (
        <>
          <span>Notifications stopped on this device.</span>
          {/* enable() must start inside this click: iOS needs the gesture to re-prompt. */}
          <button
            type="button"
            onClick={() => void enable()}
            disabled={busy}
            className="underline hover:opacity-80 cursor-pointer disabled:opacity-50 disabled:cursor-not-allowed"
          >
            {busy ? "Enabling..." : "Re-enable"}
          </button>
        </>
      )}
      <button
        type="button"
        onClick={() => {
          writeDismissed(health);
          setDismissed(health);
        }}
        aria-label="Dismiss notifications notice"
        className="ml-2 text-text-muted hover:text-text-secondary cursor-pointer text-base leading-none px-1"
      >
        &times;
      </button>
    </div>
  );
}
