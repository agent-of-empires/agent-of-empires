import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { Bell } from "lucide-react";
import { usePushSubscription } from "../hooks/usePushSubscription";
import { onDaemonChanged } from "../lib/connectionState";
import { installHint } from "../lib/platform";
import { safeGetItem, safeSetItem } from "../lib/safeStorage";
import { INSTALL_STEPS } from "../lib/tourSteps";

export const INSTALL_BANNER_DISMISSED_KEY = "aoe-install-banner-dismissed";
export const INSTALL_BANNER_HIDE_LABEL = "Hide notification steps";
export const INSTALL_BANNER_HIDE_TIP = "Hide. The steps stay in Settings › Notifications.";

/** Whether the server sends push at all; null until it answers, and asked again whenever the Daemon may have
 *  changed (a reconnect, or a restart between two polls). A failed request counts as on, so an outage doesn't hide the steps. */
function useServerPushEnabled(ask: boolean): boolean | null {
  const [enabled, setEnabled] = useState<boolean | null>(null);
  const [reconnects, setReconnects] = useState(0);
  useEffect(() => onDaemonChanged(() => setReconnects((n) => n + 1)), []);
  useEffect(() => {
    if (!ask) return;
    let alive = true;
    fetch("/api/push/status")
      .then((r) => (r.ok ? (r.json() as Promise<{ enabled?: boolean }>) : { enabled: true }))
      .catch(() => ({ enabled: true }))
      .then((s) => alive && setEnabled(s.enabled !== false));
    return () => {
      alive = false;
    };
  }, [ask, reconnects]);
  return enabled;
}

/**
 * What a phone or tablet browser tab needs for notifications (#4595), until dismissed on this device. It
 * hides once nothing is left to do here: push already on, permission denied (the push banner says how to
 * fix that), or push turned off on the server.
 */
export function InstallBanner({ hidden = false }: { hidden?: boolean }) {
  const [hint] = useState(installHint);
  const [dismissed, setDismissed] = useState(() => safeGetItem(INSTALL_BANNER_DISMISSED_KEY) === "1");
  const { state } = usePushSubscription();
  // A push tab learns the server's setting from its own subscription state; an install tab can't subscribe.
  const serverPush = useServerPushEnabled(hint === "ios" || hint === "other");

  if (hidden || hint === null || dismissed) return null;
  if (hint === "push") {
    if (state.kind !== "off" && state.kind !== "error") return null;
  } else if (serverPush !== true) return null;

  const steps = INSTALL_STEPS[hint].banner;
  return (
    <div
      role="region"
      aria-label="Get notifications on this device"
      className="relative grid grid-cols-[auto_minmax(0,1fr)] gap-x-2.5 border-b border-brand-500/30 bg-brand-500/10 py-2 pl-3 pr-11 text-[13px] leading-[1.45] text-text-primary animate-fade-in"
    >
      <Bell className="mt-px h-4 w-4 text-brand-300" aria-hidden="true" />
      <p>
        <span className="font-semibold">Get notifications on this device.</span>{" "}
        {hint === "push" ? (
          <span className="text-text-subtle">
            Turn them on in{" "}
            <Link to="/settings/notifications" className="text-text-primary underline underline-offset-2">
              Settings › Notifications
            </Link>
            .
          </span>
        ) : (
          <span className="text-text-subtle">
            {steps.path && (
              <>
                Tap <span className="whitespace-nowrap font-mono text-xs text-brand-200">{steps.path}</span>.{" "}
              </>
            )}
            {steps.text}
          </span>
        )}
      </p>
      <button
        type="button"
        aria-label={INSTALL_BANNER_HIDE_LABEL}
        title={INSTALL_BANNER_HIDE_TIP}
        onClick={() => {
          safeSetItem(INSTALL_BANNER_DISMISSED_KEY, "1");
          setDismissed(true);
        }}
        className="absolute right-1.5 top-1 inline-flex h-8 w-8 cursor-pointer items-center justify-center rounded-md text-base leading-none text-text-subtle hover:text-text-primary"
      >
        &times;
      </button>
    </div>
  );
}
