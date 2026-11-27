import { useEffect, useState } from "react";
import type { AcpContext } from "./AcpRuntime";
import { connectionRetriesExhausted } from "./startupWait";
import { useConnectionBanner } from "../../hooks/useConnectionBanner";

/** A drop is named only once it outlasts this, so a phone resuming from the background redials quietly. */
const RECONNECTING_GRACE_MS = 2000;

const ACTION_BUTTON =
  "shrink-0 rounded-lg border border-surface-600 px-2.5 py-1 text-[12px] font-medium text-text-primary hover:bg-surface-700/60 cursor-pointer";

/**
 * The conversation's own connection line, one at a time, and only what the app's banners do not already say:
 * while the Daemon is unreachable its banner speaks for every Session, so this keeps quiet.
 */
export function SystemNotices({
  status,
  lagged,
  rateLimitRetriesExhausted,
  hasEverOpened,
  replayedWhileDown = false,
  reconnecting,
  retryCount,
  maxRetries,
  manualReconnect,
  newSessionStarting = false,
}: {
  status: AcpContext["status"];
  lagged: boolean;
  /** Only while no usage-limit banner is shown; the banner says it otherwise. */
  rateLimitRetriesExhausted: boolean;
  hasEverOpened: boolean;
  /** Events still arrive over HTTP replay while the socket is down. */
  replayedWhileDown?: boolean;
  reconnecting: boolean;
  retryCount: number;
  maxRetries: number;
  manualReconnect: () => void;
  /** The startup wait line already covers a new Session's first connection. */
  newSessionStarting?: boolean;
}) {
  const daemonDown = useConnectionBanner() === "down";
  const down = status !== "open";
  const [slow, setSlow] = useState(false);
  useEffect(() => {
    if (!down || !hasEverOpened) return;
    const timer = setTimeout(() => setSlow(true), RECONNECTING_GRACE_MS);
    return () => {
      clearTimeout(timer);
      setSlow(false);
    };
  }, [down, hasEverOpened]);

  const exhausted = connectionRetriesExhausted({ status, reconnecting, retryCount, maxRetries });
  let connection: string | null = null;
  if (down && !daemonDown && !exhausted && !(newSessionStarting && !hasEverOpened)) {
    if (!hasEverOpened) connection = "Starting structured view…";
    else if (slow)
      connection = replayedWhileDown
        ? "Reconnecting… showing history fetched meanwhile; new messages wait until it is back."
        : "Reconnecting… new messages wait until it is back.";
  }
  const lines = [
    connection,
    lagged ? "Some events were missed during reconnect." : null,
    rateLimitRetriesExhausted
      ? "Auto-resume stopped: the same prompt was re-sent too many times without getting through. Send a new prompt."
      : null,
  ].filter((l): l is string => l !== null);
  const showStopped = exhausted && !daemonDown;
  if (lines.length === 0 && !showStopped) return null;
  return (
    <div className="bg-status-waiting/15 border-b border-status-waiting/30 px-4 py-1.5 space-y-1 text-xs text-status-waiting">
      {lines.map((text) => (
        <div key={text}>{text}</div>
      ))}
      {showStopped && (
        <div className="flex items-center justify-between gap-3">
          <span>{hasEverOpened ? "Connection lost." : "Could not connect."}</span>
          <button type="button" onClick={manualReconnect} className={ACTION_BUTTON}>
            Reconnect
          </button>
        </div>
      )}
    </div>
  );
}
